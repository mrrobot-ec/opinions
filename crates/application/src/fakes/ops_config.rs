//! Wave-W1 real fake for the D24 config plane plus the D25 fence-point role
//! on the shared in-memory transaction. Replaces `UnavailableOpsConfigTx`.
//!
//! Semantics honored: generation-lock serialization (writer B blocks behind
//! writer A until commit), buffered writes observable only at commit, and
//! authoritative fence-point reads of COMMITTED state. Documented
//! divergences (mirroring the fakes module's charter): fence acquisition is
//! a no-op (the blocking proofs are Pg contracts) and request fingerprints
//! persist immediately — they are only ever consulted on an idempotency-key
//! HIT, which requires the paired write to have committed.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex as PlMutex;
use serde_json::{json, Value};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use uuid::Uuid;

use crate::error::StoreError;
use crate::model::{
    AdminAction, ConfigChange, ConfigEntry, ConfigProposal, Event, MarketId, MarketRow,
};
use crate::ops::config::{FencePoint, OpsWriteSupport};
use crate::ports::{AuditWrite, Committable, OpsConfigTx, OutboxWriter};

use super::{InMemTx, InMemoryStore, Shared};

/// The 0008 catalog seed, mirrored so a fresh fake store equals a freshly
/// migrated database (generation 1, one change row per seeded key).
fn seed_entries() -> BTreeMap<String, Value> {
    [
        ("trade_fee_bps", json!(100)),
        ("min_fee_bps", json!(10)),
        ("discount_flip_window_secs", json!(3600)),
        ("fee_discount_bp_by_tier", json!([0, 0, 10, 20, 30])),
        (
            "position_cap_micro_by_tier",
            json!([
                25_000_000i64,
                50_000_000i64,
                100_000_000i64,
                250_000_000i64,
                500_000_000i64
            ]),
        ),
        (
            "rep_tier_thresholds_micro",
            json!([0, 200_000, 400_000, 600_000, 800_000]),
        ),
        ("rep_score_min_pot_micro", json!(50_000_000)),
        ("seed_micro_daily", json!(500_000_000i64)),
        ("seed_micro_flash", json!(100_000_000i64)),
        ("daily_seed_budget_micro", json!(1_000_000_000i64)),
        ("hidden_window_secs", json!(300)),
        ("min_votes_to_resolve_floor", json!(3)),
        ("oi_floor_micro", json!(0)),
        ("payout_hold_threshold_micro", json!(500_000_000i64)),
        ("sweep_delay_secs", json!(180)),
        ("max_votes_per_window", json!(30)),
        ("vote_window_secs", json!(3600)),
        ("integrity_burst_multiplier_ppm", json!(2_000_000)),
        ("integrity_young_share_max_ppm", json!(500_000)),
        ("integrity_device_share_max_ppm", json!(600_000)),
        ("integrity_subnet_share_max_ppm", json!(600_000)),
        ("integrity_min_metadata_coverage_ppm", json!(500_000)),
        ("flash_cadence_secs", json!(3600)),
        ("daily_slots", json!(2)),
        ("feature_flash_markets", json!(true)),
        ("feature_comments", json!(true)),
        ("feature_referrals", json!(false)),
        ("trading_paused", json!(false)),
        ("faucet_per_call_cap_micro", json!(1_000_000_000i64)),
        ("remedial_credit_market_cap_micro", json!(500_000_000i64)),
        ("remedial_credit_daily_cap_micro", json!(2_000_000_000i64)),
        ("receivable_outstanding_cap_micro", json!(10_000_000_000i64)),
        ("writeoff_per_item_cap_micro", json!(500_000_000i64)),
        ("writeoff_daily_cap_micro", json!(2_000_000_000i64)),
        ("proposal_ttl_secs", json!(900)),
        ("dual_control_delay_secs", json!(60)),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

/// Committed config-plane state (hangs off [`Shared`] as one field).
pub(super) struct OpsFakeState {
    entries: BTreeMap<String, Value>,
    generation: i64,
    /// `(generation, change)` rows, oldest first — the immutable history.
    changes: Vec<(i64, ConfigChange)>,
    /// Oldest generation whose history is retained (D24 bounded history).
    watermark: i64,
    proposals: BTreeMap<Uuid, ConfigProposal>,
    audits: Vec<AdminAction>,
    fingerprints: BTreeMap<String, String>,
}

impl Default for OpsFakeState {
    fn default() -> Self {
        let entries = seed_entries();
        let changes = entries
            .iter()
            .map(|(key, value)| {
                (
                    1,
                    ConfigChange {
                        key: key.clone(),
                        old: None,
                        new: value.clone(),
                    },
                )
            })
            .collect();
        Self {
            entries,
            generation: 1,
            changes,
            watermark: 1,
            proposals: BTreeMap::new(),
            audits: Vec::new(),
            fingerprints: BTreeMap::new(),
        }
    }
}

/// The config-plane shard of [`Shared`]: committed state plus the generation
/// write lock that serializes config writers (the fake's stand-in for the
/// singleton-row lock).
pub(super) struct OpsShared {
    pub(crate) state: PlMutex<OpsFakeState>,
    generation_lock: Arc<AsyncMutex<()>>,
}

impl Default for OpsShared {
    fn default() -> Self {
        Self {
            state: PlMutex::new(OpsFakeState::default()),
            generation_lock: Arc::new(AsyncMutex::new(())),
        }
    }
}

impl OpsShared {
    // Wave-facing config peeks: not every lane consumes both yet.
    #[allow(dead_code)]
    pub(crate) fn entry_bool(&self, key: &str) -> bool {
        self.state
            .lock()
            .entries
            .get(key)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    #[allow(dead_code)]
    pub(crate) fn entry_i64(&self, key: &str) -> Option<i64> {
        self.state.lock().entries.get(key).and_then(Value::as_i64)
    }
}

// ---------------------------------------------------------------------------
// The real fake OpsConfigTx
// ---------------------------------------------------------------------------

/// Buffered config write transaction. Writes land at commit under the state
/// mutex; dropping the value rolls everything back and releases the
/// generation lock.
pub(super) struct FakeOpsConfigTx {
    shared: Arc<Shared>,
    generation_guard: Option<OwnedMutexGuard<()>>,
    pending_apply: Option<(i64, Vec<ConfigChange>, String)>,
    pending_proposal_inserts: Vec<ConfigProposal>,
    pending_proposal_saves: Vec<ConfigProposal>,
    pending_audits: Vec<AdminAction>,
    pending_events: Vec<Event>,
}

impl FakeOpsConfigTx {
    pub(super) fn open(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            generation_guard: None,
            pending_apply: None,
            pending_proposal_inserts: Vec::new(),
            pending_proposal_saves: Vec::new(),
            pending_audits: Vec::new(),
            pending_events: Vec::new(),
        }
    }
}

#[async_trait]
impl OpsConfigTx for FakeOpsConfigTx {
    async fn lock_generation(&mut self) -> Result<i64, StoreError> {
        if self.generation_guard.is_none() {
            let lock = Arc::clone(&self.shared.ops.generation_lock);
            self.generation_guard = Some(lock.lock_owned().await);
        }
        Ok(self.shared.ops.state.lock().generation)
    }

    async fn config_entries(&mut self) -> Result<Vec<ConfigEntry>, StoreError> {
        Ok(self
            .shared
            .ops
            .state
            .lock()
            .entries
            .iter()
            .map(|(key, value)| ConfigEntry {
                key: key.clone(),
                value: value.clone(),
            })
            .collect())
    }

    async fn apply_changes(
        &mut self,
        generation: i64,
        changes: &[ConfigChange],
        applied_by: &str,
    ) -> Result<(), StoreError> {
        if self.generation_guard.is_none() {
            return Err(StoreError::Invariant(
                "apply_changes requires the generation lock",
            ));
        }
        self.pending_apply = Some((generation, changes.to_vec(), applied_by.to_string()));
        Ok(())
    }

    async fn changes_for_key_since(
        &mut self,
        key: &str,
        generation: i64,
    ) -> Result<Vec<ConfigChange>, StoreError> {
        Ok(self
            .shared
            .ops
            .state
            .lock()
            .changes
            .iter()
            .filter(|(g, c)| *g > generation && c.key == key)
            .map(|(_, c)| c.clone())
            .collect())
    }

    async fn insert_proposal(&mut self, proposal: ConfigProposal) -> Result<(), StoreError> {
        let state = self.shared.ops.state.lock();
        if state
            .proposals
            .values()
            .any(|p| p.idempotency_key == proposal.idempotency_key)
            || self
                .pending_proposal_inserts
                .iter()
                .any(|p| p.idempotency_key == proposal.idempotency_key)
        {
            return Err(StoreError::Conflict("config proposal idempotency key"));
        }
        drop(state);
        self.pending_proposal_inserts.push(proposal);
        Ok(())
    }

    async fn proposal_for_update(&mut self, id: Uuid) -> Result<ConfigProposal, StoreError> {
        self.shared
            .ops
            .state
            .lock()
            .proposals
            .get(&id)
            .cloned()
            .ok_or(StoreError::NotFound("config proposal"))
    }

    async fn save_proposal(&mut self, proposal: &ConfigProposal) -> Result<(), StoreError> {
        self.pending_proposal_saves.push(proposal.clone());
        Ok(())
    }
}

#[async_trait]
impl OpsWriteSupport for FakeOpsConfigTx {
    async fn acquire_exclusive_fences(&mut self, _namespaces: &[String]) -> Result<(), StoreError> {
        // Documented divergence: the fake takes no real fence — exclusive/
        // shared blocking fidelity is proven by the Pg contracts.
        Ok(())
    }

    async fn market_times(&mut self, market: MarketId) -> Result<Option<MarketRow>, StoreError> {
        Ok(self.shared.state.lock().markets.get(&market.0).cloned())
    }

    async fn proposal_by_idempotency_key(
        &mut self,
        key: &str,
    ) -> Result<Option<ConfigProposal>, StoreError> {
        Ok(self
            .shared
            .ops
            .state
            .lock()
            .proposals
            .values()
            .find(|p| p.idempotency_key == key)
            .cloned())
    }

    async fn changes_in_generation(
        &mut self,
        generation: i64,
    ) -> Result<Vec<ConfigChange>, StoreError> {
        Ok(self
            .shared
            .ops
            .state
            .lock()
            .changes
            .iter()
            .filter(|(g, _)| *g == generation)
            .map(|(_, c)| c.clone())
            .collect())
    }
}

#[async_trait]
impl AuditWrite for FakeOpsConfigTx {
    async fn audit_insert(&mut self, action: AdminAction) -> Result<(), StoreError> {
        self.pending_audits.push(action);
        Ok(())
    }
}

#[async_trait]
impl OutboxWriter for FakeOpsConfigTx {
    async fn append(&mut self, e: Event) -> Result<(), StoreError> {
        self.pending_events.push(e);
        Ok(())
    }

    async fn append_batch(&mut self, events: &[Event]) -> Result<(), StoreError> {
        self.pending_events.extend(events.iter().cloned());
        Ok(())
    }
}

#[async_trait]
impl Committable for FakeOpsConfigTx {
    async fn commit(self: Box<Self>) -> Result<(), StoreError> {
        let me = *self;
        {
            let mut ops = me.shared.ops.state.lock();
            if let Some((generation, changes, _applied_by)) = &me.pending_apply {
                if *generation != ops.generation + 1 {
                    return Err(StoreError::Invariant(
                        "config generation must advance by exactly one",
                    ));
                }
                for change in changes {
                    ops.entries.insert(change.key.clone(), change.new.clone());
                    ops.changes.push((*generation, change.clone()));
                }
                ops.generation = *generation;
            }
            for proposal in me.pending_proposal_inserts {
                ops.proposals.insert(proposal.id, proposal);
            }
            for proposal in me.pending_proposal_saves {
                ops.proposals.insert(proposal.id, proposal);
            }
            ops.audits.extend(me.pending_audits);
        }
        if !me.pending_events.is_empty() {
            let mut state = me.shared.state.lock();
            state.outbox.extend(me.pending_events);
        }
        drop(me.generation_guard);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// D25 fence point on the shared in-memory transaction
// ---------------------------------------------------------------------------

#[async_trait]
impl FencePoint for InMemTx {
    async fn acquire_shared_fences(&mut self, _namespaces: &[String]) -> Result<(), StoreError> {
        // Documented divergence: no real lock; pause visibility is the
        // committed config state, which the fake reads authoritatively.
        Ok(())
    }

    async fn fence_config_value(&mut self, key: &str) -> Result<Option<Value>, StoreError> {
        Ok(self.shared.ops.state.lock().entries.get(key).cloned())
    }

    async fn fence_changes_since(
        &mut self,
        generation: i64,
    ) -> Result<(i64, Option<Vec<ConfigChange>>), StoreError> {
        let ops = self.shared.ops.state.lock();
        if generation + 1 < ops.watermark {
            return Ok((ops.generation, None)); // history pruned: conservative
        }
        let changes = ops
            .changes
            .iter()
            .filter(|(g, _)| *g > generation)
            .map(|(_, c)| c.clone())
            .collect();
        Ok((ops.generation, Some(changes)))
    }

    async fn request_fingerprint(&mut self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self.shared.ops.state.lock().fingerprints.get(key).cloned())
    }

    async fn save_request_fingerprint(
        &mut self,
        key: &str,
        fingerprint: &str,
    ) -> Result<(), StoreError> {
        self.shared
            .ops
            .state
            .lock()
            .fingerprints
            .insert(key.to_string(), fingerprint.to_string());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Test-support fixtures on the store (not ports)
// ---------------------------------------------------------------------------

impl InMemoryStore {
    /// Directly commits one config value (bypasses validation — a fixture,
    /// not a port), bumping the generation with its change row.
    pub fn set_config_value(&self, key: &str, value: Value) {
        let mut ops = self.shared.ops.state.lock();
        let old = ops.entries.get(key).cloned();
        let generation = ops.generation + 1;
        ops.entries.insert(key.to_string(), value.clone());
        ops.changes.push((
            generation,
            ConfigChange {
                key: key.to_string(),
                old,
                new: value,
            },
        ));
        ops.generation = generation;
    }

    #[must_use]
    pub fn config_value(&self, key: &str) -> Option<Value> {
        self.shared.ops.state.lock().entries.get(key).cloned()
    }

    #[must_use]
    pub fn config_generation(&self) -> i64 {
        self.shared.ops.state.lock().generation
    }

    /// Change rows of one generation, insertion-ordered.
    #[must_use]
    pub fn config_changes_of(&self, generation: i64) -> Vec<ConfigChange> {
        self.shared
            .ops
            .state
            .lock()
            .changes
            .iter()
            .filter(|(g, _)| *g == generation)
            .map(|(_, c)| c.clone())
            .collect()
    }

    /// Prunes history older than `watermark` (the D24 bounded-history knob).
    pub fn set_config_watermark(&self, watermark: i64) {
        let mut ops = self.shared.ops.state.lock();
        ops.watermark = watermark;
        ops.changes.retain(|(g, _)| *g >= watermark);
    }

    #[must_use]
    pub fn ops_audits(&self) -> Vec<AdminAction> {
        self.shared.ops.state.lock().audits.clone()
    }

    #[must_use]
    pub fn proposal(&self, id: Uuid) -> Option<ConfigProposal> {
        self.shared.ops.state.lock().proposals.get(&id).cloned()
    }

    #[must_use]
    pub fn request_fingerprint_of(&self, key: &str) -> Option<String> {
        self.shared.ops.state.lock().fingerprints.get(key).cloned()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use time::OffsetDateTime;

    use super::*;
    use crate::model::{AdminRole, ProposalStatus};
    use crate::ports::Store;

    fn proposal(key: &str) -> ConfigProposal {
        ConfigProposal {
            id: Uuid::new_v4(),
            idempotency_key: key.to_string(),
            patch: json!({"trade_fee_bps": 110}),
            patch_hash: "h".to_string(),
            base_generation: 1,
            proposer_token_id: "p".to_string(),
            proposer_role: AdminRole::Finance,
            reason: "r".to_string(),
            status: ProposalStatus::Pending,
            expires_at: OffsetDateTime::UNIX_EPOCH,
            confirmer_token_id: None,
            resulting_generation: None,
        }
    }

    #[tokio::test]
    async fn the_fake_seeds_the_0008_catalog_at_generation_one() {
        let store = InMemoryStore::new();
        assert_eq!(store.config_generation(), 1);
        assert_eq!(store.config_value("trade_fee_bps"), Some(json!(100)));
        let seed_rows = store.config_changes_of(1);
        assert_eq!(seed_rows.len(), seed_entries().len());
        assert!(seed_rows.iter().all(|c| c.old.is_none()));
    }

    #[tokio::test]
    async fn buffered_writes_land_atomically_at_commit_and_rollback_discards() {
        let store = InMemoryStore::new();
        let mut tx = store.ops_config_tx().await.unwrap();
        let base = tx.lock_generation().await.unwrap();
        let change = ConfigChange {
            key: "sweep_delay_secs".to_string(),
            old: Some(json!(180)),
            new: json!(300),
        };
        tx.apply_changes(base + 1, std::slice::from_ref(&change), "t")
            .await
            .unwrap();
        assert_eq!(store.config_generation(), 1, "nothing before commit");
        drop(tx); // rollback
        assert_eq!(store.config_generation(), 1);
        assert_eq!(store.config_value("sweep_delay_secs"), Some(json!(180)));

        let mut tx = store.ops_config_tx().await.unwrap();
        let base = tx.lock_generation().await.unwrap();
        tx.apply_changes(base + 1, &[change], "t").await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(store.config_generation(), 2);
        assert_eq!(store.config_value("sweep_delay_secs"), Some(json!(300)));
    }

    #[tokio::test]
    async fn writer_b_blocks_behind_writer_a_on_the_generation_lock() {
        let store = InMemoryStore::new();
        let mut a = store.ops_config_tx().await.unwrap();
        a.lock_generation().await.unwrap();
        let mut b = store.ops_config_tx().await.unwrap();
        let blocked =
            tokio::time::timeout(std::time::Duration::from_millis(50), b.lock_generation()).await;
        assert!(blocked.is_err(), "B must wait for A");
        drop(a);
        let generation =
            tokio::time::timeout(std::time::Duration::from_millis(200), b.lock_generation())
                .await
                .expect("A's rollback releases the lock")
                .unwrap();
        assert_eq!(generation, 1);
    }

    #[tokio::test]
    async fn apply_without_the_generation_lock_is_an_invariant_violation() {
        let store = InMemoryStore::new();
        let mut tx = store.ops_config_tx().await.unwrap();
        let err = tx
            .apply_changes(
                2,
                &[ConfigChange {
                    key: "sweep_delay_secs".to_string(),
                    old: None,
                    new: json!(300),
                }],
                "t",
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invariant(_)));
    }

    #[tokio::test]
    async fn proposal_rows_are_unique_by_idempotency_key() {
        let store = InMemoryStore::new();
        let mut tx = store.ops_config_tx().await.unwrap();
        tx.insert_proposal(proposal("k-1")).await.unwrap();
        let err = tx.insert_proposal(proposal("k-1")).await.unwrap_err();
        assert!(matches!(err, StoreError::Conflict(_)));
        tx.commit().await.unwrap();
        let mut tx = store.ops_config_tx().await.unwrap();
        let err = tx.insert_proposal(proposal("k-1")).await.unwrap_err();
        assert!(
            matches!(err, StoreError::Conflict(_)),
            "committed rows conflict too"
        );
        let found = tx.proposal_by_idempotency_key("k-1").await.unwrap();
        assert!(found.is_some());
        let missing = tx.proposal_for_update(Uuid::new_v4()).await.unwrap_err();
        assert!(matches!(missing, StoreError::NotFound(_)));
    }

    #[tokio::test]
    async fn fence_changes_since_honors_the_retention_watermark() {
        let store = InMemoryStore::new();
        store.set_config_value("sweep_delay_secs", json!(240)); // gen 2
        store.set_config_value("sweep_delay_secs", json!(300)); // gen 3
        let mut tx = store.trade_tx().await.unwrap();
        let (generation, changes) = tx.fence_changes_since(2).await.unwrap();
        assert_eq!(generation, 3);
        let changes = changes.unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].new, json!(300));
        // Prune history to generation 3: a generation-1 preview is now
        // beyond the watermark → conservative None.
        store.set_config_watermark(3);
        let (_, beyond) = tx.fence_changes_since(1).await.unwrap();
        assert!(beyond.is_none());
        let (_, at_edge) = tx.fence_changes_since(2).await.unwrap();
        assert!(at_edge.is_some(), "watermark−1 still has complete history");
    }

    #[tokio::test]
    async fn fingerprints_round_trip_through_the_fence_point() {
        let store = InMemoryStore::new();
        let mut tx = store.trade_tx().await.unwrap();
        assert_eq!(tx.request_fingerprint("k").await.unwrap(), None);
        tx.save_request_fingerprint("k", "fp-1").await.unwrap();
        assert_eq!(
            tx.request_fingerprint("k").await.unwrap(),
            Some("fp-1".to_string())
        );
        assert_eq!(store.request_fingerprint_of("k"), Some("fp-1".to_string()));
    }

    #[tokio::test]
    async fn fence_config_value_reads_committed_pause_state() {
        let store = InMemoryStore::new();
        let mut tx = store.trade_tx().await.unwrap();
        assert_eq!(
            tx.fence_config_value("trading_paused").await.unwrap(),
            Some(json!(false))
        );
        store.set_config_value("trading_paused", json!(true));
        assert_eq!(
            tx.fence_config_value("trading_paused").await.unwrap(),
            Some(json!(true)),
            "the fence read is authoritative committed state"
        );
    }

    #[tokio::test]
    async fn key_history_and_batch_outbox_helpers_preserve_order() {
        let store = InMemoryStore::new();
        store.set_config_value("sweep_delay_secs", json!(240));
        store.set_config_value("trade_fee_bps", json!(110));
        let mut tx = store.ops_config_tx().await.unwrap();
        let changes = tx
            .changes_for_key_since("sweep_delay_secs", 1)
            .await
            .unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].new, json!(240));
        let events = [
            Event {
                event_type: "one",
                aggregate_type: "config",
                aggregate_id: Uuid::nil(),
                payload: json!({}),
            },
            Event {
                event_type: "two",
                aggregate_type: "config",
                aggregate_id: Uuid::nil(),
                payload: json!({}),
            },
        ];
        tx.append_batch(&events).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            store
                .outbox()
                .into_iter()
                .map(|event| event.event_type)
                .collect::<Vec<_>>(),
            vec!["one", "two"]
        );
    }
}

#[cfg(test)]
mod typed_peek_tests {
    use super::*;

    #[test]
    fn typed_config_peeks_follow_committed_values_and_fail_closed() {
        let store = InMemoryStore::new();
        assert!(store.shared.ops.entry_bool("feature_comments"));
        assert!(!store.shared.ops.entry_bool("feature_referrals"));
        assert!(!store.shared.ops.entry_bool("missing_bool"));
        assert_eq!(store.shared.ops.entry_i64("trade_fee_bps"), Some(100));
        assert_eq!(store.shared.ops.entry_i64("feature_comments"), None);
        store.set_config_value("feature_comments", json!(7));
        assert!(!store.shared.ops.entry_bool("feature_comments"));
        assert_eq!(store.shared.ops.entry_i64("feature_comments"), Some(7));
    }
}
