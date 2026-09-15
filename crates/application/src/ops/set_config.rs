//! `SetConfig` — the D24 direct write path for NON-sensitive keys, and the
//! shared apply core the proposal confirmation reuses. Sequence (D24 write
//! path, one transaction): exclusive pause fences in canonical order → the
//! generation row (writer B blocks behind writer A HERE) → whole-snapshot
//! typed validation → per-key change rows → audit → outbox wake.

use serde_json::{json, Value};
use time::OffsetDateTime;

use crate::error::{AppError, StoreError};
use crate::model::{AdminContext, ConfigChange, Event, MarketId};
use crate::ops::config::{
    exclusive_fences_for_patch, is_sensitive, pause_in_force, role_may_write, rule_for,
    snapshot_of, validate_patch,
};
use crate::ports::{Clock, OpsConfigTx, Store};

#[derive(Debug, Clone)]
pub struct SetConfigCmd {
    pub actor: AdminContext,
    /// JSON object: key → new value (typed patch over the whole snapshot).
    pub patch: Value,
    pub reason: String,
    /// Rides into the audit row. Direct applies are naturally idempotent:
    /// absolute values make a replayed patch a no-op (no new generation).
    pub idempotency_key: String,
    /// Optimistic base; a moved generation is a typed `StaleConfig`.
    pub expected_base_generation: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetConfigOutcome {
    pub generation: i64,
    pub changed_keys: Vec<String>,
}

pub struct SetConfig<'a, S: Store, C: Clock> {
    pub store: &'a S,
    pub clock: &'a C,
}

impl<S: Store, C: Clock> SetConfig<'_, S, C> {
    /// # Errors
    /// [`AppError::AdminForbidden`] for machine actors, sensitive keys, or a
    /// role the catalog does not list; [`AppError::ConfigInvalid`] (422) for
    /// unknown keys/bounds/delta violations; [`AppError::StaleConfig`] when
    /// the expected base moved; store failures. Nothing is observable on
    /// error.
    pub async fn execute(&self, cmd: SetConfigCmd) -> Result<SetConfigOutcome, AppError> {
        let AdminContext::Admin { token_digest, role } = &cmd.actor else {
            return Err(AppError::AdminForbidden(
                "config writes require an admin actor",
            ));
        };
        let keys = patch_keys(&cmd.patch)?;
        for key in &keys {
            // Unknown keys are a typed 422 BEFORE any role reasoning: a
            // catalog miss is a bad request, not a permission statement.
            if rule_for(key).is_none() {
                return Err(AppError::ConfigInvalid {
                    key: key.clone(),
                    reason: "unknown config key",
                });
            }
            if is_sensitive(key) {
                return Err(AppError::AdminForbidden(
                    "sensitive keys require the two-phase proposal flow",
                ));
            }
            if !role_may_write(*role, key) {
                return Err(AppError::AdminForbidden(
                    "role does not write this config key",
                ));
            }
        }

        let mut tx = self.store.ops_config_tx().await?;
        let applied = apply_patch_in_tx(
            tx.as_mut(),
            self.clock.now(),
            token_digest,
            &cmd.patch,
            cmd.expected_base_generation,
        )
        .await?;
        tx.audit_insert(crate::model::AdminAction {
            actor_role: *role,
            actor_token_digest: token_digest.clone(),
            action: "set_config".to_string(),
            subject: format!("config:{}", applied.changed_keys.join(",")),
            before: Some(applied.before.clone()),
            after: Some(applied.after.clone()),
            reason: Some(cmd.reason.clone()),
        })
        .await?;
        tx.commit().await?;
        Ok(SetConfigOutcome {
            generation: applied.generation,
            changed_keys: applied.changed_keys,
        })
    }
}

/// Extracts and sorts the patch's keys; a non-object or empty patch is 422.
pub(crate) fn patch_keys(patch: &Value) -> Result<Vec<String>, AppError> {
    let object = patch.as_object().ok_or(AppError::ConfigInvalid {
        key: "patch".to_string(),
        reason: "patch must be a JSON object",
    })?;
    if object.is_empty() {
        return Err(AppError::ConfigInvalid {
            key: "patch".to_string(),
            reason: "patch must not be empty",
        });
    }
    let mut keys: Vec<String> = object.keys().cloned().collect();
    keys.sort();
    Ok(keys)
}

/// One applied (or no-op) patch inside a still-open transaction.
pub(crate) struct AppliedPatch {
    pub generation: i64,
    pub changed_keys: Vec<String>,
    /// `{key: old}` / `{key: new}` objects for the caller's audit row.
    pub before: Value,
    pub after: Value,
}

/// The shared D24 apply core (direct writes AND proposal confirmation):
/// exclusive fences → generation lock → whole-snapshot validation → pause
/// timing rules → change rows → public pause frames → `ConfigChanged` wake.
/// The caller owns its audit row and the commit.
pub(crate) async fn apply_patch_in_tx(
    tx: &mut (dyn OpsConfigTx + '_),
    now: OffsetDateTime,
    applied_by: &str,
    patch: &Value,
    expected_base_generation: Option<i64>,
) -> Result<AppliedPatch, AppError> {
    let keys = patch_keys(patch)?;
    // Fences BEFORE the generation row (one-way lock graph): pause writers
    // wait only on in-flight trade/vote holders, never the queued convoy.
    let fences = exclusive_fences_for_patch(&keys);
    if !fences.is_empty() {
        tx.acquire_exclusive_fences(&fences).await?;
    }
    let base = tx.lock_generation().await?;
    if let Some(expected) = expected_base_generation {
        if expected != base {
            return Err(AppError::StaleConfig {
                preview_generation: expected,
                current_generation: base,
            });
        }
    }
    let snapshot = snapshot_of(&tx.config_entries().await?);
    let changes = validate_patch(&snapshot, patch)?;
    enforce_pause_timing(tx, now, patch).await?;

    let mut before = serde_json::Map::new();
    let mut after = serde_json::Map::new();
    for change in &changes {
        before.insert(
            change.key.clone(),
            change.old.clone().unwrap_or(Value::Null),
        );
        after.insert(change.key.clone(), change.new.clone());
    }

    let generation = if changes.is_empty() {
        base
    } else {
        let next = base + 1;
        tx.apply_changes(next, &changes, applied_by).await?;
        for event in pause_frames(&changes) {
            tx.append(event).await?;
        }
        tx.append(Event {
            event_type: "ConfigChanged",
            aggregate_type: "config",
            aggregate_id: uuid::Uuid::nil(),
            payload: json!({
                "generation": next,
                "keys": changes.iter().map(|c| c.key.clone()).collect::<Vec<_>>(),
            }),
        })
        .await?;
        next
    };
    Ok(AppliedPatch {
        generation,
        changed_keys: changes.into_iter().map(|c| c.key).collect(),
        before: Value::Object(before),
        after: Value::Object(after),
    })
}

/// D25 pause-write timing rules, checked against lock-free market reads:
/// - `market_paused:{id}` resume is forbidden inside `(tally_hidden_at,
///   closes_at]` (the D22 window is not an ops escape hatch);
/// - `voting_paused:{id}` may not be SET once `now ≥ tally_hidden_at`
///   (voiding one is always legal — the same rule auto-expires it).
async fn enforce_pause_timing(
    tx: &mut (dyn OpsConfigTx + '_),
    now: OffsetDateTime,
    patch: &Value,
) -> Result<(), AppError> {
    let object = patch.as_object().ok_or(AppError::ConfigInvalid {
        key: "patch".to_string(),
        reason: "config patch must be a JSON object",
    })?;
    for (key, value) in object {
        if let Some(id) = key.strip_prefix("market_paused:") {
            let market = market_of(id)?;
            let row = tx
                .market_times(market)
                .await?
                .ok_or(StoreError::NotFound("market"))?;
            let resuming = !pause_in_force(Some(value));
            if resuming && now > row.tally_hidden_at && now <= row.closes_at {
                return Err(AppError::ProposalConflict(
                    "trading resume is forbidden inside the hidden window",
                ));
            }
        } else if let Some(id) = key.strip_prefix("voting_paused:") {
            let market = market_of(id)?;
            let row = tx
                .market_times(market)
                .await?
                .ok_or(StoreError::NotFound("market"))?;
            let setting = pause_in_force(Some(value));
            if setting && now >= row.tally_hidden_at {
                return Err(AppError::ProposalConflict(
                    "a voting pause cannot be written once the hidden window began",
                ));
            }
        }
    }
    Ok(())
}

fn market_of(suffix: &str) -> Result<MarketId, AppError> {
    uuid::Uuid::parse_str(suffix)
        .map(MarketId)
        .map_err(|_| AppError::ConfigInvalid {
            key: suffix.to_string(),
            reason: "pause key requires a market UUID suffix",
        })
}

/// Public pause frames, emitted in the SAME transaction as the pause (D25).
fn pause_frames(changes: &[ConfigChange]) -> Vec<Event> {
    let mut events = Vec::new();
    for change in changes {
        let paused = pause_in_force(Some(&change.new));
        if change.key == "trading_paused" {
            events.push(Event {
                event_type: if paused {
                    "TradingPaused"
                } else {
                    "TradingResumed"
                },
                aggregate_type: "config",
                aggregate_id: uuid::Uuid::nil(),
                payload: json!({ "scope": "global" }),
            });
        } else if let Some(id) = change.key.strip_prefix("market_paused:") {
            if let Ok(market) = uuid::Uuid::parse_str(id) {
                events.push(Event {
                    event_type: if paused {
                        "TradingPaused"
                    } else {
                        "TradingResumed"
                    },
                    aggregate_type: "market",
                    aggregate_id: market,
                    payload: json!({ "market_id": id }),
                });
            }
        } else if let Some(id) = change.key.strip_prefix("voting_paused:") {
            if let Ok(market) = uuid::Uuid::parse_str(id) {
                events.push(Event {
                    event_type: if paused {
                        "MarketVotingPaused"
                    } else {
                        "MarketVotingResumed"
                    },
                    aggregate_type: "market",
                    aggregate_id: market,
                    payload: json!({ "market_id": id }),
                });
            }
        }
    }
    events
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use serde_json::json;

    use super::*;
    use crate::fakes::{FakeClock, InMemoryStore};
    use crate::model::AdminRole;

    fn admin(role: AdminRole) -> AdminContext {
        AdminContext::Admin {
            token_digest: format!("digest-{}", role.name()),
            role,
        }
    }

    fn cmd(actor: AdminContext, patch: Value) -> SetConfigCmd {
        SetConfigCmd {
            actor,
            patch,
            reason: "test".to_string(),
            idempotency_key: "set-1".to_string(),
            expected_base_generation: None,
        }
    }

    fn now() -> time::OffsetDateTime {
        time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    #[tokio::test]
    async fn a_direct_ops_key_applies_with_generation_change_row_audit_and_wake() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(now());
        let uc = SetConfig {
            store: &store,
            clock: &clock,
        };
        let outcome = uc
            .execute(cmd(admin(AdminRole::Ops), json!({"sweep_delay_secs": 300})))
            .await
            .unwrap();
        assert_eq!(outcome.generation, 2, "the 0008 seed is generation 1");
        assert_eq!(outcome.changed_keys, vec!["sweep_delay_secs".to_string()]);
        assert_eq!(store.config_value("sweep_delay_secs"), Some(json!(300)));
        assert_eq!(store.config_generation(), 2);
        let changes = store.config_changes_of(2);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].old, Some(json!(180)));
        assert_eq!(changes[0].new, json!(300));
        let audits = store.ops_audits();
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].action, "set_config");
        assert_eq!(audits[0].actor_role, AdminRole::Ops);
        let events = store.outbox();
        assert_eq!(events.len(), 1, "one ConfigChanged wake, no pause frame");
        assert_eq!(events[0].event_type, "ConfigChanged");
    }

    #[tokio::test]
    async fn sensitive_keys_and_wrong_roles_and_machines_are_403() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(now());
        let uc = SetConfig {
            store: &store,
            clock: &clock,
        };
        let err = uc
            .execute(cmd(
                admin(AdminRole::Finance),
                json!({"trade_fee_bps": 110}),
            ))
            .await
            .unwrap_err();
        assert!(
            matches!(err, AppError::AdminForbidden(_)),
            "sensitive → proposal flow"
        );
        let err = uc
            .execute(cmd(
                admin(AdminRole::Finance),
                json!({"sweep_delay_secs": 300}),
            ))
            .await
            .unwrap_err();
        assert!(
            matches!(err, AppError::AdminForbidden(_)),
            "ops key, finance actor"
        );
        let err = uc
            .execute(cmd(AdminContext::Machine, json!({"sweep_delay_secs": 300})))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::AdminForbidden(_)));
        assert_eq!(store.config_generation(), 1, "nothing observable on error");
        assert!(store.outbox().is_empty());
        assert!(store.ops_audits().is_empty());
    }

    #[tokio::test]
    async fn bounds_violations_are_typed_422_and_write_nothing() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(now());
        let uc = SetConfig {
            store: &store,
            clock: &clock,
        };
        let err = uc
            .execute(cmd(admin(AdminRole::Ops), json!({"sweep_delay_secs": 30})))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::ConfigInvalid { .. }));
        let err = uc
            .execute(cmd(admin(AdminRole::Ops), json!({"nope": 1})))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::ConfigInvalid { .. }));
        assert_eq!(store.config_generation(), 1);
    }

    #[tokio::test]
    async fn a_moved_expected_base_is_a_typed_409() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(now());
        let uc = SetConfig {
            store: &store,
            clock: &clock,
        };
        let mut command = cmd(admin(AdminRole::Ops), json!({"sweep_delay_secs": 300}));
        command.expected_base_generation = Some(7);
        let err = uc.execute(command).await.unwrap_err();
        assert_eq!(
            err,
            AppError::StaleConfig {
                preview_generation: 7,
                current_generation: 1
            }
        );
    }

    #[tokio::test]
    async fn a_noop_patch_keeps_the_generation_but_still_audits() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(now());
        let uc = SetConfig {
            store: &store,
            clock: &clock,
        };
        let outcome = uc
            .execute(cmd(admin(AdminRole::Ops), json!({"sweep_delay_secs": 180})))
            .await
            .unwrap();
        assert_eq!(outcome.generation, 1);
        assert!(outcome.changed_keys.is_empty());
        assert_eq!(store.config_generation(), 1);
        assert!(store.outbox().is_empty(), "no wake for a no-op");
        assert_eq!(
            store.ops_audits().len(),
            1,
            "the admin action is still a fact"
        );
    }

    #[tokio::test]
    async fn a_trading_pause_emits_its_public_frame_in_the_same_commit() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(now());
        let uc = SetConfig {
            store: &store,
            clock: &clock,
        };
        uc.execute(cmd(admin(AdminRole::Ops), json!({"trading_paused": true})))
            .await
            .unwrap();
        let events = store.outbox();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, "TradingPaused");
        assert_eq!(events[1].event_type, "ConfigChanged");
        // Resume emits the resumed frame.
        let uc2 = SetConfig {
            store: &store,
            clock: &clock,
        };
        uc2.execute(SetConfigCmd {
            actor: admin(AdminRole::Ops),
            patch: json!({"trading_paused": false}),
            reason: "resume".to_string(),
            idempotency_key: "set-2".to_string(),
            expected_base_generation: None,
        })
        .await
        .unwrap();
        let events = store.outbox();
        assert_eq!(events[2].event_type, "TradingResumed");
    }

    #[tokio::test]
    async fn market_pause_resume_is_forbidden_inside_the_hidden_window() {
        let store = InMemoryStore::new();
        let t0 = now();
        let market = store
            .add_market(
                "windowed",
                domain::market::MarketState::Live,
                t0 + time::Duration::hours(2), // closes_at
                t0 + time::Duration::hours(1), // tally_hidden_at
                domain::money::MicroShares(100_000_000),
                domain::money::BasisPoints(100),
            )
            .unwrap();
        let clock = FakeClock::at(t0);
        let pause_key = format!("market_paused:{}", market.id.0);
        SetConfig {
            store: &store,
            clock: &clock,
        }
        .execute(cmd(admin(AdminRole::Ops), json!({ &pause_key: true })))
        .await
        .unwrap();
        // Inside (tally_hidden_at, closes_at]: resume refused.
        clock.set(t0 + time::Duration::minutes(90));
        let err = SetConfig {
            store: &store,
            clock: &clock,
        }
        .execute(SetConfigCmd {
            actor: admin(AdminRole::Ops),
            patch: json!({ &pause_key: false }),
            reason: "resume".to_string(),
            idempotency_key: "set-3".to_string(),
            expected_base_generation: None,
        })
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::ProposalConflict(_)));
        // After closes_at: resume is legal again.
        clock.set(t0 + time::Duration::hours(3));
        SetConfig {
            store: &store,
            clock: &clock,
        }
        .execute(SetConfigCmd {
            actor: admin(AdminRole::Ops),
            patch: json!({ &pause_key: false }),
            reason: "resume".to_string(),
            idempotency_key: "set-4".to_string(),
            expected_base_generation: None,
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn concurrent_writers_serialize_on_the_generation_row() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(now());
        let uc = SetConfig {
            store: &store,
            clock: &clock,
        };
        let a = uc.execute(cmd(admin(AdminRole::Ops), json!({"sweep_delay_secs": 240})));
        let b = uc.execute(SetConfigCmd {
            actor: admin(AdminRole::Ops),
            patch: json!({"sweep_delay_secs": 300}),
            reason: "b".to_string(),
            idempotency_key: "set-b".to_string(),
            expected_base_generation: None,
        });
        let (a, b) = tokio::join!(a, b);
        let (a, b) = (a.unwrap(), b.unwrap());
        let mut generations = vec![a.generation, b.generation];
        generations.sort_unstable();
        assert_eq!(
            generations,
            vec![2, 3],
            "one generation per writer, serialized"
        );
        assert_eq!(store.config_generation(), 3);
    }

    #[test]
    fn patch_and_pause_frame_helpers_cover_empty_invalid_and_resume_shapes() {
        assert!(matches!(
            patch_keys(&json!({})),
            Err(AppError::ConfigInvalid { .. })
        ));
        assert!(matches!(
            market_of("bad"),
            Err(AppError::ConfigInvalid { .. })
        ));
        let market = uuid::Uuid::new_v4();
        assert_eq!(market_of(&market.to_string()).unwrap(), MarketId(market));
        let events = pause_frames(&[
            ConfigChange {
                key: format!("market_paused:{market}"),
                old: Some(json!(true)),
                new: json!(false),
            },
            ConfigChange {
                key: format!("voting_paused:{market}"),
                old: Some(json!(true)),
                new: json!(false),
            },
            ConfigChange {
                key: "market_paused:not-a-uuid".into(),
                old: None,
                new: json!(true),
            },
            ConfigChange {
                key: "voting_paused:not-a-uuid".into(),
                old: None,
                new: json!(true),
            },
        ]);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, "TradingResumed");
        assert_eq!(events[1].event_type, "MarketVotingResumed");
    }

    #[tokio::test]
    async fn matching_expected_generation_applies_normally() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(now());
        let mut command = cmd(admin(AdminRole::Ops), json!({"sweep_delay_secs": 300}));
        command.expected_base_generation = Some(1);
        assert_eq!(
            SetConfig {
                store: &store,
                clock: &clock
            }
            .execute(command)
            .await
            .unwrap()
            .generation,
            2
        );
    }
}
