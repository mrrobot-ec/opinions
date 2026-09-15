//! Phone possession-proof: normalized HMAC, one active challenge, atomic
//! attempts, unique verified number (D32 / D33 bind).

use hmac::{Hmac, Mac};
use sha2::Sha256;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{AppError, StoreError};
use crate::model::UserId;
use crate::ports::PhoneVerification;

use super::admin::{hex_encode, ComplianceAdminStore};

/// Default phone-challenge TTL.
pub const PHONE_CHALLENGE_TTL: time::Duration = time::Duration::minutes(10);

pub const PHONE_MAX_ATTEMPTS: i32 = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhoneVerificationRow {
    pub id: Uuid,
    pub user: UserId,
    pub number_hmac: String,
    pub hmac_key_version: i32,
    pub challenge: Option<String>,
    pub expires_at: Option<OffsetDateTime>,
    pub attempts: i32,
    pub verified_at: Option<OffsetDateTime>,
    pub provider_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhoneChallenge {
    pub row: PhoneVerificationRow,
    pub code: String,
}

/// Normalize to E.164 (`+` + 8..=15 digits).
///
/// # Errors
/// Missing plus, non-digits, or length.
pub fn normalize_e164(raw: &str) -> Result<String, AppError> {
    let compact: String = raw
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    let Some(rest) = compact.strip_prefix('+') else {
        return Err(AppError::AdminForbidden("phone must be E.164"));
    };
    if rest.len() < 8 || rest.len() > 15 || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return Err(AppError::AdminForbidden("phone must be E.164"));
    }
    Ok(compact)
}

/// `HMAC-SHA256(secret, e164 || ':' || key_version)` hex. Key-version is
/// mixed in so rotation does not collide with a prior digest.
///
/// # Errors
/// Empty secret.
pub fn hmac_e164(secret: &[u8], key_version: i32, e164: &str) -> Result<String, AppError> {
    if secret.is_empty() {
        return Err(AppError::AdminForbidden("phone hmac secret is required"));
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(secret)
        .map_err(|_| AppError::AdminForbidden("phone hmac secret is required"))?;
    mac.update(e164.as_bytes());
    mac.update(b":");
    mac.update(key_version.to_string().as_bytes());
    Ok(hex_encode(&mac.finalize().into_bytes()))
}

fn code_digest(secret: &[u8], code: &str) -> Result<String, AppError> {
    if secret.is_empty() {
        return Err(AppError::AdminForbidden("phone hmac secret is required"));
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(secret)
        .map_err(|_| AppError::AdminForbidden("phone hmac secret is required"))?;
    mac.update(b"code:");
    mac.update(code.as_bytes());
    Ok(hex_encode(&mac.finalize().into_bytes()))
}

/// Start a challenge: one active per account, unique number HMAC.
///
/// # Errors
/// Normalization, uniqueness, store, provider.
#[allow(clippy::too_many_arguments)]
pub async fn start_challenge(
    store: &impl ComplianceAdminStore,
    provider: &dyn PhoneVerification,
    user: UserId,
    raw_e164: &str,
    secret: &[u8],
    key_version: i32,
    code: &str,
    now: OffsetDateTime,
) -> Result<PhoneVerificationRow, AppError> {
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err(AppError::AdminForbidden("challenge code must be 6 digits"));
    }
    let e164 = normalize_e164(raw_e164)?;
    let number_hmac = hmac_e164(secret, key_version, &e164)?;
    let challenge = code_digest(secret, code)?;
    let mut tx = store.admin_tx().await?;
    tx.lock_user(user).await?;
    if let Some(active) = tx.active_phone_challenge(user).await? {
        if active.verified_at.is_none() && active.expires_at.is_some_and(|expires| expires > now) {
            return Err(AppError::Store(StoreError::Conflict(
                "one active phone challenge per account",
            )));
        }
    }
    if let Some(existing) = tx.phone_by_hmac(&number_hmac, key_version).await? {
        if existing.user != user {
            return Err(AppError::Store(StoreError::Conflict(
                "phone number already bound",
            )));
        }
        if existing.verified_at.is_some() {
            tx.commit().await?;
            return Ok(existing);
        }
        // Same account, same number, previous code expired: the uniqueness
        // row IS the binding, so refresh it in place (attempts reset) rather
        // than inserting a second row the unique index would reject —
        // otherwise one expired code would lock the number out forever.
        let row = tx
            .refresh_phone_challenge(existing.id, &challenge, now + PHONE_CHALLENGE_TTL)
            .await?;
        tx.commit().await?;
        provider
            .start_challenge(user, &e164)
            .await
            .map_err(AppError::Store)?;
        return Ok(row);
    }
    let row = PhoneVerificationRow {
        id: Uuid::new_v4(),
        user,
        number_hmac,
        hmac_key_version: key_version,
        challenge: Some(challenge),
        expires_at: Some(now + PHONE_CHALLENGE_TTL),
        attempts: 0,
        verified_at: None,
        provider_ref: None,
    };
    let row = tx.insert_phone_challenge(row).await?;
    tx.commit().await?;
    provider
        .start_challenge(user, &e164)
        .await
        .map_err(AppError::Store)?;
    Ok(row)
}

/// Verify a code with atomic attempt consumption.
///
/// # Errors
/// Exhausted / expired / mismatch / store.
pub async fn verify_challenge(
    store: &impl ComplianceAdminStore,
    provider: &dyn PhoneVerification,
    user: UserId,
    code: &str,
    secret: &[u8],
    now: OffsetDateTime,
) -> Result<PhoneVerificationRow, AppError> {
    let mut tx = store.admin_tx().await?;
    let Some(active) = tx.active_phone_challenge(user).await? else {
        return Err(AppError::Store(StoreError::NotFound("phone challenge")));
    };
    if active.verified_at.is_some() {
        tx.commit().await?;
        return Ok(active);
    }
    if active.expires_at.is_some_and(|expires| expires <= now) {
        return Err(AppError::Store(StoreError::Conflict(
            "phone challenge expired",
        )));
    }
    let consumed = tx.consume_phone_attempt(active.id).await?;
    let expected = active
        .challenge
        .as_deref()
        .ok_or(AppError::Store(StoreError::Invariant("missing challenge")))?;
    let presented = code_digest(secret, code)?;
    let provider_ok = provider.verify(user, code).await.map_err(AppError::Store)?;
    if presented != expected || !provider_ok {
        tx.commit().await?;
        return Err(AppError::Store(StoreError::Conflict(
            "phone challenge mismatch",
        )));
    }
    let verified = tx.mark_phone_verified(consumed.id, now).await?;
    tx.commit().await?;
    Ok(verified)
}

/// Grant-eligible iff a verified row exists for the user.
///
/// # Errors
/// Store failures.
pub async fn is_grant_eligible(
    store: &impl ComplianceAdminStore,
    user: UserId,
) -> Result<bool, StoreError> {
    let mut tx = store.admin_tx().await?;
    let found = tx.verified_phone_for_user(user).await?;
    tx.commit().await?;
    Ok(found.is_some())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::fakes::{FakeComplianceStore, RecordingPhone};
    use crate::ports::UnavailableMoney;
    use time::Duration;

    fn t0() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(80_000)
    }

    #[test]
    fn normalize_and_hmac_mix_in_key_version() {
        assert_eq!(normalize_e164("+1 555-000-1234").unwrap(), "+15550001234");
        assert!(normalize_e164("15550001234").is_err());
        assert!(normalize_e164("+12").is_err());
        assert!(normalize_e164("+1abcdefgh").is_err());
        assert!(normalize_e164(&format!("+{}", "1".repeat(16))).is_err());
        assert!(hmac_e164(b"", 1, "+15550001234").is_err());
        let v1 = hmac_e164(b"secret", 1, "+15550001234").unwrap();
        let v2 = hmac_e164(b"secret", 2, "+15550001234").unwrap();
        assert_ne!(v1, v2);
        assert_eq!(v1, hmac_e164(b"secret", 1, "+15550001234").unwrap());
        assert!(code_digest(b"", "123456").is_err());
    }

    #[tokio::test]
    async fn challenge_verify_and_two_accounts_one_number() {
        let store = FakeComplianceStore::new();
        let phone = RecordingPhone::default();
        let a = store.add_user("ph-a");
        let b = store.add_user("ph-b");
        let secret = b"phone-secret";
        let now = t0();

        assert!(
            start_challenge(&store, &phone, a, "+15550001111", secret, 1, "12", now)
                .await
                .is_err()
        );

        let row = start_challenge(
            &store,
            &phone,
            a,
            "+1 555 000 1111",
            secret,
            1,
            "123456",
            now,
        )
        .await
        .unwrap();
        assert!(row.verified_at.is_none());
        assert_eq!(phone.last_e164(a).as_deref(), Some("+15550001111"));

        assert!(
            start_challenge(&store, &phone, a, "+15550002222", secret, 1, "654321", now)
                .await
                .is_err()
        );

        assert!(
            start_challenge(&store, &phone, b, "+15550001111", secret, 1, "123456", now)
                .await
                .is_err()
        );

        assert!(verify_challenge(&store, &phone, a, "000000", secret, now)
            .await
            .is_err());
        let verified = verify_challenge(&store, &phone, a, "123456", secret, now)
            .await
            .unwrap();
        assert!(verified.verified_at.is_some());
        let again = verify_challenge(&store, &phone, a, "123456", secret, now)
            .await
            .unwrap();
        assert_eq!(again.id, verified.id);
        assert!(is_grant_eligible(&store, a).await.unwrap());
        assert!(!is_grant_eligible(&store, b).await.unwrap());

        // Same number still cannot bind to B after A verified.
        assert!(
            start_challenge(&store, &phone, b, "+15550001111", secret, 1, "111111", now)
                .await
                .is_err()
        );

        // Replay start for A after verified returns the original row.
        let replay = start_challenge(&store, &phone, a, "+15550001111", secret, 1, "123456", now)
            .await
            .unwrap();
        assert_eq!(replay.id, verified.id);
    }

    #[tokio::test]
    async fn an_expired_code_can_be_re_challenged_on_the_same_number() {
        let store = FakeComplianceStore::new();
        let phone = RecordingPhone::default();
        let user = store.add_user("ph-refresh");
        let secret = b"phone-secret";
        let now = t0();
        let first = start_challenge(
            &store,
            &phone,
            user,
            "+15550003333",
            secret,
            1,
            "123456",
            now,
        )
        .await
        .unwrap();
        // Burn an attempt so the refresh is observable.
        assert!(
            verify_challenge(&store, &phone, user, "000000", secret, now)
                .await
                .is_err()
        );

        let later = now + PHONE_CHALLENGE_TTL + Duration::seconds(1);
        let again = start_challenge(
            &store,
            &phone,
            user,
            "+15550003333",
            secret,
            1,
            "654321",
            later,
        )
        .await
        .unwrap();
        assert_eq!(again.id, first.id, "the binding row is reused");
        assert_eq!(again.attempts, 0);
        assert_eq!(again.expires_at, Some(later + PHONE_CHALLENGE_TTL));
        assert!(again.verified_at.is_none());
        // The old code is dead; the fresh one verifies.
        assert!(
            verify_challenge(&store, &phone, user, "123456", secret, later)
                .await
                .is_err()
        );
        let verified = verify_challenge(&store, &phone, user, "654321", secret, later)
            .await
            .unwrap();
        assert!(verified.verified_at.is_some());
        assert!(is_grant_eligible(&store, user).await.unwrap());
    }

    #[tokio::test]
    async fn expired_and_missing_and_unavailable_provider() {
        let store = FakeComplianceStore::new();
        let phone = RecordingPhone::default();
        let user = store.add_user("ph-c");
        let secret = b"phone-secret";
        let now = t0();
        assert!(
            verify_challenge(&store, &phone, user, "123456", secret, now)
                .await
                .is_err()
        );
        start_challenge(
            &store,
            &phone,
            user,
            "+15550003333",
            secret,
            1,
            "123456",
            now,
        )
        .await
        .unwrap();
        assert!(verify_challenge(
            &store,
            &phone,
            user,
            "123456",
            secret,
            now + PHONE_CHALLENGE_TTL
        )
        .await
        .is_err());

        let other = store.add_user("ph-d");
        assert!(start_challenge(
            &store,
            &UnavailableMoney,
            other,
            "+15550004444",
            secret,
            1,
            "123456",
            now
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn atomic_attempts_exhaust() {
        let store = FakeComplianceStore::new();
        let phone = RecordingPhone::default();
        let user = store.add_user("ph-e");
        let secret = b"phone-secret";
        let now = t0();
        start_challenge(
            &store,
            &phone,
            user,
            "+15550005555",
            secret,
            1,
            "123456",
            now,
        )
        .await
        .unwrap();
        for _ in 0..PHONE_MAX_ATTEMPTS {
            let _ = verify_challenge(&store, &phone, user, "000000", secret, now).await;
        }
        assert!(
            verify_challenge(&store, &phone, user, "123456", secret, now)
                .await
                .is_err()
        );
    }
}
