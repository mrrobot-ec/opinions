//! D26 audit plumbing shared by the manual-ops use cases: the typed policy
//! (published caps and dual-control timing from the migration-0008 catalog
//! seeds), the actor→audit-row builder, and the `audit-read` page.

use serde_json::Value;
use time::OffsetDateTime;

use crate::error::AppError;
use crate::model::{AdminAction, AdminContext};
use crate::ports::{AuditPageRow, OpsQueries};

/// Typed manual-ops policy. Defaults are the published migration-0008
/// catalog seeds; config-driven overrides ride W1's watch snapshot when it
/// lands (each value stays inside the D24 catalog bounds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpsPolicy {
    pub faucet_per_call_cap_micro: i64,
    pub remedial_market_cap_micro: i64,
    pub remedial_daily_cap_micro: i64,
    pub receivable_outstanding_cap_micro: i64,
    pub writeoff_per_item_cap_micro: i64,
    pub writeoff_daily_cap_micro: i64,
    pub proposal_ttl_secs: u64,
    pub dual_control_delay_secs: u64,
}

impl Default for OpsPolicy {
    fn default() -> Self {
        Self {
            faucet_per_call_cap_micro: 1_000_000_000,
            remedial_market_cap_micro: 500_000_000,
            remedial_daily_cap_micro: 2_000_000_000,
            receivable_outstanding_cap_micro: 10_000_000_000,
            writeoff_per_item_cap_micro: 500_000_000,
            writeoff_daily_cap_micro: 2_000_000_000,
            proposal_ttl_secs: 900,
            dual_control_delay_secs: 60,
        }
    }
}

impl OpsPolicy {
    /// The earliest instant a dual-control proposal may confirm.
    #[must_use]
    pub fn confirm_not_before(&self, now: OffsetDateTime) -> OffsetDateTime {
        now + time::Duration::seconds(
            self.dual_control_delay_secs
                .min(i64::MAX.cast_unsigned())
                .cast_signed(),
        )
    }

    /// The instant a pending proposal expires (measured from
    /// `confirm_not_before`, so the confirm window is always ≥ the TTL).
    #[must_use]
    pub fn expires_at(&self, confirm_not_before: OffsetDateTime) -> OffsetDateTime {
        confirm_not_before
            + time::Duration::seconds(
                self.proposal_ttl_secs
                    .min(i64::MAX.cast_unsigned())
                    .cast_signed(),
            )
    }
}

/// Manual-ops rejections that have no [`AppError`] shape: caps map to 422 at
/// the adapter.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum OpsError {
    #[error("amount exceeds the pinned cap of {cap_micro} micro-USD")]
    OverCap { cap_micro: i64 },
    #[error("amount must be a positive micro-USD integer")]
    InvalidAmount,
    #[error(transparent)]
    App(#[from] AppError),
}

impl From<crate::error::StoreError> for OpsError {
    fn from(e: crate::error::StoreError) -> Self {
        Self::App(AppError::Store(e))
    }
}

/// Builds the audit row for an admin actor; machine actors audit nothing
/// (D26). The caller inserts it in the SAME transaction as the effect.
#[must_use]
pub fn audit_for(
    actor: &AdminContext,
    action: &str,
    subject: String,
    before: Option<Value>,
    after: Option<Value>,
    reason: Option<String>,
) -> Option<AdminAction> {
    let AdminContext::Admin { token_digest, role } = actor else {
        return None;
    };
    Some(AdminAction {
        actor_role: *role,
        actor_token_digest: token_digest.clone(),
        action: action.to_string(),
        subject,
        before,
        after,
        reason,
    })
}

/// Builds an audit row for flows that have already required an admin
/// principal through [`principal_digest`].
///
/// # Errors
/// Machine actors are rejected if a caller violates that ordering invariant.
#[must_use]
pub fn required_audit_for(
    actor: &AdminContext,
    action: &str,
    subject: String,
    before: Option<Value>,
    after: Option<Value>,
    reason: Option<String>,
) -> AdminAction {
    audit_for(actor, action, subject, before, after, reason)
        .unwrap_or_else(|| unreachable!("principal_digest must run before required audit"))
}

/// The principal's stable identity for dual-control rows: the SHA-256 token
/// digest the RBAC layer authenticated (distinct tokens ⇒ distinct digests).
///
/// # Errors
/// Machine actors cannot hold dual-control roles.
pub fn principal_digest(actor: &AdminContext) -> Result<&str, AppError> {
    match actor {
        AdminContext::Admin { token_digest, .. } => Ok(token_digest),
        AdminContext::Machine => Err(AppError::ProposalConflict("machine actor")),
    }
}

/// `GET /admin/audit` (capability `audit-read`): newest-first redacted page.
///
/// # Errors
/// Store failures.
pub async fn audit_page<Q: OpsQueries + ?Sized>(
    queries: &Q,
    before: Option<OffsetDateTime>,
    limit: u32,
) -> Result<Vec<AuditPageRow>, AppError> {
    Ok(queries.audit_page(before, limit.clamp(1, 200)).await?)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::model::AdminRole;

    fn admin() -> AdminContext {
        AdminContext::Admin {
            token_digest: "digest-a".into(),
            role: AdminRole::Finance,
        }
    }

    #[test]
    fn machine_actors_produce_no_audit_row_and_no_principal() {
        assert!(audit_for(&AdminContext::Machine, "x", "s".into(), None, None, None).is_none());
        assert_eq!(
            principal_digest(&AdminContext::Machine).unwrap_err(),
            AppError::ProposalConflict("machine actor")
        );
    }

    #[test]
    fn admin_actors_produce_the_redacted_row() {
        let row = audit_for(
            &admin(),
            "unwind_propose",
            "market:m".into(),
            None,
            None,
            Some("r".into()),
        )
        .unwrap();
        assert_eq!(row.actor_role, AdminRole::Finance);
        assert_eq!(row.actor_token_digest, "digest-a");
        assert_eq!(row.action, "unwind_propose");
        assert_eq!(principal_digest(&admin()).unwrap(), "digest-a");
        assert_eq!(
            required_audit_for(&admin(), "x", "s".into(), None, None, None).action,
            "x"
        );
    }

    #[test]
    #[should_panic(expected = "principal_digest must run before required audit")]
    fn required_audit_asserts_its_authenticated_principal_precondition() {
        let _ = required_audit_for(&AdminContext::Machine, "x", "s".into(), None, None, None);
    }

    #[test]
    fn policy_defaults_match_the_published_catalog_seeds() {
        let policy = OpsPolicy::default();
        assert_eq!(policy.faucet_per_call_cap_micro, 1_000_000_000);
        assert_eq!(policy.remedial_market_cap_micro, 500_000_000);
        assert_eq!(policy.remedial_daily_cap_micro, 2_000_000_000);
        assert_eq!(policy.receivable_outstanding_cap_micro, 10_000_000_000);
        assert_eq!(policy.writeoff_per_item_cap_micro, 500_000_000);
        assert_eq!(policy.writeoff_daily_cap_micro, 2_000_000_000);
        let now = OffsetDateTime::UNIX_EPOCH;
        let confirm = policy.confirm_not_before(now);
        assert_eq!(confirm - now, time::Duration::seconds(60));
        assert_eq!(
            policy.expires_at(confirm) - confirm,
            time::Duration::seconds(900)
        );
    }

    #[test]
    fn store_errors_preserve_their_typed_application_meaning() {
        let error = OpsError::from(crate::error::StoreError::NotFound("row"));
        assert_eq!(
            error,
            OpsError::App(AppError::Store(crate::error::StoreError::NotFound("row")))
        );
    }

    #[tokio::test]
    async fn audit_page_clamps_limits_before_delegating() {
        let store = crate::fakes::InMemoryStore::new();
        assert!(audit_page(&store, None, 0).await.unwrap().is_empty());
        assert!(audit_page(&store, None, u32::MAX).await.unwrap().is_empty());
    }
}
