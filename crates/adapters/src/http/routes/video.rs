//! Artifact serving + on-demand share cards (Task 5.2).
//!
//! `GET /assets/{job_id}.svg` resolves the JOB by id and status — the path
//! is never a filesystem input (codex M4): the only accepted shape is a
//! UUID + `.svg`, and the on-disk name is derived from the job row itself.

use application::error::AppError;
use application::model::{JobId, JobStatus, MarketId, UserId};
use application::ports::{MarketQueries, NotificationQueries, SocialQueries, Store};
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use uuid::Uuid;

use crate::http::dto::video::{svg_response, CACHE_IMMUTABLE, CACHE_SHORT};

use super::{ApiResult, AppState, ErrorResponse};

pub(super) fn router<S>() -> Router<AppState<S>>
where
    S: Store + MarketQueries + SocialQueries + NotificationQueries + Send + Sync + 'static,
{
    Router::new()
        .route("/assets/{file}", get(serve_asset::<S>))
        .route(
            "/users/{user_id}/share_card/{market_id}",
            get(share_card::<S>),
        )
}

fn not_found() -> ErrorResponse {
    ErrorResponse::from(AppError::ArtifactNotFound)
}

#[utoipa::path(
    get,
    path = "/assets/{file}",
    tag = "video",
    params(("file" = String, Path, description = "`{job_id}.svg`")),
    responses(
        (status = 200, description = "finalized artifact bytes"),
        (status = 404, description = "unknown job or artifact"),
        (status = 409, description = "job not finalized"),
    ),
)]
pub(super) async fn serve_asset<S>(
    State(state): State<AppState<S>>,
    Path(file): Path<String>,
) -> ApiResult<Response>
where
    S: Store + MarketQueries + SocialQueries + NotificationQueries + Send + Sync + 'static,
{
    // The ONLY accepted input shape: a job UUID plus the `.svg` suffix.
    let job_id = file
        .strip_suffix(".svg")
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .ok_or_else(not_found)?;
    let mut tx = state.inner.store.video_tx().await.map_err(AppError::from)?;
    let job = tx.job(JobId(job_id)).await.map_err(AppError::from)?;
    tx.commit().await.map_err(AppError::from)?;
    let job = job.ok_or_else(not_found)?;
    if !matches!(job.status, JobStatus::Ready | JobStatus::Attached) {
        return Err(AppError::JobNotReady.into());
    }
    let bytes = state
        .inner
        .phase5
        .renderer
        .load(&state.inner.phase5.config.render_dir, job.id, job.kind)
        .await
        .map_err(AppError::from)?
        .ok_or_else(not_found)?;
    // Immutable cache is legal here: ready/attached artifacts never change.
    Ok(svg_response(bytes, CACHE_IMMUTABLE))
}

#[utoipa::path(
    get,
    path = "/users/{user_id}/share_card/{market_id}",
    tag = "video",
    params(
        ("user_id" = Uuid, Path, description = "user"),
        ("market_id" = Uuid, Path, description = "market"),
    ),
    responses(
        (status = 200, description = "share-card SVG (post-resolution only)"),
        (status = 404, description = "unknown market/user or no participation"),
        (status = 409, description = "market not resolved yet"),
    ),
)]
pub(super) async fn share_card<S>(
    State(state): State<AppState<S>>,
    Path((user_id, market_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Response>
where
    S: Store + MarketQueries + SocialQueries + NotificationQueries + Send + Sync + 'static,
{
    let bytes = application::video::share_card::share_card_svg(
        &state.inner.store,
        state.inner.phase5.renderer.as_ref(),
        &state.inner.phase5.config,
        UserId(user_id),
        MarketId(market_id),
    )
    .await?;
    Ok(svg_response(bytes, CACHE_SHORT))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use application::fakes::InMemoryStore;
    use application::model::{
        ArtifactKind, ContentConfig, NewMarket, RealizationFact, RealizationSource,
    };
    use application::ports::{Clock, SettlementIo, Store};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use domain::money::MicroUsd;
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use time::OffsetDateTime;
    use tower::util::ServiceExt;
    use uuid::Uuid;

    use crate::http::routes::{router, AppState, Phase5Services};

    fn at(unix: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(unix).unwrap()
    }

    struct FixedClock(OffsetDateTime);
    impl Clock for FixedClock {
        fn now(&self) -> OffsetDateTime {
            self.0
        }
    }

    fn scratch() -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join("opinions-w2-http")
            .join(Uuid::new_v4().to_string());
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn app(store: InMemoryStore, render_dir: std::path::PathBuf) -> axum::Router {
        let phase5 = Phase5Services {
            config: ContentConfig {
                render_dir,
                ..ContentConfig::default()
            },
            ..Phase5Services::default()
        };
        let state = AppState::with_phase5_configs(
            store,
            Arc::new(FixedClock(at(1_700_000_000))),
            application::model::ResolveConfig {
                oi_floor: MicroUsd(0),
            },
            application::model::RepConfig::default(),
            application::model::VoteIntegrityConfig::default(),
            application::model::IntegritySweepConfig::default(),
            crate::http::routes::VoteMetadataConfig::default(),
            application::model::SocialConfig::default(),
            Vec::new(),
            phase5,
        );
        router(state)
    }

    async fn seed_market(store: &InMemoryStore) -> application::model::MarketId {
        let mut tx = store.seed_tx().await.unwrap();
        let key = format!("http-video-{}", Uuid::new_v4());
        tx.serialize_key(&key).await.unwrap();
        let market = tx
            .insert_market(NewMarket {
                id: application::model::MarketId(Uuid::new_v4()),
                slug: key.clone(),
                min_votes_to_resolve: 3,
                closes_at: at(1_700_000_000) + time::Duration::hours(2),
                tally_hidden_at: at(1_700_000_000) + time::Duration::hours(1),
            })
            .await
            .unwrap();
        tx.commit().await.unwrap();
        market
    }

    async fn get(app: &axum::Router, uri: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, headers, body.to_vec())
    }

    #[tokio::test]
    async fn serves_finalized_artifacts_with_the_pinned_posture() {
        let store = InMemoryStore::default();
        let dir = scratch();
        let market = seed_market(&store).await;

        // Enqueue → claim → render+persist → complete+attach, driving the
        // real adapter renderer into the temp render dir.
        let now = at(1_700_000_000);
        let job = {
            let mut tx = store.video_tx().await.unwrap();
            let job = tx
                .enqueue(market, None, ArtifactKind::Poster, now)
                .await
                .unwrap();
            tx.commit().await.unwrap();
            job
        };
        application::video::worker_tick(
            &store,
            &FixedClock(now),
            crate::render::renderer().as_ref(),
            &ContentConfig {
                render_dir: dir.clone(),
                ..ContentConfig::default()
            },
        )
        .await
        .unwrap();

        let app = app(store.clone(), dir.clone());
        let (status, headers, body) = get(&app, &format!("/assets/{}.svg", job.0)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers.get("content-type").unwrap(),
            "image/svg+xml; charset=utf-8"
        );
        assert_eq!(headers.get("x-content-type-options").unwrap(), "nosniff");
        assert_eq!(
            headers.get("content-security-policy").unwrap(),
            "default-src 'none'"
        );
        assert_eq!(headers.get("content-disposition").unwrap(), "inline");
        assert_eq!(
            headers.get("cache-control").unwrap(),
            "public, max-age=31536000, immutable"
        );
        assert!(String::from_utf8(body).unwrap().starts_with("<svg"));
    }

    #[tokio::test]
    async fn refuses_unknown_pending_and_hostile_asset_paths() {
        let store = InMemoryStore::default();
        let dir = scratch();
        let market = seed_market(&store).await;
        let now = at(1_700_000_000);
        let queued = {
            let mut tx = store.video_tx().await.unwrap();
            let job = tx
                .enqueue(market, None, ArtifactKind::Poster, now)
                .await
                .unwrap();
            tx.commit().await.unwrap();
            job
        };
        let app = app(store, dir);

        // Unknown job id.
        let (status, _, _) = get(&app, &format!("/assets/{}.svg", Uuid::new_v4())).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // Queued (not finalized) job.
        let (status, _, _) = get(&app, &format!("/assets/{}.svg", queued.0)).await;
        assert_eq!(status, StatusCode::CONFLICT);
        // Not a UUID, wrong suffix, traversal shapes.
        for hostile in [
            "/assets/not-a-uuid.svg",
            "/assets/x.png",
            "/assets/..%2F..%2Fetc%2Fpasswd",
            "/assets/%2e%2e/secret.svg",
        ] {
            let (status, _, _) = get(&app, hostile).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "for {hostile}");
        }
    }

    #[tokio::test]
    async fn share_card_route_gates_and_serves() {
        let store = InMemoryStore::default();
        let dir = scratch();
        let market = seed_market(&store).await;
        let user = {
            let mut tx = store.bootstrap_tx().await.unwrap();
            tx.serialize_key("http-card-user").await.unwrap();
            let user = tx.insert_user("card-bob").await.unwrap();
            tx.commit().await.unwrap();
            user
        };
        let app = app(store.clone(), dir);

        // Pre-resolution → 409.
        let (status, _, _) = get(&app, &format!("/users/{}/share_card/{}", user.0, market.0)).await;
        assert_eq!(status, StatusCode::CONFLICT);

        // Resolve the market.
        {
            let mut tx = store.resolve_tx().await.unwrap();
            tx.serialize_key("http-card-resolve").await.unwrap();
            tx.market_for_update(market).await.unwrap();
            SettlementIo::set_market_state(
                tx.as_mut(),
                market,
                domain::market::MarketState::Resolved,
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
        }

        // Resolved but no participation → 404.
        let (status, _, _) = get(&app, &format!("/users/{}/share_card/{}", user.0, market.0)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // Insert a realization; the card renders with the pinned posture.
        let yes = {
            let mut tx = store.video_tx().await.unwrap();
            let row = tx.market_row(market).await.unwrap();
            tx.commit().await.unwrap();
            row.yes_outcome
        };
        {
            let mut tx = store.resolve_tx().await.unwrap();
            tx.serialize_key("http-card-fact").await.unwrap();
            tx.insert_realization(&RealizationFact {
                user,
                market,
                outcome: yes,
                source: RealizationSource::Settlement,
                realized_delta: MicroUsd(2_500_000),
                payout: MicroUsd(12_500_000),
                ledger_txn: Uuid::new_v4(),
                created_at: at(1_700_000_100),
            })
            .await
            .unwrap();
            tx.commit().await.unwrap();
        }
        let (status, headers, body) =
            get(&app, &format!("/users/{}/share_card/{}", user.0, market.0)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.get("cache-control").unwrap(), "public, max-age=300");
        assert_eq!(headers.get("x-content-type-options").unwrap(), "nosniff");
        let svg = String::from_utf8(body).unwrap();
        assert!(svg.contains("@card-bob"));
        assert!(svg.contains("Payout $12.50"));
        assert!(svg.contains("Return +25.0%"));

        // Unknown market → 404.
        let (status, _, _) = get(
            &app,
            &format!("/users/{}/share_card/{}", user.0, Uuid::new_v4()),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
