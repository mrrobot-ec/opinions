//! Public market WebSocket. Slow consumers are disconnected and recover by
//! reconnecting, re-subscribing, and applying the immediate snapshot.

use std::collections::HashSet;
use std::sync::Arc;

use application::error::StoreError;
use application::model::{MarketId, MarketSnapshot, UserId};
use application::ports::{Clock, MarketQueries, NotificationQueries, Store};
use async_trait::async_trait;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::relay::{process_faults, BusEvent, ConnectionFaultInjector, WireEvent};

use super::dto::MarketStateDto;
use super::routes::AppState;

#[async_trait]
trait SnapshotQueries: Send + Sync {
    async fn snapshot(
        &self,
        market: MarketId,
        now: OffsetDateTime,
    ) -> Result<MarketSnapshot, StoreError>;
}

#[cfg(test)]
#[async_trait]
impl<Q> SnapshotQueries for Q
where
    Q: MarketQueries + Send + Sync,
{
    async fn snapshot(
        &self,
        market: MarketId,
        now: OffsetDateTime,
    ) -> Result<MarketSnapshot, StoreError> {
        self.market_snapshot(market, now).await
    }
}

#[async_trait]
trait ConnectionQueries: SnapshotQueries {
    async fn unread_count(&self, user: UserId) -> Result<u32, StoreError>;
}

struct AppQueries<S>(Arc<super::routes::AppStateInner<S>>);

#[async_trait]
impl<S> SnapshotQueries for AppQueries<S>
where
    S: MarketQueries + Send + Sync,
{
    async fn snapshot(
        &self,
        market: MarketId,
        now: OffsetDateTime,
    ) -> Result<MarketSnapshot, StoreError> {
        self.0.store.market_snapshot(market, now).await
    }
}

#[async_trait]
impl<S> ConnectionQueries for AppQueries<S>
where
    S: MarketQueries + NotificationQueries + Send + Sync,
{
    async fn unread_count(&self, user: UserId) -> Result<u32, StoreError> {
        self.0.store.unread_count(user).await
    }
}

pub(crate) struct ConnectionState {
    queries: Arc<dyn ConnectionQueries>,
    clock: super::routes::SharedClock,
    demo_token: String,
    events: broadcast::Sender<BusEvent>,
}

impl ConnectionState {
    pub(crate) fn from_app<S>(state: AppState<S>) -> Self
    where
        S: Store + MarketQueries + NotificationQueries + Send + Sync + 'static,
    {
        Self {
            queries: Arc::new(AppQueries(Arc::clone(&state.inner))),
            clock: state.inner.clock.clone(),
            demo_token: state.inner.demo_token.clone(),
            events: state.inner.events.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum ClientFrame {
    Subscribe { market_id: Uuid },
    Unsubscribe { market_id: Uuid },
    SubscribeUser { user_id: Uuid, token: String },
}

/// Stable v1 server protocol. Outbox-derived variants carry `outbox_seq`;
/// snapshots are point-in-time query projections and have no sequence.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    Snapshot {
        v: u8,
        market_id: Uuid,
        server_now: OffsetDateTime,
        state: MarketStateDto,
        price_yes_micro: i64,
        price_no_micro: i64,
        tally: Option<TallyFrame>,
        closes_at: OffsetDateTime,
        tally_hidden_at: OffsetDateTime,
        under_review: bool,
        poster_asset_url: Option<String>,
        video_asset_url: Option<String>,
    },
    Price {
        v: u8,
        outbox_seq: i64,
        market_id: Uuid,
        price_yes_micro: i64,
        price_no_micro: i64,
    },
    Trade {
        v: u8,
        outbox_seq: i64,
        market_id: Uuid,
        handle: String,
        side: String,
        action: String,
        collateral_micro: i64,
        created_at: OffsetDateTime,
        trade_seq: i64,
    },
    Lifecycle {
        v: u8,
        outbox_seq: i64,
        market_id: Uuid,
        state: String,
        final_vote_bps: Option<u16>,
        redemption_yes_micro: Option<i64>,
        redemption_no_micro: Option<i64>,
    },
    TradingPaused {
        v: u8,
        outbox_seq: i64,
        market_id: Option<Uuid>,
    },
    TradingResumed {
        v: u8,
        outbox_seq: i64,
        market_id: Option<Uuid>,
    },
    MarketVotingPaused {
        v: u8,
        outbox_seq: i64,
        market_id: Uuid,
    },
    MarketVotingResumed {
        v: u8,
        outbox_seq: i64,
        market_id: Uuid,
    },
    Tally {
        v: u8,
        outbox_seq: i64,
        market_id: Uuid,
        yes: i64,
        no: i64,
    },
    Asset {
        v: u8,
        outbox_seq: i64,
        market_id: Uuid,
        kind: String,
        url: String,
    },
    Notif {
        v: u8,
        id: i64,
        source_seq: i64,
        notification_type: String,
        payload: serde_json::Value,
    },
    NotifSnapshot {
        v: u8,
        user_id: Uuid,
        unread_count: u32,
    },
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct TallyFrame {
    pub yes: i64,
    pub no: i64,
}

fn is_subscribed(subscriptions: &HashSet<Uuid>, market_id: Uuid) -> bool {
    subscriptions.contains(&market_id)
}

fn market_event_is_visible(subscriptions: &HashSet<Uuid>, event: &WireEvent) -> bool {
    if matches!(
        event.event_type.as_str(),
        "TradingPaused" | "TradingResumed"
    ) && event.aggregate_type == "config"
    {
        !subscriptions.is_empty()
    } else {
        is_subscribed(subscriptions, event.aggregate_id)
    }
}

fn apply_subscription_command(
    subscriptions: &mut HashSet<Uuid>,
    command: ClientFrame,
) -> Option<Uuid> {
    match command {
        ClientFrame::Subscribe { market_id } => {
            subscriptions.insert(market_id);
            Some(market_id)
        }
        ClientFrame::Unsubscribe { market_id } => {
            subscriptions.remove(&market_id);
            None
        }
        ClientFrame::SubscribeUser { .. } => None,
    }
}

fn received_event(received: Result<BusEvent, broadcast::error::RecvError>) -> Result<BusEvent, ()> {
    received.map_err(|_| ())
}

fn snapshot_frame(snapshot: MarketSnapshot, now: OffsetDateTime) -> ServerFrame {
    ServerFrame::Snapshot {
        v: 1,
        market_id: snapshot.market.0,
        server_now: now,
        state: snapshot.state.into(),
        price_yes_micro: snapshot.price_yes_micro,
        price_no_micro: snapshot.price_no_micro,
        tally: snapshot.tally.map(|tally| TallyFrame {
            yes: tally.yes_votes,
            no: tally.no_votes,
        }),
        closes_at: snapshot.closes_at,
        tally_hidden_at: snapshot.tally_hidden_at,
        under_review: snapshot.under_review,
        poster_asset_url: snapshot.poster_asset_url,
        video_asset_url: snapshot.video_asset_url,
    }
}

#[derive(Deserialize)]
struct TradePayload {
    handle: String,
    side: String,
    action: String,
    collateral_micro: i64,
    trade_seq: i64,
    created_at: OffsetDateTime,
}

/// Derives zero, one, or two public frames from an outbox event.
async fn event_frames(
    queries: &dyn SnapshotQueries,
    now: OffsetDateTime,
    event: &WireEvent,
) -> Result<Vec<ServerFrame>, application::error::StoreError> {
    let market = MarketId(event.aggregate_id);
    match event.event_type.as_str() {
        "TradePlaced" => {
            let snapshot = queries.snapshot(market, now).await?;
            let payload: TradePayload = serde_json::from_value(event.payload.clone())
                .map_err(|_| application::error::StoreError::Invariant("invalid trade event"))?;
            Ok(vec![
                ServerFrame::Price {
                    v: 1,
                    outbox_seq: event.outbox_seq,
                    market_id: market.0,
                    price_yes_micro: snapshot.price_yes_micro,
                    price_no_micro: snapshot.price_no_micro,
                },
                ServerFrame::Trade {
                    v: 1,
                    outbox_seq: event.outbox_seq,
                    market_id: market.0,
                    handle: payload.handle,
                    side: payload.side,
                    action: payload.action,
                    collateral_micro: payload.collateral_micro,
                    created_at: payload.created_at,
                    trade_seq: payload.trade_seq,
                },
            ])
        }
        "MarketSeeded" => {
            let snapshot = queries.snapshot(market, now).await?;
            Ok(vec![ServerFrame::Price {
                v: 1,
                outbox_seq: event.outbox_seq,
                market_id: market.0,
                price_yes_micro: snapshot.price_yes_micro,
                price_no_micro: snapshot.price_no_micro,
            }])
        }
        "VoteCast" => {
            let snapshot = queries.snapshot(market, now).await?;
            Ok(snapshot
                .tally
                .map(|tally| ServerFrame::Tally {
                    v: 1,
                    outbox_seq: event.outbox_seq,
                    market_id: market.0,
                    yes: tally.yes_votes,
                    no: tally.no_votes,
                })
                .into_iter()
                .collect())
        }
        "MarketAdvanced" | "MarketResolved" | "MarketVoided" => {
            let state = match event.event_type.as_str() {
                "MarketResolved" => "resolved".to_string(),
                "MarketVoided" => "voided".to_string(),
                _ => event
                    .payload
                    .get("to")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown")
                    .to_ascii_lowercase(),
            };
            Ok(vec![ServerFrame::Lifecycle {
                v: 1,
                outbox_seq: event.outbox_seq,
                market_id: market.0,
                state,
                final_vote_bps: event
                    .payload
                    .get("final_vote_bps")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|value| u16::try_from(value).ok()),
                redemption_yes_micro: event
                    .payload
                    .get("redemption_yes_micro")
                    .and_then(serde_json::Value::as_i64),
                redemption_no_micro: event
                    .payload
                    .get("redemption_no_micro")
                    .and_then(serde_json::Value::as_i64),
            }])
        }
        "TradingPaused" => Ok(vec![ServerFrame::TradingPaused {
            v: 1,
            outbox_seq: event.outbox_seq,
            market_id: (event.aggregate_type == "market").then_some(event.aggregate_id),
        }]),
        "TradingResumed" => Ok(vec![ServerFrame::TradingResumed {
            v: 1,
            outbox_seq: event.outbox_seq,
            market_id: (event.aggregate_type == "market").then_some(event.aggregate_id),
        }]),
        "MarketVotingPaused" => Ok(vec![ServerFrame::MarketVotingPaused {
            v: 1,
            outbox_seq: event.outbox_seq,
            market_id: event.aggregate_id,
        }]),
        "MarketVotingResumed" => Ok(vec![ServerFrame::MarketVotingResumed {
            v: 1,
            outbox_seq: event.outbox_seq,
            market_id: event.aggregate_id,
        }]),
        "VideoAttached" => Ok(vec![ServerFrame::Asset {
            v: 1,
            outbox_seq: event.outbox_seq,
            market_id: market.0,
            kind: event
                .payload
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .ok_or(application::error::StoreError::Invariant(
                    "invalid asset event",
                ))?
                .to_string(),
            url: event
                .payload
                .get("url")
                .and_then(serde_json::Value::as_str)
                .ok_or(application::error::StoreError::Invariant(
                    "invalid asset event",
                ))?
                .to_string(),
        }]),
        _ => Ok(Vec::new()),
    }
}

pub(crate) fn ws_upgrade(ws: WebSocketUpgrade, state: ConnectionState) -> Response {
    ws.on_upgrade(move |socket| serve(socket, state))
}

#[inline]
async fn send_frame(
    socket: &mut WebSocket,
    frame: &ServerFrame,
    faults: &mut ConnectionFaultInjector,
) -> Result<(), ()> {
    if faults.should_drop() {
        return Ok(());
    }
    let text = serde_json::to_string(frame).map_err(|_| ())?;
    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(|_| ())
}

async fn serve(mut socket: WebSocket, state: ConnectionState) {
    let mut subscriptions = HashSet::new();
    let mut user_subscription = None;
    let mut events = state.events.subscribe();
    let Ok(process_faults) = process_faults() else {
        return;
    };
    let mut faults = process_faults.connection();
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                let Some(Ok(Message::Text(text))) = incoming else { break; };
                let Ok(command) = serde_json::from_str::<ClientFrame>(&text) else { continue; };
                if let ClientFrame::SubscribeUser { user_id, token } = &command {
                    // Constant-time: this frame hands out another user's
                    // private notification stream, so the token compare must
                    // not leak a prefix. See `http::middleware::secret_eq`.
                    if !crate::http::middleware::secret_eq(token, &state.demo_token) {
                        continue;
                    }
                    user_subscription = Some(*user_id);
                    let Ok(unread_count) = state.queries.unread_count(UserId(*user_id)).await else { continue; };
                    let frame = ServerFrame::NotifSnapshot { v: 1, user_id: *user_id, unread_count };
                    if send_frame(&mut socket, &frame, &mut faults).await.is_err() { break; }
                    continue;
                }
                if let Some(market_id) = apply_subscription_command(&mut subscriptions, command) {
                    let now = state.clock.now();
                    let Ok(snapshot) = state.queries.snapshot(MarketId(market_id), now).await else { continue; };
                    if send_frame(&mut socket, &snapshot_frame(snapshot, now), &mut faults).await.is_err() { break; }
                }
            }
            received = events.recv() => {
                let Ok(event) = received_event(received) else { break; };
                match event {
                    BusEvent::Market(event) => {
                        if !market_event_is_visible(&subscriptions, &event) { continue; }
                        let now = state.clock.now();
                        let Ok(frames) = event_frames(state.queries.as_ref(), now, &event).await else { continue; };
                        for frame in frames {
                            if send_frame(&mut socket, &frame, &mut faults).await.is_err() { return; }
                        }
                    }
                    BusEvent::UserNotif { user_id, frame } => {
                        if user_subscription != Some(user_id) { continue; }
                        let frame = ServerFrame::Notif {
                            v: 1,
                            id: frame.id,
                            source_seq: frame.source_seq,
                            notification_type: frame.notification_type,
                            payload: frame.payload,
                        };
                        if send_frame(&mut socket, &frame, &mut faults).await.is_err() { return; }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use application::fakes::InMemoryStore;
    use application::ports::MarketQueries;
    use domain::market::MarketState;
    use domain::money::{BasisPoints, MicroShares};

    #[tokio::test]
    async fn vote_frames_stop_exactly_at_hidden_boundary() {
        let store = InMemoryStore::new();
        let at = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let market = store
            .add_market(
                "ws-boundary",
                MarketState::Live,
                at + time::Duration::hours(1),
                at,
                MicroShares(10_000_000),
                BasisPoints(100),
            )
            .unwrap();
        let event = WireEvent {
            outbox_seq: 1,
            event_type: "VoteCast".into(),
            aggregate_type: "market".into(),
            aggregate_id: market.id.0,
            payload: serde_json::json!({"market_id": market.id.0}),
        };
        assert_eq!(
            event_frames(&store, at - time::Duration::SECOND, &event)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(event_frames(&store, at, &event).await.unwrap().is_empty());
        assert!(store
            .market_snapshot(market.id, at)
            .await
            .unwrap()
            .tally
            .is_none());
    }

    #[tokio::test]
    async fn source_events_map_to_versioned_frames() {
        let store = InMemoryStore::new();
        let at = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let market = store
            .add_market(
                "ws-events",
                MarketState::Live,
                at + time::Duration::hours(2),
                at + time::Duration::hours(1),
                MicroShares(10_000_000),
                BasisPoints(100),
            )
            .unwrap();
        let snapshot = store.market_snapshot(market.id, at).await.unwrap();
        let snapshot_frame = snapshot_frame(snapshot, at);
        let encoded = serde_json::to_value(snapshot_frame).unwrap();
        assert_eq!(encoded["type"], "snapshot");
        assert_eq!(encoded["v"], 1);
        assert_eq!(encoded["market_id"], market.id.0.to_string());
        assert!(encoded["server_now"].is_string());
        assert!(encoded["poster_asset_url"].is_null());

        let trade = WireEvent {
            outbox_seq: 7,
            event_type: "TradePlaced".into(),
            aggregate_type: "market".into(),
            aggregate_id: market.id.0,
            payload: serde_json::json!({
                "handle": "alice",
                "side": "yes",
                "action": "buy",
                "collateral_micro": 5000,
                "trade_seq": 4,
                "created_at": at,
            }),
        };
        let trade_frames = event_frames(&store, at, &trade).await.unwrap();
        assert_eq!(trade_frames.len(), 2);
        assert!(matches!(
            trade_frames[0],
            ServerFrame::Price { outbox_seq: 7, .. }
        ));
        assert!(matches!(
            trade_frames[1],
            ServerFrame::Trade { trade_seq: 4, .. }
        ));

        let seeded = WireEvent {
            event_type: "MarketSeeded".into(),
            outbox_seq: 8,
            ..trade.clone()
        };
        assert!(matches!(
            event_frames(&store, at, &seeded).await.unwrap().as_slice(),
            [ServerFrame::Price { outbox_seq: 8, .. }]
        ));

        let advanced = WireEvent {
            event_type: "MarketAdvanced".into(),
            outbox_seq: 9,
            payload: serde_json::json!({"to":"Closing"}),
            ..trade.clone()
        };
        let frames = event_frames(&store, at, &advanced).await.unwrap();
        assert!(matches!(
            frames.as_slice(),
            [ServerFrame::Lifecycle { state, .. }] if state == "closing"
        ));

        let resolved = WireEvent {
            event_type: "MarketResolved".into(),
            outbox_seq: 10,
            payload: serde_json::json!({
                "final_vote_bps": 6000,
                "redemption_yes_micro": 600_000,
                "redemption_no_micro": 400_000,
            }),
            ..trade.clone()
        };
        let frames = event_frames(&store, at, &resolved).await.unwrap();
        assert!(matches!(
            frames.as_slice(),
            [ServerFrame::Lifecycle {
                state,
                final_vote_bps: Some(6000),
                redemption_yes_micro: Some(600_000),
                redemption_no_micro: Some(400_000),
                ..
            }] if state == "resolved"
        ));

        let voided = WireEvent {
            event_type: "MarketVoided".into(),
            outbox_seq: 11,
            ..trade.clone()
        };
        assert!(matches!(
            event_frames(&store, at, &voided).await.unwrap().as_slice(),
            [ServerFrame::Lifecycle { state, .. }] if state == "voided"
        ));
        let global_pause = WireEvent {
            event_type: "TradingPaused".into(),
            aggregate_type: "config".into(),
            aggregate_id: Uuid::nil(),
            outbox_seq: 12,
            payload: serde_json::json!({"scope":"global"}),
        };
        assert!(matches!(
            event_frames(&store, at, &global_pause)
                .await
                .unwrap()
                .as_slice(),
            [ServerFrame::TradingPaused {
                outbox_seq: 12,
                market_id: None,
                ..
            }]
        ));
        assert!(market_event_is_visible(
            &[market.id.0].into_iter().collect(),
            &global_pause
        ));
        let global_resume = WireEvent {
            event_type: "TradingResumed".into(),
            outbox_seq: 13,
            ..global_pause.clone()
        };
        assert!(matches!(
            event_frames(&store, at, &global_resume)
                .await
                .unwrap()
                .as_slice(),
            [ServerFrame::TradingResumed {
                outbox_seq: 13,
                market_id: None,
                ..
            }]
        ));
        let voting_pause = WireEvent {
            event_type: "MarketVotingPaused".into(),
            aggregate_type: "market".into(),
            aggregate_id: market.id.0,
            outbox_seq: 14,
            payload: serde_json::json!({"market_id":market.id.0}),
        };
        assert!(matches!(
            event_frames(&store, at, &voting_pause).await.unwrap().as_slice(),
            [ServerFrame::MarketVotingPaused { outbox_seq: 14, market_id, .. }]
                if *market_id == market.id.0
        ));
        let voting_resume = WireEvent {
            event_type: "MarketVotingResumed".into(),
            outbox_seq: 15,
            ..voting_pause
        };
        assert!(matches!(
            event_frames(&store, at, &voting_resume).await.unwrap().as_slice(),
            [ServerFrame::MarketVotingResumed { outbox_seq: 15, market_id, .. }]
                if *market_id == market.id.0
        ));
        let attached = WireEvent {
            event_type: "VideoAttached".into(),
            outbox_seq: 16,
            payload: serde_json::json!({"kind":"poster", "url":"/assets/a.svg"}),
            ..trade.clone()
        };
        assert!(matches!(
            event_frames(&store, at, &attached).await.unwrap().as_slice(),
            [ServerFrame::Asset { outbox_seq: 16, kind, url, .. }]
                if kind == "poster" && url == "/assets/a.svg"
        ));
        let malformed_asset = WireEvent {
            payload: serde_json::json!({"kind":"poster"}),
            ..attached
        };
        assert_eq!(
            event_frames(&store, at, &malformed_asset)
                .await
                .unwrap_err(),
            application::error::StoreError::Invariant("invalid asset event")
        );
        let ignored = WireEvent {
            event_type: "DepositCredited".into(),
            ..trade
        };
        assert!(event_frames(&store, at, &ignored).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn malformed_trade_payload_is_rejected() {
        let store = InMemoryStore::new();
        let at = OffsetDateTime::UNIX_EPOCH;
        let market = store
            .add_market(
                "ws-malformed",
                MarketState::Live,
                at + time::Duration::hours(2),
                at + time::Duration::hours(1),
                MicroShares(10_000_000),
                BasisPoints(100),
            )
            .unwrap();
        let error = event_frames(
            &store,
            at,
            &WireEvent {
                outbox_seq: 1,
                event_type: "TradePlaced".into(),
                aggregate_type: "market".into(),
                aggregate_id: market.id.0,
                payload: serde_json::json!({}),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            error,
            application::error::StoreError::Invariant("invalid trade event")
        );
    }

    #[tokio::test]
    async fn subscriptions_filter_and_lag_means_disconnect() {
        let subscribed = uuid::Uuid::new_v4();
        let ignored = uuid::Uuid::new_v4();
        let mut subscriptions = HashSet::new();
        subscriptions.insert(subscribed);
        assert!(is_subscribed(&subscriptions, subscribed));
        assert!(!is_subscribed(&subscriptions, ignored));
        subscriptions.remove(&subscribed);
        assert!(!is_subscribed(&subscriptions, subscribed));

        let (tx, mut receiver) = broadcast::channel(1);
        let wire = WireEvent {
            outbox_seq: 1,
            event_type: "MarketSeeded".into(),
            aggregate_type: "market".into(),
            aggregate_id: subscribed,
            payload: serde_json::json!({}),
        };
        // Market-scoped events are gated by the subscription set, not by the
        // global-pause rule that only "config" aggregates take.
        subscriptions.insert(subscribed);
        assert!(market_event_is_visible(&subscriptions, &wire));
        subscriptions.remove(&subscribed);
        assert!(!market_event_is_visible(&subscriptions, &wire));

        let event = BusEvent::Market(wire.clone());
        tx.send(event.clone()).unwrap();
        tx.send(BusEvent::Market(WireEvent {
            outbox_seq: 2,
            ..wire
        }))
        .unwrap();
        assert!(received_event(receiver.recv().await).is_err());
        drop(tx);
        assert!(received_event(receiver.recv().await).is_ok());
        assert!(received_event(receiver.recv().await).is_err());
    }

    #[test]
    fn subscription_commands_and_lifecycle_identity_are_explicit() {
        let market = Uuid::new_v4();
        let mut subscriptions = HashSet::new();
        assert_eq!(
            apply_subscription_command(
                &mut subscriptions,
                ClientFrame::Subscribe { market_id: market }
            ),
            Some(market)
        );
        assert!(is_subscribed(&subscriptions, market));
        assert_eq!(
            apply_subscription_command(
                &mut subscriptions,
                ClientFrame::Unsubscribe { market_id: market }
            ),
            None
        );
        assert!(!is_subscribed(&subscriptions, market));
        assert_eq!(
            apply_subscription_command(
                &mut subscriptions,
                ClientFrame::SubscribeUser {
                    user_id: Uuid::new_v4(),
                    token: "demo".to_string(),
                },
            ),
            None
        );

        let lifecycle = ServerFrame::Lifecycle {
            v: 1,
            outbox_seq: 9,
            market_id: market,
            state: "closing".to_string(),
            final_vote_bps: None,
            redemption_yes_micro: None,
            redemption_no_micro: None,
        };
        assert_eq!(
            serde_json::to_value(lifecycle).unwrap()["market_id"],
            market.to_string()
        );
    }
}
