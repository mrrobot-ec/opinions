//! Withdrawal HTTP handlers — zero business logic.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use application::model::{AdminContext, UserId};
use application::ports::withdraw_decide::{DecideCmd, DecideWithdraw};
use application::ports::withdraw_request::RequestWithdraw;
use application::ports::{
    canonicalize_dest, Clock, GeoResolver, RequestWithdrawCmd, SanctionsScreen, WithdrawStore,
    WithdrawalId, WithdrawalReceipt,
};
use axum::extract::{ConnectInfo, Extension, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use uuid::Uuid;

use super::super::dto::withdraw::{
    DecideRequestDto, WithdrawRequestDto, WithdrawalDecisionDto, WithdrawalReceiptDto,
};
use super::super::error::{ApiError, ApiResult};
use super::AppState;

/// Runtime screening services supplied by the composition root.
#[derive(Clone)]
pub struct WithdrawServices {
    pub geo: Arc<dyn GeoResolver>,
    pub sanctions: Arc<dyn SanctionsScreen>,
}

/// Public withdraw routes. Mounted by the coordinator into `core.rs` with a
/// [`WithdrawServices`] extension.
pub fn router<S>() -> Router<AppState<S>>
where
    S: WithdrawStore + Send + Sync + 'static,
{
    Router::new()
        .route("/withdrawals", post(request_withdraw::<S>))
        .route("/withdrawals/{id}", get(get_withdrawal::<S>))
}

/// Admin routes. This router must be merged inside the existing D26 RBAC
/// sub-router, never into the unguarded public router.
pub fn admin_router<S>() -> Router<AppState<S>>
where
    S: WithdrawStore + Send + Sync + 'static,
{
    Router::new()
        .route("/admin/withdrawals/{id}/approve", post(admin_approve::<S>))
        .route("/admin/withdrawals/{id}/deny", post(admin_deny::<S>))
        .route(
            "/admin/withdrawals/{id}/approve/propose",
            post(admin_propose::<S>),
        )
        .route(
            "/admin/withdrawals/{id}/approve/confirm",
            post(admin_confirm::<S>),
        )
}

#[utoipa::path(
    post,
    path = "/withdrawals",
    tag = "money",
    request_body = WithdrawRequestDto,
    responses(
        (status = 200, body = WithdrawalReceiptDto),
        (status = 403, body = WithdrawalReceiptDto),
        (status = 422, body = ApiError),
        (status = 503, body = WithdrawalReceiptDto)
    )
)]
async fn request_withdraw<S: WithdrawStore + Send + Sync + 'static>(
    State(state): State<AppState<S>>,
    Extension(services): Extension<WithdrawServices>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<WithdrawRequestDto>,
) -> ApiResult<(StatusCode, Json<WithdrawalReceiptDto>)> {
    super::require_demo(&headers, &state.inner.demo_token)?;
    let ip = client_ip(
        peer,
        &headers,
        &state.inner.vote_metadata_config.trusted_proxy_cidrs,
    );
    let receipt = execute_request(
        &state.inner.store,
        &state.inner.clock,
        services.geo.as_ref(),
        services.sanctions.as_ref(),
        body,
        ip,
    )
    .await
    .map_err(super::super::error::ErrorResponse::from)?;
    Ok((withdrawal_status(&receipt), Json(receipt)))
}

#[utoipa::path(
    get,
    path = "/withdrawals/{id}",
    tag = "money",
    params(("id" = Uuid, Path, description = "Withdrawal id")),
    responses((status = 200, body = WithdrawalReceiptDto), (status = 404, body = ApiError))
)]
async fn get_withdrawal<S: WithdrawStore + Send + Sync + 'static>(
    State(state): State<AppState<S>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<WithdrawalReceiptDto>> {
    super::require_demo(&headers, &state.inner.demo_token)?;
    let user = WithdrawStore::withdrawal_user(&state.inner.store, WithdrawalId(id))
        .await
        .map_err(application::error::AppError::from)?;
    let mut tx = WithdrawStore::withdraw_tx(&state.inner.store)
        .await
        .map_err(application::error::AppError::from)?;
    tx.lock_user(user)
        .await
        .map_err(application::error::AppError::from)?;
    let row = tx
        .withdrawal_for_update(WithdrawalId(id))
        .await
        .map_err(application::error::AppError::from)?;
    if row.user != user {
        return Err(application::error::AppError::Store(
            application::error::StoreError::Invariant("withdrawal owner changed after user lock"),
        )
        .into());
    }
    tx.commit()
        .await
        .map_err(application::error::AppError::from)?;
    Ok(Json(WithdrawalReceiptDto::from(WithdrawalReceipt {
        id: Some(row.id),
        user: row.user,
        dest: row.dest,
        amount_micro: row.amount_micro,
        combo: Some(row.combo),
        hold_tx_id: Some(row.hold_tx_id),
        replayed: false,
        refused: false,
        refuse_code: None,
        refuse_message: None,
    })))
}

/// Use-case entry used by tests and by the coordinator once `S: WithdrawStore`.
///
/// # Errors
/// Application refusals mapped by the caller.
pub async fn execute_request<S: WithdrawStore, C: Clock>(
    store: &S,
    clock: &C,
    geo: &dyn GeoResolver,
    sanctions: &dyn SanctionsScreen,
    body: WithdrawRequestDto,
    client_ip: Option<IpAddr>,
) -> Result<WithdrawalReceiptDto, application::error::AppError> {
    let dest =
        canonicalize_dest(&body.dest).ok_or(application::error::AppError::ConfigInvalid {
            key: "dest".into(),
            reason: "dest must be a 32-byte solana pubkey",
        })?;
    let confirmed = canonicalize_dest(&body.confirm_dest).ok_or(
        application::error::AppError::ConfigInvalid {
            key: "confirm_dest".into(),
            reason: "confirm_dest must be a 32-byte solana pubkey",
        },
    )?;
    if dest != confirmed {
        return Err(application::error::AppError::ConfigInvalid {
            key: "confirm_dest".into(),
            reason: "destination confirmation does not match",
        });
    }
    let receipt = RequestWithdraw {
        store,
        clock,
        geo,
        sanctions,
    }
    .execute(RequestWithdrawCmd {
        user: UserId(body.user_id),
        amount_micro: body.amount_micro,
        dest,
        client_ip,
        idempotency_key: body.idempotency_key,
    })
    .await?;
    Ok(WithdrawalReceiptDto::from(receipt))
}

#[must_use]
fn withdrawal_status(receipt: &WithdrawalReceiptDto) -> StatusCode {
    if !receipt.refused {
        return StatusCode::OK;
    }
    match receipt.refuse_code.as_deref() {
        Some("banned" | "kyc" | "geo_missing_ip") => StatusCode::FORBIDDEN,
        Some("paused") => StatusCode::LOCKED,
        Some("geo_unavailable" | "sanctions_unavailable") => StatusCode::SERVICE_UNAVAILABLE,
        Some("insufficient_funds") => StatusCode::PAYMENT_REQUIRED,
        Some("limit") => StatusCode::TOO_MANY_REQUESTS,
        _ => StatusCode::UNPROCESSABLE_ENTITY,
    }
}

#[utoipa::path(
    post,
    path = "/admin/withdrawals/{id}/approve",
    tag = "admin",
    params(("id" = Uuid, Path, description = "Withdrawal id")),
    request_body = DecideRequestDto,
    responses((status = 200, body = WithdrawalDecisionDto), (status = 403, body = ApiError))
)]
async fn admin_approve<S: WithdrawStore + Send + Sync + 'static>(
    State(state): State<AppState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
    Json(body): Json<DecideRequestDto>,
) -> ApiResult<Json<WithdrawalDecisionDto>> {
    decide_response(
        &state.inner.store,
        &state.inner.clock,
        actor,
        id,
        "approve",
        body,
    )
    .await
}

#[utoipa::path(
    post,
    path = "/admin/withdrawals/{id}/deny",
    tag = "admin",
    params(("id" = Uuid, Path, description = "Withdrawal id")),
    request_body = DecideRequestDto,
    responses((status = 200, body = WithdrawalDecisionDto), (status = 403, body = ApiError))
)]
async fn admin_deny<S: WithdrawStore + Send + Sync + 'static>(
    State(state): State<AppState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
    Json(body): Json<DecideRequestDto>,
) -> ApiResult<Json<WithdrawalDecisionDto>> {
    decide_response(
        &state.inner.store,
        &state.inner.clock,
        actor,
        id,
        "deny",
        body,
    )
    .await
}

#[utoipa::path(
    post,
    path = "/admin/withdrawals/{id}/approve/propose",
    tag = "admin",
    params(("id" = Uuid, Path, description = "Withdrawal id")),
    request_body = DecideRequestDto,
    responses((status = 200, body = WithdrawalDecisionDto), (status = 403, body = ApiError))
)]
async fn admin_propose<S: WithdrawStore + Send + Sync + 'static>(
    State(state): State<AppState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
    Json(body): Json<DecideRequestDto>,
) -> ApiResult<Json<WithdrawalDecisionDto>> {
    decide_response(
        &state.inner.store,
        &state.inner.clock,
        actor,
        id,
        "propose",
        body,
    )
    .await
}

#[utoipa::path(
    post,
    path = "/admin/withdrawals/{id}/approve/confirm",
    tag = "admin",
    params(("id" = Uuid, Path, description = "Withdrawal id")),
    request_body = DecideRequestDto,
    responses((status = 200, body = WithdrawalDecisionDto), (status = 403, body = ApiError))
)]
async fn admin_confirm<S: WithdrawStore + Send + Sync + 'static>(
    State(state): State<AppState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
    Json(body): Json<DecideRequestDto>,
) -> ApiResult<Json<WithdrawalDecisionDto>> {
    decide_response(
        &state.inner.store,
        &state.inner.clock,
        actor,
        id,
        "confirm",
        body,
    )
    .await
}

async fn decide_response<S: WithdrawStore, C: Clock>(
    store: &S,
    clock: &C,
    actor: AdminContext,
    id: Uuid,
    kind: &str,
    body: DecideRequestDto,
) -> ApiResult<Json<WithdrawalDecisionDto>> {
    let combo = execute_decide(store, clock, actor, id, kind, body)
        .await
        .map_err(super::super::error::ErrorResponse::from)?;
    Ok(Json(WithdrawalDecisionDto { id, combo }))
}

/// Admin decide entry.
///
/// # Errors
/// Application errors.
pub async fn execute_decide<S: WithdrawStore, C: Clock>(
    store: &S,
    clock: &C,
    actor: AdminContext,
    id: Uuid,
    kind: &str,
    body: DecideRequestDto,
) -> Result<String, application::error::AppError> {
    let cmd = match kind {
        "approve" => DecideCmd::FinanceApprove {
            id: WithdrawalId(id),
            reason: body.reason.unwrap_or_default(),
        },
        "deny" => DecideCmd::Deny {
            id: WithdrawalId(id),
            reason: body.reason.unwrap_or_default(),
        },
        "propose" => DecideCmd::ProposeDual {
            id: WithdrawalId(id),
            reason: body.reason.unwrap_or_default(),
        },
        "confirm" => DecideCmd::ConfirmDual {
            id: WithdrawalId(id),
        },
        "expire" => DecideCmd::ExpireProposal {
            id: WithdrawalId(id),
        },
        "machine" => DecideCmd::Machine {
            id: WithdrawalId(id),
        },
        _ => return Err(application::error::AppError::IllegalTransition),
    };
    let row = DecideWithdraw {
        store,
        clock,
        actor,
    }
    .execute(cmd)
    .await?;
    Ok(row.combo.label().unwrap_or("?").to_string())
}

/// Resolve a request IP without trusting caller-supplied forwarding headers
/// unless the direct peer belongs to a configured proxy network.
#[must_use]
pub fn client_ip(
    peer: SocketAddr,
    headers: &HeaderMap,
    trusted: &[ipnet::IpNet],
) -> Option<IpAddr> {
    let direct = peer.ip();
    if !trusted.iter().any(|network| network.contains(&direct)) {
        return Some(direct);
    }
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .into_iter()
        .flat_map(|value| value.split(',').rev())
        .filter_map(|part| part.trim().parse::<IpAddr>().ok())
        .find(|candidate| !trusted.iter().any(|network| network.contains(candidate)))
}

/// Placeholder clock adapter for handlers that only need `now`.
pub struct HandlerClock;

impl Clock for HandlerClock {
    fn now(&self) -> time::OffsetDateTime {
        time::OffsetDateTime::now_utc()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines, clippy::unwrap_used)]

    use super::*;
    use application::fakes::FakeClock;
    use application::model::UserStatus;
    use application::ports::withdraw_fakes::{dest_a, FakeScreen, FakeWithdrawStore};
    use application::ports::withdraw_request::RequestClock;
    use application::ports::ScreenVerdict;
    use axum::http::HeaderValue;

    #[tokio::test]
    async fn execute_request_returns_a_receipt() {
        let store = FakeWithdrawStore::new();
        let user = store.seed_user(UserStatus::Active, 2);
        store.credit(user, 20_000_000);
        let now = store.now();
        let screen = FakeScreen {
            verdict: ScreenVerdict::Clear {
                checked_at: now,
                expires_at: now + time::Duration::hours(1),
                policy_version: "1".into(),
            },
            fail: false,
        };
        let dto = execute_request(
            &store,
            &RequestClock(now),
            &screen,
            &screen,
            WithdrawRequestDto {
                user_id: user.0,
                amount_micro: 5_000_000,
                dest: dest_a(),
                confirm_dest: dest_a(),
                idempotency_key: Some("http-1".into()),
            },
            Some(IpAddr::from([203, 0, 113, 8])),
        )
        .await
        .unwrap();
        assert!(!dto.refused);
        assert_eq!(dto.combo.as_deref(), Some("W1"));
    }

    #[test]
    fn client_ip_only_trusts_forwarding_from_a_configured_proxy() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.9"));
        assert_eq!(
            client_ip("198.51.100.8:443".parse().unwrap(), &headers, &[]),
            Some(IpAddr::from([198, 51, 100, 8]))
        );
        assert_eq!(
            client_ip(
                "10.0.0.8:443".parse().unwrap(),
                &headers,
                &["10.0.0.0/8".parse().unwrap()]
            ),
            Some(IpAddr::from([203, 0, 113, 9]))
        );
        headers.insert("x-forwarded-for", HeaderValue::from_static("10.0.0.9"));
        assert_eq!(
            client_ip(
                "10.0.0.8:443".parse().unwrap(),
                &headers,
                &["10.0.0.0/8".parse().unwrap()]
            ),
            None,
            "a trusted-only proxy chain is not a client IP"
        );
    }

    #[tokio::test]
    async fn confirmation_echo_is_mandatory() {
        let store = FakeWithdrawStore::new();
        let user = store.seed_user(UserStatus::Active, 2);
        store.credit(user, 20_000_000);
        let now = store.now();
        let screen = FakeScreen {
            verdict: ScreenVerdict::Clear {
                checked_at: now,
                expires_at: now + time::Duration::hours(1),
                policy_version: "1".into(),
            },
            fail: false,
        };
        let err = execute_request(
            &store,
            &RequestClock(now),
            &screen,
            &screen,
            WithdrawRequestDto {
                user_id: user.0,
                amount_micro: 5_000_000,
                dest: dest_a(),
                confirm_dest: application::ports::withdraw_fakes::dest_b(),
                idempotency_key: Some("http-mismatch".into()),
            },
            Some(IpAddr::from([203, 0, 113, 8])),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            application::error::AppError::ConfigInvalid { ref key, .. }
                if key == "confirm_dest"
        ));
        assert!(store.withdrawals().is_empty());
    }

    #[test]
    fn persisted_refusal_receipts_keep_their_http_semantics_on_replay() {
        let mut receipt = WithdrawalReceiptDto {
            id: None,
            user_id: Uuid::nil(),
            dest: dest_a(),
            amount_micro: 5_000_000,
            combo: None,
            hold_tx_id: None,
            replayed: true,
            refused: true,
            refuse_code: Some("banned".into()),
            refuse_message: Some("account is banned".into()),
        };
        assert_eq!(
            withdrawal_status(&receipt),
            axum::http::StatusCode::FORBIDDEN
        );
        receipt.refuse_code = Some("geo_unavailable".into());
        assert_eq!(
            withdrawal_status(&receipt),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        receipt.refused = false;
        receipt.refuse_code = None;
        assert_eq!(withdrawal_status(&receipt), axum::http::StatusCode::OK);
    }

    #[test]
    fn every_refusal_code_has_a_stable_http_status() {
        let mut receipt = WithdrawalReceiptDto {
            id: None,
            user_id: Uuid::nil(),
            dest: dest_a(),
            amount_micro: 5_000_000,
            combo: None,
            hold_tx_id: None,
            replayed: false,
            refused: true,
            refuse_code: None,
            refuse_message: None,
        };
        for (code, expected) in [
            ("kyc", StatusCode::FORBIDDEN),
            ("geo_missing_ip", StatusCode::FORBIDDEN),
            ("paused", StatusCode::LOCKED),
            ("sanctions_unavailable", StatusCode::SERVICE_UNAVAILABLE),
            ("insufficient_funds", StatusCode::PAYMENT_REQUIRED),
            ("limit", StatusCode::TOO_MANY_REQUESTS),
            ("amount", StatusCode::UNPROCESSABLE_ENTITY),
        ] {
            receipt.refuse_code = Some(code.into());
            assert_eq!(withdrawal_status(&receipt), expected);
        }
    }

    #[tokio::test]
    async fn decide_entry_and_invalid_destination_edges_are_typed() {
        let store = FakeWithdrawStore::new();
        let user = store.seed_user(UserStatus::Active, 2);
        store.credit(user, 20_000_000);
        let now = store.now();
        let screen = FakeScreen {
            verdict: ScreenVerdict::Clear {
                checked_at: now,
                expires_at: now + time::Duration::hours(1),
                policy_version: "1".into(),
            },
            fail: false,
        };
        let invalid = execute_request(
            &store,
            &RequestClock(now),
            &screen,
            &screen,
            WithdrawRequestDto {
                user_id: user.0,
                amount_micro: 5_000_000,
                dest: "not-a-pubkey".into(),
                confirm_dest: "not-a-pubkey".into(),
                idempotency_key: None,
            },
            Some(IpAddr::from([203, 0, 113, 8])),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            invalid,
            application::error::AppError::ConfigInvalid { ref key, .. } if key == "dest"
        ));
        let invalid_confirm = execute_request(
            &store,
            &RequestClock(now),
            &screen,
            &screen,
            WithdrawRequestDto {
                user_id: user.0,
                amount_micro: 5_000_000,
                dest: dest_a(),
                confirm_dest: "not-a-pubkey".into(),
                idempotency_key: None,
            },
            Some(IpAddr::from([203, 0, 113, 8])),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            invalid_confirm,
            application::error::AppError::ConfigInvalid { ref key, .. }
                if key == "confirm_dest"
        ));

        let receipt = execute_request(
            &store,
            &RequestClock(now),
            &screen,
            &screen,
            WithdrawRequestDto {
                user_id: user.0,
                amount_micro: 5_000_000,
                dest: dest_a(),
                confirm_dest: dest_a(),
                idempotency_key: Some("http-decide".into()),
            },
            Some(IpAddr::from([203, 0, 113, 8])),
        )
        .await
        .unwrap();
        let id = receipt.id.unwrap();
        assert_eq!(
            execute_decide(
                &store,
                &RequestClock(now),
                AdminContext::Machine,
                id,
                "machine",
                DecideRequestDto { reason: None },
            )
            .await
            .unwrap(),
            "W4"
        );
        assert_eq!(
            execute_decide(
                &store,
                &RequestClock(now),
                AdminContext::Admin {
                    token_digest: "fin-http".into(),
                    role: application::model::AdminRole::Finance,
                },
                id,
                "approve",
                DecideRequestDto {
                    reason: Some("reviewed".into()),
                },
            )
            .await
            .unwrap(),
            "W2"
        );
        assert_eq!(
            execute_decide(
                &store,
                &RequestClock(now),
                AdminContext::Machine,
                id,
                "unknown",
                DecideRequestDto { reason: None },
            )
            .await
            .unwrap_err(),
            application::error::AppError::IllegalTransition
        );
    }

    #[test]
    fn handler_clock_is_live() {
        assert!(HandlerClock.now() > time::OffsetDateTime::UNIX_EPOCH);
    }

    async fn held_store() -> (FakeWithdrawStore, Uuid, time::OffsetDateTime, FakeScreen) {
        let store = FakeWithdrawStore::new();
        let user = store.seed_user(UserStatus::Active, 2);
        store.credit(user, 20_000_000);
        let now = store.now();
        let screen = FakeScreen {
            verdict: ScreenVerdict::Clear {
                checked_at: now,
                expires_at: now + time::Duration::hours(1),
                policy_version: "1".into(),
            },
            fail: false,
        };
        let receipt = execute_request(
            &store,
            &RequestClock(now),
            &screen,
            &screen,
            WithdrawRequestDto {
                user_id: user.0,
                amount_micro: 5_000_000,
                dest: dest_a(),
                confirm_dest: dest_a(),
                idempotency_key: Some(format!("handler-{}", user.0)),
            },
            Some(IpAddr::from([203, 0, 113, 8])),
        )
        .await
        .unwrap();
        (store, receipt.id.unwrap(), now, screen)
    }

    async fn review_state() -> (AppState<FakeWithdrawStore>, Uuid, Arc<FakeClock>) {
        let (store, id, now, _) = held_store().await;
        execute_decide(
            &store,
            &RequestClock(now),
            AdminContext::Machine,
            id,
            "machine",
            DecideRequestDto { reason: None },
        )
        .await
        .unwrap();
        let clock = Arc::new(FakeClock::at(now));
        let state = AppState::new(store, clock.clone());
        (state, id, clock)
    }

    #[tokio::test]
    async fn public_and_admin_handler_surfaces_delegate_end_to_end() {
        let _ = router::<FakeWithdrawStore>();
        let _ = admin_router::<FakeWithdrawStore>();

        let store = FakeWithdrawStore::new();
        let user = store.seed_user(UserStatus::Active, 2);
        store.credit(user, 20_000_000);
        let now = store.now();
        let clock = Arc::new(FakeClock::at(now));
        let state = AppState::new(store, clock);
        let screen = FakeScreen {
            verdict: ScreenVerdict::Clear {
                checked_at: now,
                expires_at: now + time::Duration::hours(1),
                policy_version: "1".into(),
            },
            fail: false,
        };
        let services = WithdrawServices {
            geo: Arc::new(screen.clone()),
            sanctions: Arc::new(screen),
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-demo-token",
            HeaderValue::from_str(&state.inner.demo_token).unwrap(),
        );
        let (status, Json(created)) = request_withdraw(
            State(state.clone()),
            Extension(services),
            ConnectInfo("198.51.100.8:443".parse().unwrap()),
            headers.clone(),
            Json(WithdrawRequestDto {
                user_id: user.0,
                amount_micro: 5_000_000,
                dest: dest_a(),
                confirm_dest: dest_a(),
                idempotency_key: Some("public-handler".into()),
            }),
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::OK);
        let id = created.id.unwrap();
        let Json(read) = get_withdrawal(State(state.clone()), headers.clone(), Path(id))
            .await
            .unwrap();
        assert_eq!(read.id, Some(id));

        let projected_user = state.inner.store.seed_user(UserStatus::Active, 2);
        state
            .inner
            .store
            .override_withdrawal_user(WithdrawalId(id), projected_user);
        assert!(get_withdrawal(State(state), headers, Path(id))
            .await
            .is_err());

        let (approve_state, approve_id, _) = review_state().await;
        let Json(approved) = admin_approve(
            State(approve_state),
            Extension(AdminContext::Admin {
                token_digest: "finance-approve".into(),
                role: application::model::AdminRole::Finance,
            }),
            Path(approve_id),
            Json(DecideRequestDto {
                reason: Some("approved".into()),
            }),
        )
        .await
        .unwrap();
        assert_eq!(approved.combo, "W2");

        let (deny_store, deny_id, now, _) = held_store().await;
        let deny_state = AppState::new(deny_store, Arc::new(FakeClock::at(now)));
        let Json(denied) = admin_deny(
            State(deny_state),
            Extension(AdminContext::Admin {
                token_digest: "finance-deny".into(),
                role: application::model::AdminRole::Finance,
            }),
            Path(deny_id),
            Json(DecideRequestDto {
                reason: Some("denied".into()),
            }),
        )
        .await
        .unwrap();
        assert_eq!(denied.combo, "W11");

        let (proposal_state, proposal_id, proposal_clock) = review_state().await;
        let Json(proposed) = admin_propose(
            State(proposal_state.clone()),
            Extension(AdminContext::Admin {
                token_digest: "finance-propose".into(),
                role: application::model::AdminRole::Finance,
            }),
            Path(proposal_id),
            Json(DecideRequestDto {
                reason: Some("dual".into()),
            }),
        )
        .await
        .unwrap();
        assert_eq!(proposed.combo, "W5");
        proposal_clock.advance(time::Duration::minutes(16));
        let Json(confirmed) = admin_confirm(
            State(proposal_state),
            Extension(AdminContext::Admin {
                token_digest: "finance-confirm".into(),
                role: application::model::AdminRole::Finance,
            }),
            Path(proposal_id),
            Json(DecideRequestDto { reason: None }),
        )
        .await
        .unwrap();
        assert_eq!(confirmed.combo, "W2");

        let (expire_state, expire_id, expire_clock) = review_state().await;
        execute_decide(
            &expire_state.inner.store,
            &expire_state.inner.clock,
            AdminContext::Admin {
                token_digest: "finance-expire-propose".into(),
                role: application::model::AdminRole::Finance,
            },
            expire_id,
            "propose",
            DecideRequestDto {
                reason: Some("expire".into()),
            },
        )
        .await
        .unwrap();
        expire_clock.advance(time::Duration::minutes(31));
        assert_eq!(
            execute_decide(
                &expire_state.inner.store,
                &expire_state.inner.clock,
                AdminContext::Machine,
                expire_id,
                "expire",
                DecideRequestDto { reason: None },
            )
            .await
            .unwrap(),
            "W4"
        );
    }

    struct OpenFailStore {
        user: UserId,
    }

    #[async_trait::async_trait]
    impl WithdrawStore for OpenFailStore {
        async fn withdraw_tx(
            &self,
        ) -> Result<Box<dyn application::ports::WithdrawTx + '_>, application::error::StoreError>
        {
            Err(application::error::StoreError::Backend(
                "open withdrawal tx".into(),
            ))
        }

        async fn withdrawal_user(
            &self,
            _id: WithdrawalId,
        ) -> Result<UserId, application::error::StoreError> {
            Ok(self.user)
        }

        async fn lookup_fingerprint(
            &self,
            _fingerprint: &str,
        ) -> Result<Option<WithdrawalReceipt>, application::error::StoreError> {
            Err(application::error::StoreError::Backend(
                "lookup fingerprint".into(),
            ))
        }
    }

    #[tokio::test]
    async fn handler_error_surfaces_fail_closed() {
        let store = FakeWithdrawStore::new();
        let user = store.seed_user(UserStatus::Active, 2);
        store.credit(user, 20_000_000);
        let now = store.now();
        let state = AppState::new(store, Arc::new(FakeClock::at(now)));
        let screen = FakeScreen {
            verdict: ScreenVerdict::Clear {
                checked_at: now,
                expires_at: now + time::Duration::hours(1),
                policy_version: "1".into(),
            },
            fail: false,
        };
        let services = WithdrawServices {
            geo: Arc::new(screen.clone()),
            sanctions: Arc::new(screen.clone()),
        };
        assert!(request_withdraw(
            State(state.clone()),
            Extension(services.clone()),
            ConnectInfo("198.51.100.8:443".parse().unwrap()),
            HeaderMap::new(),
            Json(WithdrawRequestDto {
                user_id: user.0,
                amount_micro: 5_000_000,
                dest: dest_a(),
                confirm_dest: dest_a(),
                idempotency_key: Some("handler-errors".into()),
            }),
        )
        .await
        .is_err());

        let mut headers = HeaderMap::new();
        headers.insert(
            "x-demo-token",
            HeaderValue::from_str(&state.inner.demo_token).unwrap(),
        );
        let mut invalid = WithdrawRequestDto {
            user_id: user.0,
            amount_micro: 5_000_000,
            dest: dest_a(),
            confirm_dest: dest_a(),
            idempotency_key: Some("handler-errors".into()),
        };
        invalid.dest = "not-a-pubkey".into();
        assert!(request_withdraw(
            State(state.clone()),
            Extension(services),
            ConnectInfo("198.51.100.8:443".parse().unwrap()),
            headers.clone(),
            Json(invalid),
        )
        .await
        .is_err());
        assert!(
            get_withdrawal(State(state.clone()), HeaderMap::new(), Path(Uuid::new_v4()),)
                .await
                .is_err()
        );
        assert!(
            get_withdrawal(State(state.clone()), headers.clone(), Path(Uuid::new_v4()),)
                .await
                .is_err()
        );

        let projected = state.inner.store.seed_user(UserStatus::Active, 2);
        let missing = WithdrawalId(Uuid::new_v4());
        state
            .inner
            .store
            .override_withdrawal_user(missing, projected);
        assert!(
            get_withdrawal(State(state), headers.clone(), Path(missing.0))
                .await
                .is_err()
        );

        let fail_store = OpenFailStore { user };
        let fail_state = AppState::new(fail_store, Arc::new(FakeClock::at(now)));
        assert!(
            get_withdrawal(State(fail_state), headers, Path(Uuid::new_v4()),)
                .await
                .is_err()
        );

        let (lock_fail, lock_fail_id, lock_now, _) = held_store().await;
        lock_fail.fail_lock_user();
        let lock_state = AppState::new(lock_fail, Arc::new(FakeClock::at(lock_now)));
        let mut lock_headers = HeaderMap::new();
        lock_headers.insert(
            "x-demo-token",
            HeaderValue::from_str(&lock_state.inner.demo_token).unwrap(),
        );
        assert!(
            get_withdrawal(State(lock_state), lock_headers, Path(lock_fail_id),)
                .await
                .is_err()
        );

        let (commit_fail, commit_fail_id, commit_now, _) = held_store().await;
        commit_fail.fail_commit();
        let commit_state = AppState::new(commit_fail, Arc::new(FakeClock::at(commit_now)));
        let mut commit_headers = HeaderMap::new();
        commit_headers.insert(
            "x-demo-token",
            HeaderValue::from_str(&commit_state.inner.demo_token).unwrap(),
        );
        assert!(
            get_withdrawal(State(commit_state), commit_headers, Path(commit_fail_id),)
                .await
                .is_err()
        );

        assert!(execute_request(
            &OpenFailStore { user },
            &RequestClock(now),
            &screen,
            &screen,
            WithdrawRequestDto {
                user_id: user.0,
                amount_micro: 5_000_000,
                dest: dest_a(),
                confirm_dest: dest_a(),
                idempotency_key: Some("fail-store".into()),
            },
            Some(IpAddr::from([203, 0, 113, 8])),
        )
        .await
        .is_err());

        let (held, id, now, _) = held_store().await;
        let held_state = AppState::new(held, Arc::new(FakeClock::at(now)));
        assert!(admin_approve(
            State(held_state),
            Extension(AdminContext::Admin {
                token_digest: "finance-error".into(),
                role: application::model::AdminRole::Finance,
            }),
            Path(id),
            Json(DecideRequestDto {
                reason: Some("not reviewed".into()),
            }),
        )
        .await
        .is_err());
    }
}
