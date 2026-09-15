//! Ops admin-plane routes (plan D26/D27/D30) — wave W2: invariants, audit
//! reads, withdrawal eligibility, the two-factor staging faucet, and the
//! dual-controlled unwind / remedial-credit / write-off flows. Zero business
//! logic here: every handler calls a use case; RBAC ran before any handler.

use axum::extract::{Extension, Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use uuid::Uuid;

use application::credit_deposit::{CreditDeposit, CreditDepositCmd};
use application::model::{AdminContext, DraftId, MarketId, UserId};
use application::ops::audit::{OpsError, OpsPolicy};
use application::ops::receivable_collection::{WriteOffCmd, WriteOffReceivable};
use application::ops::remedial_credit::{RemedialCredit, RemedialCreditCmd};
use application::ops::unwind_market::{UnwindCmd, UnwindMarket};
use application::ports::{MarketQueries, OpsQueries, Store};

use super::super::dto::{
    AuditActionDto, AuditPageDto, DualControlRequest, FaucetDepositDto, FaucetDepositRequest,
    InvariantIdentityDto, InvariantReportDto, MarketUnwindDto, PublishStatusDto,
    ReceivableWriteOffDto, RemedialCreditDto, RemedialCreditRequest, WithdrawalEligibilityDto,
};
use super::super::error::{ApiError, ApiResult, ErrorResponse};
use super::AppState;

fn ops_error(error: OpsError) -> ErrorResponse {
    match error {
        OpsError::OverCap { cap_micro } => ErrorResponse::new(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "OverCap",
            &format!("amount exceeds the pinned cap of {cap_micro} micro-USD"),
        ),
        OpsError::InvalidAmount => ErrorResponse::new(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "InvalidAmount",
            "amount must be a positive micro-USD integer",
        ),
        OpsError::App(app) => ErrorResponse::from(app),
    }
}

fn unwind_dto(unwind: &application::model::MarketUnwind) -> MarketUnwindDto {
    MarketUnwindDto {
        market_id: unwind.market.0,
        stage: format!("{:?}", unwind.stage).to_lowercase(),
        reason: unwind.reason.clone(),
        confirm_not_before: unwind.confirm_not_before,
        reversal_txn: unwind.reversal_txn,
    }
}

fn remedial_dto(proposal: &application::ports::RemedialCreditProposal) -> RemedialCreditDto {
    RemedialCreditDto {
        market_id: proposal.market.0,
        user_id: proposal.user.0,
        amount_micro: proposal.amount_micro,
        status: format!("{:?}", proposal.status).to_lowercase(),
    }
}

fn write_off_dto(proposal: &application::ports::WriteOffProposal) -> ReceivableWriteOffDto {
    ReceivableWriteOffDto {
        receivable_id: proposal.receivable,
        amount_micro: proposal.amount_micro,
        status: format!("{:?}", proposal.status).to_lowercase(),
    }
}

fn publish_status_dto(
    draft: Uuid,
    command: Option<&application::ports::PublicationCommand>,
) -> PublishStatusDto {
    PublishStatusDto {
        draft_id: draft,
        status: command.map_or_else(
            || "none".to_string(),
            |command| format!("{:?}", command.status).to_lowercase(),
        ),
        result_market_id: command.and_then(|command| command.result_market.map(|m| m.0)),
        error: command.and_then(|command| command.error.clone()),
        status_url: format!("/admin/drafts/{draft}/publish_status"),
    }
}

pub(super) fn router<S>(staging_faucet: bool) -> Router<AppState<S>>
where
    S: Store + MarketQueries + OpsQueries + Send + Sync + 'static,
{
    let router = Router::new()
        .route("/admin/invariants", get(invariants::<S>))
        .route("/admin/audit", get(audit_read::<S>))
        .route(
            "/admin/users/{id}/withdrawal_eligibility",
            get(withdrawal_eligibility::<S>),
        )
        .route(
            "/admin/markets/{id}/unwind/propose",
            post(unwind_propose::<S>),
        )
        .route(
            "/admin/markets/{id}/unwind/confirm",
            post(unwind_confirm::<S>),
        )
        .route(
            "/admin/markets/{id}/unwind/reject",
            post(unwind_reject::<S>),
        )
        .route(
            "/admin/markets/{id}/remedial_credit/propose",
            post(remedial_credit_propose::<S>),
        )
        .route(
            "/admin/markets/{id}/remedial_credit/confirm",
            post(remedial_credit_confirm::<S>),
        )
        .route(
            "/admin/receivables/{id}/write_off/propose",
            post(write_off_propose::<S>),
        )
        .route(
            "/admin/receivables/{id}/write_off/confirm",
            post(write_off_confirm::<S>),
        )
        .route(
            "/admin/drafts/{id}/publish_status",
            get(publish_status::<S>),
        );
    if staging_faucet {
        router.route("/admin/deposits", post(faucet_deposit::<S>))
    } else {
        router
    }
}

#[utoipa::path(
    get,
    path = "/admin/invariants",
    tag = "ops",
    responses(
        (status = 200, body = InvariantReportDto),
        (status = 401, body = ApiError),
        (status = 403, body = ApiError),
    )
)]
pub(super) async fn invariants<S: Store + MarketQueries + OpsQueries + 'static>(
    State(state): State<AppState<S>>,
) -> ApiResult<Json<InvariantReportDto>> {
    let report = application::integrity::invariant_sweep::run(&state.inner.store).await?;
    Ok(Json(InvariantReportDto {
        as_of: report.as_of,
        pass: report.pass,
        identities: report
            .identities
            .into_iter()
            .map(|identity| InvariantIdentityDto {
                identity: identity.identity.to_string(),
                pass: identity.pass,
                detail: identity.detail,
            })
            .collect(),
    }))
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub(super) struct AuditPageQuery {
    /// Strictly-before cursor (RFC 3339); omit for the newest page.
    before: Option<String>,
    /// Page size, 1..=200 (default 50).
    limit: Option<u32>,
}

#[utoipa::path(
    get,
    path = "/admin/audit",
    tag = "ops",
    params(AuditPageQuery),
    responses(
        (status = 200, body = AuditPageDto),
        (status = 401, body = ApiError),
        (status = 403, body = ApiError),
        (status = 422, body = ApiError),
    )
)]
pub(super) async fn audit_read<S: Store + MarketQueries + OpsQueries + 'static>(
    State(state): State<AppState<S>>,
    Query(query): Query<AuditPageQuery>,
) -> ApiResult<Json<AuditPageDto>> {
    let before = query
        .before
        .map(|raw| {
            time::OffsetDateTime::parse(&raw, &time::format_description::well_known::Rfc3339)
                .map_err(|_| {
                    ErrorResponse::new(
                        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                        "InvalidCursor",
                        "before must be RFC 3339",
                    )
                })
        })
        .transpose()?;
    let rows =
        application::ops::audit::audit_page(&state.inner.store, before, query.limit.unwrap_or(50))
            .await?;
    let next_before = rows.last().map(|row| row.at);
    Ok(Json(AuditPageDto {
        actions: rows
            .into_iter()
            .map(|row| AuditActionDto {
                id: row.id,
                actor_role: row.action.actor_role.name().to_string(),
                actor_token_digest: row.action.actor_token_digest,
                action: row.action.action,
                subject: row.action.subject,
                reason: row.action.reason,
                at: row.at,
            })
            .collect(),
        next_before,
    }))
}

#[utoipa::path(
    get,
    path = "/admin/users/{id}/withdrawal_eligibility",
    tag = "ops",
    params(("id" = Uuid, Path, description = "User id")),
    responses(
        (status = 200, body = WithdrawalEligibilityDto),
        (status = 401, body = ApiError),
        (status = 403, body = ApiError),
        (status = 404, body = ApiError),
    )
)]
pub(super) async fn withdrawal_eligibility<S: Store + MarketQueries + OpsQueries + 'static>(
    State(state): State<AppState<S>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<WithdrawalEligibilityDto>> {
    let view = state
        .inner
        .store
        .withdrawal_eligibility(UserId(id))
        .await
        .map_err(application::error::AppError::from)?;
    Ok(Json(WithdrawalEligibilityDto {
        user_id: view.user.0,
        cash_micro: view.cash_micro,
        open_receivables_micro: view.open_receivables_micro,
        eligible: view.eligible,
    }))
}

#[utoipa::path(
    post,
    path = "/admin/deposits",
    tag = "ops",
    request_body = FaucetDepositRequest,
    responses(
        (status = 200, body = FaucetDepositDto),
        (status = 401, body = ApiError),
        (status = 403, body = ApiError),
        (status = 422, body = ApiError),
    )
)]
pub(super) async fn faucet_deposit<S: Store + MarketQueries + OpsQueries + 'static>(
    State(state): State<AppState<S>>,
    Extension(actor): Extension<AdminContext>,
    Json(body): Json<FaucetDepositRequest>,
) -> ApiResult<Json<FaucetDepositDto>> {
    let policy = OpsPolicy::default();
    if body.amount_micro <= 0 || body.amount_micro > policy.faucet_per_call_cap_micro {
        return Err(ops_error(OpsError::OverCap {
            cap_micro: policy.faucet_per_call_cap_micro,
        }));
    }
    let receipt = CreditDeposit {
        store: &state.inner.store,
    }
    .execute_as(
        CreditDepositCmd {
            user: UserId(body.user_id),
            amount: domain::money::MicroUsd(body.amount_micro),
            chain_sig: format!("faucet:{}", body.idempotency_key),
            idempotency_key: format!("faucet:{}", body.idempotency_key),
        },
        &actor,
    )
    .await?;
    Ok(Json(FaucetDepositDto {
        user_id: body.user_id,
        amount_micro: body.amount_micro,
        ledger_txn: receipt.ledger_txn,
        replayed: receipt.replayed,
    }))
}

fn unwind_uc<S: Store>(
    state: &AppState<S>,
    actor: AdminContext,
) -> UnwindMarket<'_, S, super::SharedClock> {
    UnwindMarket {
        store: &state.inner.store,
        clock: &state.inner.clock,
        policy: OpsPolicy::default(),
        actor,
    }
}

#[utoipa::path(
    post,
    path = "/admin/markets/{id}/unwind/propose",
    tag = "ops",
    params(("id" = Uuid, Path, description = "Market id")),
    request_body = DualControlRequest,
    responses(
        (status = 200, body = MarketUnwindDto),
        (status = 401, body = ApiError),
        (status = 403, body = ApiError),
        (status = 409, body = ApiError),
        (status = 422, body = ApiError),
    )
)]
pub(super) async fn unwind_propose<S: Store + MarketQueries + OpsQueries + 'static>(
    State(state): State<AppState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
    Json(body): Json<DualControlRequest>,
) -> ApiResult<Json<MarketUnwindDto>> {
    let unwind = unwind_uc(&state, actor)
        .propose(UnwindCmd {
            market: MarketId(id),
            reason: body.reason,
            idempotency_key: body.idempotency_key,
        })
        .await
        .map_err(ops_error)?;
    Ok(Json(unwind_dto(&unwind)))
}

#[utoipa::path(
    post,
    path = "/admin/markets/{id}/unwind/confirm",
    tag = "ops",
    params(("id" = Uuid, Path, description = "Market id")),
    request_body = DualControlRequest,
    responses(
        (status = 200, body = MarketUnwindDto),
        (status = 401, body = ApiError),
        (status = 403, body = ApiError),
        (status = 409, body = ApiError),
        (status = 422, body = ApiError),
    )
)]
pub(super) async fn unwind_confirm<S: Store + MarketQueries + OpsQueries + 'static>(
    State(state): State<AppState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
    Json(body): Json<DualControlRequest>,
) -> ApiResult<Json<MarketUnwindDto>> {
    let unwind = unwind_uc(&state, actor)
        .confirm(UnwindCmd {
            market: MarketId(id),
            reason: body.reason,
            idempotency_key: body.idempotency_key,
        })
        .await
        .map_err(ops_error)?;
    Ok(Json(unwind_dto(&unwind)))
}

#[utoipa::path(
    post,
    path = "/admin/markets/{id}/unwind/reject",
    tag = "ops",
    params(("id" = Uuid, Path, description = "Market id")),
    request_body = DualControlRequest,
    responses(
        (status = 200, body = MarketUnwindDto),
        (status = 401, body = ApiError),
        (status = 403, body = ApiError),
        (status = 409, body = ApiError),
    )
)]
pub(super) async fn unwind_reject<S: Store + MarketQueries + OpsQueries + 'static>(
    State(state): State<AppState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
    Json(body): Json<DualControlRequest>,
) -> ApiResult<Json<MarketUnwindDto>> {
    let unwind = unwind_uc(&state, actor)
        .reject(UnwindCmd {
            market: MarketId(id),
            reason: body.reason,
            idempotency_key: body.idempotency_key,
        })
        .await
        .map_err(ops_error)?;
    Ok(Json(unwind_dto(&unwind)))
}

#[utoipa::path(
    post,
    path = "/admin/markets/{id}/remedial_credit/propose",
    tag = "ops",
    params(("id" = Uuid, Path, description = "Market id")),
    request_body = RemedialCreditRequest,
    responses(
        (status = 200, body = RemedialCreditDto),
        (status = 401, body = ApiError),
        (status = 403, body = ApiError),
        (status = 409, body = ApiError),
        (status = 422, body = ApiError),
    )
)]
pub(super) async fn remedial_credit_propose<S: Store + MarketQueries + OpsQueries + 'static>(
    State(state): State<AppState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
    Json(body): Json<RemedialCreditRequest>,
) -> ApiResult<Json<RemedialCreditDto>> {
    let proposal = RemedialCredit {
        store: &state.inner.store,
        clock: &state.inner.clock,
        policy: OpsPolicy::default(),
        actor,
    }
    .propose(RemedialCreditCmd {
        market: MarketId(id),
        user: UserId(body.user_id),
        amount_micro: body.amount_micro,
        reason: body.reason,
        idempotency_key: body.idempotency_key,
    })
    .await
    .map_err(ops_error)?;
    Ok(Json(remedial_dto(&proposal)))
}

#[utoipa::path(
    post,
    path = "/admin/markets/{id}/remedial_credit/confirm",
    tag = "ops",
    params(("id" = Uuid, Path, description = "Market id")),
    request_body = DualControlRequest,
    responses(
        (status = 200, body = RemedialCreditDto),
        (status = 401, body = ApiError),
        (status = 403, body = ApiError),
        (status = 409, body = ApiError),
        (status = 422, body = ApiError),
    )
)]
pub(super) async fn remedial_credit_confirm<S: Store + MarketQueries + OpsQueries + 'static>(
    State(state): State<AppState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
    Json(body): Json<DualControlRequest>,
) -> ApiResult<Json<RemedialCreditDto>> {
    let proposal = RemedialCredit {
        store: &state.inner.store,
        clock: &state.inner.clock,
        policy: OpsPolicy::default(),
        actor,
    }
    .confirm(RemedialCreditCmd {
        market: MarketId(id),
        // user/amount ride the durable proposal row on confirm.
        user: UserId(Uuid::nil()),
        amount_micro: 1,
        reason: body.reason,
        idempotency_key: body.idempotency_key,
    })
    .await
    .map_err(ops_error)?;
    Ok(Json(remedial_dto(&proposal)))
}

fn write_off_uc<S: Store>(
    state: &AppState<S>,
    actor: AdminContext,
) -> WriteOffReceivable<'_, S, super::SharedClock> {
    WriteOffReceivable {
        store: &state.inner.store,
        clock: &state.inner.clock,
        policy: OpsPolicy::default(),
        actor,
    }
}

#[utoipa::path(
    post,
    path = "/admin/receivables/{id}/write_off/propose",
    tag = "ops",
    params(("id" = Uuid, Path, description = "Receivable id")),
    request_body = DualControlRequest,
    responses(
        (status = 200, body = ReceivableWriteOffDto),
        (status = 401, body = ApiError),
        (status = 403, body = ApiError),
        (status = 404, body = ApiError),
        (status = 409, body = ApiError),
        (status = 422, body = ApiError),
    )
)]
pub(super) async fn write_off_propose<S: Store + MarketQueries + OpsQueries + 'static>(
    State(state): State<AppState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
    Json(body): Json<DualControlRequest>,
) -> ApiResult<Json<ReceivableWriteOffDto>> {
    let proposal = write_off_uc(&state, actor)
        .propose(WriteOffCmd {
            receivable: id,
            reason: body.reason,
            idempotency_key: body.idempotency_key,
        })
        .await
        .map_err(ops_error)?;
    Ok(Json(write_off_dto(&proposal)))
}

#[utoipa::path(
    post,
    path = "/admin/receivables/{id}/write_off/confirm",
    tag = "ops",
    params(("id" = Uuid, Path, description = "Receivable id")),
    request_body = DualControlRequest,
    responses(
        (status = 200, body = ReceivableWriteOffDto),
        (status = 401, body = ApiError),
        (status = 403, body = ApiError),
        (status = 409, body = ApiError),
        (status = 422, body = ApiError),
    )
)]
pub(super) async fn write_off_confirm<S: Store + MarketQueries + OpsQueries + 'static>(
    State(state): State<AppState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
    Json(body): Json<DualControlRequest>,
) -> ApiResult<Json<ReceivableWriteOffDto>> {
    let proposal = write_off_uc(&state, actor)
        .confirm(WriteOffCmd {
            receivable: id,
            reason: body.reason,
            idempotency_key: body.idempotency_key,
        })
        .await
        .map_err(ops_error)?;
    Ok(Json(write_off_dto(&proposal)))
}

#[utoipa::path(
    get,
    path = "/admin/drafts/{id}/publish_status",
    tag = "ops",
    params(("id" = Uuid, Path, description = "Draft id")),
    responses(
        (status = 200, body = PublishStatusDto),
        (status = 401, body = ApiError),
        (status = 403, body = ApiError),
        (status = 404, body = ApiError),
    )
)]
pub(super) async fn publish_status<S: Store + MarketQueries + OpsQueries + 'static>(
    State(state): State<AppState<S>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<PublishStatusDto>> {
    let command = state
        .inner
        .store
        .publication_command_for_draft(DraftId(id))
        .await
        .map_err(application::error::AppError::from)?;
    Ok(Json(publish_status_dto(id, command.as_ref())))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use application::error::AppError;
    use application::model::{ProposalStatus, UnwindStage};
    use application::ports::{PublicationCommand, PublicationCommandStatus};
    use time::OffsetDateTime;

    use super::*;

    #[test]
    fn ops_errors_and_dtos_preserve_the_full_admin_vocabulary() {
        let over = ops_error(OpsError::OverCap { cap_micro: 123 });
        assert_eq!(over.status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(over.body.code, "OverCap");
        assert!(over.body.message.contains("123"));
        let invalid = ops_error(OpsError::InvalidAmount);
        assert_eq!(invalid.status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(invalid.body.code, "InvalidAmount");
        let app = ops_error(OpsError::App(AppError::ArtifactNotFound));
        assert_eq!(app.status, axum::http::StatusCode::NOT_FOUND);

        let market = MarketId(Uuid::new_v4());
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let reversal = Uuid::new_v4();
        let unwind = application::model::MarketUnwind {
            market,
            unwind_key: "unwind-key".into(),
            stage: UnwindStage::Applied,
            proposer_token_id: "proposer".into(),
            confirmer_token_id: Some("confirmer".into()),
            reason: "wrong fraud decision".into(),
            confirm_not_before: now,
            reversal_txn: Some(reversal),
        };
        let dto = unwind_dto(&unwind);
        assert_eq!(dto.market_id, market.0);
        assert_eq!(dto.stage, "applied");
        assert_eq!(dto.reason, unwind.reason);
        assert_eq!(dto.confirm_not_before, now);
        assert_eq!(dto.reversal_txn, Some(reversal));

        let user = UserId(Uuid::new_v4());
        let remedial = application::ports::RemedialCreditProposal {
            id: Uuid::new_v4(),
            market,
            user,
            idempotency_key: "remedial-key".into(),
            amount_micro: 42,
            proposer_token_id: "proposer".into(),
            confirmer_token_id: None,
            reason: "goodwill".into(),
            status: ProposalStatus::Pending,
            confirm_not_before: now,
        };
        let dto = remedial_dto(&remedial);
        assert_eq!(dto.market_id, market.0);
        assert_eq!(dto.user_id, user.0);
        assert_eq!(dto.amount_micro, 42);
        assert_eq!(dto.status, "pending");

        let receivable = Uuid::new_v4();
        let write_off = application::ports::WriteOffProposal {
            id: Uuid::new_v4(),
            receivable,
            idempotency_key: "write-off-key".into(),
            amount_micro: 84,
            proposer_token_id: "proposer".into(),
            confirmer_token_id: Some("confirmer".into()),
            reason: "uncollectable".into(),
            status: ProposalStatus::Confirmed,
            confirm_not_before: now,
        };
        let dto = write_off_dto(&write_off);
        assert_eq!(dto.receivable_id, receivable);
        assert_eq!(dto.amount_micro, 84);
        assert_eq!(dto.status, "confirmed");
    }

    #[test]
    fn publication_status_dto_distinguishes_absence_and_every_terminal_field() {
        let draft = Uuid::new_v4();
        let none = publish_status_dto(draft, None);
        assert_eq!(none.status, "none");
        assert_eq!(
            none.status_url,
            format!("/admin/drafts/{draft}/publish_status")
        );

        let market = MarketId(Uuid::new_v4());
        let command = PublicationCommand {
            id: Uuid::new_v4(),
            draft: DraftId(draft),
            idempotency_key: "publish-key".into(),
            requested_by: "curator".into(),
            status: PublicationCommandStatus::Failed,
            attempts: 3,
            lease_expires_at: None,
            result_market: Some(market),
            error: Some("render failed".into()),
        };
        let some = publish_status_dto(draft, Some(&command));
        assert_eq!(some.status, "failed");
        assert_eq!(some.result_market_id, Some(market.0));
        assert_eq!(some.error.as_deref(), Some("render failed"));
    }
}
