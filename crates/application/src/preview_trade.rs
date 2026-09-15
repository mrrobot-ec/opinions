//! `PreviewTrade` — pure read (rule 8): `MarketQueries` + domain quote,
//! returns numbers only, writes nothing, never opens a `TradeTx` (ISP: read
//! paths never touch writer roles). Previews are advisory and can go stale;
//! `PlaceTrade` re-validates everything under row locks.

use domain::amm::Side;
use domain::money::{MicroShares, MicroUsd};

use crate::error::AppError;
use crate::model::{MarketId, RepConfig, TradeAction, UserId};
use crate::ports::{Clock, ConfigReads, MarketQueries};

#[derive(Debug, Clone)]
pub struct PreviewTradeCmd {
    /// Market UUID string or slug.
    pub market_ref: String,
    pub user_id: UserId,
    pub side: Side,
    pub action: TradeAction,
    /// Collateral micro-USD for buys; micro-shares for sells.
    pub amount_micro: i64,
}

/// Quote numbers only — no ids, nothing persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TradePreview {
    pub market: MarketId,
    pub side: Side,
    pub action: TradeAction,
    pub shares: MicroShares,
    pub gross: MicroUsd,
    pub fee: MicroUsd,
    pub avg_price_micro: i64,
    /// Config generation this preview quoted against (D25a): the public
    /// `PlaceTrade` contract echoes it as `expected_config_version`, and the
    /// fence point 409s iff a market/user-relevant key changed since.
    pub config_version: i64,
}

pub struct PreviewTrade<'a, Q: MarketQueries, C: Clock> {
    pub queries: &'a Q,
    pub clock: &'a C,
    pub rep_config: RepConfig,
    /// Lock-free generation stamp (D25a). Production default is the W1
    /// reconciler snapshot; until it lands,
    /// [`crate::ports::StaticConfigReads`] pins the 0008 seed generation.
    pub config: &'a dyn ConfigReads,
}

/// All-in gross price per whole share, floor-rounded — display only (the
/// same convention as the domain's buy quote).
pub(crate) fn avg_price_micro(gross: MicroUsd, shares: MicroShares) -> Result<i64, AppError> {
    if shares.0 <= 0 {
        return Err(AppError::Overflow);
    }
    let avg = i128::from(gross.0) * 1_000_000 / i128::from(shares.0);
    i64::try_from(avg).map_err(|_| AppError::Overflow)
}

impl<Q: MarketQueries, C: Clock> PreviewTrade<'_, Q, C> {
    /// # Errors
    /// [`AppError::Store`] for unknown markets; [`AppError::Amm`] when the
    /// domain rejects the quote.
    pub async fn execute(&self, cmd: PreviewTradeCmd) -> Result<TradePreview, AppError> {
        let config_version = self.config.current_generation().await?;
        let market = self.queries.market_by_ref(&cmd.market_ref).await?;
        // D25 Live voting-pause overlay (grok r3 NEW-3): the book never
        // trades against a frozen tape, so previews 423 too. Best-effort
        // snapshot read; the pause auto-expires at `tally_hidden_at` and
        // PlaceTrade stays authoritative at the fence point.
        if market.state == domain::market::MarketState::Live
            && self.clock.now() < market.tally_hidden_at
            && crate::ops::config::pause_in_force(
                self.config
                    .snapshot_value(&format!("voting_paused:{}", market.id.0))
                    .await?
                    .as_ref(),
            )
        {
            return Err(AppError::VotingPaused);
        }
        let pool_row = self.queries.pool(market.id).await?;
        let rep = self.queries.user_rep(cmd.user_id).await?;
        let outcome = match cmd.side {
            Side::Yes => market.yes_outcome,
            Side::No => market.no_outcome,
        };
        let is_flip = cmd.action == TradeAction::Sell
            && self.rep_config.discount_flip_window_secs != 0
            && self
                .queries
                .last_buy_at(cmd.user_id, outcome)
                .await?
                .is_some_and(|last| {
                    self.clock.now() - last
                        < time::Duration::seconds(
                            i64::try_from(self.rep_config.discount_flip_window_secs)
                                .unwrap_or(i64::MAX),
                        )
                });
        let override_bps = crate::money::FeeBpsOverride::parse(
            self.config
                .snapshot_value(&crate::money::fee_override_key(market.id))
                .await?
                .as_ref(),
        )?;
        let fee = domain::fee_policy::effective_fee(&domain::fee_policy::FeeContext {
            base_bps: override_bps.base_bps(pool_row.pool.fee.0),
            tier: rep.tier,
            discount_bp_by_tier: self.rep_config.fee_discount_bp_by_tier,
            min_fee_bps: self.rep_config.min_fee_bps,
            is_flip_within_window: is_flip,
        });
        match cmd.action {
            TradeAction::Buy => {
                let gross = MicroUsd(cmd.amount_micro);
                let q = domain::amm::quote_buy(&pool_row.pool, cmd.side, gross, fee)?;
                Ok(TradePreview {
                    market: market.id,
                    side: cmd.side,
                    action: cmd.action,
                    shares: q.shares_out,
                    gross,
                    fee: q.fee,
                    avg_price_micro: q.avg_price_micro_per_share,
                    config_version,
                })
            }
            TradeAction::Sell => {
                let shares = MicroShares(cmd.amount_micro);
                let q = domain::amm::quote_sell(&pool_row.pool, cmd.side, shares, fee)?;
                let gross = q
                    .collateral_out
                    .checked_add(q.fee)
                    .ok_or(AppError::Overflow)?;
                Ok(TradePreview {
                    market: market.id,
                    side: cmd.side,
                    action: cmd.action,
                    shares,
                    gross,
                    fee: q.fee,
                    avg_price_micro: avg_price_micro(gross, shares)?,
                    config_version,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::fakes::{FakeClock, InMemoryStore};
    use crate::ports::StaticConfigReads;
    use domain::market::MarketState;
    use domain::money::BasisPoints;
    use time::{Duration, OffsetDateTime};

    fn t0() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    fn seeded_store() -> (InMemoryStore, crate::model::MarketRow) {
        let store = InMemoryStore::new();
        let market = store
            .add_market(
                "will-it-rain",
                MarketState::Live,
                t0() + Duration::hours(2),
                t0() + Duration::hours(1),
                MicroShares(100_000_000),
                BasisPoints(100),
            )
            .unwrap();
        (store, market)
    }

    #[tokio::test]
    async fn preview_buy_matches_domain_quote_and_writes_nothing() {
        let (store, market) = seeded_store();
        let user = UserId(uuid::Uuid::new_v4());
        store.fund_user(user, MicroUsd(1)).unwrap();
        let before = store.snapshot();
        let clock = FakeClock::at(t0());
        let uc = PreviewTrade {
            queries: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
            config: &StaticConfigReads::default(),
        };
        let preview = uc
            .execute(PreviewTradeCmd {
                market_ref: "will-it-rain".to_string(),
                user_id: user,
                side: Side::Yes,
                action: TradeAction::Buy,
                amount_micro: 5_000_000,
            })
            .await
            .unwrap();

        let pool = domain::amm::Pool::new(
            MicroShares(100_000_000),
            MicroShares(100_000_000),
            BasisPoints(100),
        )
        .unwrap();
        let q = domain::amm::quote_buy(&pool, Side::Yes, MicroUsd(5_000_000), BasisPoints(100))
            .unwrap();
        assert_eq!(preview.market, market.id);
        assert_eq!(preview.shares, q.shares_out);
        assert_eq!(preview.gross, MicroUsd(5_000_000));
        assert_eq!(preview.fee, q.fee);
        assert_eq!(preview.avg_price_micro, q.avg_price_micro_per_share);
        assert_eq!(before, store.snapshot(), "preview must write nothing");
    }

    #[tokio::test]
    async fn preview_sell_matches_domain_quote() {
        let (store, market) = seeded_store();
        let user = UserId(uuid::Uuid::new_v4());
        store.fund_user(user, MicroUsd(1)).unwrap();
        let clock = FakeClock::at(t0());
        let uc = PreviewTrade {
            queries: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
            config: &StaticConfigReads::default(),
        };
        let preview = uc
            .execute(PreviewTradeCmd {
                market_ref: market.id.0.to_string(),
                user_id: user,
                side: Side::No,
                action: TradeAction::Sell,
                amount_micro: 3_000_000,
            })
            .await
            .unwrap();

        let pool = domain::amm::Pool::new(
            MicroShares(100_000_000),
            MicroShares(100_000_000),
            BasisPoints(100),
        )
        .unwrap();
        let q = domain::amm::quote_sell(&pool, Side::No, MicroShares(3_000_000), BasisPoints(100))
            .unwrap();
        let gross = MicroUsd(q.collateral_out.0 + q.fee.0);
        assert_eq!(preview.shares, MicroShares(3_000_000));
        assert_eq!(preview.gross, gross);
        assert_eq!(preview.fee, q.fee);
        assert_eq!(
            i128::from(preview.avg_price_micro),
            i128::from(gross.0) * 1_000_000 / 3_000_000
        );
    }

    #[tokio::test]
    async fn preview_unknown_market_is_not_found() {
        let (store, _) = seeded_store();
        let clock = FakeClock::at(t0());
        let uc = PreviewTrade {
            queries: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
            config: &StaticConfigReads::default(),
        };
        let err = uc
            .execute(PreviewTradeCmd {
                market_ref: "no-such-market".to_string(),
                user_id: UserId(uuid::Uuid::new_v4()),
                side: Side::Yes,
                action: TradeAction::Buy,
                amount_micro: 1_000_000,
            })
            .await
            .unwrap_err();
        assert_eq!(
            err,
            AppError::Store(crate::error::StoreError::NotFound("market"))
        );
    }

    #[tokio::test]
    async fn preview_stamps_the_config_generation_from_the_lock_free_port() {
        let (store, market) = seeded_store();
        let user = UserId(uuid::Uuid::new_v4());
        store.fund_user(user, MicroUsd(1)).unwrap();
        let clock = FakeClock::at(t0());
        let cmd = |action| PreviewTradeCmd {
            market_ref: market.id.0.to_string(),
            user_id: user,
            side: Side::Yes,
            action,
            amount_micro: 1_000_000,
        };
        // The Phase 6 pre-wave default pins the migration-0008 seed
        // generation on BOTH quote arms.
        let uc = PreviewTrade {
            queries: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
            config: &StaticConfigReads::default(),
        };
        assert_eq!(
            uc.execute(cmd(TradeAction::Buy))
                .await
                .unwrap()
                .config_version,
            1
        );
        // A later generation flows through unchanged (W1's reconciler seat).
        let later = StaticConfigReads(42);
        let uc = PreviewTrade {
            queries: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
            config: &later,
        };
        let buy = uc.execute(cmd(TradeAction::Buy)).await.unwrap();
        assert_eq!(buy.config_version, 42);
        // Sells quote against held shares in PlaceTrade only; the preview
        // itself still stamps the generation.
        let sell = cmd(TradeAction::Sell);
        assert_eq!(uc.execute(sell).await.unwrap().config_version, 42);
    }

    #[tokio::test]
    async fn a_failing_config_read_fails_the_preview_before_any_market_read() {
        struct FailingConfigReads;
        #[async_trait::async_trait]
        impl crate::ports::ConfigReads for FailingConfigReads {
            async fn current_generation(&self) -> Result<i64, crate::error::StoreError> {
                Err(crate::error::StoreError::Backend("config down".into()))
            }
        }
        let (store, _) = seeded_store();
        let clock = FakeClock::at(t0());
        let uc = PreviewTrade {
            queries: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
            config: &FailingConfigReads,
        };
        let err = uc
            .execute(PreviewTradeCmd {
                market_ref: "no-such-market".to_string(),
                user_id: UserId(uuid::Uuid::new_v4()),
                side: Side::Yes,
                action: TradeAction::Buy,
                amount_micro: 1_000_000,
            })
            .await
            .unwrap_err();
        assert_eq!(
            err,
            AppError::Store(crate::error::StoreError::Backend("config down".into()))
        );
    }

    #[tokio::test]
    async fn malformed_fee_override_fails_preview_closed() {
        struct CorruptConfigReads;
        #[async_trait::async_trait]
        impl crate::ports::ConfigReads for CorruptConfigReads {
            async fn current_generation(&self) -> Result<i64, crate::error::StoreError> {
                Ok(1)
            }

            async fn snapshot_value(
                &self,
                key: &str,
            ) -> Result<Option<serde_json::Value>, crate::error::StoreError> {
                Ok(key
                    .starts_with("fee_bps_override:")
                    .then(|| serde_json::json!(99)))
            }
        }
        let (store, market) = seeded_store();
        let user = UserId(uuid::Uuid::new_v4());
        store.fund_user(user, MicroUsd(1)).unwrap();
        let err = PreviewTrade {
            queries: &store,
            clock: &FakeClock::at(t0()),
            rep_config: RepConfig::default(),
            config: &CorruptConfigReads,
        }
        .execute(PreviewTradeCmd {
            market_ref: market.id.0.to_string(),
            user_id: user,
            side: Side::Yes,
            action: TradeAction::Buy,
            amount_micro: 1_000_000,
        })
        .await
        .unwrap_err();
        assert_eq!(
            err,
            AppError::Store(crate::error::StoreError::Invariant("invalid fee override"))
        );
    }

    #[test]
    fn average_price_rejects_non_positive_share_counts() {
        assert_eq!(
            avg_price_micro(MicroUsd(1), MicroShares(0)),
            Err(AppError::Overflow)
        );
        assert_eq!(
            avg_price_micro(MicroUsd(1), MicroShares(-1)),
            Err(AppError::Overflow)
        );
    }

    // D25 Live voting-pause overlay (grok r3 NEW-3): while the tape is
    // frozen, previews on that market 423 too — best-effort via the watch
    // snapshot; PlaceTrade stays authoritative at the fence point.
    #[tokio::test]
    async fn a_live_voting_pause_blocks_previews_until_the_hidden_window() {
        let (store, market) = seeded_store();
        let user = UserId(uuid::Uuid::new_v4());
        store.fund_user(user, MicroUsd(1)).unwrap();
        let watch = crate::ops::reconciler::ConfigWatch::new();
        watch.install(crate::ops::reconciler::ConfigSnapshot {
            generation: 2,
            entries: [(
                format!("voting_paused:{}", market.id.0),
                serde_json::json!(true),
            )]
            .into_iter()
            .collect(),
        });
        let clock = FakeClock::at(t0());
        let cmd = || PreviewTradeCmd {
            market_ref: "will-it-rain".to_string(),
            user_id: user,
            side: Side::Yes,
            action: TradeAction::Buy,
            amount_micro: 1_000_000,
        };
        let uc = PreviewTrade {
            queries: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
            config: &watch,
        };
        let err = uc.execute(cmd()).await.unwrap_err();
        assert_eq!(err, AppError::VotingPaused);
        // Auto-expiry: at `tally_hidden_at` the pause is void (D22 exactly).
        clock.set(t0() + Duration::hours(1));
        let preview = uc.execute(cmd()).await.unwrap();
        assert_eq!(preview.config_version, 2);
        // The static default (no snapshot) never pauses anything.
        let unpaused = PreviewTrade {
            queries: &store,
            clock: &FakeClock::at(t0()),
            rep_config: RepConfig::default(),
            config: &StaticConfigReads::default(),
        };
        unpaused.execute(cmd()).await.unwrap();
    }
}
