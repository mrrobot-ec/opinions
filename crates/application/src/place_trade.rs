//! `PlaceTrade` — the money-moving write path. The sequence is fixed and
//! guard-first (P1R2-final, rule 1): open `trade_tx` → `serialize_key` →
//! `txn_by_key` (hit → replay, write nothing) → `market_for_update` →
//! validate under the row lock → `user_voted` → `pool_for_update` → quote →
//! position (M5) → `ledger_apply` → `save_reserves` → `save_position` →
//! `insert_trade` → outbox → commit. Everything in ONE `TradeTx`; nothing
//! observable on error.

use domain::amm::Side;
use domain::ledger::{Currency, Entry, TxnKind};
use domain::market::MarketState;
use domain::money::{MicroShares, MicroUsd};
use serde_json::json;

use crate::error::{AppError, StoreError};
use crate::model::{
    Event, MarketId, NewTrade, OwnerRef, PositionRow, RealizationFact, RealizationSource,
    RepConfig, TradeAction, TradeReceipt, UserId,
};
use crate::ports::{Clock, Store};

#[derive(Debug, Clone)]
pub struct PlaceTradeCmd {
    pub market: MarketId,
    pub user: UserId,
    pub side: Side,
    pub action: TradeAction,
    /// Collateral micro-USD for buys; micro-shares for sells.
    pub amount_micro: i64,
    /// Saga/idempotency key: replays return the original receipt.
    pub idempotency_key: String,
    /// Agent causal chain, passed through to the trade row when supplied.
    pub run_id: Option<uuid::Uuid>,
    pub pending_action_id: Option<uuid::Uuid>,
    /// The preview's `config_version` echo (D25a public contract). Carried
    /// from Task 6.0a; W1 owns the fence-point staleness check and the
    /// idempotency request fingerprint that consume it.
    pub expected_config_version: Option<i64>,
}

pub struct PlaceTrade<'a, S: Store, C: Clock> {
    pub store: &'a S,
    pub clock: &'a C,
    pub rep_config: RepConfig,
}

/// One quoted trade, normalized across the buy/sell arms.
struct QuotedLegs {
    shares: MicroShares,
    gross: MicroUsd,
    fee: MicroUsd,
    avg_price_micro: i64,
    pool_after: domain::amm::Pool,
    /// (user delta, escrow delta, fees delta) in micro-USD.
    user_delta: i64,
    escrow_delta: i64,
    fee_delta: i64,
    position: PositionRow,
    realized_delta: Option<MicroUsd>,
}

impl<S: Store, C: Clock> PlaceTrade<'_, S, C> {
    /// # Errors
    /// Business rejections ([`AppError::MarketNotOpen`], `TradingFrozen`,
    /// `VoteRequired`, `InsufficientShares`), domain quote errors, and store
    /// failures. On any error nothing became observable.
    #[allow(clippy::too_many_lines)]
    pub async fn execute(&self, cmd: PlaceTradeCmd) -> Result<TradeReceipt, AppError> {
        // D36 is a use-case contract, not merely an HTTP DTO constraint.
        // Keeping this check ahead of opening a transaction also guarantees
        // an omitted version cannot acquire locks or trigger lazy money work.
        let expected_config_version = cmd
            .expected_config_version
            .ok_or_else(crate::money::missing_config_version)?;
        // Rule 1: guard-first, fixed sequence. There is NO post-unique-violation
        // recovery in PostgreSQL, so the key lock precedes every read.
        let mut tx = self.store.trade_tx().await?;
        tx.serialize_key(&cmd.idempotency_key).await?;
        let fingerprint = crate::ops::config::trade_fingerprint(
            cmd.market,
            cmd.user,
            cmd.side,
            cmd.action,
            cmd.amount_micro,
            cmd.expected_config_version,
        );
        if let Some(txn) = tx.txn_by_key(&cmd.idempotency_key).await? {
            // Replay precedence (D25, codex r3 NEW-3, pinned): compare the
            // persisted canonical fingerprint FIRST. A match returns the
            // original receipt WITHOUT any pause or config check; a mismatch
            // is a typed 409. Pre-0009 keys have no row and replay as before.
            let stored = tx.request_fingerprint(&cmd.idempotency_key).await?;
            crate::ops::config::validate_replay_fingerprint(stored.as_deref(), &fingerprint)?;
            let mut receipt = tx
                .trade_by_ledger_txn(txn)
                .await?
                .ok_or(StoreError::Invariant(
                    "idempotency key exists without a trade row",
                ))?;
            receipt.replayed = true;
            return Ok(receipt);
        }

        // Class-2 user lock precedes the market lock, matching settlement's
        // canonical user-lock order and making the tier read authoritative.
        tx.lock_user(cmd.user).await?;
        tx.convert_then_collect(cmd.user, self.clock.now(), &cmd.idempotency_key)
            .await?;
        crate::money::enforcement::enforce_money_mutation(
            tx.as_mut(),
            cmd.user,
            crate::money::enforcement::MoneyMutation::PlaceTrade,
            cmd.amount_micro,
            self.clock.now(),
        )
        .await?;
        let rep = tx.user_rep(cmd.user).await?;

        // Rule 3: authority checks under the market row lock, never via views.
        let market = tx.market_for_update(cmd.market).await?;
        match market.state {
            MarketState::Closing => return Err(AppError::TradingFrozen),
            MarketState::Live if self.clock.now() >= market.tally_hidden_at => {
                return Err(AppError::TradingFrozen);
            }
            MarketState::Live => {}
            MarketState::Draft
            | MarketState::Scheduled
            | MarketState::Closed
            | MarketState::Resolving
            | MarketState::Resolved
            | MarketState::Paid
            | MarketState::Voided => return Err(AppError::MarketNotOpen),
        }
        // Rule 4: the D4 vote-gate, in-tx.
        if !tx.user_voted(cmd.user, cmd.market).await? {
            return Err(AppError::VoteRequired);
        }

        // Rule 5: quote against the row-locked pool.
        let pool_row = tx.pool_for_update(cmd.market).await?;
        let outcome = match cmd.side {
            Side::Yes => market.yes_outcome,
            Side::No => market.no_outcome,
        };
        let last_buy_at = tx.last_buy_at(cmd.user, outcome).await?;
        let is_flip = cmd.action == TradeAction::Sell
            && self.rep_config.discount_flip_window_secs != 0
            && last_buy_at.is_some_and(|last| {
                self.clock.now() - last
                    < time::Duration::seconds(
                        i64::try_from(self.rep_config.discount_flip_window_secs)
                            .unwrap_or(i64::MAX),
                    )
            });
        let override_bps = crate::money::FeeBpsOverride::parse(
            tx.fence_config_value(&crate::money::fee_override_key(cmd.market))
                .await?
                .as_ref(),
        )?;
        let base_bps = override_bps.base_bps(pool_row.pool.fee.0);
        let effective_fee = domain::fee_policy::effective_fee(&domain::fee_policy::FeeContext {
            base_bps,
            tier: rep.tier,
            discount_bp_by_tier: self.rep_config.fee_discount_bp_by_tier,
            min_fee_bps: self.rep_config.min_fee_bps,
            is_flip_within_window: is_flip,
        });
        if cmd.action == TradeAction::Buy {
            let current = tx.market_position_cost(cmd.user, cmd.market).await?;
            let cap = self.rep_config.position_cap_micro_by_tier[usize::from(rep.tier.min(4))];
            let projected = current
                .0
                .checked_add(cmd.amount_micro)
                .ok_or(AppError::Overflow)?;
            if projected > cap {
                return Err(AppError::PositionCapExceeded {
                    cap_micro: cap,
                    tier: rep.tier,
                });
            }
        }
        let held = tx.position_for_update(cmd.user, outcome).await?;

        // D25 fence point: shared class-4 fences AFTER every row lock and
        // immediately before the first authoritative write (drain-safe,
        // codex r2 NEW-1). Requests still queued on the row locks hold NO
        // fence and observe the pause right here (typed 423).
        tx.acquire_shared_fences(&[
            crate::ops::config::trading_global_fence(),
            crate::ops::config::trading_market_fence(cmd.market),
        ])
        .await?;
        if crate::ops::config::pause_in_force(
            tx.fence_config_value("trading_paused").await?.as_ref(),
        ) || crate::ops::config::pause_in_force(
            tx.fence_config_value(&format!("market_paused:{}", cmd.market.0))
                .await?
                .as_ref(),
        ) {
            return Err(AppError::TradingPaused);
        }
        // Live voting-pause overlay (grok r3 NEW-3): the book never trades
        // against a frozen tape. The Live + pre-hidden guard above makes an
        // in-force pause necessarily unexpired here.
        if crate::ops::config::pause_in_force(
            tx.fence_config_value(&format!("voting_paused:{}", cmd.market.0))
                .await?
                .as_ref(),
        ) {
            return Err(AppError::VotingPaused);
        }
        // D25a staleness for previewed requests: 409 iff a market/user-
        // relevant key changed since the previewed generation; a pruned
        // history window is conservatively stale.
        let (current_generation, changes) = tx.fence_changes_since(expected_config_version).await?;
        let stale = match changes {
            None => true,
            Some(changes) => crate::ops::config::preview_relevant_drift(
                &changes,
                &crate::ops::config::StalenessContext {
                    user_tier: rep.tier,
                    pool_fee_bps: pool_row.pool.fee.0,
                    active_fee_override: override_bps,
                    rep: self.rep_config,
                    action: cmd.action,
                    secs_since_last_buy: last_buy_at
                        .map(|last| (self.clock.now() - last).whole_seconds()),
                    market: Some(cmd.market),
                },
            ),
        };
        if stale {
            return Err(AppError::StaleConfig {
                preview_generation: expected_config_version,
                current_generation,
            });
        }
        tx.save_request_fingerprint(&cmd.idempotency_key, &fingerprint)
            .await?;

        let legs = quote_legs(&cmd, &pool_row.pool, outcome, held, effective_fee)?;

        // Ledger mapping per Global Constraints; zero legs are omitted (the
        // domain and the DB both forbid zero entries; fee can be 0 bps).
        let user_acct = tx.account(OwnerRef::User(cmd.user), Currency::Usdc).await?;
        let escrow_acct = tx
            .account(OwnerRef::MarketEscrow(cmd.market), Currency::Usdc)
            .await?;
        let mut entries = vec![
            Entry {
                account: user_acct,
                amount: MicroUsd(legs.user_delta),
            },
            Entry {
                account: escrow_acct,
                amount: MicroUsd(legs.escrow_delta),
            },
        ];
        if legs.fee_delta != 0 {
            let fees_acct = tx.account(OwnerRef::Fees, Currency::Usdc).await?;
            entries.push(Entry {
                account: fees_acct,
                amount: MicroUsd(legs.fee_delta),
            });
        }
        let ledger_txn = tx
            .ledger_apply(TxnKind::Trade, &cmd.idempotency_key, &entries)
            .await?;

        if let Some(realized_delta) = legs.realized_delta {
            tx.insert_realization(&RealizationFact {
                user: cmd.user,
                market: cmd.market,
                outcome,
                source: RealizationSource::Sell,
                realized_delta,
                payout: MicroUsd(legs.user_delta),
                ledger_txn,
                created_at: self.clock.now(),
            })
            .await?;
        }

        tx.save_reserves(cmd.market, &legs.pool_after).await?;
        tx.save_position(legs.position).await?;
        let handle = tx.handle(cmd.user).await?;
        let inserted = tx
            .insert_trade(NewTrade {
                market: cmd.market,
                user: cmd.user,
                outcome,
                side: cmd.side,
                action: cmd.action,
                shares: legs.shares,
                gross: legs.gross,
                fee: legs.fee,
                avg_price_micro: legs.avg_price_micro,
                ledger_txn,
                run_id: cmd.run_id,
                pending_action_id: cmd.pending_action_id,
            })
            .await?;
        tx.append(trade_placed_event(&cmd, &handle, inserted, &legs))
            .await?;
        if legs.fee.0 > 0 {
            let lots = tx.lots_for_user(cmd.user).await?;
            let mut facts = Vec::new();
            for lot in &lots {
                facts.extend(tx.allocations_for_lot(lot.id).await?);
            }
            let planned = crate::money::credits::plan_trade_allocations(
                &lots,
                &facts,
                inserted.id.0,
                legs.fee.0,
            )
            .map_err(|_| StoreError::Invariant("credit allocation algebra"))?;
            for fact in planned {
                tx.insert_allocation(&fact).await?;
            }
        }

        // Rule 6: everything in ONE TradeTx.
        tx.commit().await?;
        Ok(TradeReceipt {
            trade_id: inserted.id,
            ledger_txn,
            side: cmd.side,
            action: cmd.action,
            shares: legs.shares,
            gross: legs.gross,
            fee: legs.fee,
            avg_price_micro: legs.avg_price_micro,
            replayed: false,
        })
    }
}

fn trade_placed_event(
    cmd: &PlaceTradeCmd,
    handle: &str,
    inserted: crate::model::InsertedTrade,
    legs: &QuotedLegs,
) -> Event {
    Event {
        event_type: "TradePlaced",
        aggregate_type: "market",
        aggregate_id: cmd.market.0,
        payload: json!({
            "trade_id": inserted.id.0.to_string(),
            "market_id": cmd.market.0.to_string(),
            "user_id": cmd.user.0.to_string(),
            "handle": handle,
            "side": format!("{:?}", cmd.side).to_ascii_lowercase(),
            "action": format!("{:?}", cmd.action).to_ascii_lowercase(),
            "shares_micro": legs.shares.0,
            "collateral_micro": legs.gross.0,
            "fee_micro": legs.fee.0,
            "trade_seq": inserted.trade_seq,
            "created_at": inserted.created_at,
        }),
    }
}

/// Applies the domain quote and the M5 position formula for one command.
fn quote_legs(
    cmd: &PlaceTradeCmd,
    pool: &domain::amm::Pool,
    outcome: crate::model::OutcomeId,
    held: Option<PositionRow>,
    fee: domain::money::BasisPoints,
) -> Result<QuotedLegs, AppError> {
    let mut position = held.unwrap_or(PositionRow {
        user: cmd.user,
        outcome,
        shares: MicroShares(0),
        cost: MicroUsd(0),
        realized_pnl: MicroUsd(0),
    });
    match cmd.action {
        TradeAction::Buy => {
            let gross = MicroUsd(cmd.amount_micro);
            let q = domain::amm::quote_buy(pool, cmd.side, gross, fee)?;
            let net = gross.checked_sub(q.fee).ok_or(AppError::Overflow)?;
            // M5 BUY: shares += q.shares_out; cost += gross.
            position.shares = MicroShares(
                position
                    .shares
                    .0
                    .checked_add(q.shares_out.0)
                    .ok_or(AppError::Overflow)?,
            );
            position.cost = position.cost.checked_add(gross).ok_or(AppError::Overflow)?;
            Ok(QuotedLegs {
                shares: q.shares_out,
                gross,
                fee: q.fee,
                avg_price_micro: q.avg_price_micro_per_share,
                pool_after: q.pool_after,
                user_delta: -gross.0,
                escrow_delta: net.0,
                fee_delta: q.fee.0,
                position,
                realized_delta: None,
            })
        }
        TradeAction::Sell => {
            let shares = MicroShares(cmd.amount_micro);
            // M5 SELL: require shares >= s else InsufficientShares.
            if position.shares.0 < shares.0 || shares.0 <= 0 {
                return Err(AppError::InsufficientShares);
            }
            let q = domain::amm::quote_sell(pool, cmd.side, shares, fee)?;
            let proceeds = q
                .collateral_out
                .checked_add(q.fee)
                .ok_or(AppError::Overflow)?;
            // M5 SELL: cost_relieved = floor(cost * s / shares);
            //          realized_pnl += net_proceeds - cost_relieved;
            //          shares -= s; cost -= cost_relieved.
            let relieved =
                i128::from(position.cost.0) * i128::from(shares.0) / i128::from(position.shares.0);
            let relieved = MicroUsd(i64::try_from(relieved).map_err(|_| AppError::Overflow)?);
            let pnl = q
                .collateral_out
                .checked_sub(relieved)
                .ok_or(AppError::Overflow)?;
            position.realized_pnl = position
                .realized_pnl
                .checked_add(pnl)
                .ok_or(AppError::Overflow)?;
            position.shares = MicroShares(position.shares.0 - shares.0);
            position.cost = position
                .cost
                .checked_sub(relieved)
                .ok_or(AppError::Overflow)?;
            Ok(QuotedLegs {
                shares,
                gross: proceeds,
                fee: q.fee,
                avg_price_micro: crate::preview_trade::avg_price_micro(proceeds, shares)?,
                pool_after: q.pool_after,
                user_delta: q.collateral_out.0,
                escrow_delta: -proceeds.0,
                fee_delta: q.fee.0,
                position,
                realized_delta: Some(pnl),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::fakes::{FakeClock, InMemoryStore};
    use crate::ports::MarketQueries;
    use crate::preview_trade::{PreviewTrade, PreviewTradeCmd};
    use domain::money::BasisPoints;
    use time::{Duration, OffsetDateTime};

    const RESERVES: i64 = 100_000_000;
    const FEE_BPS: u16 = 100;

    fn t0() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    fn seeded_live_market_with_voter() -> (InMemoryStore, FakeClock, MarketId, UserId) {
        let store = InMemoryStore::new();
        let market = store
            .add_market(
                "will-it-rain",
                domain::market::MarketState::Live,
                t0() + Duration::hours(2),
                t0() + Duration::hours(1),
                MicroShares(RESERVES),
                BasisPoints(FEE_BPS),
            )
            .unwrap();
        let user = UserId(uuid::Uuid::new_v4());
        store.fund_user(user, MicroUsd(100_000_000)).unwrap();
        store.record_vote(user, market.id, Side::Yes);
        (store, FakeClock::at(t0()), market.id, user)
    }

    fn buy_cmd(market: &MarketId, user: &UserId, amount: i64, key: &str) -> PlaceTradeCmd {
        PlaceTradeCmd {
            market: *market,
            user: *user,
            side: Side::Yes,
            action: TradeAction::Buy,
            amount_micro: amount,
            idempotency_key: key.to_string(),
            run_id: None,
            pending_action_id: None,
            expected_config_version: Some(1),
        }
    }

    fn sell_cmd(market: &MarketId, user: &UserId, shares: i64, key: &str) -> PlaceTradeCmd {
        PlaceTradeCmd {
            market: *market,
            user: *user,
            side: Side::Yes,
            action: TradeAction::Sell,
            amount_micro: shares,
            idempotency_key: key.to_string(),
            run_id: None,
            pending_action_id: None,
            expected_config_version: Some(1),
        }
    }

    fn fixture_pool() -> domain::amm::Pool {
        domain::amm::Pool::new(
            MicroShares(RESERVES),
            MicroShares(RESERVES),
            BasisPoints(FEE_BPS),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn omitted_expected_config_version_is_rejected_before_money_moves() {
        let (store, clock, market, user) = seeded_live_market_with_voter();
        let before = store.snapshot();
        let uc = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
        };
        let mut cmd = buy_cmd(&market, &user, 5_000_000, "missing-version");
        cmd.expected_config_version = None;
        let err = uc.execute(cmd).await.unwrap_err();
        assert_eq!(err, AppError::ExpectedConfigVersionRequired);
        assert_eq!(before, store.snapshot());
    }

    #[tokio::test]
    async fn malformed_fee_override_fails_trade_closed_before_money_moves() {
        let (store, clock, market, user) = seeded_live_market_with_voter();
        store.set_config_value(
            &crate::money::fee_override_key(market),
            serde_json::json!(99),
        );
        let before = store.snapshot();
        let error = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
        }
        .execute(buy_cmd(&market, &user, 5_000_000, "corrupt-fee-override"))
        .await
        .unwrap_err();
        assert_eq!(
            error,
            AppError::Store(StoreError::Invariant("invalid fee override"))
        );
        assert_eq!(before, store.snapshot());
    }

    #[tokio::test]
    async fn actual_trade_fee_allocates_across_the_users_credit_lot() {
        use crate::money::credits::{GrantCredit, GrantCreditCmd};
        use crate::money::{AllocationKind, GrantClass};
        use domain::ledger::{Currency, Entry, TxnKind};

        let (store, clock, market, user) = seeded_live_market_with_voter();
        let mut seed = store.credit_convert_tx().await.unwrap();
        let external = seed
            .account(OwnerRef::External, Currency::Usdc)
            .await
            .unwrap();
        let reserve = seed
            .account(OwnerRef::BonusReserve, Currency::Usdc)
            .await
            .unwrap();
        let external_credit = seed
            .account(OwnerRef::External, Currency::UsdcCredit)
            .await
            .unwrap();
        let house_credit = seed
            .account(OwnerRef::House, Currency::UsdcCredit)
            .await
            .unwrap();
        seed.ledger_apply(
            TxnKind::Seed,
            "allocation-reserve",
            &[
                Entry {
                    account: external,
                    amount: MicroUsd(-1_000_000),
                },
                Entry {
                    account: reserve,
                    amount: MicroUsd(1_000_000),
                },
            ],
        )
        .await
        .unwrap();
        seed.ledger_apply(
            TxnKind::CreditGrant,
            "allocation-house-credit",
            &[
                Entry {
                    account: external_credit,
                    amount: MicroUsd(-1_000_000),
                },
                Entry {
                    account: house_credit,
                    amount: MicroUsd(1_000_000),
                },
            ],
        )
        .await
        .unwrap();
        seed.commit().await.unwrap();
        let grant = GrantCredit { store: &store }
            .execute(GrantCreditCmd {
                user,
                amount: MicroUsd(1_000_000),
                source: "trade-allocation-test".into(),
                grant_class: GrantClass::RealMoney,
                policy_version: "1".into(),
                idempotency_key: "trade-allocation-grant".into(),
                granted_at: t0(),
            })
            .await
            .unwrap();
        let trade = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
        }
        .execute(buy_cmd(&market, &user, 5_000_000, "trade-allocation"))
        .await
        .unwrap();
        let mut read = store.credit_convert_tx().await.unwrap();
        let facts = read.allocations_for_lot(grant.lot_id).await.unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].trade_id, trade.trade_id.0);
        assert_eq!(facts[0].kind, AllocationKind::Allocated);
        assert_eq!(facts[0].amount_micro, trade.fee.0);
    }

    // Rule 1: replay returns the original result and writes nothing.
    #[tokio::test]
    async fn replayed_key_returns_original_and_writes_nothing() {
        let (store, clock, market, user) = seeded_live_market_with_voter();
        let uc = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
        };
        let cmd = buy_cmd(&market, &user, 5_000_000, "key-1");
        let first = uc.execute(cmd.clone()).await.unwrap();
        let snapshot = store.snapshot();
        let second = uc.execute(cmd).await.unwrap();
        assert!(second.replayed && first.trade_id == second.trade_id);
        assert_eq!(snapshot, store.snapshot()); // no double-spend, no reserve drift
        assert!(!first.replayed);
        assert_eq!(
            TradeReceipt {
                replayed: false,
                ..second
            },
            first,
            "replay must return the original receipt"
        );
    }

    // Rule 1 contract: two concurrent identical commands → exactly one write,
    // two equal receipts (loser serialized behind the key lock and replayed).
    #[tokio::test]
    async fn concurrent_identical_commands_write_once_with_equal_receipts() {
        let (store, clock, market, user) = seeded_live_market_with_voter();
        let uc = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
        };
        let cmd = buy_cmd(&market, &user, 5_000_000, "key-race");
        let (a, b) = tokio::join!(uc.execute(cmd.clone()), uc.execute(cmd));
        let (a, b) = (a.unwrap(), b.unwrap());
        assert!(a.replayed ^ b.replayed, "exactly one execution may write");
        assert_eq!(
            TradeReceipt {
                replayed: false,
                ..a
            },
            TradeReceipt {
                replayed: false,
                ..b
            },
            "both callers must observe the same receipt"
        );
        let trades: Vec<_> = store.outbox();
        assert_eq!(trades.len(), 1, "exactly one TradePlaced event");
    }

    // Rule 5: quote against the row-locked pool, fixed ledger mapping,
    // reserves saved, position updated per M5 (BUY arm).
    #[tokio::test]
    async fn buy_moves_money_reserves_position_and_emits_event() {
        let (store, clock, market, user) = seeded_live_market_with_voter();
        let uc = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
        };
        let gross = 5_000_000;
        let receipt = uc
            .execute(buy_cmd(&market, &user, gross, "key-buy"))
            .await
            .unwrap();

        let q = domain::amm::quote_buy(
            &fixture_pool(),
            Side::Yes,
            MicroUsd(gross),
            BasisPoints(FEE_BPS),
        )
        .unwrap();
        assert_eq!(receipt.shares, q.shares_out);
        assert_eq!(receipt.gross, MicroUsd(gross));
        assert_eq!(receipt.fee, q.fee);
        assert_eq!(receipt.avg_price_micro, q.avg_price_micro_per_share);
        assert_eq!(receipt.side, Side::Yes);
        assert_eq!(receipt.action, TradeAction::Buy);
        assert!(!receipt.replayed);

        // Ledger mapping (BUY): user −gross · fees +fee · escrow +net.
        let net = gross - q.fee.0;
        assert_eq!(
            store.balance_of(OwnerRef::User(user), Currency::Usdc),
            Some(MicroUsd(100_000_000 - gross))
        );
        assert_eq!(
            store.balance_of(OwnerRef::Fees, Currency::Usdc),
            Some(MicroUsd(q.fee.0))
        );
        assert_eq!(
            store.balance_of(OwnerRef::MarketEscrow(market), Currency::Usdc),
            Some(MicroUsd(net))
        );

        // Reserves = quote's pool_after.
        let pool_row = store.pool(market).await.unwrap();
        assert_eq!(pool_row.pool, q.pool_after);

        // Position per M5 BUY: shares += shares_out; cost += gross.
        let positions = store.positions(user).await.unwrap();
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].shares, q.shares_out);
        assert_eq!(positions[0].cost, MicroUsd(gross));
        assert_eq!(positions[0].realized_pnl, MicroUsd(0));
        assert_eq!(positions[0].side, Side::Yes);
        assert!(store.realizations().is_empty(), "buys do not realize PnL");

        // Outbox: TradePlaced committed with the state.
        let events = store.outbox();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "TradePlaced");
        assert_eq!(events[0].aggregate_type, "market");
        assert_eq!(events[0].aggregate_id, market.0);

        // The trade row exists and reconstructs the receipt.
        let stored = store.stored_trade(receipt.trade_id).unwrap();
        assert_eq!(stored.ledger_txn, receipt.ledger_txn);
        assert_eq!(stored.run_id, None);
    }

    // M5 SELL arm: partial sell relieves floor-proportional cost; full-path
    // pnl accounting; ledger mapping SELL: escrow −proceeds · user +net ·
    // fees +fee.
    #[tokio::test]
    async fn sell_partial_applies_m5_formula() {
        let (store, clock, market, user) = seeded_live_market_with_voter();
        let uc = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
        };
        let gross = 5_000_000;
        let buy = uc
            .execute(buy_cmd(&market, &user, gross, "key-buy"))
            .await
            .unwrap();
        let buy_quote = domain::amm::quote_buy(
            &fixture_pool(),
            Side::Yes,
            MicroUsd(gross),
            BasisPoints(FEE_BPS),
        )
        .unwrap();

        let sold = buy.shares.0 / 2;
        let receipt = uc
            .execute(sell_cmd(&market, &user, sold, "key-sell"))
            .await
            .unwrap();

        let q = domain::amm::quote_sell(
            &buy_quote.pool_after,
            Side::Yes,
            MicroShares(sold),
            BasisPoints(FEE_BPS),
        )
        .unwrap();
        let proceeds = q.collateral_out.0 + q.fee.0;
        assert_eq!(receipt.gross, MicroUsd(proceeds));
        assert_eq!(receipt.fee, q.fee);
        assert_eq!(receipt.shares, MicroShares(sold));
        assert_eq!(receipt.action, TradeAction::Sell);

        // M5: cost_relieved = floor(cost * s / shares).
        let cost_relieved = i128::from(gross) * i128::from(sold) / i128::from(buy.shares.0);
        let cost_relieved = i64::try_from(cost_relieved).unwrap();
        let positions = store.positions(user).await.unwrap();
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].shares, MicroShares(buy.shares.0 - sold));
        assert_eq!(positions[0].cost, MicroUsd(gross - cost_relieved));
        assert_eq!(
            positions[0].realized_pnl,
            MicroUsd(q.collateral_out.0 - cost_relieved)
        );

        // Ledger mapping (SELL): escrow −proceeds · user +net · fees +fee.
        let buy_net = gross - buy_quote.fee.0;
        assert_eq!(
            store.balance_of(OwnerRef::MarketEscrow(market), Currency::Usdc),
            Some(MicroUsd(buy_net - proceeds))
        );
        assert_eq!(
            store.balance_of(OwnerRef::User(user), Currency::Usdc),
            Some(MicroUsd(100_000_000 - gross + q.collateral_out.0))
        );
        assert_eq!(
            store.balance_of(OwnerRef::Fees, Currency::Usdc),
            Some(MicroUsd(buy_quote.fee.0 + q.fee.0))
        );

        // Reserves follow the sell quote.
        let pool_row = store.pool(market).await.unwrap();
        assert_eq!(pool_row.pool, q.pool_after);

        let facts = store.realizations();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].user, user);
        assert_eq!(facts[0].market, market);
        assert_eq!(facts[0].source, RealizationSource::Sell);
        assert_eq!(
            facts[0].realized_delta,
            MicroUsd(q.collateral_out.0 - cost_relieved)
        );
        assert_eq!(facts[0].payout, q.collateral_out);
        assert_eq!(facts[0].ledger_txn, receipt.ledger_txn);
    }

    // M5 SELL arm: require shares >= s else InsufficientShares — and nothing
    // observable on the failure (rule 6).
    #[tokio::test]
    async fn sell_beyond_position_is_rejected_without_trace() {
        let (store, clock, market, user) = seeded_live_market_with_voter();
        let uc = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
        };
        let buy = uc
            .execute(buy_cmd(&market, &user, 5_000_000, "key-buy"))
            .await
            .unwrap();
        let snapshot = store.snapshot();
        let err = uc
            .execute(sell_cmd(&market, &user, buy.shares.0 + 1, "key-oversell"))
            .await
            .unwrap_err();
        assert_eq!(err, AppError::InsufficientShares);
        assert_eq!(snapshot, store.snapshot());
    }

    #[tokio::test]
    async fn sell_without_position_is_rejected() {
        let (store, clock, market, user) = seeded_live_market_with_voter();
        let uc = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
        };
        let snapshot = store.snapshot();
        let err = uc
            .execute(sell_cmd(&market, &user, 1_000_000, "key-naked"))
            .await
            .unwrap_err();
        assert_eq!(err, AppError::InsufficientShares);
        assert_eq!(snapshot, store.snapshot());
    }

    // Rule 4: the D4 vote-gate, checked in-tx.
    #[tokio::test]
    async fn unvoted_user_cannot_trade() {
        let (store, clock, market, _) = seeded_live_market_with_voter();
        let stranger = UserId(uuid::Uuid::new_v4());
        store.fund_user(stranger, MicroUsd(50_000_000)).unwrap();
        let snapshot = store.snapshot();
        let uc = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
        };
        let err = uc
            .execute(buy_cmd(&market, &stranger, 5_000_000, "key-unvoted"))
            .await
            .unwrap_err();
        assert_eq!(err, AppError::VoteRequired);
        assert_eq!(snapshot, store.snapshot());
    }

    // Rule 3: Closing is the explicit frozen state (HTTP 423).
    #[tokio::test]
    async fn non_live_market_rejects_trades() {
        let (store, clock, market, user) = seeded_live_market_with_voter();
        store.set_market_state(market, MarketState::Closing);
        let snapshot = store.snapshot();
        let uc = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
        };
        let err = uc
            .execute(buy_cmd(&market, &user, 5_000_000, "key-closed"))
            .await
            .unwrap_err();
        assert_eq!(err, AppError::TradingFrozen);
        assert_eq!(snapshot, store.snapshot());
    }

    #[tokio::test]
    async fn settled_market_is_not_open_for_trading() {
        let (store, clock, market, user) = seeded_live_market_with_voter();
        store.set_market_state(market, MarketState::Resolved);
        let error = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
        }
        .execute(buy_cmd(&market, &user, 5_000_000, "key-resolved"))
        .await
        .unwrap_err();
        assert_eq!(error, AppError::MarketNotOpen);
        assert!(store.outbox().is_empty());
    }

    // Rule 3, the race shape: market freezes between a stale preview and the
    // transaction — PlaceTrade must still reject (D22 full freeze).
    #[tokio::test]
    async fn freeze_race_stale_preview_still_rejects() {
        let (store, clock, market, user) = seeded_live_market_with_voter();
        let preview = PreviewTrade {
            queries: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
            config: &crate::ports::StaticConfigReads::default(),
        };
        preview
            .execute(PreviewTradeCmd {
                market_ref: market.0.to_string(),
                user_id: user,
                side: Side::Yes,
                action: TradeAction::Buy,
                amount_micro: 5_000_000,
            })
            .await
            .unwrap(); // stale preview succeeds pre-freeze
        clock.advance(Duration::hours(1)); // now == tally_hidden_at: frozen
        let snapshot = store.snapshot();
        let uc = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
        };
        let err = uc
            .execute(buy_cmd(&market, &user, 5_000_000, "key-frozen"))
            .await
            .unwrap_err();
        assert_eq!(err, AppError::TradingFrozen);
        assert_eq!(snapshot, store.snapshot());
    }

    // Rule 6: a mid-sequence store failure leaves nothing observable.
    #[tokio::test]
    async fn insufficient_funds_leaves_no_trace() {
        let (store, clock, market, _) = seeded_live_market_with_voter();
        let pauper = UserId(uuid::Uuid::new_v4());
        store.fund_user(pauper, MicroUsd(1)).unwrap();
        store.record_vote(pauper, market, Side::No);
        let snapshot = store.snapshot();
        let uc = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
        };
        let err = uc
            .execute(buy_cmd(&market, &pauper, 5_000_000, "key-poor"))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            AppError::Store(StoreError::Ledger(
                domain::ledger::LedgerError::InsufficientFunds { .. }
            ))
        ));
        assert_eq!(snapshot, store.snapshot());
    }

    #[tokio::test]
    async fn unknown_market_is_not_found() {
        let (store, clock, _, user) = seeded_live_market_with_voter();
        let ghost = MarketId(uuid::Uuid::new_v4());
        let uc = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
        };
        let err = uc
            .execute(buy_cmd(&ghost, &user, 5_000_000, "key-ghost"))
            .await
            .unwrap_err();
        assert_eq!(err, AppError::Store(StoreError::NotFound("market")));
    }

    // Rule 7: run_id / pending_action_id pass through to the trade row.
    #[tokio::test]
    async fn agent_causal_chain_passes_through() {
        let (store, clock, market, user) = seeded_live_market_with_voter();
        let uc = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
        };
        let run_id = uuid::Uuid::new_v4();
        let pending_action_id = uuid::Uuid::new_v4();
        let mut cmd = buy_cmd(&market, &user, 5_000_000, "key-agent");
        cmd.run_id = Some(run_id);
        cmd.pending_action_id = Some(pending_action_id);
        let receipt = uc.execute(cmd).await.unwrap();
        let stored = store.stored_trade(receipt.trade_id).unwrap();
        assert_eq!(stored.run_id, Some(run_id));
        assert_eq!(stored.pending_action_id, Some(pending_action_id));
    }

    // NO-side buys touch the other reserve; sanity-check the mapping is not
    // YES-only.
    #[tokio::test]
    async fn no_side_buy_updates_the_no_position() {
        let (store, clock, market, user) = seeded_live_market_with_voter();
        let uc = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
        };
        let mut cmd = buy_cmd(&market, &user, 2_000_000, "key-no");
        cmd.side = Side::No;
        let receipt = uc.execute(cmd).await.unwrap();
        let q = domain::amm::quote_buy(
            &fixture_pool(),
            Side::No,
            MicroUsd(2_000_000),
            BasisPoints(FEE_BPS),
        )
        .unwrap();
        assert_eq!(receipt.shares, q.shares_out);
        let positions = store.positions(user).await.unwrap();
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].side, Side::No);
    }

    #[tokio::test]
    async fn aggregate_position_cap_counts_both_sides_and_is_inclusive() {
        let (store, clock, market, user) = seeded_live_market_with_voter();
        let config = RepConfig {
            position_cap_micro_by_tier: [5_000_000; 5],
            ..RepConfig::default()
        };
        let uc = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: config,
        };
        uc.execute(buy_cmd(&market, &user, 3_000_000, "cap-yes"))
            .await
            .unwrap();
        let mut no = buy_cmd(&market, &user, 2_000_000, "cap-no");
        no.side = Side::No;
        uc.execute(no).await.unwrap();
        let mut over = buy_cmd(&market, &user, 1, "cap-over");
        over.side = Side::No;
        assert_eq!(
            uc.execute(over).await,
            Err(AppError::PositionCapExceeded {
                cap_micro: 5_000_000,
                tier: 0,
            })
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn fee_preview_execute_and_flip_boundary_share_one_policy() {
        let store = InMemoryStore::new();
        let now = OffsetDateTime::now_utc();
        let market = store
            .add_market(
                "fee-window",
                MarketState::Live,
                now + Duration::hours(2),
                now + Duration::hours(1),
                MicroShares(RESERVES),
                BasisPoints(FEE_BPS),
            )
            .unwrap()
            .id;
        let user = UserId(uuid::Uuid::new_v4());
        store.fund_user(user, MicroUsd(100_000_000)).unwrap();
        store.record_vote(user, market, Side::Yes);
        let clock = FakeClock::at(now);
        store.set_user_rep(user, 500_000, 2);
        let config = RepConfig {
            fee_discount_bp_by_tier: [0, 10, 40, 50, 60],
            min_fee_bps: 10,
            discount_flip_window_secs: 60,
            ..RepConfig::default()
        };
        let preview = PreviewTrade {
            queries: &store,
            clock: &clock,
            rep_config: config,
            config: &crate::ports::StaticConfigReads::default(),
        }
        .execute(PreviewTradeCmd {
            market_ref: market.0.to_string(),
            user_id: user,
            side: Side::Yes,
            action: TradeAction::Buy,
            amount_micro: 5_000_000,
        })
        .await
        .unwrap();
        let buy = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: config,
        }
        .execute(buy_cmd(&market, &user, 5_000_000, "fee-buy"))
        .await
        .unwrap();
        assert_eq!(preview.fee, buy.fee);
        assert_eq!(store.stored_trade(buy.trade_id).unwrap().fee, buy.fee);

        let outcome = store
            .market_by_ref(&market.0.to_string())
            .await
            .unwrap()
            .yes_outcome;
        let last_buy = store.last_buy_at(user, outcome).await.unwrap().unwrap();
        clock.set(last_buy + Duration::seconds(59));
        let recent_preview = PreviewTrade {
            queries: &store,
            clock: &clock,
            rep_config: config,
            config: &crate::ports::StaticConfigReads::default(),
        }
        .execute(PreviewTradeCmd {
            market_ref: market.0.to_string(),
            user_id: user,
            side: Side::Yes,
            action: TradeAction::Sell,
            amount_micro: buy.shares.0 / 4,
        })
        .await
        .unwrap();
        let recent = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: config,
        }
        .execute(sell_cmd(
            &market,
            &user,
            buy.shares.0 / 4,
            "fee-recent-sell",
        ))
        .await
        .unwrap();
        assert_eq!(recent_preview.fee, recent.fee);

        clock.set(last_buy + Duration::seconds(60));
        let after = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: config,
        }
        .execute(sell_cmd(
            &market,
            &user,
            buy.shares.0 / 4,
            "fee-boundary-sell",
        ))
        .await
        .unwrap();
        assert!(
            recent.fee.0 > after.fee.0,
            "exact boundary earns the discount"
        );
    }

    // ---- D25 fence point + replay precedence + D25a staleness ----

    #[tokio::test]
    async fn trading_pause_is_423_and_replay_takes_precedence_over_it() {
        let (store, clock, market, user) = seeded_live_market_with_voter();
        let uc = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
        };
        let first = uc
            .execute(buy_cmd(&market, &user, 5_000_000, "key-pre-pause"))
            .await
            .unwrap();
        store.set_config_value("trading_paused", serde_json::json!(true));
        let snapshot = store.snapshot();
        let err = uc
            .execute(buy_cmd(&market, &user, 5_000_000, "key-during-pause"))
            .await
            .unwrap_err();
        assert_eq!(err, AppError::TradingPaused);
        assert_eq!(snapshot, store.snapshot(), "423 leaves nothing observable");
        // Replay-after-pause (codex r3 NEW-3, pinned): a key HIT returns the
        // original receipt WITHOUT any pause or config check.
        let replay = uc
            .execute(buy_cmd(&market, &user, 5_000_000, "key-pre-pause"))
            .await
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.trade_id, first.trade_id);
    }

    #[tokio::test]
    async fn a_market_scoped_pause_blocks_only_that_market() {
        let (store, clock, paused_market, user) = seeded_live_market_with_voter();
        let other = store
            .add_market(
                "still-open",
                domain::market::MarketState::Live,
                t0() + Duration::hours(2),
                t0() + Duration::hours(1),
                MicroShares(RESERVES),
                BasisPoints(FEE_BPS),
            )
            .unwrap();
        store.record_vote(user, other.id, Side::Yes);
        store.set_config_value(
            &format!("market_paused:{}", paused_market.0),
            serde_json::json!(true),
        );
        let uc = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
        };
        let err = uc
            .execute(buy_cmd(&paused_market, &user, 5_000_000, "key-scoped"))
            .await
            .unwrap_err();
        assert_eq!(err, AppError::TradingPaused);
        uc.execute(buy_cmd(&other.id, &user, 5_000_000, "key-open"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_live_voting_pause_also_freezes_the_book() {
        // grok r3 NEW-3 residual: the book never trades against a frozen
        // tape — while `voting_paused` is in force during Live, trades 423.
        let (store, clock, market, user) = seeded_live_market_with_voter();
        store.set_config_value(
            &format!("voting_paused:{}", market.0),
            serde_json::json!(true),
        );
        let uc = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
        };
        let err = uc
            .execute(buy_cmd(&market, &user, 5_000_000, "key-frozen-tape"))
            .await
            .unwrap_err();
        assert_eq!(err, AppError::VotingPaused);
    }

    #[tokio::test]
    async fn the_same_key_with_a_different_payload_is_a_409_conflict() {
        let (store, clock, market, user) = seeded_live_market_with_voter();
        let uc = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
        };
        uc.execute(buy_cmd(&market, &user, 5_000_000, "key-fp"))
            .await
            .unwrap();
        // Different amount.
        let err = uc
            .execute(buy_cmd(&market, &user, 6_000_000, "key-fp"))
            .await
            .unwrap_err();
        assert_eq!(err, AppError::IdempotencyConflict);
        // Different expected_config_version — part of the pinned fingerprint.
        let mut versioned = buy_cmd(&market, &user, 5_000_000, "key-fp");
        versioned.expected_config_version = Some(2);
        let err = uc.execute(versioned).await.unwrap_err();
        assert_eq!(err, AppError::IdempotencyConflict);
        // The exact original payload still replays.
        let replay = uc
            .execute(buy_cmd(&market, &user, 5_000_000, "key-fp"))
            .await
            .unwrap();
        assert!(replay.replayed);
    }

    #[tokio::test]
    async fn relevant_config_drift_since_the_preview_is_a_409_and_replay_precedes_it() {
        let (store, clock, market, user) = seeded_live_market_with_voter();
        let uc = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
        };
        // A trade previewed at generation 1 and executed BEFORE the drift.
        let mut pre_drift = buy_cmd(&market, &user, 5_000_000, "key-pre-drift");
        pre_drift.expected_config_version = Some(1);
        let receipt = uc.execute(pre_drift.clone()).await.unwrap();
        // This user's effective position cap changes (tier 0 entry).
        store.set_config_value(
            "position_cap_micro_by_tier",
            serde_json::json!([
                30_000_000i64,
                50_000_000i64,
                100_000_000i64,
                250_000_000i64,
                500_000_000i64
            ]),
        );
        // A FRESH request previewed at the stale generation 409s…
        let mut stale = buy_cmd(&market, &user, 5_000_000, "key-stale");
        stale.expected_config_version = Some(1);
        let err = uc.execute(stale).await.unwrap_err();
        assert_eq!(
            err,
            AppError::StaleConfig {
                preview_generation: 1,
                current_generation: 2
            }
        );
        // …but the replay of the pre-drift execution returns the original
        // receipt with no config check (replay-after-relevant-drift).
        let replay = uc.execute(pre_drift).await.unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.trade_id, receipt.trade_id);
        // A preview at the CURRENT generation is not stale.
        let mut fresh = buy_cmd(&market, &user, 1_000_000, "key-fresh");
        fresh.expected_config_version = Some(2);
        uc.execute(fresh).await.unwrap();
    }

    #[tokio::test]
    async fn flip_window_churn_does_not_409_a_non_flip_buy() {
        let (store, clock, market, user) = seeded_live_market_with_voter();
        store.set_config_value("discount_flip_window_secs", serde_json::json!(7200));
        let uc = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
        };
        let mut cmd = buy_cmd(&market, &user, 5_000_000, "key-churn");
        cmd.expected_config_version = Some(1);
        uc.execute(cmd).await.unwrap();
    }

    #[tokio::test]
    async fn a_preview_older_than_the_watermark_is_conservatively_stale() {
        let (store, clock, market, user) = seeded_live_market_with_voter();
        store.set_config_value("sweep_delay_secs", serde_json::json!(240)); // gen 2
        store.set_config_value("sweep_delay_secs", serde_json::json!(300)); // gen 3
        store.set_config_watermark(3); // history before gen 3 is pruned
        let uc = PlaceTrade {
            store: &store,
            clock: &clock,
            rep_config: RepConfig::default(),
        };
        let mut cmd = buy_cmd(&market, &user, 5_000_000, "key-ancient");
        cmd.expected_config_version = Some(1);
        let err = uc.execute(cmd).await.unwrap_err();
        assert_eq!(
            err,
            AppError::StaleConfig {
                preview_generation: 1,
                current_generation: 3
            }
        );
    }
}
