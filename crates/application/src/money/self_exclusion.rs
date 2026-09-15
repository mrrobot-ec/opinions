//! Self-exclusion and user deposit limits (D34).

use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::AppError;
use crate::model::{AdminAction, AdminContext, AdminRole, UserId};

use super::admin::{
    new_proposal, payload_hash, require_admin, require_distinct_tokens, require_role,
    ComplianceAdminStore, FrozenFundsLicense, MoneyProposal, ProposalStatus,
    DEPOSIT_LIMIT_RAISE_DELAY, FROZEN_FUNDS_DELAY,
};
use super::AmlFlag;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfExclusion {
    pub id: Uuid,
    pub user: UserId,
    pub starts_at: OffsetDateTime,
    pub cooling_off_until: OffsetDateTime,
    pub lifted_at: Option<OffsetDateTime>,
}

impl SelfExclusion {
    #[must_use]
    pub fn is_active(&self, now: OffsetDateTime) -> bool {
        self.lifted_at.is_none() && now >= self.starts_at
    }

    #[must_use]
    pub fn cooling_off_elapsed(&self, now: OffsetDateTime) -> bool {
        now >= self.cooling_off_until
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserDepositLimit {
    pub user: UserId,
    pub limit_micro: i64,
    pub pending_limit_micro: Option<i64>,
    pub pending_effective_at: Option<OffsetDateTime>,
    pub updated_at: OffsetDateTime,
}

/// Apply a pending raise once the 24h delay has elapsed.
#[must_use]
pub fn effective_deposit_limit(limit: &UserDepositLimit, now: OffsetDateTime) -> i64 {
    match (limit.pending_limit_micro, limit.pending_effective_at) {
        (Some(pending), Some(at)) if now >= at => pending,
        _ => limit.limit_micro,
    }
}

/// Lowering is immediate; raising is delayed 24h.
#[must_use]
pub fn deposit_limit_after_write(
    current: Option<&UserDepositLimit>,
    requested: i64,
    now: OffsetDateTime,
    user: UserId,
) -> UserDepositLimit {
    let current_effective = current.map(|row| effective_deposit_limit(row, now));
    match current_effective {
        Some(have) if requested > have => UserDepositLimit {
            user,
            limit_micro: have,
            pending_limit_micro: Some(requested),
            pending_effective_at: Some(now + DEPOSIT_LIMIT_RAISE_DELAY),
            updated_at: now,
        },
        _ => UserDepositLimit {
            user,
            limit_micro: requested,
            pending_limit_micro: None,
            pending_effective_at: None,
            updated_at: now,
        },
    }
}

/// User-initiated self-exclusion. Effective immediately; irreversible for
/// the stamped cooling-off interval.
///
/// # Errors
/// Already-active exclusion; store failures.
pub async fn start_self_exclusion(
    store: &impl ComplianceAdminStore,
    user: UserId,
    cooling_off: time::Duration,
    now: OffsetDateTime,
) -> Result<SelfExclusion, AppError> {
    if cooling_off <= time::Duration::ZERO {
        return Err(AppError::AdminForbidden("cooling-off must be positive"));
    }
    let mut tx = store.admin_tx().await?;
    tx.lock_user(user).await?;
    // An unlifted exclusion is in force whether or not its cooling-off has
    // elapsed: expiry only makes a *lift* proposable (D34), it never makes
    // the exclusion lapse on its own.
    if tx.active_self_exclusion(user).await?.is_some() {
        return Err(AppError::AdminForbidden("self-exclusion is already active"));
    }
    let exclusion = SelfExclusion {
        id: Uuid::new_v4(),
        user,
        starts_at: now,
        cooling_off_until: now + cooling_off,
        lifted_at: None,
    };
    let exclusion = tx.insert_self_exclusion(exclusion).await?;
    tx.insert_decision(super::ComplianceDecision {
        id: Uuid::new_v4(),
        subject_type: "self_exclusion".into(),
        subject_id: exclusion.id,
        kind: "start".into(),
        actor: "user".into(),
        at: now,
        payload: json!({ "cooling_off_until": exclusion.cooling_off_until.unix_timestamp() }),
    })
    .await?;
    tx.commit().await?;
    Ok(exclusion)
}

/// Propose a post-expiry lift (finance).
///
/// # Errors
/// Role, cooling-off not elapsed, store.
pub async fn propose_lift(
    store: &impl ComplianceAdminStore,
    actor: &AdminContext,
    exclusion_id: Uuid,
    reason: String,
    now: OffsetDateTime,
) -> Result<MoneyProposal, AppError> {
    let (digest, role) = require_admin(actor)?;
    require_role(role, &[AdminRole::Finance])?;
    if reason.trim().is_empty() {
        return Err(AppError::AdminForbidden("reason is required"));
    }
    let mut tx = store.admin_tx().await?;
    let exclusion = tx.get_self_exclusion(exclusion_id).await?;
    if !exclusion.cooling_off_elapsed(now) {
        return Err(AppError::ProposalConflict("cooling-off has not elapsed"));
    }
    if exclusion.lifted_at.is_some() {
        return Err(AppError::ProposalConflict("already lifted"));
    }
    let replay = format!("lift_self_exclusion:{}", exclusion.id);
    if let Some(existing) = tx.get_proposal_by_replay(&replay).await? {
        tx.commit().await?;
        return Ok(existing);
    }
    let proposal = new_proposal(
        "lift_self_exclusion",
        exclusion.id,
        payload_hash(replay.as_bytes()),
        digest.to_string(),
        reason.clone(),
        replay,
        time::Duration::ZERO,
        now,
    );
    tx.insert_proposal(proposal.clone()).await?;
    tx.audit_insert(AdminAction {
        actor_role: role,
        actor_token_digest: digest.to_string(),
        action: "propose_lift_self_exclusion".into(),
        subject: format!("self_exclusion:{}", exclusion.id),
        before: None,
        after: None,
        reason: Some(reason),
    })
    .await?;
    tx.commit().await?;
    Ok(proposal)
}

/// Confirm lift (superadmin, distinct token).
///
/// # Errors
/// Role, token, expiry, store.
pub async fn confirm_lift(
    store: &impl ComplianceAdminStore,
    actor: &AdminContext,
    proposal_id: Uuid,
    now: OffsetDateTime,
) -> Result<SelfExclusion, AppError> {
    let (digest, role) = require_admin(actor)?;
    require_role(role, &[AdminRole::Superadmin])?;
    let mut tx = store.admin_tx().await?;
    let proposal = tx.get_proposal(proposal_id).await?;
    require_distinct_tokens(&proposal.proposer_token_id, digest)?;
    if proposal.kind != "lift_self_exclusion" {
        return Err(AppError::ProposalConflict("not a lift proposal"));
    }
    if proposal.status != ProposalStatus::Pending {
        return Err(AppError::ProposalConflict("proposal is not pending"));
    }
    if now < proposal.confirm_not_before || now > proposal.expires_at {
        return Err(AppError::ProposalConflict("proposal not confirmable"));
    }
    let exclusion = tx.get_self_exclusion(proposal.subject_id).await?;
    if !exclusion.cooling_off_elapsed(now) {
        return Err(AppError::ProposalConflict("cooling-off has not elapsed"));
    }
    tx.confirm_proposal(proposal_id, digest, now).await?;
    let lifted = tx.lift_self_exclusion(exclusion.id, now).await?;
    tx.audit_insert(AdminAction {
        actor_role: role,
        actor_token_digest: digest.to_string(),
        action: "confirm_lift_self_exclusion".into(),
        subject: format!("self_exclusion:{}", exclusion.id),
        before: None,
        after: Some(json!({ "lifted_at": now.unix_timestamp() })),
        reason: Some(proposal.reason),
    })
    .await?;
    tx.commit().await?;
    Ok(lifted)
}

/// Persist a self-set deposit limit.
///
/// # Errors
/// Negative amount; store.
pub async fn set_deposit_limit(
    store: &impl ComplianceAdminStore,
    user: UserId,
    requested: i64,
    now: OffsetDateTime,
) -> Result<UserDepositLimit, AppError> {
    if requested < 0 {
        return Err(AppError::AdminForbidden("limit must be non-negative"));
    }
    let mut tx = store.admin_tx().await?;
    tx.lock_user(user).await?;
    let current = tx.get_deposit_limit(user).await?;
    let next = deposit_limit_after_write(current.as_ref(), requested, now, user);
    tx.upsert_deposit_limit(next.clone()).await?;
    tx.commit().await?;
    Ok(next)
}

/// Propose/confirm AML (or sanctions-hold) clear.
///
/// # Errors
/// Role / store / proposal.
pub async fn propose_clear_flag(
    store: &impl ComplianceAdminStore,
    actor: &AdminContext,
    flag_id: Uuid,
    reason: String,
    now: OffsetDateTime,
) -> Result<MoneyProposal, AppError> {
    let (digest, role) = require_admin(actor)?;
    require_role(role, &[AdminRole::Finance])?;
    if reason.trim().is_empty() {
        return Err(AppError::AdminForbidden("reason is required"));
    }
    let mut tx = store.admin_tx().await?;
    let flag = tx.get_aml_flag(flag_id).await?;
    if !flag.open {
        return Err(AppError::ProposalConflict("flag is not open"));
    }
    let replay = format!("clear_aml:{}", flag.id);
    if let Some(existing) = tx.get_proposal_by_replay(&replay).await? {
        tx.commit().await?;
        return Ok(existing);
    }
    let proposal = new_proposal(
        "clear_aml",
        flag.id,
        payload_hash(replay.as_bytes()),
        digest.to_string(),
        reason.clone(),
        replay,
        time::Duration::ZERO,
        now,
    );
    tx.insert_proposal(proposal.clone()).await?;
    tx.audit_insert(AdminAction {
        actor_role: role,
        actor_token_digest: digest.to_string(),
        action: "propose_clear_aml".into(),
        subject: format!("aml:{}", flag.id),
        before: None,
        after: None,
        reason: Some(reason),
    })
    .await?;
    tx.commit().await?;
    Ok(proposal)
}

/// Confirm AML/sanctions flag clear (superadmin, distinct, no delay).
///
/// # Errors
/// Role / token / store.
pub async fn confirm_clear_flag(
    store: &impl ComplianceAdminStore,
    actor: &AdminContext,
    proposal_id: Uuid,
    now: OffsetDateTime,
) -> Result<AmlFlag, AppError> {
    let (digest, role) = require_admin(actor)?;
    require_role(role, &[AdminRole::Superadmin])?;
    let mut tx = store.admin_tx().await?;
    let proposal = tx.get_proposal(proposal_id).await?;
    require_distinct_tokens(&proposal.proposer_token_id, digest)?;
    if proposal.kind != "clear_aml" || proposal.status != ProposalStatus::Pending {
        return Err(AppError::ProposalConflict("not a pending clear"));
    }
    if now < proposal.confirm_not_before || now > proposal.expires_at {
        return Err(AppError::ProposalConflict("proposal not confirmable"));
    }
    tx.confirm_proposal(proposal_id, digest, now).await?;
    let cleared = tx.clear_aml_flag(proposal.subject_id).await?;
    tx.audit_insert(AdminAction {
        actor_role: role,
        actor_token_digest: digest.to_string(),
        action: "confirm_clear_aml".into(),
        subject: format!("aml:{}", proposal.subject_id),
        before: None,
        after: Some(json!({ "open": false })),
        reason: Some(proposal.reason),
    })
    .await?;
    tx.commit().await?;
    Ok(cleared)
}

/// Propose a counsel-shaped frozen-funds license (24h delay).
///
/// # Errors
/// Role / dest / store.
pub async fn propose_frozen_license(
    store: &impl ComplianceAdminStore,
    actor: &AdminContext,
    license: FrozenFundsLicense,
    reason: String,
    now: OffsetDateTime,
) -> Result<MoneyProposal, AppError> {
    let (digest, role) = require_admin(actor)?;
    require_role(role, &[AdminRole::Finance])?;
    if reason.trim().is_empty() || license.dest.trim().is_empty() {
        return Err(AppError::AdminForbidden(
            "reason and counsel-shaped dest are required",
        ));
    }
    if license.amount_micro <= 0 {
        return Err(AppError::AdminForbidden("amount must be positive"));
    }
    let mut tx = store.admin_tx().await?;
    tx.lock_user(license.user).await?;
    let stored = tx.insert_frozen_license(license.clone()).await?;
    let replay = format!("frozen_funds_license:{}", stored.id);
    let proposal = new_proposal(
        "frozen_funds_license",
        stored.id,
        payload_hash(stored.dest.as_bytes()),
        digest.to_string(),
        reason.clone(),
        replay,
        FROZEN_FUNDS_DELAY,
        now,
    );
    tx.insert_proposal(proposal.clone()).await?;
    tx.audit_insert(AdminAction {
        actor_role: role,
        actor_token_digest: digest.to_string(),
        action: "propose_frozen_funds_license".into(),
        subject: format!("license:{}", stored.id),
        before: None,
        after: Some(json!({ "dest": stored.dest })),
        reason: Some(reason),
    })
    .await?;
    tx.commit().await?;
    Ok(proposal)
}

/// Confirm frozen-funds license (superadmin, distinct, after 24h).
///
/// # Errors
/// Role / token / store.
pub async fn confirm_frozen_license(
    store: &impl ComplianceAdminStore,
    actor: &AdminContext,
    proposal_id: Uuid,
    now: OffsetDateTime,
) -> Result<FrozenFundsLicense, AppError> {
    let (digest, role) = require_admin(actor)?;
    require_role(role, &[AdminRole::Superadmin])?;
    let mut tx = store.admin_tx().await?;
    let proposal = tx.get_proposal(proposal_id).await?;
    require_distinct_tokens(&proposal.proposer_token_id, digest)?;
    if proposal.kind != "frozen_funds_license" || proposal.status != ProposalStatus::Pending {
        return Err(AppError::ProposalConflict("not a pending license"));
    }
    if now < proposal.confirm_not_before || now > proposal.expires_at {
        return Err(AppError::ProposalConflict("proposal not confirmable"));
    }
    tx.confirm_proposal(proposal_id, digest, now).await?;
    let license = tx.get_frozen_license(proposal.subject_id).await?;
    tx.audit_insert(AdminAction {
        actor_role: role,
        actor_token_digest: digest.to_string(),
        action: "confirm_frozen_funds_license".into(),
        subject: format!("license:{}", license.id),
        before: None,
        after: Some(json!({ "dest": license.dest })),
        reason: Some(proposal.reason),
    })
    .await?;
    tx.commit().await?;
    Ok(license)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::fakes::FakeComplianceStore;
    use crate::money::{evaluate_at_request, AmlDirection, AmlKind, AmlLeg, AmlPolicy};
    use crate::money::{FrozenFundsLicense, PINNED_WITHDRAW_BAND_MICRO};
    use time::Duration;

    fn t0() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(70_000)
    }

    fn admin(role: AdminRole, tok: &str) -> AdminContext {
        AdminContext::Admin {
            token_digest: tok.into(),
            role,
        }
    }

    #[test]
    fn exclusion_activity_and_limit_raise_delay() {
        let now = t0();
        let ex = SelfExclusion {
            id: Uuid::nil(),
            user: UserId(Uuid::nil()),
            starts_at: now,
            cooling_off_until: now + Duration::hours(24),
            lifted_at: None,
        };
        assert!(ex.is_active(now));
        assert!(!ex.cooling_off_elapsed(now));
        assert!(ex.cooling_off_elapsed(now + Duration::hours(24)));
        let lifted = SelfExclusion {
            lifted_at: Some(now),
            ..ex
        };
        assert!(!lifted.is_active(now));

        let user = UserId(Uuid::nil());
        let first = deposit_limit_after_write(None, 100, now, user);
        assert_eq!(first.limit_micro, 100);
        assert_eq!(effective_deposit_limit(&first, now), 100);
        let raised = deposit_limit_after_write(Some(&first), 200, now, user);
        assert_eq!(raised.limit_micro, 100);
        assert_eq!(raised.pending_limit_micro, Some(200));
        assert_eq!(effective_deposit_limit(&raised, now), 100);
        assert_eq!(
            effective_deposit_limit(&raised, now + DEPOSIT_LIMIT_RAISE_DELAY),
            200
        );
        let lowered = deposit_limit_after_write(Some(&first), 50, now, user);
        assert_eq!(lowered.limit_micro, 50);
        assert_eq!(lowered.pending_limit_micro, None);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn self_exclusion_lift_and_limits_and_flag_clear() {
        let store = FakeComplianceStore::new();
        let user = store.add_user("se-a");
        let now = t0();
        assert!(start_self_exclusion(&store, user, Duration::ZERO, now)
            .await
            .is_err());
        let ex = start_self_exclusion(&store, user, Duration::hours(24), now)
            .await
            .unwrap();
        assert!(start_self_exclusion(&store, user, Duration::hours(24), now)
            .await
            .is_err());
        assert!(propose_lift(
            &store,
            &admin(AdminRole::Finance, "fin"),
            ex.id,
            "too soon".into(),
            now
        )
        .await
        .is_err());
        assert!(propose_lift(
            &store,
            &admin(AdminRole::Ops, "ops"),
            ex.id,
            "r".into(),
            now + Duration::hours(24)
        )
        .await
        .is_err());
        assert!(propose_lift(
            &store,
            &admin(AdminRole::Finance, "fin"),
            ex.id,
            " ".into(),
            now + Duration::hours(24)
        )
        .await
        .is_err());

        let later = now + Duration::hours(24);
        let p = propose_lift(
            &store,
            &admin(AdminRole::Finance, "fin"),
            ex.id,
            "elapsed".into(),
            later,
        )
        .await
        .unwrap();
        let replay = propose_lift(
            &store,
            &admin(AdminRole::Finance, "fin"),
            ex.id,
            "elapsed".into(),
            later,
        )
        .await
        .unwrap();
        assert_eq!(p.id, replay.id);
        assert!(
            confirm_lift(&store, &admin(AdminRole::Finance, "fin"), p.id, later)
                .await
                .is_err()
        );
        assert!(
            confirm_lift(&store, &admin(AdminRole::Superadmin, "fin"), p.id, later)
                .await
                .is_err()
        );
        let lifted = confirm_lift(&store, &admin(AdminRole::Superadmin, "sa"), p.id, later)
            .await
            .unwrap();
        assert!(lifted.lifted_at.is_some());
        assert!(propose_lift(
            &store,
            &admin(AdminRole::Finance, "fin"),
            ex.id,
            "again".into(),
            later
        )
        .await
        .is_err());

        assert!(set_deposit_limit(&store, user, -1, now).await.is_err());
        let lim = set_deposit_limit(&store, user, 50, now).await.unwrap();
        assert_eq!(lim.limit_micro, 50);
        let raise = set_deposit_limit(&store, user, 80, now).await.unwrap();
        assert_eq!(raise.pending_limit_micro, Some(80));

        // Flag clear path (reuse AML evaluator).
        let policy = AmlPolicy::seed();
        for n in 1_u8..=4 {
            evaluate_at_request(
                &store,
                AmlLeg {
                    id: Uuid::from_u128(u128::from(n)),
                    user,
                    dest: "d".into(),
                    amount_micro: PINNED_WITHDRAW_BAND_MICRO,
                    at: now + Duration::minutes(i64::from(n)),
                    direction: AmlDirection::Withdrawal,
                },
                policy,
            )
            .await
            .unwrap();
        }
        let mut tx = store.admin_tx().await.unwrap();
        let flag = tx.open_aml_flags(user).await.unwrap().remove(0);
        tx.commit().await.unwrap();
        assert!(propose_clear_flag(
            &store,
            &admin(AdminRole::Ops, "o"),
            flag.id,
            "r".into(),
            now
        )
        .await
        .is_err());
        assert!(propose_clear_flag(
            &store,
            &admin(AdminRole::Finance, "fin"),
            flag.id,
            " ".into(),
            now
        )
        .await
        .is_err());
        let cp = propose_clear_flag(
            &store,
            &admin(AdminRole::Finance, "fin"),
            flag.id,
            "reviewed".into(),
            now,
        )
        .await
        .unwrap();
        let cp2 = propose_clear_flag(
            &store,
            &admin(AdminRole::Finance, "fin"),
            flag.id,
            "reviewed".into(),
            now,
        )
        .await
        .unwrap();
        assert_eq!(cp.id, cp2.id);
        let cleared = confirm_clear_flag(&store, &admin(AdminRole::Superadmin, "sa"), cp.id, now)
            .await
            .unwrap();
        assert!(!cleared.open);
        assert_eq!(cleared.rule, AmlKind::Structuring);
        assert!(propose_clear_flag(
            &store,
            &admin(AdminRole::Finance, "fin"),
            flag.id,
            "again".into(),
            now
        )
        .await
        .is_err());
        assert!(
            confirm_clear_flag(&store, &admin(AdminRole::Superadmin, "sa"), cp.id, now)
                .await
                .is_err()
        );

        let license = FrozenFundsLicense {
            id: Uuid::new_v4(),
            user,
            dest: "counsel-dest".into(),
            amount_micro: 10,
        };
        assert!(propose_frozen_license(
            &store,
            &admin(AdminRole::Ops, "o"),
            license.clone(),
            "r".into(),
            now
        )
        .await
        .is_err());
        assert!(propose_frozen_license(
            &store,
            &admin(AdminRole::Finance, "fin"),
            FrozenFundsLicense {
                dest: String::new(),
                ..license.clone()
            },
            "r".into(),
            now
        )
        .await
        .is_err());
        assert!(propose_frozen_license(
            &store,
            &admin(AdminRole::Finance, "fin"),
            FrozenFundsLicense {
                amount_micro: 0,
                ..license.clone()
            },
            "r".into(),
            now
        )
        .await
        .is_err());
        let lp = propose_frozen_license(
            &store,
            &admin(AdminRole::Finance, "fin"),
            license,
            "counsel".into(),
            now,
        )
        .await
        .unwrap();
        assert!(
            confirm_frozen_license(&store, &admin(AdminRole::Superadmin, "sa"), lp.id, now)
                .await
                .is_err()
        );
        let granted = confirm_frozen_license(
            &store,
            &admin(AdminRole::Superadmin, "sa"),
            lp.id,
            now + FROZEN_FUNDS_DELAY,
        )
        .await
        .unwrap();
        assert_eq!(granted.dest, "counsel-dest");
    }

    #[tokio::test]
    async fn confirm_lift_rejects_wrong_kind() {
        let store = FakeComplianceStore::new();
        let user = store.add_user("se-b");
        let now = t0();
        let mut tx = store.admin_tx().await.unwrap();
        let p = new_proposal(
            "ban_user",
            user.0,
            "h".into(),
            "fin".into(),
            "r".into(),
            "rk2".into(),
            Duration::ZERO,
            now,
        );
        let id = p.id;
        tx.insert_proposal(p).await.unwrap();
        tx.commit().await.unwrap();
        assert!(
            confirm_lift(&store, &admin(AdminRole::Superadmin, "sa"), id, now)
                .await
                .is_err()
        );
        assert!(
            confirm_frozen_license(&store, &admin(AdminRole::Superadmin, "sa"), id, now)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn self_exclusion_is_irreversible_until_the_stamped_cooling_off_expires() {
        let store = FakeComplianceStore::new();
        let user = store.add_user("se-c");
        let now = t0();
        let finance = admin(AdminRole::Finance, "fin");
        let root = admin(AdminRole::Superadmin, "sa");

        assert!(matches!(
            start_self_exclusion(&store, user, Duration::ZERO, now).await,
            Err(AppError::AdminForbidden("cooling-off must be positive"))
        ));
        let exclusion = start_self_exclusion(&store, user, Duration::hours(24), now)
            .await
            .unwrap();
        // Effective immediately and not restartable while it runs.
        assert!(exclusion.is_active(now));
        assert!(matches!(
            start_self_exclusion(&store, user, Duration::hours(1), now).await,
            Err(AppError::AdminForbidden("self-exclusion is already active"))
        ));
        // A lift cannot even be proposed before the cooling-off elapses.
        assert!(
            propose_lift(&store, &finance, exclusion.id, "early".into(), now)
                .await
                .is_err()
        );

        let after = now + Duration::hours(24);
        let proposal = propose_lift(&store, &finance, exclusion.id, "requested".into(), after)
            .await
            .unwrap();

        // Outside the confirm window.
        assert!(matches!(
            confirm_lift(
                &store,
                &root,
                proposal.id,
                proposal.expires_at + Duration::seconds(1)
            )
            .await,
            Err(AppError::ProposalConflict("proposal not confirmable"))
        ));
        // Defense in depth: a hand-written proposal that names an exclusion
        // still inside its cooling-off is refused at confirm time too, so the
        // window is enforced on both sides of the two-person command.
        let running =
            start_self_exclusion(&store, store.add_user("se-c2"), Duration::hours(24), after)
                .await
                .unwrap();
        let forged = {
            let mut tx = store.admin_tx().await.unwrap();
            let forged = new_proposal(
                "lift_self_exclusion",
                running.id,
                "h".into(),
                "fin".into(),
                "forged".into(),
                "rk-forged".into(),
                Duration::ZERO,
                after,
            );
            let id = forged.id;
            tx.insert_proposal(forged).await.unwrap();
            tx.commit().await.unwrap();
            id
        };
        assert!(matches!(
            confirm_lift(&store, &root, forged, after).await,
            Err(AppError::ProposalConflict("cooling-off has not elapsed"))
        ));

        let lifted = confirm_lift(&store, &root, proposal.id, after)
            .await
            .unwrap();
        assert_eq!(lifted.id, exclusion.id);
        assert_eq!(lifted.lifted_at, Some(after));
        assert!(!lifted.is_active(after));
        // Second confirm: the proposal is no longer pending.
        assert!(matches!(
            confirm_lift(&store, &root, proposal.id, after).await,
            Err(AppError::ProposalConflict("proposal is not pending"))
        ));
        // And the lifted exclusion can no longer be re-proposed.
        assert!(matches!(
            propose_lift(&store, &finance, exclusion.id, "twice".into(), after).await,
            Err(AppError::ProposalConflict("already lifted"))
        ));
    }

    /// Drive one at-request evaluation over the structuring threshold and
    /// return the id of the flag it opened.
    async fn open_flag_id(store: &FakeComplianceStore, user: UserId, now: OffsetDateTime) -> Uuid {
        evaluate_at_request(
            store,
            AmlLeg {
                id: Uuid::new_v4(),
                user,
                dest: "dest-x".into(),
                amount_micro: PINNED_WITHDRAW_BAND_MICRO,
                at: now,
                direction: AmlDirection::Withdrawal,
            },
            AmlPolicy {
                n: 1,
                ..AmlPolicy::seed()
            },
        )
        .await
        .unwrap();
        let mut tx = store.admin_tx().await.unwrap();
        let flags = tx.open_aml_flags(user).await.unwrap();
        tx.commit().await.unwrap();
        flags[0].id
    }

    #[tokio::test]
    async fn clearing_a_flag_twice_finds_no_pending_proposal() {
        let store = FakeComplianceStore::new();
        let user = store.add_user("se-d");
        let now = t0();
        let finance = admin(AdminRole::Finance, "fin");
        let root = admin(AdminRole::Superadmin, "sa");
        evaluate_at_request(
            &store,
            AmlLeg {
                id: Uuid::new_v4(),
                user,
                dest: "dest-x".into(),
                amount_micro: PINNED_WITHDRAW_BAND_MICRO,
                at: now,
                direction: AmlDirection::Withdrawal,
            },
            AmlPolicy {
                n: 1,
                ..AmlPolicy::seed()
            },
        )
        .await
        .unwrap();
        let flag = {
            let mut tx = store.admin_tx().await.unwrap();
            let flags = tx.open_aml_flags(user).await.unwrap();
            tx.commit().await.unwrap();
            flags[0].id
        };
        let proposal = propose_clear_flag(&store, &finance, flag, "reviewed".into(), now)
            .await
            .unwrap();
        assert!(matches!(
            confirm_clear_flag(&store, &finance, proposal.id, now).await,
            Err(AppError::AdminForbidden(_))
        ));
        assert!(
            !confirm_clear_flag(&store, &root, proposal.id, now)
                .await
                .unwrap()
                .open
        );
        assert!(matches!(
            confirm_clear_flag(&store, &root, proposal.id, now).await,
            Err(AppError::ProposalConflict("not a pending clear"))
        ));
        // Outside the 15-minute window the confirm is refused on timing, not
        // on state — proven on a still-pending proposal.
        let second = propose_clear_flag(
            &store,
            &finance,
            open_flag_id(&store, store.add_user("se-d2"), now).await,
            "second".into(),
            now,
        )
        .await
        .unwrap();
        assert!(matches!(
            confirm_clear_flag(
                &store,
                &root,
                second.id,
                second.expires_at + Duration::seconds(1)
            )
            .await,
            Err(AppError::ProposalConflict("proposal not confirmable"))
        ));
        // A closed flag cannot be re-proposed.
        assert!(matches!(
            propose_clear_flag(&store, &finance, flag, "again".into(), now).await,
            Err(AppError::ProposalConflict("flag is not open"))
        ));
    }
}
