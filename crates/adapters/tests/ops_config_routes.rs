//! Wave-W1 HTTP wiring tests for the D24 config plane: RBAC fail-closed,
//! DTO mapping, and the typed error surface (403/409/422) through the real
//! middleware + use cases over `InMemoryStore`. Dual-token proposal
//! semantics are proven at the use-case layer and in the swarm e2e; the
//! all-roles test token deterministically resolves to Curator (first
//! ordered granting role), which these tests lean on.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use adapters::http::VoteMetadataConfig;
use adapters::http::{router, AppState};
use application::fakes::{FakeClock, InMemoryStore};
use application::model::{IntegritySweepConfig, RepConfig, ResolveConfig, VoteIntegrityConfig};
use application::ports::{Clock, StaticConfigReads};
use axum::http::StatusCode;
use domain::money::MicroUsd;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use time::OffsetDateTime;
use tower::ServiceExt;

const DEMO: &str = "test-demo-token";
const ADMIN: &str = "test-admin-token";

fn t0() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

fn setup() -> (axum::Router, InMemoryStore) {
    let store = InMemoryStore::new();
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::at(t0()));
    let state = AppState::with_tokens(store.clone(), clock, DEMO, ADMIN);
    (router(state), store)
}

fn sha256_hex(token: &str) -> String {
    use sha2::Digest;
    use std::fmt::Write;
    let digest = sha2::Sha256::digest(token.as_bytes());
    digest.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn finance_app() -> axum::Router {
    let tokens = adapters::http::middleware::AdminTokens::parse(
        &serde_json::to_string(&[
            json!({"id":"finance-1","roles":["finance"],"sha256":sha256_hex("finance-1")}),
            json!({"id":"finance-2","roles":["finance"],"sha256":sha256_hex("finance-2")}),
        ])
        .unwrap(),
    )
    .unwrap();
    let state = AppState::with_phase6_ops(
        InMemoryStore::new(),
        Arc::new(FakeClock::at(t0())),
        ResolveConfig {
            oi_floor: MicroUsd(0),
        },
        RepConfig::default(),
        VoteIntegrityConfig::default(),
        IntegritySweepConfig::default(),
        VoteMetadataConfig::default(),
        application::model::SocialConfig::default(),
        Vec::new(),
        adapters::http::Phase5Services::default(),
        tokens,
        Arc::new(application::ports::NoopCrashPoint),
        Arc::new(StaticConfigReads::default()),
    );
    router(state)
}

async fn body_json(res: axum::response::Response) -> Value {
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!({"raw": String::from_utf8_lossy(&bytes)}))
}

#[allow(clippy::needless_pass_by_value)]
fn admin_request(method: &str, uri: &str, body: Value) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-admin-token", ADMIN)
        .body(axum::body::Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn get_config_returns_the_seeded_snapshot_and_is_401_without_a_token() {
    let (app, _store) = setup();
    let unauthenticated = axum::http::Request::builder()
        .method("GET")
        .uri("/admin/config")
        .body(axum::body::Body::empty())
        .unwrap();
    let res = app.clone().oneshot(unauthenticated).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    let res = app
        .oneshot(admin_request("GET", "/admin/config", json!({})))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert_eq!(body["generation"], 1, "the 0008 seed generation");
    let entries = body["entries"].as_array().unwrap();
    assert!(entries
        .iter()
        .any(|e| e["key"] == "trade_fee_bps" && e["value"] == 100));
}

#[tokio::test]
async fn a_curator_direct_key_applies_end_to_end_with_audit_and_change_rows() {
    let (app, store) = setup();
    let res = app
        .oneshot(admin_request(
            "POST",
            "/admin/config",
            json!({
                "patch": {"daily_slots": 3},
                "reason": "route wiring",
                "idempotency_key": "route-set-1"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert_eq!(body["generation"], 2);
    assert_eq!(body["changed_keys"], json!(["daily_slots"]));
    assert_eq!(store.config_value("daily_slots"), Some(json!(3)));
    let audits = store.ops_audits();
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].action, "set_config");
    assert_eq!(store.config_changes_of(2).len(), 1);
}

#[tokio::test]
async fn per_key_role_rules_beat_the_route_capability() {
    // The all-roles token resolves to Curator; sweep_delay_secs is an Ops
    // key, so the catalog rejects what the capability matrix admitted.
    let (app, store) = setup();
    let res = app
        .oneshot(admin_request(
            "POST",
            "/admin/config",
            json!({
                "patch": {"sweep_delay_secs": 300},
                "reason": "wrong role",
                "idempotency_key": "route-set-2"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(res).await["code"], "AdminForbidden");
    assert_eq!(store.config_generation(), 1, "nothing applied");
}

#[tokio::test]
async fn sensitive_keys_are_403_on_the_direct_path() {
    let (app, _store) = setup();
    let res = app
        .oneshot(admin_request(
            "POST",
            "/admin/config",
            json!({
                "patch": {"trade_fee_bps": 110},
                "reason": "should use proposals",
                "idempotency_key": "route-set-3"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(res).await["code"], "AdminForbidden");
}

#[tokio::test]
async fn bounds_violations_and_unknown_keys_are_typed_422s() {
    let (app, _store) = setup();
    let res = app
        .clone()
        .oneshot(admin_request(
            "POST",
            "/admin/config",
            json!({
                "patch": {"daily_slots": 9},
                "reason": "out of bounds",
                "idempotency_key": "route-set-4"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body_json(res).await["code"], "ConfigInvalid");

    let res = app
        .oneshot(admin_request(
            "POST",
            "/admin/config",
            json!({
                "patch": {"no_such_key": 1},
                "reason": "unknown",
                "idempotency_key": "route-set-5"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body_json(res).await["code"], "ConfigInvalid");
}

#[tokio::test]
async fn a_moved_expected_base_generation_is_a_409_stale_config() {
    let (app, _store) = setup();
    let res = app
        .oneshot(admin_request(
            "POST",
            "/admin/config",
            json!({
                "patch": {"daily_slots": 3},
                "expected_base_generation": 7,
                "reason": "optimistic",
                "idempotency_key": "route-set-6"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(body_json(res).await["code"], "StaleConfig");
}

#[tokio::test]
async fn curator_cannot_enter_the_proposal_flow_for_finance_keys() {
    let (app, _store) = setup();
    let res = app
        .oneshot(admin_request(
            "POST",
            "/admin/config/proposals",
            json!({
                "patch": {"trade_fee_bps": 110},
                "reason": "curator reaches too far",
                "idempotency_key": "route-prop-1"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(res).await["code"], "AdminForbidden");
}

#[tokio::test]
async fn settling_an_unknown_proposal_is_404() {
    let (app, _store) = setup();
    let id = uuid::Uuid::new_v4();
    let res = app
        .oneshot(admin_request(
            "POST",
            &format!("/admin/config/proposals/{id}/reject"),
            json!({"reason": "nothing there"}),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn finance_proposals_create_confirm_and_reject_through_the_real_routes() {
    let app = finance_app();
    let owned_error = adapters::http::error::ErrorResponse::new(
        StatusCode::BAD_REQUEST,
        "OwnedCode",
        "owned message",
    );
    assert_eq!(owned_error.body.code, "OwnedCode");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let ws_app = app.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, ws_app).await.unwrap();
    });
    tokio::task::spawn_blocking(move || {
        use tokio_tungstenite::tungstenite::{connect, Message};
        let (mut socket, _) = connect(format!("ws://{address}/ws")).unwrap();
        socket
            .send(Message::Text(
                json!({"op":"subscribe","market_id":uuid::Uuid::new_v4()})
                    .to_string()
                    .into(),
            ))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
    })
    .await
    .unwrap();
    server.abort();

    let create = |key: &str, fee: u16| {
        admin_request(
            "POST",
            "/admin/config/proposals",
            json!({
                "patch": {"trade_fee_bps": fee},
                "reason": "two person control",
                "idempotency_key": key,
            }),
        )
    };

    let mut first = create("proposal-confirm", 110);
    first
        .headers_mut()
        .insert("x-admin-token", "finance-1".parse().unwrap());
    let created = app.clone().oneshot(first).await.unwrap();
    assert_eq!(created.status(), StatusCode::OK);
    let id = body_json(created).await["id"].as_str().unwrap().to_string();
    let mut confirm = admin_request(
        "POST",
        &format!("/admin/config/proposals/{id}/confirm"),
        json!({"reason":"approved"}),
    );
    confirm
        .headers_mut()
        .insert("x-admin-token", "finance-2".parse().unwrap());
    let confirmed = app.clone().oneshot(confirm).await.unwrap();
    assert_eq!(confirmed.status(), StatusCode::OK);
    assert_eq!(body_json(confirmed).await["status"], "confirmed");

    let mut second = create("proposal-reject", 120);
    second
        .headers_mut()
        .insert("x-admin-token", "finance-1".parse().unwrap());
    let created = app.clone().oneshot(second).await.unwrap();
    assert_eq!(created.status(), StatusCode::OK);
    let id = body_json(created).await["id"].as_str().unwrap().to_string();
    let mut reject = admin_request(
        "POST",
        &format!("/admin/config/proposals/{id}/reject"),
        json!({}),
    );
    reject
        .headers_mut()
        .insert("x-admin-token", "finance-2".parse().unwrap());
    let rejected = app.oneshot(reject).await.unwrap();
    assert_eq!(rejected.status(), StatusCode::OK);
    assert_eq!(body_json(rejected).await["status"], "rejected");
}
