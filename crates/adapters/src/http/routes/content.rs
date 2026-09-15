//! Curator draft HTTP surface.

use application::content::create_draft::{CreateDraft, CreateDraftCmd};
use application::content::review_draft::ReviewDraft;
use application::content::template_engine::TemplateDraftEngine;
use application::model::{DraftId, UserId};
use application::ports::{MarketQueries, NotificationQueries, SocialQueries, Store};
use axum::extract::{Path, Query, State};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use uuid::Uuid;

use crate::http::dto::{CreateDraftRequest, DraftDto, EditDraftRequest, ReviewDraftRequest};

use super::{ApiResult, AppState, ErrorResponse};

pub(super) fn router<S>() -> Router<AppState<S>>
where
    S: Store + MarketQueries + SocialQueries + NotificationQueries + Send + Sync + 'static,
{
    Router::new()
        .route(
            "/admin/drafts",
            get(list_drafts::<S>).post(create_drafts::<S>),
        )
        .route("/admin/drafts/{id}", patch(edit_draft::<S>))
        .route("/admin/drafts/{id}/approve", post(approve_draft::<S>))
        .route("/admin/drafts/{id}/reject", post(reject_draft::<S>))
        .route("/admin/drafts/{id}/publish_now", post(publish_now::<S>))
}

#[derive(Debug, Deserialize)]
pub(super) struct DraftListQuery {
    #[serde(default = "default_limit")]
    limit: u32,
}

const fn default_limit() -> u32 {
    100
}

#[utoipa::path(
    get,
    path = "/admin/drafts",
    tag = "content",
    params(("limit" = Option<u32>, Query, description = "maximum drafts, 1..=500")),
    responses(
        (status = 200, body = Vec<DraftDto>),
        (status = 401, description = "invalid admin token"),
        (status = 422, description = "invalid limit"),
    ),
)]
pub(super) async fn list_drafts<S: Store + 'static>(
    State(state): State<AppState<S>>,
    Query(query): Query<DraftListQuery>,
) -> ApiResult<Json<Vec<DraftDto>>> {
    if !(1..=500).contains(&query.limit) {
        return Err(ErrorResponse::new(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "InvalidLimit",
            "limit must be between 1 and 500",
        ));
    }
    let mut tx = state
        .inner
        .store
        .content_tx()
        .await
        .map_err(application::error::AppError::from)?;
    let rows = tx
        .list_drafts(query.limit)
        .await
        .map_err(application::error::AppError::from)?;
    tx.commit()
        .await
        .map_err(application::error::AppError::from)?;
    Ok(Json(rows.into_iter().map(DraftDto::from).collect()))
}

#[utoipa::path(
    post,
    path = "/admin/drafts",
    tag = "content",
    request_body = CreateDraftRequest,
    responses(
        (status = 200, body = Vec<DraftDto>),
        (status = 401, description = "invalid admin token"),
        (status = 409, description = "draft admission limit reached"),
        (status = 422, description = "invalid draft request"),
    ),
)]
pub(super) async fn create_drafts<S: Store + 'static>(
    State(state): State<AppState<S>>,
    Json(request): Json<CreateDraftRequest>,
) -> ApiResult<Json<Vec<DraftDto>>> {
    let template = TemplateDraftEngine::new(state.inner.phase5.config.clone());
    let receipt = CreateDraft {
        store: &state.inner.store,
        clock: &state.inner.clock,
        config: &state.inner.phase5.config,
        primary: state.inner.phase5.draft_engine.as_ref(),
        template: &template,
    }
    .execute(CreateDraftCmd {
        topics: request.topics,
        tier: request.tier.into(),
        requested_source: request.source.into(),
        allow_fallback: request.allow_fallback,
    })
    .await?;
    Ok(Json(
        receipt.drafts.into_iter().map(DraftDto::from).collect(),
    ))
}

#[utoipa::path(
    patch,
    path = "/admin/drafts/{id}",
    tag = "content",
    params(("id" = Uuid, Path, description = "draft id")),
    request_body = EditDraftRequest,
    responses(
        (status = 200, body = DraftDto),
        (status = 401, description = "invalid admin token"),
        (status = 404, description = "unknown draft"),
        (status = 409, description = "draft is not editable"),
    ),
)]
pub(super) async fn edit_draft<S: Store + 'static>(
    State(state): State<AppState<S>>,
    Path(id): Path<Uuid>,
    Json(request): Json<EditDraftRequest>,
) -> ApiResult<Json<DraftDto>> {
    let row = review(&state)
        .edit(
            DraftId(id),
            UserId(request.reviewer_id),
            request.spec.into(),
        )
        .await?;
    Ok(Json(row.into()))
}

#[utoipa::path(
    post,
    path = "/admin/drafts/{id}/approve",
    tag = "content",
    params(("id" = Uuid, Path, description = "draft id")),
    request_body = ReviewDraftRequest,
    responses(
        (status = 200, body = DraftDto),
        (status = 401, description = "invalid admin token"),
        (status = 404, description = "unknown draft"),
        (status = 409, description = "draft cannot be approved"),
    ),
)]
pub(super) async fn approve_draft<S: Store + 'static>(
    State(state): State<AppState<S>>,
    axum::extract::Extension(actor): axum::extract::Extension<application::model::AdminContext>,
    Path(id): Path<Uuid>,
    Json(request): Json<ReviewDraftRequest>,
) -> ApiResult<Json<DraftDto>> {
    let row = review(&state)
        .approve_as(DraftId(id), UserId(request.reviewer_id), &actor)
        .await?;
    Ok(Json(row.into()))
}

#[utoipa::path(
    post,
    path = "/admin/drafts/{id}/reject",
    tag = "content",
    params(("id" = Uuid, Path, description = "draft id")),
    request_body = ReviewDraftRequest,
    responses(
        (status = 200, body = DraftDto),
        (status = 401, description = "invalid admin token"),
        (status = 404, description = "unknown draft"),
        (status = 409, description = "draft cannot be rejected"),
    ),
)]
pub(super) async fn reject_draft<S: Store + 'static>(
    State(state): State<AppState<S>>,
    axum::extract::Extension(actor): axum::extract::Extension<application::model::AdminContext>,
    Path(id): Path<Uuid>,
    Json(request): Json<ReviewDraftRequest>,
) -> ApiResult<Json<DraftDto>> {
    let row = review(&state)
        .reject_as(DraftId(id), UserId(request.reviewer_id), &actor)
        .await?;
    Ok(Json(row.into()))
}

#[utoipa::path(
    post,
    path = "/admin/drafts/{id}/publish_now",
    tag = "content",
    params(("id" = Uuid, Path, description = "draft id")),
    responses(
        (status = 202, body = super::super::dto::PublishStatusDto),
        (status = 401, description = "invalid admin token"),
        (status = 404, description = "unknown draft"),
        (status = 409, description = "draft is not approved"),
    ),
)]
pub(super) async fn publish_now<S: Store + 'static>(
    State(state): State<AppState<S>>,
    axum::extract::Extension(actor): axum::extract::Extension<application::model::AdminContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<(
    axum::http::StatusCode,
    Json<super::super::dto::PublishStatusDto>,
)> {
    // D26 / codex r3 NEW-4: authorize atomically (command + audit), answer
    // 202 + a status URL; the publisher tick executes the idempotent saga.
    let receipt = application::content::publish_now_command::PublishNow {
        store: &state.inner.store,
        actor,
    }
    .execute(application::content::publish_now_command::PublishNowCmd {
        draft: DraftId(id),
        idempotency_key: format!("publish-now:{id}"),
    })
    .await?;
    Ok((
        axum::http::StatusCode::ACCEPTED,
        Json(super::super::dto::PublishStatusDto {
            draft_id: id,
            status: format!("{:?}", receipt.command.status).to_lowercase(),
            result_market_id: receipt.command.result_market.map(|m| m.0),
            error: receipt.command.error,
            status_url: format!("/admin/drafts/{id}/publish_status"),
        }),
    ))
}

fn review<S: Store>(state: &AppState<S>) -> ReviewDraft<'_, S, super::SharedClock> {
    ReviewDraft {
        store: &state.inner.store,
        clock: &state.inner.clock,
        config: &state.inner.phase5.config,
        lp_kill_config: state.inner.phase5.lp_kill,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use application::ensure_genesis::{EnsureGenesis, EnsureGenesisCmd};
    use application::fakes::{FakeClock, InMemoryStore};
    use domain::ledger::Currency;
    use domain::money::MicroUsd;
    use http_body_util::BodyExt;
    use time::OffsetDateTime;
    use tower::ServiceExt;

    use super::*;

    #[tokio::test]
    async fn curator_routes_enforce_auth_review_state_and_publish_authority() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_006_400).unwrap();
        let store = InMemoryStore::new();
        EnsureGenesis { store: &store }
            .execute(EnsureGenesisCmd {
                currency: Currency::Usdc,
                amount: MicroUsd(1_000_000_000),
            })
            .await
            .unwrap();
        let reviewer = store.add_user("curator", now - time::Duration::days(2), 2);
        let state = AppState::with_tokens(
            store,
            Arc::new(FakeClock::at(now)),
            "demo-token",
            "admin-token",
        );
        let app = super::super::router(state);
        let body = serde_json::json!({
            "topics": ["local transit"],
            "tier": "flash",
            "source": "template",
            "allow_fallback": false
        });
        let unauthorized = app
            .clone()
            .oneshot(json_request("POST", "/admin/drafts", &body, None))
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), axum::http::StatusCode::UNAUTHORIZED);

        let created = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/admin/drafts",
                &body,
                Some("admin-token"),
            ))
            .await
            .unwrap();
        assert_eq!(created.status(), axum::http::StatusCode::OK);
        let json: serde_json::Value =
            serde_json::from_slice(&created.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        let id = json[0]["id"].as_str().unwrap();

        let mut edited_spec = json[0]["spec"].clone();
        edited_spec["question"] = serde_json::json!("Will the curator edit persist?");
        let edited = app
            .clone()
            .oneshot(json_request(
                "PATCH",
                &format!("/admin/drafts/{id}"),
                &serde_json::json!({
                    "reviewer_id": reviewer.0,
                    "spec": edited_spec,
                }),
                Some("admin-token"),
            ))
            .await
            .unwrap();
        assert_eq!(edited.status(), axum::http::StatusCode::OK);
        let edited_json: serde_json::Value =
            serde_json::from_slice(&edited.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(
            edited_json["spec"]["question"],
            "Will the curator edit persist?"
        );

        let rejected = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/admin/drafts",
                &serde_json::json!({
                    "topics": ["a rejected draft"],
                    "tier": "daily",
                    "source": "template",
                    "allow_fallback": false,
                }),
                Some("admin-token"),
            ))
            .await
            .unwrap();
        let rejected_json: serde_json::Value =
            serde_json::from_slice(&rejected.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        let rejected_id = rejected_json[0]["id"].as_str().unwrap();
        let rejected = app
            .clone()
            .oneshot(json_request(
                "POST",
                &format!("/admin/drafts/{rejected_id}/reject"),
                &serde_json::json!({"reviewer_id": reviewer.0}),
                Some("admin-token"),
            ))
            .await
            .unwrap();
        assert_eq!(rejected.status(), axum::http::StatusCode::OK);
        let rejected_json: serde_json::Value =
            serde_json::from_slice(&rejected.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(rejected_json["status"], "rejected");

        let pending_publish = app
            .clone()
            .oneshot(json_request(
                "POST",
                &format!("/admin/drafts/{id}/publish_now"),
                &serde_json::Value::Null,
                Some("admin-token"),
            ))
            .await
            .unwrap();
        assert_eq!(pending_publish.status(), axum::http::StatusCode::CONFLICT);

        let review = serde_json::json!({"reviewer_id": reviewer.0});
        let approved = app
            .clone()
            .oneshot(json_request(
                "POST",
                &format!("/admin/drafts/{id}/approve"),
                &review,
                Some("admin-token"),
            ))
            .await
            .unwrap();
        assert_eq!(approved.status(), axum::http::StatusCode::OK);
        let published = app
            .clone()
            .oneshot(json_request(
                "POST",
                &format!("/admin/drafts/{id}/publish_now"),
                &serde_json::Value::Null,
                Some("admin-token"),
            ))
            .await
            .unwrap();
        // W2 (codex r3 NEW-4): publish_now is command-based — 202 + status
        // URL; the publisher tick executes the saga.
        assert_eq!(published.status(), axum::http::StatusCode::ACCEPTED);

        let invalid_list = app
            .clone()
            .oneshot(json_request(
                "GET",
                "/admin/drafts?limit=0",
                &serde_json::Value::Null,
                Some("admin-token"),
            ))
            .await
            .unwrap();
        assert_eq!(
            invalid_list.status(),
            axum::http::StatusCode::UNPROCESSABLE_ENTITY
        );

        let listed = app
            .oneshot(json_request(
                "GET",
                "/admin/drafts",
                &serde_json::Value::Null,
                Some("admin-token"),
            ))
            .await
            .unwrap();
        assert_eq!(listed.status(), axum::http::StatusCode::OK);
    }

    fn json_request(
        method: &str,
        uri: &str,
        body: &serde_json::Value,
        admin: Option<&str>,
    ) -> axum::http::Request<axum::body::Body> {
        let mut builder = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header(axum::http::header::CONTENT_TYPE, "application/json");
        if let Some(admin) = admin {
            builder = builder.header("x-admin-token", admin);
        }
        builder
            .body(axum::body::Body::from(body.to_string()))
            .unwrap()
    }
}
