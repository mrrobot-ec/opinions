//! Ops config-plane routes (plan D24/D25) — wave W1 real handlers.
//!
//! RBAC (D26) authenticates and inserts the typed [`AdminContext`] before
//! any handler runs; per-key role/sensitivity rules live in the use cases,
//! so a route-level capability can never widen a key's write rule. Pauses
//! are config keys (`trading_paused`, `market_paused:{id}`,
//! `voting_paused:{id}`), so they ride these endpoints — there is no
//! separate pause surface.

use application::model::AdminContext;
use application::ops::proposals::{
    ConfirmProposal, CreateProposal, CreateProposalCmd, RejectProposal, SettleProposalCmd,
};
use application::ops::set_config::{SetConfig, SetConfigCmd};
use axum::extract::{Extension, Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use uuid::Uuid;

use super::super::dto::{
    ConfigAppliedDto, ConfigEntryDto, ConfigProposalDto, ConfigSnapshotDto,
    CreateConfigProposalRequest, SetConfigRequest, SettleConfigProposalRequest,
};
use super::super::error::{ApiError, ApiResult};
use super::AppState;
use application::ports::{MarketQueries, Store};

pub(super) fn router<S>() -> Router<AppState<S>>
where
    S: Store + MarketQueries + Send + Sync + 'static,
{
    Router::new()
        .route("/admin/config", get(get_config::<S>).post(set_config::<S>))
        .route("/admin/config/proposals", post(create_proposal::<S>))
        .route(
            "/admin/config/proposals/{id}/confirm",
            post(confirm_proposal::<S>),
        )
        .route(
            "/admin/config/proposals/{id}/reject",
            post(reject_proposal::<S>),
        )
}

fn proposal_dto(p: application::model::ConfigProposal) -> ConfigProposalDto {
    let status = match p.status {
        application::model::ProposalStatus::Pending => "pending",
        application::model::ProposalStatus::Confirmed => "confirmed",
        application::model::ProposalStatus::Rejected => "rejected",
        application::model::ProposalStatus::Expired => "expired",
    };
    ConfigProposalDto {
        id: p.id,
        status: status.to_string(),
        base_generation: p.base_generation,
        patch: p.patch,
        reason: p.reason,
        expires_at: p.expires_at,
        resulting_generation: p.resulting_generation,
    }
}

#[utoipa::path(
    get,
    path = "/admin/config",
    tag = "ops",
    responses(
        (status = 200, body = ConfigSnapshotDto),
        (status = 401, body = ApiError),
        (status = 403, body = ApiError),
    )
)]
pub(super) async fn get_config<S: Store + MarketQueries + 'static>(
    State(state): State<AppState<S>>,
) -> ApiResult<Json<ConfigSnapshotDto>> {
    // Authoritative committed read: one transaction, dropped (rolled back)
    // after the reads. The brief generation-row share is an admin GET.
    let mut tx = state
        .inner
        .store
        .ops_config_tx()
        .await
        .map_err(application::error::AppError::from)?;
    let generation = tx
        .lock_generation()
        .await
        .map_err(application::error::AppError::from)?;
    let entries = tx
        .config_entries()
        .await
        .map_err(application::error::AppError::from)?;
    drop(tx);
    Ok(Json(ConfigSnapshotDto {
        generation,
        entries: entries
            .into_iter()
            .map(|e| ConfigEntryDto {
                key: e.key,
                value: e.value,
            })
            .collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/admin/config",
    tag = "ops",
    request_body = SetConfigRequest,
    responses(
        (status = 200, body = ConfigAppliedDto),
        (status = 401, body = ApiError),
        (status = 403, body = ApiError),
        (status = 409, body = ApiError),
        (status = 422, body = ApiError),
    )
)]
pub(super) async fn set_config<S: Store + MarketQueries + 'static>(
    State(state): State<AppState<S>>,
    Extension(actor): Extension<AdminContext>,
    Json(body): Json<SetConfigRequest>,
) -> ApiResult<Json<ConfigAppliedDto>> {
    let outcome = SetConfig {
        store: &state.inner.store,
        clock: &state.inner.clock,
    }
    .execute(SetConfigCmd {
        actor,
        patch: body.patch,
        reason: body.reason,
        idempotency_key: body.idempotency_key,
        expected_base_generation: body.expected_base_generation,
    })
    .await?;
    Ok(Json(ConfigAppliedDto {
        generation: outcome.generation,
        changed_keys: outcome.changed_keys,
    }))
}

#[utoipa::path(
    post,
    path = "/admin/config/proposals",
    tag = "ops",
    request_body = CreateConfigProposalRequest,
    responses(
        (status = 200, body = ConfigProposalDto),
        (status = 401, body = ApiError),
        (status = 403, body = ApiError),
        (status = 409, body = ApiError),
        (status = 422, body = ApiError),
    )
)]
pub(super) async fn create_proposal<S: Store + MarketQueries + 'static>(
    State(state): State<AppState<S>>,
    Extension(actor): Extension<AdminContext>,
    Json(body): Json<CreateConfigProposalRequest>,
) -> ApiResult<Json<ConfigProposalDto>> {
    let proposal = CreateProposal {
        store: &state.inner.store,
        clock: &state.inner.clock,
    }
    .execute(CreateProposalCmd {
        actor,
        patch: body.patch,
        revert_of_generation: body.revert_of_generation,
        reason: body.reason,
        idempotency_key: body.idempotency_key,
    })
    .await?;
    Ok(Json(proposal_dto(proposal)))
}

#[utoipa::path(
    post,
    path = "/admin/config/proposals/{id}/confirm",
    tag = "ops",
    params(("id" = Uuid, Path, description = "Proposal id")),
    request_body = SettleConfigProposalRequest,
    responses(
        (status = 200, body = ConfigProposalDto),
        (status = 401, body = ApiError),
        (status = 403, body = ApiError),
        (status = 404, body = ApiError),
        (status = 409, body = ApiError),
    )
)]
pub(super) async fn confirm_proposal<S: Store + MarketQueries + 'static>(
    State(state): State<AppState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
    body: Option<Json<SettleConfigProposalRequest>>,
) -> ApiResult<Json<ConfigProposalDto>> {
    let proposal = ConfirmProposal {
        store: &state.inner.store,
        clock: &state.inner.clock,
    }
    .execute(SettleProposalCmd {
        actor,
        id,
        reason: body.and_then(|Json(body)| body.reason),
    })
    .await?;
    Ok(Json(proposal_dto(proposal)))
}

#[utoipa::path(
    post,
    path = "/admin/config/proposals/{id}/reject",
    tag = "ops",
    params(("id" = Uuid, Path, description = "Proposal id")),
    request_body = SettleConfigProposalRequest,
    responses(
        (status = 200, body = ConfigProposalDto),
        (status = 401, body = ApiError),
        (status = 403, body = ApiError),
        (status = 404, body = ApiError),
        (status = 409, body = ApiError),
    )
)]
pub(super) async fn reject_proposal<S: Store + MarketQueries + 'static>(
    State(state): State<AppState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
    body: Option<Json<SettleConfigProposalRequest>>,
) -> ApiResult<Json<ConfigProposalDto>> {
    let proposal = RejectProposal {
        store: &state.inner.store,
        clock: &state.inner.clock,
    }
    .execute(SettleProposalCmd {
        actor,
        id,
        reason: body.and_then(|Json(body)| body.reason),
    })
    .await?;
    Ok(Json(proposal_dto(proposal)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use application::model::{AdminRole, ConfigProposal, ProposalStatus};
    use time::OffsetDateTime;

    use super::*;

    #[test]
    fn proposal_dto_maps_every_durable_status() {
        let id = Uuid::new_v4();
        let expires_at = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        for (status, expected) in [
            (ProposalStatus::Pending, "pending"),
            (ProposalStatus::Confirmed, "confirmed"),
            (ProposalStatus::Rejected, "rejected"),
            (ProposalStatus::Expired, "expired"),
        ] {
            let dto = proposal_dto(ConfigProposal {
                id,
                idempotency_key: "key".into(),
                patch: serde_json::json!({"trade_fee_bps": 110}),
                patch_hash: "hash".into(),
                base_generation: 7,
                proposer_token_id: "proposer".into(),
                proposer_role: AdminRole::Finance,
                reason: "reason".into(),
                status,
                expires_at,
                confirmer_token_id: Some("confirmer".into()),
                resulting_generation: Some(8),
            });
            assert_eq!(dto.id, id);
            assert_eq!(dto.status, expected);
            assert_eq!(dto.base_generation, 7);
            assert_eq!(dto.patch, serde_json::json!({"trade_fee_bps": 110}));
            assert_eq!(dto.reason, "reason");
            assert_eq!(dto.expires_at, expires_at);
            assert_eq!(dto.resulting_generation, Some(8));
        }
    }
}
