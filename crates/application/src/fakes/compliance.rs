//! In-memory W2 compliance store: facts, admin commands, phone, inbox.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use time::OffsetDateTime;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use uuid::Uuid;

use crate::error::StoreError;
use crate::model::{AdminAction, UserId, UserStatus};
use crate::money::admin::{
    ComplianceAdminStore, ComplianceAdminTx, FrozenFundsLicense, InboxRecord, MoneyProposal,
    ProposalStatus,
};
use crate::money::aml::{AmlFlag, AmlLeg};
use crate::money::kyc::KycEvent;
use crate::money::phone_verification::{PhoneVerificationRow, PHONE_MAX_ATTEMPTS};
use crate::money::sanctions::SanctionScreening;
use crate::money::self_exclusion::{SelfExclusion, UserDepositLimit};
use crate::money::statuses::status_name;
use crate::money::{ComplianceDecision, ComplianceStore, ComplianceTx, UserComplianceRow};
use crate::ports::{Committable, PhoneVerification};

#[derive(Default)]
struct Inner {
    users: HashMap<Uuid, UserComplianceRow>,
    kyc: HashMap<Uuid, Vec<KycEvent>>,
    screens: Vec<SanctionScreening>,
    flags: Vec<AmlFlag>,
    legs: Vec<AmlLeg>,
    decisions: Vec<ComplianceDecision>,
    exclusions: Vec<SelfExclusion>,
    limits: HashMap<Uuid, UserDepositLimit>,
    phones: Vec<PhoneVerificationRow>,
    inbox: Vec<InboxRecord>,
    proposals: Vec<MoneyProposal>,
    audits: Vec<AdminAction>,
    settled: HashMap<Uuid, Vec<String>>,
    config_i64: HashMap<String, i64>,
    config_json: HashMap<String, serde_json::Value>,
    licenses: Vec<FrozenFundsLicense>,
}

#[derive(Clone, Default)]
pub struct FakeComplianceStore {
    inner: Arc<Mutex<Inner>>,
    inbox_locks: Arc<KeyLocks>,
}

impl FakeComplianceStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Same as [`ComplianceStore::compliance_tx`] without the trait in scope.
    ///
    /// # Errors
    /// Never — the in-memory transaction always opens.
    pub async fn compliance_tx(&self) -> Result<Box<dyn ComplianceTx + '_>, StoreError> {
        ComplianceStore::compliance_tx(self).await
    }

    #[must_use]
    pub fn add_user(&self, handle: &str) -> UserId {
        let user = UserId(Uuid::new_v4());
        let _ = handle;
        self.inner.lock().users.insert(
            user.0,
            UserComplianceRow {
                user,
                kyc_tier: 0,
                status: "active".into(),
            },
        );
        user
    }
}

/// One named async lock per key: the fake's stand-in for a class-1 advisory
/// lock, mirroring `LockMap` in `fakes/state.rs` (private there, so this is a
/// local copy rather than a cross-owner refactor). A no-op would let the fake
/// keep masking the inbox race it exists to model.
#[derive(Default)]
struct KeyLocks {
    locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

impl KeyLocks {
    fn handle(&self, key: &str) -> Arc<AsyncMutex<()>> {
        Arc::clone(
            self.locks
                .lock()
                .entry(key.to_owned())
                .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
        )
    }
}

struct FakeComplianceTx {
    inner: Arc<Mutex<Inner>>,
    inbox_locks: Arc<KeyLocks>,
    /// Held for the life of the transaction: dropping the tx — by commit or by
    /// rollback — releases the key, exactly like `pg_advisory_xact_lock`.
    inbox_guards: HashMap<String, OwnedMutexGuard<()>>,
}

#[async_trait]
impl ComplianceStore for FakeComplianceStore {
    async fn compliance_tx(&self) -> Result<Box<dyn ComplianceTx + '_>, StoreError> {
        Ok(Box::new(FakeComplianceTx {
            inner: Arc::clone(&self.inner),
            inbox_locks: Arc::clone(&self.inbox_locks),
            inbox_guards: HashMap::new(),
        }))
    }
}

#[async_trait]
impl ComplianceAdminStore for FakeComplianceStore {
    async fn admin_tx(&self) -> Result<Box<dyn ComplianceAdminTx + '_>, StoreError> {
        Ok(Box::new(FakeComplianceTx {
            inner: Arc::clone(&self.inner),
            inbox_locks: Arc::clone(&self.inbox_locks),
            inbox_guards: HashMap::new(),
        }))
    }
}

#[async_trait]
impl ComplianceTx for FakeComplianceTx {
    async fn lock_user(&mut self, user: UserId) -> Result<UserComplianceRow, StoreError> {
        self.inner
            .lock()
            .users
            .get(&user.0)
            .cloned()
            .ok_or(StoreError::NotFound("user"))
    }

    async fn insert_kyc_event(&mut self, event: KycEvent) -> Result<(), StoreError> {
        self.inner
            .lock()
            .kyc
            .entry(event.user.0)
            .or_default()
            .push(event);
        Ok(())
    }

    async fn set_kyc_tier(&mut self, user: UserId, tier: i32) -> Result<(), StoreError> {
        let mut inner = self.inner.lock();
        let row = inner.users.entry(user.0).or_insert(UserComplianceRow {
            user,
            kyc_tier: 0,
            status: "active".into(),
        });
        row.kyc_tier = tier;
        Ok(())
    }

    async fn latest_kyc(&mut self, user: UserId) -> Result<Option<KycEvent>, StoreError> {
        Ok(self
            .inner
            .lock()
            .kyc
            .get(&user.0)
            .and_then(|rows| rows.last())
            .cloned())
    }

    async fn insert_screening(&mut self, screening: SanctionScreening) -> Result<(), StoreError> {
        self.inner.lock().screens.push(screening);
        Ok(())
    }

    async fn latest_screening(
        &mut self,
        user: UserId,
        context: &str,
    ) -> Result<Option<SanctionScreening>, StoreError> {
        Ok(self
            .inner
            .lock()
            .screens
            .iter()
            .rev()
            .find(|row| row.user == user && row.context == context)
            .cloned())
    }

    async fn insert_aml_flag(&mut self, flag: AmlFlag) -> Result<(), StoreError> {
        self.inner.lock().flags.push(flag);
        Ok(())
    }

    async fn open_aml_flags(&mut self, user: UserId) -> Result<Vec<AmlFlag>, StoreError> {
        Ok(self
            .inner
            .lock()
            .flags
            .iter()
            .filter(|flag| flag.user == user && flag.open)
            .cloned()
            .collect())
    }

    async fn list_aml_legs(
        &mut self,
        user: UserId,
        since: OffsetDateTime,
    ) -> Result<Vec<AmlLeg>, StoreError> {
        Ok(self
            .inner
            .lock()
            .legs
            .iter()
            .filter(|leg| leg.user == user && leg.at >= since)
            .cloned()
            .collect())
    }

    async fn list_dest_aml_legs(
        &mut self,
        dest: &str,
        since: OffsetDateTime,
    ) -> Result<Vec<AmlLeg>, StoreError> {
        Ok(self
            .inner
            .lock()
            .legs
            .iter()
            .filter(|leg| leg.dest == dest && leg.at >= since)
            .cloned()
            .collect())
    }

    async fn record_aml_leg(&mut self, leg: AmlLeg) -> Result<(), StoreError> {
        self.inner.lock().legs.push(leg);
        Ok(())
    }

    async fn insert_decision(&mut self, decision: ComplianceDecision) -> Result<(), StoreError> {
        self.inner.lock().decisions.push(decision);
        Ok(())
    }

    /// Real keyed lock, not a no-op: the fake must be able to make a second
    /// transaction WAIT on the same inbox key, or it silently re-hides the
    /// race it is supposed to model. Mirrors the Pg class-1 advisory lock —
    /// the guard is held until this transaction commits or is dropped.
    async fn serialize_inbox(&mut self, provider: &str, event_id: &str) -> Result<(), StoreError> {
        let key = format!("kyc-inbox:{provider}:{event_id}");
        if !self.inbox_guards.contains_key(&key) {
            let guard = self.inbox_locks.handle(&key).lock_owned().await;
            self.inbox_guards.insert(key, guard);
        }
        Ok(())
    }

    /// Same rows as the `ComplianceAdminTx` methods of the same name: one
    /// concrete transaction, one inbox, so the effect can be committed with
    /// the acceptance.
    async fn inbox_get(
        &mut self,
        provider: &str,
        event_id: &str,
    ) -> Result<Option<InboxRecord>, StoreError> {
        Ok(self
            .inner
            .lock()
            .inbox
            .iter()
            .find(|row| row.provider == provider && row.event_id == event_id)
            .cloned())
    }

    async fn inbox_insert(&mut self, record: InboxRecord) -> Result<InboxRecord, StoreError> {
        let mut inner = self.inner.lock();
        if inner
            .inbox
            .iter()
            .any(|row| row.provider == record.provider && row.event_id == record.event_id)
        {
            return Err(StoreError::Conflict("inbox event"));
        }
        inner.inbox.push(record.clone());
        Ok(record)
    }
}

#[async_trait]
impl ComplianceAdminTx for FakeComplianceTx {
    async fn get_aml_flag(&mut self, id: Uuid) -> Result<AmlFlag, StoreError> {
        self.inner
            .lock()
            .flags
            .iter()
            .find(|flag| flag.id == id)
            .cloned()
            .ok_or(StoreError::NotFound("aml flag"))
    }

    async fn clear_aml_flag(&mut self, id: Uuid) -> Result<AmlFlag, StoreError> {
        let mut inner = self.inner.lock();
        let flag = inner
            .flags
            .iter_mut()
            .find(|flag| flag.id == id)
            .ok_or(StoreError::NotFound("aml flag"))?;
        flag.open = false;
        Ok(flag.clone())
    }

    async fn set_user_status(
        &mut self,
        user: UserId,
        status: UserStatus,
    ) -> Result<UserStatus, StoreError> {
        let mut inner = self.inner.lock();
        let row = inner
            .users
            .get_mut(&user.0)
            .ok_or(StoreError::NotFound("user"))?;
        row.status = status_name(status).into();
        Ok(status)
    }

    async fn insert_self_exclusion(
        &mut self,
        exclusion: SelfExclusion,
    ) -> Result<SelfExclusion, StoreError> {
        self.inner.lock().exclusions.push(exclusion.clone());
        Ok(exclusion)
    }

    async fn active_self_exclusion(
        &mut self,
        user: UserId,
    ) -> Result<Option<SelfExclusion>, StoreError> {
        Ok(self
            .inner
            .lock()
            .exclusions
            .iter()
            .rev()
            .find(|row| row.user == user && row.lifted_at.is_none())
            .cloned())
    }

    async fn get_self_exclusion(&mut self, id: Uuid) -> Result<SelfExclusion, StoreError> {
        self.inner
            .lock()
            .exclusions
            .iter()
            .find(|row| row.id == id)
            .cloned()
            .ok_or(StoreError::NotFound("self exclusion"))
    }

    async fn lift_self_exclusion(
        &mut self,
        id: Uuid,
        at: OffsetDateTime,
    ) -> Result<SelfExclusion, StoreError> {
        let mut inner = self.inner.lock();
        let row = inner
            .exclusions
            .iter_mut()
            .find(|row| row.id == id)
            .ok_or(StoreError::NotFound("self exclusion"))?;
        row.lifted_at = Some(at);
        Ok(row.clone())
    }

    async fn get_deposit_limit(
        &mut self,
        user: UserId,
    ) -> Result<Option<UserDepositLimit>, StoreError> {
        Ok(self.inner.lock().limits.get(&user.0).cloned())
    }

    async fn upsert_deposit_limit(&mut self, limit: UserDepositLimit) -> Result<(), StoreError> {
        self.inner.lock().limits.insert(limit.user.0, limit);
        Ok(())
    }

    async fn insert_phone_challenge(
        &mut self,
        row: PhoneVerificationRow,
    ) -> Result<PhoneVerificationRow, StoreError> {
        let mut inner = self.inner.lock();
        if inner.phones.iter().any(|existing| {
            existing.number_hmac == row.number_hmac
                && existing.hmac_key_version == row.hmac_key_version
        }) {
            return Err(StoreError::Conflict("phone number already bound"));
        }
        inner.phones.push(row.clone());
        Ok(row)
    }

    async fn active_phone_challenge(
        &mut self,
        user: UserId,
    ) -> Result<Option<PhoneVerificationRow>, StoreError> {
        Ok(self
            .inner
            .lock()
            .phones
            .iter()
            .rev()
            .find(|row| row.user == user)
            .cloned())
    }

    async fn phone_by_hmac(
        &mut self,
        hmac: &str,
        key_version: i32,
    ) -> Result<Option<PhoneVerificationRow>, StoreError> {
        Ok(self
            .inner
            .lock()
            .phones
            .iter()
            .find(|row| row.number_hmac == hmac && row.hmac_key_version == key_version)
            .cloned())
    }

    async fn refresh_phone_challenge(
        &mut self,
        id: Uuid,
        challenge: &str,
        expires_at: OffsetDateTime,
    ) -> Result<PhoneVerificationRow, StoreError> {
        let mut inner = self.inner.lock();
        let row = inner
            .phones
            .iter_mut()
            .find(|row| row.id == id)
            .ok_or(StoreError::NotFound("phone challenge"))?;
        row.challenge = Some(challenge.to_string());
        row.expires_at = Some(expires_at);
        row.attempts = 0;
        Ok(row.clone())
    }

    async fn consume_phone_attempt(
        &mut self,
        id: Uuid,
    ) -> Result<PhoneVerificationRow, StoreError> {
        let mut inner = self.inner.lock();
        let row = inner
            .phones
            .iter_mut()
            .find(|row| row.id == id)
            .ok_or(StoreError::NotFound("phone challenge"))?;
        if row.attempts >= PHONE_MAX_ATTEMPTS {
            return Err(StoreError::Conflict("phone challenge exhausted"));
        }
        row.attempts += 1;
        Ok(row.clone())
    }

    async fn mark_phone_verified(
        &mut self,
        id: Uuid,
        at: OffsetDateTime,
    ) -> Result<PhoneVerificationRow, StoreError> {
        let mut inner = self.inner.lock();
        let row = inner
            .phones
            .iter_mut()
            .find(|row| row.id == id)
            .ok_or(StoreError::NotFound("phone challenge"))?;
        row.verified_at = Some(at);
        row.challenge = None;
        Ok(row.clone())
    }

    async fn verified_phone_for_user(
        &mut self,
        user: UserId,
    ) -> Result<Option<PhoneVerificationRow>, StoreError> {
        Ok(self
            .inner
            .lock()
            .phones
            .iter()
            .find(|row| row.user == user && row.verified_at.is_some())
            .cloned())
    }

    async fn insert_proposal(
        &mut self,
        proposal: MoneyProposal,
    ) -> Result<MoneyProposal, StoreError> {
        let mut inner = self.inner.lock();
        if inner
            .proposals
            .iter()
            .any(|row| row.replay_key == proposal.replay_key)
        {
            return Err(StoreError::Conflict("proposal replay"));
        }
        inner.proposals.push(proposal.clone());
        Ok(proposal)
    }

    async fn get_proposal_by_replay(
        &mut self,
        replay_key: &str,
    ) -> Result<Option<MoneyProposal>, StoreError> {
        Ok(self
            .inner
            .lock()
            .proposals
            .iter()
            .find(|row| row.replay_key == replay_key)
            .cloned())
    }

    async fn get_proposal(&mut self, id: Uuid) -> Result<MoneyProposal, StoreError> {
        self.inner
            .lock()
            .proposals
            .iter()
            .find(|row| row.id == id)
            .cloned()
            .ok_or(StoreError::NotFound("proposal"))
    }

    async fn confirm_proposal(
        &mut self,
        id: Uuid,
        confirmer: &str,
        now: OffsetDateTime,
    ) -> Result<MoneyProposal, StoreError> {
        let _ = now;
        let mut inner = self.inner.lock();
        let row = inner
            .proposals
            .iter_mut()
            .find(|row| row.id == id)
            .ok_or(StoreError::NotFound("proposal"))?;
        row.confirmer_token_id = Some(confirmer.to_string());
        row.status = ProposalStatus::Confirmed;
        Ok(row.clone())
    }

    async fn audit_insert(&mut self, action: AdminAction) -> Result<(), StoreError> {
        self.inner.lock().audits.push(action);
        Ok(())
    }

    async fn settled_dests(&mut self, user: UserId) -> Result<Vec<String>, StoreError> {
        Ok(self
            .inner
            .lock()
            .settled
            .get(&user.0)
            .cloned()
            .unwrap_or_default())
    }

    async fn record_settled_dest(&mut self, user: UserId, dest: String) -> Result<(), StoreError> {
        self.inner
            .lock()
            .settled
            .entry(user.0)
            .or_default()
            .push(dest);
        Ok(())
    }

    async fn config_i64(&mut self, key: &str) -> Result<Option<i64>, StoreError> {
        Ok(self.inner.lock().config_i64.get(key).copied())
    }

    async fn config_json(&mut self, key: &str) -> Result<Option<serde_json::Value>, StoreError> {
        Ok(self.inner.lock().config_json.get(key).cloned())
    }

    async fn set_config_i64(&mut self, key: &str, value: i64) -> Result<(), StoreError> {
        self.inner.lock().config_i64.insert(key.to_string(), value);
        Ok(())
    }

    async fn set_config_json(
        &mut self,
        key: &str,
        value: serde_json::Value,
    ) -> Result<(), StoreError> {
        self.inner.lock().config_json.insert(key.to_string(), value);
        Ok(())
    }

    async fn insert_frozen_license(
        &mut self,
        license: FrozenFundsLicense,
    ) -> Result<FrozenFundsLicense, StoreError> {
        self.inner.lock().licenses.push(license.clone());
        Ok(license)
    }

    async fn get_frozen_license(&mut self, id: Uuid) -> Result<FrozenFundsLicense, StoreError> {
        self.inner
            .lock()
            .licenses
            .iter()
            .find(|row| row.id == id)
            .cloned()
            .ok_or(StoreError::NotFound("frozen license"))
    }
}

#[async_trait]
impl Committable for FakeComplianceTx {
    async fn commit(self: Box<Self>) -> Result<(), StoreError> {
        Ok(())
    }
}

/// Test phone provider that records the last E.164 and accepts every code.
#[derive(Clone)]
pub struct RecordingPhone {
    e164: Arc<Mutex<HashMap<UserId, String>>>,
    pub accept: bool,
}

impl RecordingPhone {
    #[must_use]
    pub fn last_e164(&self, user: UserId) -> Option<String> {
        self.e164.lock().get(&user).cloned()
    }
}

impl Default for RecordingPhone {
    fn default() -> Self {
        Self {
            e164: Arc::new(Mutex::new(HashMap::new())),
            accept: true,
        }
    }
}

#[async_trait]
impl PhoneVerification for RecordingPhone {
    async fn start_challenge(&self, user: UserId, e164: &str) -> Result<(), StoreError> {
        self.e164.lock().insert(user, e164.to_string());
        Ok(())
    }

    async fn verify(&self, _user: UserId, _code: &str) -> Result<bool, StoreError> {
        Ok(self.accept)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use time::Duration;

    fn t0() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(60_000)
    }

    /// The in-memory double must answer the same contract the Pg adapter
    /// does, including the paths no use case reaches yet — otherwise a wave
    /// swapping one for the other would diverge silently.
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn the_fake_matches_the_pg_contract_on_dests_config_and_key_rotation() {
        let store = FakeComplianceStore::new();
        let user = store.add_user("fake-contract");
        let rival = store.add_user("fake-rival");
        let now = t0();
        let mut tx = store.admin_tx().await.unwrap();

        assert!(tx.settled_dests(user).await.unwrap().is_empty());
        tx.record_settled_dest(user, "dest-a".into()).await.unwrap();
        tx.record_settled_dest(user, "dest-b".into()).await.unwrap();
        assert_eq!(
            tx.settled_dests(user).await.unwrap(),
            vec!["dest-a".to_string(), "dest-b".to_string()]
        );
        assert!(tx.settled_dests(rival).await.unwrap().is_empty());

        assert!(tx.config_i64("missing").await.unwrap().is_none());
        assert!(tx.config_json("missing").await.unwrap().is_none());
        tx.set_config_i64("shadow_trade_cap_micro", 25_000_000)
            .await
            .unwrap();
        assert_eq!(
            tx.config_i64("shadow_trade_cap_micro").await.unwrap(),
            Some(25_000_000)
        );
        tx.set_config_json("region_allowset", serde_json::json!(["CA"]))
            .await
            .unwrap();
        assert_eq!(
            tx.config_json("region_allowset").await.unwrap(),
            Some(serde_json::json!(["CA"]))
        );

        // Same number under a rotated HMAC key version is a distinct row.
        let base = PhoneVerificationRow {
            id: Uuid::new_v4(),
            user,
            number_hmac: "hmac-x".into(),
            hmac_key_version: 1,
            challenge: Some("digest".into()),
            expires_at: Some(now + Duration::minutes(10)),
            attempts: 0,
            verified_at: None,
            provider_ref: None,
        };
        tx.insert_phone_challenge(base.clone()).await.unwrap();
        assert!(matches!(
            tx.insert_phone_challenge(PhoneVerificationRow {
                id: Uuid::new_v4(),
                user: rival,
                ..base.clone()
            })
            .await,
            Err(StoreError::Conflict("phone number already bound"))
        ));
        tx.insert_phone_challenge(PhoneVerificationRow {
            id: Uuid::new_v4(),
            user: rival,
            hmac_key_version: 2,
            ..base.clone()
        })
        .await
        .unwrap();
        assert!(matches!(
            tx.refresh_phone_challenge(Uuid::new_v4(), "d", now).await,
            Err(StoreError::NotFound("phone challenge"))
        ));
        assert!(matches!(
            tx.consume_phone_attempt(Uuid::new_v4()).await,
            Err(StoreError::NotFound("phone challenge"))
        ));
        assert!(matches!(
            tx.mark_phone_verified(Uuid::new_v4(), now).await,
            Err(StoreError::NotFound("phone challenge"))
        ));
        assert!(tx.phone_by_hmac("hmac-x", 9).await.unwrap().is_none());

        // Missing subjects fail closed rather than inventing a row.
        assert!(matches!(
            tx.get_frozen_license(Uuid::new_v4()).await,
            Err(StoreError::NotFound("frozen license"))
        ));
        assert!(matches!(
            tx.get_proposal(Uuid::new_v4()).await,
            Err(StoreError::NotFound("proposal"))
        ));
        assert!(matches!(
            tx.get_self_exclusion(Uuid::new_v4()).await,
            Err(StoreError::NotFound("self exclusion"))
        ));
        assert!(matches!(
            tx.clear_aml_flag(Uuid::new_v4()).await,
            Err(StoreError::NotFound("aml flag"))
        ));
        assert!(matches!(
            tx.set_user_status(UserId(Uuid::new_v4()), UserStatus::Banned)
                .await,
            Err(StoreError::NotFound("user"))
        ));
        assert!(tx.get_deposit_limit(rival).await.unwrap().is_none());
        assert!(tx.lift_self_exclusion(Uuid::new_v4(), now).await.is_err());
        tx.commit().await.unwrap();

        // The recording provider echoes what the use case sent it.
        let phone = RecordingPhone::default();
        assert!(phone.last_e164(user).is_none());
        PhoneVerification::start_challenge(&phone, user, "+15550009999")
            .await
            .unwrap();
        assert_eq!(phone.last_e164(user).as_deref(), Some("+15550009999"));
        assert!(PhoneVerification::verify(&phone, user, "000000")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn a_duplicate_inbox_event_is_a_conflict_not_a_second_row() {
        let store = FakeComplianceStore::new();
        let user = store.add_user("fake-inbox");
        let now = t0();
        let record = InboxRecord {
            provider: "persona".into(),
            event_id: "evt-1".into(),
            payload_hash: "h".into(),
            payload: serde_json::json!({}),
            user_id: Some(user),
            received_at: now,
        };
        let mut tx = store.admin_tx().await.unwrap();
        assert!(tx.inbox_get("persona", "evt-1").await.unwrap().is_none());
        tx.inbox_insert(record.clone()).await.unwrap();
        assert!(matches!(
            tx.inbox_insert(record).await,
            Err(StoreError::Conflict("inbox event"))
        ));
        tx.commit().await.unwrap();
    }

    /// Fake/Pg role-contract parity for the class-1 inbox lock. The Pg side is
    /// a `pg_advisory_xact_lock`; if this fake could not make a second
    /// transaction wait, it would keep hiding the inbox race exactly as it did
    /// before (the application suite was green while Postgres raced).
    #[tokio::test]
    async fn serialize_inbox_blocks_a_second_tx_on_the_same_key_until_the_first_ends() {
        let store = FakeComplianceStore::new();
        let mut first = ComplianceStore::compliance_tx(&store).await.unwrap();
        first.serialize_inbox("kyc", "evt-1").await.unwrap();

        let mut second = ComplianceStore::compliance_tx(&store).await.unwrap();
        let blocked = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            second.serialize_inbox("kyc", "evt-1"),
        )
        .await;
        assert!(
            blocked.is_err(),
            "a second transaction must wait on the same inbox key"
        );

        // A different key is not blocked by the held one.
        let mut other = ComplianceStore::compliance_tx(&store).await.unwrap();
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            other.serialize_inbox("kyc", "evt-2"),
        )
        .await
        .unwrap()
        .unwrap();

        first.commit().await.unwrap();
        assert!(
            matches!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(200),
                    second.serialize_inbox("kyc", "evt-1"),
                )
                .await,
                Ok(Ok(()))
            ),
            "committing the first transaction releases the key"
        );
    }

    #[tokio::test]
    async fn a_dropped_transaction_releases_its_inbox_key() {
        let store = FakeComplianceStore::new();
        let mut first = ComplianceStore::compliance_tx(&store).await.unwrap();
        first.serialize_inbox("kyc", "evt-1").await.unwrap();
        let mut second = ComplianceStore::compliance_tx(&store).await.unwrap();
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(50),
            second.serialize_inbox("kyc", "evt-1"),
        )
        .await
        .is_err());
        // Rollback, not commit: an aborted transaction must not hold the key.
        drop(first);
        assert!(
            matches!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(200),
                    second.serialize_inbox("kyc", "evt-1"),
                )
                .await,
                Ok(Ok(()))
            ),
            "a rolled-back transaction releases the key"
        );
    }

    #[tokio::test]
    async fn serialize_inbox_is_reentrant_within_one_transaction() {
        let store = FakeComplianceStore::new();
        let mut tx = ComplianceStore::compliance_tx(&store).await.unwrap();
        tx.serialize_inbox("kyc", "evt-1").await.unwrap();
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            tx.serialize_inbox("kyc", "evt-1"),
        )
        .await
        .unwrap()
        .unwrap();
        tx.commit().await.unwrap();
    }
}
