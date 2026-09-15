//! Sanctions screening facts and dual-control hold (D33).

use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::StoreError;
use crate::model::UserId;
use crate::ports::ScreenVerdict;

use super::{allows_progress, needs_rescreen, AmlFlag, AmlKind, ComplianceStore, ComplianceTx};

/// Persisted screening row (`sanction_screenings`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanctionScreening {
    pub id: Uuid,
    pub user: UserId,
    pub context: String,
    pub verdict: ScreenVerdict,
    pub raw_ref: Option<String>,
}

/// Reconstruct the D33 algebra from a stored row. A Clear that has no
/// timestamps is treated as Indeterminate (fail closed).
#[must_use]
pub fn screening_verdict(screening: &SanctionScreening) -> ScreenVerdict {
    screening.verdict.clone()
}

/// A Hit opens a dual-control compliance hold (same flag authority as AML).
#[must_use]
pub fn hit_opens_hold(verdict: &ScreenVerdict) -> bool {
    matches!(verdict, ScreenVerdict::Hit)
}

/// Persist a screening. Hits also open a `sanctions_hit` flag.
///
/// # Errors
/// Store failures.
pub async fn persist_screening(
    tx: &mut dyn ComplianceTx,
    screening: SanctionScreening,
    now: OffsetDateTime,
) -> Result<SanctionScreening, StoreError> {
    let hit = hit_opens_hold(&screening.verdict);
    tx.insert_screening(screening.clone()).await?;
    if hit {
        let flag = AmlFlag {
            id: Uuid::new_v4(),
            user: screening.user,
            rule: AmlKind::SanctionsHit,
            window_label: screening.context.clone(),
            evidence: json!({ "screening_id": screening.id }),
            open: true,
            at: now,
        };
        tx.insert_aml_flag(flag).await?;
    }
    Ok(screening)
}

/// Latest stored verdict or Indeterminate.
///
/// # Errors
/// Store failures.
pub async fn current_sanctions(
    tx: &mut dyn ComplianceTx,
    user: UserId,
    context: &str,
    now: OffsetDateTime,
) -> Result<ScreenVerdict, StoreError> {
    let Some(row) = tx.latest_screening(user, context).await? else {
        return Ok(ScreenVerdict::Indeterminate);
    };
    let verdict = screening_verdict(&row);
    if needs_rescreen(&verdict, now) && !matches!(verdict, ScreenVerdict::Hit) {
        return Ok(ScreenVerdict::Indeterminate);
    }
    Ok(verdict)
}

/// Persist a remote screen result through a fresh transaction.
///
/// # Errors
/// Store failures.
pub async fn record_remote_screen(
    store: &impl ComplianceStore,
    user: UserId,
    context: &str,
    verdict: ScreenVerdict,
    raw_ref: Option<String>,
    now: OffsetDateTime,
) -> Result<SanctionScreening, StoreError> {
    let mut tx = store.compliance_tx().await?;
    tx.lock_user(user).await?;
    let screening = SanctionScreening {
        id: Uuid::new_v4(),
        user,
        context: context.to_string(),
        verdict,
        raw_ref,
    };
    let screening = persist_screening(tx.as_mut(), screening, now).await?;
    tx.commit().await?;
    Ok(screening)
}

/// Send-boundary check: Hit / stale / missing refuses the send.
#[must_use]
pub fn send_allowed(verdict: &ScreenVerdict, now: OffsetDateTime) -> bool {
    allows_progress(verdict, now)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::fakes::FakeComplianceStore;
    use time::Duration;

    fn t0() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(30_000)
    }

    fn clear(now: OffsetDateTime) -> ScreenVerdict {
        ScreenVerdict::Clear {
            checked_at: now,
            expires_at: now + Duration::hours(12),
            policy_version: "s-1".into(),
        }
    }

    #[test]
    fn hit_opens_hold_and_send_gate() {
        let now = t0();
        assert!(hit_opens_hold(&ScreenVerdict::Hit));
        assert!(!hit_opens_hold(&ScreenVerdict::Indeterminate));
        assert!(!hit_opens_hold(&clear(now)));
        assert!(send_allowed(&clear(now), now));
        assert!(!send_allowed(&ScreenVerdict::Hit, now));
        let row = SanctionScreening {
            id: Uuid::nil(),
            user: UserId(Uuid::nil()),
            context: "withdraw".into(),
            verdict: ScreenVerdict::Hit,
            raw_ref: None,
        };
        assert_eq!(screening_verdict(&row), ScreenVerdict::Hit);
    }

    #[tokio::test]
    async fn persist_hit_opens_flag_and_stale_clear_becomes_indeterminate() {
        let store = FakeComplianceStore::new();
        let user = store.add_user("san-a");
        let now = t0();
        record_remote_screen(
            &store,
            user,
            "withdraw",
            ScreenVerdict::Hit,
            Some("ref".into()),
            now,
        )
        .await
        .unwrap();
        let mut tx = store.compliance_tx().await.unwrap();
        let flags = tx.open_aml_flags(user).await.unwrap();
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].rule, AmlKind::SanctionsHit);
        assert_eq!(
            current_sanctions(tx.as_mut(), user, "withdraw", now)
                .await
                .unwrap(),
            ScreenVerdict::Hit
        );
        tx.commit().await.unwrap();

        record_remote_screen(&store, user, "trade", clear(now), None, now)
            .await
            .unwrap();
        let mut tx = store.compliance_tx().await.unwrap();
        assert!(send_allowed(
            &current_sanctions(tx.as_mut(), user, "trade", now)
                .await
                .unwrap(),
            now
        ));
        let later = now + Duration::hours(13);
        assert_eq!(
            current_sanctions(tx.as_mut(), user, "trade", later)
                .await
                .unwrap(),
            ScreenVerdict::Indeterminate
        );
        assert_eq!(
            current_sanctions(tx.as_mut(), user, "missing", now)
                .await
                .unwrap(),
            ScreenVerdict::Indeterminate
        );
        tx.commit().await.unwrap();
    }
}
