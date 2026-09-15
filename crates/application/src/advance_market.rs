//! `AdvanceMarket` — admin-driven lifecycle event through
//! `domain::market::transition` (codex M3, whitelisted in P1R2 B4): accepts
//! ONLY non-financial events (`Approve`, `GoLive`, `EnterCloseWindow`,
//! `Close`, `StartIntegritySweep`). `Resolve`, `Pay`, and both `Void*`
//! events are rejected with [`AppError::UseResolveMarket`] because state
//! changes with money consequences must ride the conservation-checked
//! settlement transaction. Illegal transitions surface as 409 at the
//! adapter. This is what `POST /admin/markets/{id}/advance` calls; no
//! business logic in the HTTP adapter.

use domain::market::{MarketEvent, MarketState};
use serde_json::json;

use crate::error::AppError;
use crate::model::{AdminContext, Event, LifecycleCommand, MarketId};
use crate::ops::audit::audit_for;
use crate::ports::Store;

#[derive(Debug, Clone)]
pub struct AdvanceMarketCmd {
    pub market: MarketId,
    pub event: MarketEvent,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdvanceReceipt {
    pub market: MarketId,
    pub from: MarketState,
    pub to: MarketState,
    pub replayed: bool,
}

pub struct AdvanceMarket<'a, S: Store> {
    pub store: &'a S,
}

impl<S: Store> AdvanceMarket<'_, S> {
    /// # Errors
    /// [`AppError::UseResolveMarket`] for financial events,
    /// [`AppError::IllegalTransition`] for edges the domain rejects, and
    /// store failures.
    pub async fn execute(&self, cmd: AdvanceMarketCmd) -> Result<AdvanceReceipt, AppError> {
        self.execute_as(cmd, &AdminContext::Machine).await
    }

    /// D26 actor threading: audit facts are written ONLY for admin actors,
    /// in the SAME transaction as the transition; machine paths (scheduler,
    /// publisher, pre-phase-6 call sites) stay audit-free through
    /// [`Self::execute`].
    ///
    /// # Errors
    /// As [`Self::execute`].
    pub async fn execute_as(
        &self,
        cmd: AdvanceMarketCmd,
        actor: &AdminContext,
    ) -> Result<AdvanceReceipt, AppError> {
        // Non-financial whitelist (P1R2 B4) — checked before touching state.
        if !matches!(
            cmd.event,
            MarketEvent::Approve
                | MarketEvent::GoLive
                | MarketEvent::EnterCloseWindow
                | MarketEvent::Close
                | MarketEvent::StartIntegritySweep
        ) {
            return Err(AppError::UseResolveMarket);
        }
        let mut tx = self.store.advance_tx().await?;
        tx.serialize_key(&cmd.idempotency_key).await?;
        if let Some(recorded) = tx.lifecycle_command(&cmd.idempotency_key).await? {
            return Ok(AdvanceReceipt {
                market: recorded.market,
                from: source_state(recorded.event),
                to: recorded.resulting_state,
                replayed: true,
            });
        }
        let market = tx.market_for_update(cmd.market).await?;
        let to = domain::market::transition(market.state, cmd.event)
            .map_err(|_| AppError::IllegalTransition)?;
        tx.set_market_state(cmd.market, to).await?;
        tx.record_lifecycle_command(
            &cmd.idempotency_key,
            LifecycleCommand {
                market: cmd.market,
                event: cmd.event,
                resulting_state: to,
            },
        )
        .await?;
        tx.append(Event {
            event_type: "MarketAdvanced",
            aggregate_type: "market",
            aggregate_id: cmd.market.0,
            payload: json!({
                "event": format!("{:?}", cmd.event),
                "from": format!("{:?}", market.state),
                "to": format!("{to:?}"),
            }),
        })
        .await?;
        if let Some(row) = audit_for(
            actor,
            "advance_market",
            format!("market:{}", cmd.market.0),
            Some(serde_json::json!({ "state": format!("{:?}", market.state) })),
            Some(
                serde_json::json!({ "state": format!("{to:?}"), "event": format!("{:?}", cmd.event) }),
            ),
            None,
        ) {
            tx.audit_insert(row).await?;
        }
        tx.commit().await?;
        Ok(AdvanceReceipt {
            market: cmd.market,
            from: market.state,
            to,
            replayed: false,
        })
    }
}

const fn source_state(event: MarketEvent) -> MarketState {
    match event {
        MarketEvent::Approve => MarketState::Draft,
        MarketEvent::GoLive => MarketState::Scheduled,
        MarketEvent::EnterCloseWindow => MarketState::Live,
        MarketEvent::Close => MarketState::Closing,
        MarketEvent::StartIntegritySweep => MarketState::Closed,
        MarketEvent::Resolve
        | MarketEvent::Pay
        | MarketEvent::VoidLowParticipation
        | MarketEvent::VoidByAdmin => MarketState::Resolving,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::fakes::InMemoryStore;
    use crate::model::AdminRole;
    use crate::ports::OpsQueries;
    use domain::money::{BasisPoints, MicroShares};
    use time::{Duration, OffsetDateTime};

    fn t0() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    fn scheduled_market(store: &InMemoryStore) -> MarketId {
        store
            .add_market(
                "advanceable",
                MarketState::Scheduled,
                t0() + Duration::hours(2),
                t0() + Duration::hours(1),
                MicroShares(100_000_000),
                BasisPoints(100),
            )
            .unwrap()
            .id
    }

    #[tokio::test]
    async fn advances_scheduled_to_live_and_emits() {
        let store = InMemoryStore::new();
        let market = scheduled_market(&store);
        let uc = AdvanceMarket { store: &store };
        let receipt = uc
            .execute_as(
                AdvanceMarketCmd {
                    market,
                    event: MarketEvent::GoLive,
                    idempotency_key: "adv-1".to_string(),
                },
                &AdminContext::Admin {
                    token_digest: "ops-token".into(),
                    role: AdminRole::Ops,
                },
            )
            .await
            .unwrap();
        assert_eq!(receipt.from, MarketState::Scheduled);
        assert_eq!(receipt.to, MarketState::Live);
        assert_eq!(store.market_state(market), Some(MarketState::Live));
        let events = store.outbox();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "MarketAdvanced");
        assert_eq!(store.audit_page(None, 10).await.unwrap().len(), 1);
        let replay = uc
            .execute(AdvanceMarketCmd {
                market,
                event: MarketEvent::GoLive,
                idempotency_key: "adv-1".to_string(),
            })
            .await
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(store.outbox().len(), 1);
    }

    #[tokio::test]
    async fn financial_events_must_use_resolve_market() {
        let store = InMemoryStore::new();
        let market = scheduled_market(&store);
        let snapshot = store.snapshot();
        let uc = AdvanceMarket { store: &store };
        for event in [
            MarketEvent::Resolve,
            MarketEvent::Pay,
            MarketEvent::VoidLowParticipation,
            MarketEvent::VoidByAdmin,
        ] {
            let err = uc
                .execute(AdvanceMarketCmd {
                    market,
                    event,
                    idempotency_key: format!("adv-{event:?}"),
                })
                .await
                .unwrap_err();
            assert_eq!(err, AppError::UseResolveMarket, "event {event:?}");
        }
        assert_eq!(snapshot, store.snapshot());
    }

    #[tokio::test]
    async fn illegal_transition_is_rejected() {
        let store = InMemoryStore::new();
        let market = scheduled_market(&store);
        let uc = AdvanceMarket { store: &store };
        let err = uc
            .execute(AdvanceMarketCmd {
                market,
                event: MarketEvent::Close, // Scheduled → Close is not an edge
                idempotency_key: "adv-bad".to_string(),
            })
            .await
            .unwrap_err();
        assert_eq!(err, AppError::IllegalTransition);
        assert_eq!(store.market_state(market), Some(MarketState::Scheduled));
    }

    #[tokio::test]
    async fn full_nonfinancial_walk_reaches_closed() {
        let store = InMemoryStore::new();
        let market = scheduled_market(&store);
        let uc = AdvanceMarket { store: &store };
        for (i, event) in [
            MarketEvent::GoLive,
            MarketEvent::EnterCloseWindow,
            MarketEvent::Close,
            MarketEvent::StartIntegritySweep,
        ]
        .into_iter()
        .enumerate()
        {
            uc.execute(AdvanceMarketCmd {
                market,
                event,
                idempotency_key: format!("walk-{i}"),
            })
            .await
            .unwrap();
        }
        assert_eq!(store.market_state(market), Some(MarketState::Resolving));
    }

    #[tokio::test]
    async fn concurrent_same_command_has_one_transition_and_one_replay() {
        let store = InMemoryStore::new();
        let market = scheduled_market(&store);
        let uc = AdvanceMarket { store: &store };
        let command = AdvanceMarketCmd {
            market,
            event: MarketEvent::GoLive,
            idempotency_key: "advance-race".to_string(),
        };
        let (first, second) = tokio::join!(uc.execute(command.clone()), uc.execute(command));
        let (first, second) = (first.unwrap(), second.unwrap());
        assert!(first.replayed ^ second.replayed);
        assert_eq!(first.to, MarketState::Live);
        assert_eq!(second.to, MarketState::Live);
        assert_eq!(store.outbox().len(), 1);
    }

    #[tokio::test]
    async fn failed_command_does_not_consume_its_durable_key() {
        let store = InMemoryStore::new();
        let market = scheduled_market(&store);
        let uc = AdvanceMarket { store: &store };
        let close = AdvanceMarketCmd {
            market,
            event: MarketEvent::Close,
            idempotency_key: "retry-after-failure".to_string(),
        };
        assert_eq!(
            uc.execute(close.clone()).await.unwrap_err(),
            AppError::IllegalTransition
        );
        for (event, key) in [
            (MarketEvent::GoLive, "prepare-live"),
            (MarketEvent::EnterCloseWindow, "prepare-closing"),
        ] {
            uc.execute(AdvanceMarketCmd {
                market,
                event,
                idempotency_key: key.to_string(),
            })
            .await
            .unwrap();
        }
        let receipt = uc.execute(close).await.unwrap();
        assert!(!receipt.replayed);
        assert_eq!(receipt.to, MarketState::Closed);
    }

    #[test]
    fn replay_source_state_is_defined_for_every_persisted_event_name() {
        let cases = [
            (MarketEvent::Approve, MarketState::Draft),
            (MarketEvent::GoLive, MarketState::Scheduled),
            (MarketEvent::EnterCloseWindow, MarketState::Live),
            (MarketEvent::Close, MarketState::Closing),
            (MarketEvent::StartIntegritySweep, MarketState::Closed),
            (MarketEvent::Resolve, MarketState::Resolving),
            (MarketEvent::Pay, MarketState::Resolving),
            (MarketEvent::VoidLowParticipation, MarketState::Resolving),
            (MarketEvent::VoidByAdmin, MarketState::Resolving),
        ];
        for (event, expected) in cases {
            assert_eq!(source_state(event), expected, "event {event:?}");
        }
    }
}
