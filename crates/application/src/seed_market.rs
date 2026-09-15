//! `SeedMarket` — admin-side market creation (grok-p1r1 B3). Lifecycle
//! contract (P1R2 B3): creates the market in `Draft`, applies `Approve`
//! internally, and returns it in `Scheduled` — exactly one stated
//! post-state; `seed.rs` then advances `Scheduled → Live` via
//! `AdvanceMarket(GoLive)`. The SEED ledger transaction (house −S, escrow
//! +S) backs the minted pool inventory: S micro-USD of collateral backs S
//! complete sets held by the pool. Precondition: house balance ≥ S (else
//! [`AppError::InsufficientFunds`] — run `EnsureGenesis` first).

use domain::amm::Pool;
use domain::ledger::{Currency, Entry, LedgerError, TxnKind};
use domain::market::{transition, MarketEvent, MarketState};
use domain::money::{BasisPoints, MicroShares, MicroUsd};
use serde_json::json;
use time::OffsetDateTime;

use crate::error::{AppError, StoreError};
use crate::model::{Event, LpKillConfig, MarketId, NewMarket, OwnerRef, RepConfig};
use crate::ports::{Clock, Store};

#[derive(Debug, Clone)]
pub struct SeedMarketCmd {
    /// Caller-generated market id — this is what makes the seed replayable
    /// without a reader role on `SeedTx`.
    pub market_id: MarketId,
    pub slug: String,
    pub min_votes_to_resolve: i32,
    pub closes_at: OffsetDateTime,
    pub tally_hidden_at: OffsetDateTime,
    pub fee: BasisPoints,
    /// S: the seed collateral in micro-USD; also the minted reserves in
    /// micro-shares per side.
    pub seed: MicroUsd,
    pub idempotency_key: String,
    /// Explicit admin override for the seed-time-only LP loss breaker.
    pub force: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeedReceipt {
    pub market: MarketId,
    pub replayed: bool,
}

pub struct SeedMarket<'a, S: Store + ?Sized, C: Clock + ?Sized> {
    pub store: &'a S,
    pub clock: &'a C,
    pub rep_config: RepConfig,
    pub lp_kill_config: LpKillConfig,
}

impl<S: Store + ?Sized, C: Clock + ?Sized> SeedMarket<'_, S, C> {
    /// # Errors
    /// [`AppError::InsufficientFunds`] when the house cannot cover S,
    /// [`AppError::Amm`] for an unseedable pool (S ≤ 0), and store failures.
    pub async fn execute(&self, cmd: SeedMarketCmd) -> Result<SeedReceipt, AppError> {
        if cmd.seed.0 <= 0 {
            return Err(AppError::Amm(domain::amm::AmmError::EmptyPool));
        }
        if cmd.fee.0 < self.rep_config.min_fee_bps {
            return Err(AppError::SeedFeeBelowMinimum {
                base_bps: cmd.fee.0,
                min_bps: self.rep_config.min_fee_bps,
            });
        }
        let mut tx = self.store.seed_tx().await?;
        tx.serialize_key(&cmd.idempotency_key).await?;
        if let Some(market) = tx.seeded_market_by_key(&cmd.idempotency_key).await? {
            return Ok(SeedReceipt {
                market,
                replayed: true,
            });
        }
        tx.lock_lp_kill_switch().await?;
        let now = self.clock.now();
        let window_days = i64::from(self.lp_kill_config.window_days);
        let since = now - time::Duration::days(window_days);
        let lp_pnl = tx.lp_pnl_sum(since, now).await?;
        let tripped = lp_pnl.0 <= -self.lp_kill_config.max_loss_micro;
        if tripped && !cmd.force {
            return Err(AppError::LpPaused);
        }
        if tripped {
            tx.append(Event {
                event_type: "LpKillOverride",
                aggregate_type: "market",
                aggregate_id: cmd.market_id.0,
                payload: json!({
                    "lp_pnl_micro": lp_pnl.0,
                    "max_loss_micro": self.lp_kill_config.max_loss_micro,
                    "window_days": self.lp_kill_config.window_days,
                    "seed_time_only": true,
                }),
            })
            .await?;
        }
        let market = tx
            .insert_market(NewMarket {
                id: cmd.market_id,
                slug: cmd.slug.clone(),
                min_votes_to_resolve: cmd.min_votes_to_resolve,
                closes_at: cmd.closes_at,
                tally_hidden_at: cmd.tally_hidden_at,
            })
            .await?;
        tx.create_pool(market, cmd.fee, cmd.seed).await?;
        // SEED ledger mapping: house −S · escrow +S. The ledger itself is the
        // house-balance precondition (grok-p1r1 B3).
        let house = tx.account(OwnerRef::House, Currency::Usdc).await?;
        let escrow = tx
            .account(OwnerRef::MarketEscrow(market), Currency::Usdc)
            .await?;
        tx.ledger_apply(
            TxnKind::Seed,
            &cmd.idempotency_key,
            &[
                Entry {
                    account: house,
                    amount: MicroUsd(-cmd.seed.0),
                },
                Entry {
                    account: escrow,
                    amount: cmd.seed,
                },
            ],
        )
        .await
        .map_err(map_seed_ledger_error)?;
        // Mint the pool inventory: S micro-USD of collateral backs S complete
        // sets held by the pool.
        let pool = Pool::new(MicroShares(cmd.seed.0), MicroShares(cmd.seed.0), cmd.fee)?;
        tx.save_reserves(market, &pool).await?;
        // Lifecycle contract (P1R2 B3): Draft → Approve → returns Scheduled.
        let scheduled = transition(MarketState::Draft, MarketEvent::Approve)
            .map_err(|_| AppError::IllegalTransition)?;
        tx.set_market_state(market, scheduled).await?;
        tx.append(Event {
            event_type: "MarketSeeded",
            aggregate_type: "market",
            aggregate_id: market.0,
            payload: json!({
                "slug": cmd.slug,
                "seed_micro": cmd.seed.0,
                "fee_bps": cmd.fee.0,
            }),
        })
        .await?;
        tx.commit().await?;
        Ok(SeedReceipt {
            market,
            replayed: false,
        })
    }
}

fn map_seed_ledger_error(error: StoreError) -> AppError {
    match error {
        StoreError::Ledger(LedgerError::InsufficientFunds { .. }) => AppError::InsufficientFunds,
        other => AppError::Store(other),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::ensure_genesis::{EnsureGenesis, EnsureGenesisCmd};
    use crate::fakes::InMemoryStore;
    use crate::ports::MarketQueries;
    use time::Duration;

    fn t0() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    fn seed_cmd(seed: i64, key: &str) -> SeedMarketCmd {
        SeedMarketCmd {
            market_id: MarketId(uuid::Uuid::new_v4()),
            slug: "seeded-market".to_string(),
            min_votes_to_resolve: 3,
            closes_at: t0() + Duration::hours(2),
            tally_hidden_at: t0() + Duration::hours(1),
            fee: BasisPoints(100),
            seed: MicroUsd(seed),
            idempotency_key: key.to_string(),
            force: false,
        }
    }

    async fn capitalized_store(amount: i64) -> InMemoryStore {
        let store = InMemoryStore::new();
        EnsureGenesis { store: &store }
            .execute(EnsureGenesisCmd {
                currency: Currency::Usdc,
                amount: MicroUsd(amount),
            })
            .await
            .unwrap();
        store
    }

    #[tokio::test]
    async fn seed_creates_scheduled_market_with_pool_escrow_and_event() {
        let store = capitalized_store(10_000_000_000).await;
        let clock = crate::fakes::FakeClock::at(t0());
        let uc = SeedMarket {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
            lp_kill_config: LpKillConfig::default(),
        };
        let cmd = seed_cmd(1_000_000_000, "seed-1");
        let market_id = cmd.market_id;
        let receipt = uc.execute(cmd).await.unwrap();
        assert_eq!(receipt.market, market_id);
        assert!(!receipt.replayed);

        assert_eq!(store.market_state(market_id), Some(MarketState::Scheduled));
        let row = store.market_by_ref("seeded-market").await.unwrap();
        assert_eq!(row.id, market_id);
        assert_eq!(row.min_votes_to_resolve, 3);

        // Pool inventory: S micro-shares per side at the configured fee.
        let pool = store.pool(market_id).await.unwrap().pool;
        assert_eq!(pool.yes, MicroShares(1_000_000_000));
        assert_eq!(pool.no, MicroShares(1_000_000_000));
        assert_eq!(pool.fee, BasisPoints(100));

        // SEED ledger mapping: house −S · escrow +S.
        assert_eq!(
            store.balance_of(OwnerRef::House, Currency::Usdc),
            Some(MicroUsd(9_000_000_000))
        );
        assert_eq!(
            store.balance_of(OwnerRef::MarketEscrow(market_id), Currency::Usdc),
            Some(MicroUsd(1_000_000_000))
        );

        let seeded: Vec<_> = store
            .outbox()
            .into_iter()
            .filter(|e| e.event_type == "MarketSeeded")
            .collect();
        assert_eq!(seeded.len(), 1);
        assert_eq!(seeded[0].aggregate_id, market_id.0);
    }

    #[tokio::test]
    async fn seed_replay_echoes_the_market_and_writes_nothing() {
        let store = capitalized_store(10_000_000_000).await;
        let clock = crate::fakes::FakeClock::at(t0());
        let uc = SeedMarket {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
            lp_kill_config: LpKillConfig::default(),
        };
        let mut cmd = seed_cmd(1_000_000_000, "seed-1");
        let first = uc.execute(cmd.clone()).await.unwrap();
        let snapshot = store.snapshot();
        // A racing saga may retry with a different in-memory id; replay is
        // authoritative from the committed seed record.
        cmd.market_id = MarketId(uuid::Uuid::new_v4());
        let second = uc.execute(cmd).await.unwrap();
        assert!(second.replayed);
        assert_eq!(second.market, first.market);
        assert_eq!(snapshot, store.snapshot());
    }

    #[tokio::test]
    async fn seeding_an_uncapitalized_house_fails_without_trace() {
        let store = InMemoryStore::new();
        let snapshot = store.snapshot();
        let clock = crate::fakes::FakeClock::at(t0());
        let uc = SeedMarket {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
            lp_kill_config: LpKillConfig::default(),
        };
        let err = uc
            .execute(seed_cmd(1_000_000_000, "seed-poor"))
            .await
            .unwrap_err();
        assert_eq!(err, AppError::InsufficientFunds);
        assert_eq!(snapshot, store.snapshot());
    }

    #[tokio::test]
    async fn zero_seed_is_rejected() {
        let store = capitalized_store(10_000).await;
        let clock = crate::fakes::FakeClock::at(t0());
        let uc = SeedMarket {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
            lp_kill_config: LpKillConfig::default(),
        };
        let err = uc.execute(seed_cmd(0, "seed-zero")).await.unwrap_err();
        assert_eq!(err, AppError::Amm(domain::amm::AmmError::EmptyPool));
    }

    #[tokio::test]
    async fn seeded_base_fee_must_meet_the_configured_floor() {
        let store = capitalized_store(10_000).await;
        let snapshot = store.snapshot();
        let clock = crate::fakes::FakeClock::at(t0());
        let uc = SeedMarket {
            store: &store,
            clock: &clock,
            rep_config: RepConfig {
                min_fee_bps: 101,
                ..RepConfig::default()
            },
            lp_kill_config: LpKillConfig::default(),
        };
        assert_eq!(
            uc.execute(seed_cmd(1_000, "below-fee-floor"))
                .await
                .unwrap_err(),
            AppError::SeedFeeBelowMinimum {
                base_bps: 100,
                min_bps: 101,
            }
        );
        assert_eq!(snapshot, store.snapshot());
    }

    #[tokio::test]
    async fn lp_breaker_is_inclusive_race_safe_and_force_is_audited() {
        let store = capitalized_store(10_000).await;
        let clock = crate::fakes::FakeClock::at(t0());
        store.record_lp_result(
            MarketId(uuid::Uuid::new_v4()),
            MicroUsd(-100),
            t0() - Duration::days(1),
        );
        let uc = SeedMarket {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
            lp_kill_config: LpKillConfig {
                max_loss_micro: 100,
                window_days: 7,
            },
        };
        let a = seed_cmd(1_000, "paused-a");
        let b = seed_cmd(1_000, "paused-b");
        let (a, b) = tokio::join!(uc.execute(a), uc.execute(b));
        assert_eq!(a.unwrap_err(), AppError::LpPaused);
        assert_eq!(b.unwrap_err(), AppError::LpPaused);

        let mut forced = seed_cmd(1_000, "forced");
        forced.force = true;
        let market = forced.market_id;
        assert!(!uc.execute(forced).await.unwrap().replayed);
        assert_eq!(store.market_state(market), Some(MarketState::Scheduled));
        let events: Vec<_> = store
            .outbox()
            .into_iter()
            .filter(|event| event.aggregate_id == market.0)
            .collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, "LpKillOverride");
        assert_eq!(events[0].payload["seed_time_only"], true);
        assert_eq!(events[1].event_type, "MarketSeeded");
    }

    #[test]
    fn non_balance_seed_failures_remain_store_errors() {
        let error = StoreError::Backend("offline".to_string());
        assert_eq!(
            map_seed_ledger_error(error),
            AppError::Store(StoreError::Backend("offline".to_string()))
        );
    }
}
