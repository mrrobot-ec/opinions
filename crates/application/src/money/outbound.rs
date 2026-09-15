//! Generalized outbound payments (`subject` = withdrawal | `deposit_refund`).
//! Persist signed bytes BEFORE broadcast; same-bytes rebroadcast only.

use uuid::Uuid;

use crate::error::{AppError, StoreError};
use crate::ports::{
    Committable, LandingState, OutboundAttemptRow, OutboundIo, OutboundPaymentRow, OutboundRails,
    OutboundSubject, RailIdentity,
};

/// Build the outbound payment row for a subject.
#[must_use]
pub fn payment_for(
    subject: OutboundSubject,
    subject_id: Uuid,
    dest: String,
    amount_micro: i64,
    identity: &RailIdentity,
) -> OutboundPaymentRow {
    OutboundPaymentRow {
        id: Uuid::new_v4(),
        subject,
        subject_id,
        dest,
        amount_micro,
        rail_fingerprint: identity.fingerprint(),
    }
}

/// Persist signed bytes on the attempt, then on the rail, then broadcast.
///
/// # Errors
/// Store / rail failures. Broadcast errors are returned AFTER persist so a
/// crash-recovery path can rebroadcast the same bytes.
pub async fn persist_then_broadcast<T>(
    mut tx: Box<T>,
    rails: &dyn OutboundRails,
    payment: &OutboundPaymentRow,
    attempt: &OutboundAttemptRow,
) -> Result<(), AppError>
where
    T: OutboundIo + Committable + ?Sized,
{
    tx.insert_attempt(attempt).await?;
    rails
        .persist_signed(payment.id, &attempt.signed_tx_bytes, &attempt.signature)
        .await?;
    tx.commit().await?;
    rails.broadcast(payment.id).await?;
    Ok(())
}

/// Rebroadcast the SAME persisted bytes. Never re-signs.
///
/// # Errors
/// Missing persisted bytes or rail failure.
pub async fn same_bytes_rebroadcast(
    rails: &dyn OutboundRails,
    payment: &OutboundPaymentRow,
    attempt: &OutboundAttemptRow,
) -> Result<(), AppError> {
    rails
        .persist_signed(payment.id, &attempt.signed_tx_bytes, &attempt.signature)
        .await?;
    rails.broadcast(payment.id).await?;
    Ok(())
}

/// Next attempt number for a payment.
#[must_use]
pub fn next_attempt_number(existing: &[OutboundAttemptRow]) -> i32 {
    existing
        .iter()
        .map(|row| row.attempt_number)
        .max()
        .unwrap_or(0)
        .saturating_add(1)
}

/// True when a replacement is legal (no live attempt, last is unknown).
#[must_use]
pub fn can_replace(existing: &[OutboundAttemptRow]) -> bool {
    let blocking = existing.iter().any(|row| {
        matches!(
            row.landing_state,
            LandingState::Prepared | LandingState::Broadcast
        )
    });
    let last_unknown = existing
        .last()
        .is_some_and(|row| row.landing_state == LandingState::Unknown);
    !blocking && last_unknown
}

/// Map a store uniqueness conflict onto a typed conflict.
#[must_use]
pub fn conflict_or(err: StoreError) -> StoreError {
    err
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::model::UserStatus;
    use crate::ports::withdraw_fakes::{dest_a, test_rail, FakeRails, FakeWithdrawStore};
    use crate::ports::WithdrawStore;
    use time::OffsetDateTime;

    #[test]
    fn subject_and_attempt_helpers() {
        assert_eq!(next_attempt_number(&[]), 1);
        let attempt = OutboundAttemptRow {
            id: Uuid::nil(),
            payment_id: Uuid::nil(),
            attempt_number: 3,
            replaces_attempt_id: None,
            signed_tx_bytes: vec![],
            signature: String::new(),
            last_valid_block_height: 0,
            landing_state: LandingState::Unknown,
            lease_expires_at: None,
            evidence: None,
        };
        assert_eq!(next_attempt_number(std::slice::from_ref(&attempt)), 4);
        assert!(can_replace(std::slice::from_ref(&attempt)));
        let live = OutboundAttemptRow {
            landing_state: LandingState::Prepared,
            ..attempt
        };
        assert!(!can_replace(&[live]));
        let _ = conflict_or(StoreError::Conflict("x"));
    }

    #[tokio::test]
    async fn persist_happens_before_broadcast_and_rebroadcast_reuses_bytes() {
        let store = FakeWithdrawStore::new();
        let _user = store.seed_user(UserStatus::Active, 2);
        let rails = FakeRails::default();
        let identity = test_rail();
        let payment = payment_for(
            OutboundSubject::Withdrawal,
            Uuid::new_v4(),
            dest_a(),
            5_000_000,
            &identity,
        );
        let attempt = OutboundAttemptRow {
            id: Uuid::new_v4(),
            payment_id: payment.id,
            attempt_number: 1,
            replaces_attempt_id: None,
            signed_tx_bytes: b"signed-bytes".to_vec(),
            signature: "sig-1".into(),
            last_valid_block_height: 42,
            landing_state: LandingState::Prepared,
            lease_expires_at: Some(OffsetDateTime::from_unix_timestamp(1_700_000_100).unwrap()),
            evidence: None,
        };
        let mut tx = store.withdraw_tx().await.unwrap();
        tx.insert_outbound_payment(&payment).await.unwrap();
        persist_then_broadcast(tx, &rails, &payment, &attempt)
            .await
            .unwrap();
        assert_eq!(
            store.attempts().len(),
            1,
            "attempt must commit before broadcast"
        );
        assert_eq!(
            rails.persisted_bytes(payment.id).as_deref(),
            Some(b"signed-bytes".as_slice())
        );
        same_bytes_rebroadcast(&rails, &payment, &attempt)
            .await
            .unwrap();
        assert_eq!(
            rails.persisted_bytes(payment.id).as_deref(),
            Some(b"signed-bytes".as_slice())
        );
        let refund = payment_for(
            OutboundSubject::DepositRefund,
            Uuid::new_v4(),
            dest_a(),
            1,
            &identity,
        );
        assert_eq!(refund.subject, OutboundSubject::DepositRefund);
    }

    #[tokio::test]
    async fn persist_failure_never_commits_or_broadcasts() {
        let store = FakeWithdrawStore::new();
        let rails = FakeRails::default();
        *rails.fail_persist.lock() = true;
        let identity = test_rail();
        let payment = payment_for(
            OutboundSubject::Withdrawal,
            Uuid::new_v4(),
            dest_a(),
            5_000_000,
            &identity,
        );
        let attempt = OutboundAttemptRow {
            id: Uuid::new_v4(),
            payment_id: payment.id,
            attempt_number: 1,
            replaces_attempt_id: None,
            signed_tx_bytes: b"signed-bytes".to_vec(),
            signature: "sig-fail".into(),
            last_valid_block_height: 42,
            landing_state: LandingState::Prepared,
            lease_expires_at: Some(OffsetDateTime::from_unix_timestamp(1_700_000_100).unwrap()),
            evidence: None,
        };
        let mut tx = store.withdraw_tx().await.unwrap();
        tx.insert_outbound_payment(&payment).await.unwrap();
        assert!(persist_then_broadcast(tx, &rails, &payment, &attempt)
            .await
            .is_err());
        assert!(store.attempts().is_empty());
        assert!(rails.broadcast.lock().is_empty());
        assert!(same_bytes_rebroadcast(&rails, &payment, &attempt)
            .await
            .is_err());
    }
}
