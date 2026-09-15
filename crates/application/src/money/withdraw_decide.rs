//! Machine decision pass, dest-warmth, tighten-only, single-finance
//! exception, dual-control via `money_command_proposals`, expire-proposal.

use serde_json::json;
use sha2::{Digest, Sha256};
use time::Duration;
use uuid::Uuid;

use crate::error::AppError;
use crate::model::{AdminContext, AdminRole, Event, ProposalStatus, UserStatus, WithdrawalStatus};
use crate::ops::audit::{audit_for, principal_digest};
use crate::ports::{
    blocks_auto_approve, union_reasons, Clock, Combo, MoneyProposal, UserMoneyView, WithdrawLimits,
    WithdrawStore, WithdrawTx, WithdrawalId, WithdrawalRow,
};

use super::withdraw_request::{geo_context, is_fresh_clear, is_self_exclusion_egress};

const APPROVE_KIND: &str = "approve_withdrawal";
const DUAL_DELAY: Duration = Duration::minutes(15);
const CONFIRM_WINDOW: Duration = Duration::minutes(15);

/// Decision command against a held row.
#[derive(Debug, Clone)]
pub enum DecideCmd {
    Machine { id: WithdrawalId },
    FinanceApprove { id: WithdrawalId, reason: String },
    ProposeDual { id: WithdrawalId, reason: String },
    ConfirmDual { id: WithdrawalId },
    ExpireProposal { id: WithdrawalId },
    Deny { id: WithdrawalId, reason: String },
}

/// Decide / approve / deny / expire a withdrawal.
pub struct DecideWithdraw<'a, S: WithdrawStore, C: Clock> {
    pub store: &'a S,
    pub clock: &'a C,
    pub actor: AdminContext,
}

impl<S: WithdrawStore, C: Clock> DecideWithdraw<'_, S, C> {
    /// # Errors
    /// Illegal transition, permission, or store failure.
    pub async fn execute(&self, cmd: DecideCmd) -> Result<WithdrawalRow, AppError> {
        let id = cmd_id(&cmd);
        let user = self.store.withdrawal_user(id).await?;
        let mut tx = self.store.withdraw_tx().await?;
        tx.serialize_key(&format!("withdraw-decide:{}", id.0))
            .await?;
        tx.lock_user(user).await?;
        let row = tx.withdrawal_for_update(id).await?;
        if row.user != user {
            return Err(crate::error::StoreError::Invariant(
                "withdrawal owner changed after user lock",
            )
            .into());
        }
        let now = self.clock.now();
        let next = match cmd {
            DecideCmd::Machine { .. } => {
                require_machine(&self.actor)?;
                machine(tx.as_mut(), row, now).await?
            }
            DecideCmd::FinanceApprove { reason, .. } => {
                finance_approve(tx.as_mut(), row, &self.actor, &reason, now).await?
            }
            DecideCmd::ProposeDual { reason, .. } => {
                propose_dual(tx.as_mut(), row, &self.actor, &reason, now).await?
            }
            DecideCmd::ConfirmDual { .. } => {
                confirm_dual(tx.as_mut(), row, &self.actor, now).await?
            }
            DecideCmd::ExpireProposal { .. } => {
                require_machine(&self.actor)?;
                expire_proposal(tx.as_mut(), row, now).await?
            }
            DecideCmd::Deny { reason, .. } => {
                deny(tx.as_mut(), row, &self.actor, &reason, now).await?
            }
        };
        tx.commit().await?;
        Ok(next)
    }
}

fn cmd_id(cmd: &DecideCmd) -> WithdrawalId {
    match *cmd {
        DecideCmd::Machine { id }
        | DecideCmd::FinanceApprove { id, .. }
        | DecideCmd::ProposeDual { id, .. }
        | DecideCmd::ConfirmDual { id }
        | DecideCmd::ExpireProposal { id }
        | DecideCmd::Deny { id, .. } => id,
    }
}

async fn machine(
    tx: &mut dyn WithdrawTx,
    mut row: WithdrawalRow,
    now: time::OffsetDateTime,
) -> Result<WithdrawalRow, AppError> {
    if row.combo != Combo::W1 {
        return Err(AppError::IllegalTransition);
    }
    let limits = tx.limits().await?;
    let view = tx.user_money_view(row.user).await?;
    let current = live_reasons(tx, &row, &limits, &view, now).await?;
    let tightened = union_reasons(&row.risk_reasons, &current);
    row.risk_reasons = tightened.clone();
    row.decided_at = Some(now);
    let target = if blocks_auto_approve(&tightened) {
        Combo::W4
    } else {
        Combo::W2
    };
    let from = Combo::W1;
    row.combo = target;
    cas(tx, from, &row, "machine").await
}

async fn live_reasons(
    tx: &mut dyn WithdrawTx,
    row: &WithdrawalRow,
    limits: &WithdrawLimits,
    view: &UserMoneyView,
    now: time::OffsetDateTime,
) -> Result<Vec<String>, AppError> {
    let mut reasons = Vec::new();
    if limits.pause_withdrawals {
        reasons.push("withdrawals_paused".into());
    }
    if row.amount_micro >= limits.auto_approve_micro {
        reasons.push("amount_ge_auto".into());
    }
    if row.amount_micro >= limits.dual_control_micro {
        reasons.push("amount_ge_dual".into());
    }
    match view.status {
        UserStatus::Banned => reasons.push("banned".into()),
        UserStatus::ShadowLimited => reasons.push("shadow_limited".into()),
        UserStatus::Active => {}
    }
    if i64::from(view.kyc_tier) < limits.withdraw_kyc_tier {
        reasons.push("kyc_not_eligible".into());
    }
    if tx.open_aml_flag_count(row.user).await? > 0 {
        reasons.push("aml_open".into());
    }
    let sanctions = tx.latest_screening(row.user, "withdraw").await?;
    if !is_fresh_clear(sanctions.as_ref(), now) {
        reasons.push("sanctions_not_clear".into());
    }
    let geo = tx
        .latest_screening(row.user, &geo_context(&row.request_fingerprint))
        .await?;
    if !is_fresh_clear(geo.as_ref(), now) {
        reasons.push("geo_not_clear".into());
    }
    let warmth = tx.dest_warmth(&row.dest).await?;
    if !warmth.is_warm(*limits, now) {
        reasons.push("dest_not_warm".into());
    }
    if warmth.distinct_users >= 2 {
        reasons.push("dest_shared".into());
    }
    if warmth.is_refund_dest {
        reasons.push("dest_is_refund".into());
    }
    if view.self_excluded_until.is_some_and(|until| until > now)
        && !is_self_exclusion_egress(tx, row.user, &row.dest).await?
    {
        reasons.push("self_exclusion_new_dest".into());
    }
    Ok(reasons)
}

async fn finance_approve(
    tx: &mut dyn WithdrawTx,
    mut row: WithdrawalRow,
    actor: &AdminContext,
    reason: &str,
    now: time::OffsetDateTime,
) -> Result<WithdrawalRow, AppError> {
    require_finance(actor)?;
    require_reason(reason)?;
    if row.combo != Combo::W4 {
        return Err(AppError::IllegalTransition);
    }
    let limits = tx.limits().await?;
    if row.amount_micro >= limits.dual_control_micro
        || row
            .risk_reasons
            .iter()
            .any(|reason| reason == "amount_ge_dual")
    {
        return Err(AppError::AdminForbidden(
            "amount requires dual-control proposal",
        ));
    }
    let view = tx.user_money_view(row.user).await?;
    let live = live_reasons(tx, &row, &limits, &view, now).await?;
    if live.iter().any(|reason| hard_gate(reason))
        || row.risk_reasons.iter().any(|reason| hard_gate(reason))
    {
        return Err(AppError::AdminForbidden(
            "compliance-held withdrawal requires separate clearance",
        ));
    }
    row.risk_reasons = union_reasons(&row.risk_reasons, &live);
    consume_approval_cap(tx, &row, &limits, now).await?;
    insert_optional_audit(
        tx,
        audit_for(
            actor,
            "withdraw_finance_approve",
            format!("withdrawal:{}", row.id.0),
            None,
            Some(json!({"to": "W2"})),
            Some(reason.to_string()),
        ),
    )
    .await?;
    row.combo = Combo::W2;
    row.decided_at = Some(now);
    cas(tx, Combo::W4, &row, "finance").await
}

async fn propose_dual(
    tx: &mut dyn WithdrawTx,
    mut row: WithdrawalRow,
    actor: &AdminContext,
    reason: &str,
    now: time::OffsetDateTime,
) -> Result<WithdrawalRow, AppError> {
    require_finance(actor)?;
    require_reason(reason)?;
    let digest = principal_digest(actor)?.to_string();
    if row.combo == Combo::W5 {
        let existing = tx
            .open_proposal_for(row.id.0, APPROVE_KIND)
            .await?
            .ok_or(AppError::ProposalConflict("no open proposal"))?;
        if existing.kind == APPROVE_KIND
            && existing.subject_id == row.id.0
            && existing.payload_hash == payload_hash(row.id, row.amount_micro)
            && existing.proposer_token_id == digest
            && existing.reason == reason
        {
            return Ok(row);
        }
        return Err(AppError::ProposalConflict("proposal payload mismatch"));
    }
    if row.combo != Combo::W4 {
        return Err(AppError::IllegalTransition);
    }
    let confirm_not_before = now + DUAL_DELAY;
    let proposal_id = Uuid::new_v4();
    let proposal = MoneyProposal {
        id: proposal_id,
        kind: APPROVE_KIND.to_string(),
        subject_id: row.id.0,
        payload_hash: payload_hash(row.id, row.amount_micro),
        proposer_token_id: digest,
        confirmer_token_id: None,
        reason: reason.to_string(),
        status: ProposalStatus::Pending,
        confirm_not_before,
        expires_at: confirm_not_before + CONFIRM_WINDOW,
        replay_key: format!("approve-withdrawal:{proposal_id}"),
    };
    tx.insert_proposal(&proposal).await?;
    insert_optional_audit(
        tx,
        audit_for(
            actor,
            "withdraw_propose_dual",
            format!("withdrawal:{}", row.id.0),
            None,
            Some(json!({"proposal": proposal.id.to_string()})),
            Some(reason.to_string()),
        ),
    )
    .await?;
    row.combo = Combo::W5;
    cas(tx, Combo::W4, &row, "propose-dual").await
}

async fn confirm_dual(
    tx: &mut dyn WithdrawTx,
    mut row: WithdrawalRow,
    actor: &AdminContext,
    now: time::OffsetDateTime,
) -> Result<WithdrawalRow, AppError> {
    require_finance_or_super(actor)?;
    if row.combo != Combo::W5 {
        return Err(AppError::IllegalTransition);
    }
    let confirmer = principal_digest(actor)?.to_string();
    let mut proposal = tx
        .open_proposal_for(row.id.0, APPROVE_KIND)
        .await?
        .ok_or(AppError::ProposalConflict("no open proposal"))?;
    if confirmer == proposal.proposer_token_id {
        return Err(AppError::AdminForbidden(
            "confirming token must be distinct",
        ));
    }
    if now < proposal.confirm_not_before {
        return Err(AppError::ProposalConflict("delay has not elapsed"));
    }
    if now >= proposal.expires_at {
        return Err(AppError::ProposalConflict("proposal expired"));
    }
    if proposal.kind != APPROVE_KIND
        || proposal.payload_hash != payload_hash(row.id, row.amount_micro)
        || proposal.replay_key != format!("approve-withdrawal:{}", proposal.id)
    {
        return Err(AppError::ProposalConflict("proposal payload mismatch"));
    }
    let limits = tx.limits().await?;
    let view = tx.user_money_view(row.user).await?;
    let live = live_reasons(tx, &row, &limits, &view, now).await?;
    if live.iter().any(|reason| hard_gate(reason)) {
        return Err(AppError::AdminForbidden(
            "current money gate blocks approval",
        ));
    }
    row.risk_reasons = union_reasons(&row.risk_reasons, &live);
    consume_approval_cap(tx, &row, &limits, now).await?;
    proposal.status = ProposalStatus::Confirmed;
    proposal.confirmer_token_id = Some(confirmer);
    tx.save_proposal(&proposal).await?;
    insert_optional_audit(
        tx,
        audit_for(
            actor,
            "withdraw_confirm_dual",
            format!("withdrawal:{}", row.id.0),
            None,
            Some(json!({"proposal": proposal.id.to_string()})),
            Some(proposal.reason.clone()),
        ),
    )
    .await?;
    row.combo = Combo::W2;
    row.decided_at = Some(now);
    cas(tx, Combo::W5, &row, "confirm-dual").await
}

async fn expire_proposal(
    tx: &mut dyn WithdrawTx,
    mut row: WithdrawalRow,
    now: time::OffsetDateTime,
) -> Result<WithdrawalRow, AppError> {
    if row.combo != Combo::W5 {
        return Err(AppError::IllegalTransition);
    }
    let Some(mut proposal) = tx.open_proposal_for(row.id.0, APPROVE_KIND).await? else {
        return Err(AppError::ProposalConflict("no open proposal"));
    };
    if now < proposal.expires_at {
        return Err(AppError::ProposalConflict("proposal has not expired"));
    }
    proposal.status = ProposalStatus::Expired;
    tx.save_proposal(&proposal).await?;
    row.combo = Combo::W4;
    cas(tx, Combo::W5, &row, "expire-proposal").await
}

async fn deny(
    tx: &mut dyn WithdrawTx,
    mut row: WithdrawalRow,
    actor: &AdminContext,
    reason: &str,
    now: time::OffsetDateTime,
) -> Result<WithdrawalRow, AppError> {
    require_finance(actor)?;
    require_reason(reason)?;
    let target = row.combo.deny_target().ok_or(AppError::IllegalTransition)?;
    let from = row.combo;
    let key = format!("withdraw-release:{}", row.id.0);
    let release = tx.apply_release(row.user, row.amount_micro, &key).await?;
    row.release_tx_id = Some(release);
    row.combo = target;
    row.decided_at = Some(now);
    insert_optional_audit(
        tx,
        audit_for(
            actor,
            "withdraw_deny",
            format!("withdrawal:{}", row.id.0),
            None,
            Some(json!({"reason": reason})),
            Some(reason.to_string()),
        ),
    )
    .await?;
    if let Some(mut proposal) = tx.open_proposal_for(row.id.0, APPROVE_KIND).await? {
        proposal.status = ProposalStatus::Rejected;
        tx.save_proposal(&proposal).await?;
    }
    let _ = WithdrawalStatus::Denied;
    cas(tx, from, &row, "deny").await
}

async fn cas(
    tx: &mut dyn WithdrawTx,
    from: Combo,
    row: &WithdrawalRow,
    actor: &str,
) -> Result<WithdrawalRow, AppError> {
    if !tx.cas_withdrawal(row.id, from, row).await? {
        return Err(AppError::IllegalTransition);
    }
    tx.append_withdrawal_event(
        row.id,
        "cas",
        actor,
        json!({
            "from": from.label(),
            "to": row.combo.label(),
        }),
    )
    .await?;
    let _ = tx
        .record_event(Event {
            event_type: "WithdrawalDecided",
            aggregate_type: "withdrawal",
            aggregate_id: row.id.0,
            payload: json!({"to": row.combo.label()}),
        })
        .await?;
    Ok(row.clone())
}

fn require_finance(actor: &AdminContext) -> Result<(), AppError> {
    match actor {
        AdminContext::Admin {
            role: AdminRole::Finance,
            ..
        } => Ok(()),
        _ => Err(AppError::AdminForbidden("finance role required")),
    }
}

fn require_machine(actor: &AdminContext) -> Result<(), AppError> {
    if matches!(actor, AdminContext::Machine) {
        return Ok(());
    }
    Err(AppError::AdminForbidden("machine actor required"))
}

fn require_finance_or_super(actor: &AdminContext) -> Result<(), AppError> {
    match actor {
        AdminContext::Admin {
            role: AdminRole::Finance | AdminRole::Superadmin,
            ..
        } => Ok(()),
        _ => Err(AppError::AdminForbidden(
            "finance or superadmin role required",
        )),
    }
}

fn require_reason(reason: &str) -> Result<(), AppError> {
    if reason.trim().is_empty() {
        return Err(AppError::AdminForbidden("reason is required"));
    }
    Ok(())
}

fn hard_gate(reason: &str) -> bool {
    matches!(
        reason,
        "withdrawals_paused"
            | "banned"
            | "kyc_not_eligible"
            | "aml_open"
            | "sanctions_not_clear"
            | "geo_not_clear"
            | "self_exclusion_new_dest"
    )
}

async fn consume_approval_cap(
    tx: &mut dyn WithdrawTx,
    row: &WithdrawalRow,
    limits: &WithdrawLimits,
    now: time::OffsetDateTime,
) -> Result<(), AppError> {
    tx.lock_cap("withdraw-approve-daily").await?;
    let used = tx
        .finance_approve_sum_since(now - Duration::hours(24))
        .await?;
    if used.saturating_add(row.amount_micro) > limits.approve_daily_cap_micro {
        return Err(AppError::AdminForbidden("daily finance-approve cap"));
    }
    tx.append_withdrawal_event(
        row.id,
        "finance-approve",
        "finance",
        json!({"amount_micro": row.amount_micro}),
    )
    .await?;
    Ok(())
}

fn payload_hash(id: WithdrawalId, amount: i64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(id.0.as_bytes());
    hasher.update(amount.to_le_bytes());
    hasher
        .finalize()
        .iter()
        .fold(String::new(), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        })
}

struct WithdrawIoAudit<'a>(&'a mut dyn WithdrawTx);

impl WithdrawIoAudit<'_> {
    async fn insert(&mut self, action: crate::model::AdminAction) -> Result<(), AppError> {
        crate::ports::AuditWrite::audit_insert(self.0, action).await?;
        Ok(())
    }
}

async fn insert_optional_audit(
    tx: &mut dyn WithdrawTx,
    audit: Option<crate::model::AdminAction>,
) -> Result<(), AppError> {
    let Some(audit) = audit else {
        return Ok(());
    };
    WithdrawIoAudit(tx).insert(audit).await
}

/// Helper used by tests to seed a W4/W1 row through the real request path.
///
/// # Errors
/// Returns the same store or transition errors as [`DecideWithdraw::execute`].
pub async fn decide_machine<S: WithdrawStore, C: Clock>(
    store: &S,
    clock: &C,
    id: WithdrawalId,
) -> Result<WithdrawalRow, AppError> {
    DecideWithdraw {
        store,
        clock,
        actor: AdminContext::Machine,
    }
    .execute(DecideCmd::Machine { id })
    .await
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines, clippy::unwrap_used)]

    use super::*;
    use crate::error::StoreError;
    use crate::ports::withdraw_fakes::{dest_a, dest_b, FakeScreen, FakeWithdrawStore};
    use crate::ports::withdraw_request::{RequestClock, RequestWithdraw};
    use crate::ports::RequestWithdrawCmd;
    use crate::ports::WithdrawStore;
    use crate::ports::{LandingState, ScreenVerdict};
    use std::net::IpAddr;

    fn finance(digest: &str) -> AdminContext {
        AdminContext::Admin {
            token_digest: digest.into(),
            role: AdminRole::Finance,
        }
    }

    fn superadmin(digest: &str) -> AdminContext {
        AdminContext::Admin {
            token_digest: digest.into(),
            role: AdminRole::Superadmin,
        }
    }

    fn clear() -> FakeScreen {
        let now = OffsetDateTime_now();
        FakeScreen {
            verdict: ScreenVerdict::Clear {
                checked_at: now,
                expires_at: now + Duration::hours(24),
                policy_version: "1".into(),
            },
            fail: false,
        }
    }

    #[allow(non_snake_case)]
    fn OffsetDateTime_now() -> time::OffsetDateTime {
        time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    async fn held_w1(store: &FakeWithdrawStore, amount: i64) -> WithdrawalRow {
        let user = store.seed_user(crate::model::UserStatus::Active, 2);
        store.credit(user, amount.saturating_mul(4).max(20_000_000));
        let screen = clear();
        let receipt = RequestWithdraw {
            store,
            clock: &RequestClock(store.now()),
            geo: &screen,
            sanctions: &screen,
        }
        .execute(RequestWithdrawCmd {
            user,
            amount_micro: amount,
            dest: dest_a(),
            client_ip: Some(IpAddr::from([203, 0, 113, 9])),
            idempotency_key: Some(format!("k-{amount}-{}", user.0)),
        })
        .await
        .unwrap();
        let mut tx = store.withdraw_tx().await.unwrap();
        tx.withdrawal_for_update(receipt.id.unwrap()).await.unwrap()
    }

    #[tokio::test]
    async fn machine_sends_novel_dest_to_review_and_tighten_only_sticks() {
        let store = FakeWithdrawStore::new();
        let row = held_w1(&store, 5_000_000).await;
        let decided = decide_machine(&store, &RequestClock(store.now()), row.id)
            .await
            .unwrap();
        assert_eq!(decided.combo, Combo::W4);
        // Raising nothing / warming dest later must not auto-approve.
        store.advance(100);
        let again = decide_machine(&store, &RequestClock(store.now()), row.id).await;
        assert!(again.is_err());
        assert!(decided
            .risk_reasons
            .iter()
            .any(|reason| reason == "dest_not_warm"));
        let _ = dest_b();
        let _ = LandingState::Prepared;
    }

    #[tokio::test]
    async fn user_lock_precedes_withdrawal_row_lock() {
        let store = FakeWithdrawStore::new();
        let row = held_w1(&store, 5_000_000).await;
        store.enforce_withdraw_lock_order();

        let decided = decide_machine(&store, &RequestClock(store.now()), row.id)
            .await
            .unwrap();

        assert_eq!(decided.combo, Combo::W4);
    }

    #[tokio::test]
    async fn machine_rechecks_current_kill_switch_and_screening_freshness() {
        let store = FakeWithdrawStore::new();
        let row = held_w1(&store, 5_000_000).await;
        store.set_pause(true);
        let later = store.now() + Duration::hours(25);
        let decided = decide_machine(&store, &RequestClock(later), row.id)
            .await
            .unwrap();
        assert_eq!(decided.combo, Combo::W4);
        assert!(decided
            .risk_reasons
            .iter()
            .any(|reason| reason == "withdrawals_paused"));
        assert!(decided
            .risk_reasons
            .iter()
            .any(|reason| reason == "sanctions_not_clear"));
        assert!(decided
            .risk_reasons
            .iter()
            .any(|reason| reason == "geo_not_clear"));
    }

    #[tokio::test]
    async fn decision_snapshots_refund_and_shared_destination_risk_that_appeared_later() {
        let store = FakeWithdrawStore::new();
        let row = held_w1(&store, 5_000_000).await;
        store.mark_refund_dest(&row.dest);
        store.set_dest_distinct_users(&row.dest, 2);

        let decided = decide_machine(&store, &RequestClock(store.now()), row.id)
            .await
            .unwrap();

        assert!(decided
            .risk_reasons
            .iter()
            .any(|reason| reason == "dest_is_refund"));
        assert!(decided
            .risk_reasons
            .iter()
            .any(|reason| reason == "dest_shared"));
    }

    #[tokio::test]
    async fn warm_dest_under_auto_and_clear_auto_approves() {
        let store = FakeWithdrawStore::new();
        let user = store.seed_user(crate::model::UserStatus::Active, 2);
        store.credit(user, 500_000_000);
        // Pretend a prior settled withdrawal warmed the dest.
        {
            let mut tx = store.withdraw_tx().await.unwrap();
            tx.lock_user(user).await.unwrap();
            let hold = tx.apply_hold(user, 100_000_000, "warm-hold").await.unwrap();
            let settle = tx.apply_settle(100_000_000, "warm-settle").await.unwrap();
            let row = WithdrawalRow {
                id: WithdrawalId(Uuid::new_v4()),
                user,
                dest: dest_a(),
                amount_micro: 100_000_000,
                combo: Combo::W10,
                hold_tx_id: hold,
                release_tx_id: None,
                settle_tx_id: Some(settle),
                request_fingerprint: "prior".into(),
                risk_reasons: vec![],
                requested_at: store.now() - Duration::hours(100),
                decided_at: Some(store.now() - Duration::hours(100)),
                sent_at: Some(store.now() - Duration::hours(100)),
                settled_at: Some(store.now() - Duration::hours(100)),
            };
            tx.insert_withdrawal(&row).await.unwrap();
            tx.commit().await.unwrap();
        }
        let screen = clear();
        let receipt = RequestWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            geo: &screen,
            sanctions: &screen,
        }
        .execute(RequestWithdrawCmd {
            user,
            amount_micro: 5_000_000,
            dest: dest_a(),
            client_ip: Some(IpAddr::from([203, 0, 113, 9])),
            idempotency_key: Some("warm-req".into()),
        })
        .await
        .unwrap();
        let decided = decide_machine(&store, &RequestClock(store.now()), receipt.id.unwrap())
            .await
            .unwrap();
        assert_eq!(decided.combo, Combo::W2);
    }

    #[tokio::test]
    async fn single_finance_exception_and_dual_control_and_expire() {
        let store = FakeWithdrawStore::new();
        let small = held_w1(&store, 5_000_000).await;
        decide_machine(&store, &RequestClock(store.now()), small.id)
            .await
            .unwrap();
        let approved = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            actor: finance("fin-a"),
        }
        .execute(DecideCmd::FinanceApprove {
            id: small.id,
            reason: "ok".into(),
        })
        .await
        .unwrap();
        assert_eq!(approved.combo, Combo::W2);

        store.set_dual(1);
        let large = held_w1(&store, 6_000_000).await;
        decide_machine(&store, &RequestClock(store.now()), large.id)
            .await
            .unwrap();
        let err = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            actor: finance("fin-a"),
        }
        .execute(DecideCmd::FinanceApprove {
            id: large.id,
            reason: "too big".into(),
        })
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::AdminForbidden(_)));

        store.set_dual(500_000_000);
        let loosened_config = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            actor: finance("fin-a"),
        }
        .execute(DecideCmd::FinanceApprove {
            id: large.id,
            reason: "still too big at request time".into(),
        })
        .await
        .unwrap_err();
        assert!(matches!(loosened_config, AppError::AdminForbidden(_)));
        let proposed = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            actor: finance("fin-a"),
        }
        .execute(DecideCmd::ProposeDual {
            id: large.id,
            reason: "large".into(),
        })
        .await
        .unwrap();
        assert_eq!(proposed.combo, Combo::W5);
        let proposed_replay = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            actor: finance("fin-a"),
        }
        .execute(DecideCmd::ProposeDual {
            id: large.id,
            reason: "large".into(),
        })
        .await
        .unwrap();
        assert_eq!(proposed_replay.combo, Combo::W5);
        let early = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            actor: finance("fin-b"),
        }
        .execute(DecideCmd::ConfirmDual { id: large.id })
        .await
        .unwrap_err();
        assert!(matches!(early, AppError::ProposalConflict(_)));
        let same = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now() + Duration::minutes(16)),
            actor: finance("fin-a"),
        }
        .execute(DecideCmd::ConfirmDual { id: large.id })
        .await
        .unwrap_err();
        assert!(matches!(same, AppError::AdminForbidden(_)));
        let confirmed = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now() + Duration::minutes(16)),
            actor: finance("fin-b"),
        }
        .execute(DecideCmd::ConfirmDual { id: large.id })
        .await
        .unwrap();
        assert_eq!(confirmed.combo, Combo::W2);

        let expire_row = held_w1(&store, 7_000_000).await;
        decide_machine(&store, &RequestClock(store.now()), expire_row.id)
            .await
            .unwrap();
        DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            actor: finance("fin-a"),
        }
        .execute(DecideCmd::ProposeDual {
            id: expire_row.id,
            reason: "later".into(),
        })
        .await
        .unwrap();
        let expired = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now() + Duration::minutes(31)),
            actor: AdminContext::Machine,
        }
        .execute(DecideCmd::ExpireProposal { id: expire_row.id })
        .await
        .unwrap();
        assert_eq!(expired.combo, Combo::W4);
        let reproposed = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now() + Duration::minutes(32)),
            actor: finance("fin-a"),
        }
        .execute(DecideCmd::ProposeDual {
            id: expire_row.id,
            reason: "later again".into(),
        })
        .await
        .unwrap();
        assert_eq!(reproposed.combo, Combo::W5);
    }

    #[tokio::test]
    async fn single_finance_cannot_bypass_aml_or_exact_role_and_requires_reason() {
        let store = FakeWithdrawStore::new();
        let flagged = held_w1(&store, 5_000_000).await;
        decide_machine(&store, &RequestClock(store.now()), flagged.id)
            .await
            .unwrap();
        store.open_aml(flagged.user);
        let err = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            actor: finance("fin-a"),
        }
        .execute(DecideCmd::FinanceApprove {
            id: flagged.id,
            reason: "looks fine".into(),
        })
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::AdminForbidden(_)));

        let wrong_role = held_w1(&store, 5_000_000).await;
        decide_machine(&store, &RequestClock(store.now()), wrong_role.id)
            .await
            .unwrap();
        let err = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            actor: superadmin("root-a"),
        }
        .execute(DecideCmd::FinanceApprove {
            id: wrong_role.id,
            reason: "wrong seat".into(),
        })
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::AdminForbidden(_)));

        let no_reason = held_w1(&store, 5_000_000).await;
        decide_machine(&store, &RequestClock(store.now()), no_reason.id)
            .await
            .unwrap();
        let err = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            actor: finance("fin-a"),
        }
        .execute(DecideCmd::FinanceApprove {
            id: no_reason.id,
            reason: "   ".into(),
        })
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::AdminForbidden(_)));
    }

    #[tokio::test]
    async fn dual_confirmation_consumes_the_daily_approval_cap() {
        let store = FakeWithdrawStore::new();
        store.set_dual(1);
        store.set_approve_daily_cap(10_000_000);

        let first = held_w1(&store, 6_000_000).await;
        decide_machine(&store, &RequestClock(store.now()), first.id)
            .await
            .unwrap();
        DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            actor: finance("fin-a"),
        }
        .execute(DecideCmd::ProposeDual {
            id: first.id,
            reason: "first".into(),
        })
        .await
        .unwrap();
        DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now() + Duration::minutes(16)),
            actor: finance("fin-b"),
        }
        .execute(DecideCmd::ConfirmDual { id: first.id })
        .await
        .unwrap();

        let second = held_w1(&store, 6_000_000).await;
        decide_machine(&store, &RequestClock(store.now()), second.id)
            .await
            .unwrap();
        DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            actor: finance("fin-a"),
        }
        .execute(DecideCmd::ProposeDual {
            id: second.id,
            reason: "second".into(),
        })
        .await
        .unwrap();
        let err = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now() + Duration::minutes(16)),
            actor: finance("fin-b"),
        }
        .execute(DecideCmd::ConfirmDual { id: second.id })
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::AdminForbidden(_)));
    }

    #[tokio::test]
    async fn deny_releases_the_hold_exactly_once() {
        let store = FakeWithdrawStore::new();
        let row = held_w1(&store, 5_000_000).await;
        let before = store.withheld();
        let denied = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            actor: finance("fin-a"),
        }
        .execute(DecideCmd::Deny {
            id: row.id,
            reason: "no".into(),
        })
        .await
        .unwrap();
        assert_eq!(denied.combo, Combo::W11);
        assert!(denied.release_tx_id.is_some());
        assert_eq!(store.withheld(), before - 5_000_000);
        let replay = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            actor: finance("fin-a"),
        }
        .execute(DecideCmd::Deny {
            id: row.id,
            reason: "no".into(),
        })
        .await
        .unwrap_err();
        assert_eq!(replay, AppError::IllegalTransition);
    }

    #[tokio::test]
    async fn machine_and_expiry_edges_reject_human_principals() {
        let store = FakeWithdrawStore::new();
        let machine_row = held_w1(&store, 5_000_000).await;
        let err = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            actor: finance("fin-a"),
        }
        .execute(DecideCmd::Machine { id: machine_row.id })
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::AdminForbidden(_)));

        decide_machine(&store, &RequestClock(store.now()), machine_row.id)
            .await
            .unwrap();
        DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            actor: finance("fin-a"),
        }
        .execute(DecideCmd::ProposeDual {
            id: machine_row.id,
            reason: "dual".into(),
        })
        .await
        .unwrap();
        let err = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now() + Duration::minutes(31)),
            actor: finance("fin-b"),
        }
        .execute(DecideCmd::ExpireProposal { id: machine_row.id })
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::AdminForbidden(_)));
    }

    #[tokio::test]
    async fn decision_machine_and_proposal_errors_fail_closed() {
        let store = FakeWithdrawStore::new();
        let banned = held_w1(&store, 5_000_000).await;
        store.set_status(banned.user, UserStatus::Banned);
        let decided = decide_machine(&store, &RequestClock(store.now()), banned.id)
            .await
            .unwrap();
        assert!(decided.risk_reasons.iter().any(|reason| reason == "banned"));

        let constrained = held_w1(&store, 6_000_000).await;
        store.set_status(constrained.user, UserStatus::ShadowLimited);
        store.set_kyc(constrained.user, 0);
        store.set_self_excluded(constrained.user, store.now() + Duration::days(1));
        let decided = decide_machine(&store, &RequestClock(store.now()), constrained.id)
            .await
            .unwrap();
        for reason in [
            "shadow_limited",
            "kyc_not_eligible",
            "self_exclusion_new_dest",
        ] {
            assert!(decided.risk_reasons.iter().any(|item| item == reason));
        }

        let proposed_row = held_w1(&store, 7_000_000).await;
        decide_machine(&store, &RequestClock(store.now()), proposed_row.id)
            .await
            .unwrap();
        let invalid_confirm = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            actor: finance("fin-b"),
        }
        .execute(DecideCmd::ConfirmDual {
            id: proposed_row.id,
        })
        .await
        .unwrap_err();
        assert_eq!(invalid_confirm, AppError::IllegalTransition);
        let invalid_expire = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            actor: AdminContext::Machine,
        }
        .execute(DecideCmd::ExpireProposal {
            id: proposed_row.id,
        })
        .await
        .unwrap_err();
        assert_eq!(invalid_expire, AppError::IllegalTransition);

        DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            actor: finance("fin-a"),
        }
        .execute(DecideCmd::ProposeDual {
            id: proposed_row.id,
            reason: "original".into(),
        })
        .await
        .unwrap();
        let mismatch = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            actor: finance("fin-a"),
        }
        .execute(DecideCmd::ProposeDual {
            id: proposed_row.id,
            reason: "different".into(),
        })
        .await
        .unwrap_err();
        assert!(matches!(mismatch, AppError::ProposalConflict(_)));
        let early_expire = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            actor: AdminContext::Machine,
        }
        .execute(DecideCmd::ExpireProposal {
            id: proposed_row.id,
        })
        .await
        .unwrap_err();
        assert!(matches!(early_expire, AppError::ProposalConflict(_)));
        let expired_confirm = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now() + Duration::minutes(31)),
            actor: finance("fin-b"),
        }
        .execute(DecideCmd::ConfirmDual {
            id: proposed_row.id,
        })
        .await
        .unwrap_err();
        assert!(matches!(expired_confirm, AppError::ProposalConflict(_)));
        let denied = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            actor: finance("fin-b"),
        }
        .execute(DecideCmd::Deny {
            id: proposed_row.id,
            reason: "reject".into(),
        })
        .await
        .unwrap();
        assert_eq!(denied.combo, Combo::W13);

        let hard_gate = held_w1(&store, 8_000_000).await;
        decide_machine(&store, &RequestClock(store.now()), hard_gate.id)
            .await
            .unwrap();
        DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            actor: finance("fin-a"),
        }
        .execute(DecideCmd::ProposeDual {
            id: hard_gate.id,
            reason: "dual".into(),
        })
        .await
        .unwrap();
        store.open_aml(hard_gate.user);
        let blocked = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now() + Duration::minutes(16)),
            actor: superadmin("root-b"),
        }
        .execute(DecideCmd::ConfirmDual { id: hard_gate.id })
        .await
        .unwrap_err();
        assert!(matches!(blocked, AppError::AdminForbidden(_)));
    }

    #[tokio::test]
    async fn decision_owner_role_proposal_and_cas_invariants_fail_closed() {
        let audit_store = FakeWithdrawStore::new();
        let mut audit_tx = audit_store.withdraw_tx().await.unwrap();
        insert_optional_audit(audit_tx.as_mut(), None)
            .await
            .unwrap();

        let owner_store = FakeWithdrawStore::new();
        let owner_row = held_w1(&owner_store, 5_000_000).await;
        let wrong_user = owner_store.seed_user(UserStatus::Active, 2);
        owner_store.override_withdrawal_user(owner_row.id, wrong_user);
        let err = decide_machine(&owner_store, &RequestClock(owner_store.now()), owner_row.id)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Store(StoreError::Invariant(_))));

        let cas_store = FakeWithdrawStore::new();
        let cas_row = held_w1(&cas_store, 5_000_000).await;
        cas_store.force_next_cas_miss();
        assert_eq!(
            decide_machine(&cas_store, &RequestClock(cas_store.now()), cas_row.id)
                .await
                .unwrap_err(),
            AppError::IllegalTransition
        );

        let store = FakeWithdrawStore::new();
        store.set_auto_approve(1);
        let row = held_w1(&store, 5_000_000).await;
        let decided = decide_machine(&store, &RequestClock(store.now()), row.id)
            .await
            .unwrap();
        assert!(decided
            .risk_reasons
            .iter()
            .any(|reason| reason == "amount_ge_auto"));

        let raw = held_w1(&store, 5_000_000).await;
        for command in [
            DecideCmd::FinanceApprove {
                id: raw.id,
                reason: "premature".into(),
            },
            DecideCmd::ProposeDual {
                id: raw.id,
                reason: "premature".into(),
            },
        ] {
            let err = DecideWithdraw {
                store: &store,
                clock: &RequestClock(store.now()),
                actor: finance("fin-a"),
            }
            .execute(command)
            .await
            .unwrap_err();
            assert_eq!(err, AppError::IllegalTransition);
        }

        let malformed = held_w1(&store, 6_000_000).await;
        decide_machine(&store, &RequestClock(store.now()), malformed.id)
            .await
            .unwrap();
        DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            actor: finance("fin-a"),
        }
        .execute(DecideCmd::ProposeDual {
            id: malformed.id,
            reason: "dual".into(),
        })
        .await
        .unwrap();
        let mut tx = store.withdraw_tx().await.unwrap();
        let mut proposal = tx
            .open_proposal_for(malformed.id.0, APPROVE_KIND)
            .await
            .unwrap()
            .unwrap();
        proposal.payload_hash = "corrupt".into();
        tx.save_proposal(&proposal).await.unwrap();
        tx.commit().await.unwrap();
        let err = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now() + Duration::minutes(16)),
            actor: finance("fin-b"),
        }
        .execute(DecideCmd::ConfirmDual { id: malformed.id })
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::ProposalConflict(_)));

        let no_open = held_w1(&store, 7_000_000).await;
        decide_machine(&store, &RequestClock(store.now()), no_open.id)
            .await
            .unwrap();
        DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            actor: finance("fin-a"),
        }
        .execute(DecideCmd::ProposeDual {
            id: no_open.id,
            reason: "dual".into(),
        })
        .await
        .unwrap();
        let err = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            actor: AdminContext::Machine,
        }
        .execute(DecideCmd::ConfirmDual { id: no_open.id })
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::AdminForbidden(_)));
        let mut tx = store.withdraw_tx().await.unwrap();
        let mut proposal = tx
            .open_proposal_for(no_open.id.0, APPROVE_KIND)
            .await
            .unwrap()
            .unwrap();
        proposal.status = ProposalStatus::Expired;
        tx.save_proposal(&proposal).await.unwrap();
        tx.commit().await.unwrap();
        let err = DecideWithdraw {
            store: &store,
            clock: &RequestClock(store.now() + Duration::minutes(31)),
            actor: AdminContext::Machine,
        }
        .execute(DecideCmd::ExpireProposal { id: no_open.id })
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::ProposalConflict(_)));
    }
}
