//! KYC tier facts, validity horizon, and webhook application (D33).

use serde_json::{json, Value};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::StoreError;
use crate::model::UserId;
use crate::ports::ScreenVerdict;

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
    use time::Duration;

    fn t0() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
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
