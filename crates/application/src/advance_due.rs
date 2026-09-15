//! Clock-driven lifecycle sweep. One bad market is isolated in the report;
//! failure of the initial due query is returned to the caller.

use domain::market::{MarketEvent, MarketState};
use serde_json::json;

use crate::advance_market::{AdvanceMarket, AdvanceMarketCmd};
use crate::error::{AppError, StoreError};
use crate::model::{
    Event, IntegrityReportRow, IntegritySweepConfig, LifecycleCommand, MarketId, RepConfig,
    ResolveConfig,
};
use crate::ports::{Clock, MarketQueries, ResolutionCrashPoint, Store};
use crate::resolve_market::{ResolveMarket, ResolveMarketCmd, ResolveOutcome};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepReport {
    pub advanced: u32,
    pub resolved: u32,
    pub curator_needed: u32,
    pub held: u32,
    pub swept_pass: u32,
    pub swept_flag: u32,
    pub errors: Vec<(MarketId, String)>,
}

#[derive(Debug, Default)]
struct CascadeResult {
    advanced: u32,
    resolved: u32,
    curator_needed: u32,
    held: u32,
    swept_pass: u32,
    swept_flag: u32,
}

fn classify_outcome(outcome: ResolveOutcome, advanced: u32) -> CascadeResult {
    let mut result = CascadeResult {
        advanced,
        ..CascadeResult::default()
    };
    match outcome {
        ResolveOutcome::Settled | ResolveOutcome::Voided => result.resolved = 1,
        ResolveOutcome::HeldForReview { .. } => result.held = 1,
        ResolveOutcome::CuratorRequired => result.curator_needed = 1,
    }
    result
}

pub struct AdvanceDue<'a, S: Store + MarketQueries, C: Clock> {
    pub store: &'a S,
    pub clock: &'a C,
    pub resolve_config: ResolveConfig,
    pub rep_config: RepConfig,
    pub integrity_config: IntegritySweepConfig,
    /// D29 slot: the scheduler is the production resolution path, so the
    /// armed staging crash point must flow through here. Noop unless armed.
    pub crash_point: &'a dyn ResolutionCrashPoint,
}

impl<S: Store + MarketQueries, C: Clock> AdvanceDue<'_, S, C> {
    /// Advances every bounded due batch. Each selected market cascades across
    /// all already-passed boundaries before the next batch, preventing a busy
    /// early page from starving a flash market.
    ///
    /// # Errors
    /// Returns a query-level store failure. Per-market failures are isolated
    /// in [`SweepReport::errors`].
    pub async fn sweep(&self) -> Result<SweepReport, StoreError> {
        let mut report = SweepReport::default();
        for _ in 0..8 {
            let due = self.store.due_markets(self.clock.now(), 256).await?;
            if due.is_empty() {
                break;
            }
            let mut progress = 0_u32;
            for row in due {
                match self.cascade(row.market).await {
                    Ok(result) => {
                        report.advanced = report.advanced.saturating_add(result.advanced);
                        report.resolved = report.resolved.saturating_add(result.resolved);
                        report.curator_needed =
                            report.curator_needed.saturating_add(result.curator_needed);
                        report.held = report.held.saturating_add(result.held);
                        report.swept_pass = report.swept_pass.saturating_add(result.swept_pass);
                        report.swept_flag = report.swept_flag.saturating_add(result.swept_flag);
                        progress = progress.saturating_add(
                            result.advanced
                                + result.resolved
                                + result.curator_needed
                                + result.held
                                + result.swept_pass
                                + result.swept_flag,
                        );
                    }
                    Err(error) => report.errors.push((row.market, error.to_string())),
                }
            }
            if progress == 0 {
                break;
            }
        }
        Ok(report)
    }

    async fn cascade(&self, market: MarketId) -> Result<CascadeResult, AppError> {
        let mut advanced = 0;
        loop {
            let row = self.store.market_by_ref(&market.0.to_string()).await?;
            let now = self.clock.now();
            let event = match row.state {
                MarketState::Scheduled if now >= row.opens_at => (MarketEvent::GoLive, "open"),
                MarketState::Live if now >= row.tally_hidden_at => {
                    (MarketEvent::EnterCloseWindow, "freeze")
                }
                MarketState::Closing if now >= row.closes_at => (MarketEvent::Close, "close"),
                MarketState::Closed => return self.resolve_closed(market, advanced).await,
                MarketState::Resolving
                    if row.integrity_due_at.is_some_and(|due_at| due_at <= now)
                        && row.curator_flagged_at.is_none() =>
                {
                    return self.sweep_resolving(market, advanced).await;
                }
                MarketState::Draft
                | MarketState::Scheduled
                | MarketState::Live
                | MarketState::Closing
                | MarketState::Resolved
                | MarketState::Paid
                | MarketState::Resolving
                | MarketState::Voided => {
                    return Ok(CascadeResult {
                        advanced,
                        ..CascadeResult::default()
                    });
                }
            };
            let (event, name) = event;
            AdvanceMarket { store: self.store }
                .execute(AdvanceMarketCmd {
                    market,
                    event,
                    idempotency_key: format!("sched:{}:{name}", market.0),
                })
                .await?;
            advanced += 1;
        }
    }

    fn resolver(&self) -> ResolveMarket<'_, S, C> {
        ResolveMarket {
            crash_point: self.crash_point,
            actor: crate::model::AdminContext::Machine,
            store: self.store,
            clock: self.clock,
            config: self.resolve_config,
            rep_config: self.rep_config,
            integrity_config: self.integrity_config,
        }
    }

    async fn resolve_closed(
        &self,
        market: MarketId,
        advanced: u32,
    ) -> Result<CascadeResult, AppError> {
        match self
            .resolver()
            .execute(ResolveMarketCmd {
                market,
                curator_override: None,
            })
            .await
        {
            Ok(receipt) => Ok(classify_outcome(receipt.outcome, advanced)),
            Err(AppError::NeedsCuratorDecision) => {
                let won = self.flag_curator(market).await?;
                Ok(CascadeResult {
                    advanced,
                    curator_needed: u32::from(won),
                    ..CascadeResult::default()
                })
            }
            Err(error) => Err(error),
        }
    }

    async fn sweep_resolving(
        &self,
        market: MarketId,
        advanced: u32,
    ) -> Result<CascadeResult, AppError> {
        if self.write_integrity_report(market).await? == domain::integrity::Verdict::Flag {
            let won = self.flag_curator(market).await?;
            return Ok(CascadeResult {
                advanced,
                curator_needed: u32::from(won),
                swept_flag: u32::from(won),
                ..CascadeResult::default()
            });
        }
        let receipt = self
            .resolver()
            .execute(ResolveMarketCmd {
                market,
                curator_override: None,
            })
            .await?;
        let mut result = classify_outcome(receipt.outcome, advanced);
        result.swept_pass = u32::from(result.resolved != 0);
        Ok(result)
    }

    async fn write_integrity_report(
        &self,
        market: MarketId,
    ) -> Result<domain::integrity::Verdict, AppError> {
        let key = format!("sweep-report:{}", market.0);
        let mut tx = self.store.integrity_tx().await?;
        tx.serialize_key(&key).await?;
        let row = tx.market_for_update(market).await?;
        if row.state != MarketState::Resolving {
            return Err(StoreError::Invariant("integrity report requires resolving market").into());
        }
        let stats = tx.vote_stats(market, self.integrity_config).await?;
        let checks = domain::integrity::sweep_checks(&stats, &self.integrity_config.thresholds());
        let verdict = domain::integrity::verdict(&checks);
        let checks_json = serde_json::Value::Array(
            checks
                .iter()
                .map(|check| {
                    json!({
                        "name": check.name,
                        "value": check.value_ppm,
                        "threshold": check.threshold_ppm,
                        "coverage": check.coverage_ppm,
                        "strength": match check.strength {
                            domain::integrity::Strength::Weak => "weak",
                            domain::integrity::Strength::Medium => "medium",
                        },
                        "note": check.note,
                        "flagged": check.flagged,
                        "config_version": 1,
                    })
                })
                .collect(),
        );
        let candidate = IntegrityReportRow {
            market,
            checks: checks_json,
            verdict,
            created_at: self.clock.now(),
        };
        let _inserted = tx.insert_integrity_report(&candidate).await?;
        let stored = tx
            .integrity_report(market)
            .await?
            .ok_or(StoreError::Invariant("integrity report insert disappeared"))?;
        tx.commit().await?;
        Ok(stored.verdict)
    }

    async fn flag_curator(&self, market: MarketId) -> Result<bool, AppError> {
        let key = format!("sched:{}:curator", market.0);
        let mut tx = self.store.resolve_tx().await?;
        tx.serialize_key(&key).await?;
        let row = tx.market_for_update(market).await?;
        if row.state == MarketState::Closed {
            let resolving =
                domain::market::transition(MarketState::Closed, MarketEvent::StartIntegritySweep)
                    .map_err(|_| AppError::IllegalTransition)?;
            crate::ports::SettlementIo::set_market_state(tx.as_mut(), market, resolving).await?;
            tx.record_lifecycle_command(
                &key,
                LifecycleCommand {
                    market,
                    event: MarketEvent::StartIntegritySweep,
                    resulting_state: resolving,
                },
            )
            .await?;
        } else if row.state != MarketState::Resolving {
            return Ok(false);
        }
        let won = tx.flag_curator_needed(market).await?;
        if won {
            tx.append(Event {
                event_type: "CuratorNeeded",
                aggregate_type: "market",
                aggregate_id: market.0,
                payload: json!({"market_id": market.0.to_string()}),
            })
            .await?;
        }
        tx.commit().await?;
        Ok(won)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::ensure_genesis::{EnsureGenesis, EnsureGenesisCmd};
    use crate::fakes::{FakeClock, InMemoryStore};
    use crate::model::{AdminContext, UserId};
    use crate::ports::NoopCrashPoint;
    use crate::resolve_market::{CuratorDecision, ResolveMarket};
    use crate::seed_market::{SeedMarket, SeedMarketCmd};
    use domain::ledger::Currency;
    use domain::money::{BasisPoints, MicroUsd};
    use time::{Duration, OffsetDateTime};

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    async fn overdue(store: &InMemoryStore) -> MarketId {
        EnsureGenesis { store }
            .execute(EnsureGenesisCmd {
                currency: Currency::Usdc,
                amount: MicroUsd(10_000_000),
            })
            .await
            .unwrap();
        let market = MarketId(uuid::Uuid::new_v4());
        let clock = FakeClock::at(now());
        SeedMarket {
            store,
            clock: &clock,
            rep_config: crate::model::RepConfig::default(),
            lp_kill_config: crate::model::LpKillConfig::default(),
        }
        .execute(SeedMarketCmd {
            market_id: market,
            slug: format!("due-{market:?}"),
            min_votes_to_resolve: 3,
            closes_at: now() - Duration::seconds(1),
            tally_hidden_at: now() - Duration::seconds(2),
            fee: BasisPoints(0),
            seed: MicroUsd(1_000_000),
            idempotency_key: format!("seed-{market:?}"),
            force: false,
        })
        .await
        .unwrap();
        market
    }

    #[tokio::test]
    async fn overdue_market_cascades_to_void_in_one_sweep() {
        let store = InMemoryStore::new();
        let market = overdue(&store).await;
        let clock = FakeClock::at(now());
        let report = AdvanceDue {
            crash_point: &NoopCrashPoint,
            store: &store,
            clock: &clock,
            resolve_config: ResolveConfig {
                oi_floor: MicroUsd(1),
            },
            rep_config: RepConfig::default(),
            integrity_config: IntegritySweepConfig::default(),
        }
        .sweep()
        .await
        .unwrap();
        assert_eq!(report.advanced, 3);
        assert_eq!(report.resolved, 1);
        assert_eq!(store.market_state(market), Some(MarketState::Voided));
    }

    #[tokio::test]
    async fn curator_needed_is_flagged_and_emitted_once() {
        let store = InMemoryStore::new();
        let market = overdue(&store).await;
        let clock = FakeClock::at(now());
        let use_case = AdvanceDue {
            crash_point: &NoopCrashPoint,
            store: &store,
            clock: &clock,
            resolve_config: ResolveConfig {
                oi_floor: MicroUsd(0),
            },
            rep_config: RepConfig::default(),
            integrity_config: IntegritySweepConfig::default(),
        };
        let first = use_case.sweep().await.unwrap();
        let second = use_case.sweep().await.unwrap();
        assert_eq!(first.curator_needed, 1);
        assert_eq!(second.curator_needed, 0);
        assert_eq!(
            store
                .outbox()
                .iter()
                .filter(|event| event.event_type == "CuratorNeeded")
                .count(),
            1
        );
        assert_eq!(store.market_state(market), Some(MarketState::Resolving));
        let receipt = ResolveMarket {
            crash_point: &NoopCrashPoint,
            actor: AdminContext::Machine,
            store: &store,
            clock: &clock,
            config: ResolveConfig {
                oi_floor: MicroUsd(0),
            },
            rep_config: RepConfig::default(),
            integrity_config: IntegritySweepConfig::default(),
        }
        .execute(ResolveMarketCmd {
            market,
            curator_override: Some(CuratorDecision::Void),
        })
        .await
        .unwrap();
        assert_eq!(
            receipt.outcome,
            crate::resolve_market::ResolveOutcome::Voided
        );
        assert!(store
            .market_by_ref(&market.0.to_string())
            .await
            .unwrap()
            .curator_flagged_at
            .is_none());
    }

    #[tokio::test]
    async fn curator_can_resolve_at_the_actual_tally_and_clear_the_flag() {
        let store = InMemoryStore::new();
        let market = overdue(&store).await;
        store.record_vote(UserId(uuid::Uuid::new_v4()), market, domain::amm::Side::Yes);
        let clock = FakeClock::at(now());
        let use_case = AdvanceDue {
            crash_point: &NoopCrashPoint,
            store: &store,
            clock: &clock,
            resolve_config: ResolveConfig {
                oi_floor: MicroUsd(0),
            },
            rep_config: RepConfig::default(),
            integrity_config: IntegritySweepConfig::default(),
        };
        assert_eq!(use_case.sweep().await.unwrap().curator_needed, 1);
        let receipt = ResolveMarket {
            crash_point: &NoopCrashPoint,
            actor: AdminContext::Machine,
            store: &store,
            clock: &clock,
            config: ResolveConfig {
                oi_floor: MicroUsd(0),
            },
            rep_config: RepConfig::default(),
            integrity_config: IntegritySweepConfig::default(),
        }
        .execute(ResolveMarketCmd {
            market,
            curator_override: Some(CuratorDecision::ResolveAtTally),
        })
        .await
        .unwrap();
        assert_eq!(
            receipt.outcome,
            crate::resolve_market::ResolveOutcome::Settled
        );
        assert_eq!(receipt.final_yes_bps, Some(10_000));
        assert!(store
            .market_by_ref(&market.0.to_string())
            .await
            .unwrap()
            .curator_flagged_at
            .is_none());
    }

    #[tokio::test]
    async fn one_poisoned_market_does_not_stall_a_valid_market() {
        let store = InMemoryStore::new();
        let valid = overdue(&store).await;
        let poisoned = store
            .add_market(
                "poisoned-closed",
                MarketState::Closed,
                now() - Duration::seconds(1),
                now() - Duration::seconds(2),
                domain::money::MicroShares(1_000_000),
                BasisPoints(0),
            )
            .unwrap()
            .id;
        let report = AdvanceDue {
            crash_point: &NoopCrashPoint,
            store: &store,
            clock: &FakeClock::at(now()),
            resolve_config: ResolveConfig {
                oi_floor: MicroUsd(1),
            },
            rep_config: RepConfig::default(),
            integrity_config: IntegritySweepConfig::default(),
        }
        .sweep()
        .await
        .unwrap();
        assert_eq!(store.market_state(valid), Some(MarketState::Voided));
        assert!(report.errors.iter().any(|(market, _)| *market == poisoned));
    }

    #[tokio::test]
    async fn due_query_failure_is_sweep_level() {
        let store = InMemoryStore::new();
        store.set_due_query_failure(true);
        let error = AdvanceDue {
            crash_point: &NoopCrashPoint,
            store: &store,
            clock: &FakeClock::at(now()),
            resolve_config: ResolveConfig {
                oi_floor: MicroUsd(0),
            },
            rep_config: RepConfig::default(),
            integrity_config: IntegritySweepConfig::default(),
        }
        .sweep()
        .await
        .unwrap_err();
        assert!(matches!(error, StoreError::Backend(_)));
    }

    #[tokio::test]
    async fn overdue_flash_market_cascades_with_three_hundred_due_peers() {
        let store = InMemoryStore::new();
        let flash = overdue(&store).await;
        for index in 0..300 {
            store
                .add_market(
                    &format!("backlog-{index}"),
                    MarketState::Live,
                    now() + Duration::hours(1),
                    now() - Duration::seconds(1),
                    domain::money::MicroShares(1_000_000),
                    BasisPoints(0),
                )
                .unwrap();
        }
        let report = AdvanceDue {
            crash_point: &NoopCrashPoint,
            store: &store,
            clock: &FakeClock::at(now()),
            resolve_config: ResolveConfig {
                oi_floor: MicroUsd(1),
            },
            rep_config: RepConfig::default(),
            integrity_config: IntegritySweepConfig::default(),
        }
        .sweep()
        .await
        .unwrap();
        assert_eq!(store.market_state(flash), Some(MarketState::Voided));
        assert_eq!(report.resolved, 1);
        assert!(report.errors.is_empty());
    }

    #[tokio::test]
    async fn cascade_stops_at_a_future_boundary_without_writing() {
        let store = InMemoryStore::new();
        let market = store
            .add_market(
                "future",
                MarketState::Scheduled,
                now() + Duration::hours(2),
                now() + Duration::hours(1),
                domain::money::MicroShares(1_000_000),
                BasisPoints(0),
            )
            .unwrap()
            .id;
        let use_case = AdvanceDue {
            crash_point: &NoopCrashPoint,
            store: &store,
            clock: &FakeClock::at(now()),
            resolve_config: ResolveConfig {
                oi_floor: MicroUsd(1),
            },
            rep_config: RepConfig::default(),
            integrity_config: IntegritySweepConfig::default(),
        };
        let result = use_case.cascade(market).await.unwrap();
        assert_eq!(result.advanced, 1);
        assert_eq!(result.resolved, 0);
        assert_eq!(result.curator_needed, 0);
        let curator = classify_outcome(ResolveOutcome::CuratorRequired, 2);
        assert_eq!(curator.advanced, 2);
        assert_eq!(curator.curator_needed, 1);
        assert_eq!(store.outbox().len(), 1);
    }

    #[tokio::test]
    async fn curator_flagging_loses_cleanly_if_market_is_terminal() {
        let store = InMemoryStore::new();
        let market = store
            .add_market(
                "already-paid",
                MarketState::Paid,
                now() - Duration::seconds(1),
                now() - Duration::seconds(2),
                domain::money::MicroShares(1_000_000),
                BasisPoints(0),
            )
            .unwrap()
            .id;
        let use_case = AdvanceDue {
            crash_point: &NoopCrashPoint,
            store: &store,
            clock: &FakeClock::at(now()),
            resolve_config: ResolveConfig {
                oi_floor: MicroUsd(1),
            },
            rep_config: RepConfig::default(),
            integrity_config: IntegritySweepConfig::default(),
        };
        assert!(!use_case.flag_curator(market).await.unwrap());
        assert_eq!(
            use_case.write_integrity_report(market).await.unwrap_err(),
            AppError::Store(StoreError::Invariant(
                "integrity report requires resolving market"
            ))
        );
        assert!(store.outbox().is_empty());
    }

    #[tokio::test]
    async fn repeated_curator_flagging_emits_only_for_the_atomic_winner() {
        let store = InMemoryStore::new();
        let market = store
            .add_market(
                "closed-for-curator",
                MarketState::Closed,
                now() - Duration::seconds(1),
                now() - Duration::seconds(2),
                domain::money::MicroShares(1_000_000),
                BasisPoints(0),
            )
            .unwrap()
            .id;
        let use_case = AdvanceDue {
            crash_point: &NoopCrashPoint,
            store: &store,
            clock: &FakeClock::at(now()),
            resolve_config: ResolveConfig {
                oi_floor: MicroUsd(1),
            },
            rep_config: RepConfig::default(),
            integrity_config: IntegritySweepConfig::default(),
        };
        assert!(use_case.flag_curator(market).await.unwrap());
        assert!(!use_case.flag_curator(market).await.unwrap());
        assert_eq!(
            store
                .outbox()
                .iter()
                .filter(|event| event.event_type == "CuratorNeeded")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn held_market_waits_for_due_then_racing_pass_sweeps_settle_once() {
        let store = InMemoryStore::new();
        let market = overdue(&store).await;
        let clock = FakeClock::at(now());
        let config = IntegritySweepConfig {
            payout_hold_threshold_micro: 1,
            sweep_delay_secs: 10,
            ..IntegritySweepConfig::default()
        };
        let use_case = AdvanceDue {
            crash_point: &NoopCrashPoint,
            store: &store,
            clock: &clock,
            resolve_config: ResolveConfig {
                oi_floor: MicroUsd(1),
            },
            rep_config: RepConfig::default(),
            integrity_config: config,
        };
        let held = use_case.sweep().await.unwrap();
        assert_eq!(held.held, 1);
        assert_eq!(store.market_state(market), Some(MarketState::Resolving));
        assert_eq!(use_case.sweep().await.unwrap().swept_pass, 0);

        clock.advance(Duration::seconds(10));
        let (left, right) = tokio::join!(use_case.sweep(), use_case.sweep());
        let left = left.unwrap();
        let right = right.unwrap();
        assert!(left.errors.is_empty() && right.errors.is_empty());
        assert!(left.swept_pass + right.swept_pass >= 1);
        assert_eq!(store.market_state(market), Some(MarketState::Voided));
        assert_eq!(
            store
                .outbox()
                .iter()
                .filter(|event| event.event_type == "MarketVoided")
                .count(),
            1
        );
        let mut tx = store.integrity_tx().await.unwrap();
        tx.serialize_key("inspect-report").await.unwrap();
        let report = tx.integrity_report(market).await.unwrap().unwrap();
        assert_eq!(report.verdict, domain::integrity::Verdict::Pass);
        let checks = report.checks.as_array().unwrap();
        assert_eq!(checks.len(), 4);
        for check in checks {
            for field in [
                "name",
                "value",
                "threshold",
                "coverage",
                "strength",
                "note",
                "flagged",
                "config_version",
            ] {
                assert!(check.get(field).is_some(), "missing {field}");
            }
        }
    }

    #[tokio::test]
    async fn two_independent_signals_flag_and_hold_money_for_curator() {
        let store = InMemoryStore::new();
        let market = overdue(&store).await;
        for index in 0..5 {
            store.record_integrity_vote(
                UserId(uuid::Uuid::new_v4()),
                market,
                if index == 0 {
                    domain::amm::Side::No
                } else {
                    domain::amm::Side::Yes
                },
                now() - Duration::seconds(i64::from(index) + 2),
                (index == 0).then(|| "203.0.113.9".parse().unwrap()),
                (index == 0).then(|| "device-a".to_string()),
            );
        }
        let clock = FakeClock::at(now());
        let use_case = AdvanceDue {
            crash_point: &NoopCrashPoint,
            store: &store,
            clock: &clock,
            resolve_config: ResolveConfig {
                oi_floor: MicroUsd(1),
            },
            rep_config: RepConfig::default(),
            integrity_config: IntegritySweepConfig {
                payout_hold_threshold_micro: 1,
                sweep_delay_secs: 1,
                ..IntegritySweepConfig::default()
            },
        };
        assert_eq!(use_case.sweep().await.unwrap().held, 1);
        clock.advance(Duration::seconds(1));
        let report = use_case.sweep().await.unwrap();
        assert_eq!(report.swept_flag, 1);
        assert_eq!(report.curator_needed, 1);
        assert_eq!(store.market_state(market), Some(MarketState::Resolving));
        let row = store.market_by_ref(&market.0.to_string()).await.unwrap();
        assert!(row.curator_flagged_at.is_some());
        let flagged = store.flagged_markets().await.unwrap();
        assert_eq!(flagged.len(), 1);
        assert_eq!(flagged[0].market.id, market);
        assert_eq!(
            flagged[0].report.as_ref().unwrap().verdict,
            domain::integrity::Verdict::Flag
        );
        assert_eq!(
            store.balance_of(crate::model::OwnerRef::MarketEscrow(market), Currency::Usdc),
            Some(MicroUsd(1_000_000))
        );
    }
}
