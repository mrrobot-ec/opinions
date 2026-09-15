//! D24 two-phase proposals for sensitive config keys: durable rows, distinct
//! principals (schema-enforced twice), expiry, reject, and the emergency
//! revert pre-filled from `config_changes` history. Confirmation locks the
//! proposal row FIRST, then rides the same apply core as `SetConfig`
//! (exclusive fences → generation row → whole-snapshot revalidation) in one
//! transaction.

use serde_json::Value;

use crate::error::{AppError, StoreError};
use crate::model::{AdminAction, AdminContext, AdminRole, ConfigProposal, ProposalStatus};
use crate::ops::config::{is_sensitive, patch_hash, rule_for, WriteRule};
use crate::ops::set_config::{apply_patch_in_tx, patch_keys};
use crate::ports::{Clock, OpsConfigTx, Store};

/// Fallback TTL when the catalog row is unreadable (mirrors the 0008 seed).
const DEFAULT_TTL_SECS: i64 = 900;

/// May `role` propose/confirm a change to `key`? The listed role or
/// superadmin (D24 write path) — for BOTH sensitive keys and the
/// revert-of-history case that can carry direct keys.
fn role_may_propose(role: AdminRole, key: &str) -> bool {
    match rule_for(key).map(|r| r.write) {
        Some(WriteRule::TwoPhase(listed) | WriteRule::Direct(listed)) => {
            role == listed || role == AdminRole::Superadmin
        }
        None => false,
    }
}

#[derive(Debug, Clone)]
pub struct CreateProposalCmd {
    pub actor: AdminContext,
    /// Explicit patch — or `None` with `revert_of_generation` set.
    pub patch: Option<Value>,
    /// Emergency revert (D24): pre-fill the patch from this generation's
    /// change rows (`old` values). Never a trading pause — pause keys are
    /// stripped and a pause-only generation is a typed 422 (ops.md forbids
    /// pause-as-config-hatch).
    pub revert_of_generation: Option<i64>,
    pub reason: String,
    pub idempotency_key: String,
}

pub struct CreateProposal<'a, S: Store, C: Clock> {
    pub store: &'a S,
    pub clock: &'a C,
}

impl<S: Store, C: Clock> CreateProposal<'_, S, C> {
    /// # Errors
    /// [`AppError::AdminForbidden`] for machine actors or unlisted roles;
    /// [`AppError::ConfigInvalid`] for bad patches (422, validated against
    /// the CURRENT snapshot at create time — confirm revalidates);
    /// [`AppError::ProposalConflict`] when the idempotency key was reused
    /// with a different patch.
    pub async fn execute(&self, cmd: CreateProposalCmd) -> Result<ConfigProposal, AppError> {
        let AdminContext::Admin { token_digest, role } = &cmd.actor else {
            return Err(AppError::AdminForbidden(
                "config proposals require an admin actor",
            ));
        };
        let mut tx = self.store.ops_config_tx().await?;

        let patch = match (&cmd.patch, cmd.revert_of_generation) {
            (Some(patch), None) => {
                let keys = patch_keys(patch)?;
                for key in &keys {
                    if !is_sensitive(key) {
                        return Err(AppError::ConfigInvalid {
                            key: key.clone(),
                            reason: "non-sensitive keys apply directly through SetConfig",
                        });
                    }
                }
                patch.clone()
            }
            (None, Some(generation)) => revert_patch(tx.as_mut(), generation).await?,
            _ => {
                return Err(AppError::ConfigInvalid {
                    key: "patch".to_string(),
                    reason: "exactly one of patch or revert_of_generation is required",
                });
            }
        };
        let keys = patch_keys(&patch)?;
        for key in &keys {
            if !role_may_propose(*role, key) {
                return Err(AppError::AdminForbidden(
                    "role does not propose changes to this config key",
                ));
            }
        }

        // Early whole-snapshot validation (422 now beats 409 later); the
        // generation lock also pins base_generation.
        let base = tx.lock_generation().await?;
        let snapshot = crate::ops::config::snapshot_of(&tx.config_entries().await?);
        crate::ops::config::validate_patch(&snapshot, &patch)?;

        let hash = patch_hash(&patch);
        if let Some(existing) = tx.proposal_by_idempotency_key(&cmd.idempotency_key).await? {
            if existing.patch_hash == hash {
                return Ok(existing); // idempotent create
            }
            return Err(AppError::ProposalConflict(
                "idempotency key was reused with a different patch",
            ));
        }

        let ttl = snapshot
            .get("proposal_ttl_secs")
            .and_then(Value::as_i64)
            .unwrap_or(DEFAULT_TTL_SECS);
        let proposal = ConfigProposal {
            id: uuid::Uuid::new_v4(),
            idempotency_key: cmd.idempotency_key.clone(),
            patch,
            patch_hash: hash,
            base_generation: base,
            proposer_token_id: token_digest.clone(),
            proposer_role: *role,
            reason: cmd.reason.clone(),
            status: ProposalStatus::Pending,
            expires_at: self.clock.now() + time::Duration::seconds(ttl),
            confirmer_token_id: None,
            resulting_generation: None,
        };
        tx.insert_proposal(proposal.clone()).await?;
        tx.audit_insert(AdminAction {
            actor_role: *role,
            actor_token_digest: token_digest.clone(),
            action: "propose_config".to_string(),
            subject: format!("config_proposal:{}", proposal.id),
            before: None,
            after: Some(proposal.patch.clone()),
            reason: Some(cmd.reason),
        })
        .await?;
        tx.commit().await?;
        Ok(proposal)
    }
}

/// Pre-fills the revert patch from one generation's change history: every
/// changed key back to its `old` value, pause keys stripped.
async fn revert_patch(tx: &mut (dyn OpsConfigTx + '_), generation: i64) -> Result<Value, AppError> {
    let changes = tx.changes_in_generation(generation).await?;
    revert_patch_from_changes(changes)
}

fn revert_patch_from_changes(changes: Vec<crate::model::ConfigChange>) -> Result<Value, AppError> {
    if changes.is_empty() {
        return Err(AppError::Store(StoreError::NotFound("config generation")));
    }
    let mut object = serde_json::Map::new();
    for change in changes {
        let is_pause = change.key == "trading_paused"
            || change.key.starts_with("market_paused:")
            || change.key.starts_with("voting_paused:");
        if is_pause {
            continue; // never a trading pause (D24)
        }
        let Some(old) = change.old else {
            return Err(AppError::ConfigInvalid {
                key: change.key,
                reason: "cannot revert a key's creation",
            });
        };
        object.insert(change.key, old);
    }
    if object.is_empty() {
        return Err(AppError::ConfigInvalid {
            key: "patch".to_string(),
            reason: "generation contains only pause changes; nothing to revert",
        });
    }
    Ok(Value::Object(object))
}

#[derive(Debug, Clone)]
pub struct SettleProposalCmd {
    pub actor: AdminContext,
    pub id: uuid::Uuid,
    pub reason: Option<String>,
}

pub struct ConfirmProposal<'a, S: Store, C: Clock> {
    pub store: &'a S,
    pub clock: &'a C,
}

impl<S: Store, C: Clock> ConfirmProposal<'_, S, C> {
    /// # Errors
    /// [`AppError::AdminForbidden`] (403) when the confirmer token equals
    /// the proposer token or the role is unlisted;
    /// [`AppError::ProposalConflict`] (409) for settled/expired proposals
    /// or a base generation that moved incompatibly; store failures.
    pub async fn execute(&self, cmd: SettleProposalCmd) -> Result<ConfigProposal, AppError> {
        let AdminContext::Admin { token_digest, role } = &cmd.actor else {
            return Err(AppError::AdminForbidden(
                "config proposals require an admin actor",
            ));
        };
        let now = self.clock.now();
        let mut tx = self.store.ops_config_tx().await?;
        let mut proposal = tx.proposal_for_update(cmd.id).await?;
        if proposal.status != ProposalStatus::Pending {
            return Err(AppError::ProposalConflict("proposal is already settled"));
        }
        if now >= proposal.expires_at {
            // Settle the expiry durably (audited), then refuse.
            proposal.status = ProposalStatus::Expired;
            tx.save_proposal(&proposal).await?;
            tx.audit_insert(settle_audit(
                *role,
                token_digest,
                &proposal,
                "expire_config_proposal",
                None,
            ))
            .await?;
            tx.commit().await?;
            return Err(AppError::ProposalConflict("proposal has expired"));
        }
        if *token_digest == proposal.proposer_token_id {
            return Err(AppError::AdminForbidden(
                "confirmation requires a principal distinct from the proposer",
            ));
        }
        for key in patch_keys(&proposal.patch)? {
            if !role_may_propose(*role, &key) {
                return Err(AppError::AdminForbidden(
                    "role does not confirm changes to this config key",
                ));
            }
        }

        // The shared apply core: exclusive fences → generation row → FULL
        // prospective-snapshot revalidation against CURRENT state. A base
        // that moved incompatibly surfaces as a typed conflict, never a
        // silent apply (D24).
        let applied = apply_patch_in_tx(tx.as_mut(), now, token_digest, &proposal.patch, None)
            .await
            .map_err(|error| match error {
                AppError::ConfigInvalid { .. } => {
                    AppError::ProposalConflict("the base generation moved incompatibly")
                }
                other => other,
            })?;

        proposal.status = ProposalStatus::Confirmed;
        proposal.confirmer_token_id = Some(token_digest.clone());
        proposal.resulting_generation = Some(applied.generation);
        tx.save_proposal(&proposal).await?;
        tx.audit_insert(settle_audit(
            *role,
            token_digest,
            &proposal,
            "confirm_config_proposal",
            cmd.reason,
        ))
        .await?;
        tx.commit().await?;
        Ok(proposal)
    }
}

pub struct RejectProposal<'a, S: Store, C: Clock> {
    pub store: &'a S,
    pub clock: &'a C,
}

impl<S: Store, C: Clock> RejectProposal<'_, S, C> {
    /// Either principal may reject (audited). Expired proposals settle as
    /// expired.
    ///
    /// # Errors
    /// [`AppError::ProposalConflict`] for already-settled proposals; store
    /// failures.
    pub async fn execute(&self, cmd: SettleProposalCmd) -> Result<ConfigProposal, AppError> {
        let AdminContext::Admin { token_digest, role } = &cmd.actor else {
            return Err(AppError::AdminForbidden(
                "config proposals require an admin actor",
            ));
        };
        let mut tx = self.store.ops_config_tx().await?;
        let mut proposal = tx.proposal_for_update(cmd.id).await?;
        if proposal.status != ProposalStatus::Pending {
            return Err(AppError::ProposalConflict("proposal is already settled"));
        }
        let (status, action) = if self.clock.now() >= proposal.expires_at {
            (ProposalStatus::Expired, "expire_config_proposal")
        } else {
            (ProposalStatus::Rejected, "reject_config_proposal")
        };
        proposal.status = status;
        tx.save_proposal(&proposal).await?;
        tx.audit_insert(settle_audit(
            *role,
            token_digest,
            &proposal,
            action,
            cmd.reason,
        ))
        .await?;
        tx.commit().await?;
        Ok(proposal)
    }
}

fn settle_audit(
    role: AdminRole,
    digest: &str,
    proposal: &ConfigProposal,
    action: &str,
    reason: Option<String>,
) -> AdminAction {
    AdminAction {
        actor_role: role,
        actor_token_digest: digest.to_string(),
        action: action.to_string(),
        subject: format!("config_proposal:{}", proposal.id),
        before: None,
        after: Some(proposal.patch.clone()),
        reason,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use serde_json::json;

    use super::*;
    use crate::fakes::{FakeClock, InMemoryStore};

    fn admin(role: AdminRole, token: &str) -> AdminContext {
        AdminContext::Admin {
            token_digest: token.to_string(),
            role,
        }
    }

    fn now() -> time::OffsetDateTime {
        time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    async fn propose_fee(
        store: &InMemoryStore,
        clock: &FakeClock,
        key: &str,
        fee: i64,
    ) -> ConfigProposal {
        CreateProposal { store, clock }
            .execute(CreateProposalCmd {
                actor: admin(AdminRole::Finance, "finance-1"),
                patch: Some(json!({ "trade_fee_bps": fee })),
                revert_of_generation: None,
                reason: "fee move".to_string(),
                idempotency_key: key.to_string(),
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn the_two_phase_happy_path_applies_with_distinct_tokens() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(now());
        let proposal = propose_fee(&store, &clock, "p-1", 115).await;
        assert_eq!(proposal.status, ProposalStatus::Pending);
        assert_eq!(proposal.base_generation, 1);
        assert_eq!(
            store.config_value("trade_fee_bps"),
            Some(json!(100)),
            "no apply yet"
        );

        let confirmed = ConfirmProposal {
            store: &store,
            clock: &clock,
        }
        .execute(SettleProposalCmd {
            actor: admin(AdminRole::Finance, "finance-2"),
            id: proposal.id,
            reason: None,
        })
        .await
        .unwrap();
        assert_eq!(confirmed.status, ProposalStatus::Confirmed);
        assert_eq!(confirmed.resulting_generation, Some(2));
        assert_eq!(store.config_value("trade_fee_bps"), Some(json!(115)));
        assert_eq!(store.config_generation(), 2);
        let audits = store.ops_audits();
        assert_eq!(audits.len(), 2, "propose + confirm each audit");
        assert_eq!(audits[0].action, "propose_config");
        assert_eq!(audits[1].action, "confirm_config_proposal");
    }

    #[tokio::test]
    async fn same_token_confirmation_is_403_and_applies_nothing() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(now());
        let proposal = propose_fee(&store, &clock, "p-2", 115).await;
        let err = ConfirmProposal {
            store: &store,
            clock: &clock,
        }
        .execute(SettleProposalCmd {
            actor: admin(AdminRole::Finance, "finance-1"),
            id: proposal.id,
            reason: None,
        })
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::AdminForbidden(_)));
        assert_eq!(store.config_value("trade_fee_bps"), Some(json!(100)));
        assert_eq!(store.config_generation(), 1);
    }

    #[tokio::test]
    async fn a_non_sensitive_key_is_rejected_toward_set_config() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(now());
        let err = CreateProposal {
            store: &store,
            clock: &clock,
        }
        .execute(CreateProposalCmd {
            actor: admin(AdminRole::Ops, "ops-1"),
            patch: Some(json!({ "sweep_delay_secs": 300 })),
            revert_of_generation: None,
            reason: "r".to_string(),
            idempotency_key: "p-3".to_string(),
        })
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::ConfigInvalid { .. }));
    }

    #[tokio::test]
    async fn out_of_bounds_patches_422_at_create_time() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(now());
        let err = CreateProposal {
            store: &store,
            clock: &clock,
        }
        .execute(CreateProposalCmd {
            actor: admin(AdminRole::Finance, "finance-1"),
            patch: Some(json!({ "trade_fee_bps": 500 })),
            revert_of_generation: None,
            reason: "r".to_string(),
            idempotency_key: "p-4".to_string(),
        })
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::ConfigInvalid { .. }));
    }

    #[tokio::test]
    async fn create_is_idempotent_by_key_and_conflicts_on_a_different_patch() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(now());
        let first = propose_fee(&store, &clock, "p-5", 115).await;
        let replay = propose_fee(&store, &clock, "p-5", 115).await;
        assert_eq!(first.id, replay.id, "same key + same patch replays");
        let err = CreateProposal {
            store: &store,
            clock: &clock,
        }
        .execute(CreateProposalCmd {
            actor: admin(AdminRole::Finance, "finance-1"),
            patch: Some(json!({ "trade_fee_bps": 120 })),
            revert_of_generation: None,
            reason: "different".to_string(),
            idempotency_key: "p-5".to_string(),
        })
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::ProposalConflict(_)));
    }

    #[tokio::test]
    async fn expiry_refuses_confirmation_and_settles_the_row() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(now());
        let proposal = propose_fee(&store, &clock, "p-6", 115).await;
        clock.advance(time::Duration::seconds(901)); // past the 0008 TTL seed
        let err = ConfirmProposal {
            store: &store,
            clock: &clock,
        }
        .execute(SettleProposalCmd {
            actor: admin(AdminRole::Finance, "finance-2"),
            id: proposal.id,
            reason: None,
        })
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::ProposalConflict(_)));
        assert_eq!(
            store.proposal(proposal.id).unwrap().status,
            ProposalStatus::Expired,
            "the expiry is settled durably"
        );
        assert_eq!(store.config_generation(), 1);
        // A settled proposal cannot be confirmed later either.
        let err = ConfirmProposal {
            store: &store,
            clock: &clock,
        }
        .execute(SettleProposalCmd {
            actor: admin(AdminRole::Finance, "finance-2"),
            id: proposal.id,
            reason: None,
        })
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::ProposalConflict(_)));
    }

    #[tokio::test]
    async fn reject_unblocks_a_fresh_proposal_for_the_same_key() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(now());
        let first = propose_fee(&store, &clock, "p-7", 115).await;
        let rejected = RejectProposal {
            store: &store,
            clock: &clock,
        }
        .execute(SettleProposalCmd {
            actor: admin(AdminRole::Finance, "finance-1"),
            id: first.id,
            reason: Some("wrong number".to_string()),
        })
        .await
        .unwrap();
        assert_eq!(rejected.status, ProposalStatus::Rejected);
        assert_eq!(store.config_generation(), 1, "reject applies nothing");

        let second = propose_fee(&store, &clock, "p-8", 118).await;
        let confirmed = ConfirmProposal {
            store: &store,
            clock: &clock,
        }
        .execute(SettleProposalCmd {
            actor: admin(AdminRole::Superadmin, "root-1"),
            id: second.id,
            reason: None,
        })
        .await
        .unwrap();
        assert_eq!(confirmed.status, ProposalStatus::Confirmed);
        assert_eq!(store.config_value("trade_fee_bps"), Some(json!(118)));
    }

    #[tokio::test]
    async fn an_incompatibly_moved_base_conflicts_at_confirm_time() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(now());
        // Proposal A: 100 → 118. Proposal B: 100 → 85 (−15, within Δ20).
        let a = propose_fee(&store, &clock, "p-9", 118).await;
        let b = CreateProposal {
            store: &store,
            clock: &clock,
        }
        .execute(CreateProposalCmd {
            actor: admin(AdminRole::Finance, "finance-1"),
            patch: Some(json!({ "trade_fee_bps": 85 })),
            revert_of_generation: None,
            reason: "down".to_string(),
            idempotency_key: "p-10".to_string(),
        })
        .await
        .unwrap();
        ConfirmProposal {
            store: &store,
            clock: &clock,
        }
        .execute(SettleProposalCmd {
            actor: admin(AdminRole::Finance, "finance-2"),
            id: b.id,
            reason: None,
        })
        .await
        .unwrap();
        // Fee is now 85; applying A's 118 would move |118−85| = 33 > Δ20.
        let err = ConfirmProposal {
            store: &store,
            clock: &clock,
        }
        .execute(SettleProposalCmd {
            actor: admin(AdminRole::Finance, "finance-2"),
            id: a.id,
            reason: None,
        })
        .await
        .unwrap_err();
        assert_eq!(
            err,
            AppError::ProposalConflict("the base generation moved incompatibly")
        );
        assert_eq!(store.config_value("trade_fee_bps"), Some(json!(85)));
    }

    #[tokio::test]
    async fn emergency_revert_prefills_from_history_under_the_same_dual_control() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(now());
        let p = propose_fee(&store, &clock, "p-11", 115).await;
        ConfirmProposal {
            store: &store,
            clock: &clock,
        }
        .execute(SettleProposalCmd {
            actor: admin(AdminRole::Finance, "finance-2"),
            id: p.id,
            reason: None,
        })
        .await
        .unwrap();
        assert_eq!(store.config_generation(), 2);

        let revert = CreateProposal {
            store: &store,
            clock: &clock,
        }
        .execute(CreateProposalCmd {
            actor: admin(AdminRole::Finance, "finance-1"),
            patch: None,
            revert_of_generation: Some(2),
            reason: "revert the fee move".to_string(),
            idempotency_key: "p-12".to_string(),
        })
        .await
        .unwrap();
        assert_eq!(
            revert.patch,
            json!({ "trade_fee_bps": 100 }),
            "pre-filled from old"
        );
        // Same dual control: same-token confirm still 403.
        let err = ConfirmProposal {
            store: &store,
            clock: &clock,
        }
        .execute(SettleProposalCmd {
            actor: admin(AdminRole::Finance, "finance-1"),
            id: revert.id,
            reason: None,
        })
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::AdminForbidden(_)));
        ConfirmProposal {
            store: &store,
            clock: &clock,
        }
        .execute(SettleProposalCmd {
            actor: admin(AdminRole::Finance, "finance-2"),
            id: revert.id,
            reason: None,
        })
        .await
        .unwrap();
        assert_eq!(store.config_value("trade_fee_bps"), Some(json!(100)));
        assert_eq!(store.config_generation(), 3, "a revert is a NEW generation");
    }

    #[tokio::test]
    async fn a_voting_pause_rides_proposals_and_respects_the_hidden_window() {
        let store = InMemoryStore::new();
        let t0 = now();
        let market = store
            .add_market(
                "pausable",
                domain::market::MarketState::Live,
                t0 + time::Duration::hours(2),
                t0 + time::Duration::hours(1),
                domain::money::MicroShares(100_000_000),
                domain::money::BasisPoints(100),
            )
            .unwrap();
        let clock = FakeClock::at(t0);
        let key = format!("voting_paused:{}", market.id.0);
        let p = CreateProposal {
            store: &store,
            clock: &clock,
        }
        .execute(CreateProposalCmd {
            actor: admin(AdminRole::Superadmin, "root-1"),
            patch: Some(json!({ &key: true })),
            revert_of_generation: None,
            reason: "integrity hold".to_string(),
            idempotency_key: "p-13".to_string(),
        })
        .await
        .unwrap();
        // Confirmation inside the hidden window is refused (write forbidden
        // once now ≥ tally_hidden_at).
        clock.set(t0 + time::Duration::minutes(61));
        let err = ConfirmProposal {
            store: &store,
            clock: &clock,
        }
        .execute(SettleProposalCmd {
            actor: admin(AdminRole::Superadmin, "root-2"),
            id: p.id,
            reason: None,
        })
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::ProposalConflict(_)));

        // Before the window it lands, with the public frame in the commit.
        clock.set(t0 + time::Duration::minutes(5));
        let p2 = CreateProposal {
            store: &store,
            clock: &clock,
        }
        .execute(CreateProposalCmd {
            actor: admin(AdminRole::Superadmin, "root-1"),
            patch: Some(json!({ &key: true })),
            revert_of_generation: None,
            reason: "integrity hold".to_string(),
            idempotency_key: "p-14".to_string(),
        })
        .await
        .unwrap();
        ConfirmProposal {
            store: &store,
            clock: &clock,
        }
        .execute(SettleProposalCmd {
            actor: admin(AdminRole::Superadmin, "root-2"),
            id: p2.id,
            reason: None,
        })
        .await
        .unwrap();
        assert_eq!(store.config_value(&key), Some(json!(true)));
        assert!(store
            .outbox()
            .iter()
            .any(|e| e.event_type == "MarketVotingPaused"));

        // Auto-expiry is semantic: the stored value remains true. Re-setting
        // that same value after the boundary is still a forbidden write, not
        // a no-op escape around the timing fence.
        clock.set(t0 + time::Duration::minutes(61));
        let p3 = CreateProposal {
            store: &store,
            clock: &clock,
        }
        .execute(CreateProposalCmd {
            actor: admin(AdminRole::Superadmin, "root-1"),
            patch: Some(json!({ &key: true })),
            revert_of_generation: None,
            reason: "late repeat".to_string(),
            idempotency_key: "p-14-late-repeat".to_string(),
        })
        .await
        .unwrap();
        assert!(matches!(
            ConfirmProposal {
                store: &store,
                clock: &clock,
            }
            .execute(SettleProposalCmd {
                actor: admin(AdminRole::Superadmin, "root-2"),
                id: p3.id,
                reason: None,
            })
            .await,
            Err(AppError::ProposalConflict(_))
        ));
    }

    #[tokio::test]
    async fn machine_actors_and_unlisted_roles_cannot_propose() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(now());
        let err = CreateProposal {
            store: &store,
            clock: &clock,
        }
        .execute(CreateProposalCmd {
            actor: AdminContext::Machine,
            patch: Some(json!({ "trade_fee_bps": 110 })),
            revert_of_generation: None,
            reason: "r".to_string(),
            idempotency_key: "p-15".to_string(),
        })
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::AdminForbidden(_)));
        let err = CreateProposal {
            store: &store,
            clock: &clock,
        }
        .execute(CreateProposalCmd {
            actor: admin(AdminRole::Curator, "curator-1"),
            patch: Some(json!({ "trade_fee_bps": 110 })),
            revert_of_generation: None,
            reason: "r".to_string(),
            idempotency_key: "p-16".to_string(),
        })
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::AdminForbidden(_)));
    }

    #[tokio::test]
    async fn unknown_proposals_are_not_found() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(now());
        let err = ConfirmProposal {
            store: &store,
            clock: &clock,
        }
        .execute(SettleProposalCmd {
            actor: admin(AdminRole::Finance, "finance-2"),
            id: uuid::Uuid::new_v4(),
            reason: None,
        })
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::Store(StoreError::NotFound(_))));
    }

    #[test]
    fn revert_prefill_rejects_missing_pause_only_and_created_keys() {
        use crate::model::ConfigChange;

        assert!(matches!(
            revert_patch_from_changes(Vec::new()),
            Err(AppError::Store(StoreError::NotFound("config generation")))
        ));
        assert!(matches!(
            revert_patch_from_changes(vec![ConfigChange {
                key: "trading_paused".into(),
                old: Some(json!(false)),
                new: json!(true),
            }]),
            Err(AppError::ConfigInvalid { .. })
        ));
        assert!(matches!(
            revert_patch_from_changes(vec![ConfigChange {
                key: "trade_fee_bps".into(),
                old: None,
                new: json!(100),
            }]),
            Err(AppError::ConfigInvalid { .. })
        ));
        assert_eq!(
            revert_patch_from_changes(vec![ConfigChange {
                key: "trade_fee_bps".into(),
                old: Some(json!(100)),
                new: json!(110),
            }])
            .unwrap(),
            json!({"trade_fee_bps": 100})
        );
        assert!(!role_may_propose(AdminRole::Superadmin, "unknown"));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn malformed_and_unauthorized_settlement_paths_are_typed() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(now());
        for (patch, revert) in [(None, None), (Some(json!({"trade_fee_bps": 110})), Some(1))] {
            assert!(matches!(
                CreateProposal {
                    store: &store,
                    clock: &clock
                }
                .execute(CreateProposalCmd {
                    actor: admin(AdminRole::Finance, "finance-1"),
                    patch,
                    revert_of_generation: revert,
                    reason: "bad shape".into(),
                    idempotency_key: uuid::Uuid::new_v4().to_string(),
                })
                .await,
                Err(AppError::ConfigInvalid { .. })
            ));
        }
        assert!(matches!(
            CreateProposal {
                store: &store,
                clock: &clock
            }
            .execute(CreateProposalCmd {
                actor: admin(AdminRole::Finance, "finance-1"),
                patch: None,
                revert_of_generation: Some(99),
                reason: "missing".into(),
                idempotency_key: "missing-generation".into(),
            })
            .await,
            Err(AppError::Store(StoreError::NotFound(_)))
        ));

        let proposal = propose_fee(&store, &clock, "settle-auth", 110).await;
        assert!(matches!(
            ConfirmProposal {
                store: &store,
                clock: &clock
            }
            .execute(SettleProposalCmd {
                actor: AdminContext::Machine,
                id: proposal.id,
                reason: None,
            })
            .await,
            Err(AppError::AdminForbidden(_))
        ));
        assert!(matches!(
            ConfirmProposal {
                store: &store,
                clock: &clock
            }
            .execute(SettleProposalCmd {
                actor: admin(AdminRole::Ops, "ops-2"),
                id: proposal.id,
                reason: None,
            })
            .await,
            Err(AppError::AdminForbidden(_))
        ));
        assert!(matches!(
            RejectProposal {
                store: &store,
                clock: &clock
            }
            .execute(SettleProposalCmd {
                actor: AdminContext::Machine,
                id: proposal.id,
                reason: None,
            })
            .await,
            Err(AppError::AdminForbidden(_))
        ));
        RejectProposal {
            store: &store,
            clock: &clock,
        }
        .execute(SettleProposalCmd {
            actor: admin(AdminRole::Finance, "finance-1"),
            id: proposal.id,
            reason: None,
        })
        .await
        .unwrap();
        assert!(matches!(
            RejectProposal {
                store: &store,
                clock: &clock
            }
            .execute(SettleProposalCmd {
                actor: admin(AdminRole::Finance, "finance-1"),
                id: proposal.id,
                reason: None,
            })
            .await,
            Err(AppError::ProposalConflict(_))
        ));

        let expiring = propose_fee(&store, &clock, "reject-expired", 115).await;
        clock.advance(time::Duration::seconds(901));
        let expired = RejectProposal {
            store: &store,
            clock: &clock,
        }
        .execute(SettleProposalCmd {
            actor: admin(AdminRole::Finance, "finance-1"),
            id: expiring.id,
            reason: None,
        })
        .await
        .unwrap();
        assert_eq!(expired.status, ProposalStatus::Expired);
    }
}
