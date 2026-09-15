//! Send: lease, persist signed bytes BEFORE broadcast, same-bytes rebroadcast,
//! W3 lease-lookup precedence.

use async_trait::async_trait;
use serde_json::json;
use time::Duration;
use uuid::Uuid;

use crate::error::{AppError, StoreError};
use crate::model::Event;
use crate::ports::{
    Clock, Combo, LandingState, OutboundAttemptRow, OutboundRails, OutboundSubject,
    QuorumObservation, RailIdentity, WithdrawStore, WithdrawTx, WithdrawalId, WithdrawalRow,
};

use super::withdraw_request::{geo_context, is_fresh_clear, is_self_exclusion_egress};

const LEASE: Duration = Duration::seconds(30);

/// Signs a transfer and answers signature-presence lookups.
#[async_trait]
pub trait WithdrawSigner: Send + Sync {
    async fn sign(
        &self,
        dest: &str,
        amount_micro: i64,
    ) -> Result<(Vec<u8>, String, i64), StoreError>;
    async fn lookup_signature(&self, signature: &str) -> Result<SignaturePresence, StoreError>;
    async fn finalized_block_height(&self) -> Result<i64, StoreError>;
}

/// Signature lookup used by W3 lease recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignaturePresence {
    Present,
    Absent,
    Unknown,
}

/// Send / claim / rebroadcast a withdrawal.
pub struct SendWithdraw<'a, S: WithdrawStore, C: Clock> {
    pub store: &'a S,
    pub clock: &'a C,
    pub rails: &'a dyn OutboundRails,
    pub signer: &'a dyn WithdrawSigner,
    pub identity: &'a RailIdentity,
}

impl<S: WithdrawStore, C: Clock> SendWithdraw<'_, S, C> {
    /// Claim W2→W3 (or recover W3), persist bytes, broadcast, CAS to W6.
    ///
    /// # Errors
    /// Illegal transition, lien failure, or rail/store errors.
    pub async fn execute(&self, id: WithdrawalId) -> Result<WithdrawalRow, AppError> {
        let user = self.store.withdrawal_user(id).await?;
        let mut tx = self.store.withdraw_tx().await?;
        tx.serialize_key(&format!("withdraw-send:{}", id.0)).await?;
        tx.lock_user(user).await?;
        let row = tx.withdrawal_for_update(id).await?;
        ensure_locked_owner(&row, user)?;
        if matches!(row.combo, Combo::W6 | Combo::W9 | Combo::W10) {
            let payment = tx
                .outbound_by_subject(OutboundSubject::Withdrawal, row.id.0)
                .await?
                .ok_or(StoreError::NotFound("outbound payment"))?;
            ensure_payment_matches(&payment, &row, self.identity)?;
            let attempts = tx.attempts_for(payment.id).await?;
            let state_ok = if row.combo == Combo::W6 {
                attempts
                    .iter()
                    .any(|attempt| attempt.landing_state == LandingState::Broadcast)
            } else {
                attempts
                    .iter()
                    .any(|attempt| attempt.landing_state == LandingState::Finalized)
            };
            if !state_ok {
                return Err(StoreError::Invariant(
                    "withdrawal state has no matching outbound attempt",
                )
                .into());
            }
            return Ok(row);
        }
        if row.combo == Combo::W3 || row.combo == Combo::W8 {
            return self.recover_sending(tx, row).await;
        }
        if row.combo == Combo::W7 {
            return self.rebroadcast_unknown(tx, row).await;
        }
        if row.combo != Combo::W2 {
            return Err(AppError::IllegalTransition);
        }
        ensure_send_allowed(tx.as_mut(), &row, self.clock.now()).await?;
        self.claim_w2(tx, row).await
    }

    /// Replace an expired unknown attempt only after archival non-landing proof.
    ///
    /// # Errors
    /// The proof is not definitive, the blockhash is still live, or store/rail failure.
    pub async fn replace_expired(
        &self,
        id: WithdrawalId,
        observations: &[QuorumObservation],
    ) -> Result<WithdrawalRow, AppError> {
        let user = self.store.withdrawal_user(id).await?;
        let mut tx = self.store.withdraw_tx().await?;
        tx.serialize_key(&format!("withdraw-send:{}", id.0)).await?;
        tx.lock_user(user).await?;
        let mut row = tx.withdrawal_for_update(id).await?;
        ensure_locked_owner(&row, user)?;
        if row.combo != Combo::W7 {
            return Err(AppError::IllegalTransition);
        }
        let payment = tx
            .outbound_by_subject(OutboundSubject::Withdrawal, row.id.0)
            .await?
            .ok_or(StoreError::NotFound("outbound payment"))?;
        ensure_payment_matches(&payment, &row, self.identity)?;
        let mut attempts = tx.attempts_for(payment.id).await?;
        let mut old = tx
            .live_attempt(payment.id)
            .await?
            .ok_or(StoreError::NotFound("outbound attempt"))?;
        if old.landing_state != LandingState::Unknown {
            return Err(StoreError::Invariant("W7 attempt is not unknown").into());
        }
        let height = self.signer.finalized_block_height().await?;
        if height <= old.last_valid_block_height {
            return Err(AppError::ProposalConflict("blockhash has not expired"));
        }
        let verdict = super::withdraw_reconcile::safe_non_landing_verdict(
            old.last_valid_block_height,
            observations,
            &self.identity.rpc_endpoints,
        );
        old.evidence = Some(json!({
            "purpose": "replacement",
            "last_valid_block_height": old.last_valid_block_height,
            "observed_at": self.clock.now(),
            "observations": observations.iter().map(|item| json!({
                "endpoint": item.endpoint,
                "finalized_height": item.finalized_height,
                "signature_present": item.signature_present,
                "pruned": item.pruned,
            })).collect::<Vec<_>>(),
            "verdict": match verdict {
                crate::ports::NonLandingVerdict::DefinitiveFailed => "definitive_failed",
                crate::ports::NonLandingVerdict::Unknown => "unknown",
            },
        }));
        if verdict != crate::ports::NonLandingVerdict::DefinitiveFailed {
            tx.save_attempt(&old).await?;
            tx.commit().await?;
            return Ok(row);
        }
        old.landing_state = LandingState::DefinitiveFailed;
        tx.save_attempt(&old).await?;

        let (bytes, signature, last_valid) = self.signer.sign(&row.dest, row.amount_micro).await?;
        if attempts
            .iter()
            .any(|attempt| attempt.signature == signature)
        {
            return Err(StoreError::Invariant("replacement signature was reused").into());
        }
        let attempt = OutboundAttemptRow {
            id: Uuid::new_v4(),
            payment_id: payment.id,
            attempt_number: crate::ports::outbound::next_attempt_number(&attempts),
            replaces_attempt_id: Some(old.id),
            signed_tx_bytes: bytes,
            signature,
            last_valid_block_height: last_valid,
            landing_state: LandingState::Prepared,
            lease_expires_at: Some(self.clock.now() + LEASE),
            evidence: None,
        };
        attempts.push(attempt.clone());
        tx.insert_attempt(&attempt).await?;
        self.rails
            .persist_signed(payment.id, &attempt.signed_tx_bytes, &attempt.signature)
            .await?;
        row.combo = Combo::W8;
        finish_cas(tx.as_mut(), Combo::W7, &row, "replace-expired").await?;
        tx.commit().await?;
        match self.rails.broadcast(payment.id).await {
            Ok(()) => self.cas_broadcast(row.id, Combo::W8).await,
            Err(err) => Err(err.into()),
        }
    }

    async fn claim_w2(
        &self,
        mut tx: Box<dyn WithdrawTx + '_>,
        mut row: WithdrawalRow,
    ) -> Result<WithdrawalRow, AppError> {
        let now = self.clock.now();
        let payment = if let Some(existing) = tx
            .outbound_by_subject(OutboundSubject::Withdrawal, row.id.0)
            .await?
        {
            existing
        } else {
            let payment = crate::ports::outbound::payment_for(
                OutboundSubject::Withdrawal,
                row.id.0,
                row.dest.clone(),
                row.amount_micro,
                self.identity,
            );
            tx.insert_outbound_payment(&payment).await?;
            payment
        };
        ensure_payment_matches(&payment, &row, self.identity)?;
        if !tx.attempts_for(payment.id).await?.is_empty() {
            return Err(StoreError::Invariant("W2 already has a send attempt").into());
        }
        let (bytes, signature, last_valid) = self.signer.sign(&row.dest, row.amount_micro).await?;
        let attempt = OutboundAttemptRow {
            id: Uuid::new_v4(),
            payment_id: payment.id,
            attempt_number: 1,
            replaces_attempt_id: None,
            signed_tx_bytes: bytes,
            signature,
            last_valid_block_height: last_valid,
            landing_state: LandingState::Prepared,
            lease_expires_at: Some(now + LEASE),
            evidence: None,
        };
        tx.insert_attempt(&attempt).await?;
        self.rails
            .persist_signed(payment.id, &attempt.signed_tx_bytes, &attempt.signature)
            .await?;
        row.combo = Combo::W3;
        if !tx.cas_withdrawal(row.id, Combo::W2, &row).await? {
            return Err(AppError::IllegalTransition);
        }
        tx.append_withdrawal_event(row.id, "send-claim", "machine", json!({"to": "W3"}))
            .await?;
        tx.commit().await?;

        // Broadcast AFTER the W3 persist is committed so a crash here
        // recovers via lease-lookup, never a second prepared attempt.
        match self.rails.broadcast(payment.id).await {
            Ok(()) => self.cas_broadcast(row.id, Combo::W3).await,
            Err(err) => Err(err.into()),
        }
    }

    async fn recover_sending(
        &self,
        mut tx: Box<dyn WithdrawTx + '_>,
        mut row: WithdrawalRow,
    ) -> Result<WithdrawalRow, AppError> {
        let from = row.combo;
        let now = self.clock.now();
        let payment = tx
            .outbound_by_subject(OutboundSubject::Withdrawal, row.id.0)
            .await?
            .ok_or(StoreError::NotFound("outbound payment"))?;
        ensure_payment_matches(&payment, &row, self.identity)?;
        let mut attempt = tx
            .live_attempt(payment.id)
            .await?
            .ok_or(StoreError::NotFound("outbound attempt"))?;
        if attempt.landing_state != LandingState::Prepared {
            return Err(StoreError::Invariant("sending withdrawal attempt is not prepared").into());
        }
        let lease_expires_at = attempt
            .lease_expires_at
            .ok_or(StoreError::Invariant("sending attempt has no lease"))?;
        if now < lease_expires_at {
            return Err(AppError::ProposalConflict("send lease still held"));
        }
        // W3 lease-lookup precedence: signature lookup FIRST.
        let presence = self
            .signer
            .lookup_signature(&attempt.signature)
            .await
            .unwrap_or(SignaturePresence::Unknown);
        match presence {
            SignaturePresence::Present => {
                attempt.landing_state = LandingState::Broadcast;
                tx.save_attempt(&attempt).await?;
                row.combo = Combo::W6;
                row.sent_at = Some(now);
                finish_cas(tx.as_mut(), from, &row, "lease-lookup-hit").await?;
                tx.commit().await?;
                return Ok(row);
            }
            SignaturePresence::Absent => {
                if self
                    .signer
                    .finalized_block_height()
                    .await
                    .is_ok_and(|height| height <= attempt.last_valid_block_height)
                {
                    tx.commit().await?;
                    crate::ports::outbound::same_bytes_rebroadcast(self.rails, &payment, &attempt)
                        .await?;
                    return self.cas_broadcast(row.id, from).await;
                }
            }
            SignaturePresence::Unknown => {}
        }
        row.combo = Combo::W7;
        attempt.landing_state = LandingState::Unknown;
        tx.save_attempt(&attempt).await?;
        finish_cas(tx.as_mut(), from, &row, "lease-lookup-unknown").await?;
        tx.commit().await?;
        Ok(row)
    }

    async fn rebroadcast_unknown(
        &self,
        mut tx: Box<dyn WithdrawTx + '_>,
        row: WithdrawalRow,
    ) -> Result<WithdrawalRow, AppError> {
        let payment = tx
            .outbound_by_subject(OutboundSubject::Withdrawal, row.id.0)
            .await?
            .ok_or(StoreError::NotFound("outbound payment"))?;
        ensure_payment_matches(&payment, &row, self.identity)?;
        let attempt = tx
            .live_attempt(payment.id)
            .await?
            .ok_or(StoreError::NotFound("outbound attempt"))?;
        if attempt.landing_state != LandingState::Unknown {
            return Err(StoreError::Invariant("W7 attempt is not unknown").into());
        }
        let height = self.signer.finalized_block_height().await?;
        if height > attempt.last_valid_block_height {
            return Err(AppError::ProposalConflict(
                "blockhash expired; archival proof is required",
            ));
        }
        tx.commit().await?;
        crate::ports::outbound::same_bytes_rebroadcast(self.rails, &payment, &attempt).await?;
        Ok(row)
    }

    async fn cas_broadcast(
        &self,
        id: WithdrawalId,
        expected: Combo,
    ) -> Result<WithdrawalRow, AppError> {
        let user = self.store.withdrawal_user(id).await?;
        let mut tx = self.store.withdraw_tx().await?;
        tx.serialize_key(&format!("withdraw-send:{}", id.0)).await?;
        tx.lock_user(user).await?;
        let mut row = tx.withdrawal_for_update(id).await?;
        ensure_locked_owner(&row, user)?;
        if row.combo != expected {
            return Ok(row);
        }
        let payment = tx
            .outbound_by_subject(OutboundSubject::Withdrawal, row.id.0)
            .await?
            .ok_or(StoreError::NotFound("outbound payment"))?;
        ensure_payment_matches(&payment, &row, self.identity)?;
        let mut attempt = tx
            .live_attempt(payment.id)
            .await?
            .ok_or(StoreError::NotFound("outbound attempt"))?;
        if attempt.landing_state != LandingState::Prepared {
            return Err(StoreError::Invariant("broadcast CAS attempt is not prepared").into());
        }
        attempt.landing_state = LandingState::Broadcast;
        tx.save_attempt(&attempt).await?;
        row.combo = Combo::W6;
        row.sent_at.get_or_insert_with(|| self.clock.now());
        finish_cas(tx.as_mut(), expected, &row, "broadcast").await?;
        tx.commit().await?;
        Ok(row)
    }
}

fn ensure_locked_owner(row: &WithdrawalRow, user: crate::model::UserId) -> Result<(), AppError> {
    if row.user != user {
        return Err(StoreError::Invariant("withdrawal owner changed after user lock").into());
    }
    Ok(())
}

pub(super) fn ensure_payment_matches(
    payment: &crate::ports::OutboundPaymentRow,
    row: &WithdrawalRow,
    identity: &RailIdentity,
) -> Result<(), AppError> {
    if payment.subject != OutboundSubject::Withdrawal
        || payment.subject_id != row.id.0
        || payment.dest != row.dest
        || payment.amount_micro != row.amount_micro
        || payment.rail_fingerprint != identity.fingerprint()
    {
        return Err(StoreError::Invariant("outbound payment intent mismatch").into());
    }
    Ok(())
}

async fn ensure_send_allowed(
    tx: &mut dyn WithdrawTx,
    row: &WithdrawalRow,
    now: time::OffsetDateTime,
) -> Result<(), AppError> {
    let limits = tx.limits().await?;
    let view = tx.user_money_view(row.user).await?;
    if limits.pause_withdrawals {
        return Err(AppError::MoneyForbidden("withdrawals are paused"));
    }
    if view.status == crate::model::UserStatus::Banned {
        return Err(AppError::MoneyForbidden("account is banned"));
    }
    if view.status == crate::model::UserStatus::ShadowLimited
        && !row
            .risk_reasons
            .iter()
            .any(|reason| reason == "shadow_limited")
    {
        return Err(AppError::MoneyForbidden(
            "withdrawal risk tightened after approval",
        ));
    }
    if i64::from(view.kyc_tier) < limits.withdraw_kyc_tier {
        return Err(AppError::MoneyForbidden("kyc tier too low"));
    }
    if tx.open_aml_flag_count(row.user).await? > 0 {
        return Err(AppError::MoneyForbidden("open AML flag"));
    }
    let sanctions = tx.latest_screening(row.user, "withdraw").await?;
    if !is_fresh_clear(sanctions.as_ref(), now) {
        return Err(AppError::MoneyForbidden(
            "sanctions screening is not fresh Clear",
        ));
    }
    let geo = tx
        .latest_screening(row.user, &geo_context(&row.request_fingerprint))
        .await?;
    if !is_fresh_clear(geo.as_ref(), now) {
        return Err(AppError::MoneyForbidden("geo screening is not fresh Clear"));
    }
    if view.self_excluded_until.is_some_and(|until| until > now)
        && !is_self_exclusion_egress(tx, row.user, &row.dest).await?
    {
        return Err(AppError::MoneyForbidden(
            "self-excluded user may only use a settled or observation-source destination",
        ));
    }
    if row.amount_micro >= limits.auto_approve_micro
        && !row
            .risk_reasons
            .iter()
            .any(|reason| reason == "amount_ge_auto")
    {
        return Err(AppError::MoneyForbidden(
            "withdrawal risk tightened after approval",
        ));
    }
    if row.amount_micro >= limits.dual_control_micro
        && !row
            .risk_reasons
            .iter()
            .any(|reason| reason == "amount_ge_dual")
    {
        return Err(AppError::MoneyForbidden(
            "withdrawal risk tightened after approval",
        ));
    }
    let warmth = tx.dest_warmth(&row.dest).await?;
    for (applies, reason) in [
        (!warmth.is_warm(limits, now), "dest_not_warm"),
        (warmth.distinct_users >= 2, "dest_shared"),
        (warmth.is_refund_dest, "dest_is_refund"),
    ] {
        if applies && !row.risk_reasons.iter().any(|existing| existing == reason) {
            return Err(AppError::MoneyForbidden(
                "withdrawal risk tightened after approval",
            ));
        }
    }
    let outstanding = tx
        .open_receivables(row.user)
        .await?
        .into_iter()
        .fold(0_i64, |sum, item| {
            sum.saturating_add(item.outstanding_micro)
        });
    if outstanding > 0 {
        return Err(AppError::ReceivableOpen {
            outstanding_micro: outstanding,
        });
    }
    Ok(())
}

async fn finish_cas(
    tx: &mut dyn WithdrawTx,
    from: Combo,
    row: &WithdrawalRow,
    kind: &str,
) -> Result<(), AppError> {
    if !tx.cas_withdrawal(row.id, from, row).await? {
        return Err(AppError::IllegalTransition);
    }
    tx.append_withdrawal_event(row.id, kind, "machine", json!({"to": row.combo.label()}))
        .await?;
    let _ = tx
        .record_event(Event {
            event_type: "WithdrawalSend",
            aggregate_type: "withdrawal",
            aggregate_id: row.id.0,
            payload: json!({"to": row.combo.label()}),
        })
        .await?;
    Ok(())
}

/// Deterministic test signer.
pub struct FakeSigner {
    pub bytes: Vec<u8>,
    pub signature: String,
    pub last_valid: i64,
    pub presence: std::sync::Mutex<SignaturePresence>,
    pub finalized_height: std::sync::Mutex<i64>,
}

impl FakeSigner {
    #[must_use]
    pub fn new(signature: &str) -> Self {
        Self {
            bytes: signature.as_bytes().to_vec(),
            signature: signature.to_string(),
            last_valid: 1_000,
            presence: std::sync::Mutex::new(SignaturePresence::Absent),
            finalized_height: std::sync::Mutex::new(0),
        }
    }

    pub fn set_presence(&self, presence: SignaturePresence) {
        if let Ok(mut slot) = self.presence.lock() {
            *slot = presence;
        }
    }

    pub fn set_finalized_height(&self, height: i64) {
        if let Ok(mut slot) = self.finalized_height.lock() {
            *slot = height;
        }
    }
}

#[async_trait]
impl WithdrawSigner for FakeSigner {
    async fn sign(
        &self,
        _dest: &str,
        _amount_micro: i64,
    ) -> Result<(Vec<u8>, String, i64), StoreError> {
        Ok((self.bytes.clone(), self.signature.clone(), self.last_valid))
    }

    async fn lookup_signature(&self, _signature: &str) -> Result<SignaturePresence, StoreError> {
        Ok(self
            .presence
            .lock()
            .map_or(SignaturePresence::Unknown, |slot| *slot))
    }

    async fn finalized_block_height(&self) -> Result<i64, StoreError> {
        Ok(self.finalized_height.lock().map_or(0, |slot| *slot))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines, clippy::unwrap_used)]

    use super::*;
    use crate::model::{AdminContext, AdminRole, UserStatus};
    use crate::ports::withdraw_decide::{decide_machine, DecideCmd, DecideWithdraw};
    use crate::ports::withdraw_fakes::{
        dest_a, dest_b, test_rail, FakeRails, FakeScreen, FakeWithdrawStore,
    };
    use crate::ports::withdraw_request::{RequestClock, RequestWithdraw};
    use crate::ports::RequestWithdrawCmd;
    use crate::ports::ScreenVerdict;
    use crate::ports::WithdrawStore;
    use std::net::IpAddr;
    use time::OffsetDateTime;

    fn finance() -> AdminContext {
        AdminContext::Admin {
            token_digest: "fin-a".into(),
            role: AdminRole::Finance,
        }
    }

    async fn approved_row(store: &FakeWithdrawStore) -> WithdrawalId {
        let user = store.seed_user(UserStatus::Active, 2);
        store.credit(user, 20_000_000);
        let now = store.now();
        let screen = FakeScreen {
            verdict: ScreenVerdict::Clear {
                checked_at: now,
                expires_at: now + Duration::hours(24),
                policy_version: "1".into(),
            },
            fail: false,
        };
        let receipt = RequestWithdraw {
            store,
            clock: &RequestClock(now),
            geo: &screen,
            sanctions: &screen,
        }
        .execute(RequestWithdrawCmd {
            user,
            amount_micro: 5_000_000,
            dest: dest_a(),
            client_ip: Some(IpAddr::from([203, 0, 113, 1])),
            idempotency_key: Some(format!("s-{}", user.0)),
        })
        .await
        .unwrap();
        let id = receipt.id.unwrap();
        decide_machine(store, &RequestClock(now), id).await.unwrap();
        DecideWithdraw {
            store,
            clock: &RequestClock(now),
            actor: finance(),
        }
        .execute(DecideCmd::FinanceApprove {
            id,
            reason: "send".into(),
        })
        .await
        .unwrap();
        id
    }

    #[tokio::test]
    async fn persist_before_broadcast_and_same_bytes_on_recovery() {
        let store = FakeWithdrawStore::new();
        let id = approved_row(&store).await;
        let rails = FakeRails::default();
        let signer = FakeSigner::new("sig-send-1");
        let identity = test_rail();
        let sent = SendWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            rails: &rails,
            signer: &signer,
            identity: &identity,
        }
        .execute(id)
        .await
        .unwrap();
        assert_eq!(sent.combo, Combo::W6);
        let bytes = rails.persisted_bytes(
            store
                .attempts()
                .first()
                .map(|attempt| attempt.payment_id)
                .unwrap(),
        );
        assert_eq!(bytes.as_deref(), Some(b"sig-send-1".as_slice()));

        // Crash window: row forced back to W3 with expired lease; lookup hit.
        {
            let mut tx = store.withdraw_tx().await.unwrap();
            let mut row = tx.withdrawal_for_update(id).await.unwrap();
            row.combo = Combo::W3;
            tx.cas_withdrawal(id, Combo::W6, &row).await.unwrap();
            let mut attempt = store.attempts().into_iter().next().unwrap();
            attempt.landing_state = LandingState::Prepared;
            attempt.lease_expires_at = Some(OffsetDateTime::from_unix_timestamp(1).unwrap());
            tx.save_attempt(&attempt).await.unwrap();
            tx.commit().await.unwrap();
        }
        signer.set_presence(SignaturePresence::Present);
        let recovered = SendWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            rails: &rails,
            signer: &signer,
            identity: &identity,
        }
        .execute(id)
        .await
        .unwrap();
        assert_eq!(recovered.combo, Combo::W6);
        assert_eq!(store.attempts().len(), 1);
        let replay = SendWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            rails: &rails,
            signer: &signer,
            identity: &identity,
        }
        .execute(id)
        .await
        .unwrap();
        assert_eq!(replay.combo, Combo::W6);
        assert_eq!(store.attempts().len(), 1);
    }

    #[tokio::test]
    async fn broadcast_cas_reacquires_the_user_lock_before_the_withdrawal_row() {
        let store = FakeWithdrawStore::new();
        let id = approved_row(&store).await;
        store.enforce_withdraw_lock_order();
        let rails = FakeRails::default();
        let signer = FakeSigner::new("sig-lock-order");
        let identity = test_rail();

        let sent = SendWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            rails: &rails,
            signer: &signer,
            identity: &identity,
        }
        .execute(id)
        .await
        .unwrap();

        assert_eq!(sent.combo, Combo::W6);
    }

    #[tokio::test]
    async fn broadcast_cas_requires_the_prepared_attempt_shape() {
        let store = FakeWithdrawStore::new();
        let id = approved_row(&store).await;
        let rails = FakeRails::default();
        let signer = FakeSigner::new("sig-cas-state");
        let identity = test_rail();
        SendWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            rails: &rails,
            signer: &signer,
            identity: &identity,
        }
        .execute(id)
        .await
        .unwrap();
        {
            let mut tx = store.withdraw_tx().await.unwrap();
            let mut row = tx.withdrawal_for_update(id).await.unwrap();
            row.combo = Combo::W3;
            tx.cas_withdrawal(id, Combo::W6, &row).await.unwrap();
            let mut attempt = store.attempts().into_iter().next().unwrap();
            attempt.landing_state = LandingState::Unknown;
            tx.save_attempt(&attempt).await.unwrap();
            tx.commit().await.unwrap();
        }
        let result = SendWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            rails: &rails,
            signer: &signer,
            identity: &identity,
        }
        .cas_broadcast(id, Combo::W3)
        .await;

        assert!(matches!(
            result,
            Err(AppError::Store(StoreError::Invariant(_)))
        ));
    }

    #[tokio::test]
    async fn lease_lookup_miss_rebroadcasts_same_bytes() {
        let store = FakeWithdrawStore::new();
        let id = approved_row(&store).await;
        let rails = FakeRails::default();
        let signer = FakeSigner::new("sig-send-2");
        let identity = test_rail();
        SendWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            rails: &rails,
            signer: &signer,
            identity: &identity,
        }
        .execute(id)
        .await
        .unwrap();
        {
            let mut tx = store.withdraw_tx().await.unwrap();
            let mut row = tx.withdrawal_for_update(id).await.unwrap();
            let from = row.combo;
            row.combo = Combo::W3;
            tx.cas_withdrawal(id, from, &row).await.unwrap();
            let mut attempt = store.attempts().into_iter().next().unwrap();
            attempt.landing_state = LandingState::Prepared;
            attempt.lease_expires_at = Some(OffsetDateTime::from_unix_timestamp(1).unwrap());
            tx.save_attempt(&attempt).await.unwrap();
            tx.commit().await.unwrap();
        }
        signer.set_presence(SignaturePresence::Absent);
        let recovered = SendWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            rails: &rails,
            signer: &signer,
            identity: &identity,
        }
        .execute(id)
        .await
        .unwrap();
        assert_eq!(recovered.combo, Combo::W6);
        assert_eq!(store.attempts().len(), 1);
    }

    #[tokio::test]
    async fn broadcast_failure_leaves_one_durable_prepared_attempt_for_retry() {
        let store = FakeWithdrawStore::new();
        let id = approved_row(&store).await;
        let rails = FakeRails::default();
        *rails.fail_broadcast.lock() = true;
        let signer = FakeSigner::new("sig-broadcast-fail");
        let identity = test_rail();
        let service = SendWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            rails: &rails,
            signer: &signer,
            identity: &identity,
        };
        assert!(matches!(
            service.execute(id).await,
            Err(AppError::Store(StoreError::Backend(_)))
        ));
        assert_eq!(store.withdrawals()[0].combo, Combo::W3);
        assert_eq!(store.attempts().len(), 1);
        assert_eq!(store.attempts()[0].landing_state, LandingState::Prepared);
        assert!(matches!(
            service.execute(id).await,
            Err(AppError::ProposalConflict("send lease still held"))
        ));

        store.advance(1);
        let retry = SendWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            rails: &rails,
            signer: &signer,
            identity: &identity,
        };
        assert!(matches!(
            retry.execute(id).await,
            Err(AppError::Store(StoreError::Backend(_)))
        ));
        *rails.fail_broadcast.lock() = false;
        assert_eq!(retry.execute(id).await.unwrap().combo, Combo::W6);
        assert_eq!(store.attempts().len(), 1);
    }

    #[tokio::test]
    async fn sending_recovery_refuses_outbound_intent_drift() {
        let store = FakeWithdrawStore::new();
        let id = approved_row(&store).await;
        let rails = FakeRails::default();
        let signer = FakeSigner::new("sig-drift");
        let identity = test_rail();
        SendWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            rails: &rails,
            signer: &signer,
            identity: &identity,
        }
        .execute(id)
        .await
        .unwrap();
        {
            let mut tx = store.withdraw_tx().await.unwrap();
            let mut row = tx.withdrawal_for_update(id).await.unwrap();
            row.combo = Combo::W3;
            tx.cas_withdrawal(id, Combo::W6, &row).await.unwrap();
            let mut attempt = store.attempts().into_iter().next().unwrap();
            attempt.landing_state = LandingState::Prepared;
            attempt.lease_expires_at = Some(OffsetDateTime::from_unix_timestamp(1).unwrap());
            tx.save_attempt(&attempt).await.unwrap();
            tx.commit().await.unwrap();
        }
        assert!(store.corrupt_outbound_dest(id.0, &dest_b()));
        signer.set_presence(SignaturePresence::Present);

        let result = SendWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            rails: &rails,
            signer: &signer,
            identity: &identity,
        }
        .execute(id)
        .await;

        assert!(matches!(
            result,
            Err(AppError::Store(StoreError::Invariant(_)))
        ));
    }

    #[tokio::test]
    async fn sending_recovery_requires_a_prepared_attempt_with_a_lease() {
        let store = FakeWithdrawStore::new();
        let id = approved_row(&store).await;
        let rails = FakeRails::default();
        let signer = FakeSigner::new("sig-state");
        let identity = test_rail();
        SendWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            rails: &rails,
            signer: &signer,
            identity: &identity,
        }
        .execute(id)
        .await
        .unwrap();
        {
            let mut tx = store.withdraw_tx().await.unwrap();
            let mut row = tx.withdrawal_for_update(id).await.unwrap();
            row.combo = Combo::W3;
            tx.cas_withdrawal(id, Combo::W6, &row).await.unwrap();
            let mut attempt = store.attempts().into_iter().next().unwrap();
            attempt.landing_state = LandingState::Prepared;
            attempt.lease_expires_at = None;
            tx.save_attempt(&attempt).await.unwrap();
            tx.commit().await.unwrap();
        }

        let result = SendWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            rails: &rails,
            signer: &signer,
            identity: &identity,
        }
        .execute(id)
        .await;
        assert!(matches!(
            result,
            Err(AppError::Store(StoreError::Invariant(_)))
        ));

        {
            let mut tx = store.withdraw_tx().await.unwrap();
            let mut attempt = store.attempts().into_iter().next().unwrap();
            attempt.landing_state = LandingState::Broadcast;
            attempt.lease_expires_at = Some(OffsetDateTime::from_unix_timestamp(1).unwrap());
            tx.save_attempt(&attempt).await.unwrap();
            tx.commit().await.unwrap();
        }
        let result = SendWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            rails: &rails,
            signer: &signer,
            identity: &identity,
        }
        .execute(id)
        .await;
        assert!(matches!(
            result,
            Err(AppError::Store(StoreError::Invariant(_)))
        ));
    }

    #[tokio::test]
    async fn send_rechecks_the_receivable_lien_and_open_aml_flag() {
        let store = FakeWithdrawStore::new();
        let id = approved_row(&store).await;
        let row = store
            .withdrawals()
            .into_iter()
            .find(|row| row.id == id)
            .unwrap();
        let _ = store.add_receivable(row.user, 1_000_000);
        store.open_aml(row.user);
        let rails = FakeRails::default();
        let signer = FakeSigner::new("sig-blocked");
        let identity = test_rail();
        let err = SendWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            rails: &rails,
            signer: &signer,
            identity: &identity,
        }
        .execute(id)
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            AppError::ReceivableOpen { .. } | AppError::MoneyForbidden(_)
        ));
        assert!(store.attempts().is_empty());
        assert_eq!(
            store
                .withdrawals()
                .into_iter()
                .find(|candidate| candidate.id == id)
                .unwrap()
                .combo,
            Combo::W2
        );

        store.clear_aml(row.user);
        let err = SendWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            rails: &rails,
            signer: &signer,
            identity: &identity,
        }
        .execute(id)
        .await
        .unwrap_err();
        assert_eq!(
            err,
            AppError::ReceivableOpen {
                outstanding_micro: 1_000_000
            }
        );
    }

    async fn execute_error(
        store: &FakeWithdrawStore,
        id: WithdrawalId,
        now: OffsetDateTime,
    ) -> AppError {
        let rails = FakeRails::default();
        let signer = FakeSigner::new("sig-gate");
        let identity = test_rail();
        SendWithdraw {
            store,
            clock: &RequestClock(now),
            rails: &rails,
            signer: &signer,
            identity: &identity,
        }
        .execute(id)
        .await
        .unwrap_err()
    }

    #[tokio::test]
    async fn send_boundary_fails_closed_for_every_tightened_money_gate() {
        let store = FakeWithdrawStore::new();
        let id = approved_row(&store).await;
        store.set_pause(true);
        assert!(matches!(
            execute_error(&store, id, store.now()).await,
            AppError::MoneyForbidden("withdrawals are paused")
        ));

        let store = FakeWithdrawStore::new();
        let id = approved_row(&store).await;
        let user = store.withdrawals()[0].user;
        store.set_status(user, UserStatus::Banned);
        assert!(matches!(
            execute_error(&store, id, store.now()).await,
            AppError::MoneyForbidden("account is banned")
        ));

        let store = FakeWithdrawStore::new();
        let id = approved_row(&store).await;
        let user = store.withdrawals()[0].user;
        store.set_status(user, UserStatus::ShadowLimited);
        assert!(matches!(
            execute_error(&store, id, store.now()).await,
            AppError::MoneyForbidden("withdrawal risk tightened after approval")
        ));

        let store = FakeWithdrawStore::new();
        let id = approved_row(&store).await;
        let user = store.withdrawals()[0].user;
        store.set_kyc(user, 0);
        assert!(matches!(
            execute_error(&store, id, store.now()).await,
            AppError::MoneyForbidden("kyc tier too low")
        ));

        let store = FakeWithdrawStore::new();
        let id = approved_row(&store).await;
        let later = store.now() + Duration::hours(25);
        assert!(matches!(
            execute_error(&store, id, later).await,
            AppError::MoneyForbidden("sanctions screening is not fresh Clear")
        ));

        let store = FakeWithdrawStore::new();
        let id = approved_row(&store).await;
        let row = store.withdrawals()[0].clone();
        let later = store.now() + Duration::hours(25);
        {
            let mut tx = store.withdraw_tx().await.unwrap();
            tx.persist_screening(
                row.user,
                "withdraw",
                &ScreenVerdict::Clear {
                    checked_at: later,
                    expires_at: later + Duration::hours(1),
                    policy_version: "2".into(),
                },
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
        }
        assert!(matches!(
            execute_error(&store, id, later).await,
            AppError::MoneyForbidden("geo screening is not fresh Clear")
        ));

        let store = FakeWithdrawStore::new();
        let id = approved_row(&store).await;
        let user = store.withdrawals()[0].user;
        store.set_self_excluded(user, store.now() + Duration::days(1));
        assert!(matches!(
            execute_error(&store, id, store.now()).await,
            AppError::MoneyForbidden(_)
        ));

        let store = FakeWithdrawStore::new();
        let id = approved_row(&store).await;
        store.set_dual(1);
        assert!(matches!(
            execute_error(&store, id, store.now()).await,
            AppError::MoneyForbidden("withdrawal risk tightened after approval")
        ));

        let store = FakeWithdrawStore::new();
        let id = approved_row(&store).await;
        store.set_dest_distinct_users(&dest_a(), 2);
        assert!(matches!(
            execute_error(&store, id, store.now()).await,
            AppError::MoneyForbidden("withdrawal risk tightened after approval")
        ));

        let store = FakeWithdrawStore::new();
        let id = approved_row(&store).await;
        store.mark_refund_dest(&dest_a());
        assert!(matches!(
            execute_error(&store, id, store.now()).await,
            AppError::MoneyForbidden("withdrawal risk tightened after approval")
        ));
    }

    #[tokio::test]
    async fn send_blocks_risk_that_tightened_after_approval() {
        let store = FakeWithdrawStore::new();
        let id = approved_row(&store).await;
        store.set_auto_approve(1_000_000);
        store.mark_refund_dest(&dest_a());
        let rails = FakeRails::default();
        let signer = FakeSigner::new("sig-tightened");
        let identity = test_rail();
        let err = SendWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            rails: &rails,
            signer: &signer,
            identity: &identity,
        }
        .execute(id)
        .await
        .unwrap_err();
        assert_eq!(
            err,
            AppError::MoneyForbidden("withdrawal risk tightened after approval")
        );
        assert!(store.attempts().is_empty());
    }

    #[tokio::test]
    async fn expired_w3_lookup_miss_becomes_unknown_without_rebroadcast() {
        let identity = test_rail();
        for presence in [SignaturePresence::Absent, SignaturePresence::Unknown] {
            let store = FakeWithdrawStore::new();
            let id = approved_row(&store).await;
            let rails = FakeRails::default();
            let signer = FakeSigner::new("sig-expired");
            SendWithdraw {
                store: &store,
                clock: &RequestClock(store.now()),
                rails: &rails,
                signer: &signer,
                identity: &identity,
            }
            .execute(id)
            .await
            .unwrap();
            {
                let mut tx = store.withdraw_tx().await.unwrap();
                let mut row = tx.withdrawal_for_update(id).await.unwrap();
                row.combo = Combo::W3;
                tx.cas_withdrawal(id, Combo::W6, &row).await.unwrap();
                let mut attempt = store.attempts().into_iter().next().unwrap();
                attempt.landing_state = LandingState::Prepared;
                attempt.lease_expires_at = Some(OffsetDateTime::from_unix_timestamp(1).unwrap());
                tx.save_attempt(&attempt).await.unwrap();
                tx.commit().await.unwrap();
            }
            signer.set_presence(presence);
            signer.set_finalized_height(1_001);
            let recovered = SendWithdraw {
                store: &store,
                clock: &RequestClock(store.now()),
                rails: &rails,
                signer: &signer,
                identity: &identity,
            }
            .execute(id)
            .await
            .unwrap();
            assert_eq!(recovered.combo, Combo::W7);
            assert_eq!(store.attempts().len(), 1);
        }
    }

    #[tokio::test]
    async fn unknown_rebroadcast_requires_an_unknown_attempt() {
        let store = FakeWithdrawStore::new();
        let id = approved_row(&store).await;
        let rails = FakeRails::default();
        let signer = FakeSigner::new("sig-unknown-state");
        let identity = test_rail();
        SendWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            rails: &rails,
            signer: &signer,
            identity: &identity,
        }
        .execute(id)
        .await
        .unwrap();
        crate::ports::withdraw_reconcile::ReconcileWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        }
        .mark_unknown(id)
        .await
        .unwrap();
        {
            let mut tx = store.withdraw_tx().await.unwrap();
            let mut attempt = store.attempts().into_iter().next().unwrap();
            attempt.landing_state = LandingState::Prepared;
            tx.save_attempt(&attempt).await.unwrap();
            tx.commit().await.unwrap();
        }

        let result = SendWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            rails: &rails,
            signer: &signer,
            identity: &identity,
        }
        .execute(id)
        .await;

        assert!(matches!(
            result,
            Err(AppError::Store(StoreError::Invariant(_)))
        ));
    }

    #[tokio::test]
    async fn archival_proof_replaces_expired_attempt_with_monotone_lineage() {
        let store = FakeWithdrawStore::new();
        let id = approved_row(&store).await;
        let rails = FakeRails::default();
        let first_signer = FakeSigner::new("sig-old");
        let identity = test_rail();
        SendWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            rails: &rails,
            signer: &first_signer,
            identity: &identity,
        }
        .execute(id)
        .await
        .unwrap();
        crate::ports::withdraw_reconcile::ReconcileWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        }
        .mark_unknown(id)
        .await
        .unwrap();

        let replacement_signer = FakeSigner::new("sig-new");
        replacement_signer.set_finalized_height(1_001);
        let observations = identity
            .rpc_endpoints
            .iter()
            .cloned()
            .map(|endpoint| QuorumObservation {
                endpoint,
                finalized_height: Some(1_001),
                signature_present: Some(false),
                pruned: false,
            })
            .collect::<Vec<_>>();
        let replaced = SendWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            rails: &rails,
            signer: &replacement_signer,
            identity: &identity,
        }
        .replace_expired(id, &observations)
        .await
        .unwrap();
        assert_eq!(replaced.combo, Combo::W6);
        let attempts = store.attempts();
        assert_eq!(attempts.len(), 2);
        let old = attempts
            .iter()
            .find(|attempt| attempt.signature == "sig-old")
            .unwrap();
        let new = attempts
            .iter()
            .find(|attempt| attempt.signature == "sig-new")
            .unwrap();
        assert_eq!(old.landing_state, LandingState::DefinitiveFailed);
        assert_eq!(new.attempt_number, old.attempt_number + 1);
        assert_eq!(new.replaces_attempt_id, Some(old.id));
        assert_eq!(new.landing_state, LandingState::Broadcast);
    }

    #[tokio::test]
    async fn replacement_requires_expiry_quorum_and_a_fresh_signature() {
        let store = FakeWithdrawStore::new();
        let id = approved_row(&store).await;
        let rails = FakeRails::default();
        let signer = FakeSigner::new("sig-reused");
        let identity = test_rail();
        let service = SendWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            rails: &rails,
            signer: &signer,
            identity: &identity,
        };
        assert_eq!(
            service.replace_expired(id, &[]).await.unwrap_err(),
            AppError::IllegalTransition
        );
        service.execute(id).await.unwrap();
        crate::ports::withdraw_reconcile::ReconcileWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        }
        .mark_unknown(id)
        .await
        .unwrap();

        let replayed = service.execute(id).await.unwrap();
        assert_eq!(replayed.combo, Combo::W7);
        assert_eq!(store.attempts().len(), 1);
        assert!(matches!(
            service.replace_expired(id, &[]).await,
            Err(AppError::ProposalConflict("blockhash has not expired"))
        ));

        signer.set_finalized_height(1_001);
        let stayed = service.replace_expired(id, &[]).await.unwrap();
        assert_eq!(stayed.combo, Combo::W7);
        assert!(store.attempts()[0].evidence.is_some());
        assert!(matches!(
            service.execute(id).await,
            Err(AppError::ProposalConflict(
                "blockhash expired; archival proof is required"
            ))
        ));

        let observations = identity
            .rpc_endpoints
            .iter()
            .map(|endpoint| QuorumObservation {
                endpoint: endpoint.clone(),
                finalized_height: Some(1_001),
                signature_present: Some(false),
                pruned: false,
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            service.replace_expired(id, &observations).await,
            Err(AppError::Store(StoreError::Invariant(
                "replacement signature was reused"
            )))
        ));
        assert_eq!(store.attempts().len(), 1);
    }

    #[tokio::test]
    async fn send_terminal_owner_existing_intent_and_cas_edges_are_total() {
        let identity = test_rail();

        let terminal_store = FakeWithdrawStore::new();
        let terminal_id = approved_row(&terminal_store).await;
        let rails = FakeRails::default();
        let signer = FakeSigner::new("sig-terminal-corrupt");
        let service = SendWithdraw {
            store: &terminal_store,
            clock: &RequestClock(terminal_store.now()),
            rails: &rails,
            signer: &signer,
            identity: &identity,
        };
        service.execute(terminal_id).await.unwrap();
        let mut tx = terminal_store.withdraw_tx().await.unwrap();
        let mut attempt = terminal_store.attempts().into_iter().next().unwrap();
        attempt.landing_state = LandingState::Prepared;
        tx.save_attempt(&attempt).await.unwrap();
        tx.commit().await.unwrap();
        assert!(matches!(
            service.execute(terminal_id).await,
            Err(AppError::Store(StoreError::Invariant(_)))
        ));

        let illegal_store = FakeWithdrawStore::new();
        let illegal_id = approved_row(&illegal_store).await;
        let mut tx = illegal_store.withdraw_tx().await.unwrap();
        let mut row = tx.withdrawal_for_update(illegal_id).await.unwrap();
        row.combo = Combo::W4;
        tx.cas_withdrawal(illegal_id, Combo::W2, &row)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            SendWithdraw {
                store: &illegal_store,
                clock: &RequestClock(illegal_store.now()),
                rails: &FakeRails::default(),
                signer: &FakeSigner::new("unused"),
                identity: &identity,
            }
            .execute(illegal_id)
            .await
            .unwrap_err(),
            AppError::IllegalTransition
        );

        let owner_store = FakeWithdrawStore::new();
        let owner_id = approved_row(&owner_store).await;
        let wrong = owner_store.seed_user(UserStatus::Active, 2);
        owner_store.override_withdrawal_user(owner_id, wrong);
        let err = SendWithdraw {
            store: &owner_store,
            clock: &RequestClock(owner_store.now()),
            rails: &FakeRails::default(),
            signer: &FakeSigner::new("unused"),
            identity: &identity,
        }
        .execute(owner_id)
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::Store(StoreError::Invariant(_))));

        let existing_store = FakeWithdrawStore::new();
        let existing_id = approved_row(&existing_store).await;
        let row = existing_store.withdrawals()[0].clone();
        let payment = crate::ports::outbound::payment_for(
            OutboundSubject::Withdrawal,
            existing_id.0,
            row.dest.clone(),
            row.amount_micro,
            &identity,
        );
        let mut tx = existing_store.withdraw_tx().await.unwrap();
        tx.insert_outbound_payment(&payment).await.unwrap();
        tx.commit().await.unwrap();
        let sent = SendWithdraw {
            store: &existing_store,
            clock: &RequestClock(existing_store.now()),
            rails: &FakeRails::default(),
            signer: &FakeSigner::new("sig-existing-payment"),
            identity: &identity,
        }
        .execute(existing_id)
        .await
        .unwrap();
        assert_eq!(sent.combo, Combo::W6);

        let attempt_store = FakeWithdrawStore::new();
        let attempt_id = approved_row(&attempt_store).await;
        let row = attempt_store.withdrawals()[0].clone();
        let payment = crate::ports::outbound::payment_for(
            OutboundSubject::Withdrawal,
            attempt_id.0,
            row.dest,
            row.amount_micro,
            &identity,
        );
        let mut tx = attempt_store.withdraw_tx().await.unwrap();
        tx.insert_outbound_payment(&payment).await.unwrap();
        tx.insert_attempt(&OutboundAttemptRow {
            id: Uuid::new_v4(),
            payment_id: payment.id,
            attempt_number: 1,
            replaces_attempt_id: None,
            signed_tx_bytes: vec![1],
            signature: "preexisting".into(),
            last_valid_block_height: 1,
            landing_state: LandingState::Prepared,
            lease_expires_at: Some(attempt_store.now()),
            evidence: None,
        })
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let err = SendWithdraw {
            store: &attempt_store,
            clock: &RequestClock(attempt_store.now()),
            rails: &FakeRails::default(),
            signer: &FakeSigner::new("unused"),
            identity: &identity,
        }
        .execute(attempt_id)
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::Store(StoreError::Invariant(_))));

        let claim_cas_store = FakeWithdrawStore::new();
        let claim_cas_id = approved_row(&claim_cas_store).await;
        claim_cas_store.force_next_cas_miss();
        assert_eq!(
            SendWithdraw {
                store: &claim_cas_store,
                clock: &RequestClock(claim_cas_store.now()),
                rails: &FakeRails::default(),
                signer: &FakeSigner::new("sig-claim-cas"),
                identity: &identity,
            }
            .execute(claim_cas_id)
            .await
            .unwrap_err(),
            AppError::IllegalTransition
        );
    }

    #[tokio::test]
    async fn replacement_and_broadcast_cas_failure_paths_remain_durable() {
        let identity = test_rail();
        let store = FakeWithdrawStore::new();
        let id = approved_row(&store).await;
        let rails = FakeRails::default();
        let signer = FakeSigner::new("sig-old-failure");
        SendWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            rails: &rails,
            signer: &signer,
            identity: &identity,
        }
        .execute(id)
        .await
        .unwrap();
        crate::ports::withdraw_reconcile::ReconcileWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        }
        .mark_unknown(id)
        .await
        .unwrap();

        let mut tx = store.withdraw_tx().await.unwrap();
        let mut attempt = store.attempts().into_iter().next().unwrap();
        attempt.landing_state = LandingState::Prepared;
        tx.save_attempt(&attempt).await.unwrap();
        tx.commit().await.unwrap();
        let wrong_state = SendWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            rails: &rails,
            signer: &signer,
            identity: &identity,
        }
        .replace_expired(id, &[])
        .await
        .unwrap_err();
        assert!(matches!(
            wrong_state,
            AppError::Store(StoreError::Invariant(_))
        ));

        let mut tx = store.withdraw_tx().await.unwrap();
        let mut attempt = store.attempts().into_iter().next().unwrap();
        attempt.landing_state = LandingState::Unknown;
        tx.save_attempt(&attempt).await.unwrap();
        tx.commit().await.unwrap();
        let replacement = FakeSigner::new("sig-replacement-fail");
        replacement.set_finalized_height(1_001);
        let observations = identity
            .rpc_endpoints
            .iter()
            .map(|endpoint| QuorumObservation {
                endpoint: endpoint.clone(),
                finalized_height: Some(1_001),
                signature_present: Some(false),
                pruned: false,
            })
            .collect::<Vec<_>>();
        *rails.fail_broadcast.lock() = true;
        let err = SendWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            rails: &rails,
            signer: &replacement,
            identity: &identity,
        }
        .replace_expired(id, &observations)
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::Store(StoreError::Backend(_))));
        assert_eq!(store.withdrawals()[0].combo, Combo::W8);

        let cas_store = FakeWithdrawStore::new();
        let cas_id = approved_row(&cas_store).await;
        let cas_rails = FakeRails::default();
        *cas_rails.fail_broadcast.lock() = true;
        let cas_signer = FakeSigner::new("sig-broadcast-cas-miss");
        let service = SendWithdraw {
            store: &cas_store,
            clock: &RequestClock(cas_store.now()),
            rails: &cas_rails,
            signer: &cas_signer,
            identity: &identity,
        };
        assert!(service.execute(cas_id).await.is_err());
        cas_store.force_next_cas_miss();
        assert_eq!(
            service.cas_broadcast(cas_id, Combo::W3).await.unwrap_err(),
            AppError::IllegalTransition
        );
        assert_eq!(cas_store.withdrawals()[0].combo, Combo::W3);

        let moved = service.cas_broadcast(cas_id, Combo::W8).await.unwrap();
        assert_eq!(moved.combo, Combo::W3);
    }
}
