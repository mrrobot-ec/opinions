//! W2 compliance admin plane (D33/D34 + the money-command matrix).
//!
//! Zero business logic: every handler threads the authenticated
//! [`AdminContext`] into a W2 use case. Dual-control distinctness, role
//! sets, delays and replay keys are enforced inside those use cases — this
//! module only binds paths, bodies and status codes.
//!
//! Path convention, matching the capability matrix rows in
//! `http/middleware.rs`: `.../propose` carries the **subject** id (user,
//! flag, exclusion), `.../confirm` carries the **proposal** id, because the
//! matrix's replay key for a confirm is the proposal id.
//!
//! The admin router is state-erased so the composition root can mount it
//! *inside* the existing D26 RBAC sub-router; it must never be merged into
//! the unguarded public router.

use std::sync::Arc;

use application::model::{AdminContext, UserId, UserStatus};
use application::money::admin::{
    ComplianceAdminStore, FrozenFundsLicense, MoneyProposal, ProposalStatus,
};
use application::money::aml::AmlFlag;
use application::money::kyc::sandbox_complete_full;
use application::money::self_exclusion::{
    confirm_clear_flag, confirm_frozen_license, confirm_lift, effective_deposit_limit,
    propose_clear_flag, propose_frozen_license, propose_lift, set_deposit_limit,
    start_self_exclusion, SelfExclusion, UserDepositLimit,
};
use application::money::statuses::{confirm_status, propose_status, set_shadow, status_name};
use application::ports::Clock;
use axum::extract::{Extension, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use time::Duration;
use uuid::Uuid;

use super::super::dto::{
    AmlFlagDto, DepositLimitDto, DepositLimitRequest, DualControlBody, FrozenLicenseDto,
    FrozenLicenseRequest, KycEventDto, ProposalDto, SandboxKycCompleteRequest, SelfExclusionDto,
    SelfExclusionRequest, StatusDto, StatusProposeRequest,
};
use super::super::error::{ApiResult, ErrorResponse};

/// Sandbox KYC horizon: long enough for a staging money-path run, short
/// enough that a forgotten staging user re-screens.
const SANDBOX_KYC_HORIZON: Duration = Duration::days(30);

/// Composition-root state for the compliance plane.
#[derive(Clone)]
pub struct ComplianceAdminState<S> {
    pub store: S,
    pub clock: Arc<dyn Clock>,
    /// Two-factor staging arm (`OPINIONS_ENV=staging` AND `SANDBOX_KYC=1`),
    /// armed exactly like the faucet: off means the route is NOT mounted.
    pub sandbox_kyc: bool,
    /// Policy version stamped on sandbox-completed KYC facts.
    pub policy_version: String,
    /// Demo token guarding the self-service (non-admin) routes.
    pub demo_token: String,
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

fn status_dto(user: UserId, status: UserStatus) -> StatusDto {
    StatusDto {
        user_id: user.0,
        status: status_name(status).to_string(),
    }
}

fn flag_dto(flag: &AmlFlag) -> AmlFlagDto {
    AmlFlagDto {
        id: flag.id,
        user_id: flag.user.0,
        rule: flag.rule.as_str().to_string(),
        open: flag.open,
    }
}

fn license_dto(license: &FrozenFundsLicense) -> FrozenLicenseDto {
    FrozenLicenseDto {
        id: license.id,
        user_id: license.user.0,
        dest: license.dest.clone(),
        amount_micro: license.amount_micro,
    }
}

fn exclusion_dto(exclusion: &SelfExclusion) -> SelfExclusionDto {
    SelfExclusionDto {
        id: exclusion.id,
        cooling_off_until: exclusion.cooling_off_until,
    }
}

fn limit_dto(limit: &UserDepositLimit, now: time::OffsetDateTime) -> DepositLimitDto {
    DepositLimitDto {
        limit_micro: effective_deposit_limit(limit, now),
        pending_limit_micro: limit.pending_limit_micro,
        pending_effective_at: limit.pending_effective_at,
    }
}

fn require_demo(headers: &HeaderMap, expected: &str) -> ApiResult<()> {
    let got = headers
        .get("x-demo-token")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    // Constant-time: see `http::middleware::secret_eq`.
    if crate::http::middleware::secret_eq(got, expected) {
        Ok(())
    } else {
        Err(ErrorResponse::new(
            StatusCode::UNAUTHORIZED,
            "Unauthorized",
            "missing or invalid x-demo-token",
        ))
    }
}

/// Admin plane. Merge INSIDE the RBAC sub-router — every path here has a
/// capability row in `http/middleware.rs`.
pub fn admin_router<S>(state: ComplianceAdminState<S>) -> Router
where
    S: ComplianceAdminStore + Clone + Send + Sync + 'static,
{
    Router::new()
        .route(
            "/admin/aml/{id}/clear/propose",
            post(aml_clear_propose::<S>),
        )
        .route(
            "/admin/aml/{id}/clear/confirm",
            post(aml_clear_confirm::<S>),
        )
        .route("/admin/users/{id}/ban/propose", post(ban_propose::<S>))
        .route("/admin/users/{id}/ban/confirm", post(status_confirm::<S>))
        .route("/admin/users/{id}/unban/propose", post(unban_propose::<S>))
        .route("/admin/users/{id}/unban/confirm", post(status_confirm::<S>))
        .route("/admin/users/{id}/shadow", post(shadow::<S>))
        .route("/admin/users/{id}/unshadow", post(unshadow::<S>))
        .route(
            "/admin/self_exclusions/{id}/lift/propose",
            post(lift_propose::<S>),
        )
        .route(
            "/admin/self_exclusions/{id}/lift/confirm",
            post(lift_confirm::<S>),
        )
        .route(
            "/admin/frozen_funds/{id}/license/propose",
            post(license_propose::<S>),
        )
        .route(
            "/admin/frozen_funds/{id}/license/confirm",
            post(license_confirm::<S>),
        )
        .with_state(state)
}

/// Self-service plane: user-initiated self-exclusion and self-set deposit
/// limits (D34). Demo-token authenticated, never admin.
pub fn public_router<S>(state: ComplianceAdminState<S>) -> Router
where
    S: ComplianceAdminStore + Clone + Send + Sync + 'static,
{
    let sandbox = state.sandbox_kyc;
    let router = Router::new()
        .route("/self_exclusions", post(start_exclusion::<S>))
        .route("/users/{id}/deposit_limit", post(deposit_limit::<S>));
    let router = if sandbox {
        router.route("/sandbox/kyc/complete", post(sandbox_kyc::<S>))
    } else {
        router
    };
    router.with_state(state)
}

async fn aml_clear_propose<S: ComplianceAdminStore + Send + Sync>(
    State(state): State<ComplianceAdminState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
    Json(body): Json<DualControlBody>,
) -> ApiResult<Json<ProposalDto>> {
    let proposal =
        propose_clear_flag(&state.store, &actor, id, body.reason, state.clock.now()).await?;
    Ok(Json(proposal_dto(&proposal)))
}

async fn aml_clear_confirm<S: ComplianceAdminStore + Send + Sync>(
    State(state): State<ComplianceAdminState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<AmlFlagDto>> {
    let flag = confirm_clear_flag(&state.store, &actor, id, state.clock.now()).await?;
    Ok(Json(flag_dto(&flag)))
}

async fn ban_propose<S: ComplianceAdminStore + Send + Sync>(
    State(state): State<ComplianceAdminState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
    Json(body): Json<StatusProposeRequest>,
) -> ApiResult<Json<ProposalDto>> {
    execute_status_propose(&state, &actor, id, UserStatus::Banned, body).await
}

async fn unban_propose<S: ComplianceAdminStore + Send + Sync>(
    State(state): State<ComplianceAdminState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
    Json(body): Json<StatusProposeRequest>,
) -> ApiResult<Json<ProposalDto>> {
    execute_status_propose(&state, &actor, id, UserStatus::Active, body).await
}

async fn execute_status_propose<S: ComplianceAdminStore + Send + Sync>(
    state: &ComplianceAdminState<S>,
    actor: &AdminContext,
    user: Uuid,
    target: UserStatus,
    body: StatusProposeRequest,
) -> ApiResult<Json<ProposalDto>> {
    let proposal = propose_status(
        &state.store,
        actor,
        UserId(user),
        target,
        body.reason,
        body.epoch,
        state.clock.now(),
    )
    .await?;
    Ok(Json(proposal_dto(&proposal)))
}

async fn status_confirm<S: ComplianceAdminStore + Send + Sync>(
    State(state): State<ComplianceAdminState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<StatusDto>> {
    let now = state.clock.now();
    let status = confirm_status(&state.store, &actor, id, now).await?;
    // The proposal, not the user, is addressed on a confirm; re-read the
    // subject so the echo names the user whose status actually moved.
    let mut tx = state
        .store
        .admin_tx()
        .await
        .map_err(application::error::AppError::from)?;
    let subject = tx
        .get_proposal(id)
        .await
        .map_err(application::error::AppError::from)?
        .subject_id;
    tx.commit()
        .await
        .map_err(application::error::AppError::from)?;
    Ok(Json(status_dto(UserId(subject), status)))
}

async fn shadow<S: ComplianceAdminStore + Send + Sync>(
    State(state): State<ComplianceAdminState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
    Json(body): Json<DualControlBody>,
) -> ApiResult<Json<StatusDto>> {
    execute_shadow(&state, &actor, id, true, body).await
}

async fn unshadow<S: ComplianceAdminStore + Send + Sync>(
    State(state): State<ComplianceAdminState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
    Json(body): Json<DualControlBody>,
) -> ApiResult<Json<StatusDto>> {
    execute_shadow(&state, &actor, id, false, body).await
}

async fn execute_shadow<S: ComplianceAdminStore + Send + Sync>(
    state: &ComplianceAdminState<S>,
    actor: &AdminContext,
    user: Uuid,
    shadow: bool,
    body: DualControlBody,
) -> ApiResult<Json<StatusDto>> {
    let status = set_shadow(
        &state.store,
        actor,
        UserId(user),
        shadow,
        body.reason,
        state.clock.now(),
    )
    .await?;
    Ok(Json(status_dto(UserId(user), status)))
}

async fn lift_propose<S: ComplianceAdminStore + Send + Sync>(
    State(state): State<ComplianceAdminState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
    Json(body): Json<DualControlBody>,
) -> ApiResult<Json<ProposalDto>> {
    let proposal = propose_lift(&state.store, &actor, id, body.reason, state.clock.now()).await?;
    Ok(Json(proposal_dto(&proposal)))
}

async fn lift_confirm<S: ComplianceAdminStore + Send + Sync>(
    State(state): State<ComplianceAdminState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<SelfExclusionDto>> {
    let lifted = confirm_lift(&state.store, &actor, id, state.clock.now()).await?;
    Ok(Json(exclusion_dto(&lifted)))
}

async fn license_propose<S: ComplianceAdminStore + Send + Sync>(
    State(state): State<ComplianceAdminState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
    Json(body): Json<FrozenLicenseRequest>,
) -> ApiResult<Json<ProposalDto>> {
    let license = FrozenFundsLicense {
        id: Uuid::new_v4(),
        user: UserId(id),
        dest: body.dest,
        amount_micro: body.amount_micro,
    };
    let proposal = propose_frozen_license(
        &state.store,
        &actor,
        license,
        body.reason,
        state.clock.now(),
    )
    .await?;
    Ok(Json(proposal_dto(&proposal)))
}

async fn license_confirm<S: ComplianceAdminStore + Send + Sync>(
    State(state): State<ComplianceAdminState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<FrozenLicenseDto>> {
    let license = confirm_frozen_license(&state.store, &actor, id, state.clock.now()).await?;
    Ok(Json(license_dto(&license)))
}

async fn start_exclusion<S: ComplianceAdminStore + Send + Sync>(
    State(state): State<ComplianceAdminState<S>>,
    headers: HeaderMap,
    Json(body): Json<SelfExclusionRequest>,
) -> ApiResult<Json<SelfExclusionDto>> {
    require_demo(&headers, &state.demo_token)?;
    let exclusion = start_self_exclusion(
        &state.store,
        UserId(body.user_id),
        Duration::hours(body.cooling_off_hours),
        state.clock.now(),
    )
    .await?;
    Ok(Json(exclusion_dto(&exclusion)))
}

async fn deposit_limit<S: ComplianceAdminStore + Send + Sync>(
    State(state): State<ComplianceAdminState<S>>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(body): Json<DepositLimitRequest>,
) -> ApiResult<Json<DepositLimitDto>> {
    require_demo(&headers, &state.demo_token)?;
    let now = state.clock.now();
    let limit = set_deposit_limit(&state.store, UserId(id), body.limit_micro, now).await?;
    Ok(Json(limit_dto(&limit, now)))
}

async fn sandbox_kyc<S: ComplianceAdminStore + Send + Sync>(
    State(state): State<ComplianceAdminState<S>>,
    headers: HeaderMap,
    Json(body): Json<SandboxKycCompleteRequest>,
) -> ApiResult<Json<KycEventDto>> {
    require_demo(&headers, &state.demo_token)?;
    let event = sandbox_complete_full(
        &state.store,
        UserId(body.user_id),
        state.clock.now(),
        SANDBOX_KYC_HORIZON,
        state.policy_version.clone(),
    )
    .await
    .map_err(application::error::AppError::from)?;
    Ok(Json(KycEventDto {
        user_id: body.user_id,
        to_tier: event.to_tier,
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use application::fakes::{FakeClock, FakeComplianceStore};
    use application::model::AdminRole;
    use application::money::aml::{AmlDirection, AmlLeg, AmlPolicy, PINNED_WITHDRAW_BAND_MICRO};
    use application::money::{evaluate_at_request, ComplianceStore};
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    fn t0() -> time::OffsetDateTime {
        time::OffsetDateTime::UNIX_EPOCH + Duration::days(80_000)
    }

    fn admin(role: AdminRole, token: &str) -> AdminContext {
        AdminContext::Admin {
            token_digest: token.into(),
            role,
        }
    }

    fn state(
        store: FakeComplianceStore,
        now: time::OffsetDateTime,
        sandbox: bool,
    ) -> ComplianceAdminState<FakeComplianceStore> {
        ComplianceAdminState {
            store,
            clock: Arc::new(FakeClock::at(now)),
            sandbox_kyc: sandbox,
            policy_version: "kyc-1".into(),
            demo_token: "demo".into(),
        }
    }

    fn post_json(uri: &str, actor: Option<AdminContext>, body: Value) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .header("x-demo-token", "demo");
        if let Some(actor) = actor {
            builder = builder.extension(actor);
        }
        builder.body(Body::from(body.to_string())).unwrap()
    }

    async fn body_json(response: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn open_flag(
        store: &FakeComplianceStore,
        user: UserId,
        now: time::OffsetDateTime,
    ) -> Uuid {
        let policy = AmlPolicy::seed();
        for n in 0..4 {
            evaluate_at_request(
                store,
                AmlLeg {
                    id: Uuid::new_v4(),
                    user,
                    dest: "dest-a".into(),
                    amount_micro: PINNED_WITHDRAW_BAND_MICRO,
                    at: now + Duration::minutes(i64::from(n)),
                    direction: AmlDirection::Withdrawal,
                },
                policy,
            )
            .await
            .unwrap();
        }
        let mut tx = ComplianceStore::compliance_tx(store).await.unwrap();
        let flags = tx.open_aml_flags(user).await.unwrap();
        tx.commit().await.unwrap();
        flags[0].id
    }

    #[tokio::test]
    async fn aml_clear_is_two_person_and_ban_flows_end_to_end() {
        let store = FakeComplianceStore::new();
        let user = store.add_user("admin-routes");
        let now = t0();
        let flag = open_flag(&store, user, now).await;
        let app = admin_router(state(store.clone(), now, false));

        let proposed = app
            .clone()
            .oneshot(post_json(
                &format!("/admin/aml/{flag}/clear/propose"),
                Some(admin(AdminRole::Finance, "fin-1")),
                json!({ "reason": "manual review cleared" }),
            ))
            .await
            .unwrap();
        assert_eq!(proposed.status(), StatusCode::OK);
        let proposal = body_json(proposed).await;
        assert_eq!(proposal["kind"], "clear_aml");
        assert_eq!(proposal["status"], "pending");
        let proposal_id = proposal["id"].as_str().unwrap().to_string();

        // Same token cannot confirm its own proposal.
        let same_token = app
            .clone()
            .oneshot(post_json(
                &format!("/admin/aml/{proposal_id}/clear/confirm"),
                Some(admin(AdminRole::Superadmin, "fin-1")),
                json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(same_token.status(), StatusCode::FORBIDDEN);

        let confirmed = app
            .clone()
            .oneshot(post_json(
                &format!("/admin/aml/{proposal_id}/clear/confirm"),
                Some(admin(AdminRole::Superadmin, "root-1")),
                json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(confirmed.status(), StatusCode::OK);
        let cleared = body_json(confirmed).await;
        assert_eq!(cleared["open"], false);
        assert_eq!(cleared["rule"], "structuring");
        assert_eq!(cleared["user_id"], user.0.to_string());

        // Ban: ops proposes, superadmin confirms after the 15-minute delay.
        let ban = app
            .clone()
            .oneshot(post_json(
                &format!("/admin/users/{}/ban/propose", user.0),
                Some(admin(AdminRole::Ops, "ops-1")),
                json!({ "reason": "sanctions hit", "epoch": 1 }),
            ))
            .await
            .unwrap();
        assert_eq!(ban.status(), StatusCode::OK);
        let ban_proposal = body_json(ban).await;
        assert_eq!(ban_proposal["kind"], "ban_user");
        let ban_id = ban_proposal["id"].as_str().unwrap().to_string();

        let too_early = app
            .clone()
            .oneshot(post_json(
                &format!("/admin/users/{ban_id}/ban/confirm"),
                Some(admin(AdminRole::Superadmin, "root-1")),
                json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(too_early.status(), StatusCode::CONFLICT);

        let later = admin_router(state(store.clone(), now + Duration::minutes(16), false));
        let banned = later
            .clone()
            .oneshot(post_json(
                &format!("/admin/users/{ban_id}/ban/confirm"),
                Some(admin(AdminRole::Superadmin, "root-1")),
                json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(banned.status(), StatusCode::OK);
        assert_eq!(body_json(banned).await["status"], "banned");

        // Unban runs the same confirm handler through its own propose path.
        let unban = later
            .clone()
            .oneshot(post_json(
                &format!("/admin/users/{}/unban/propose", user.0),
                Some(admin(AdminRole::Ops, "ops-1")),
                json!({ "reason": "cleared", "epoch": 2 }),
            ))
            .await
            .unwrap();
        assert_eq!(unban.status(), StatusCode::OK);
        let unban_id = body_json(unban).await["id"].as_str().unwrap().to_string();
        let restored = admin_router(state(store.clone(), now + Duration::minutes(32), false))
            .oneshot(post_json(
                &format!("/admin/users/{unban_id}/unban/confirm"),
                Some(admin(AdminRole::Superadmin, "root-1")),
                json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(restored.status(), StatusCode::OK);
        assert_eq!(body_json(restored).await["status"], "active");
    }

    #[tokio::test]
    async fn shadow_is_single_ops_and_licenses_are_two_person() {
        let store = FakeComplianceStore::new();
        let user = store.add_user("shadow-routes");
        let now = t0();
        let app = admin_router(state(store.clone(), now, false));

        let shadowed = app
            .clone()
            .oneshot(post_json(
                &format!("/admin/users/{}/shadow", user.0),
                Some(admin(AdminRole::Ops, "ops-1")),
                json!({ "reason": "risk signal" }),
            ))
            .await
            .unwrap();
        assert_eq!(shadowed.status(), StatusCode::OK);
        assert_eq!(body_json(shadowed).await["status"], "shadow_limited");

        let unshadowed = app
            .clone()
            .oneshot(post_json(
                &format!("/admin/users/{}/unshadow", user.0),
                Some(admin(AdminRole::Ops, "ops-1")),
                json!({ "reason": "resolved" }),
            ))
            .await
            .unwrap();
        assert_eq!(unshadowed.status(), StatusCode::OK);
        assert_eq!(body_json(unshadowed).await["status"], "active");

        // Frozen-funds license: dest is counsel-shaped, never user-picked.
        let proposed = app
            .clone()
            .oneshot(post_json(
                &format!("/admin/frozen_funds/{}/license/propose", user.0),
                Some(admin(AdminRole::Finance, "fin-1")),
                json!({
                    "dest": "counsel-escrow-1",
                    "amount_micro": 1_000_000,
                    "reason": "court order"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(proposed.status(), StatusCode::OK);
        let license_proposal = body_json(proposed).await;
        assert_eq!(license_proposal["kind"], "frozen_funds_license");
        let proposal_id = license_proposal["id"].as_str().unwrap().to_string();

        let early = app
            .clone()
            .oneshot(post_json(
                &format!("/admin/frozen_funds/{proposal_id}/license/confirm"),
                Some(admin(AdminRole::Superadmin, "root-1")),
                json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(early.status(), StatusCode::CONFLICT);

        let confirmed = admin_router(state(
            store.clone(),
            now + Duration::hours(24) + Duration::minutes(5),
            false,
        ))
        .oneshot(post_json(
            &format!("/admin/frozen_funds/{proposal_id}/license/confirm"),
            Some(admin(AdminRole::Superadmin, "root-1")),
            json!({}),
        ))
        .await
        .unwrap();
        assert_eq!(confirmed.status(), StatusCode::OK);
        let license = body_json(confirmed).await;
        assert_eq!(license["dest"], "counsel-escrow-1");
        assert_eq!(license["amount_micro"], 1_000_000);
        assert_eq!(license["user_id"], user.0.to_string());
    }

    #[tokio::test]
    async fn self_service_exclusion_limits_and_lift() {
        let store = FakeComplianceStore::new();
        let user = store.add_user("self-serve");
        let now = t0();
        let public = public_router(state(store.clone(), now, true));

        let unauthorized = public
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/self_exclusions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"user_id": user.0, "cooling_off_hours": 24}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let started = public
            .clone()
            .oneshot(post_json(
                "/self_exclusions",
                None,
                json!({"user_id": user.0, "cooling_off_hours": 24}),
            ))
            .await
            .unwrap();
        assert_eq!(started.status(), StatusCode::OK);
        let exclusion = body_json(started).await;
        let exclusion_id = exclusion["id"].as_str().unwrap().to_string();

        // Irreversible while the cooling-off runs: a second start is refused.
        let again = public
            .clone()
            .oneshot(post_json(
                "/self_exclusions",
                None,
                json!({"user_id": user.0, "cooling_off_hours": 24}),
            ))
            .await
            .unwrap();
        assert_eq!(again.status(), StatusCode::FORBIDDEN);

        // Lowering is immediate; raising waits 24h.
        let lowered = public
            .clone()
            .oneshot(post_json(
                &format!("/users/{}/deposit_limit", user.0),
                None,
                json!({ "limit_micro": 50_000_000 }),
            ))
            .await
            .unwrap();
        assert_eq!(lowered.status(), StatusCode::OK);
        assert_eq!(body_json(lowered).await["limit_micro"], 50_000_000);

        let raised = public
            .clone()
            .oneshot(post_json(
                &format!("/users/{}/deposit_limit", user.0),
                None,
                json!({ "limit_micro": 900_000_000 }),
            ))
            .await
            .unwrap();
        assert_eq!(raised.status(), StatusCode::OK);
        let pending = body_json(raised).await;
        assert_eq!(pending["limit_micro"], 50_000_000);
        assert_eq!(pending["pending_limit_micro"], 900_000_000);

        let sandbox = public
            .clone()
            .oneshot(post_json(
                "/sandbox/kyc/complete",
                None,
                json!({ "user_id": user.0 }),
            ))
            .await
            .unwrap();
        assert_eq!(sandbox.status(), StatusCode::OK);
        assert_eq!(body_json(sandbox).await["to_tier"], 2);

        // Lift is post-expiry, finance proposes, superadmin confirms.
        let after = now + Duration::hours(25);
        let admin_app = admin_router(state(store.clone(), after, false));
        let proposed = admin_app
            .clone()
            .oneshot(post_json(
                &format!("/admin/self_exclusions/{exclusion_id}/lift/propose"),
                Some(admin(AdminRole::Finance, "fin-1")),
                json!({ "reason": "user requested after cooling-off" }),
            ))
            .await
            .unwrap();
        assert_eq!(proposed.status(), StatusCode::OK);
        let proposal_id = body_json(proposed).await["id"]
            .as_str()
            .unwrap()
            .to_string();
        let confirmed = admin_app
            .oneshot(post_json(
                &format!("/admin/self_exclusions/{proposal_id}/lift/confirm"),
                Some(admin(AdminRole::Superadmin, "root-1")),
                json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(confirmed.status(), StatusCode::OK);
        assert_eq!(body_json(confirmed).await["id"], exclusion_id);
    }

    #[tokio::test]
    async fn sandbox_route_is_not_mounted_without_the_second_factor() {
        let store = FakeComplianceStore::new();
        let user = store.add_user("no-sandbox");
        let public = public_router(state(store, t0(), false));
        let response = public
            .oneshot(post_json(
                "/sandbox/kyc/complete",
                None,
                json!({ "user_id": user.0 }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn proposal_status_names_are_stable() {
        assert_eq!(proposal_status_name(ProposalStatus::Pending), "pending");
        assert_eq!(proposal_status_name(ProposalStatus::Confirmed), "confirmed");
        assert_eq!(proposal_status_name(ProposalStatus::Rejected), "rejected");
        assert_eq!(proposal_status_name(ProposalStatus::Expired), "expired");
    }
}
