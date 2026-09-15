#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration as StdDuration;

use adapters::http::{router, AppState};
use application::fakes::{FakeClock, InMemoryStore};
use domain::market::MarketState;
use domain::money::{BasisPoints, MicroShares};
use time::{Duration, OffsetDateTime};
use tokio_tungstenite::tungstenite::stream::MaybeTlsStream;
use tokio_tungstenite::tungstenite::{connect, Message};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_frame_drop_fault_keeps_the_socket_open_without_a_snapshot() {
    let owned_error = adapters::http::error::ErrorResponse::new(
        axum::http::StatusCode::BAD_REQUEST,
        "OwnedCode",
        "owned message",
    );
    assert_eq!(owned_error.body.code, "OwnedCode");
    std::env::set_var("CHAOS_WS_DROP_EVERY_N", "1");
    std::env::remove_var("CHAOS_RELAY_DELAY_MS");
    let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
    let store = InMemoryStore::new();
    let market = store
        .add_market(
            "ws-drop-fault",
            MarketState::Live,
            now + Duration::hours(2),
            now + Duration::hours(1),
            MicroShares(10_000_000),
            BasisPoints(100),
        )
        .unwrap();
    let state = AppState::with_tokens(
        store,
        Arc::new(FakeClock::at(now)),
        "demo-token",
        "admin-token",
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    let market_id = market.id.0;
    tokio::task::spawn_blocking(move || {
        let (mut socket, _) = connect(format!("ws://{address}/ws")).unwrap();
        if let MaybeTlsStream::Plain(stream) = socket.get_mut() {
            stream
                .set_read_timeout(Some(StdDuration::from_millis(100)))
                .unwrap();
        }
        socket
            .send(Message::Text(
                serde_json::json!({"op":"subscribe","market_id":market_id})
                    .to_string()
                    .into(),
            ))
            .unwrap();
        let error = socket.read().unwrap_err();
        assert!(matches!(
            error,
            tokio_tungstenite::tungstenite::Error::Io(ref io)
                if matches!(io.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
        ));
    })
    .await
    .unwrap();
    server.abort();
}
