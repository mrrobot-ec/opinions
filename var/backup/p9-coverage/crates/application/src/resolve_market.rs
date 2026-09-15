//! `ResolveMarket` — settlement (+ D21 auto-void branch). Legal only from
//! `Closed`/`Resolving` via `domain::market::transition`. The tally decides
//! `actual_yes_bps`. D21: votes below `min_votes_to_resolve` AND open
//! interest under the configured floor → `VoidLowParticipation` and a
//! neutral settle; votes below minimum with OI at/above the floor →
//! [`AppError::NeedsCuratorDecision`] (no silent auto-void — the one-time
//! voting-window extension is deliberately Phase 5, see the plan's ADR).
//! The void path REUSES `settle_market(holdings, 5_000, escrow)` verbatim:
//! full holdings including pool inventory, identical conservation + dust
//! rules, dust → fees (grok-p1r1 M7). `HoldingsIncomplete` propagates —
//! never masked. The payout ledger key is `resolve:<market_id>`; replays
//! return without rewriting. Zero-valued legs are omitted (the domain and
//! the DB forbid zero entries); if every leg is zero no ledger transaction
//! is written at all. States walk `Closed→Resolving→Resolved→Paid` (or
//! `→Voided`). Emits `MarketResolved`/`MarketVoided`.

use domain::ledger::{Currency, Entry, TxnKind};
use domain::market::{transition, MarketEvent, MarketState};
use domain::money::MicroUsd;
use domain::resolution::{redemptions, settle_market, Settlement};
use serde_json::json;

use crate::error::{AppError, StoreError};
use crate::model::{
    AdminAction, AdminContext, Event, IntegritySweepConfig, LifecycleCommand, MarketId, OwnerRef,
    RealizationFact, RealizationSource, RepConfig, ReputationRow, ResolveConfig, UserId,
    VoteScoreUpdate,
};
use crate::ports::{Clock, NoopCrashPoint, ResolutionCrashPoint, Store};

#[derive(Debug, Clone, Copy)]
pub struct ResolveMarketCmd {
    pub market: MarketId,
    pub curator_override: Option<CuratorDecision>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CuratorDecision {
    ResolveAtTally,
    Void,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveOutcome {
    Settled,
    HeldForReview { due_at: time::OffsetDateTime },
    CuratorRequired,
    Voided,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolveReceipt {
    pub market: MarketId,
    pub outcome: ResolveOutcome,
    pub final_yes_bps: Option<u16>,
    /// `None` when settlement produced no nonzero leg (degenerate market).
    pub ledger_txn: Option<uuid::Uuid>,
    pub replayed: bool,
}

pub struct ResolveMarket<'a, S: Store, C: Clock> {
    pub store: &'a S,
    pub clock: &'a C,
    /// Typed config (codex M4): the D21 open-interest floor.
    pub config: ResolveConfig,
    pub rep_config: RepConfig,
    pub integrity_config: IntegritySweepConfig,
    /// Injected crash point (D29): called EXACTLY ONCE, after the payout
    /// `ledger_apply` and before any subsequent write. Production wiring
    /// passes [`NoopCrashPoint`] unless the two-factor chaos arm selects the
    /// W4 staging adapter.
    pub crash_point: &'a dyn ResolutionCrashPoint,
    /// Caller identity (D26): audit facts are written ONLY for admin actors;
    /// machine paths (scheduler) stay audit-free and Phase 0–5 green.
    pub actor: AdminContext,
}

impl<'a, S: Store, C: Clock> ResolveMarket<'a, S, C> {
    /// Machine-path constructor: Noop crash point, no audit fact — the
    /// default for every pre-Phase-6 call site.
    pub fn machine(
        store: &'a S,
        clock: &'a C,
        config: ResolveConfig,
        rep_config: RepConfig,
        integrity_config: IntegritySweepConfig,
    ) -> Self {
        Self {
            store,
            clock,
            config,
            rep_config,
            integrity_config,
            crash_point: &NoopCrashPoint,
            actor: AdminContext::Machine,
        }
    }
}

/// The deterministic payout ledger key for one market's resolution.
#[must_use]
pub fn resolve_key(m: MarketId) -> String {
    format!("resolve:{}", m.0)
}

/// Materializes settlement legs, omitting every zero-valued one (the domain
/// and the DB forbid zero entries): each holder +payout · fees +dust ·
/// escrow −(Σ payouts + dust). An empty result means "write no ledger
/// transaction at all" (degenerate market).
fn settlement_entries(
    escrow_account: domain::ledger::AccountId,
    fees_account: domain::ledger::AccountId,
    settlement: &Settlement,
) -> Result<Vec<Entry>, AppError> {
    let mut entries = Vec::with_capacity(settlement.payouts.len() + 2);
    let mut total = 0_i64;
    for (account, payout) in &settlement.payouts {
        total = total.checked_add(payout.0).ok_or(AppError::Overflow)?;
        if payout.0 != 0 {
            entries.push(Entry {
                account: *account,
                amount: *payout,
            });
        }
    }
    if settlement.dust.0 != 0 {
        entries.push(Entry {
            account: fees_account,
            amount: settlement.dust,
        });
    }
    total = total
        .checked_add(settlement.dust.0)
        .ok_or(AppError::Overflow)?;
    if total != 0 {
        entries.push(Entry {
            account: escrow_account,
            amount: MicroUsd(-total),
        });
    }
    Ok(entries)
}

const RESOLUTION_PARTICIPANT_LOCK_ATTEMPTS: usize = 4;

struct ResolutionParticipants {
    voters: Vec<UserId>,
    all: Vec<UserId>,
}

async fn resolution_participants(
    tx: &mut (dyn crate::ports::ResolveTx + '_),
    market: MarketId,
) -> Result<ResolutionParticipants, StoreError> {
    let mut voters = tx.voter_ids(market).await?;
    voters.sort_unstable_by_key(|user| user.0);
    voters.dedup();

    let mut all = voters.clone();
    all.extend(tx.referral_relevant_users(market).await?);
    all.extend(
        tx.holdings(market)
            .await?
            .into_iter()
            .filter_map(|holding| match holding.owner {
                crate::model::HoldingOwner::User(user) => Some(user),
                crate::model::HoldingOwner::Pool => None,
            }),
    );
    all.sort_unstable_by_key(|user| user.0);
    all.dedup();
    Ok(ResolutionParticipants { voters, all })
}

impl<S: Store, C: Clock> ResolveMarket<'_, S, C> {
    /// # Errors
    /// [`AppError::IllegalTransition`] outside `Closed`/`Resolving`,
    /// [`AppError::NeedsCuratorDecision`] on the D21 curator branch,
    /// [`AppError::Resolution`] for domain settlement rejections
    /// (`HoldingsIncomplete` included — propagated, never masked), and store
    /// failures. On any error nothing became observable.
    #[allow(clippy::too_many_lines)]
    pub async fn execute(&self, cmd: ResolveMarketCmd) -> Result<ResolveReceipt, AppError> {
        let key = resolve_key(cmd.market);
        for _attempt in 0..RESOLUTION_PARTICIPANT_LOCK_ATTEMPTS {
            let mut tx = self.store.resolve_tx().await?;
            tx.serialize_key(&key).await?;
            if let Some(txn) = tx.txn_by_key(&key).await? {
                // Idempotent replay: return without rewriting.
                return replay_receipt(tx.as_mut(), cmd.market, txn).await;
            }
            // B5: enumerate the full participant union lock-free, acquire its one
            // globally sorted class-2 lock run, then take the market lock. Holders
            // join voters and referral parties because payout collection and
            // position settlement both operate under their user locks.
            let prelock = resolution_participants(tx.as_mut(), cmd.market).await?;
            let locked_reps = tx.reps_for_update(&prelock.all).await?;
            let market = tx.market_for_update(cmd.market).await?;
            // A trade/bind may commit between the first enumeration and the market
            // lock. Never add its user lock late (that would invert the global
            // order): abandon this transaction and reacquire every lock fresh.
            let postlock = resolution_participants(tx.as_mut(), cmd.market).await?;
            if postlock.all.iter().any(|user| {
                prelock
                    .all
                    .binary_search_by_key(&user.0, |held| held.0)
                    .is_err()
            }) {
                drop(tx);
                continue;
            }
            let locked_reps: Vec<_> = locked_reps
                .into_iter()
                .filter(|row| {
                    postlock
                        .voters
                        .binary_search_by_key(&row.user.0, |user| user.0)
                        .is_ok()
                })
                .collect();
            if market.state == MarketState::Resolving {
                if market.curator_flagged_at.is_some() {
                    if cmd.curator_override.is_none() {
                        return Err(AppError::CuratorRequired);
                    }
                } else {
                    if cmd.curator_override.is_some() {
                        return Err(AppError::CuratorOverrideNotAllowed);
                    }
                    match tx.integrity_report(cmd.market).await? {
                        Some(report) if report.verdict == domain::integrity::Verdict::Pass => {}
                        Some(_) | None => return Err(AppError::UnderReview),
                    }
                }
            } else if cmd.curator_override.is_some() {
                return Err(AppError::CuratorOverrideNotAllowed);
            }
            // Legal only from Closed (walking into Resolving) or Resolving.
            let resolving = match market.state {
                MarketState::Resolving => MarketState::Resolving,
                other => {
                    let next = transition(other, MarketEvent::StartIntegritySweep)
                        .map_err(|_| AppError::IllegalTransition)?;
                    tx.set_market_state(cmd.market, next).await?;
                    next
                }
            };

            let escrow_balance = tx.escrow_balance(cmd.market).await?;
            if market.state == MarketState::Closed
                && escrow_balance.0 >= self.integrity_config.payout_hold_threshold_micro
            {
                let delay = i64::try_from(self.integrity_config.sweep_delay_secs)
                    .map_err(|_| AppError::Overflow)?;
                let due_at = self.clock.now() + time::Duration::seconds(delay);
                tx.set_integrity_due_at(cmd.market, Some(due_at)).await?;
                let sweep_key = format!("sweep:{}", cmd.market.0);
                tx.record_lifecycle_command(
                    &sweep_key,
                    LifecycleCommand {
                        market: cmd.market,
                        event: MarketEvent::StartIntegritySweep,
                        resulting_state: resolving,
                    },
                )
                .await?;
                tx.append(Event {
                    event_type: "MarketAdvanced",
                    aggregate_type: "market",
                    aggregate_id: cmd.market.0,
                    payload: json!({
                        "event": "StartIntegritySweep",
                        "from": "Closed",
                        "to": "Resolving",
                        "integrity_due_at": due_at,
                    }),
                })
                .await?;
                self.audit(
                    tx.as_mut(),
                    cmd.market,
                    &market.state,
                    json!({ "outcome": "held_for_review", "integrity_due_at": due_at }),
                )
                .await?;
                tx.commit().await?;
                return Ok(ResolveReceipt {
                    market: cmd.market,
                    outcome: ResolveOutcome::HeldForReview { due_at },
                    final_yes_bps: None,
                    ledger_txn: None,
                    replayed: false,
                });
            }

            let tally = tx.tally(cmd.market).await?;
            let open_interest = tx.open_interest(cmd.market).await?;
            let (voided, final_bps) = match cmd.curator_override {
                Some(CuratorDecision::ResolveAtTally) => (
                    false,
                    tally
                        .actual_yes_bps()
                        .ok_or(AppError::NeedsCuratorDecision)?,
                ),
                Some(CuratorDecision::Void) => (true, 5_000),
                None => decide(
                    &tally,
                    market.min_votes_to_resolve,
                    open_interest,
                    self.config.oi_floor,
                )?,
            };

            // Settle from locked reads (codex B4): full holdings incl. pool
            // inventory against the escrow account's actual balance. The void
            // path reuses settle_market(holdings, 5_000, escrow) verbatim.
            let holdings = tx.holdings(cmd.market).await?;
            let settlement_input: Vec<_> = holdings
                .iter()
                .map(|holding| (holding.account, holding.side, holding.shares))
                .collect();
            let settlement = settle_market(&settlement_input, final_bps, escrow_balance)?;
            let escrow_acct = tx
                .account(OwnerRef::MarketEscrow(cmd.market), Currency::Usdc)
                .await?;
            let fees_acct = tx.account(OwnerRef::Fees, Currency::Usdc).await?;
            let entries = settlement_entries(escrow_acct, fees_acct, &settlement)?;
            let ledger_txn = Some(tx.ledger_apply(TxnKind::Payout, &key, &entries).await?);
            // D29: the injected crash point's ONLY call site — after the payout
            // ledger write, before every subsequent write. An armed staging
            // adapter dies here; the restart proves exactly-once payout.
            self.crash_point.fire().await?;
            // A payout is another cash ingress point. Collect every recipient's
            // open receivable before any later settlement writes, while the
            // resolution transaction still holds the globally ordered user locks.
            let mut payout_users: Vec<_> = holdings
                .iter()
                .zip(&settlement.payouts)
                .filter_map(|(holding, (_, payout))| match holding.owner {
                    crate::model::HoldingOwner::User(user) if payout.0 > 0 => Some(user),
                    crate::model::HoldingOwner::Pool | crate::model::HoldingOwner::User(_) => None,
                })
                .collect();
            payout_users.sort_unstable_by_key(|user| user.0);
            payout_users.dedup();
            for user in payout_users {
                crate::ops::receivable_collection::auto_collect(
                    tx.as_mut(),
                    user,
                    &format!("payout:{}:{}", cmd.market.0, user.0),
                    "machine:resolution",
                )
                .await?;
            }
            // D27 identity 5: the settled escrow fact rides the SAME transaction.
            tx.set_collateral_at_close(cmd.market, escrow_balance)
                .await?;
            let settlement_txn =
                ledger_txn.ok_or(StoreError::Invariant("missing payout transaction"))?;
            let settled_at = self.clock.now();
            let mut pool_payout = MicroUsd(0);

            for (holding, (_, payout)) in holdings.iter().zip(&settlement.payouts) {
                let user = match holding.owner {
                    crate::model::HoldingOwner::Pool => {
                        pool_payout = pool_payout.checked_add(*payout).ok_or(AppError::Overflow)?;
                        continue;
                    }
                    crate::model::HoldingOwner::User(user) => user,
                };
                let mut position = tx
                    .position_for_update(user, holding.outcome)
                    .await?
                    .ok_or(StoreError::Invariant("settled holding without position"))?;
                let delta = payout
                    .0
                    .checked_sub(position.cost.0)
                    .ok_or(AppError::Overflow)?;
                position.realized_pnl = MicroUsd(
                    position
                        .realized_pnl
                        .0
                        .checked_add(delta)
                        .ok_or(AppError::Overflow)?,
                );
                position.shares = domain::money::MicroShares(0);
                position.cost = MicroUsd(0);
                tx.save_position(position).await?;
                tx.insert_realization(&RealizationFact {
                    user,
                    market: cmd.market,
                    outcome: holding.outcome,
                    source: if voided {
                        RealizationSource::Void
                    } else {
                        RealizationSource::Settlement
                    },
                    realized_delta: MicroUsd(delta),
                    payout: *payout,
                    ledger_txn: settlement_txn,
                    created_at: settled_at,
                })
                .await?;
            }

            let seeded = tx.pool_seeded_micro(cmd.market).await?;
            let lp_pnl = pool_payout.checked_sub(seeded).ok_or(AppError::Overflow)?;
            tx.set_lp_result(cmd.market, lp_pnl, settled_at).await?;

            let (yes_redemption, no_redemption) = redemptions(final_bps)?;
            tx.write_outcome_resolution(market.yes_outcome, final_bps, yes_redemption)
                .await?;
            tx.write_outcome_resolution(market.no_outcome, 10_000 - final_bps, no_redemption)
                .await?;

            // Scores only when the oracle produced a truth; a voided market has
            // none, and neutral scoring would mint free majority reputation.
            let mut rep_events = Vec::new();
            if !voided {
                let facts = tx.vote_facts(cmd.market).await?;
                let scores: Vec<VoteScoreUpdate> = facts
                    .iter()
                    .map(|fact| {
                        Ok(VoteScoreUpdate {
                            vote_id: fact.vote_id,
                            score: domain::scoring::score_vote(
                                fact.side,
                                fact.crowd_guess_pct,
                                final_bps,
                            )?,
                        })
                    })
                    .collect::<Result<_, AppError>>()?;
                tx.save_vote_scores(&scores).await?;
                if tally.total() >= i64::from(market.min_votes_to_resolve)
                    && escrow_balance.0 >= self.rep_config.rep_score_min_pot_micro
                {
                    let by_user: std::collections::HashMap<_, _> = facts
                        .iter()
                        .zip(&scores)
                        .map(|(fact, score)| (fact.user, score.score))
                        .collect();
                    let updated: Vec<ReputationRow> = locked_reps
                        .iter()
                        .map(|row| {
                            let score = by_user
                                .get(&row.user)
                                .ok_or(StoreError::Invariant("voter score missing"))?;
                            let rep_micro = domain::reputation::update_rep(
                                row.rep_micro,
                                i64::from(score.score_bp) * 100,
                                self.rep_config.half_life,
                            )
                            .map_err(|_| StoreError::Invariant("invalid reputation operand"))?;
                            let tier = domain::reputation::tier_for(
                                rep_micro,
                                &self.rep_config.tier_thresholds_micro,
                            );
                            if tier != row.tier {
                                rep_events.push(Event {
                                    event_type: "RepUpdated",
                                    aggregate_type: "user",
                                    aggregate_id: row.user.0,
                                    payload: json!({
                                        "user_id": row.user.0.to_string(),
                                        "rep_micro": rep_micro,
                                        "tier": tier,
                                        "previous_tier": row.tier,
                                        "market_id": cmd.market.0.to_string(),
                                    }),
                                });
                            }
                            Ok(ReputationRow {
                                user: row.user,
                                rep_micro,
                                tier,
                            })
                        })
                        .collect::<Result<_, StoreError>>()?;
                    tx.save_reps(&updated).await?;
                }
            }

            let outcome = if voided {
                let event = if matches!(cmd.curator_override, Some(CuratorDecision::Void)) {
                    MarketEvent::VoidByAdmin
                } else {
                    MarketEvent::VoidLowParticipation
                };
                let voided_state =
                    transition(resolving, event).map_err(|_| AppError::IllegalTransition)?;
                tx.set_market_state(cmd.market, voided_state).await?;
                ResolveOutcome::Voided
            } else {
                let resolved = transition(resolving, MarketEvent::Resolve)
                    .map_err(|_| AppError::IllegalTransition)?;
                tx.set_market_state(cmd.market, resolved).await?;
                let paid = transition(resolved, MarketEvent::Pay)
                    .map_err(|_| AppError::IllegalTransition)?;
                tx.set_market_state(cmd.market, paid).await?;
                // D32: fee allocations finalize ONLY at Paid — never in the void
                // branch (Voided books forfeit provisional bonus progress).
                tx.finalize_market_fee_allocations(cmd.market).await?;
                // D32 referral-on-Paid: both legs mint only when the referee's
                // first Paid market qualifies; the hook re-validates under the
                // pre-taken user locks. Never in the void branch.
                tx.grant_referrals_on_paid(cmd.market).await?;
                ResolveOutcome::Settled
            };
            if cmd.curator_override.is_some() {
                tx.clear_curator_flag(cmd.market).await?;
            }
            rep_events.push(Event {
                event_type: if voided {
                    "MarketVoided"
                } else {
                    "MarketResolved"
                },
                aggregate_type: "market",
                aggregate_id: cmd.market.0,
                payload: json!({
                    "final_vote_bps": final_bps,
                    "voided": voided,
                    "ledger_txn": ledger_txn.map(|t| t.to_string()),
                    "redemption_yes_micro": yes_redemption.0,
                    "redemption_no_micro": no_redemption.0,
                }),
            });
            tx.append_batch(&rep_events).await?;
            self.audit(
                tx.as_mut(),
                cmd.market,
                &market.state,
                json!({ "final_yes_bps": final_bps, "voided": voided }),
            )
            .await?;
            tx.commit().await?;
            return Ok(ResolveReceipt {
                market: cmd.market,
                outcome,
                final_yes_bps: Some(final_bps),
                ledger_txn,
                replayed: false,
            });
        }
        Err(StoreError::Invariant("resolution participant set did not stabilize").into())
    }

    /// Writes the D26 audit fact for admin actors — in the SAME transaction
    /// as the settlement effect. Machine actors write nothing.
    async fn audit(
        &self,
        tx: &mut (dyn crate::ports::ResolveTx + '_),
        market: MarketId,
        state_before: &MarketState,
        after: serde_json::Value,
    ) -> Result<(), AppError> {
        let AdminContext::Admin { token_digest, role } = &self.actor else {
            return Ok(());
        };
        tx.audit_insert(AdminAction {
            actor_role: *role,
            actor_token_digest: token_digest.clone(),
            action: "resolve_market".to_string(),
            subject: format!("market:{}", market.0),
            before: Some(json!({ "state": format!("{state_before:?}") })),
            after: Some(after),
            reason: None,
        })
        .await
        .map_err(AppError::from)
    }
}

/// The D21 decision: enough votes → resolve at the tally; too few votes →
/// void only under the OI floor, otherwise a curator must decide.
fn decide(
    tally: &crate::model::Tally,
    min_votes: i32,
    open_interest: MicroUsd,
    oi_floor: MicroUsd,
) -> Result<(bool, u16), AppError> {
    match tally.actual_yes_bps() {
        Some(bps) if tally.total() >= i64::from(min_votes) => Ok((false, bps)),
        _ => {
            if open_interest < oi_floor {
                Ok((true, 5_000)) // settle neutrally
            } else {
                Err(AppError::NeedsCuratorDecision)
            }
        }
    }
}

/// Reconstructs the receipt for a replayed resolution from the settled
/// market state (the payout key proves this use case wrote it).
async fn replay_receipt(
    tx: &mut (dyn crate::ports::ResolveTx + '_),
    market: MarketId,
    txn: uuid::Uuid,
) -> Result<ResolveReceipt, AppError> {
    let row = tx.market_for_update(market).await?;
    let (outcome, final_bps) = match row.state {
        MarketState::Voided => (ResolveOutcome::Voided, 5_000),
        MarketState::Paid => {
            let tally = tx.tally(market).await?;
            let bps = tally
                .actual_yes_bps()
                .ok_or(StoreError::Invariant("paid market without a tally"))?;
            (ResolveOutcome::Settled, bps)
        }
        _ => {
            return Err(AppError::Store(StoreError::Invariant(
                "resolve key exists but market is not settled",
            )))
        }
    };
    Ok(ResolveReceipt {
        market,
        outcome,
        final_yes_bps: Some(final_bps),
        ledger_txn: Some(txn),
        replayed: true,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::advance_market::{AdvanceMarket, AdvanceMarketCmd};
    use crate::cast_vote::{CastVote, CastVoteCmd};
    use crate::credit_deposit::{CreditDeposit, CreditDepositCmd};
    use crate::ensure_genesis::{EnsureGenesis, EnsureGenesisCmd};
    use crate::fakes::{FakeClock, InMemoryStore};
    use crate::model::{IntegrityReportRow, TradeAction, UserId};
    use crate::place_trade::{PlaceTrade, PlaceTradeCmd};
    use crate::ports::MarketQueries;
    use crate::seed_market::{SeedMarket, SeedMarketCmd};
    use domain::amm::Side;
    use domain::money::{BasisPoints, MicroShares};
    use time::{Duration, OffsetDateTime};

    const SEED: i64 = 1_000_000_000; // $1,000 in micro
    const FLOOR: MicroUsd = MicroUsd(50_000_000); // $50 OI floor

    fn t0() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    /// Full-stack fixture through the real use cases: genesis → seed →
    /// go-live → deposits/votes/trades → close. Returns the market id.
    struct Fixture {
        store: InMemoryStore,
        clock: FakeClock,
        market: MarketId,
    }

    impl Fixture {
        async fn seeded(min_votes: i32) -> Self {
            let store = InMemoryStore::new();
            let clock = FakeClock::at(t0());
            EnsureGenesis { store: &store }
                .execute(EnsureGenesisCmd {
                    currency: Currency::Usdc,
                    amount: MicroUsd(10 * SEED),
                })
                .await
                .unwrap();
            let market = MarketId(uuid::Uuid::new_v4());
            SeedMarket {
                store: &store,
                clock: &clock,
                rep_config: RepConfig::default(),
                lp_kill_config: crate::model::LpKillConfig::default(),
            }
            .execute(SeedMarketCmd {
                market_id: market,
                slug: format!("resolvable-{}", market.0),
                min_votes_to_resolve: min_votes,
                closes_at: t0() + Duration::hours(2),
                tally_hidden_at: t0() + Duration::hours(1),
                fee: BasisPoints(0), // fee-free: keeps settlement math exact
                seed: MicroUsd(SEED),
                idempotency_key: format!("seed-{}", market.0),
                force: false,
            })
            .await
            .unwrap();
            AdvanceMarket { store: &store }
                .execute(AdvanceMarketCmd {
                    market,
                    event: domain::market::MarketEvent::GoLive,
                    idempotency_key: format!("golive-{}", market.0),
                })
                .await
                .unwrap();
            Self {
                store,
                clock,
                market,
            }
        }

        async fn voter(&self, side: Side, guess: u8) -> UserId {
            let user = UserId(uuid::Uuid::new_v4());
            self.store
                .link_user_channel("imessage", &user.0.to_string(), user);
            CastVote {
                store: &self.store,
                clock: &self.clock,
                config: crate::model::VoteIntegrityConfig::default(),
            }
            .execute(CastVoteCmd {
                market: self.market,
                user,
                side,
                crowd_guess_pct: guess,
                idempotency_key: format!("vote-{user:?}"),
                cast_ip: None,
                device_hash: None,
            })
            .await
            .unwrap();
            user
        }

        /// Deposits funds and buys `collateral` of `side` for `user`.
        async fn buy(&self, user: UserId, side: Side, collateral: i64) {
            CreditDeposit { store: &self.store }
                .execute(CreditDepositCmd {
                    user,
                    amount: MicroUsd(collateral),
                    chain_sig: format!("sig-{user:?}-{side:?}"),
                    idempotency_key: format!("dep-{user:?}-{side:?}"),
                })
                .await
                .unwrap();
            PlaceTrade {
                store: &self.store,
                clock: &self.clock,
                rep_config: RepConfig::default(),
            }
            .execute(PlaceTradeCmd {
                market: self.market,
                user,
                side,
                action: TradeAction::Buy,
                amount_micro: collateral,
                idempotency_key: format!("trade-{user:?}-{side:?}"),
                run_id: None,
                pending_action_id: None,
                expected_config_version: Some(1),
            })
            .await
            .unwrap();
        }

        async fn close(&self) {
            let advance = AdvanceMarket { store: &self.store };
            for (i, event) in [
                domain::market::MarketEvent::EnterCloseWindow,
                domain::market::MarketEvent::Close,
            ]
            .into_iter()
            .enumerate()
            {
                advance
                    .execute(AdvanceMarketCmd {
                        market: self.market,
                        event,
                        idempotency_key: format!("close-{}-{i}", self.market.0),
                    })
                    .await
                    .unwrap();
            }
        }

        fn resolve(&self) -> ResolveMarket<'_, InMemoryStore, FakeClock> {
            ResolveMarket::machine(
                &self.store,
                &self.clock,
                ResolveConfig { oi_floor: FLOOR },
                RepConfig::default(),
                IntegritySweepConfig::default(),
            )
        }

        /// Same policy as [`Self::resolve`] with an injected crash point and
        /// actor (the Phase 6 seams).
        fn resolve_as<'x>(
            &'x self,
            crash_point: &'x dyn crate::ports::ResolutionCrashPoint,
            actor: AdminContext,
        ) -> ResolveMarket<'x, InMemoryStore, FakeClock> {
            ResolveMarket {
                store: &self.store,
                clock: &self.clock,
                config: ResolveConfig { oi_floor: FLOOR },
                rep_config: RepConfig::default(),
                integrity_config: IntegritySweepConfig::default(),
                crash_point,
                actor,
            }
        }

        fn user_balance(&self, u: UserId) -> i64 {
            self.store
                .balance_of(OwnerRef::User(u), Currency::Usdc)
                .unwrap()
                .0
        }

        fn escrow_balance(&self) -> i64 {
            self.store
                .balance_of(OwnerRef::MarketEscrow(self.market), Currency::Usdc)
                .unwrap()
                .0
        }
    }

    /// Counts D29 crash-point firings; optionally fails to prove atomicity.
    struct CountingCrashPoint {
        fired: std::sync::atomic::AtomicUsize,
        fail: bool,
    }

    impl CountingCrashPoint {
        fn armed(fail: bool) -> Self {
            Self {
                fired: std::sync::atomic::AtomicUsize::new(0),
                fail,
            }
        }

        fn count(&self) -> usize {
            self.fired.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl crate::ports::ResolutionCrashPoint for CountingCrashPoint {
        async fn fire(&self) -> Result<(), StoreError> {
            self.fired.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail {
                return Err(StoreError::Backend("armed crash point fired".into()));
            }
            Ok(())
        }
    }

    fn admin_actor() -> AdminContext {
        AdminContext::Admin {
            token_digest: "test-digest".to_string(),
            role: crate::model::AdminRole::Curator,
        }
    }

    #[tokio::test]
    async fn settlement_stamps_collateral_at_close_and_fires_the_crash_point_once() {
        let fx = Fixture::seeded(1).await;
        let voter = fx.voter(Side::Yes, 80).await;
        fx.buy(voter, Side::Yes, 60_000_000).await;
        fx.close().await;
        let escrow_before = fx.escrow_balance();
        let crash_point = CountingCrashPoint::armed(false);
        let resolver = fx.resolve_as(&crash_point, AdminContext::Machine);
        let receipt = resolver
            .execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: None,
            })
            .await
            .unwrap();
        assert_eq!(receipt.outcome, ResolveOutcome::Settled);
        assert_eq!(crash_point.count(), 1, "exactly one crash-point call");
        assert_eq!(
            fx.store.collateral_at_close(fx.market),
            Some(MicroUsd(escrow_before)),
            "the settled escrow fact rides the resolution transaction"
        );
        // Replay returns before the payout write: no second firing, no
        // rewritten fact.
        let replay = resolver
            .execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: None,
            })
            .await
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(crash_point.count(), 1);
        assert_eq!(
            fx.store.collateral_at_close(fx.market),
            Some(MicroUsd(escrow_before))
        );
    }

    #[tokio::test]
    async fn payout_collects_the_recipients_open_receivable_in_the_same_transaction() {
        let fx = Fixture::seeded(1).await;
        let voter = fx.voter(Side::Yes, 80).await;
        fx.buy(voter, Side::Yes, 60_000_000).await;
        let receivable = crate::model::Receivable {
            id: uuid::Uuid::new_v4(),
            market: fx.market,
            user: voter,
            origin_reversal_txn: uuid::Uuid::new_v4(),
            opened_micro: 10_000_000,
        };
        let mut seed_receivable = fx.store.unwind_tx().await.unwrap();
        seed_receivable.insert_receivable(receivable).await.unwrap();
        seed_receivable
            .insert_receivable_movement(crate::model::ReceivableMovement {
                id: uuid::Uuid::new_v4(),
                receivable: receivable.id,
                kind: crate::model::ReceivableMovementKind::Opened,
                amount_micro: receivable.opened_micro,
                actor: "test".into(),
                cash_txn: None,
                idempotency_key: format!("open:{}", receivable.id),
            })
            .await
            .unwrap();
        seed_receivable.commit().await.unwrap();
        fx.close().await;

        let receipt = fx
            .resolve()
            .execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: None,
            })
            .await
            .unwrap();
        assert_eq!(receipt.outcome, ResolveOutcome::Settled);
        let eligibility =
            crate::ports::WithdrawalEligibility::withdrawal_eligibility(&fx.store, voter)
                .await
                .unwrap();
        assert_eq!(eligibility.open_receivables_micro, 0);
        assert!(eligibility.cash_micro > 0);
    }

    #[tokio::test]
    async fn referral_participants_are_enumerated_before_a_paid_resolution() {
        let fx = Fixture::seeded(1).await;
        let referrer = fx.store.add_user("referrer", t0(), 1);
        let referee = fx.voter(Side::Yes, 80).await;
        fx.store.set_phone_verified(referee, true);
        crate::money::referrals::BindReferral { store: &fx.store }
            .execute(crate::money::referrals::BindReferralCmd {
                referrer,
                referee,
                bind_key: "resolution-bind".into(),
                idempotency_key: "resolution-bind-command".into(),
            })
            .await
            .unwrap();
        fx.buy(referee, Side::Yes, 60_000_000).await;
        fx.close().await;

        let mut inspection = fx.store.resolve_tx().await.unwrap();
        let mut relevant = inspection.referral_relevant_users(fx.market).await.unwrap();
        relevant.sort_unstable_by_key(|user| user.0);
        let mut expected = vec![referrer, referee];
        expected.sort_unstable_by_key(|user| user.0);
        assert_eq!(relevant, expected);
        drop(inspection);

        let receipt = fx
            .resolve()
            .execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: None,
            })
            .await
            .unwrap();
        assert_eq!(receipt.outcome, ResolveOutcome::Settled);
        assert_eq!(fx.store.market_state(fx.market), Some(MarketState::Paid));
    }

    #[tokio::test]
    async fn resolution_acquires_one_globally_sorted_user_lock_run() {
        let fx = Fixture::seeded(1).await;
        let first = fx.store.add_user("lock-first", t0(), 1);
        let second = fx.store.add_user("lock-second", t0(), 1);
        let (referrer, voter) = if first.0 < second.0 {
            (first, second)
        } else {
            (second, first)
        };
        fx.store
            .link_user_channel("imessage", &voter.0.to_string(), voter);
        CastVote {
            store: &fx.store,
            clock: &fx.clock,
            config: crate::model::VoteIntegrityConfig::default(),
        }
        .execute(CastVoteCmd {
            market: fx.market,
            user: voter,
            side: Side::Yes,
            crowd_guess_pct: 80,
            idempotency_key: "global-lock-vote".into(),
            cast_ip: None,
            device_hash: None,
        })
        .await
        .unwrap();
        fx.store.set_phone_verified(voter, true);
        crate::money::referrals::BindReferral { store: &fx.store }
            .execute(crate::money::referrals::BindReferralCmd {
                referrer,
                referee: voter,
                bind_key: "global-lock-bind".into(),
                idempotency_key: "global-lock-bind-command".into(),
            })
            .await
            .unwrap();
        fx.buy(voter, Side::Yes, 60_000_000).await;
        fx.close().await;

        let mut lower_user_blocker = fx.store.credit_convert_tx().await.unwrap();
        lower_user_blocker.lock_user(referrer).await.unwrap();

        let resolver = fx.resolve();
        let resolution = resolver.execute(ResolveMarketCmd {
            market: fx.market,
            curator_override: None,
        });
        tokio::pin!(resolution);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), &mut resolution,)
                .await
                .is_err(),
            "resolution must wait for the lower referral-participant lock"
        );

        let mut higher_user_probe = fx.store.credit_convert_tx().await.unwrap();
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            higher_user_probe.lock_user(voter),
        )
        .await
        .unwrap_or_else(|_| {
            panic!("resolution must not lock a higher voter before the lower referral participant")
        })
        .unwrap();
        drop(higher_user_probe);
        drop(lower_user_blocker);

        let receipt = tokio::time::timeout(std::time::Duration::from_secs(1), resolution)
            .await
            .unwrap_or_else(|_| {
                panic!("resolution should finish after the canonical locks are released")
            })
            .unwrap();
        assert_eq!(receipt.outcome, ResolveOutcome::Settled);
    }

    #[tokio::test]
    async fn resolution_retries_when_a_referral_bind_commits_during_prelock_wait() {
        let fx = Fixture::seeded(1).await;
        let first = fx.voter(Side::Yes, 80).await;
        let second = fx.voter(Side::Yes, 80).await;
        fx.buy(first, Side::Yes, 60_000_000).await;
        fx.buy(second, Side::Yes, 60_000_000).await;
        fx.close().await;
        let (lower, referee) = if first.0 < second.0 {
            (first, second)
        } else {
            (second, first)
        };
        let referrer = fx.store.add_user("late-referral-referrer", t0(), 1);
        fx.store.set_phone_verified(referee, true);

        // The resolver has enumerated both voters but is blocked acquiring the
        // lower UUID. The bind can therefore commit while the higher referee
        // lock is still free, adding its referrer to the participant union.
        let mut lower_blocker = fx.store.credit_convert_tx().await.unwrap();
        lower_blocker.lock_user(lower).await.unwrap();

        let resolver = fx.resolve();
        let resolution = resolver.execute(ResolveMarketCmd {
            market: fx.market,
            curator_override: None,
        });
        tokio::pin!(resolution);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), &mut resolution)
                .await
                .is_err(),
            "resolution must wait at the held market lock"
        );
        let bind_action = crate::money::referrals::BindReferral { store: &fx.store };
        let bind = bind_action.execute(crate::money::referrals::BindReferralCmd {
            referrer,
            referee,
            bind_key: "late-resolution-bind".into(),
            idempotency_key: "late-resolution-bind-command".into(),
        });
        let bind_receipt = tokio::time::timeout(std::time::Duration::from_secs(1), bind)
            .await
            .unwrap_or_else(|_| panic!("bind should commit while resolver waits on lower voter"))
            .unwrap();
        assert!(!bind_receipt.replayed);

        let mut late_referrer_blocker = fx.store.credit_convert_tx().await.unwrap();
        late_referrer_blocker.lock_user(referrer).await.unwrap();
        drop(lower_blocker);

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), &mut resolution,)
                .await
                .is_err(),
            "participant drift must restart and wait for the late referrer lock"
        );
        drop(late_referrer_blocker);
        let receipt = tokio::time::timeout(std::time::Duration::from_secs(1), resolution)
            .await
            .unwrap_or_else(|_| panic!("resolution should finish after the late referrer unlocks"))
            .unwrap();
        assert_eq!(receipt.outcome, ResolveOutcome::Settled);
    }

    #[tokio::test]
    async fn stable_resolution_does_not_wait_for_an_unrelated_user_lock() {
        let fx = Fixture::seeded(1).await;
        let voter = fx.voter(Side::Yes, 80).await;
        fx.buy(voter, Side::Yes, 60_000_000).await;
        fx.close().await;
        let unrelated = fx.store.add_user("unrelated-resolution-user", t0(), 1);
        let mut blocker = fx.store.credit_convert_tx().await.unwrap();
        blocker.lock_user(unrelated).await.unwrap();

        let receipt = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            fx.resolve().execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: None,
            }),
        )
        .await
        .unwrap_or_else(|_| panic!("stable resolution must ignore unrelated user locks"))
        .unwrap();
        assert_eq!(receipt.outcome, ResolveOutcome::Settled);
        drop(blocker);
    }

    #[tokio::test]
    async fn a_voided_market_also_records_its_collateral_fact() {
        let fx = Fixture::seeded(3).await;
        let voter = fx.voter(Side::Yes, 80).await; // 1 vote < 3
        fx.buy(voter, Side::Yes, 10_000_000).await; // OI $10 < $50 floor
        fx.close().await;
        let escrow_before = fx.escrow_balance();
        let crash_point = CountingCrashPoint::armed(false);
        let receipt = fx
            .resolve_as(&crash_point, AdminContext::Machine)
            .execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: None,
            })
            .await
            .unwrap();
        assert_eq!(receipt.outcome, ResolveOutcome::Voided);
        assert_eq!(crash_point.count(), 1);
        assert_eq!(
            fx.store.collateral_at_close(fx.market),
            Some(MicroUsd(escrow_before))
        );
    }

    #[tokio::test]
    async fn a_crash_at_the_injected_point_leaves_nothing_observable() {
        let fx = Fixture::seeded(1).await;
        let voter = fx.voter(Side::Yes, 80).await;
        fx.buy(voter, Side::Yes, 60_000_000).await;
        fx.close().await;
        let snapshot = fx.store.snapshot();
        let crash_point = CountingCrashPoint::armed(true);
        let error = fx
            .resolve_as(&crash_point, AdminContext::Machine)
            .execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: None,
            })
            .await
            .unwrap_err();
        assert_eq!(
            error,
            AppError::Store(StoreError::Backend("armed crash point fired".into()))
        );
        assert_eq!(crash_point.count(), 1);
        assert_eq!(
            snapshot,
            fx.store.snapshot(),
            "the payout write and the crash share one transaction"
        );
        assert_eq!(fx.store.collateral_at_close(fx.market), None);
        // The market is still resolvable once the chaos scenario ends.
        let receipt = fx
            .resolve()
            .execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: None,
            })
            .await
            .unwrap();
        assert_eq!(receipt.outcome, ResolveOutcome::Settled);
    }

    #[tokio::test]
    async fn the_held_for_review_branch_never_reaches_the_crash_point() {
        let fx = Fixture::seeded(1).await;
        fx.voter(Side::Yes, 75).await;
        fx.close().await;
        let crash_point = CountingCrashPoint::armed(true);
        let resolver = ResolveMarket {
            store: &fx.store,
            clock: &fx.clock,
            config: ResolveConfig { oi_floor: FLOOR },
            rep_config: RepConfig::default(),
            integrity_config: IntegritySweepConfig {
                payout_hold_threshold_micro: SEED,
                ..IntegritySweepConfig::default()
            },
            crash_point: &crash_point,
            actor: AdminContext::Machine,
        };
        let held = resolver
            .execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: None,
            })
            .await
            .unwrap();
        assert!(matches!(held.outcome, ResolveOutcome::HeldForReview { .. }));
        assert_eq!(crash_point.count(), 0, "no payout write, no crash point");
        assert_eq!(fx.store.collateral_at_close(fx.market), None);
    }

    #[tokio::test]
    async fn an_admin_actor_writes_the_audit_fact_in_the_same_transaction() {
        // W2's real audit sink: an admin-actor settlement commits WITH its
        // audit row (machine actors settle audit-free in every other test).
        let fx = Fixture::seeded(1).await;
        let voter = fx.voter(Side::Yes, 80).await;
        fx.buy(voter, Side::Yes, 60_000_000).await;
        fx.close().await;
        let receipt = fx
            .resolve_as(&NoopCrashPoint, admin_actor())
            .execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: None,
            })
            .await
            .unwrap();
        assert!(receipt.ledger_txn.is_some());
        let page = crate::ports::OpsQueries::audit_page(&fx.store, None, 10)
            .await
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].action.action, "resolve_market");
        assert_eq!(page[0].action.subject, format!("market:{}", fx.market.0));
    }

    #[tokio::test]
    async fn an_admin_actor_audits_the_held_for_review_branch_too() {
        let fx = Fixture::seeded(1).await;
        fx.voter(Side::Yes, 75).await;
        fx.close().await;
        let snapshot = fx.store.snapshot();
        let resolver = ResolveMarket {
            store: &fx.store,
            clock: &fx.clock,
            config: ResolveConfig { oi_floor: FLOOR },
            rep_config: RepConfig::default(),
            integrity_config: IntegritySweepConfig {
                payout_hold_threshold_micro: SEED,
                ..IntegritySweepConfig::default()
            },
            crash_point: &NoopCrashPoint,
            actor: admin_actor(),
        };
        let receipt = resolver
            .execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: None,
            })
            .await
            .unwrap();
        assert!(matches!(
            receipt.outcome,
            ResolveOutcome::HeldForReview { .. }
        ));
        drop(snapshot);
        let page = crate::ports::OpsQueries::audit_page(&fx.store, None, 10)
            .await
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].action.action, "resolve_market");
    }

    #[tokio::test]
    async fn resolves_at_10000_bps_and_pays_yes_holders() {
        let fx = Fixture::seeded(3).await;
        let (a, b, c) = (
            fx.voter(Side::Yes, 80).await,
            fx.voter(Side::Yes, 70).await,
            fx.voter(Side::Yes, 90).await,
        );
        fx.buy(a, Side::Yes, 100_000_000).await;
        fx.close().await;

        let before_position = fx.store.positions(a).await.unwrap()[0];
        let pool_yes_before = fx.store.pool(fx.market).await.unwrap().pool.yes.0;
        let shares_a = before_position.shares.0;
        let receipt = fx
            .resolve()
            .execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: None,
            })
            .await
            .unwrap();
        assert_eq!(receipt.outcome, ResolveOutcome::Settled);
        assert_eq!(receipt.final_yes_bps, Some(10_000));
        assert!(receipt.ledger_txn.is_some());
        assert!(!receipt.replayed);

        // YES redeems at $1/whole-share: payout == micro-shares exactly (the
        // buyer spent their whole deposit, so their balance IS the payout).
        assert_eq!(fx.user_balance(a), shares_a);
        let settled = fx.store.positions(a).await.unwrap()[0];
        assert_eq!(settled.shares.0, 0);
        assert_eq!(settled.cost.0, 0);
        assert_eq!(settled.realized_pnl.0, shares_a - before_position.cost.0);
        let facts = fx.store.realizations();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].source, RealizationSource::Settlement);
        assert_eq!(facts[0].realized_delta.0, shares_a - before_position.cost.0);
        assert_eq!(facts[0].payout, MicroUsd(shares_a));
        assert_eq!(facts[0].ledger_txn, receipt.ledger_txn.unwrap());
        assert_eq!(
            fx.store.lp_result(fx.market),
            Some((MicroUsd(pool_yes_before - SEED), fx.clock.now()))
        );

        // Escrow fully drained: payouts + dust == escrow (dust → fees).
        assert_eq!(fx.escrow_balance(), 0);

        // States walked to Paid; outcome rows written for both sides.
        assert_eq!(fx.store.market_state(fx.market), Some(MarketState::Paid));
        let row = fx
            .store
            .market_by_ref(&fx.market.0.to_string())
            .await
            .unwrap();
        assert_eq!(
            fx.store.outcome_resolution(row.yes_outcome),
            Some((10_000, 1_000_000))
        );
        assert_eq!(fx.store.outcome_resolution(row.no_outcome), Some((0, 0)));

        // Scores were computed for every vote via domain::scoring.
        let events = fx.store.outbox();
        assert!(events.iter().any(|e| e.event_type == "MarketResolved"));
        let _ = (b, c);
    }

    #[tokio::test]
    async fn resolves_at_0_bps_and_pays_no_holders() {
        let fx = Fixture::seeded(2).await;
        let (yes_buyer, no_voter) = (fx.voter(Side::No, 20).await, fx.voter(Side::No, 30).await);
        fx.buy(yes_buyer, Side::Yes, 50_000_000).await; // loses everything
        fx.close().await;

        let before = fx.user_balance(yes_buyer);
        let before_position = fx.store.positions(yes_buyer).await.unwrap()[0];
        let receipt = fx
            .resolve()
            .execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: None,
            })
            .await
            .unwrap();
        assert_eq!(receipt.final_yes_bps, Some(0));
        assert_eq!(receipt.outcome, ResolveOutcome::Settled);
        // The YES buyer's zero-valued payout leg was omitted, not written.
        assert_eq!(fx.user_balance(yes_buyer), before);
        let settled = fx.store.positions(yes_buyer).await.unwrap()[0];
        assert_eq!(settled.shares.0, 0);
        assert_eq!(settled.cost.0, 0);
        assert_eq!(settled.realized_pnl.0, -before_position.cost.0);
        let facts = fx.store.realizations();
        assert_eq!(facts.len(), 1, "zero-payout losers still realize a fact");
        assert_eq!(facts[0].realized_delta, MicroUsd(-before_position.cost.0));
        assert_eq!(facts[0].payout, MicroUsd(0));
        assert_eq!(fx.escrow_balance(), 0);
        let _ = no_voter;
    }

    #[tokio::test]
    async fn resolves_at_5000_bps_with_split_tally() {
        let fx = Fixture::seeded(2).await;
        let (y, n) = (fx.voter(Side::Yes, 50).await, fx.voter(Side::No, 50).await);
        fx.buy(y, Side::Yes, 40_000_000).await;
        fx.close().await;

        let shares = fx.store.positions(y).await.unwrap()[0].shares.0;
        let receipt = fx
            .resolve()
            .execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: None,
            })
            .await
            .unwrap();
        assert_eq!(receipt.final_yes_bps, Some(5_000));
        // Half-redemption, floor per holding.
        let expected = i64::try_from(i128::from(shares) * 500_000 / 1_000_000).unwrap();
        assert_eq!(fx.user_balance(y), expected);
        assert_eq!(fx.escrow_balance(), 0);

        // Both voters were scored; at 5,000 both sides win majority.
        let _ = n;
        assert_eq!(fx.store.market_state(fx.market), Some(MarketState::Paid));
    }

    #[tokio::test]
    async fn reputation_updates_are_golden_batched_tier_events_and_replay_safe() {
        let fx = Fixture::seeded(2).await;
        let winner = fx.voter(Side::Yes, 75).await;
        let loser = fx.voter(Side::No, 25).await;
        fx.close().await;
        let config = RepConfig {
            tier_thresholds_micro: [1, 2, 3, 4],
            ..RepConfig::default()
        };
        let resolver = ResolveMarket {
            crash_point: &NoopCrashPoint,
            actor: AdminContext::Machine,
            store: &fx.store,
            clock: &fx.clock,
            config: ResolveConfig { oi_floor: FLOOR },
            rep_config: config,
            integrity_config: IntegritySweepConfig::default(),
        };
        let first = resolver
            .execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: None,
            })
            .await
            .unwrap();
        assert!(!first.replayed);
        for (user, side, guess) in [(winner, Side::Yes, 75), (loser, Side::No, 25)] {
            let score = domain::scoring::score_vote(side, guess, 5_000).unwrap();
            let expected = domain::reputation::update_rep(
                0,
                i64::from(score.score_bp) * 100,
                domain::reputation::HalfLife::H20,
            )
            .unwrap();
            let actual = fx.store.user_rep(user).await.unwrap();
            assert_eq!(actual.rep_micro, expected);
            assert_eq!(actual.tier, 4);
        }
        let tier_events = fx
            .store
            .outbox()
            .into_iter()
            .filter(|event| event.event_type == "RepUpdated")
            .count();
        assert_eq!(tier_events, 2);
        let replay = resolver
            .execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: None,
            })
            .await
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(
            fx.store
                .outbox()
                .into_iter()
                .filter(|event| event.event_type == "RepUpdated")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn reputation_quality_floor_is_inclusive_and_thin_markets_only_score() {
        for (floor, updates) in [(SEED, true), (SEED + 1, false)] {
            let fx = Fixture::seeded(1).await;
            let voter = fx.voter(Side::Yes, 80).await;
            fx.close().await;
            ResolveMarket {
                crash_point: &NoopCrashPoint,
                actor: AdminContext::Machine,
                store: &fx.store,
                clock: &fx.clock,
                config: ResolveConfig { oi_floor: FLOOR },
                rep_config: RepConfig {
                    rep_score_min_pot_micro: floor,
                    ..RepConfig::default()
                },
                integrity_config: IntegritySweepConfig::default(),
            }
            .execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: None,
            })
            .await
            .unwrap();
            assert_eq!(
                fx.store.user_rep(voter).await.unwrap().rep_micro > 0,
                updates
            );
        }

        let fx = Fixture::seeded(2).await;
        let voter = fx.voter(Side::Yes, 80).await;
        fx.close().await;
        let mut flag_tx = fx.store.resolve_tx().await.unwrap();
        flag_tx.serialize_key("thin-quality-flag").await.unwrap();
        flag_tx.market_for_update(fx.market).await.unwrap();
        crate::ports::SettlementIo::set_market_state(
            flag_tx.as_mut(),
            fx.market,
            MarketState::Resolving,
        )
        .await
        .unwrap();
        assert!(flag_tx.flag_curator_needed(fx.market).await.unwrap());
        flag_tx.commit().await.unwrap();
        ResolveMarket {
            crash_point: &NoopCrashPoint,
            actor: AdminContext::Machine,
            store: &fx.store,
            clock: &fx.clock,
            config: ResolveConfig { oi_floor: FLOOR },
            rep_config: RepConfig::default(),
            integrity_config: IntegritySweepConfig::default(),
        }
        .execute(ResolveMarketCmd {
            market: fx.market,
            curator_override: Some(CuratorDecision::ResolveAtTally),
        })
        .await
        .unwrap();
        assert!(fx.store.vote_score_for_user(voter, fx.market).is_some());
        assert_eq!(fx.store.user_rep(voter).await.unwrap().rep_micro, 0);
    }

    #[tokio::test]
    async fn corrupt_vote_fact_aborts_settlement_without_state_drift() {
        let fx = Fixture::seeded(1).await;
        let voter = fx.voter(Side::Yes, 80).await;
        fx.store.set_vote_guess(voter, fx.market, 101);
        fx.close().await;
        let before = fx.store.snapshot();
        let error = fx
            .resolve()
            .execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: None,
            })
            .await
            .unwrap_err();
        assert_eq!(
            error,
            AppError::Scoring(domain::scoring::ScoringError::GuessOutOfRange)
        );
        assert_eq!(before, fx.store.snapshot());
    }

    #[tokio::test]
    async fn low_votes_and_low_oi_void_neutrally() {
        let fx = Fixture::seeded(3).await;
        let v = fx.voter(Side::Yes, 80).await; // 1 vote < 3
        fx.buy(v, Side::Yes, 10_000_000).await; // OI $10 < $50 floor
        fx.close().await;

        let shares = fx.store.positions(v).await.unwrap()[0].shares.0;
        let receipt = fx
            .resolve()
            .execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: None,
            })
            .await
            .unwrap();
        assert_eq!(receipt.outcome, ResolveOutcome::Voided);
        assert_eq!(receipt.final_yes_bps, Some(5_000));
        assert_eq!(fx.store.market_state(fx.market), Some(MarketState::Voided));
        // Neutral settle: the voider's YES shares redeem at 50¢.
        let expected = i64::try_from(i128::from(shares) * 500_000 / 1_000_000).unwrap();
        assert_eq!(fx.user_balance(v), expected);
        assert_eq!(fx.escrow_balance(), 0);
        let events = fx.store.outbox();
        assert!(events.iter().any(|e| e.event_type == "MarketVoided"));
        let facts = fx.store.realizations();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].source, RealizationSource::Void);
        assert_eq!(facts[0].payout, MicroUsd(expected));
    }

    #[tokio::test]
    async fn low_votes_with_material_oi_needs_curator() {
        let fx = Fixture::seeded(3).await;
        let v = fx.voter(Side::Yes, 80).await;
        fx.buy(v, Side::Yes, 60_000_000).await; // OI $60 ≥ $50 floor
        fx.close().await;

        let snapshot = fx.store.snapshot();
        let err = fx
            .resolve()
            .execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: None,
            })
            .await
            .unwrap_err();
        assert_eq!(err, AppError::NeedsCuratorDecision);
        assert_eq!(snapshot, fx.store.snapshot());
        assert_eq!(fx.store.market_state(fx.market), Some(MarketState::Closed));
    }

    #[tokio::test]
    async fn replay_returns_without_rewriting() {
        let fx = Fixture::seeded(2).await;
        let v = fx.voter(Side::Yes, 80).await;
        let w = fx.voter(Side::Yes, 70).await;
        fx.buy(v, Side::Yes, 60_000_000).await;
        fx.close().await;

        let first = fx
            .resolve()
            .execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: None,
            })
            .await
            .unwrap();
        let snapshot = fx.store.snapshot();
        let second = fx
            .resolve()
            .execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: None,
            })
            .await
            .unwrap();
        assert!(second.replayed);
        assert_eq!(second.ledger_txn, first.ledger_txn);
        assert_eq!(second.outcome, first.outcome);
        assert_eq!(second.final_yes_bps, first.final_yes_bps);
        assert_eq!(snapshot, fx.store.snapshot());
        let _ = w;
    }

    #[tokio::test]
    async fn resolving_from_live_is_illegal() {
        let fx = Fixture::seeded(2).await;
        let err = fx
            .resolve()
            .execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: None,
            })
            .await
            .unwrap_err();
        assert_eq!(err, AppError::IllegalTransition);
    }

    #[tokio::test]
    async fn holdings_incomplete_propagates_unmasked() {
        // A fixture market with pool reserves but an empty escrow presents
        // shares that no escrow backs — the domain must reject settlement and
        // the use case must surface it verbatim.
        let store = InMemoryStore::new();
        let market = store
            .add_market(
                "corrupt",
                MarketState::Closed,
                t0() + Duration::hours(2),
                t0() + Duration::hours(1),
                MicroShares(1_000_000),
                BasisPoints(0),
            )
            .unwrap()
            .id;
        let uc = ResolveMarket {
            crash_point: &NoopCrashPoint,
            actor: AdminContext::Machine,
            store: &store,
            clock: &FakeClock::at(t0()),
            config: ResolveConfig { oi_floor: FLOOR },
            rep_config: RepConfig::default(),
            integrity_config: IntegritySweepConfig::default(),
        };
        // 0 votes < 3 min and OI 0 < floor → void branch → settle → must fail.
        let err = uc
            .execute(ResolveMarketCmd {
                market,
                curator_override: None,
            })
            .await
            .unwrap_err();
        let incomplete = matches!(
            err,
            AppError::Resolution(domain::resolution::ResolutionError::HoldingsIncomplete { .. })
        );
        assert!(incomplete, "resolution must reject incomplete holdings");
        assert_eq!(store.market_state(market), Some(MarketState::Closed));
    }

    #[test]
    fn zero_leg_materialization_can_produce_no_transaction() {
        // The degenerate all-zero settlement writes no ledger txn at all.
        let settlement = Settlement {
            payouts: vec![(domain::ledger::AccountId(uuid::Uuid::new_v4()), MicroUsd(0))],
            dust: MicroUsd(0),
        };
        let escrow = domain::ledger::AccountId(uuid::Uuid::new_v4());
        let fees = domain::ledger::AccountId(uuid::Uuid::new_v4());
        let entries = super::settlement_entries(escrow, fees, &settlement).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn settlement_entries_route_dust_and_balance_the_escrow() {
        let winner = domain::ledger::AccountId(uuid::Uuid::new_v4());
        let escrow = domain::ledger::AccountId(uuid::Uuid::new_v4());
        let fees = domain::ledger::AccountId(uuid::Uuid::new_v4());
        let entries = settlement_entries(
            escrow,
            fees,
            &Settlement {
                payouts: vec![(winner, MicroUsd(7))],
                dust: MicroUsd(2),
            },
        )
        .unwrap();
        assert_eq!(
            entries,
            vec![
                Entry {
                    account: winner,
                    amount: MicroUsd(7),
                },
                Entry {
                    account: fees,
                    amount: MicroUsd(2),
                },
                Entry {
                    account: escrow,
                    amount: MicroUsd(-9),
                },
            ]
        );
    }

    #[tokio::test]
    async fn curator_override_requires_the_scheduler_flag() {
        let fx = Fixture::seeded(2).await;
        fx.close().await;
        let error = fx
            .resolve()
            .execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: Some(CuratorDecision::Void),
            })
            .await
            .unwrap_err();
        assert_eq!(error, AppError::CuratorOverrideNotAllowed);
        assert_eq!(fx.store.market_state(fx.market), Some(MarketState::Closed));
    }

    #[tokio::test]
    async fn an_already_resolving_market_completes_the_financial_transition() {
        let fx = Fixture::seeded(1).await;
        let voter = fx.voter(Side::Yes, 75).await;
        fx.close().await;
        fx.store.set_market_state(fx.market, MarketState::Resolving);
        let mut report_tx = fx.store.integrity_tx().await.unwrap();
        report_tx.serialize_key("pass-report").await.unwrap();
        report_tx
            .insert_integrity_report(&IntegrityReportRow {
                market: fx.market,
                checks: json!([]),
                verdict: domain::integrity::Verdict::Pass,
                created_at: fx.clock.now(),
            })
            .await
            .unwrap();
        report_tx.commit().await.unwrap();
        let receipt = fx
            .resolve()
            .execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: None,
            })
            .await
            .unwrap();
        assert_eq!(receipt.outcome, ResolveOutcome::Settled);
        assert_eq!(fx.store.market_state(fx.market), Some(MarketState::Paid));
        let _ = voter;
    }

    #[tokio::test]
    async fn hold_threshold_is_inclusive_due_is_atomic_and_bypasses_are_rejected() {
        let fx = Fixture::seeded(1).await;
        fx.voter(Side::Yes, 75).await;
        fx.close().await;
        let config = IntegritySweepConfig {
            payout_hold_threshold_micro: SEED,
            sweep_delay_secs: 180,
            ..IntegritySweepConfig::default()
        };
        let resolver = ResolveMarket {
            crash_point: &NoopCrashPoint,
            actor: AdminContext::Machine,
            store: &fx.store,
            clock: &fx.clock,
            config: ResolveConfig { oi_floor: FLOOR },
            rep_config: RepConfig::default(),
            integrity_config: config,
        };
        let held = resolver
            .execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: None,
            })
            .await
            .unwrap();
        let due_at = fx.clock.now() + Duration::seconds(180);
        assert_eq!(held.outcome, ResolveOutcome::HeldForReview { due_at });
        let row = fx
            .store
            .market_by_ref(&fx.market.0.to_string())
            .await
            .unwrap();
        assert_eq!(row.state, MarketState::Resolving);
        assert_eq!(row.integrity_due_at, Some(due_at));
        assert_eq!(
            resolver
                .execute(ResolveMarketCmd {
                    market: fx.market,
                    curator_override: None,
                })
                .await
                .unwrap_err(),
            AppError::UnderReview
        );
        assert_eq!(
            resolver
                .execute(ResolveMarketCmd {
                    market: fx.market,
                    curator_override: Some(CuratorDecision::Void),
                })
                .await
                .unwrap_err(),
            AppError::CuratorOverrideNotAllowed
        );

        let mut flag_tx = fx.store.resolve_tx().await.unwrap();
        flag_tx.serialize_key("flag-held").await.unwrap();
        flag_tx.market_for_update(fx.market).await.unwrap();
        assert!(flag_tx.flag_curator_needed(fx.market).await.unwrap());
        flag_tx.commit().await.unwrap();
        assert_eq!(
            resolver
                .execute(ResolveMarketCmd {
                    market: fx.market,
                    curator_override: None,
                })
                .await
                .unwrap_err(),
            AppError::CuratorRequired
        );

        let below = Fixture::seeded(1).await;
        below.voter(Side::Yes, 75).await;
        below.close().await;
        let receipt = ResolveMarket {
            crash_point: &NoopCrashPoint,
            actor: AdminContext::Machine,
            store: &below.store,
            clock: &below.clock,
            config: ResolveConfig { oi_floor: FLOOR },
            rep_config: RepConfig::default(),
            integrity_config: IntegritySweepConfig {
                payout_hold_threshold_micro: SEED + 1,
                ..IntegritySweepConfig::default()
            },
        }
        .execute(ResolveMarketCmd {
            market: below.market,
            curator_override: None,
        })
        .await
        .unwrap();
        assert_eq!(receipt.outcome, ResolveOutcome::Settled);
    }

    #[tokio::test]
    async fn void_replay_reconstructs_the_neutral_receipt() {
        let fx = Fixture::seeded(3).await;
        fx.close().await;
        let first = fx
            .resolve()
            .execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: None,
            })
            .await
            .unwrap();
        let replay = fx
            .resolve()
            .execute(ResolveMarketCmd {
                market: fx.market,
                curator_override: None,
            })
            .await
            .unwrap();
        assert_eq!(first.outcome, ResolveOutcome::Voided);
        assert_eq!(replay.outcome, ResolveOutcome::Voided);
        assert_eq!(replay.final_yes_bps, Some(5_000));
        assert!(replay.replayed);
    }

    #[tokio::test]
    async fn a_resolve_key_on_an_unsettled_market_is_detected_as_corruption() {
        let store = InMemoryStore::new();
        let market = store
            .add_market(
                "corrupt-resolve-key",
                MarketState::Closed,
                t0() + Duration::hours(2),
                t0() + Duration::hours(1),
                MicroShares(1_000_000),
                BasisPoints(0),
            )
            .unwrap()
            .id;
        let key = resolve_key(market);
        let mut tx = store.resolve_tx().await.unwrap();
        tx.serialize_key(&key).await.unwrap();
        let external = tx
            .account(OwnerRef::External, Currency::Usdc)
            .await
            .unwrap();
        let fees = tx.account(OwnerRef::Fees, Currency::Usdc).await.unwrap();
        tx.ledger_apply(
            TxnKind::Payout,
            &key,
            &[
                Entry {
                    account: external,
                    amount: MicroUsd(-1),
                },
                Entry {
                    account: fees,
                    amount: MicroUsd(1),
                },
            ],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        let error = ResolveMarket {
            crash_point: &NoopCrashPoint,
            actor: AdminContext::Machine,
            store: &store,
            clock: &FakeClock::at(t0()),
            config: ResolveConfig { oi_floor: FLOOR },
            rep_config: RepConfig::default(),
            integrity_config: IntegritySweepConfig::default(),
        }
        .execute(ResolveMarketCmd {
            market,
            curator_override: None,
        })
        .await
        .unwrap_err();
        assert_eq!(
            error,
            AppError::Store(StoreError::Invariant(
                "resolve key exists but market is not settled"
            ))
        );
    }
}
