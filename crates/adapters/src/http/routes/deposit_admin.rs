// Deposit hold command handlers (D32).
//
// The `{id}` in a propose path is a deposit id; the `{id}` in a confirm
// path is the immutable money-command proposal id. This module contains no
// state-machine logic: authority, distinct-token checks, TTL validation,
// source locking, ledger effects, and audit/event atomicity live in
// `application::credit_deposit`.

use application::credit_deposit::{
    confirm_deposit_refund, confirm_manual_deposit_admission, propose_deposit_command,
    DepositReceipt, RefundReceipt,
};
use application::model::{AdminContext, DepositId};
use application::money::{MoneyProposal, ProposalStatus};
use application::ports::{Clock, Store};
use axum::extract::{Extension, Path, State};
use axum::routing::post;
use axum::{Json, Router};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use super::super::dto::{DualControlBody, ProposalDto};
use super::super::error::ApiResult;
use super::AppState;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct DepositAdmissionDto {
    pub deposit_id: Uuid,
    pub ledger_txn: Option<Uuid>,
    pub collected_micro: i64,
    pub replayed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct DepositRefundDto {
    pub deposit_id: Uuid,
    pub payment_id: Uuid,
    pub dest: String,
    pub replayed: bool,
}

impl From<DepositReceipt> for DepositAdmissionDto {
    fn from(receipt: DepositReceipt) -> Self {
        Self {
            deposit_id: receipt.deposit_id.0,
            ledger_txn: receipt.ledger_txn,
            collected_micro: receipt.collected_micro,
            replayed: receipt.replayed,
        }
    }
}

impl From<RefundReceipt> for DepositRefundDto {
    fn from(receipt: RefundReceipt) -> Self {
        Self {
            deposit_id: receipt.deposit_id.0,
            payment_id: receipt.payment_id,
            dest: receipt.dest,
            replayed: receipt.replayed,
        }
    }
}

fn proposal_status_name(status: ProposalStatus) -> &'static str {
    match status {
        ProposalStatus::Pending => "pending",
        ProposalStatus::Confirmed => "confirmed",
        ProposalStatus::Rejected => "rejected",
        ProposalStatus::Expired => "expired",
    }
}

fn proposal_dto(proposal: &MoneyProposal) -> ProposalDto {
    ProposalDto {
        id: proposal.id,
        kind: proposal.kind.clone(),
        status: proposal_status_name(proposal.status).to_string(),
        confirm_not_before: proposal.confirm_not_before,
    }
}

/// Merge inside the RBAC-protected admin sub-router. The capability matrix
/// already owns all four method/path rows.
pub fn router<S>() -> Router<AppState<S>>
where
    S: Store + Send + Sync + 'static,
{
    Router::new()
        .route(
            "/admin/deposits/{id}/admit/propose",
            post(admit_propose::<S>),
        )
        .route(
            "/admin/deposits/{id}/admit/confirm",
            post(admit_confirm::<S>),
        )
        .route(
            "/admin/deposits/{id}/refund/propose",
            post(refund_propose::<S>),
        )
        .route(
            "/admin/deposits/{id}/refund/confirm",
            post(refund_confirm::<S>),
        )
}

#[utoipa::path(
    post,
    path = "/admin/deposits/{id}/admit/propose",
    tag = "admin",
    params(("id" = Uuid, Path, description = "Held deposit id")),
    request_body = DualControlBody,
    responses((status = 200, body = ProposalDto))
)]
pub(super) async fn admit_propose<S: Store + Send + Sync + 'static>(
    State(state): State<AppState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
    Json(body): Json<DualControlBody>,
) -> ApiResult<Json<ProposalDto>> {
    let proposal = propose_deposit_command(
        &state.inner.store,
        DepositId(id),
        false,
        body.reason,
        state.inner.clock.now(),
        &actor,
    )
    .await?;
    Ok(Json(proposal_dto(&proposal)))
}

#[utoipa::path(
    post,
    path = "/admin/deposits/{id}/admit/confirm",
    tag = "admin",
    params(("id" = Uuid, Path, description = "Money proposal id")),
    responses((status = 200, body = DepositAdmissionDto))
)]
pub(super) async fn admit_confirm<S: Store + Send + Sync + 'static>(
    State(state): State<AppState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<DepositAdmissionDto>> {
    let receipt =
        confirm_manual_deposit_admission(&state.inner.store, id, state.inner.clock.now(), &actor)
            .await?;
    Ok(Json(receipt.into()))
}

#[utoipa::path(
    post,
    path = "/admin/deposits/{id}/refund/propose",
    tag = "admin",
    params(("id" = Uuid, Path, description = "Held deposit id")),
    request_body = DualControlBody,
    responses((status = 200, body = ProposalDto))
)]
pub(super) async fn refund_propose<S: Store + Send + Sync + 'static>(
    State(state): State<AppState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
    Json(body): Json<DualControlBody>,
) -> ApiResult<Json<ProposalDto>> {
    let proposal = propose_deposit_command(
        &state.inner.store,
        DepositId(id),
        true,
        body.reason,
        state.inner.clock.now(),
        &actor,
    )
    .await?;
    Ok(Json(proposal_dto(&proposal)))
}

#[utoipa::path(
    post,
    path = "/admin/deposits/{id}/refund/confirm",
    tag = "admin",
    params(("id" = Uuid, Path, description = "Money proposal id")),
    responses((status = 200, body = DepositRefundDto))
)]
pub(super) async fn refund_confirm<S: Store + Send + Sync + 'static>(
    State(state): State<AppState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<DepositRefundDto>> {
    let receipt =
        confirm_deposit_refund(&state.inner.store, id, state.inner.clock.now(), &actor).await?;
    Ok(Json(receipt.into()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn admin(role: application::model::AdminRole, token: &str) -> AdminContext {
        AdminContext::Admin {
            token_digest: token.into(),
            role,
        }
    }

    #[test]
    fn receipt_dtos_do_not_expose_internal_machine_state() {
        assert_eq!(proposal_status_name(ProposalStatus::Pending), "pending");
        assert_eq!(proposal_status_name(ProposalStatus::Confirmed), "confirmed");
        assert_eq!(proposal_status_name(ProposalStatus::Rejected), "rejected");
        assert_eq!(proposal_status_name(ProposalStatus::Expired), "expired");

        let deposit = DepositId(Uuid::new_v4());
        let ledger = Uuid::new_v4();
        assert_eq!(
            DepositAdmissionDto::from(DepositReceipt {
                deposit_id: deposit,
                ledger_txn: Some(ledger),
                replayed: false,
                collected_micro: 7,
            }),
            DepositAdmissionDto {
                deposit_id: deposit.0,
                ledger_txn: Some(ledger),
                collected_micro: 7,
                replayed: false,
            }
        );

        let payment = Uuid::new_v4();
        assert_eq!(
            DepositRefundDto::from(RefundReceipt {
                deposit_id: deposit,
                payment_id: payment,
                dest: "immutable-source".into(),
                replayed: false,
            }),
            DepositRefundDto {
                deposit_id: deposit.0,
                payment_id: payment,
                dest: "immutable-source".into(),
                replayed: false,
            }
        );
    }

    #[tokio::test]
    async fn handlers_delegate_both_dual_control_commands_end_to_end() {
        use std::sync::Arc;

        use application::credit_deposit::{CreditDeposit, CreditDepositCmd};
        use application::fakes::{FakeClock, InMemoryStore};
        use application::model::{AdminRole, UserId};
        use domain::money::MicroUsd;

        let now = time::OffsetDateTime::UNIX_EPOCH;
        let store = InMemoryStore::new();
        store.set_money_flag("pause_deposits", true);
        let held = CreditDeposit { store: &store }
            .execute(CreditDepositCmd {
                user: UserId(Uuid::new_v4()),
                amount: MicroUsd(7_000_000),
                chain_sig: "route-manual-admit".into(),
                idempotency_key: "route-manual-admit".into(),
            })
            .await
            .unwrap();
        let state = AppState::new(store.clone(), Arc::new(FakeClock::at(now)));
        let proposal = admit_propose(
            State(state.clone()),
            Extension(admin(AdminRole::Finance, "finance-a")),
            Path(held.deposit_id.0),
            Json(DualControlBody {
                reason: "reviewed finalized observation".into(),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(proposal.kind, "manual_deposit_admit");
        assert_eq!(proposal.status, "pending");
        let admitted = admit_confirm(
            State(state.clone()),
            Extension(admin(AdminRole::Superadmin, "super-b")),
            Path(proposal.id),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(admitted.deposit_id, held.deposit_id.0);
        assert!(admitted.ledger_txn.is_some());

        let refund_held = CreditDeposit { store: &store }
            .execute(CreditDepositCmd {
                user: UserId(Uuid::new_v4()),
                amount: MicroUsd(9_000_000),
                chain_sig: "route-source-refund".into(),
                idempotency_key: "route-source-refund".into(),
            })
            .await
            .unwrap();
        let refund_proposal = refund_propose(
            State(state.clone()),
            Extension(admin(AdminRole::Finance, "finance-c")),
            Path(refund_held.deposit_id.0),
            Json(DualControlBody {
                reason: "return held funds".into(),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(refund_proposal.kind, "deposit_refund");
        let refund = refund_confirm(
            State(state),
            Extension(admin(AdminRole::Superadmin, "super-d")),
            Path(refund_proposal.id),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(refund.deposit_id, refund_held.deposit_id.0);
        assert!(refund.dest.starts_with("staging-faucet:"));
    }
}
