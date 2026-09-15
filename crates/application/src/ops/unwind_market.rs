//! D30 — dual-controlled true unwind. Legal only from `Voided` or
//! flagged-`Resolving` pre-payout, NEVER from `Paid`. The apply is ONE
//! transaction regardless of participant count (Phase 3 precedent): every
//! original ledger transaction of the market is negated into a single
//! `Reversal` transaction keyed `unwind:<market>`; users short of cash pay
//! what they have and the House books the shortfall leg while a non-cash
//! receivable opens (identity 3 stays cash-exact). A `Voided` market's
//! already-committed neutral payout and its dust legs reverse too, with
//! compensating realization facts and the LP result re-zeroed.

use domain::ledger::{Entry, TxnKind};
use domain::market::{MarketEvent, MarketState};
use serde_json::json;
use uuid::Uuid;

use crate::error::AppError;
use crate::model::{
    AdminContext, Event, MarketId, MarketUnwind, OwnerRef, RealizationFact, RealizationSource,
    Receivable, ReceivableMovement, ReceivableMovementKind, UnwindStage, UserId,
};
use crate::ports::{Clock, LedgerEntryFacts, Store};

use super::audit::{principal_digest, required_audit_for, OpsError, OpsPolicy};

/// The single reversal ledger key for one market's unwind.
#[must_use]
pub fn unwind_ledger_key(market: MarketId) -> String {
    format!("unwind:{}", market.0)
}

/// The namespaced authority key (`unwind:<market>:<client key>` retains the
/// proposer's idempotency key for replay detection).
#[must_use]
pub fn unwind_key(market: MarketId, client_key: &str) -> String {
    format!("unwind:{}:{client_key}", market.0)
}

#[derive(Debug, Clone)]
pub struct UnwindCmd {
    pub market: MarketId,
    pub reason: String,
    pub idempotency_key: String,
}

pub struct UnwindMarket<'a, S: Store, C: Clock> {
    pub store: &'a S,
    pub clock: &'a C,
    pub policy: OpsPolicy,
    pub actor: AdminContext,
}

impl<S: Store, C: Clock> UnwindMarket<'_, S, C> {
    /// Stage 1: dual-control proposal (superadmin per the RBAC matrix).
    ///
    /// # Errors
    /// [`AppError::ProposalConflict`] when an unwind authority already exists
    /// with a different key; legality errors as on confirm (checked early so
    /// a `Paid` market can never even hold a proposal).
    pub async fn propose(&self, cmd: UnwindCmd) -> Result<MarketUnwind, OpsError> {
        let proposer = principal_digest(&self.actor)?.to_string();
        let mut tx = self.store.unwind_tx().await?;
        tx.serialize_key(&format!("unwind-propose:{}", cmd.market.0))
            .await?;
        let market = tx
            .market_for_update(cmd.market)
            .await
            .map_err(AppError::from)?;
        check_unwindable(market.state, market.curator_flagged_at.is_some())?;
        if let Some(existing) = tx.unwind_for_update(cmd.market).await? {
            // Idempotent replay of the same proposal; anything else conflicts.
            if existing.unwind_key == unwind_key(cmd.market, &cmd.idempotency_key)
                && existing.stage == UnwindStage::Proposed
            {
                return Ok(existing);
            }
            return Err(OpsError::App(AppError::ProposalConflict(
                "unwind already exists for this market",
            )));
        }
        let now = self.clock.now();
        let unwind = MarketUnwind {
            market: cmd.market,
            unwind_key: unwind_key(cmd.market, &cmd.idempotency_key),
            stage: UnwindStage::Proposed,
            proposer_token_id: proposer.clone(),
            confirmer_token_id: None,
            reason: cmd.reason.clone(),
            confirm_not_before: self.policy.confirm_not_before(now),
            reversal_txn: None,
        };
        tx.insert_unwind(unwind.clone()).await?;
        let row = required_audit_for(
            &self.actor,
            "unwind_propose",
            format!("market:{}", cmd.market.0),
            Some(json!({ "state": format!("{:?}", market.state) })),
            Some(json!({ "confirm_not_before": unwind.confirm_not_before.to_string() })),
            Some(cmd.reason),
        );
        tx.audit_insert(row).await?;
        tx.commit().await?;
        Ok(unwind)
    }

    /// Stage 2: distinct-principal confirm — executes the one-transaction
    /// reversal atomically with the stage change and the second audit.
    ///
    /// # Errors
    /// [`AppError::ProposalConflict`] for same-principal, early, expired or
    /// mis-staged confirms; [`AppError::IllegalTransition`] when the market
    /// left the unwindable set (`Paid` included); [`OpsError::OverCap`] when
    /// house receivable outstanding would pass the pinned threshold.
    #[allow(clippy::too_many_lines)]
    pub async fn confirm(&self, cmd: UnwindCmd) -> Result<MarketUnwind, OpsError> {
        let confirmer = principal_digest(&self.actor)?.to_string();
        let mut tx = self.store.unwind_tx().await?;
        tx.serialize_key(&format!("unwind-apply:{}", cmd.market.0))
            .await?;
        let market = tx
            .market_for_update(cmd.market)
            .await
            .map_err(AppError::from)?;
        let mut unwind = tx
            .unwind_for_update(cmd.market)
            .await?
            .ok_or(AppError::ProposalConflict("no unwind proposed"))?;
        if unwind.stage == UnwindStage::Applied {
            // Idempotent replay of a completed unwind.
            return Ok(unwind);
        }
        if unwind.stage != UnwindStage::Proposed {
            return Err(OpsError::App(AppError::ProposalConflict(
                "unwind is not pending",
            )));
        }
        if unwind.proposer_token_id == confirmer {
            return Err(OpsError::App(AppError::ProposalConflict(
                "confirm requires a distinct principal",
            )));
        }
        let now = self.clock.now();
        if now < unwind.confirm_not_before {
            return Err(OpsError::App(AppError::ProposalConflict(
                "dual-control delay has not elapsed",
            )));
        }
        if now > self.policy.expires_at(unwind.confirm_not_before) {
            unwind.stage = UnwindStage::Expired;
            tx.save_unwind(&unwind).await?;
            let row = required_audit_for(
                &self.actor,
                "unwind_expire",
                format!("market:{}", cmd.market.0),
                None,
                None,
                Some(cmd.reason),
            );
            tx.audit_insert(row).await?;
            tx.commit().await?;
            return Err(OpsError::App(AppError::ProposalConflict("unwind expired")));
        }
        check_unwindable(market.state, market.curator_flagged_at.is_some())?;
        if market.state == MarketState::Resolving {
            // Pre-payout gate: a settled resolution key means money moved.
            let paid = tx
                .txn_by_key(&crate::resolve_market::resolve_key(cmd.market))
                .await?;
            if paid.is_some() {
                return Err(OpsError::App(AppError::IllegalTransition));
            }
        }
        // House-receivable outstanding cap (422 past the pinned threshold).
        let outstanding = tx.open_receivables_total().await?;
        if outstanding >= self.policy.receivable_outstanding_cap_micro {
            return Err(OpsError::OverCap {
                cap_micro: self.policy.receivable_outstanding_cap_micro,
            });
        }

        // ---- one-transaction reversal ----
        let originals = tx.market_ledger_txns(cmd.market).await?;
        let mut net: std::collections::HashMap<domain::ledger::AccountId, (OwnerRef, i64)> =
            std::collections::HashMap::new();
        for original in &originals {
            if original.kind == TxnKind::Reversal {
                return Err(OpsError::App(AppError::ProposalConflict(
                    "market already carries a reversal",
                )));
            }
            for entry in &original.entries {
                let slot = net.entry(entry.account).or_insert((entry.owner, 0));
                slot.1 = slot
                    .1
                    .checked_sub(entry.amount_micro)
                    .ok_or(AppError::Overflow)?;
            }
        }
        let house = tx
            .account(OwnerRef::House, domain::ledger::Currency::Usdc)
            .await?;
        let mut entries: Vec<Entry> = Vec::new();
        let house_amount: i64 = net.remove(&house).map_or(0, |(_, amount)| amount);
        let mut shortfalls: Vec<(UserId, i64)> = Vec::new();
        let mut net: Vec<_> = net.into_iter().collect();
        net.sort_by_key(|(account, _)| account.0);
        for (account, (owner, amount)) in net {
            if amount == 0 {
                continue;
            }
            if let OwnerRef::User(user) = owner {
                if amount < 0 {
                    let cash = tx.account_balance(account).await?.0;
                    let debit = amount.checked_neg().ok_or(AppError::Overflow)?;
                    let pay = debit.min(cash.max(0));
                    let shortfall = debit - pay;
                    if pay > 0 {
                        entries.push(Entry {
                            account,
                            amount: domain::money::MicroUsd(-pay),
                        });
                    }
                    if shortfall > 0 {
                        shortfalls.push((user, shortfall));
                    }
                    continue;
                }
            }
            entries.push(Entry {
                account,
                amount: domain::money::MicroUsd(amount),
            });
        }
        // The house carries its negated original legs (seed return) and a
        // SEPARATE negative leg per booked shortfall total — identity 7 reads
        // the reversal's negative House entries as the shortfall record.
        if house_amount != 0 {
            entries.push(Entry {
                account: house,
                amount: domain::money::MicroUsd(house_amount),
            });
        }
        let shortfall_total = shortfall_total(&shortfalls)?;
        if shortfall_total > 0 {
            entries.push(Entry {
                account: house,
                amount: domain::money::MicroUsd(-shortfall_total),
            });
        }
        let reversal_txn = if entries.is_empty() {
            None
        } else {
            Some(
                tx.ledger_apply(TxnKind::Reversal, &unwind_ledger_key(cmd.market), &entries)
                    .await
                    .map_err(AppError::from)?,
            )
        };
        for original in &originals {
            tx.record_reversal(
                original.txn,
                reversal_txn.unwrap_or(Uuid::nil()),
                cmd.market,
            )
            .await?;
        }
        // Shortfall receivables (non-cash subledger; identity 7 traces per
        // origin reversal transaction).
        if let Some(reversal) = reversal_txn {
            for (user, shortfall) in &shortfalls {
                let receivable = Receivable {
                    id: Uuid::new_v4(),
                    market: cmd.market,
                    user: *user,
                    origin_reversal_txn: reversal,
                    opened_micro: *shortfall,
                };
                tx.insert_receivable(receivable).await?;
                tx.insert_receivable_movement(ReceivableMovement {
                    id: Uuid::new_v4(),
                    receivable: receivable.id,
                    kind: ReceivableMovementKind::Opened,
                    amount_micro: *shortfall,
                    actor: confirmer.clone(),
                    cash_txn: None,
                    idempotency_key: format!("unwind:{}:recv:{}", cmd.market.0, user.0),
                })
                .await?;
            }
        }
        // Positions zeroed; realized PnL compensated with immutable facts.
        let positions = tx.positions_for_market(cmd.market).await?;
        for position in positions {
            match unwind_realization(&position, cmd.market, reversal_txn, now) {
                Some(fact) => tx.insert_realization(&fact).await?,
                None => false,
            };
            let mut zeroed = position;
            zeroed.shares = domain::money::MicroShares(0);
            zeroed.cost = domain::money::MicroUsd(0);
            zeroed.realized_pnl = domain::money::MicroUsd(0);
            tx.save_position(zeroed).await?;
        }
        // Voided-unwind LP compensation: the neutral payout's LP result is
        // re-zeroed in the same transaction (no EWMA rewrite anywhere).
        tx.set_lp_result(cmd.market, domain::money::MicroUsd(0), now)
            .await?;
        // Hold cancelled; curator flag cleared; market tombstoned in Voided.
        tx.set_integrity_due_at(cmd.market, None).await?;
        tx.clear_curator_flag(cmd.market).await?;
        if market.state == MarketState::Resolving {
            let voided = domain::market::transition(market.state, MarketEvent::VoidByAdmin)
                .map_err(|_| AppError::IllegalTransition)?;
            tx.set_market_state(cmd.market, voided).await?;
        }
        // D32: every live provisional fee allocation of the unwound market
        // gains its `reversed` terminal child in this same transaction.
        tx.reverse_market_fee_allocations(cmd.market).await?;
        unwind.stage = UnwindStage::Applied;
        unwind.confirmer_token_id = Some(confirmer.clone());
        unwind.reversal_txn = reversal_txn;
        tx.save_unwind(&unwind).await?;
        tx.append(Event {
            event_type: "MarketUnwound",
            aggregate_type: "market",
            aggregate_id: cmd.market.0,
            payload: json!({
                "market_id": cmd.market.0.to_string(),
                "reversal_txn": reversal_txn.map(|t| t.to_string()),
                "receivables_opened": shortfalls.len(),
            }),
        })
        .await?;
        let row = required_audit_for(
            &self.actor,
            "unwind_confirm",
            format!("market:{}", cmd.market.0),
            Some(json!({ "state": format!("{:?}", market.state) })),
            Some(json!({
                "reversal_txn": reversal_txn.map(|t| t.to_string()),
                "receivables_opened": shortfalls.len(),
            })),
            Some(cmd.reason),
        );
        tx.audit_insert(row).await?;
        tx.commit().await?;
        Ok(unwind)
    }

    /// Rejects a pending proposal (either dual-control principal; audited).
    ///
    /// # Errors
    /// [`AppError::ProposalConflict`] when no pending proposal exists.
    pub async fn reject(&self, cmd: UnwindCmd) -> Result<MarketUnwind, OpsError> {
        principal_digest(&self.actor)?;
        let mut tx = self.store.unwind_tx().await?;
        tx.serialize_key(&format!("unwind-apply:{}", cmd.market.0))
            .await?;
        let mut unwind = tx
            .unwind_for_update(cmd.market)
            .await?
            .ok_or(AppError::ProposalConflict("no unwind proposed"))?;
        if unwind.stage != UnwindStage::Proposed {
            return Err(OpsError::App(AppError::ProposalConflict(
                "unwind is not pending",
            )));
        }
        unwind.stage = UnwindStage::Rejected;
        tx.save_unwind(&unwind).await?;
        let row = required_audit_for(
            &self.actor,
            "unwind_reject",
            format!("market:{}", cmd.market.0),
            None,
            None,
            Some(cmd.reason),
        );
        tx.audit_insert(row).await?;
        tx.commit().await?;
        Ok(unwind)
    }
}

fn unwind_realization(
    position: &crate::model::PositionRow,
    market: MarketId,
    reversal: Option<Uuid>,
    now: time::OffsetDateTime,
) -> Option<RealizationFact> {
    reversal
        .filter(|_| position.realized_pnl.0 != 0)
        .map(|ledger_txn| RealizationFact {
            user: position.user,
            market,
            outcome: position.outcome,
            source: RealizationSource::Void,
            realized_delta: domain::money::MicroUsd(-position.realized_pnl.0),
            payout: domain::money::MicroUsd(0),
            ledger_txn,
            created_at: now,
        })
}

/// The D30 legality set: `Voided`, or flagged `Resolving` (payout gate is
/// checked separately against the resolution ledger key). ops.md: void is
/// terminal settlement; unwind-from-`Voided` means "void was the WRONG fraud
/// decision", never t0 cleanup.
fn check_unwindable(state: MarketState, curator_flagged: bool) -> Result<(), OpsError> {
    match state {
        MarketState::Voided => Ok(()),
        MarketState::Resolving if curator_flagged => Ok(()),
        _ => Err(OpsError::App(AppError::IllegalTransition)),
    }
}

fn shortfall_total(shortfalls: &[(UserId, i64)]) -> Result<i64, AppError> {
    shortfalls.iter().try_fold(0_i64, |total, (_, shortfall)| {
        total.checked_add(*shortfall).ok_or(AppError::Overflow)
    })
}

/// Extracts the shortfall house legs of a reversal for identity 7 (the
/// negative House entries; seed returns are positive).
#[must_use]
pub fn house_shortfall_micro(entries: &[LedgerEntryFacts]) -> i64 {
    entries
        .iter()
        .filter(|entry| entry.owner == OwnerRef::House && entry.amount_micro < 0)
        .map(|entry| entry.amount_micro.saturating_neg())
        .sum()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::too_many_lines)]
    use super::*;
    use crate::advance_market::{AdvanceMarket, AdvanceMarketCmd};
    use crate::credit_deposit::{CreditDeposit, CreditDepositCmd};
    use crate::fakes::{FakeClock, InMemoryStore};
    use crate::model::{AdminRole, IntegritySweepConfig, RepConfig, ResolveConfig, TradeAction};
    use crate::place_trade::{PlaceTrade, PlaceTradeCmd};
    use crate::ports::{NoopCrashPoint, OpsQueries, WithdrawalEligibility};
    use crate::resolve_market::{ResolveMarket, ResolveMarketCmd};
    use domain::amm::Side;
    use domain::ledger::Currency;
    use domain::market::MarketState;
    use domain::money::{BasisPoints, MicroShares, MicroUsd};
    use time::{Duration, OffsetDateTime};

    const START: i64 = 1_700_000_000;

    fn superadmin() -> AdminContext {
        AdminContext::Admin {
            token_digest: "digest-superadmin".into(),
            role: AdminRole::Superadmin,
        }
    }

    fn finance() -> AdminContext {
        AdminContext::Admin {
            token_digest: "digest-finance".into(),
            role: AdminRole::Finance,
        }
    }

    struct Fx {
        store: InMemoryStore,
        clock: FakeClock,
        market: MarketId,
        alice: UserId,
        bob: UserId,
    }

    impl Fx {
        fn t0() -> OffsetDateTime {
            OffsetDateTime::from_unix_timestamp(START).unwrap()
        }

        async fn new() -> Self {
            let store = InMemoryStore::new();
            let clock = FakeClock::at(Self::t0());
            crate::ensure_genesis::EnsureGenesis { store: &store }
                .execute(crate::ensure_genesis::EnsureGenesisCmd {
                    currency: Currency::Usdc,
                    amount: MicroUsd(1_000_000_000),
                })
                .await
                .unwrap();
            let market = Self::seed_market(&store, &clock, "unwindable").await;
            let alice = store.add_user("alice", Self::t0() - Duration::days(90), 1);
            let bob = store.add_user("bob", Self::t0() - Duration::days(90), 1);
            store.fund_user(alice, MicroUsd(100_000_000)).unwrap();
            store.fund_user(bob, MicroUsd(100_000_000)).unwrap();
            store.record_vote(alice, market, Side::Yes);
            store.record_vote(bob, market, Side::Yes);
            Self {
                store,
                clock,
                market,
                alice,
                bob,
            }
        }

        /// Seeds through the production path (house → pool via the ledger)
        /// so the unwind's seed-return arm is exercised.
        async fn seed_market(store: &InMemoryStore, clock: &FakeClock, slug: &str) -> MarketId {
            let market = MarketId(uuid::Uuid::new_v4());
            crate::seed_market::SeedMarket {
                store,
                clock,
                rep_config: RepConfig::default(),
                lp_kill_config: crate::model::LpKillConfig::default(),
            }
            .execute(crate::seed_market::SeedMarketCmd {
                market_id: market,
                slug: format!("{slug}-{}", market.0),
                min_votes_to_resolve: 3,
                closes_at: Self::t0() + Duration::hours(2),
                tally_hidden_at: Self::t0() + Duration::hours(1),
                fee: BasisPoints(100),
                seed: MicroUsd(100_000_000),
                idempotency_key: format!("seed-{}", market.0),
                force: false,
            })
            .await
            .unwrap();
            AdvanceMarket { store }
                .execute(AdvanceMarketCmd {
                    market,
                    event: domain::market::MarketEvent::GoLive,
                    idempotency_key: format!("golive-{}", market.0),
                })
                .await
                .unwrap();
            market
        }

        async fn buy(&self, user: UserId, amount: i64, key: &str) {
            self.trade(self.market, user, TradeAction::Buy, amount, key)
                .await;
        }

        async fn trade(
            &self,
            market: MarketId,
            user: UserId,
            action: TradeAction,
            amount: i64,
            key: &str,
        ) {
            PlaceTrade {
                store: &self.store,
                clock: &self.clock,
                rep_config: RepConfig::default(),
            }
            .execute(PlaceTradeCmd {
                market,
                user,
                side: Side::Yes,
                action,
                amount_micro: amount,
                idempotency_key: key.to_string(),
                run_id: None,
                pending_action_id: None,
                expected_config_version: Some(1),
            })
            .await
            .unwrap();
        }

        async fn close(&self) {
            for (event, key) in [
                (domain::market::MarketEvent::EnterCloseWindow, "fx-closing"),
                (domain::market::MarketEvent::Close, "fx-closed"),
            ] {
                AdvanceMarket { store: &self.store }
                    .execute(AdvanceMarketCmd {
                        market: self.market,
                        event,
                        idempotency_key: key.to_string(),
                    })
                    .await
                    .unwrap();
            }
        }

        /// Voids the market (2 votes < 3 minimum; OI floor pinned above the
        /// book) — the neutral payout + dust commit like production.
        async fn void(&self) {
            let receipt = ResolveMarket {
                store: &self.store,
                clock: &self.clock,
                config: ResolveConfig {
                    oi_floor: MicroUsd(i64::MAX),
                },
                rep_config: RepConfig::default(),
                integrity_config: IntegritySweepConfig {
                    payout_hold_threshold_micro: i64::MAX,
                    ..IntegritySweepConfig::default()
                },
                crash_point: &NoopCrashPoint,
                actor: AdminContext::Machine,
            }
            .execute(ResolveMarketCmd {
                market: self.market,
                curator_override: None,
            })
            .await
            .unwrap();
            assert!(matches!(
                receipt.outcome,
                crate::resolve_market::ResolveOutcome::Voided
            ));
            assert_eq!(
                self.store.market_state(self.market),
                Some(MarketState::Voided)
            );
        }

        fn uc(&self, actor: AdminContext) -> UnwindMarket<'_, InMemoryStore, FakeClock> {
            UnwindMarket {
                store: &self.store,
                clock: &self.clock,
                policy: OpsPolicy::default(),
                actor,
            }
        }

        fn cmd(&self, key: &str) -> UnwindCmd {
            UnwindCmd {
                market: self.market,
                reason: "fraud decision was wrong".to_string(),
                idempotency_key: key.to_string(),
            }
        }

        fn cash(&self, user: UserId) -> i64 {
            self.store
                .balance_of(crate::model::OwnerRef::User(user), Currency::Usdc)
                .map_or(0, |b| b.0)
        }
    }

    #[tokio::test]
    async fn voided_unwind_restores_cash_exactly_and_reverses_the_neutral_payout() {
        let fx = Fx::new().await;
        fx.buy(fx.alice, 30_000_000, "buy-alice").await;
        fx.buy(fx.bob, 20_000_000, "buy-bob").await;
        fx.close().await;
        fx.void().await;
        let realized_user = fx
            .store
            .add_user("realized", Fx::t0() - Duration::days(90), 1);
        let zero_user = fx
            .store
            .add_user("zero-realized", Fx::t0() - Duration::days(90), 1);
        let outcome = {
            let mut tx = fx.store.unwind_tx().await.unwrap();
            let outcome = tx.market_for_update(fx.market).await.unwrap().yes_outcome;
            tx.save_position(crate::model::PositionRow {
                user: realized_user,
                outcome,
                shares: MicroShares(0),
                cost: MicroUsd(0),
                realized_pnl: MicroUsd(5),
            })
            .await
            .unwrap();
            tx.save_position(crate::model::PositionRow {
                user: zero_user,
                outcome,
                shares: MicroShares(0),
                cost: MicroUsd(0),
                realized_pnl: MicroUsd(0),
            })
            .await
            .unwrap();
            tx.commit().await.unwrap();
            outcome
        };
        assert_ne!(outcome.0, uuid::Uuid::nil());
        // The neutral payout moved money; balances are NOT the originals.
        let escrow_after_void = fx
            .store
            .balance_of(
                crate::model::OwnerRef::MarketEscrow(fx.market),
                Currency::Usdc,
            )
            .unwrap();
        assert_eq!(escrow_after_void, MicroUsd(0), "void drained escrow");

        let proposed = fx.uc(superadmin()).propose(fx.cmd("k1")).await.unwrap();
        assert_eq!(proposed.stage, UnwindStage::Proposed);
        // Same-principal confirm is refused (schema-grade dual control).
        fx.clock.advance(Duration::seconds(61));
        let same = fx.uc(superadmin()).confirm(fx.cmd("k1")).await.unwrap_err();
        assert_eq!(
            same,
            OpsError::App(AppError::ProposalConflict(
                "confirm requires a distinct principal"
            ))
        );
        let applied = fx.uc(finance()).confirm(fx.cmd("k1")).await.unwrap();
        assert_eq!(applied.stage, UnwindStage::Applied);
        let reversal = applied.reversal_txn.unwrap();

        assert!(fx.store.realizations().iter().any(|fact| {
            fact.user == realized_user
                && fact.market == fx.market
                && fact.realized_delta == MicroUsd(-5)
                && fact.ledger_txn == reversal
        }));
        assert!(!fx
            .store
            .realizations()
            .iter()
            .any(|fact| fact.user == zero_user));

        // t0-exact: both traders hold their full original bankroll again.
        assert_eq!(fx.cash(fx.alice), 100_000_000);
        assert_eq!(fx.cash(fx.bob), 100_000_000);
        // Fees reversed to users ⇒ the fees account carries none of this
        // market's take; escrow stays zero.
        assert_eq!(
            fx.store
                .balance_of(crate::model::OwnerRef::Fees, Currency::Usdc),
            Some(MicroUsd(0))
        );
        // Positions zeroed.
        let mut check = fx.store.unwind_tx().await.unwrap();
        for position in check.positions_for_market(fx.market).await.unwrap() {
            assert_eq!(position.shares, MicroShares(0));
            assert_eq!(position.cost, MicroUsd(0));
            assert_eq!(position.realized_pnl, MicroUsd(0));
        }
        // Lineage: replaying confirm returns the applied authority without a
        // second reversal.
        drop(check);
        let replay = fx.uc(finance()).confirm(fx.cmd("k1")).await.unwrap();
        assert_eq!(replay.reversal_txn, Some(reversal));
        // Outbox carries the public frame.
        assert!(fx
            .store
            .outbox()
            .iter()
            .any(|event| event.event_type == "MarketUnwound"));
        // Both audits exist (propose + confirm).
        let audits = fx.store.audit_page(None, 10).await.unwrap();
        let actions: Vec<&str> = audits
            .iter()
            .map(|row| row.action.action.as_str())
            .collect();
        assert!(actions.contains(&"unwind_propose"));
        assert!(actions.contains(&"unwind_confirm"));
        // The full invariant suite stays green post-unwind.
        let report = crate::integrity::invariant_sweep::run(&fx.store)
            .await
            .unwrap();
        assert!(report.pass, "{:?}", report.identities);
    }

    #[tokio::test]
    async fn shortfall_books_house_leg_opens_receivable_and_deposit_auto_collects() {
        let fx = Fx::new().await;
        // Bob buys cheap, Alice pushes the price, Bob sells at a profit —
        // Bob's reversal debit exceeds his refund.
        fx.buy(fx.bob, 20_000_000, "bob-early").await;
        fx.buy(fx.alice, 60_000_000, "alice-pump").await;
        let bob_shares = {
            let mut tx = fx.store.unwind_tx().await.unwrap();
            let positions = tx.positions_for_market(fx.market).await.unwrap();
            positions
                .iter()
                .find(|p| p.user == fx.bob)
                .unwrap()
                .shares
                .0
        };
        fx.trade(fx.market, fx.bob, TradeAction::Sell, bob_shares, "bob-exit")
            .await;
        // Bob moves his bankroll into a second market so the clawback finds
        // thin cash.
        let market2 = Fx::seed_market(&fx.store, &fx.clock, "elsewhere").await;
        fx.store.record_vote(fx.bob, market2, Side::Yes);
        let bob_cash = fx.cash(fx.bob);
        fx.trade(
            market2,
            fx.bob,
            TradeAction::Buy,
            bob_cash - 1_000_000,
            "bob-elsewhere",
        )
        .await;
        fx.close().await;
        fx.void().await;

        fx.uc(superadmin()).propose(fx.cmd("k1")).await.unwrap();
        fx.clock.advance(Duration::seconds(61));
        let applied = fx.uc(finance()).confirm(fx.cmd("k1")).await.unwrap();
        let reversal = applied.reversal_txn.unwrap();

        // Bob's cash never went negative (identity 3 stays cash-exact).
        assert!(fx.cash(fx.bob) >= 0);
        // A receivable opened for Bob, traced to the reversal transaction.
        let eligibility = fx.store.withdrawal_eligibility(fx.bob).await.unwrap();
        assert!(!eligibility.eligible);
        assert!(eligibility.open_receivables_micro > 0);
        let owed = eligibility.open_receivables_micro;
        // Alice is whole and unblocked.
        assert!(
            fx.store
                .withdrawal_eligibility(fx.alice)
                .await
                .unwrap()
                .eligible
        );
        // Identity 7 reconciles: opened == the reversal's house shortfall.
        {
            let mut invariants = fx.store.invariant_read_tx().await.unwrap();
            let recon = invariants.receivable_reconciliation().await.unwrap();
            let row = recon
                .iter()
                .find(|row| row.origin_reversal_txn == reversal)
                .unwrap();
            assert_eq!(row.opened_micro, owed);
            assert_eq!(row.house_shortfall_micro, owed);
            assert_eq!(row.collected_micro, 0);
        }
        let report = crate::integrity::invariant_sweep::run(&fx.store)
            .await
            .unwrap();
        assert!(report.pass, "{:?}", report.identities);

        // Faucet-style deposit: $200 against the receivable auto-collects in
        // the SAME transaction and clears eligibility.
        let house_before = fx
            .store
            .balance_of(crate::model::OwnerRef::House, Currency::Usdc)
            .map_or(0, |b| b.0);
        let receipt = CreditDeposit { store: &fx.store }
            .execute_as(
                CreditDepositCmd {
                    user: fx.bob,
                    amount: MicroUsd(200_000_000),
                    chain_sig: "faucet-sig-1".to_string(),
                    idempotency_key: "faucet-1".to_string(),
                },
                &finance(),
            )
            .await
            .unwrap();
        assert_eq!(receipt.collected_micro, owed);
        let after = fx.store.withdrawal_eligibility(fx.bob).await.unwrap();
        assert!(
            after.eligible,
            "auto-collection cleared the lien: {after:?}"
        );
        assert_eq!(after.open_receivables_micro, 0);
        let house_after = fx
            .store
            .balance_of(crate::model::OwnerRef::House, Currency::Usdc)
            .map_or(0, |b| b.0);
        assert_eq!(
            house_after - house_before,
            owed,
            "house recovered the shortfall"
        );
        // Movements ledgered: opened + collected, derived outstanding zero.
        {
            let mut invariants = fx.store.invariant_read_tx().await.unwrap();
            let recon = invariants.receivable_reconciliation().await.unwrap();
            let row = recon
                .iter()
                .find(|row| row.origin_reversal_txn == reversal)
                .unwrap();
            assert_eq!(row.collected_micro, owed);
        }
        let report = crate::integrity::invariant_sweep::run(&fx.store)
            .await
            .unwrap();
        assert!(report.pass, "{:?}", report.identities);
    }

    #[tokio::test]
    async fn paid_markets_refuse_unwind_and_delay_expiry_gates_hold() {
        let fx = Fx::new().await;
        let carol = fx.store.add_user("carol", Fx::t0() - Duration::days(90), 1);
        fx.store.fund_user(carol, MicroUsd(50_000_000)).unwrap();
        fx.store.record_vote(carol, fx.market, Side::Yes);
        fx.buy(fx.alice, 20_000_000, "a").await;
        fx.close().await;
        // Three votes ⇒ resolves at tally and pays out (Paid).
        ResolveMarket {
            store: &fx.store,
            clock: &fx.clock,
            config: ResolveConfig {
                oi_floor: MicroUsd(0),
            },
            rep_config: RepConfig::default(),
            integrity_config: IntegritySweepConfig {
                payout_hold_threshold_micro: i64::MAX,
                ..IntegritySweepConfig::default()
            },
            crash_point: &NoopCrashPoint,
            actor: AdminContext::Machine,
        }
        .execute(ResolveMarketCmd {
            market: fx.market,
            curator_override: None,
        })
        .await
        .unwrap();
        assert_eq!(fx.store.market_state(fx.market), Some(MarketState::Paid));
        // NEVER from Paid (409 IllegalTransition at propose already).
        let refused = fx.uc(superadmin()).propose(fx.cmd("k1")).await.unwrap_err();
        assert_eq!(refused, OpsError::App(AppError::IllegalTransition));
    }

    #[tokio::test]
    async fn dual_control_delay_and_ttl_are_enforced() {
        let fx = Fx::new().await;
        fx.buy(fx.alice, 10_000_000, "a").await;
        fx.close().await;
        fx.void().await;
        let first = fx.uc(superadmin()).propose(fx.cmd("k1")).await.unwrap();
        let replay = fx.uc(superadmin()).propose(fx.cmd("k1")).await.unwrap();
        assert_eq!(first, replay);
        // Early confirm: the delay has not elapsed.
        let early = fx.uc(finance()).confirm(fx.cmd("k1")).await.unwrap_err();
        assert_eq!(
            early,
            OpsError::App(AppError::ProposalConflict(
                "dual-control delay has not elapsed"
            ))
        );
        // Reject unblocks a fresh proposal.
        let rejected = fx.uc(finance()).reject(fx.cmd("k1")).await.unwrap();
        assert_eq!(rejected.stage, UnwindStage::Rejected);
        assert!(matches!(
            fx.uc(finance()).confirm(fx.cmd("k1")).await,
            Err(OpsError::App(AppError::ProposalConflict(
                "unwind is not pending"
            )))
        ));
        assert!(matches!(
            fx.uc(finance()).reject(fx.cmd("k1")).await,
            Err(OpsError::App(AppError::ProposalConflict(
                "unwind is not pending"
            )))
        ));
        // A rejected authority occupies the market slot: re-propose conflicts
        // (the coordinator escalation path is a new market decision).
        let again = fx.uc(superadmin()).propose(fx.cmd("k2")).await.unwrap_err();
        assert_eq!(
            again,
            OpsError::App(AppError::ProposalConflict(
                "unwind already exists for this market"
            ))
        );
    }

    #[tokio::test]
    async fn machine_actors_hold_no_dual_control_authority() {
        let fx = Fx::new().await;
        let denied = fx
            .uc(AdminContext::Machine)
            .propose(fx.cmd("k"))
            .await
            .unwrap_err();
        assert_eq!(
            denied,
            OpsError::App(AppError::ProposalConflict("machine actor"))
        );
    }

    #[test]
    fn legality_and_shortfall_helpers_cover_the_full_vocabulary() {
        assert!(check_unwindable(MarketState::Voided, false).is_ok());
        assert!(check_unwindable(MarketState::Resolving, true).is_ok());
        assert_eq!(
            check_unwindable(MarketState::Paid, true),
            Err(OpsError::App(AppError::IllegalTransition))
        );
        let account = domain::ledger::AccountId(uuid::Uuid::new_v4());
        assert_eq!(
            house_shortfall_micro(&[
                LedgerEntryFacts {
                    account,
                    owner: OwnerRef::House,
                    amount_micro: -7,
                },
                LedgerEntryFacts {
                    account,
                    owner: OwnerRef::House,
                    amount_micro: 3,
                },
                LedgerEntryFacts {
                    account,
                    owner: OwnerRef::External,
                    amount_micro: -100,
                },
            ]),
            7
        );
        let position = crate::model::PositionRow {
            user: UserId(uuid::Uuid::new_v4()),
            outcome: crate::model::OutcomeId(uuid::Uuid::new_v4()),
            shares: MicroShares(0),
            cost: MicroUsd(0),
            realized_pnl: MicroUsd(5),
        };
        assert_eq!(
            unwind_realization(&position, MarketId(uuid::Uuid::new_v4()), None, Fx::t0()),
            None
        );
        assert_eq!(
            unwind_realization(
                &crate::model::PositionRow {
                    realized_pnl: MicroUsd(0),
                    ..position
                },
                MarketId(uuid::Uuid::new_v4()),
                Some(uuid::Uuid::new_v4()),
                Fx::t0()
            ),
            None
        );
        assert_eq!(
            unwind_realization(
                &position,
                MarketId(uuid::Uuid::new_v4()),
                Some(uuid::Uuid::new_v4()),
                Fx::t0()
            )
            .unwrap()
            .realized_delta,
            MicroUsd(-5)
        );
    }

    #[test]
    fn unwind_rejects_a_shortfall_total_larger_than_i64() {
        let first = UserId(uuid::Uuid::from_u128(1));
        let second = UserId(uuid::Uuid::from_u128(2));
        assert!(matches!(
            shortfall_total(&[(first, i64::MAX), (second, 1)]),
            Err(AppError::Overflow)
        ));
    }

    #[tokio::test]
    async fn expiry_and_receivable_cap_settle_without_a_reversal() {
        let fx = Fx::new().await;
        fx.close().await;
        fx.void().await;
        fx.uc(superadmin()).propose(fx.cmd("expire")).await.unwrap();
        fx.clock.advance(Duration::seconds(61 + 901));
        assert!(matches!(
            fx.uc(finance()).confirm(fx.cmd("expire")).await,
            Err(OpsError::App(AppError::ProposalConflict("unwind expired")))
        ));

        let store = InMemoryStore::new();
        let clock = FakeClock::at(Fx::t0());
        crate::ensure_genesis::EnsureGenesis { store: &store }
            .execute(crate::ensure_genesis::EnsureGenesisCmd {
                currency: Currency::Usdc,
                amount: MicroUsd(1_000_000),
            })
            .await
            .unwrap();
        let market = store
            .add_market(
                "empty-void",
                MarketState::Voided,
                Fx::t0(),
                Fx::t0(),
                MicroShares(1),
                BasisPoints(0),
            )
            .unwrap()
            .id;
        let command = UnwindCmd {
            market,
            reason: "wrong void".into(),
            idempotency_key: "empty".into(),
        };
        UnwindMarket {
            store: &store,
            clock: &clock,
            policy: OpsPolicy::default(),
            actor: superadmin(),
        }
        .propose(command.clone())
        .await
        .unwrap();
        clock.advance(Duration::seconds(61));
        let applied = UnwindMarket {
            store: &store,
            clock: &clock,
            policy: OpsPolicy::default(),
            actor: finance(),
        }
        .confirm(command)
        .await
        .unwrap();
        assert_eq!(applied.reversal_txn, None);

        let capped_fx = Fx::new().await;
        capped_fx.close().await;
        capped_fx.void().await;
        capped_fx
            .uc(superadmin())
            .propose(capped_fx.cmd("cap"))
            .await
            .unwrap();
        capped_fx.clock.advance(Duration::seconds(61));
        let error = UnwindMarket {
            store: &capped_fx.store,
            clock: &capped_fx.clock,
            policy: OpsPolicy {
                receivable_outstanding_cap_micro: 0,
                ..OpsPolicy::default()
            },
            actor: finance(),
        }
        .confirm(capped_fx.cmd("cap"))
        .await
        .unwrap_err();
        assert_eq!(error, OpsError::OverCap { cap_micro: 0 });
    }

    #[tokio::test]
    async fn resolving_with_a_paid_resolution_key_cannot_unwind() {
        use crate::ports::Store;
        use domain::ledger::{Entry, TxnKind};

        let store = InMemoryStore::new();
        let clock = FakeClock::at(Fx::t0());
        crate::ensure_genesis::EnsureGenesis { store: &store }
            .execute(crate::ensure_genesis::EnsureGenesisCmd {
                currency: Currency::Usdc,
                amount: MicroUsd(1_000_000),
            })
            .await
            .unwrap();
        let market = store
            .add_market(
                "flagged-resolving",
                MarketState::Resolving,
                Fx::t0(),
                Fx::t0(),
                MicroShares(1),
                BasisPoints(0),
            )
            .unwrap()
            .id;
        let mut tx = store.unwind_tx().await.unwrap();
        assert!(tx.flag_curator_needed(market).await.unwrap());
        let house = tx.account(OwnerRef::House, Currency::Usdc).await.unwrap();
        let external = tx
            .account(OwnerRef::External, Currency::Usdc)
            .await
            .unwrap();
        tx.ledger_apply(
            TxnKind::Payout,
            &crate::resolve_market::resolve_key(market),
            &[
                Entry {
                    account: house,
                    amount: MicroUsd(-1),
                },
                Entry {
                    account: external,
                    amount: MicroUsd(1),
                },
            ],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let command = UnwindCmd {
            market,
            reason: "wrong".into(),
            idempotency_key: "resolving".into(),
        };
        UnwindMarket {
            store: &store,
            clock: &clock,
            policy: OpsPolicy::default(),
            actor: superadmin(),
        }
        .propose(command.clone())
        .await
        .unwrap();
        clock.advance(Duration::seconds(61));
        assert_eq!(
            UnwindMarket {
                store: &store,
                clock: &clock,
                policy: OpsPolicy::default(),
                actor: finance(),
            }
            .confirm(command)
            .await
            .unwrap_err(),
            OpsError::App(AppError::IllegalTransition)
        );
    }

    #[tokio::test]
    async fn flagged_resolving_without_payout_transitions_to_voided() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(Fx::t0());
        crate::ensure_genesis::EnsureGenesis { store: &store }
            .execute(crate::ensure_genesis::EnsureGenesisCmd {
                currency: Currency::Usdc,
                amount: MicroUsd(1_000_000),
            })
            .await
            .unwrap();
        let market = store
            .add_market(
                "flagged-unpaid",
                MarketState::Resolving,
                Fx::t0(),
                Fx::t0(),
                MicroShares(1),
                BasisPoints(0),
            )
            .unwrap()
            .id;
        let mut flag = store.unwind_tx().await.unwrap();
        assert!(flag.flag_curator_needed(market).await.unwrap());
        flag.commit().await.unwrap();
        let command = UnwindCmd {
            market,
            reason: "wrong flag".into(),
            idempotency_key: "unpaid".into(),
        };
        UnwindMarket {
            store: &store,
            clock: &clock,
            policy: OpsPolicy::default(),
            actor: superadmin(),
        }
        .propose(command.clone())
        .await
        .unwrap();
        clock.advance(Duration::seconds(61));
        UnwindMarket {
            store: &store,
            clock: &clock,
            policy: OpsPolicy::default(),
            actor: finance(),
        }
        .confirm(command)
        .await
        .unwrap();
        assert_eq!(store.market_state(market), Some(MarketState::Voided));
    }

    #[tokio::test]
    async fn an_existing_market_reversal_blocks_a_second_reversal_lineage() {
        use domain::ledger::{Entry, TxnKind};

        let fx = Fx::new().await;
        fx.close().await;
        fx.void().await;
        fx.uc(superadmin())
            .propose(fx.cmd("already-reversed"))
            .await
            .unwrap();
        let mut tx = fx.store.unwind_tx().await.unwrap();
        let pool = tx
            .account(OwnerRef::MarketPool(fx.market), Currency::Usdc)
            .await
            .unwrap();
        let house = tx.account(OwnerRef::House, Currency::Usdc).await.unwrap();
        tx.ledger_apply(
            TxnKind::Reversal,
            "preexisting-reversal",
            &[
                Entry {
                    account: pool,
                    amount: MicroUsd(1),
                },
                Entry {
                    account: house,
                    amount: MicroUsd(-1),
                },
            ],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        fx.clock.advance(Duration::seconds(61));
        assert_eq!(
            fx.uc(finance())
                .confirm(fx.cmd("already-reversed"))
                .await
                .unwrap_err(),
            OpsError::App(AppError::ProposalConflict(
                "market already carries a reversal"
            ))
        );
    }
}
