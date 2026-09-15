//! The sole HTTP/WS construction boundary for the outer swarm harness.

use std::collections::BTreeMap;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use reqwest::header::{HeaderValue, AUTHORIZATION};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::domain::action::{Action, AgentAction, Decision, MarketView, Side, TradeDirection};
use crate::domain::persona::AgentSpec;
use crate::ports::{SwarmError, Transport};
use crate::trace::NormalizedOutcome;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpOutcome {
    pub status: u16,
    pub code: Option<String>,
}

impl From<HttpOutcome> for NormalizedOutcome {
    fn from(outcome: HttpOutcome) -> Self {
        Self::Http {
            status: outcome.status,
            code: outcome.code,
        }
    }
}

pub struct HttpTransport {
    base_url: reqwest::Url,
    client: reqwest::Client,
    demo_token: Option<String>,
}

impl HttpTransport {
    pub fn new(base_url: &str) -> Result<Self, SwarmError> {
        let mut base_url = match reqwest::Url::parse(base_url) {
            Ok(url) => url,
            Err(error) => return Err(SwarmError::Transport(error.to_string())),
        };
        if !base_url.path().ends_with('/') {
            base_url.set_path(&format!("{}/", base_url.path()));
        }
        let client = reqwest::Client::new();
        Ok(Self {
            base_url,
            client,
            demo_token: None,
        })
    }

    #[must_use]
    pub fn with_demo_token(mut self, token: &str) -> Self {
        self.demo_token = Some(token.to_owned());
        self
    }

    fn url(&self, path: &str) -> reqwest::Url {
        let mut url = self.base_url.clone();
        let base_path = self.base_url.path().trim_end_matches('/');
        let suffix = path.trim_start_matches('/');
        url.set_path(&format!("{base_path}/{suffix}"));
        url
    }

    pub async fn get_json_bearer(&self, path: &str, bearer: &str) -> Result<Value, SwarmError> {
        self.send_get(path, Some(bearer)).await
    }

    async fn send_get(&self, path: &str, bearer: Option<&str>) -> Result<Value, SwarmError> {
        let mut request = self.client.get(self.url(path));
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        if let Some(token) = &self.demo_token {
            request = request.header("x-demo-token", token);
        }
        request
            .send()
            .await
            .map_err(transport_error)?
            .json()
            .await
            .map_err(protocol_error)
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn get_json(&self, path: &str) -> Result<Value, SwarmError> {
        self.send_get(path, None).await
    }

    async fn post_json(
        &self,
        path: &str,
        body: Value,
        bearer: Option<&str>,
        device_id: Option<&str>,
        forwarded_for: Option<&str>,
    ) -> Result<(u16, Value), SwarmError> {
        let mut request = self.client.post(self.url(path)).json(&body);
        if let Some(token) = bearer {
            request = request.header(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}"))
                    .map_err(|error| SwarmError::Transport(error.to_string()))?,
            );
        }
        if let Some(token) = &self.demo_token {
            request = request.header("x-demo-token", token);
        }
        if let Some(device) = device_id {
            request = request.header("x-device-id", device);
        }
        if let Some(source) = forwarded_for {
            request = request.header("x-forwarded-for", source);
        }
        let response = request.send().await.map_err(transport_error)?;
        let status = response.status().as_u16();
        let body = response.json().await.map_err(protocol_error)?;
        Ok((status, body))
    }
}

fn transport_error(error: reqwest::Error) -> SwarmError {
    SwarmError::Transport(error.to_string())
}

fn protocol_error(error: reqwest::Error) -> SwarmError {
    SwarmError::Protocol(error.to_string())
}

pub struct WsTransport {
    stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl WsTransport {
    pub async fn connect(url: &str) -> Result<Self, SwarmError> {
        let (stream, _) = match tokio_tungstenite::connect_async(url).await {
            Ok(connected) => connected,
            Err(error) => return Err(SwarmError::Transport(error.to_string())),
        };
        Ok(Self { stream })
    }

    pub async fn send_json(&mut self, value: &Value) -> Result<(), SwarmError> {
        let text = value.to_string();
        match self
            .stream
            .send(tokio_tungstenite::tungstenite::Message::Text(text))
            .await
        {
            Ok(()) => Ok(()),
            Err(error) => Err(SwarmError::Transport(error.to_string())),
        }
    }

    pub async fn next_json(&mut self) -> Result<Option<Value>, SwarmError> {
        let Some(message) = self.stream.next().await else {
            return Ok(None);
        };
        let message = match message {
            Ok(message) => message,
            Err(error) => return Err(SwarmError::Transport(error.to_string())),
        };
        if message.is_close() {
            return Ok(None);
        }
        let text = match message.into_text() {
            Ok(text) => text,
            Err(error) => return Err(SwarmError::Protocol(error.to_string())),
        };
        match serde_json::from_str(&text) {
            Ok(value) => Ok(Some(value)),
            Err(error) => Err(SwarmError::Protocol(error.to_string())),
        }
    }
}

#[derive(Debug, Deserialize)]
struct MarketDto {
    slug: String,
    state: String,
    price_yes_micro: i64,
    price_no_micro: i64,
}

#[derive(Debug, Serialize)]
struct PreviewTradeDto<'a> {
    user_id: &'a str,
    market_ref: &'a str,
    side: &'static str,
    action: &'static str,
    amount_micro: i64,
}

#[derive(Debug, Serialize)]
struct PlaceTradeDto<'a> {
    user_id: &'a str,
    market_ref: &'a str,
    side: &'static str,
    action: &'static str,
    amount_micro: i64,
    idempotency_key: String,
    expected_config_version: i64,
}

#[derive(Debug, Serialize)]
struct VoteDto<'a> {
    user_id: &'a str,
    market_ref: &'a str,
    side: &'static str,
    crowd_guess_pct: u8,
    idempotency_key: String,
}

/// D35 money path. The dest is derived from the agent so a release run
/// never shares a destination across users (D31 dest warmth).
#[derive(Debug, Serialize)]
struct WithdrawDto<'a> {
    user_id: &'a str,
    amount_micro: i64,
    dest: String,
    idempotency_key: String,
}

impl From<WithdrawDto<'_>> for Value {
    fn from(dto: WithdrawDto<'_>) -> Self {
        json!({
            "user_id": dto.user_id,
            "amount_micro": dto.amount_micro,
            "dest": dto.dest,
            "idempotency_key": dto.idempotency_key,
        })
    }
}

impl From<PreviewTradeDto<'_>> for Value {
    fn from(dto: PreviewTradeDto<'_>) -> Self {
        json!({
            "user_id": dto.user_id,
            "market_ref": dto.market_ref,
            "side": dto.side,
            "action": dto.action,
            "amount_micro": dto.amount_micro,
        })
    }
}

impl From<PlaceTradeDto<'_>> for Value {
    fn from(dto: PlaceTradeDto<'_>) -> Self {
        json!({
            "user_id": dto.user_id,
            "market_ref": dto.market_ref,
            "side": dto.side,
            "action": dto.action,
            "amount_micro": dto.amount_micro,
            "idempotency_key": dto.idempotency_key,
            "expected_config_version": dto.expected_config_version,
        })
    }
}

impl From<VoteDto<'_>> for Value {
    fn from(dto: VoteDto<'_>) -> Self {
        json!({
            "user_id": dto.user_id,
            "market_ref": dto.market_ref,
            "side": dto.side,
            "crowd_guess_pct": dto.crowd_guess_pct,
            "idempotency_key": dto.idempotency_key,
        })
    }
}

pub async fn fetch_market_view(
    transport: &dyn Transport,
    market_ref: &str,
    sim_tick: u64,
    closes_tick: u64,
) -> Result<MarketView, SwarmError> {
    let value = transport
        .get_json(&format!("/markets/{market_ref}"))
        .await?;
    let dto: MarketDto = serde_json::from_value(value.clone()).map_err(|error| {
        SwarmError::Protocol(format!(
            "invalid market response for {market_ref}: {error}; body={value}"
        ))
    })?;
    Ok(MarketView {
        market_ref: dto.slug,
        state: dto.state,
        price_yes_micro: dto.price_yes_micro,
        price_no_micro: dto.price_no_micro,
        sim_tick,
        closes_tick,
    })
}

pub async fn execute_decision(
    transport: &dyn Transport,
    agents: &[AgentSpec],
    market_ref: &str,
    local_seq: u64,
    decision: &Decision,
) -> Result<Vec<HttpOutcome>, SwarmError> {
    let by_id = agents
        .iter()
        .map(|agent| (agent.id.as_str(), agent))
        .collect::<BTreeMap<_, _>>();
    let mut outcomes = Vec::with_capacity(decision.actions.len());
    for (vector_index, action) in decision.actions.iter().enumerate() {
        let agent = by_id
            .get(action.agent_id.as_str())
            .ok_or_else(|| SwarmError::Protocol(format!("unknown agent {}", action.agent_id)))?;
        outcomes.push(
            execute_action(
                transport,
                agent,
                market_ref,
                local_seq,
                vector_index,
                action,
            )
            .await?,
        );
    }
    Ok(outcomes)
}

async fn execute_action(
    transport: &dyn Transport,
    agent: &AgentSpec,
    market_ref: &str,
    local_seq: u64,
    vector_index: usize,
    action: &AgentAction,
) -> Result<HttpOutcome, SwarmError> {
    match action.action {
        Action::Observe | Action::Abstain { .. } => Ok(HttpOutcome {
            status: 204,
            code: None,
        }),
        Action::Vote {
            side,
            crowd_guess_pct,
        } => {
            let dto = VoteDto {
                user_id: &agent.user_id,
                market_ref,
                side: side_wire(side),
                crowd_guess_pct,
                idempotency_key: request_key(agent, local_seq, vector_index, "vote"),
            };
            let body = Value::from(dto);
            let (status, body) = transport
                .post_json(
                    "/votes",
                    body,
                    None,
                    Some(&agent.device_id),
                    Some(&agent.forwarded_for),
                )
                .await?;
            Ok(http_outcome(status, &body))
        }
        Action::Trade {
            side,
            direction,
            amount_micro,
        } => {
            let dto = PreviewTradeDto {
                user_id: &agent.user_id,
                market_ref,
                side: side_wire(side),
                action: direction_wire(direction),
                amount_micro,
            };
            let preview = Value::from(dto);
            let (preview_status, preview_body) = transport
                .post_json(
                    "/trades/preview",
                    preview,
                    None,
                    Some(&agent.device_id),
                    Some(&agent.forwarded_for),
                )
                .await?;
            if !(200..300).contains(&preview_status) {
                return Ok(http_outcome(preview_status, &preview_body));
            }
            let Some(config_version) = preview_body.get("config_version").and_then(Value::as_i64)
            else {
                return Err(SwarmError::Protocol(
                    "preview omitted config_version".into(),
                ));
            };
            let dto = PlaceTradeDto {
                user_id: &agent.user_id,
                market_ref,
                side: side_wire(side),
                action: direction_wire(direction),
                amount_micro,
                idempotency_key: request_key(agent, local_seq, vector_index, "trade"),
                expected_config_version: config_version,
            };
            let place = Value::from(dto);
            let (status, body) = transport
                .post_json(
                    "/trades",
                    place,
                    None,
                    Some(&agent.device_id),
                    Some(&agent.forwarded_for),
                )
                .await?;
            Ok(http_outcome(status, &body))
        }
        Action::Withdraw { amount_micro } => {
            let dto = WithdrawDto {
                user_id: &agent.user_id,
                amount_micro,
                dest: withdraw_dest(agent),
                idempotency_key: request_key(agent, local_seq, vector_index, "withdraw"),
            };
            let body = Value::from(dto);
            let (status, body) = transport
                .post_json(
                    "/withdrawals",
                    body,
                    None,
                    Some(&agent.device_id),
                    Some(&agent.forwarded_for),
                )
                .await?;
            Ok(http_outcome(status, &body))
        }
    }
}

/// One pinned base58-shaped destination per agent: a dest shared by ≥2 users
/// is never warm (D31), so the release profile must not share one.
fn withdraw_dest(agent: &AgentSpec) -> String {
    format!("Dest{:0>39}", agent.index)
}

fn request_key(agent: &AgentSpec, local_seq: u64, vector_index: usize, kind: &str) -> String {
    format!("simswarm:{}:{local_seq}:{vector_index}:{kind}", agent.id)
}

fn http_outcome(status: u16, body: &Value) -> HttpOutcome {
    HttpOutcome {
        status,
        code: body.get("code").and_then(Value::as_str).map(str::to_owned),
    }
}

const fn side_wire(side: Side) -> &'static str {
    match side {
        Side::Yes => "yes",
        Side::No => "no",
    }
}

const fn direction_wire(direction: TradeDirection) -> &'static str {
    match direction {
        TradeDirection::Buy => "buy",
        TradeDirection::Sell => "sell",
    }
}

pub fn validate_openapi(document: &str) -> Result<(), SwarmError> {
    let root: Value = match serde_json::from_str(document) {
        Ok(root) => root,
        Err(error) => return Err(SwarmError::Protocol(error.to_string())),
    };
    let schemas = match root.pointer("/components/schemas") {
        Some(Value::Object(schemas)) => schemas,
        _ => return Err(SwarmError::Protocol("OpenAPI schemas missing".into())),
    };
    for (schema, required) in [
        (
            "PreviewTradeRequest",
            &["user_id", "market_ref", "side", "action", "amount_micro"][..],
        ),
        (
            "PlaceTradeRequest",
            &[
                "user_id",
                "market_ref",
                "side",
                "action",
                "amount_micro",
                "idempotency_key",
            ][..],
        ),
        (
            "CastVoteRequest",
            &[
                "user_id",
                "market_ref",
                "side",
                "crowd_guess_pct",
                "idempotency_key",
            ][..],
        ),
        ("InvariantReportDto", &["as_of", "pass", "identities"][..]),
    ] {
        let properties = match schemas.get(schema) {
            Some(Value::Object(schema_value)) => match schema_value.get("properties") {
                Some(Value::Object(properties)) => properties,
                _ => {
                    return Err(SwarmError::Protocol(format!(
                        "OpenAPI schema missing: {schema}"
                    )));
                }
            },
            _ => {
                return Err(SwarmError::Protocol(format!(
                    "OpenAPI schema missing: {schema}"
                )));
            }
        };
        for field in required {
            if !properties.contains_key(*field) {
                return Err(SwarmError::Protocol(format!(
                    "OpenAPI schema drift: {schema}"
                )));
            }
        }
        if schema == "PlaceTradeRequest" && !properties.contains_key("expected_config_version") {
            return Err(SwarmError::Protocol(
                "PlaceTradeRequest.expected_config_version missing".into(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::json;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    use super::*;
    use crate::domain::persona::Persona;

    type RecordedPost = (
        String,
        serde_json::Value,
        Option<String>,
        Option<String>,
        Option<String>,
    );

    #[derive(Default)]
    struct FakeTransport {
        gets: Mutex<Vec<String>>,
        posts: Mutex<Vec<RecordedPost>>,
        responses: Mutex<VecDeque<(u16, serde_json::Value)>>,
    }

    #[async_trait]
    impl Transport for FakeTransport {
        async fn get_json(&self, path: &str) -> Result<serde_json::Value, SwarmError> {
            self.gets.lock().unwrap().push(path.into());
            Ok(json!({
                "id":"00000000-0000-4000-8000-000000000001",
                "slug":"market", "question":"?", "state":"live",
                "yes_outcome_id":"00000000-0000-4000-8000-000000000002",
                "no_outcome_id":"00000000-0000-4000-8000-000000000003",
                "price_yes_micro":500000, "price_no_micro":500000,
                "closes_at":"2026-01-01T00:00:00Z", "tally_hidden_at":"2026-01-01T00:00:00Z",
                "under_review":false
            }))
        }

        async fn post_json(
            &self,
            path: &str,
            body: serde_json::Value,
            bearer: Option<&str>,
            device_id: Option<&str>,
            forwarded_for: Option<&str>,
        ) -> Result<(u16, serde_json::Value), SwarmError> {
            self.posts.lock().unwrap().push((
                path.into(),
                body,
                bearer.map(str::to_owned),
                device_id.map(str::to_owned),
                forwarded_for.map(str::to_owned),
            ));
            Ok(self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or((200, json!({}))))
        }
    }

    fn agent() -> AgentSpec {
        AgentSpec::synthetic(1, Persona::SmallDabbler, "market")
    }

    #[tokio::test]
    async fn public_api_builds_all_http_requests_here_and_preserves_vector_order() {
        let fake = FakeTransport::default();
        fake.responses.lock().unwrap().extend([
            (200, json!({"config_version":7})),
            (200, json!({"trade_id":"ok"})),
            (200, json!({"vote_id":"ok"})),
        ]);
        assert_eq!(
            fetch_market_view(&fake, "market", 3, 9)
                .await
                .unwrap()
                .sim_tick,
            3
        );
        let decision = Decision::new(vec![
            AgentAction::new(
                "agent-0001",
                Action::Trade {
                    side: Side::Yes,
                    direction: TradeDirection::Buy,
                    amount_micro: 50,
                },
            ),
            AgentAction::new(
                "agent-0001",
                Action::Vote {
                    side: Side::No,
                    crowd_guess_pct: 40,
                },
            ),
            AgentAction::new("agent-0001", Action::Observe),
            AgentAction::new(
                "agent-0001",
                Action::Abstain {
                    reason: "hold".into(),
                },
            ),
            AgentAction::new(
                "agent-0001",
                Action::Withdraw {
                    amount_micro: 5_000_000,
                },
            ),
        ]);
        let outcomes = execute_decision(&fake, &[agent()], "market", 2, &decision)
            .await
            .unwrap();
        assert_eq!(outcomes.len(), 5);
        let paths = fake
            .posts
            .lock()
            .unwrap()
            .iter()
            .map(|row| row.0.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            ["/trades/preview", "/trades", "/votes", "/withdrawals"]
        );
        assert_eq!(
            fake.posts.lock().unwrap()[2].3.as_deref(),
            Some("device-0001")
        );
        let withdraw_body = fake.posts.lock().unwrap()[3].1.clone();
        assert_eq!(withdraw_body["amount_micro"], json!(5_000_000));
        assert_eq!(withdraw_body["dest"], json!(withdraw_dest(&agent())));
        assert_eq!(withdraw_dest(&agent()).len(), 43);
        assert_ne!(
            withdraw_dest(&agent()),
            withdraw_dest(&AgentSpec::synthetic(
                2,
                crate::domain::persona::Persona::MoneyPath,
                "market"
            ))
        );
    }

    #[tokio::test]
    async fn public_api_classifies_protocol_and_missing_agent_errors() {
        let fake = FakeTransport::default();
        fake.responses.lock().unwrap().push_back((200, json!({})));
        let trade = Decision::new(vec![AgentAction::new(
            "agent-0001",
            Action::Trade {
                side: Side::Yes,
                direction: TradeDirection::Buy,
                amount_micro: 1,
            },
        )]);
        assert!(matches!(
            execute_decision(&fake, &[agent()], "m", 0, &trade).await,
            Err(SwarmError::Protocol(_))
        ));
        let unknown = Decision::new(vec![AgentAction::new("missing", Action::Observe)]);
        assert!(matches!(
            execute_decision(&fake, &[agent()], "m", 0, &unknown).await,
            Err(SwarmError::Protocol(_))
        ));
        let broken = BrokenGet;
        assert!(matches!(
            fetch_market_view(&broken, "m", 1, 2).await,
            Err(SwarmError::Protocol(_))
        ));
        assert!(matches!(
            broken.post_json("/x", json!({}), None, None, None).await,
            Err(SwarmError::Transport(_))
        ));

        let fake = FakeTransport::default();
        fake.responses
            .lock()
            .unwrap()
            .push_back((422, json!({"code":"blocked"})));
        assert_eq!(
            execute_decision(&fake, &[agent()], "m", 0, &trade)
                .await
                .unwrap(),
            vec![HttpOutcome {
                status: 422,
                code: Some("blocked".into())
            }]
        );
    }

    struct BrokenGet;

    #[async_trait]
    impl Transport for BrokenGet {
        async fn get_json(&self, _path: &str) -> Result<serde_json::Value, SwarmError> {
            Ok(json!({"slug":"m"}))
        }

        async fn post_json(
            &self,
            _path: &str,
            _body: serde_json::Value,
            _bearer: Option<&str>,
            _device_id: Option<&str>,
            _forwarded_for: Option<&str>,
        ) -> Result<(u16, serde_json::Value), SwarmError> {
            Err(SwarmError::Transport("broken".into()))
        }
    }

    async fn one_http_response(
        response: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let read = tokio::io::AsyncReadExt::read(&mut stream, &mut request)
                .await
                .unwrap();
            tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes())
                .await
                .unwrap();
            String::from_utf8(request[..read].to_vec()).unwrap()
        });
        (format!("http://{address}"), task)
    }

    #[tokio::test]
    async fn real_http_transport_handles_headers_status_json_and_failures() {
        let response = "HTTP/1.1 201 Created\r\ncontent-type: application/json\r\ncontent-length: 11\r\nconnection: close\r\n\r\n{\"ok\":true}";
        let (base, server) = one_http_response(response).await;
        let client = HttpTransport::new(&base)
            .unwrap()
            .with_demo_token("demo-token");
        let (status, body) = client
            .post_json(
                "/x",
                json!({"n":1}),
                Some("token"),
                Some("dev"),
                Some("10.0.0.1"),
            )
            .await
            .unwrap();
        assert_eq!(status, 201);
        assert_eq!(body, json!({"ok":true}));
        let request = server.await.unwrap().to_ascii_lowercase();
        assert!(request.contains("authorization: bearer token"));
        assert!(request.contains("x-demo-token: demo-token"));
        assert!(request.contains("x-device-id: dev"));
        assert!(request.contains("x-forwarded-for: 10.0.0.1"));

        let response = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\nconnection: close\r\n\r\n{\"ok\":true}";
        let (base, server) = one_http_response(response).await;
        assert_eq!(
            HttpTransport::new(&base)
                .unwrap()
                .with_demo_token("demo-token")
                .get_json_bearer("/x", "token")
                .await
                .unwrap(),
            json!({"ok":true})
        );
        let request = server.await.unwrap().to_ascii_lowercase();
        assert!(request.contains("authorization: bearer token"));
        assert!(request.contains("x-demo-token: demo-token"));

        let response = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\nconnection: close\r\n\r\n{\"ok\":true}";
        let (base, _server) = one_http_response(response).await;
        assert_eq!(
            HttpTransport::new(&base)
                .unwrap()
                .get_json("/x")
                .await
                .unwrap(),
            json!({"ok":true})
        );
        assert!(HttpTransport::new("::bad").is_err());
        let nested = HttpTransport::new("http://example.test/api").unwrap();
        assert_eq!(nested.url("/x").as_str(), "http://example.test/api/x");
        assert!(nested
            .post_json("/x", json!({}), Some("bad\nvalue"), None, None)
            .await
            .is_err());

        let response = "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 3\r\nconnection: close\r\n\r\nbad";
        let (base, _server) = one_http_response(response).await;
        assert!(HttpTransport::new(&base)
            .unwrap()
            .get_json("/x")
            .await
            .is_err());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let refused = listener.local_addr().unwrap();
        drop(listener);
        assert!(HttpTransport::new(&format!("http://{refused}"))
            .unwrap()
            .get_json("/x")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn real_websocket_transport_subscribes_and_receives() {
        assert!(WsTransport::connect("not a url").await.is_err());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let inbound = ws.next().await.unwrap().unwrap().into_text().unwrap();
            ws.send(tokio_tungstenite::tungstenite::Message::Text(
                json!({"type":"price","outbox_seq":9}).to_string(),
            ))
            .await
            .unwrap();
            inbound
        });
        let mut ws = WsTransport::connect(&format!("ws://{address}/ws"))
            .await
            .unwrap();
        ws.send_json(&json!({"op":"subscribe","market_id":"m"}))
            .await
            .unwrap();
        assert_eq!(ws.next_json().await.unwrap().unwrap()["outbox_seq"], 9);
        assert!(server.await.unwrap().contains("subscribe"));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            ws.close(None).await.unwrap();
        });
        let mut ws = WsTransport::connect(&format!("ws://{address}/ws"))
            .await
            .unwrap();
        assert_eq!(ws.next_json().await.unwrap(), None);
        assert_eq!(ws.next_json().await.unwrap(), None);
        assert!(ws.send_json(&json!({"after":"close"})).await.is_err());

        for message in [
            tokio_tungstenite::tungstenite::Message::Text("not-json".into()),
            tokio_tungstenite::tungstenite::Message::Binary(vec![0xff]),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = accept_async(stream).await.unwrap();
                socket.send(message).await.unwrap();
            });
            let mut ws = WsTransport::connect(&format!("ws://{address}/ws"))
                .await
                .unwrap();
            assert!(matches!(ws.next_json().await, Err(SwarmError::Protocol(_))));
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let socket = accept_async(stream).await.unwrap();
            drop(socket);
        });
        let mut ws = WsTransport::connect(&format!("ws://{address}/ws"))
            .await
            .unwrap();
        assert!(matches!(
            ws.next_json().await,
            Err(SwarmError::Transport(_))
        ));
    }

    #[test]
    fn local_dtos_match_the_openapi_contract() {
        validate_openapi(include_str!("../../../openapi.json")).unwrap();
        assert_eq!(side_wire(Side::Yes), "yes");
        assert_eq!(side_wire(Side::No), "no");
        assert_eq!(direction_wire(TradeDirection::Buy), "buy");
        assert_eq!(direction_wire(TradeDirection::Sell), "sell");

        let mut document: Value =
            serde_json::from_str(include_str!("../../../openapi.json")).unwrap();
        document["components"]["schemas"]["CastVoteRequest"]["properties"]
            .as_object_mut()
            .unwrap()
            .remove("user_id");
        assert!(validate_openapi(&serde_json::to_string(&document).unwrap()).is_err());

        let mut document: Value =
            serde_json::from_str(include_str!("../../../openapi.json")).unwrap();
        document["components"]["schemas"]["PlaceTradeRequest"]["properties"]
            .as_object_mut()
            .unwrap()
            .remove("expected_config_version");
        assert!(validate_openapi(&serde_json::to_string(&document).unwrap()).is_err());
        assert!(validate_openapi("{}").is_err());
        assert!(validate_openapi("not-json").is_err());

        let mut document: Value =
            serde_json::from_str(include_str!("../../../openapi.json")).unwrap();
        document["components"]["schemas"]
            .as_object_mut()
            .unwrap()
            .remove("CastVoteRequest");
        assert!(validate_openapi(&serde_json::to_string(&document).unwrap()).is_err());

        let mut document: Value =
            serde_json::from_str(include_str!("../../../openapi.json")).unwrap();
        document["components"]["schemas"]["CastVoteRequest"]
            .as_object_mut()
            .unwrap()
            .remove("properties");
        assert!(validate_openapi(&serde_json::to_string(&document).unwrap()).is_err());
    }
}
