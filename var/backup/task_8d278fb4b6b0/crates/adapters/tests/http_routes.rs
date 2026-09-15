//! Hermetic HTTP tests against `AppState<InMemoryStore>` (tower oneshot).

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use adapters::http::{router, AppState, VoteMetadataConfig};
use adapters::relay::{BusEvent, UserNotifFrame, WireEvent};
use application::advance_market::{AdvanceMarket, AdvanceMarketCmd};
use application::ensure_genesis::{EnsureGenesis, EnsureGenesisCmd};
use application::fakes::{FakeClock, InMemoryStore};
use application::model::{
    IntegrityReportRow, IntegritySweepConfig, MarketId, NewNotification, Receivable,
    ReceivableMovement, ReceivableMovementKind, RepConfig, ResolveConfig, UserId,
    VoteIntegrityConfig,
};
use application::ports::{Clock, MarketQueries, OpsQueries, SettlementIo, Store};
use application::seed_market::{SeedMarket, SeedMarketCmd};
use axum::http::StatusCode;
use domain::amm::Side;
use domain::ledger::Currency;
use domain::market::{MarketEvent, MarketState};
use domain::money::{BasisPoints, MicroShares, MicroUsd};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use time::{Duration, OffsetDateTime};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::{connect, Message};
use tower::ServiceExt;
use uuid::Uuid;

const DEMO: &str = "test-demo-token";
const ADMIN: &str = "test-admin-token";
const RESERVES: i64 = 100_000_000;

fn t0() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

fn setup() -> (axum::Router, InMemoryStore, UserId, String) {
    let store = InMemoryStore::new();
    let market = store
        .add_market(
            "will-it-rain",
            MarketState::Live,
            t0() + Duration::hours(2),
            t0() + Duration::hours(1),
            MicroShares(RESERVES),
            BasisPoints(100),
        )
        .unwrap();
    let user = UserId(Uuid::new_v4());
    store.fund_user(user, MicroUsd(100_000_000)).unwrap();
    store.link_user_channel(
        "imessage",
        &format!("+1{}", &user.0.simple().to_string()[..10]),
        user,
    );
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::at(t0()));
    let state = AppState::with_tokens(store.clone(), clock, DEMO, ADMIN);
    (router(state), store, user, market.slug)
}

async fn body_json(res: axum::response::Response) -> Value {
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!({"raw": String::from_utf8_lossy(&bytes)}))
}

#[tokio::test]
async fn public_signup_rejects_blank_identity_fields_and_unarmed_overrides() {
    let (app, _, _, _) = setup();
    let blank = app
        .clone()
        .oneshot(request(
            "POST",
            "/users",
            None,
            json!({"handle":" ","channel":"imessage","address":"+15550001111"}),
        ))
        .await
        .unwrap();
    assert_eq!(blank.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body_json(blank).await["code"], "InvalidSignup");

    let negative = app
        .oneshot(request(
            "POST",
            "/users",
            None,
            json!({
                "handle":"negative-rep",
                "channel":"imessage",
                "address":"+15550002222",
                "rep_seed_micro":-1,
            }),
        ))
        .await
        .unwrap();
    assert_eq!(negative.status(), StatusCode::FORBIDDEN);
}

#[allow(clippy::needless_pass_by_value)]
fn request(
    method: &str,
    uri: impl AsRef<str>,
    token: Option<(&str, &str)>,
    body: Value,
) -> axum::http::Request<axum::body::Body> {
    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(uri.as_ref())
        .header("content-type", "application/json");
    if let Some((name, value)) = token {
        builder = builder.header(name, value);
    }
    builder
        .body(axum::body::Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_upgrade_with_in_memory_store_returns_a_snapshot() {
    let (app, store, _, slug) = setup();
    let market = store.market_by_ref(&slug).await.unwrap().id.0;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let snapshot = timeout(
        std::time::Duration::from_secs(5),
        tokio::task::spawn_blocking(move || {
            let (mut socket, _) = connect(format!("ws://{address}/ws").as_str()).unwrap();
            socket
                .send(Message::Text(
                    json!({"op":"subscribe", "market_id":market})
                        .to_string()
                        .into(),
                ))
                .unwrap();
            serde_json::from_str::<Value>(&socket.read().unwrap().into_text().unwrap()).unwrap()
        }),
    )
    .await
    .unwrap()
    .unwrap();
    server.abort();

    assert_eq!(snapshot["type"], "snapshot");
    assert_eq!(snapshot["market_id"], market.to_string());
    assert_eq!(snapshot["state"], "live");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_process_fault_configuration_closes_websockets_fail_closed() {
    let owned_error = adapters::http::error::ErrorResponse::new(
        StatusCode::BAD_REQUEST,
        "OwnedCode",
        "owned message",
    );
    assert_eq!(owned_error.body.code, "OwnedCode");
    if std::env::var("OPINIONS_INVALID_WS_FAULT_CHILD").as_deref() != Ok("1") {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "malformed_process_fault_configuration_closes_websockets_fail_closed",
                "--nocapture",
            ])
            .env("OPINIONS_INVALID_WS_FAULT_CHILD", "1")
            .status()
            .unwrap();
        assert!(result.success(), "invalid-fault child failed");
        return;
    }

    std::env::set_var("CHAOS_WS_DROP_EVERY_N", "0");
    std::env::remove_var("CHAOS_RELAY_DELAY_MS");
    let state = AppState::with_tokens(
        InMemoryStore::new(),
        Arc::new(FakeClock::at(t0())),
        DEMO,
        ADMIN,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    tokio::task::spawn_blocking(move || {
        let (mut socket, _) = connect(format!("ws://{address}/ws").as_str()).unwrap();
        if let tokio_tungstenite::tungstenite::stream::MaybeTlsStream::Plain(stream) =
            socket.get_mut()
        {
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(1)))
                .unwrap();
        }
        match socket.read() {
            Ok(Message::Close(_)) | Err(_) => {}
            other => panic!("faulted websocket remained open: {other:?}"),
        }
    })
    .await
    .unwrap();
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_user_subscription_auth_replacement_and_filtering_are_explicit() {
    let store = InMemoryStore::new();
    let first = store.add_user("ws_first", t0(), 1);
    let second = store.add_user("ws_second", t0(), 1);
    let market = store
        .add_market(
            "ws-user-market",
            MarketState::Live,
            t0() + Duration::hours(2),
            t0() + Duration::hours(1),
            MicroShares(RESERVES),
            BasisPoints(100),
        )
        .unwrap();
    let market_id = market.id.0;
    let state = AppState::with_tokens(store, Arc::new(FakeClock::at(t0())), DEMO, ADMIN);
    let events = state.event_sender();
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let reader = tokio::task::spawn_blocking(move || {
        let (mut socket, _) = connect(format!("ws://{address}/ws").as_str()).unwrap();
        for frame in [
            json!({"op":"subscribe_user", "user_id":first.0, "token":"wrong"}),
            json!({"op":"subscribe_user", "user_id":first.0, "token":DEMO}),
        ] {
            socket
                .send(Message::Text(frame.to_string().into()))
                .unwrap();
        }
        let first_snapshot =
            serde_json::from_str::<Value>(&socket.read().unwrap().into_text().unwrap()).unwrap();
        assert_eq!(first_snapshot["type"], "notif_snapshot");
        assert_eq!(first_snapshot["user_id"], first.0.to_string());
        socket
            .send(Message::Text(
                json!({"op":"subscribe_user", "user_id":second.0, "token":DEMO})
                    .to_string()
                    .into(),
            ))
            .unwrap();
        let replacement =
            serde_json::from_str::<Value>(&socket.read().unwrap().into_text().unwrap()).unwrap();
        assert_eq!(replacement["user_id"], second.0.to_string());
        socket
            .send(Message::Text(
                json!({"op":"subscribe", "market_id":market_id})
                    .to_string()
                    .into(),
            ))
            .unwrap();
        let market_snapshot =
            serde_json::from_str::<Value>(&socket.read().unwrap().into_text().unwrap()).unwrap();
        assert_eq!(market_snapshot["market_id"], market_id.to_string());
        ready_tx.send(()).unwrap();
        let market_frame =
            serde_json::from_str::<Value>(&socket.read().unwrap().into_text().unwrap()).unwrap();
        let notification =
            serde_json::from_str::<Value>(&socket.read().unwrap().into_text().unwrap()).unwrap();
        (market_frame, notification)
    });
    ready_rx.await.unwrap();
    events
        .send(BusEvent::Market(WireEvent {
            outbox_seq: 8,
            event_type: "MarketSeeded".to_string(),
            aggregate_type: "market".to_string(),
            aggregate_id: market_id,
            payload: json!({}),
        }))
        .unwrap();
    for user in [first, second] {
        events
            .send(BusEvent::UserNotif {
                user_id: user.0,
                frame: UserNotifFrame {
                    id: if user == first { 1 } else { 2 },
                    source_seq: 9,
                    notification_type: "mention".to_string(),
                    payload: json!({"recipient":user.0}),
                },
            })
            .unwrap();
    }
    let (market_frame, notification) = timeout(std::time::Duration::from_secs(5), reader)
        .await
        .unwrap()
        .unwrap();
    server.abort();
    assert_eq!(market_frame["type"], "price");
    assert_eq!(market_frame["outbox_seq"], 8);
    assert_eq!(notification["type"], "notif");
    assert_eq!(notification["id"], 2);
    assert_eq!(notification["source_seq"], 9);
    assert_eq!(notification["payload"]["recipient"], second.0.to_string());
}

fn app_from(store: InMemoryStore) -> axum::Router {
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::at(t0()));
    router(AppState::with_tokens(store, clock, DEMO, ADMIN))
}

async fn seeded_closed_market(store: &InMemoryStore, slug: &str, min_votes: i32) -> MarketId {
    EnsureGenesis { store }
        .execute(EnsureGenesisCmd {
            currency: Currency::Usdc,
            amount: MicroUsd(10_000_000),
        })
        .await
        .unwrap();
    let market = MarketId(Uuid::new_v4());
    let clock = FakeClock::at(t0());
    SeedMarket {
        store,
        clock: &clock,
        rep_config: RepConfig::default(),
        lp_kill_config: application::model::LpKillConfig::default(),
    }
    .execute(SeedMarketCmd {
        market_id: market,
        slug: slug.to_string(),
        min_votes_to_resolve: min_votes,
        closes_at: t0() + Duration::hours(2),
        tally_hidden_at: t0() + Duration::hours(1),
        fee: BasisPoints(0),
        seed: MicroUsd(1_000_000),
        idempotency_key: format!("seed-{market:?}"),
        force: false,
    })
    .await
    .unwrap();
    let advance = AdvanceMarket { store };
    for (index, event) in [
        MarketEvent::GoLive,
        MarketEvent::EnterCloseWindow,
        MarketEvent::Close,
    ]
    .into_iter()
    .enumerate()
    {
        advance
            .execute(AdvanceMarketCmd {
                market,
                event,
                idempotency_key: format!("close-{market:?}-{index}"),
            })
            .await
            .unwrap();
    }
    market
}

#[tokio::test]
async fn healthz_ok() {
    let (app, _, _, _) = setup();
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/healthz")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
}

#[tokio::test]
async fn preview_happy_path_matches_domain_quote() {
    let (app, _store, user, slug) = setup();
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/trades/preview")
                .header("content-type", "application/json")
                .header("x-demo-token", DEMO)
                .body(axum::body::Body::from(
                    json!({
                        "user_id": user.0,
                        "market_ref": slug,
                        "side": "yes",
                        "action": "buy",
                        "amount_micro": 5_000_000
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body = body_json(res).await;
    let pool = domain::amm::Pool::new(
        MicroShares(RESERVES),
        MicroShares(RESERVES),
        BasisPoints(100),
    )
    .unwrap();
    let q =
        domain::amm::quote_buy(&pool, Side::Yes, MicroUsd(5_000_000), BasisPoints(100)).unwrap();
    assert_eq!(body["shares_micro"], q.shares_out.0);
    assert_eq!(body["fee_micro"], q.fee.0);
    assert_eq!(body["gross_micro"], 5_000_000);
    assert_eq!(body["avg_price_micro"], q.avg_price_micro_per_share);
}

#[tokio::test]
async fn place_trade_without_expected_config_version_is_422() {
    let (app, store, user, slug) = setup();
    let market = store.list_markets(None).await.unwrap()[0].id;
    store.record_vote(user, market, Side::Yes);
    let before = store.snapshot();
    let response = app
        .oneshot(request(
            "POST",
            "/trades",
            Some(("x-demo-token", DEMO)),
            json!({
                "user_id": user.0,
                "market_ref": slug,
                "side": "yes",
                "action": "buy",
                "amount_micro": 5_000_000,
                "idempotency_key": "missing-config-version"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(before, store.snapshot());
}

#[tokio::test]
async fn place_trade_without_vote_returns_vote_required() {
    let (app, _store, user, slug) = setup();
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/trades")
                .header("content-type", "application/json")
                .header("x-demo-token", DEMO)
                .body(axum::body::Body::from(
                    json!({
                        "user_id": user.0,
                        "market_ref": slug,
                        "side": "yes",
                        "action": "buy",
                        "amount_micro": 5_000_000,
                        "idempotency_key": "k-no-vote",
                        "expected_config_version": 1
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(res.status().is_client_error());
    let body = body_json(res).await;
    assert_eq!(body["code"], "VoteRequired");
}

#[tokio::test]
async fn place_trade_replay_returns_replayed_true() {
    let (_app, store, user, slug) = setup();
    let markets = store.list_markets(None).await.unwrap();
    store.record_vote(user, markets[0].id, Side::Yes);

    let payload = json!({
        "user_id": user.0,
        "market_ref": slug,
        "side": "yes",
        "action": "buy",
        "amount_micro": 5_000_000,
        "idempotency_key": "k-replay-1",
        "expected_config_version": 1
    });

    let r1 = app_from(store.clone())
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/trades")
                .header("content-type", "application/json")
                .header("x-demo-token", DEMO)
                .body(axum::body::Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r1.status(), 200, "first: {:?}", body_json(r1).await);
    // re-issue: body already consumed above — redo first call cleanly
    let r1 = app_from(store.clone())
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/trades")
                .header("content-type", "application/json")
                .header("x-demo-token", DEMO)
                .body(axum::body::Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let b1 = body_json(r1).await;
    // After first successful trade, second identical key is a replay.
    // First call above may have already written — so b1 is replayed:true on second
    // construction. Do a clean first+second:

    let store2 = InMemoryStore::new();
    let market = store2
        .add_market(
            "will-it-rain",
            MarketState::Live,
            t0() + Duration::hours(2),
            t0() + Duration::hours(1),
            MicroShares(RESERVES),
            BasisPoints(100),
        )
        .unwrap();
    let user2 = UserId(Uuid::new_v4());
    store2.fund_user(user2, MicroUsd(100_000_000)).unwrap();
    store2.record_vote(user2, market.id, Side::Yes);
    let payload2 = json!({
        "user_id": user2.0,
        "market_ref": market.slug,
        "side": "yes",
        "action": "buy",
        "amount_micro": 5_000_000,
        "idempotency_key": "k-replay-clean",
        "expected_config_version": 1
    });
    let first = app_from(store2.clone())
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/trades")
                .header("content-type", "application/json")
                .header("x-demo-token", DEMO)
                .body(axum::body::Body::from(payload2.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    let b_first = body_json(first).await;
    assert_eq!(b_first["replayed"], false);

    let second = app_from(store2)
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/trades")
                .header("content-type", "application/json")
                .header("x-demo-token", DEMO)
                .body(axum::body::Body::from(payload2.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), 200);
    let b_second = body_json(second).await;
    assert_eq!(b_second["replayed"], true);
    assert_eq!(b_first["trade_id"], b_second["trade_id"]);
    let _ = b1; // silence unused from the exploratory first half
}

#[tokio::test]
async fn missing_demo_token_is_401() {
    let (app, _, user, slug) = setup();
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/trades/preview")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({
                        "user_id": user.0,
                        "market_ref": slug,
                        "side": "yes",
                        "action": "buy",
                        "amount_micro": 1_000_000
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    let body = body_json(res).await;
    assert_eq!(body["code"], "Unauthorized");
}

#[tokio::test]
async fn admin_without_token_is_401() {
    let (app, store, _, _) = setup();
    let market_id = store.list_markets(None).await.unwrap()[0].id.0;
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/admin/markets/{market_id}/advance"))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({"event": "close"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
}

#[tokio::test]
async fn openapi_doc_contains_every_route() {
    let doc = adapters::http::routes::openapi_json();
    let json = serde_json::to_value(&doc).unwrap();
    let paths = json["paths"].as_object().unwrap();
    for p in [
        "/healthz",
        "/markets",
        "/markets/{id_or_slug}",
        "/markets/{id}/chart",
        "/markets/{id}/tape",
        "/markets/{id_or_slug}/comments",
        "/markets/{id}/comments",
        "/comments/{id}/vote",
        "/comments/{id}/report",
        "/markets/{id}/holders",
        "/leaderboards/traders",
        "/leaderboards/voters",
        "/trades/preview",
        "/trades",
        "/votes",
        "/users",
        "/users/{id}/positions",
        "/users/{id}/profile",
        "/users/{id}/notifications",
        "/users/{id}/notifications/read",
        "/users/{id}/notifications/unread_count",
        "/users/by-channel",
        "/admin/markets/{id}/advance",
        "/admin/markets/{id}/resolve",
        "/admin/markets/flagged",
        "/admin/fees/summary",
        "/admin/comments/reported",
        "/admin/comments/{id}/moderate",
    ] {
        assert!(
            paths.contains_key(p),
            "openapi missing path {p}; have {:?}",
            paths.keys().collect::<Vec<_>>()
        );
    }
}

#[tokio::test]
async fn economy_routes_validate_windows_and_protect_fee_revenue() {
    let (app, _, _, _) = setup();
    for uri in ["/leaderboards/traders", "/leaderboards/voters"] {
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(uri)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(body_json(response).await, json!([]));
    }
    for uri in [
        "/leaderboards/traders?days=0",
        "/leaderboards/voters?limit=101",
    ] {
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(uri)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 422);
        assert_eq!(body_json(response).await["code"], "InvalidWindow");
    }

    let unauthorized = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/admin/fees/summary")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);
    let invalid = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/admin/fees/summary?days=0")
                .header("x-admin-token", ADMIN)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(invalid.status(), 422);
    let valid = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/admin/fees/summary")
                .header("x-admin-token", ADMIN)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(valid.status(), 200);
    assert_eq!(body_json(valid).await, json!([]));
}

#[tokio::test]
async fn user_by_channel_found() {
    let (app, store, user, _) = setup();
    store.link_user_channel("imessage", "+15551234567", user);
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/users/by-channel?channel=imessage&address=%2B15551234567")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body = body_json(res).await;
    assert_eq!(body["user_id"], user.0.to_string());
}

#[tokio::test]
async fn public_signup_is_idempotent_by_channel_key() {
    let store = InMemoryStore::new();
    let payload = json!({
        "handle": "new-voter",
        "channel": "imessage",
        "address": "+15551230000"
    });
    let first = app_from(store.clone())
        .oneshot(request("POST", "/users", None, payload.clone()))
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    let first_id = body_json(first).await["user_id"].clone();

    let replay = app_from(store.clone())
        .oneshot(request("POST", "/users", None, payload))
        .await
        .unwrap();
    assert_eq!(replay.status(), 200);
    assert_eq!(body_json(replay).await["user_id"], first_id);
    assert_eq!(store.outbox().len(), 1);
}

#[tokio::test]
async fn market_list_detail_and_positions_routes_return_views() {
    let (_app, store, user, slug) = setup();
    let market = store.list_markets(None).await.unwrap()[0].clone();
    store.record_vote(user, market.id, Side::Yes);
    let trade = json!({
        "user_id": user.0,
        "market_ref": slug,
        "side": "yes",
        "action": "buy",
        "amount_micro": 2_000_000,
        "expected_config_version": 1,
        "idempotency_key": format!("route-{}", Uuid::new_v4())
    });
    let traded = app_from(store.clone())
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/trades")
                .header("content-type", "application/json")
                .header("x-demo-token", DEMO)
                .body(axum::body::Body::from(trade.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(traded.status(), 200);

    let list = app_from(store.clone())
        .oneshot(
            axum::http::Request::builder()
                .uri("/markets?status=live")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list.status(), 200);
    let list_body = body_json(list).await;
    assert_eq!(list_body.as_array().unwrap().len(), 1);
    assert_eq!(list_body[0]["id"], market.id.0.to_string());
    assert_eq!(list_body[0]["question"], "will-it-rain");
    assert_eq!(list_body[0]["state"], "live");
    assert!(list_body[0]["closes_at"].is_string());
    assert!(list_body[0]["tally_hidden_at"].is_string());

    let detail = app_from(store.clone())
        .oneshot(
            axum::http::Request::builder()
                .uri(format!("/markets/{}", market.id.0))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(detail.status(), 200);
    assert_eq!(body_json(detail).await["slug"], "will-it-rain");

    let positions = app_from(store)
        .oneshot(
            axum::http::Request::builder()
                .uri(format!("/users/{}/positions", user.0))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(positions.status(), 200);
    assert_eq!(body_json(positions).await.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn missing_reads_return_not_found_envelopes() {
    let (app, _, _, _) = setup();
    let missing_market = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/markets/no-such-market")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_market.status(), 404);
    assert_eq!(body_json(missing_market).await["code"], "NotFound");

    let missing_user = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/users/by-channel?channel=imessage&address=%2B10000000000")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_user.status(), 404);
    assert_eq!(body_json(missing_user).await["code"], "NotFound");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn comment_routes_enforce_identity_replay_shadow_visibility_and_vote_uniqueness() {
    let (_app, store, author, slug) = setup();
    let market = store.market_by_ref(&slug).await.unwrap().id.0;
    let payload = json!({
        "user_id": author.0,
        "body": "root comment",
        "parent_id": null,
        "idempotency_key": "root-1"
    });
    let unauthorized = app_from(store.clone())
        .oneshot(request(
            "POST",
            format!("/markets/{market}/comments"),
            None,
            payload.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);
    let first = app_from(store.clone())
        .oneshot(request(
            "POST",
            format!("/markets/{market}/comments"),
            Some(("x-demo-token", DEMO)),
            payload.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(first.status(), 201);
    let root = body_json(first).await;
    assert_eq!(root["moderation_status"], "visible");
    let replay = app_from(store.clone())
        .oneshot(request(
            "POST",
            format!("/markets/{market}/comments"),
            Some(("x-demo-token", DEMO)),
            payload,
        ))
        .await
        .unwrap();
    assert_eq!(replay.status(), 200);
    assert_eq!(body_json(replay).await["id"], root["id"]);

    let shadow = app_from(store.clone())
        .oneshot(request(
            "POST",
            format!("/markets/{market}/comments"),
            Some(("x-demo-token", DEMO)),
            json!({
                "user_id": author.0,
                "body": "http://a http://b http://c",
                "parent_id": null,
                "idempotency_key": "shadow-1"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(shadow.status(), 201);
    assert_eq!(body_json(shadow).await["moderation_status"], "shadow");

    let public = app_from(store.clone())
        .oneshot(request(
            "GET",
            format!("/markets/{slug}/comments?sort=recent"),
            None,
            json!(null),
        ))
        .await
        .unwrap();
    assert_eq!(
        body_json(public).await["comments"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let viewer_without_token = app_from(store.clone())
        .oneshot(request(
            "GET",
            format!("/markets/{slug}/comments?viewer_id={}", author.0),
            None,
            json!(null),
        ))
        .await
        .unwrap();
    assert_eq!(viewer_without_token.status(), 401);
    let viewer = app_from(store.clone())
        .oneshot(request(
            "GET",
            format!(
                "/markets/{slug}/comments?sort=recent&viewer_id={}",
                author.0
            ),
            Some(("x-demo-token", DEMO)),
            json!(null),
        ))
        .await
        .unwrap();
    assert_eq!(
        body_json(viewer).await["comments"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let comment = root["id"].as_str().unwrap();
    let vote = json!({"user_id": author.0, "value": 1, "idempotency_key": "vote-1"});
    let first_vote = app_from(store.clone())
        .oneshot(request(
            "POST",
            format!("/comments/{comment}/vote"),
            Some(("x-demo-token", DEMO)),
            vote.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(body_json(first_vote).await["score"], 1);
    let duplicate = app_from(store)
        .oneshot(request(
            "POST",
            format!("/comments/{comment}/vote"),
            Some(("x-demo-token", DEMO)),
            vote,
        ))
        .await
        .unwrap();
    assert_eq!(duplicate.status(), 409);
    assert_eq!(body_json(duplicate).await["code"], "duplicate_vote");
}

#[tokio::test]
async fn comment_route_validation_and_frozen_hot_cursor_are_stable() {
    let (_app, store, author, slug) = setup();
    let market = store.market_by_ref(&slug).await.unwrap().id.0;
    let invalid_market = app_from(store.clone())
        .oneshot(request(
            "POST",
            "/markets/not-a-uuid/comments",
            Some(("x-demo-token", DEMO)),
            json!({"user_id":author.0,"body":"body","idempotency_key":"bad-market"}),
        ))
        .await
        .unwrap();
    assert_eq!(
        (
            invalid_market.status(),
            body_json(invalid_market).await["code"].clone()
        ),
        (StatusCode::NOT_FOUND, json!("NotFound"))
    );
    for (body, key) in [("first", "hot-1"), ("second", "hot-2")] {
        let response = app_from(store.clone())
            .oneshot(request(
                "POST",
                format!("/markets/{market}/comments"),
                Some(("x-demo-token", DEMO)),
                json!({"user_id":author.0,"body":body,"idempotency_key":key}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), 201);
    }
    let first = app_from(store.clone())
        .oneshot(request(
            "GET",
            format!("/markets/{slug}/comments?sort=hot&limit=1"),
            None,
            json!(null),
        ))
        .await
        .unwrap();
    let first = body_json(first).await;
    assert_eq!(first["comments"].as_array().unwrap().len(), 1);
    let cursor = first["next_cursor"].as_str().unwrap();
    let second = app_from(store.clone())
        .oneshot(request(
            "GET",
            format!("/markets/{slug}/comments?sort=hot&limit=1&cursor={cursor}"),
            None,
            json!(null),
        ))
        .await
        .unwrap();
    assert_eq!(
        body_json(second).await["comments"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    for uri in [
        format!("/markets/{slug}/comments?sort=wrong"),
        format!("/markets/{slug}/comments?limit=0"),
        format!("/markets/{slug}/comments?cursor=%%%"),
    ] {
        let response = app_from(store.clone())
            .oneshot(request("GET", uri, None, json!(null)))
            .await
            .unwrap();
        assert_eq!(response.status(), 422);
    }
    let empty_key = app_from(store)
        .oneshot(request(
            "POST",
            format!("/comments/{}/vote", Uuid::new_v4()),
            Some(("x-demo-token", DEMO)),
            json!({"user_id":author.0,"value":1,"idempotency_key":""}),
        ))
        .await
        .unwrap();
    assert_eq!(
        (
            empty_key.status(),
            body_json(empty_key).await["code"].clone()
        ),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            json!("InvalidIdempotencyKey")
        )
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn report_admin_restore_epoch_and_notification_scope_are_http_visible() {
    let (_app, store, author, slug) = setup();
    let market = store.market_by_ref(&slug).await.unwrap().id.0;
    let posted = app_from(store.clone())
        .oneshot(request(
            "POST",
            format!("/markets/{market}/comments"),
            Some(("x-demo-token", DEMO)),
            json!({"user_id": author.0, "body":"report me", "idempotency_key":"reported"}),
        ))
        .await
        .unwrap();
    let comment = body_json(posted).await["id"].as_str().unwrap().to_string();
    for index in 0..3 {
        let reporter = store.add_user(&format!("reporter_{index}"), t0() - Duration::days(10), 1);
        let response = app_from(store.clone())
            .oneshot(request(
                "POST",
                format!("/comments/{comment}/report"),
                Some(("x-demo-token", DEMO)),
                json!({"user_id":reporter.0}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(body_json(response).await["report_count"], index + 1);
    }
    let reported = app_from(store.clone())
        .oneshot(request(
            "GET",
            "/admin/comments/reported",
            Some(("x-admin-token", ADMIN)),
            json!(null),
        ))
        .await
        .unwrap();
    let reported = body_json(reported).await;
    assert_eq!(reported[0]["report_count"], 3);
    assert_eq!(reported[0]["reporters"].as_array().unwrap().len(), 3);
    let restored = app_from(store.clone())
        .oneshot(request(
            "POST",
            format!("/admin/comments/{comment}/moderate"),
            Some(("x-admin-token", ADMIN)),
            json!({"status":"visible"}),
        ))
        .await
        .unwrap();
    assert_eq!(body_json(restored).await["status"], "visible");
    for status in ["shadow", "blocked"] {
        let moderated = app_from(store.clone())
            .oneshot(request(
                "POST",
                format!("/admin/comments/{comment}/moderate"),
                Some(("x-admin-token", ADMIN)),
                json!({"status":status}),
            ))
            .await
            .unwrap();
        assert_eq!(body_json(moderated).await["status"], status);
    }
    let invalid = app_from(store.clone())
        .oneshot(request(
            "POST",
            format!("/admin/comments/{comment}/moderate"),
            Some(("x-admin-token", ADMIN)),
            json!({"status":"deleted"}),
        ))
        .await
        .unwrap();
    assert_eq!(
        (invalid.status(), body_json(invalid).await["code"].clone()),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            json!("InvalidModerationStatus")
        )
    );
    let cleared = app_from(store.clone())
        .oneshot(request(
            "GET",
            "/admin/comments/reported",
            Some(("x-admin-token", ADMIN)),
            json!(null),
        ))
        .await
        .unwrap();
    assert!(body_json(cleared).await.as_array().unwrap().is_empty());

    let other = store.add_user("notification_other", t0(), 1);
    let mut tx = store.notification_tx().await.unwrap();
    tx.insert_notifications(&[NewNotification {
        user: author,
        notification_type: "mention".to_string(),
        market: Some(MarketId(market)),
        payload: json!({"comment_id":comment}),
        source_seq: 77,
        created_at: t0(),
    }])
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let list = app_from(store.clone())
        .oneshot(request(
            "GET",
            format!("/users/{}/notifications", author.0),
            Some(("x-demo-token", DEMO)),
            json!(null),
        ))
        .await
        .unwrap();
    let page = body_json(list).await;
    let id = page["notifications"][0]["id"].as_i64().unwrap();
    assert_eq!(page["unread_count"], 1);
    let cross = app_from(store.clone())
        .oneshot(request(
            "POST",
            format!("/users/{}/notifications/read", other.0),
            Some(("x-demo-token", DEMO)),
            json!({"ids":[id]}),
        ))
        .await
        .unwrap();
    assert_eq!(body_json(cross).await["updated"], 0);
    let own = app_from(store.clone())
        .oneshot(request(
            "POST",
            format!("/users/{}/notifications/read", author.0),
            Some(("x-demo-token", DEMO)),
            json!({"ids":[id]}),
        ))
        .await
        .unwrap();
    assert_eq!(body_json(own).await["updated"], 1);
    let unread = app_from(store)
        .oneshot(request(
            "GET",
            format!("/users/{}/notifications/unread_count", author.0),
            Some(("x-demo-token", DEMO)),
            json!(null),
        ))
        .await
        .unwrap();
    assert_eq!(body_json(unread).await["unread_count"], 0);
}

#[tokio::test]
async fn holders_and_profile_expose_committed_capital_and_hide_live_vote_side() {
    let (_app, store, user, slug) = setup();
    let market = store.market_by_ref(&slug).await.unwrap();
    let vote = app_from(store.clone())
        .oneshot(request(
            "POST",
            "/votes",
            Some(("x-demo-token", DEMO)),
            json!({
                "user_id":user.0, "market_ref":slug, "side":"yes",
                "crowd_guess_pct":55, "idempotency_key":"profile-vote"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(vote.status(), 200);
    let trade = app_from(store.clone())
        .oneshot(request(
            "POST",
            "/trades",
            Some(("x-demo-token", DEMO)),
            json!({
                "user_id":user.0, "market_ref":slug, "side":"yes", "action":"buy",
                "amount_micro":2_000_000, "expected_config_version":1,
                "idempotency_key":"profile-trade"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(trade.status(), 200);
    let holders = app_from(store.clone())
        .oneshot(request(
            "GET",
            format!("/markets/{}/holders", market.id.0),
            None,
            json!(null),
        ))
        .await
        .unwrap();
    let holders = body_json(holders).await;
    assert_eq!(holders["yes"][0]["user_id"], user.0.to_string());
    assert!(holders["yes"][0]["cost_micro"].as_i64().unwrap() > 0);
    let profile = app_from(store)
        .oneshot(request(
            "GET",
            format!("/users/{}/profile", user.0),
            None,
            json!(null),
        ))
        .await
        .unwrap();
    let profile = body_json(profile).await;
    assert_eq!(profile["recent_trades"].as_array().unwrap().len(), 1);
    assert!(profile["recent_votes"][0]["side"].is_null());
    assert!(profile["recent_votes"][0]["score_bp"].is_null());
    assert!(profile["recent_votes"][0]["cast_at"].is_string());
}

#[tokio::test]
async fn task_1_2_routes_are_wired_to_use_cases() {
    let (app, store, user, slug) = setup();
    let market = store.list_markets(None).await.unwrap()[0].id.0;
    let vote = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/votes")
                .header("content-type", "application/json")
                .header("x-demo-token", DEMO)
                .body(axum::body::Body::from(
                    json!({
                        "user_id": user.0,
                        "market_ref": slug,
                        "side": "no",
                        "crowd_guess_pct": 45,
                        "idempotency_key": "vote-http"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(vote.status(), 200);
    assert_eq!(body_json(vote).await["crowd_guess_pct"], 45);

    // Live → Close is illegal; proves AdvanceMarket (not 501) is on the wire.
    let advance = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/admin/markets/{market}/advance"))
                .header("content-type", "application/json")
                .header("x-admin-token", ADMIN)
                .body(axum::body::Body::from(
                    json!({"event": "close"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(advance.status(), 409);
    assert_eq!(body_json(advance).await["code"], "IllegalTransition");

    let resolve = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/admin/markets/{market}/resolve"))
                .header("x-admin-token", ADMIN)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resolve.status(), 409);
    assert_eq!(body_json(resolve).await["code"], "IllegalTransition");
}

#[tokio::test]
async fn vote_route_captures_trusted_peer_and_hmac_never_raw_device_id() {
    let store = InMemoryStore::new();
    let market = store
        .add_market(
            "metadata-vote",
            MarketState::Live,
            t0() + Duration::hours(2),
            t0() + Duration::hours(1),
            MicroShares(RESERVES),
            BasisPoints(100),
        )
        .unwrap();
    let user = UserId(Uuid::new_v4());
    store.link_user_channel("imessage", "+15550001111", user);
    let state = AppState::with_phase3_tokens(
        store.clone(),
        Arc::new(FakeClock::at(t0())),
        DEMO,
        ADMIN,
        ResolveConfig {
            oi_floor: MicroUsd(0),
        },
        RepConfig::default(),
        VoteIntegrityConfig::default(),
        IntegritySweepConfig::default(),
        VoteMetadataConfig {
            trusted_proxy_cidrs: vec!["10.0.0.0/8".parse().unwrap()],
            device_hash_secret: Some(b"secret".to_vec()),
        },
    );
    let response = router(state)
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/votes")
                .header("content-type", "application/json")
                .header("x-demo-token", DEMO)
                .header("x-forwarded-for", "198.51.100.8, 10.1.2.3")
                .header("x-device-id", "client-123")
                .extension(axum::extract::ConnectInfo(
                    "10.9.8.7:4321".parse::<std::net::SocketAddr>().unwrap(),
                ))
                .body(axum::body::Body::from(
                    json!({
                        "user_id": user.0,
                        "market_ref": market.slug,
                        "side": "yes",
                        "crowd_guess_pct": 50,
                        "idempotency_key": "metadata-vote-key"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{:?}", body_json(response).await);
    let (ip, device_hash) = store.vote_metadata_for_user(user, market.id).unwrap();
    assert_eq!(ip, Some("198.51.100.8".parse().unwrap()));
    assert_eq!(
        device_hash.as_deref(),
        Some("6b975ee9ae4fa08c109702cf04e462ca")
    );
    assert_ne!(device_hash.as_deref(), Some("client-123"));
}

#[tokio::test]
async fn curator_inbox_is_admin_only_and_public_surfaces_expose_only_review_state() {
    let store = InMemoryStore::new();
    let market = seeded_closed_market(&store, "inbox-market", 2).await;
    let mut tx = store.resolve_tx().await.unwrap();
    tx.serialize_key("inbox-flag").await.unwrap();
    tx.market_for_update(market).await.unwrap();
    SettlementIo::set_market_state(tx.as_mut(), market, MarketState::Resolving)
        .await
        .unwrap();
    assert!(tx.flag_curator_needed(market).await.unwrap());
    tx.commit().await.unwrap();
    let mut report_tx = store.integrity_tx().await.unwrap();
    report_tx.serialize_key("inbox-report").await.unwrap();
    assert!(report_tx
        .insert_integrity_report(&IntegrityReportRow {
            market,
            checks: json!([{"name":"vote_burst","flagged":true}]),
            verdict: domain::integrity::Verdict::Flag,
            created_at: t0(),
        })
        .await
        .unwrap());
    report_tx.commit().await.unwrap();
    let pass_market = seeded_closed_market(&store, "inbox-pass-market", 2).await;
    let mut pass_state = store.resolve_tx().await.unwrap();
    pass_state.serialize_key("inbox-pass-state").await.unwrap();
    pass_state.market_for_update(pass_market).await.unwrap();
    SettlementIo::set_market_state(pass_state.as_mut(), pass_market, MarketState::Resolving)
        .await
        .unwrap();
    pass_state.commit().await.unwrap();
    let mut pass_report = store.integrity_tx().await.unwrap();
    pass_report
        .serialize_key("inbox-pass-report")
        .await
        .unwrap();
    assert!(pass_report
        .insert_integrity_report(&IntegrityReportRow {
            market: pass_market,
            checks: json!([{"name":"vote_burst","flagged":false}]),
            verdict: domain::integrity::Verdict::Pass,
            created_at: t0(),
        })
        .await
        .unwrap());
    pass_report.commit().await.unwrap();

    let unauthorized = app_from(store.clone())
        .oneshot(
            axum::http::Request::builder()
                .uri("/admin/markets/flagged")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);
    let inbox = app_from(store.clone())
        .oneshot(
            axum::http::Request::builder()
                .uri("/admin/markets/flagged")
                .header("x-admin-token", ADMIN)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(inbox.status(), 200);
    let inbox = body_json(inbox).await;
    let rows = inbox.as_array().unwrap();
    let flagged = rows
        .iter()
        .find(|row| row["market_id"] == market.0.to_string())
        .unwrap();
    assert_eq!(flagged["report"]["verdict"], "flag");
    assert!(flagged["report"]["checks"].is_array());
    let passed = rows
        .iter()
        .find(|row| row["market_id"] == pass_market.0.to_string())
        .unwrap();
    assert_eq!(passed["report"]["verdict"], "pass");

    let public = app_from(store)
        .oneshot(
            axum::http::Request::builder()
                .uri(format!("/markets/{}", market.0))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let public = body_json(public).await;
    assert_eq!(public["under_review"], true);
    assert!(public.get("report").is_none());
    assert!(public.get("checks").is_none());
}

#[test]
fn app_state_default_constructor_and_clock_are_usable() {
    let store = InMemoryStore::new();
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::at(t0()));
    // Cover the env-default arms of with_configs: demo falls back, admin
    // tokens fail CLOSED (no legacy ADMIN_TOKEN fallback — D26/6.0b), and
    // the faucet arm stays off outside staging.
    let saved_demo = std::env::var("DEMO_TOKEN").ok();
    let saved_admin = std::env::var("ADMIN_TOKENS_JSON").ok();
    std::env::remove_var("DEMO_TOKEN");
    std::env::remove_var("ADMIN_TOKENS_JSON");
    std::env::remove_var("OPINIONS_ENV");
    std::env::remove_var("STAGING_FAUCET");
    let state = AppState::new(store, Arc::clone(&clock));
    assert_eq!(state.inner.demo_token, "demo-token");
    assert!(state
        .inner
        .admin_tokens
        .authenticate("admin-token")
        .is_none());
    assert!(!state.inner.staging_faucet);
    assert_eq!(state.inner.clock.now(), clock.now());
    let cloned = state.clone();
    assert!(Arc::ptr_eq(&state.inner, &cloned.inner));
    let phase3 = AppState::with_phase3_configs(
        InMemoryStore::new(),
        Arc::clone(&clock),
        ResolveConfig {
            oi_floor: MicroUsd(7),
        },
        RepConfig::default(),
        VoteIntegrityConfig::default(),
        IntegritySweepConfig {
            payout_hold_threshold_micro: 99,
            ..IntegritySweepConfig::default()
        },
        VoteMetadataConfig::default(),
    );
    assert_eq!(phase3.inner.resolve_config.oi_floor, MicroUsd(7));
    assert_eq!(
        phase3
            .inner
            .integrity_sweep_config
            .payout_hold_threshold_micro,
        99
    );
    match saved_demo {
        Some(v) => std::env::set_var("DEMO_TOKEN", v),
        None => std::env::remove_var("DEMO_TOKEN"),
    }
    match saved_admin {
        Some(v) => std::env::set_var("ADMIN_TOKENS_JSON", v),
        None => std::env::remove_var("ADMIN_TOKENS_JSON"),
    }
}

#[tokio::test]
async fn vote_happy_path_and_already_voted() {
    let (_app, store, user, slug) = setup();
    let payload = json!({
        "user_id": user.0,
        "market_ref": slug,
        "side": "yes",
        "crowd_guess_pct": 65,
        "idempotency_key": "vote-k1"
    });
    let first = app_from(store.clone())
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/votes")
                .header("content-type", "application/json")
                .header("x-demo-token", DEMO)
                .body(axum::body::Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = first.status();
    let b1 = body_json(first).await;
    assert_eq!(status, 200, "{b1:?}");
    assert_eq!(b1["side"], "yes");
    assert_eq!(b1["crowd_guess_pct"], 65);
    assert_eq!(b1["replayed"], false);
    assert!(b1["seq"].as_i64().unwrap() >= 1);

    // Same user, different idempotency key → AlreadyVoted (one vote per user/market)
    let second = app_from(store.clone())
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/votes")
                .header("content-type", "application/json")
                .header("x-demo-token", DEMO)
                .body(axum::body::Body::from(
                    json!({
                        "user_id": user.0,
                        "market_ref": slug,
                        "side": "no",
                        "crowd_guess_pct": 40,
                        "idempotency_key": "vote-k2"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), 409);
    assert_eq!(body_json(second).await["code"], "AlreadyVoted");
}

#[tokio::test]
async fn chart_and_tape_routes_validate_and_return_trades() {
    let (_app, store, user, slug) = setup();
    let market = store.list_markets(None).await.unwrap()[0].clone();
    store.record_vote(user, market.id, Side::Yes);
    let trade = json!({
        "user_id": user.0,
        "market_ref": slug,
        "side": "yes",
        "action": "buy",
        "amount_micro": 2_000_000,
        "expected_config_version": 1,
        "idempotency_key": format!("chart-{}", Uuid::new_v4())
    });
    let response = app_from(store.clone())
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/trades")
                .header("content-type", "application/json")
                .header("x-demo-token", DEMO)
                .body(axum::body::Body::from(trade.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    let chart = app_from(store.clone())
        .oneshot(
            axum::http::Request::builder()
                .uri(format!(
                    "/markets/{}/chart?bucket=60&since=1970-01-01T00:00:00Z",
                    market.id.0
                ))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(chart.status(), 200);
    assert_eq!(body_json(chart).await.as_array().unwrap().len(), 1);

    let invalid = app_from(store.clone())
        .oneshot(
            axum::http::Request::builder()
                .uri(format!(
                    "/markets/{}/chart?bucket=0&since=1970-01-01T00:00:00Z",
                    market.id.0
                ))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(invalid.status(), 422);

    let invalid_since = app_from(store.clone())
        .oneshot(
            axum::http::Request::builder()
                .uri(format!(
                    "/markets/{}/chart?bucket=60&since=not-a-timestamp",
                    market.id.0
                ))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(invalid_since.status(), 422);
    assert_eq!(body_json(invalid_since).await["code"], "InvalidSince");

    let tape = app_from(store)
        .oneshot(
            axum::http::Request::builder()
                .uri(format!("/markets/{}/tape?limit=500", market.id.0))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(tape.status(), 200);
    let row = &body_json(tape).await[0];
    assert_eq!(row["side"], "yes");
    assert_eq!(row["action"], "buy");
    assert_eq!(row["trade_seq"], 1);
}

#[tokio::test]
async fn vote_missing_demo_token_is_401() {
    let (app, _, user, slug) = setup();
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/votes")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({
                        "user_id": user.0,
                        "market_ref": slug,
                        "side": "yes",
                        "crowd_guess_pct": 50,
                        "idempotency_key": "vote-no-token"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
}

#[tokio::test]
async fn advance_whitelist_rejects_financial_events() {
    let (app, store, _, _) = setup();
    let market_id = store.list_markets(None).await.unwrap()[0].id.0;
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/admin/markets/{market_id}/advance"))
                .header("content-type", "application/json")
                .header("x-admin-token", ADMIN)
                .body(axum::body::Body::from(
                    json!({"event": "resolve"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 422);
    let body = body_json(res).await;
    assert_eq!(body["code"], "UseResolveMarket");
}

#[tokio::test]
async fn resolve_on_live_market_is_illegal_transition() {
    let (app, store, _, _) = setup();
    // setup market is Live — resolve requires Closed/Resolving
    let market_id = store.list_markets(None).await.unwrap()[0].id.0;
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/admin/markets/{market_id}/resolve"))
                .header("x-admin-token", ADMIN)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 409);
    assert_eq!(body_json(res).await["code"], "IllegalTransition");
}

#[tokio::test]
async fn place_trade_maps_closing_to_423_and_resolved_to_409() {
    let (_app, store, user, slug) = setup();
    let market = store.list_markets(None).await.unwrap()[0].id;
    store.record_vote(user, market, Side::Yes);
    let request = |key: &str| {
        axum::http::Request::builder()
            .method("POST")
            .uri("/trades")
            .header("content-type", "application/json")
            .header("x-demo-token", DEMO)
            .body(axum::body::Body::from(
                json!({
                    "user_id": user.0,
                    "market_ref": slug,
                    "side": "yes",
                    "action": "buy",
                    "amount_micro": 1_000_000,
                    "expected_config_version": 1,
                    "idempotency_key": key,
                })
                .to_string(),
            ))
            .unwrap()
    };
    store.set_market_state(market, MarketState::Closing);
    let frozen = app_from(store.clone())
        .oneshot(request("http-closing"))
        .await
        .unwrap();
    assert_eq!(frozen.status(), 423);
    assert_eq!(body_json(frozen).await["code"], "TradingFrozen");

    store.set_market_state(market, MarketState::Resolved);
    let resolved = app_from(store)
        .oneshot(request("http-resolved"))
        .await
        .unwrap();
    assert_eq!(resolved.status(), 409);
    assert_eq!(body_json(resolved).await["code"], "MarketNotOpen");
}

#[tokio::test]
async fn curator_override_without_scheduler_flag_is_422() {
    let (app, store, _, _) = setup();
    let market = store.list_markets(None).await.unwrap()[0].id.0;
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/admin/markets/{market}/resolve"))
                .header("content-type", "application/json")
                .header("x-admin-token", ADMIN)
                .body(axum::body::Body::from(
                    json!({"decision":"void"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 422);
    assert_eq!(
        body_json(response).await["code"],
        "CuratorOverrideNotAllowed"
    );
}

#[tokio::test]
async fn admin_advance_success_returns_the_persisted_transition() {
    let (_app, store, _, _) = setup();
    let market = store.list_markets(None).await.unwrap()[0].id;
    let response = app_from(store.clone())
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/admin/markets/{}/advance", market.0))
                .header("content-type", "application/json")
                .header("x-admin-token", ADMIN)
                .body(axum::body::Body::from(
                    json!({"event":"enter_close_window"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    assert_eq!(body["market_id"], market.0.to_string());
    assert_eq!(body["from_state"], "live");
    assert_eq!(body["state"], "closing");
    assert_eq!(store.market_state(market), Some(MarketState::Closing));
}

#[tokio::test]
async fn admin_resolve_success_serializes_voided_paid_and_replay_receipts() {
    let void_store = InMemoryStore::new();
    let void_market = seeded_closed_market(&void_store, "http-void", 3).await;
    let mut flag_tx = void_store.resolve_tx().await.unwrap();
    SettlementIo::set_market_state(flag_tx.as_mut(), void_market, MarketState::Resolving)
        .await
        .unwrap();
    assert!(flag_tx.flag_curator_needed(void_market).await.unwrap());
    flag_tx.commit().await.unwrap();
    let first_void = app_from(void_store.clone())
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/admin/markets/{}/resolve", void_market.0))
                .header("content-type", "application/json")
                .header("x-admin-token", ADMIN)
                .body(axum::body::Body::from(
                    json!({"decision":"void"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first_void.status(), 200);
    let first_void = body_json(first_void).await;
    assert_eq!(first_void["state"], "voided");
    assert_eq!(first_void["voided"], true);
    assert_eq!(first_void["actual_yes_bps"], 5_000);
    assert_eq!(first_void["replayed"], false);
    assert!(first_void["ledger_txn"].is_string());

    let audit = void_store.audit_page(None, 10).await.unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].action.action, "resolve_market");
    assert!(!audit[0].action.actor_token_digest.is_empty());

    let replay = app_from(void_store)
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/admin/markets/{}/resolve", void_market.0))
                .header("x-admin-token", ADMIN)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(body_json(replay).await["replayed"], true);

    let paid_store = InMemoryStore::new();
    let paid_market = seeded_closed_market(&paid_store, "http-paid", 1).await;
    paid_store.record_vote(UserId(Uuid::new_v4()), paid_market, Side::Yes);
    let paid = app_from(paid_store.clone())
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/admin/markets/{}/resolve", paid_market.0))
                .header("x-admin-token", ADMIN)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(paid.status(), 200);
    let paid = body_json(paid).await;
    assert_eq!(paid["state"], "paid");
    assert_eq!(paid["voided"], false);
    assert_eq!(paid["actual_yes_bps"], 10_000);

    let override_without_flag = app_from(paid_store)
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/admin/markets/{}/resolve", Uuid::new_v4()))
                .header("content-type", "application/json")
                .header("x-admin-token", ADMIN)
                .body(axum::body::Body::from(
                    json!({"decision":"resolve_at_tally"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(override_without_flag.status().is_client_error());
}

// ---------------------------------------------------------------------------
// Phase 6 (Task 6.0b): fail-closed RBAC, ops stubs, faucet arm, config stamp
// ---------------------------------------------------------------------------

fn sha256_hex(token: &str) -> String {
    use sha2::Digest;
    use std::fmt::Write;
    let digest = sha2::Sha256::digest(token.as_bytes());
    digest.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

async fn ops_success_app() -> (
    axum::Router,
    InMemoryStore,
    Arc<FakeClock>,
    MarketId,
    MarketId,
    UserId,
    Uuid,
) {
    let store = InMemoryStore::new();
    EnsureGenesis { store: &store }
        .execute(EnsureGenesisCmd {
            currency: Currency::Usdc,
            amount: MicroUsd(1_000_000_000),
        })
        .await
        .unwrap();
    let user = store.add_user("ops-beneficiary", t0() - Duration::days(100), 2);
    store.fund_user(user, MicroUsd(1)).unwrap();
    let market = store
        .add_market(
            "ops-unwind-confirm",
            MarketState::Voided,
            t0(),
            t0(),
            MicroShares(RESERVES),
            BasisPoints(100),
        )
        .unwrap()
        .id;
    let reject_market = store
        .add_market(
            "ops-unwind-reject",
            MarketState::Voided,
            t0(),
            t0(),
            MicroShares(RESERVES),
            BasisPoints(100),
        )
        .unwrap()
        .id;
    let receivable = Uuid::new_v4();
    let mut tx = store.unwind_tx().await.unwrap();
    tx.insert_receivable(Receivable {
        id: receivable,
        market,
        user,
        origin_reversal_txn: Uuid::new_v4(),
        opened_micro: 10_000_000,
    })
    .await
    .unwrap();
    tx.insert_receivable_movement(ReceivableMovement {
        id: Uuid::new_v4(),
        receivable,
        kind: ReceivableMovementKind::Opened,
        amount_micro: 10_000_000,
        actor: "fixture".into(),
        cash_txn: None,
        idempotency_key: "ops-route-open".into(),
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let doc = serde_json::to_string(&[
        json!({"id":"superadmin-1","roles":["superadmin"],"sha256":sha256_hex("superadmin-1")}),
        json!({"id":"finance-1","roles":["finance"],"sha256":sha256_hex("finance-1")}),
        json!({"id":"finance-2","roles":["finance"],"sha256":sha256_hex("finance-2")}),
        json!({"id":"ops-1","roles":["ops"],"sha256":sha256_hex("ops-1")}),
    ])
    .unwrap();
    let tokens = adapters::http::middleware::AdminTokens::parse(&doc).unwrap();
    let clock = Arc::new(FakeClock::at(t0()));
    let state = AppState::with_phase6_ops(
        store.clone(),
        clock.clone(),
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
        Arc::new(application::ports::StaticConfigReads::default()),
    );
    (
        router(state),
        store,
        clock,
        market,
        reject_market,
        user,
        receivable,
    )
}

#[tokio::test]
async fn audit_route_validates_its_cursor_and_maps_nonempty_pages() {
    let (app, store, _, _) = setup();
    let invalid = app
        .clone()
        .oneshot(request(
            "GET",
            "/admin/audit?before=not-a-timestamp",
            Some(("x-admin-token", ADMIN)),
            json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body_json(invalid).await["code"], "InvalidCursor");

    application::create_user::CreateUser { store: &store }
        .execute_as(
            application::create_user::CreateUserCmd {
                handle: "audit-route-user".into(),
                channel: None,
                idempotency_key: "audit-route-user".into(),
                created_at_override: Some(t0() - Duration::days(80)),
                rep_seed_micro: Some(400_000),
            },
            &application::model::AdminContext::Admin {
                token_digest: "audit-fixture".into(),
                role: application::model::AdminRole::Superadmin,
            },
            RepConfig::default(),
        )
        .await
        .unwrap();
    let page = app
        .oneshot(request(
            "GET",
            "/admin/audit?limit=1",
            Some(("x-admin-token", ADMIN)),
            json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    let body = body_json(page).await;
    assert_eq!(body["actions"].as_array().unwrap().len(), 1);
    assert_eq!(body["actions"][0]["action"], "create_user_override");
    assert!(body["next_before"].is_string());
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn successful_dual_control_ops_routes_return_their_durable_rows() {
    let (app, _store, clock, market, reject_market, user, receivable) = ops_success_app().await;
    let propose = |market: MarketId, key: &str| {
        request(
            "POST",
            format!("/admin/markets/{}/unwind/propose", market.0),
            Some(("x-admin-token", "superadmin-1")),
            json!({"reason":"wrong fraud decision","idempotency_key":key}),
        )
    };
    let first = app
        .clone()
        .oneshot(propose(market, "unwind-ok"))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(body_json(first).await["stage"], "proposed");
    clock.advance(Duration::seconds(61));
    let confirmed = app
        .clone()
        .oneshot(request(
            "POST",
            format!("/admin/markets/{}/unwind/confirm", market.0),
            Some(("x-admin-token", "finance-1")),
            dual_control_body(),
        ))
        .await
        .unwrap();
    assert_eq!(confirmed.status(), StatusCode::OK);
    assert_eq!(body_json(confirmed).await["stage"], "applied");

    let proposed = app
        .clone()
        .oneshot(propose(reject_market, "unwind-reject"))
        .await
        .unwrap();
    assert_eq!(proposed.status(), StatusCode::OK);
    let rejected = app
        .clone()
        .oneshot(request(
            "POST",
            format!("/admin/markets/{}/unwind/reject", reject_market.0),
            Some(("x-admin-token", "finance-1")),
            dual_control_body(),
        ))
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::OK);
    assert_eq!(body_json(rejected).await["stage"], "rejected");

    let remedial = app
        .clone()
        .oneshot(request(
            "POST",
            format!("/admin/markets/{}/remedial_credit/propose", market.0),
            Some(("x-admin-token", "finance-1")),
            json!({
                "user_id":user.0,
                "amount_micro":1_000_000,
                "reason":"goodwill",
                "idempotency_key":"remedial-ok"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(remedial.status(), StatusCode::OK);
    assert_eq!(body_json(remedial).await["status"], "pending");
    clock.advance(Duration::seconds(61));
    let remedial_confirmed = app
        .clone()
        .oneshot(request(
            "POST",
            format!("/admin/markets/{}/remedial_credit/confirm", market.0),
            Some(("x-admin-token", "superadmin-1")),
            json!({"reason":"approved","idempotency_key":"remedial-ok"}),
        ))
        .await
        .unwrap();
    assert_eq!(remedial_confirmed.status(), StatusCode::OK);
    assert_eq!(body_json(remedial_confirmed).await["status"], "confirmed");

    let write_off = app
        .clone()
        .oneshot(request(
            "POST",
            format!("/admin/receivables/{receivable}/write_off/propose"),
            Some(("x-admin-token", "finance-1")),
            json!({"reason":"uncollectable","idempotency_key":"write-off-ok"}),
        ))
        .await
        .unwrap();
    assert_eq!(write_off.status(), StatusCode::OK);
    assert_eq!(body_json(write_off).await["status"], "pending");
    clock.advance(Duration::seconds(61));
    let write_off_confirmed = app
        .oneshot(request(
            "POST",
            format!("/admin/receivables/{receivable}/write_off/confirm"),
            Some(("x-admin-token", "superadmin-1")),
            json!({"reason":"approved","idempotency_key":"write-off-ok"}),
        ))
        .await
        .unwrap();
    assert_eq!(write_off_confirmed.status(), StatusCode::OK);
    assert_eq!(body_json(write_off_confirmed).await["status"], "confirmed");
}

/// One state with four single-role principals (curator/ops/finance/superadmin
/// tokens named after their role) built through the full-injection Phase 6
/// constructor.
fn rbac_app() -> axum::Router {
    let doc = serde_json::to_string(
        &["curator", "ops", "finance", "superadmin"]
            .iter()
            .map(|role| {
                json!({
                    "id": format!("{role}-1"),
                    "roles": [role],
                    "sha256": sha256_hex(&format!("{role}-token")),
                })
            })
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let tokens = adapters::http::middleware::AdminTokens::parse(&doc).unwrap();
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
        Arc::new(application::ports::StaticConfigReads::default()),
    );
    router(state)
}

fn dual_control_body() -> Value {
    json!({"reason": "test", "idempotency_key": "k"})
}

#[tokio::test]
async fn rbac_denies_unauthenticated_unknown_and_unmapped_requests() {
    let app = rbac_app();
    // No token and an unknown token are both 401 before any handler runs.
    for token in [None, Some(("x-admin-token", "wrong-token"))] {
        let res = app
            .clone()
            .oneshot(request("GET", "/admin/config", token, json!({})))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
    // A known token on a mapped route it lacks is 403, never the handler.
    let res = app
        .clone()
        .oneshot(request(
            "GET",
            "/admin/audit",
            Some(("x-admin-token", "curator-token")),
            json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::FORBIDDEN,
        "audit reads need audit-read, which curators do not have (D26)"
    );
    let body = body_json(res).await;
    assert_eq!(body["code"], "Forbidden");
}

#[tokio::test]
async fn rbac_dual_control_split_and_role_grants_are_enforced_per_route() {
    let app = rbac_app();
    let market = Uuid::new_v4();
    // D30: propose is superadmin-only, confirm is finance-only.
    let cases = [
        (
            "POST",
            format!("/admin/markets/{market}/unwind/propose"),
            "finance-token",
            StatusCode::FORBIDDEN,
        ),
        (
            "POST",
            format!("/admin/markets/{market}/unwind/propose"),
            "superadmin-token",
            StatusCode::NOT_FOUND,
        ),
        (
            "POST",
            format!("/admin/markets/{market}/unwind/confirm"),
            "superadmin-token",
            StatusCode::FORBIDDEN,
        ),
        (
            "POST",
            format!("/admin/markets/{market}/unwind/confirm"),
            "finance-token",
            StatusCode::NOT_FOUND,
        ),
        (
            "GET",
            "/admin/fees/summary".to_string(),
            "curator-token",
            StatusCode::FORBIDDEN,
        ),
        (
            "GET",
            "/admin/fees/summary".to_string(),
            "finance-token",
            StatusCode::OK,
        ),
        (
            "GET",
            "/admin/audit".to_string(),
            "ops-token",
            StatusCode::OK,
        ),
        (
            "GET",
            "/admin/invariants".to_string(),
            "finance-token",
            StatusCode::FORBIDDEN,
        ),
        (
            "GET",
            "/admin/invariants".to_string(),
            "ops-token",
            StatusCode::OK,
        ),
        (
            "POST",
            format!("/admin/markets/{market}/advance"),
            "finance-token",
            StatusCode::FORBIDDEN,
        ),
    ];
    for (method, uri, token, expected) in cases {
        let body = if method == "POST" {
            json!({"reason": "test", "idempotency_key": "k", "event": "approve"})
        } else {
            json!({})
        };
        let res = app
            .clone()
            .oneshot(request(method, &uri, Some(("x-admin-token", token)), body))
            .await
            .unwrap();
        assert_eq!(res.status(), expected, "{method} {uri} as {token}");
    }
}

#[tokio::test]
async fn every_ops_route_answers_with_its_real_w2_vocabulary() {
    let (app, _, user, _) = setup();
    let market = Uuid::new_v4();
    let receivable = Uuid::new_v4();
    let admin = Some(("x-admin-token", ADMIN));
    // W2 landed the manual-ops plane: reads answer 200 on an empty store,
    // authority flows answer 404/409 for unknown subjects — never 501.
    let cases: Vec<(&str, String, Value, StatusCode)> = vec![
        ("GET", "/admin/invariants".into(), json!({}), StatusCode::OK),
        ("GET", "/admin/audit".into(), json!({}), StatusCode::OK),
        (
            "GET",
            format!("/admin/users/{}/withdrawal_eligibility", user.0),
            json!({}),
            StatusCode::OK,
        ),
        (
            "POST",
            format!("/admin/markets/{market}/unwind/propose"),
            dual_control_body(),
            StatusCode::NOT_FOUND,
        ),
        (
            "POST",
            format!("/admin/markets/{market}/unwind/confirm"),
            dual_control_body(),
            StatusCode::NOT_FOUND,
        ),
        (
            "POST",
            format!("/admin/markets/{market}/unwind/reject"),
            dual_control_body(),
            StatusCode::CONFLICT,
        ),
        (
            "POST",
            format!("/admin/markets/{market}/remedial_credit/propose"),
            json!({"user_id": user.0, "amount_micro": 1, "reason": "test", "idempotency_key": "k"}),
            StatusCode::NOT_FOUND,
        ),
        (
            "POST",
            format!("/admin/markets/{market}/remedial_credit/confirm"),
            dual_control_body(),
            StatusCode::CONFLICT,
        ),
        (
            "POST",
            format!("/admin/receivables/{receivable}/write_off/propose"),
            dual_control_body(),
            StatusCode::NOT_FOUND,
        ),
        (
            "POST",
            format!("/admin/receivables/{receivable}/write_off/confirm"),
            dual_control_body(),
            StatusCode::CONFLICT,
        ),
        (
            "GET",
            format!("/admin/drafts/{}/publish_status", Uuid::new_v4()),
            json!({}),
            StatusCode::OK,
        ),
    ];
    for (method, uri, body, expected) in cases {
        let res = app
            .clone()
            .oneshot(request(method, &uri, admin, body))
            .await
            .unwrap();
        assert_eq!(res.status(), expected, "{method} {uri}");
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn the_staging_faucet_mounts_only_under_both_factors() {
    // Without the two-factor arm the route does not EXIST (404, not 401/403).
    let (app, _, user, _) = setup();
    let deposit = json!({
        "user_id": user.0,
        "amount_micro": 1_000_000,
        "reason": "test",
        "idempotency_key": "k",
    });
    let res = app
        .clone()
        .oneshot(request(
            "POST",
            "/admin/deposits",
            Some(("x-admin-token", ADMIN)),
            deposit.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // With BOTH factors the route exists behind the finance capability.
    let saved_env = std::env::var("OPINIONS_ENV").ok();
    let saved_faucet = std::env::var("STAGING_FAUCET").ok();
    std::env::set_var("OPINIONS_ENV", "staging");
    std::env::set_var("STAGING_FAUCET", "1");
    let armed_store = InMemoryStore::new();
    let armed = {
        let state = AppState::with_tokens(
            armed_store.clone(),
            Arc::new(FakeClock::at(t0())),
            DEMO,
            ADMIN,
        );
        assert!(state.inner.staging_faucet);
        router(state)
    };
    // One factor alone must NOT mount.
    std::env::remove_var("STAGING_FAUCET");
    let single_factor = {
        let store = InMemoryStore::new();
        let state = AppState::with_tokens(store, Arc::new(FakeClock::at(t0())), DEMO, ADMIN);
        assert!(!state.inner.staging_faucet);
        router(state)
    };
    match saved_env {
        Some(v) => std::env::set_var("OPINIONS_ENV", v),
        None => std::env::remove_var("OPINIONS_ENV"),
    }
    match saved_faucet {
        Some(v) => std::env::set_var("STAGING_FAUCET", v),
        None => std::env::remove_var("STAGING_FAUCET"),
    }
    let negative_rep = armed
        .clone()
        .oneshot(request(
            "POST",
            "/users",
            None,
            json!({
                "handle": "negative-rep",
                "channel": "imessage",
                "address": "+15559990002",
                "rep_seed_micro": -1,
            }),
        ))
        .await
        .unwrap();
    assert_eq!(negative_rep.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body_json(negative_rep).await["code"], "InvalidSignup");
    let over_cap = armed
        .clone()
        .oneshot(request(
            "POST",
            "/admin/deposits",
            Some(("x-admin-token", ADMIN)),
            json!({
                "user_id": user.0,
                "amount_micro": i64::MAX,
                "reason": "too much",
                "idempotency_key": "over-cap",
            }),
        ))
        .await
        .unwrap();
    assert_eq!(over_cap.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body_json(over_cap).await["code"], "OverCap");
    let override_signup = armed
        .clone()
        .oneshot(request(
            "POST",
            "/users",
            None,
            json!({
                "handle": "aged-swarm-user",
                "channel": "imessage",
                "address": "+15559990000",
                "created_at_override": (t0() - Duration::days(80)),
                "rep_seed_micro": 400_000,
            }),
        ))
        .await
        .unwrap();
    assert_eq!(override_signup.status(), StatusCode::OK);
    let override_user = UserId(
        body_json(override_signup).await["user_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap(),
    );
    assert_eq!(armed_store.user_rep(override_user).await.unwrap().tier, 2);
    assert!(armed_store
        .audit_page(None, 10)
        .await
        .unwrap()
        .iter()
        .any(|row| row.action.action == "create_user_override"));

    let forbidden_override = single_factor
        .clone()
        .oneshot(request(
            "POST",
            "/users",
            None,
            json!({
                "handle": "forbidden-aged-user",
                "channel": "imessage",
                "address": "+15559990001",
                "created_at_override": (t0() - Duration::days(80)),
            }),
        ))
        .await
        .unwrap();
    assert_eq!(forbidden_override.status(), StatusCode::FORBIDDEN);
    let res = armed
        .oneshot(request(
            "POST",
            "/admin/deposits",
            Some(("x-admin-token", ADMIN)),
            deposit.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "armed faucet credits for real"
    );
    let res = single_factor
        .oneshot(request(
            "POST",
            "/admin/deposits",
            Some(("x-admin-token", ADMIN)),
            deposit,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn preview_stamps_config_version_and_place_accepts_the_echo() {
    let (app, store, user, slug) = setup();
    store.record_vote(
        user,
        store.market_by_ref(&slug).await.unwrap().id,
        Side::Yes,
    );
    let preview = app
        .clone()
        .oneshot(request(
            "POST",
            "/trades/preview",
            Some(("x-demo-token", DEMO)),
            json!({
                "user_id": user.0,
                "market_ref": slug,
                "side": "yes",
                "action": "buy",
                "amount_micro": 5_000_000,
            }),
        ))
        .await
        .unwrap();
    assert_eq!(preview.status(), StatusCode::OK);
    let preview = body_json(preview).await;
    assert_eq!(
        preview["config_version"], 1,
        "the pre-wave stamp is the 0008 seed generation"
    );
    let place = app
        .oneshot(request(
            "POST",
            "/trades",
            Some(("x-demo-token", DEMO)),
            json!({
                "user_id": user.0,
                "market_ref": slug,
                "side": "yes",
                "action": "buy",
                "amount_micro": 5_000_000,
                "idempotency_key": "cfg-echo",
                "expected_config_version": preview["config_version"],
            }),
        ))
        .await
        .unwrap();
    assert_eq!(
        place.status(),
        StatusCode::OK,
        "W1 owns the fence-point check"
    );
}
