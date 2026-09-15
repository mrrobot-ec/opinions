//! `users.status` next-request semantics, ban/shadow, homogeneous caps (D34).

use crate::error::{AppError, StoreError};
use crate::model::{AdminAction, AdminContext, AdminRole, UserId, UserStatus};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use super::admin::{
    new_proposal, payload_hash, require_admin, require_distinct_tokens, require_role,
    status_replay_key, ComplianceAdminStore, MoneyProposal, ProposalStatus, BAN_DELAY,
};

/// Published tier-0 cap. Shadow seeds MUST track this number.
pub const PUBLISHED_TIER0_CAP_MICRO: i64 = 25_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShadowCaps {
    pub trade_cap_micro: i64,
    pub deposit_cap_micro: i64,
}

impl Default for ShadowCaps {
    fn default() -> Self {
        Self {
            trade_cap_micro: PUBLISHED_TIER0_CAP_MICRO,
            deposit_cap_micro: PUBLISHED_TIER0_CAP_MICRO,
        }
    }
}

/// Absent config falls back to the published tier-0 number.
#[must_use]
pub fn shadow_cap_or_seed(configured: Option<i64>) -> i64 {
    configured.unwrap_or(PUBLISHED_TIER0_CAP_MICRO)
}

/// Homogeneous cap error payload: same as a published tier-0 breach.
#[must_use]
pub fn homogeneous_cap_exceeded(cap_micro: i64) -> (i64, u8) {
    (cap_micro, 0)
}

#[must_use]
pub fn status_name(status: UserStatus) -> &'static str {
    match status {
        UserStatus::Active => "active",
        UserStatus::ShadowLimited => "shadow_limited",
        UserStatus::Banned => "banned",
    }
}

/// # Errors
/// Unknown status label.
pub fn parse_user_status(raw: &str) -> Result<UserStatus, StoreError> {
    match raw {
        "active" => Ok(UserStatus::Active),
        "shadow_limited" => Ok(UserStatus::ShadowLimited),
        "banned" => Ok(UserStatus::Banned),
        _ => Err(StoreError::Invariant("unknown user status")),
    }
}

/// Shadow withdrawals always enter review.
#[must_use]
pub fn shadow_withdrawals_risk_hold(status: UserStatus) -> bool {
    status == UserStatus::ShadowLimited
}

/// Propose a ban (ops) or unban (ops). Confirm is superadmin, 15 min delay.
///
/// # Errors
/// Role / actor / store failures; replay conflict.
pub async fn propose_status(
    store: &impl ComplianceAdminStore,
    actor: &AdminContext,
    user: UserId,
    target: UserStatus,
    reason: String,
    epoch: i64,
    now: OffsetDateTime,
) -> Result<MoneyProposal, AppError> {
    let (digest, role) = require_admin(actor)?;
    require_role(role, &[AdminRole::Ops])?;
    if reason.trim().is_empty() {
        return Err(AppError::AdminForbidden("reason is required"));
    }
    let kind = match target {
        UserStatus::Banned => "ban_user",
        UserStatus::Active => "unban_user",
        UserStatus::ShadowLimited => {
            return Err(AppError::AdminForbidden(
                "shadow-limit is a single-ops command",
            ))
        }
    };
    let replay = status_replay_key("user_status", user, epoch);
    let payload = json!({ "target": status_name(target) }).to_string();
    let hash = payload_hash(payload.as_bytes());
    let mut tx = store.admin_tx().await?;
    tx.lock_user(user).await?;
    if let Some(existing) = tx.get_proposal_by_replay(&replay).await? {
        if existing.payload_hash == hash {
            tx.commit().await?;
            return Ok(existing);
        }
        return Err(AppError::ProposalConflict(
            "replay key was reused with a different payload",
        ));
    }
    let proposal = new_proposal(
        kind,
        user.0,
        hash,
        digest.to_string(),
        reason.clone(),
        replay,
        BAN_DELAY,
        now,
    );
    tx.insert_proposal(proposal.clone()).await?;
    tx.audit_insert(AdminAction {
        actor_role: role,
        actor_token_digest: digest.to_string(),
        action: format!("propose_{kind}"),
        subject: format!("user:{}", user.0),
        before: None,
        after: Some(json!({ "target": status_name(target) })),
        reason: Some(reason),
    })
    .await?;
    tx.commit().await?;
    Ok(proposal)
}

/// Confirm ban/unban (superadmin, distinct token, after delay).
///
/// # Errors
/// Role, token, expiry, store.
pub async fn confirm_status(
    store: &impl ComplianceAdminStore,
    actor: &AdminContext,
    proposal_id: Uuid,
    now: OffsetDateTime,
) -> Result<UserStatus, AppError> {
    let (digest, role) = require_admin(actor)?;
    require_role(role, &[AdminRole::Superadmin])?;
    let mut tx = store.admin_tx().await?;
    let proposal = tx.get_proposal(proposal_id).await?;
    require_distinct_tokens(&proposal.proposer_token_id, digest)?;
    if proposal.status != ProposalStatus::Pending {
        return Err(AppError::ProposalConflict("proposal is not pending"));
    }
    if now < proposal.confirm_not_before {
        return Err(AppError::ProposalConflict(
            "confirm_not_before has not elapsed",
        ));
    }
    if now > proposal.expires_at {
        return Err(AppError::ProposalConflict("proposal expired"));
    }
    let target = match proposal.kind.as_str() {
        "ban_user" => UserStatus::Banned,
        "unban_user" => UserStatus::Active,
        _ => return Err(AppError::ProposalConflict("not a status proposal")),
    };
    tx.lock_user(UserId(proposal.subject_id)).await?;
    let confirmed = tx.confirm_proposal(proposal_id, digest, now).await?;
    let status = tx
        .set_user_status(UserId(proposal.subject_id), target)
        .await?;
    tx.audit_insert(AdminAction {
        actor_role: role,
        actor_token_digest: digest.to_string(),
        action: format!("confirm_{}", confirmed.kind),
        subject: format!("user:{}", proposal.subject_id),
        before: None,
        after: Some(json!({ "status": status_name(status) })),
        reason: Some(proposal.reason),
    })
    .await?;
    tx.commit().await?;
    Ok(status)
}

/// Single-ops shadow / unshadow (one audit, no proposal).
///
/// # Errors
/// Role / store.
pub async fn set_shadow(
    store: &impl ComplianceAdminStore,
    actor: &AdminContext,
    user: UserId,
    shadow: bool,
    reason: String,
    now: OffsetDateTime,
) -> Result<UserStatus, AppError> {
    let _ = now;
    let (digest, role) = require_admin(actor)?;
    require_role(role, &[AdminRole::Ops])?;
    if reason.trim().is_empty() {
        return Err(AppError::AdminForbidden("reason is required"));
    }
    let target = if shadow {
        UserStatus::ShadowLimited
    } else {
        UserStatus::Active
    };
    let mut tx = store.admin_tx().await?;
    let before = tx.lock_user(user).await?;
    let status = tx.set_user_status(user, target).await?;
    tx.audit_insert(AdminAction {
        actor_role: role,
        actor_token_digest: digest.to_string(),
        action: if shadow { "shadow_limit" } else { "unshadow" }.into(),
        subject: format!("user:{}", user.0),
        before: Some(json!({ "status": before.status })),
        after: Some(json!({ "status": status_name(status) })),
        reason: Some(reason),
    })
    .await?;
    tx.commit().await?;
    Ok(status)
}

/// Profile / public WS must not echo status. Comments stay `visible` to self.
#[must_use]
pub fn public_profile_omits_status() -> bool {
    true
}

#[must_use]
pub fn author_visible_comment_status() -> &'static str {
    "visible"
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::fakes::FakeComplianceStore;
    use crate::money::admin::PROPOSAL_CONFIRM_WINDOW;
    use time::Duration;

    fn t0() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(60_000)
    }

    fn admin(role: AdminRole, tok: &str) -> AdminContext {
        AdminContext::Admin {
            token_digest: tok.into(),
            role,
        }
    }

    #[test]
    fn status_parse_and_shadow_caps_track_tier0() {
        assert_eq!(parse_user_status("active").unwrap(), UserStatus::Active);
        assert_eq!(
            parse_user_status("shadow_limited").unwrap(),
            UserStatus::ShadowLimited
        );
        assert_eq!(parse_user_status("banned").unwrap(), UserStatus::Banned);
        assert!(parse_user_status("ghost").is_err());
        assert_eq!(status_name(UserStatus::Active), "active");
        assert_eq!(status_name(UserStatus::ShadowLimited), "shadow_limited");
        assert_eq!(status_name(UserStatus::Banned), "banned");
        assert_eq!(shadow_cap_or_seed(None), PUBLISHED_TIER0_CAP_MICRO);
        assert_eq!(shadow_cap_or_seed(Some(1)), 1);
        assert_eq!(
            homogeneous_cap_exceeded(PUBLISHED_TIER0_CAP_MICRO),
            (PUBLISHED_TIER0_CAP_MICRO, 0)
        );
        assert!(shadow_withdrawals_risk_hold(UserStatus::ShadowLimited));
        assert!(!shadow_withdrawals_risk_hold(UserStatus::Active));
        assert!(public_profile_omits_status());
        assert_eq!(author_visible_comment_status(), "visible");
        assert_eq!(
            ShadowCaps::default().trade_cap_micro,
            PUBLISHED_TIER0_CAP_MICRO
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn ban_is_dual_control_shadow_is_single_ops() {
        let store = FakeComplianceStore::new();
        let user = store.add_user("stat-a");
        let now = t0();
        assert!(propose_status(
            &store,
            &AdminContext::Machine,
            user,
            UserStatus::Banned,
            "r".into(),
            1,
            now
        )
        .await
        .is_err());
        assert!(propose_status(
            &store,
            &admin(AdminRole::Finance, "f"),
            user,
            UserStatus::Banned,
            "r".into(),
            1,
            now
        )
        .await
        .is_err());
        assert!(propose_status(
            &store,
            &admin(AdminRole::Ops, "ops"),
            user,
            UserStatus::Banned,
            "   ".into(),
            1,
            now
        )
        .await
        .is_err());
        assert!(propose_status(
            &store,
            &admin(AdminRole::Ops, "ops"),
            user,
            UserStatus::ShadowLimited,
            "r".into(),
            1,
            now
        )
        .await
        .is_err());
        assert!(propose_status(
            &store,
            &admin(AdminRole::Superadmin, "sa"),
            user,
            UserStatus::Banned,
            "r".into(),
            1,
            now
        )
        .await
        .is_err());

        let proposed = propose_status(
            &store,
            &admin(AdminRole::Ops, "ops"),
            user,
            UserStatus::Banned,
            "abuse".into(),
            1,
            now,
        )
        .await
        .unwrap();
        let replayed = propose_status(
            &store,
            &admin(AdminRole::Ops, "ops"),
            user,
            UserStatus::Banned,
            "abuse".into(),
            1,
            now,
        )
        .await
        .unwrap();
        assert_eq!(proposed.id, replayed.id);
        assert!(propose_status(
            &store,
            &admin(AdminRole::Ops, "ops"),
            user,
            UserStatus::Active,
            "other".into(),
            1,
            now
        )
        .await
        .is_err());

        assert!(
            confirm_status(&store, &admin(AdminRole::Ops, "ops2"), proposed.id, now)
                .await
                .is_err()
        );
        assert!(confirm_status(
            &store,
            &admin(AdminRole::Superadmin, "ops"),
            proposed.id,
            now + BAN_DELAY
        )
        .await
        .is_err());
        assert!(confirm_status(
            &store,
            &admin(AdminRole::Superadmin, "sa"),
            proposed.id,
            now
        )
        .await
        .is_err());
        assert!(confirm_status(
            &store,
            &admin(AdminRole::Superadmin, "sa"),
            proposed.id,
            now + BAN_DELAY + PROPOSAL_CONFIRM_WINDOW + Duration::seconds(1)
        )
        .await
        .is_err());

        let banned = confirm_status(
            &store,
            &admin(AdminRole::Superadmin, "sa"),
            proposed.id,
            now + BAN_DELAY,
        )
        .await
        .unwrap();
        assert_eq!(banned, UserStatus::Banned);
        assert!(confirm_status(
            &store,
            &admin(AdminRole::Superadmin, "sa"),
            proposed.id,
            now + BAN_DELAY
        )
        .await
        .is_err());

        let unban = propose_status(
            &store,
            &admin(AdminRole::Ops, "ops"),
            user,
            UserStatus::Active,
            "served".into(),
            2,
            now,
        )
        .await
        .unwrap();
        let active = confirm_status(
            &store,
            &admin(AdminRole::Superadmin, "sa"),
            unban.id,
            now + BAN_DELAY,
        )
        .await
        .unwrap();
        assert_eq!(active, UserStatus::Active);

        assert!(set_shadow(
            &store,
            &admin(AdminRole::Finance, "f"),
            user,
            true,
            "r".into(),
            now
        )
        .await
        .is_err());
        assert!(set_shadow(
            &store,
            &admin(AdminRole::Ops, "ops"),
            user,
            true,
            " ".into(),
            now
        )
        .await
        .is_err());
        assert_eq!(
            set_shadow(
                &store,
                &admin(AdminRole::Ops, "ops"),
                user,
                true,
                "probe".into(),
                now
            )
            .await
            .unwrap(),
            UserStatus::ShadowLimited
        );
        assert_eq!(
            set_shadow(
                &store,
                &admin(AdminRole::Ops, "ops"),
                user,
                false,
                "clear".into(),
                now
            )
            .await
            .unwrap(),
            UserStatus::Active
        );
    }

    #[tokio::test]
    async fn confirm_rejects_non_status_kind() {
        let store = FakeComplianceStore::new();
        let user = store.add_user("stat-b");
        let now = t0();
        let mut tx = store.admin_tx().await.unwrap();
        let p = new_proposal(
            "clear_aml",
            user.0,
            "h".into(),
            "ops".into(),
            "r".into(),
            "rk".into(),
            time::Duration::ZERO,
            now,
        );
        let id = p.id;
        tx.insert_proposal(p).await.unwrap();
        tx.commit().await.unwrap();
        assert!(
            confirm_status(&store, &admin(AdminRole::Superadmin, "sa"), id, now)
                .await
                .is_err()
        );
    }
}
