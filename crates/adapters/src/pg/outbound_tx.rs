//! Outbound payment / attempt SQL helpers used by the W1 withdraw tx.

use application::error::StoreError;
use application::ports::{LandingState, OutboundAttemptRow, OutboundPaymentRow, OutboundSubject};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use super::rows::db_error;

pub(super) fn subject_name(subject: OutboundSubject) -> &'static str {
    subject.as_str()
}

pub(super) fn parse_subject(value: &str) -> Result<OutboundSubject, StoreError> {
    match value {
        "withdrawal" => Ok(OutboundSubject::Withdrawal),
        "deposit_refund" => Ok(OutboundSubject::DepositRefund),
        _ => Err(StoreError::Invariant("unknown outbound subject")),
    }
}

pub(super) fn parse_landing(value: &str) -> Result<LandingState, StoreError> {
    match value {
        "prepared" => Ok(LandingState::Prepared),
        "broadcast" => Ok(LandingState::Broadcast),
        "unknown" => Ok(LandingState::Unknown),
        "finalized" => Ok(LandingState::Finalized),
        "definitive_failed" => Ok(LandingState::DefinitiveFailed),
        _ => Err(StoreError::Invariant("unknown landing state")),
    }
}

/// Insert an outbound payment.
///
/// # Errors
/// Uniqueness / backend.
pub async fn insert_payment(
    conn: &mut PgConnection,
    payment: &OutboundPaymentRow,
) -> Result<(), StoreError> {
    sqlx::query(
        r"
        insert into outbound_payments
            (id, subject, subject_id, dest, amount_micro, rail_fingerprint)
        values ($1, $2, $3, $4, $5, $6)
        ",
    )
    .bind(payment.id)
    .bind(subject_name(payment.subject))
    .bind(payment.subject_id)
    .bind(&payment.dest)
    .bind(payment.amount_micro)
    .bind(&payment.rail_fingerprint)
    .execute(conn)
    .await
    .map_err(db_error)?;
    Ok(())
}

/// Fetch by subject.
///
/// # Errors
/// Backend.
pub async fn payment_by_subject(
    conn: &mut PgConnection,
    subject: OutboundSubject,
    subject_id: Uuid,
) -> Result<Option<OutboundPaymentRow>, StoreError> {
    let row = sqlx::query_as::<_, (Uuid, String, Uuid, String, i64, String)>(
        r"
        select id, subject, subject_id, dest, amount_micro, rail_fingerprint
          from outbound_payments
         where subject = $1 and subject_id = $2
        ",
    )
    .bind(subject_name(subject))
    .bind(subject_id)
    .fetch_optional(conn)
    .await
    .map_err(db_error)?;
    row.map(
        |(id, subject, subject_id, dest, amount_micro, rail_fingerprint)| {
            Ok(OutboundPaymentRow {
                id,
                subject: parse_subject(&subject)?,
                subject_id,
                dest,
                amount_micro,
                rail_fingerprint,
            })
        },
    )
    .transpose()
}

/// Insert an attempt.
///
/// # Errors
/// Backend / uniqueness.
pub async fn insert_attempt(
    conn: &mut PgConnection,
    attempt: &OutboundAttemptRow,
) -> Result<(), StoreError> {
    sqlx::query("select id from outbound_payments where id = $1 for update")
        .bind(attempt.payment_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(db_error)?
        .ok_or(StoreError::NotFound("outbound payment"))?;
    let prior = sqlx::query(
        r"select id, attempt_number, landing_state
              from outbound_send_attempts
             where payment_id = $1
             order by attempt_number",
    )
    .bind(attempt.payment_id)
    .fetch_all(&mut *conn)
    .await
    .map_err(db_error)?;
    let expected_number = prior.last().map_or(1, |row| {
        row.get::<i32, _>("attempt_number").saturating_add(1)
    });
    let previous = prior
        .last()
        .map(|row| {
            Ok::<_, StoreError>((
                row.try_get::<Uuid, _>("id").map_err(db_error)?,
                row.try_get::<String, _>("landing_state")
                    .map_err(db_error)?,
            ))
        })
        .transpose()?;
    validate_attempt_lineage(
        attempt,
        expected_number,
        previous.as_ref().map(|(id, state)| (*id, state.as_str())),
    )?;
    validate_prepared_shape(attempt)?;
    let signature_exists: bool = sqlx::query_scalar(
        "select exists(select 1 from outbound_send_attempts where signature = $1)",
    )
    .bind(&attempt.signature)
    .fetch_one(&mut *conn)
    .await
    .map_err(db_error)?;
    ensure_signature_available(signature_exists)?;
    sqlx::query(
        r"
        insert into outbound_send_attempts
            (id, payment_id, attempt_number, replaces_attempt_id, signed_tx_bytes,
             signature, last_valid_block_height, landing_state, lease_expires_at, evidence)
        values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
        ",
    )
    .bind(attempt.id)
    .bind(attempt.payment_id)
    .bind(attempt.attempt_number)
    .bind(attempt.replaces_attempt_id)
    .bind(&attempt.signed_tx_bytes)
    .bind(&attempt.signature)
    .bind(attempt.last_valid_block_height)
    .bind(attempt.landing_state.as_str())
    .bind(attempt.lease_expires_at)
    .bind(attempt.evidence.clone())
    .execute(conn)
    .await
    .map_err(db_error)?;
    Ok(())
}

/// Validate and persist the mutable attempt state while its payment row is locked.
///
/// # Errors
/// Missing row, immutable-field drift, illegal state edge, or competing live/finalized attempt.
pub async fn save_attempt(
    conn: &mut PgConnection,
    attempt: &OutboundAttemptRow,
) -> Result<(), StoreError> {
    sqlx::query("select id from outbound_payments where id = $1 for update")
        .bind(attempt.payment_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(db_error)?
        .ok_or(StoreError::NotFound("outbound payment"))?;
    let current = sqlx::query("select * from outbound_send_attempts where id = $1 for update")
        .bind(attempt.id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(db_error)?
        .ok_or(StoreError::NotFound("outbound attempt"))?;
    let stored = StoredAttemptIdentity {
        payment_id: current.try_get("payment_id").map_err(db_error)?,
        attempt_number: current.try_get("attempt_number").map_err(db_error)?,
        replaces_attempt_id: current.try_get("replaces_attempt_id").map_err(db_error)?,
        signed_tx_bytes: current.try_get("signed_tx_bytes").map_err(db_error)?,
        signature: current.try_get("signature").map_err(db_error)?,
        last_valid_block_height: current
            .try_get("last_valid_block_height")
            .map_err(db_error)?,
        landing_state: current.try_get("landing_state").map_err(db_error)?,
    };
    validate_attempt_update(attempt, &stored)?;
    let other_live = if attempt.landing_state.is_live() {
        sqlx::query_scalar(
            r"select exists(select 1 from outbound_send_attempts
                 where payment_id = $1 and id <> $2
                   and landing_state in ('prepared','broadcast','unknown'))",
        )
        .bind(attempt.payment_id)
        .bind(attempt.id)
        .fetch_one(&mut *conn)
        .await
        .map_err(db_error)?
    } else {
        false
    };
    let other_finalized = if attempt.landing_state == LandingState::Finalized {
        sqlx::query_scalar(
            r"select exists(select 1 from outbound_send_attempts
                 where payment_id = $1 and id <> $2 and landing_state = 'finalized')",
        )
        .bind(attempt.payment_id)
        .bind(attempt.id)
        .fetch_one(&mut *conn)
        .await
        .map_err(db_error)?
    } else {
        false
    };
    validate_competing_attempts(attempt.landing_state, other_live, other_finalized)?;
    let saved = sqlx::query(
        r"update outbound_send_attempts
              set landing_state = $2, evidence = $3, lease_expires_at = $4
            where id = $1",
    )
    .bind(attempt.id)
    .bind(attempt.landing_state.as_str())
    .bind(attempt.evidence.clone())
    .bind(attempt.lease_expires_at)
    .execute(&mut *conn)
    .await
    .map_err(db_error)?;
    ensure_single_update(saved.rows_affected())?;
    Ok(())
}

fn validate_attempt_lineage(
    attempt: &OutboundAttemptRow,
    expected_number: i32,
    previous: Option<(Uuid, &str)>,
) -> Result<(), StoreError> {
    if attempt.attempt_number != expected_number {
        return Err(StoreError::Invariant(
            "outbound attempt number is not monotone",
        ));
    }
    match previous {
        None if attempt.replaces_attempt_id.is_some() => Err(StoreError::Invariant(
            "first outbound attempt has replacement lineage",
        )),
        Some((previous_id, previous_state)) => {
            if attempt.replaces_attempt_id != Some(previous_id)
                || parse_landing(previous_state)? != LandingState::DefinitiveFailed
            {
                return Err(StoreError::Invariant(
                    "outbound replacement lineage is invalid",
                ));
            }
            Ok(())
        }
        None => Ok(()),
    }
}

fn validate_prepared_shape(attempt: &OutboundAttemptRow) -> Result<(), StoreError> {
    if attempt.signed_tx_bytes.is_empty()
        || attempt.signature.trim().is_empty()
        || attempt.last_valid_block_height <= 0
        || attempt.landing_state != LandingState::Prepared
        || attempt.lease_expires_at.is_none()
    {
        return Err(StoreError::Invariant("outbound prepared attempt shape"));
    }
    Ok(())
}

fn ensure_signature_available(exists: bool) -> Result<(), StoreError> {
    if exists {
        return Err(StoreError::Conflict("outbound signature"));
    }
    Ok(())
}

struct StoredAttemptIdentity {
    payment_id: Uuid,
    attempt_number: i32,
    replaces_attempt_id: Option<Uuid>,
    signed_tx_bytes: Vec<u8>,
    signature: String,
    last_valid_block_height: i64,
    landing_state: String,
}

fn validate_attempt_update(
    attempt: &OutboundAttemptRow,
    current: &StoredAttemptIdentity,
) -> Result<(), StoreError> {
    if current.payment_id != attempt.payment_id
        || current.attempt_number != attempt.attempt_number
        || current.replaces_attempt_id != attempt.replaces_attempt_id
        || current.signed_tx_bytes != attempt.signed_tx_bytes
        || current.signature != attempt.signature
        || current.last_valid_block_height != attempt.last_valid_block_height
    {
        return Err(StoreError::Invariant(
            "outbound attempt immutable fields changed",
        ));
    }
    let current_state = parse_landing(&current.landing_state)?;
    if !legal_attempt_edge(current_state, attempt.landing_state) {
        return Err(StoreError::Invariant("illegal outbound attempt transition"));
    }
    Ok(())
}

fn validate_competing_attempts(
    state: LandingState,
    other_live: bool,
    other_finalized: bool,
) -> Result<(), StoreError> {
    if state.is_live() && other_live {
        return Err(StoreError::Invariant("multiple live outbound attempts"));
    }
    if state == LandingState::Finalized && other_finalized {
        return Err(StoreError::Invariant(
            "multiple finalized outbound attempts",
        ));
    }
    Ok(())
}

fn ensure_single_update(rows_affected: u64) -> Result<(), StoreError> {
    if rows_affected != 1 {
        return Err(StoreError::Conflict("outbound attempt changed"));
    }
    Ok(())
}

fn legal_attempt_edge(from: LandingState, to: LandingState) -> bool {
    from == to
        || matches!(
            (from, to),
            (
                LandingState::Prepared,
                LandingState::Broadcast
                    | LandingState::Unknown
                    | LandingState::Finalized
                    | LandingState::DefinitiveFailed
            ) | (
                LandingState::Broadcast,
                LandingState::Unknown | LandingState::Finalized
            ) | (
                LandingState::Unknown,
                LandingState::Finalized | LandingState::DefinitiveFailed
            )
        )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn names_round_trip() {
        assert_eq!(subject_name(OutboundSubject::Withdrawal), "withdrawal");
        assert_eq!(
            subject_name(OutboundSubject::DepositRefund),
            "deposit_refund"
        );
        assert_eq!(
            parse_subject("withdrawal").unwrap(),
            OutboundSubject::Withdrawal
        );
        assert_eq!(
            parse_subject("deposit_refund").unwrap(),
            OutboundSubject::DepositRefund
        );
        for (name, state) in [
            ("prepared", LandingState::Prepared),
            ("broadcast", LandingState::Broadcast),
            ("unknown", LandingState::Unknown),
            ("finalized", LandingState::Finalized),
            ("definitive_failed", LandingState::DefinitiveFailed),
        ] {
            assert_eq!(parse_landing(name).unwrap(), state);
        }
        assert!(parse_subject("nope").is_err());
        assert!(parse_landing("nope").is_err());
    }

    fn attempt() -> OutboundAttemptRow {
        OutboundAttemptRow {
            id: Uuid::new_v4(),
            payment_id: Uuid::new_v4(),
            attempt_number: 1,
            replaces_attempt_id: None,
            signed_tx_bytes: vec![1],
            signature: "sig".into(),
            last_valid_block_height: 10,
            landing_state: LandingState::Prepared,
            lease_expires_at: Some(time::OffsetDateTime::UNIX_EPOCH),
            evidence: None,
        }
    }

    fn stored(attempt: &OutboundAttemptRow, landing_state: &str) -> StoredAttemptIdentity {
        StoredAttemptIdentity {
            payment_id: attempt.payment_id,
            attempt_number: attempt.attempt_number,
            replaces_attempt_id: attempt.replaces_attempt_id,
            signed_tx_bytes: attempt.signed_tx_bytes.clone(),
            signature: attempt.signature.clone(),
            last_valid_block_height: attempt.last_valid_block_height,
            landing_state: landing_state.into(),
        }
    }

    #[test]
    fn attempt_validation_rejects_every_corrupt_shape() {
        let first = attempt();
        assert!(validate_attempt_lineage(&first, 1, None).is_ok());
        assert!(validate_attempt_lineage(&first, 2, None).is_err());

        let mut replacement = attempt();
        replacement.replaces_attempt_id = Some(Uuid::new_v4());
        assert!(validate_attempt_lineage(&replacement, 1, None).is_err());
        let prior = replacement.replaces_attempt_id.unwrap();
        replacement.attempt_number = 2;
        assert!(
            validate_attempt_lineage(&replacement, 2, Some((prior, "definitive_failed"))).is_ok()
        );
        assert!(
            validate_attempt_lineage(&replacement, 2, Some((Uuid::new_v4(), "prepared"))).is_err()
        );
        assert!(validate_attempt_lineage(&replacement, 2, Some((prior, "corrupt"))).is_err());

        assert!(validate_prepared_shape(&first).is_ok());
        for malformed in [
            OutboundAttemptRow {
                signed_tx_bytes: Vec::new(),
                ..first.clone()
            },
            OutboundAttemptRow {
                signature: " ".into(),
                ..first.clone()
            },
            OutboundAttemptRow {
                last_valid_block_height: 0,
                ..first.clone()
            },
            OutboundAttemptRow {
                landing_state: LandingState::Broadcast,
                ..first.clone()
            },
            OutboundAttemptRow {
                lease_expires_at: None,
                ..first.clone()
            },
        ] {
            assert!(validate_prepared_shape(&malformed).is_err());
        }
        assert!(ensure_signature_available(false).is_ok());
        assert!(ensure_signature_available(true).is_err());
    }

    #[test]
    fn attempt_update_validation_covers_all_edges_and_competition() {
        let mut next = attempt();
        assert!(validate_attempt_update(&next, &stored(&next, "prepared")).is_ok());
        let mut immutable_drift = stored(&next, "prepared");
        immutable_drift.signature = "changed".into();
        assert!(validate_attempt_update(&next, &immutable_drift).is_err());
        assert!(validate_attempt_update(&next, &stored(&next, "corrupt")).is_err());

        next.landing_state = LandingState::DefinitiveFailed;
        assert!(validate_attempt_update(&next, &stored(&next, "broadcast")).is_err());
        for (from, to) in [
            (LandingState::Prepared, LandingState::Broadcast),
            (LandingState::Prepared, LandingState::Unknown),
            (LandingState::Prepared, LandingState::Finalized),
            (LandingState::Prepared, LandingState::DefinitiveFailed),
            (LandingState::Broadcast, LandingState::Unknown),
            (LandingState::Broadcast, LandingState::Finalized),
            (LandingState::Unknown, LandingState::Finalized),
            (LandingState::Unknown, LandingState::DefinitiveFailed),
        ] {
            assert!(legal_attempt_edge(from, to));
        }
        assert!(legal_attempt_edge(
            LandingState::Finalized,
            LandingState::Finalized
        ));
        assert!(!legal_attempt_edge(
            LandingState::Finalized,
            LandingState::Broadcast
        ));

        assert!(validate_competing_attempts(LandingState::Prepared, false, false).is_ok());
        assert!(validate_competing_attempts(LandingState::Prepared, true, false).is_err());
        assert!(validate_competing_attempts(LandingState::Finalized, false, true).is_err());
        assert!(ensure_single_update(1).is_ok());
        assert!(ensure_single_update(0).is_err());
    }
}
