//! KYC tier facts, validity horizon, and webhook application (D33).

use serde_json::{json, Value};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{AppError, StoreError};
use crate::model::UserId;
use crate::ports::ScreenVerdict;

use super::admin::{inbox_algebra, InboxOutcome, InboxRecord};
use super::{allows_progress, ComplianceStore, ComplianceTx};

/// Documented `users.kyc_tier` values.
pub const KYC_TIER_NONE: i32 = 0;
pub const KYC_TIER_BASIC: i32 = 1;
pub const KYC_TIER_FULL: i32 = 2;

/// Closed KYC tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct KycTier(pub i32);

/// Append-only KYC event (validity horizon lives in `valid_until`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KycEvent {
    pub id: Uuid,
    pub user: UserId,
    pub from_tier: Option<i32>,
    pub to_tier: i32,
    pub provider_ref: Option<String>,
    pub at: OffsetDateTime,
    pub valid_until: Option<OffsetDateTime>,
    pub policy_version: String,
    pub payload: Value,
}

/// Whether `have` satisfies `need` (0=none, 1=basic, 2=full).
#[must_use]
pub fn kyc_meets(have: i32, need: i32) -> bool {
    have >= need && (0..=2).contains(&have) && (0..=2).contains(&need)
}

/// Map a stored event to the D33 result algebra. Missing/expired horizon
/// is Indeterminate (fail closed). Revocation (`to_tier` 0 after a
/// positive `from_tier`) is Hit.
#[must_use]
pub fn kyc_verdict(event: &KycEvent, now: OffsetDateTime) -> ScreenVerdict {
    if event.to_tier == KYC_TIER_NONE && event.from_tier.unwrap_or(0) > KYC_TIER_NONE {
        return ScreenVerdict::Hit;
    }
    if event.to_tier < KYC_TIER_NONE || event.to_tier > KYC_TIER_FULL {
        return ScreenVerdict::Indeterminate;
    }
    let Some(expires_at) = event.valid_until else {
        return ScreenVerdict::Indeterminate;
    };
    if expires_at <= now {
        return ScreenVerdict::Indeterminate;
    }
    ScreenVerdict::Clear {
        checked_at: event.at,
        expires_at,
        policy_version: event.policy_version.clone(),
    }
}

/// Build the next append-only event. Downgrade/revocation is representable.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn apply_kyc_event(
    user: UserId,
    from_tier: Option<i32>,
    to_tier: i32,
    provider_ref: Option<String>,
    at: OffsetDateTime,
    valid_until: Option<OffsetDateTime>,
    policy_version: String,
    payload: Value,
) -> KycEvent {
    KycEvent {
        id: Uuid::new_v4(),
        user,
        from_tier,
        to_tier,
        provider_ref,
        at,
        valid_until,
        policy_version,
        payload,
    }
}

/// Persist a KYC event and update `users.kyc_tier`. Machine path: decision
/// fact only. Manual path also writes `admin_actions` at the call-site.
///
/// # Errors
/// Store failures.
pub async fn persist_kyc_event(
    tx: &mut dyn ComplianceTx,
    event: KycEvent,
) -> Result<KycEvent, StoreError> {
    tx.insert_kyc_event(event.clone()).await?;
    tx.set_kyc_tier(event.user, event.to_tier).await?;
    Ok(event)
}

/// Latest event as a verdict, or Indeterminate when none exists.
///
/// # Errors
/// Store failures.
pub async fn current_kyc_verdict(
    tx: &mut dyn ComplianceTx,
    user: UserId,
    now: OffsetDateTime,
) -> Result<ScreenVerdict, StoreError> {
    Ok(tx
        .latest_kyc(user)
        .await?
        .map_or(ScreenVerdict::Indeterminate, |event| {
            kyc_verdict(&event, now)
        }))
}

/// Apply an already-authenticated inbox payload that names **our** user.
///
/// # Errors
/// Store failures; unknown our-user.
#[allow(clippy::too_many_arguments)]
pub async fn apply_inboxed_kyc(
    store: &impl ComplianceStore,
    user: UserId,
    to_tier: i32,
    provider_ref: Option<String>,
    valid_until: Option<OffsetDateTime>,
    policy_version: String,
    payload: Value,
    now: OffsetDateTime,
) -> Result<KycEvent, StoreError> {
    let mut tx = store.compliance_tx().await?;
    let row = tx.lock_user(user).await?;
    let event = apply_kyc_event(
        user,
        Some(row.kyc_tier),
        to_tier,
        provider_ref,
        now,
        valid_until,
        policy_version,
        payload,
    );
    let event = persist_kyc_event(tx.as_mut(), event).await?;
    tx.insert_decision(super::ComplianceDecision {
        id: Uuid::new_v4(),
        subject_type: "user".into(),
        subject_id: user.0,
        kind: "kyc_event".into(),
        actor: "machine".into(),
        at: now,
        payload: json!({ "to_tier": event.to_tier }),
    })
    .await?;
    tx.commit().await?;
    Ok(event)
}

/// Accept one authenticated provider delivery AND apply its KYC effect.
///
/// The inbox row, the KYC event, the tier and the compliance decision are one
/// unit of work, so a failure anywhere leaves no acceptance behind and the
/// provider's retry is a fresh `Accepted`. Splitting acceptance and effect
/// across two commits strands the inbox row on any failure after the first
/// commit, and every retry then answers `Replay` with the effect never applied
/// (codex-p7r1:73-75; docs/reviews/p7r1-resolution.md:13,24).
///
/// Same key + same payload hash is `Replay` and writes nothing — in
/// particular it never appends a second KYC event. Same key + different hash
/// is the typed conflict the caller pages on.
///
/// # Errors
/// Unresolved user, payload-hash conflict, or store failures.
pub async fn accept_and_apply_inboxed_kyc(
    store: &impl ComplianceStore,
    record: InboxRecord,
    to_tier: i32,
    provider_ref: Option<String>,
    valid_until: Option<OffsetDateTime>,
    policy_version: String,
    now: OffsetDateTime,
) -> Result<InboxOutcome, AppError> {
    let user = record
        .user_id
        .ok_or(AppError::Store(StoreError::NotFound("provider mapping")))?;
    let mut tx = store.compliance_tx().await?;
    // codex B3: serialize the inbox key BEFORE the read-or-create. Without it
    // two concurrent deliveries of one key both read absent and the loser
    // raises a raw unique violation instead of the D33 answer — and because a
    // conflicting payload can name a different `provider_ref`, and therefore a
    // different user, the class-2 user lock below does not order them.
    tx.serialize_inbox(&record.provider, &record.event_id)
        .await?;
    let existing = tx.inbox_get(&record.provider, &record.event_id).await?;
    match inbox_algebra(existing.as_ref(), &record) {
        InboxOutcome::Replay => {
            tx.commit().await?;
            return Ok(InboxOutcome::Replay);
        }
        InboxOutcome::Conflict => {
            tx.commit().await?;
            return Err(AppError::ProposalConflict("inbox payload hash conflict"));
        }
        InboxOutcome::Accepted => {}
    }
    // One unit of work from here: user lock (class-2 order first), the
    // acceptance, and the effect. Any failure below rolls the acceptance back
    // with it, so the provider's retry is Accepted rather than a Replay over
    // an effect that never happened.
    let payload = record.payload.clone();
    let row = tx.lock_user(user).await?;
    tx.inbox_insert(record).await?;
    let event = apply_kyc_event(
        user,
        Some(row.kyc_tier),
        to_tier,
        provider_ref,
        now,
        valid_until,
        policy_version,
        payload,
    );
    let event = persist_kyc_event(tx.as_mut(), event).await?;
    tx.insert_decision(super::ComplianceDecision {
        id: Uuid::new_v4(),
        subject_type: "user".into(),
        subject_id: user.0,
        kind: "kyc_event".into(),
        actor: "machine".into(),
        at: now,
        payload: json!({ "to_tier": event.to_tier }),
    })
    .await?;
    tx.commit().await?;
    Ok(InboxOutcome::Accepted)
}

/// Staging sandbox completion: force full KYC for a known user.
///
/// # Errors
/// Store failures.
pub async fn sandbox_complete_full(
    store: &impl ComplianceStore,
    user: UserId,
    now: OffsetDateTime,
    horizon: time::Duration,
    policy_version: String,
) -> Result<KycEvent, StoreError> {
    apply_inboxed_kyc(
        store,
        user,
        KYC_TIER_FULL,
        Some("sandbox".into()),
        Some(now + horizon),
        policy_version,
        json!({ "source": "sandbox_complete" }),
        now,
    )
    .await
}

/// True when a fresh Clear at `required` tier would progress.
#[must_use]
pub fn kyc_progresses(verdict: &ScreenVerdict, have: i32, need: i32, now: OffsetDateTime) -> bool {
    allows_progress(verdict, now) && kyc_meets(have, need)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::fakes::FakeComplianceStore;
    use crate::money::{
        AmlDirection, AmlFlag, AmlKind, AmlLeg, ComplianceDecision, SanctionScreening,
        UserComplianceRow,
    };
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use time::Duration;

    fn t0() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
    }

    /// A `ComplianceTx` that really is a transaction: writes land in a staged
    /// copy and become visible only on `commit`. The shared
    /// `FakeComplianceStore` commits nothing and rolls back nothing, so it
    /// cannot express the property under test — that a failed effect leaves no
    /// acceptance behind.
    #[derive(Default, Clone)]
    struct TxState {
        inbox: Vec<InboxRecord>,
        events: Vec<KycEvent>,
        tiers: HashMap<Uuid, i32>,
        decisions: Vec<ComplianceDecision>,
    }

    #[derive(Default, Clone)]
    struct AtomicStore {
        committed: Arc<Mutex<TxState>>,
        /// Injects the failure exactly where the shipped route committed:
        /// after the inbox row is written, before the effect is durable.
        fail_after_inbox_insert: Arc<AtomicBool>,
    }

    impl AtomicStore {
        fn seed_user(&self, user: UserId, tier: i32) {
            self.state().tiers.insert(user.0, tier);
        }
        fn state(&self) -> std::sync::MutexGuard<'_, TxState> {
            self.committed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
        fn committed_inbox(&self) -> usize {
            self.state().inbox.len()
        }
        fn committed_events(&self) -> usize {
            self.state().events.len()
        }
        fn committed_decisions(&self) -> usize {
            self.state().decisions.len()
        }
        fn tier_of(&self, user: UserId) -> Option<i32> {
            self.state().tiers.get(&user.0).copied()
        }
    }

    struct AtomicTx {
        shared: Arc<Mutex<TxState>>,
        staged: TxState,
        fail_after_inbox_insert: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl crate::ports::Committable for AtomicTx {
        async fn commit(self: Box<Self>) -> Result<(), StoreError> {
            *self
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = self.staged;
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl ComplianceStore for AtomicStore {
        async fn compliance_tx(&self) -> Result<Box<dyn ComplianceTx + '_>, StoreError> {
            let staged = self.state().clone();
            Ok(Box::new(AtomicTx {
                shared: Arc::clone(&self.committed),
                staged,
                fail_after_inbox_insert: Arc::clone(&self.fail_after_inbox_insert),
            }))
        }
    }

    #[async_trait::async_trait]
    impl ComplianceTx for AtomicTx {
        async fn lock_user(&mut self, user: UserId) -> Result<UserComplianceRow, StoreError> {
            Ok(UserComplianceRow {
                user,
                kyc_tier: self.staged.tiers.get(&user.0).copied().unwrap_or(0),
                status: "active".into(),
            })
        }
        async fn insert_kyc_event(&mut self, event: KycEvent) -> Result<(), StoreError> {
            if self.fail_after_inbox_insert.load(Ordering::SeqCst) {
                return Err(StoreError::Unavailable("kyc write failed"));
            }
            self.staged.events.push(event);
            Ok(())
        }
        async fn set_kyc_tier(&mut self, user: UserId, tier: i32) -> Result<(), StoreError> {
            self.staged.tiers.insert(user.0, tier);
            Ok(())
        }
        async fn latest_kyc(&mut self, user: UserId) -> Result<Option<KycEvent>, StoreError> {
            Ok(self
                .staged
                .events
                .iter()
                .rfind(|event| event.user == user)
                .cloned())
        }
        async fn insert_screening(
            &mut self,
            _screening: crate::money::sanctions::SanctionScreening,
        ) -> Result<(), StoreError> {
            Ok(())
        }
        async fn latest_screening(
            &mut self,
            _user: UserId,
            _context: &str,
        ) -> Result<Option<crate::money::sanctions::SanctionScreening>, StoreError> {
            Ok(None)
        }
        async fn insert_aml_flag(
            &mut self,
            _flag: crate::money::aml::AmlFlag,
        ) -> Result<(), StoreError> {
            Ok(())
        }
        async fn open_aml_flags(
            &mut self,
            _user: UserId,
        ) -> Result<Vec<crate::money::aml::AmlFlag>, StoreError> {
            Ok(Vec::new())
        }
        async fn list_aml_legs(
            &mut self,
            _user: UserId,
            _since: OffsetDateTime,
        ) -> Result<Vec<crate::money::aml::AmlLeg>, StoreError> {
            Ok(Vec::new())
        }
        async fn list_dest_aml_legs(
            &mut self,
            _dest: &str,
            _since: OffsetDateTime,
        ) -> Result<Vec<crate::money::aml::AmlLeg>, StoreError> {
            Ok(Vec::new())
        }
        async fn record_aml_leg(
            &mut self,
            _leg: crate::money::aml::AmlLeg,
        ) -> Result<(), StoreError> {
            Ok(())
        }
        async fn insert_decision(
            &mut self,
            decision: ComplianceDecision,
        ) -> Result<(), StoreError> {
            self.staged.decisions.push(decision);
            Ok(())
        }
        async fn serialize_inbox(
            &mut self,
            _provider: &str,
            _event_id: &str,
        ) -> Result<(), StoreError> {
            // This double drives single-threaded atomicity cases only; the
            // blocking contract is proven against FakeComplianceStore in
            // fakes/compliance.rs and against Postgres in the adapter race.
            Ok(())
        }
        async fn inbox_get(
            &mut self,
            provider: &str,
            event_id: &str,
        ) -> Result<Option<InboxRecord>, StoreError> {
            Ok(self
                .staged
                .inbox
                .iter()
                .find(|row| row.provider == provider && row.event_id == event_id)
                .cloned())
        }
        async fn inbox_insert(&mut self, record: InboxRecord) -> Result<InboxRecord, StoreError> {
            if self
                .staged
                .inbox
                .iter()
                .any(|row| row.provider == record.provider && row.event_id == record.event_id)
            {
                return Err(StoreError::Conflict("inbox event"));
            }
            self.staged.inbox.push(record.clone());
            Ok(record)
        }
    }

    fn delivery(user: UserId, hash: &str) -> InboxRecord {
        InboxRecord {
            provider: "kyc".into(),
            event_id: "evt-1".into(),
            payload_hash: hash.into(),
            payload: json!({ "provider_ref": "sess-1", "to_tier": 2 }),
            user_id: Some(user),
            received_at: t0(),
        }
    }

    #[tokio::test]
    async fn atomic_kyc_double_implements_the_unused_compliance_read_surfaces() {
        let store = AtomicStore::default();
        let user = UserId(Uuid::from_u128(1));
        store.seed_user(user, 0);
        let mut tx = store.compliance_tx().await.unwrap();

        assert_eq!(tx.latest_kyc(user).await.unwrap(), None);
        let event = apply_kyc_event(
            user,
            Some(0),
            1,
            Some("session".into()),
            t0(),
            Some(t0() + Duration::days(1)),
            "kyc-coverage".into(),
            Value::Null,
        );
        tx.insert_kyc_event(event.clone()).await.unwrap();
        assert_eq!(tx.latest_kyc(user).await.unwrap(), Some(event));

        tx.insert_screening(SanctionScreening {
            id: Uuid::from_u128(2),
            user,
            context: "deposit".into(),
            verdict: ScreenVerdict::Indeterminate,
            raw_ref: None,
        })
        .await
        .unwrap();
        assert_eq!(tx.latest_screening(user, "deposit").await.unwrap(), None);

        let flag = AmlFlag {
            id: Uuid::from_u128(3),
            user,
            rule: AmlKind::Structuring,
            window_label: "24h".into(),
            evidence: Value::Null,
            open: true,
            at: t0(),
        };
        tx.insert_aml_flag(flag).await.unwrap();
        assert!(tx.open_aml_flags(user).await.unwrap().is_empty());
        assert!(tx
            .list_aml_legs(user, OffsetDateTime::UNIX_EPOCH)
            .await
            .unwrap()
            .is_empty());
        assert!(tx
            .list_dest_aml_legs("source", OffsetDateTime::UNIX_EPOCH)
            .await
            .unwrap()
            .is_empty());
        tx.record_aml_leg(AmlLeg {
            id: Uuid::from_u128(4),
            user,
            dest: "source".into(),
            amount_micro: 100_000_000,
            at: t0(),
            direction: AmlDirection::Deposit,
        })
        .await
        .unwrap();

        let inbox = delivery(user, "coverage-hash");
        tx.inbox_insert(inbox.clone()).await.unwrap();
        assert_eq!(
            tx.inbox_insert(inbox).await,
            Err(StoreError::Conflict("inbox event"))
        );
    }

    #[tokio::test]
    async fn a_failed_kyc_effect_leaves_no_acceptance_so_the_retry_still_applies() {
        let store = AtomicStore::default();
        let user = UserId(Uuid::new_v4());
        store.seed_user(user, 0);
        store.fail_after_inbox_insert.store(true, Ordering::SeqCst);

        let failed = accept_and_apply_inboxed_kyc(
            &store,
            delivery(user, "hash-a"),
            2,
            Some("sess-1".into()),
            Some(t0() + Duration::days(30)),
            "kyc-1".into(),
            t0(),
        )
        .await;
        assert!(failed.is_err(), "the effect failure must surface");
        assert_eq!(
            store.committed_inbox(),
            0,
            "an acceptance must not survive a failed effect: the stranded row is what makes every retry a Replay"
        );
        assert_eq!(store.tier_of(user), Some(0));

        store.fail_after_inbox_insert.store(false, Ordering::SeqCst);
        let retry = accept_and_apply_inboxed_kyc(
            &store,
            delivery(user, "hash-a"),
            2,
            Some("sess-1".into()),
            Some(t0() + Duration::days(30)),
            "kyc-1".into(),
            t0(),
        )
        .await
        .unwrap();
        assert_eq!(
            retry,
            InboxOutcome::Accepted,
            "the retry must be able to apply the effect"
        );
        assert_eq!(store.tier_of(user), Some(2));
        assert_eq!(store.committed_inbox(), 1);
        assert_eq!(store.committed_events(), 1);
        assert_eq!(store.committed_decisions(), 1);
    }

    #[tokio::test]
    async fn a_replayed_delivery_applies_nothing_twice() {
        let store = AtomicStore::default();
        let user = UserId(Uuid::new_v4());
        store.seed_user(user, 0);
        let first = accept_and_apply_inboxed_kyc(
            &store,
            delivery(user, "hash-a"),
            2,
            Some("sess-1".into()),
            None,
            "kyc-1".into(),
            t0(),
        )
        .await
        .unwrap();
        assert_eq!(first, InboxOutcome::Accepted);
        let replay = accept_and_apply_inboxed_kyc(
            &store,
            delivery(user, "hash-a"),
            2,
            Some("sess-1".into()),
            None,
            "kyc-1".into(),
            t0(),
        )
        .await
        .unwrap();
        assert_eq!(replay, InboxOutcome::Replay);
        assert_eq!(
            store.committed_events(),
            1,
            "a same-key same-hash replay must not append a second KYC event"
        );
        assert_eq!(store.committed_inbox(), 1);
        assert_eq!(store.committed_decisions(), 1);
        assert_eq!(store.tier_of(user), Some(2));
    }

    #[tokio::test]
    async fn a_same_key_different_hash_delivery_is_a_typed_conflict_that_applies_nothing() {
        let store = AtomicStore::default();
        let user = UserId(Uuid::new_v4());
        store.seed_user(user, 0);
        accept_and_apply_inboxed_kyc(
            &store,
            delivery(user, "hash-a"),
            2,
            Some("sess-1".into()),
            None,
            "kyc-1".into(),
            t0(),
        )
        .await
        .unwrap();
        let conflict = accept_and_apply_inboxed_kyc(
            &store,
            delivery(user, "hash-b"),
            2,
            Some("sess-1".into()),
            None,
            "kyc-1".into(),
            t0(),
        )
        .await;
        assert!(matches!(
            conflict,
            Err(AppError::ProposalConflict("inbox payload hash conflict"))
        ));
        assert_eq!(store.committed_events(), 1);
        assert_eq!(store.committed_inbox(), 1);
    }

    #[tokio::test]
    async fn an_unresolved_user_is_refused_before_any_write() {
        let store = AtomicStore::default();
        let mut record = delivery(UserId(Uuid::new_v4()), "hash-a");
        record.user_id = None;
        let refused =
            accept_and_apply_inboxed_kyc(&store, record, 2, None, None, "kyc-1".into(), t0()).await;
        assert!(matches!(
            refused,
            Err(AppError::Store(StoreError::NotFound("provider mapping")))
        ));
        assert_eq!(store.committed_inbox(), 0);
    }

    #[test]
    fn tiers_and_meets() {
        assert!(kyc_meets(KYC_TIER_FULL, KYC_TIER_BASIC));
        assert!(kyc_meets(1, 1));
        assert!(!kyc_meets(0, 1));
        assert!(!kyc_meets(3, 1));
        assert!(!kyc_meets(1, 3));
        assert!(kyc_meets(0, 0));
        assert_eq!(KycTier(1).0, KYC_TIER_BASIC);
        assert!(KycTier(0) < KycTier(2));
    }

    #[test]
    fn verdict_covers_clear_hit_indeterminate() {
        let now = t0();
        let clear = KycEvent {
            id: Uuid::nil(),
            user: UserId(Uuid::nil()),
            from_tier: Some(0),
            to_tier: 2,
            provider_ref: Some("p".into()),
            at: now,
            valid_until: Some(now + Duration::days(30)),
            policy_version: "kyc-1".into(),
            payload: Value::Null,
        };
        assert!(matches!(
            kyc_verdict(&clear, now),
            ScreenVerdict::Clear {
                policy_version, ..
            } if policy_version == "kyc-1"
        ));
        let mut expired = clear.clone();
        expired.valid_until = Some(now);
        assert_eq!(kyc_verdict(&expired, now), ScreenVerdict::Indeterminate);
        let mut no_horizon = clear.clone();
        no_horizon.valid_until = None;
        assert_eq!(kyc_verdict(&no_horizon, now), ScreenVerdict::Indeterminate);
        let revoke = apply_kyc_event(
            UserId(Uuid::nil()),
            Some(2),
            0,
            None,
            now,
            Some(now + Duration::days(1)),
            "kyc-1".into(),
            json!({}),
        );
        assert_eq!(kyc_verdict(&revoke, now), ScreenVerdict::Hit);
        let mut bad = clear;
        bad.to_tier = 9;
        assert_eq!(kyc_verdict(&bad, now), ScreenVerdict::Indeterminate);
    }

    #[test]
    fn progresses_requires_fresh_clear_and_tier() {
        let now = t0();
        let clear = ScreenVerdict::Clear {
            checked_at: now,
            expires_at: now + Duration::hours(1),
            policy_version: "1".into(),
        };
        assert!(kyc_progresses(&clear, 2, 1, now));
        assert!(!kyc_progresses(&clear, 0, 1, now));
        assert!(!kyc_progresses(&ScreenVerdict::Hit, 2, 1, now));
    }

    #[tokio::test]
    async fn persist_and_sandbox_complete_round_trip() {
        let store = FakeComplianceStore::new();
        let user = store.add_user("kyc-a");
        let now = t0();
        let event = sandbox_complete_full(&store, user, now, Duration::days(30), "kyc-1".into())
            .await
            .unwrap();
        assert_eq!(event.to_tier, KYC_TIER_FULL);
        let mut tx = store.compliance_tx().await.unwrap();
        assert_eq!(tx.lock_user(user).await.unwrap().kyc_tier, KYC_TIER_FULL);
        let verdict = current_kyc_verdict(tx.as_mut(), user, now).await.unwrap();
        assert!(allows_progress(&verdict, now));
        let missing = current_kyc_verdict(tx.as_mut(), UserId(Uuid::new_v4()), now)
            .await
            .unwrap();
        assert_eq!(missing, ScreenVerdict::Indeterminate);
        tx.commit().await.unwrap();
    }
}
