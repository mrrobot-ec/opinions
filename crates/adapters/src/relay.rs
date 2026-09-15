//! Transactional-outbox relay. Publication is deliberately at-least-once:
//! broadcast precedes the `published_at` commit, so consumers dedupe by
//! `(outbox_seq, frame_type)`.

use std::num::NonZeroU64;
use std::sync::OnceLock;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use sqlx::{PgPool, Row};
use tokio::sync::broadcast;

// Kept under the D29-owned chaos path while registered from this W4-owned
// module so the wave does not mutate the frozen crate root.
#[path = "chaos/crash_point.rs"]
pub mod crash_point;

/// Startup-time parse failure for a chaos fault knob. The caller's D29 arm
/// validation decides whether knobs are permitted; this parser ensures a
/// permitted knob is still typed, non-zero, and never silently ignored.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FaultConfigError {
    #[error("invalid {name} value {value:?}; expected a non-zero integer")]
    InvalidValue { name: &'static str, value: String },
}

/// Redacted fault-set payload returned by `/healthz`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FaultDisclosure {
    pub active: Vec<&'static str>,
    pub relay_delay_ms: Option<u64>,
    pub ws_drop_every_n: Option<u64>,
}

/// Parse-once D29 fault injector. `Default` is the production Noop and is
/// observationally transparent. Each WebSocket receives its own counter via
/// [`Self::connection`], so concurrent connection scheduling cannot change
/// which frame number is dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FaultInjector {
    relay_delay_ms: Option<NonZeroU64>,
    ws_drop_every_n: Option<NonZeroU64>,
}

static PROCESS_FAULTS: OnceLock<Result<FaultInjector, FaultConfigError>> = OnceLock::new();

/// Returns the process-wide, parse-once fault configuration. `main` invokes
/// this during startup so malformed knobs fail before serving; HTTP health
/// and each WS connection then observe the identical immutable value.
///
/// # Errors
/// Returns the cached typed parse error for a malformed or zero knob.
pub fn process_faults() -> Result<&'static FaultInjector, &'static FaultConfigError> {
    faults_result(PROCESS_FAULTS.get_or_init(|| FaultInjector::parse(std::env::vars())))
}

fn faults_result(
    result: &'static Result<FaultInjector, FaultConfigError>,
) -> Result<&'static FaultInjector, &'static FaultConfigError> {
    match result {
        Ok(faults) => Ok(faults),
        Err(error) => Err(error),
    }
}

impl FaultInjector {
    /// Parses only the two adapter fault knobs; unrelated environment values
    /// are ignored. D29's two-factor arm is validated by composition before
    /// this constructor is called.
    ///
    /// # Errors
    /// Empty, non-numeric, or zero values fail closed.
    pub fn parse<I, K, V>(vars: I) -> Result<Self, FaultConfigError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut parsed = Self::default();
        for (name, value) in vars {
            let name = name.as_ref();
            let value = value.as_ref();
            match name {
                "CHAOS_RELAY_DELAY_MS" => {
                    parsed.relay_delay_ms = Some(parse_non_zero(name, value)?);
                }
                "CHAOS_WS_DROP_EVERY_N" => {
                    parsed.ws_drop_every_n = Some(parse_non_zero(name, value)?);
                }
                _ => {}
            }
        }
        Ok(parsed)
    }

    #[must_use]
    pub fn disclosure(&self) -> FaultDisclosure {
        let mut active = Vec::with_capacity(2);
        if self.relay_delay_ms.is_some() {
            active.push("relay_delay");
        }
        if self.ws_drop_every_n.is_some() {
            active.push("ws_drop");
        }
        FaultDisclosure {
            active,
            relay_delay_ms: self.relay_delay_ms.map(NonZeroU64::get),
            ws_drop_every_n: self.ws_drop_every_n.map(NonZeroU64::get),
        }
    }

    pub async fn before_relay_delivery(&self) {
        if let Some(delay) = self.relay_delay_ms {
            tokio::time::sleep(Duration::from_millis(delay.get())).await;
        }
    }

    #[must_use]
    pub const fn connection(&self) -> ConnectionFaultInjector {
        ConnectionFaultInjector {
            drop_every_n: self.ws_drop_every_n,
            emitted: 0,
        }
    }
}

fn parse_non_zero(name: &str, value: &str) -> Result<NonZeroU64, FaultConfigError> {
    value
        .parse::<NonZeroU64>()
        .map_err(|_| FaultConfigError::InvalidValue {
            name: match name {
                "CHAOS_RELAY_DELAY_MS" => "CHAOS_RELAY_DELAY_MS",
                _ => "CHAOS_WS_DROP_EVERY_N",
            },
            value: value.to_string(),
        })
}

/// Per-connection deterministic outbound frame counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionFaultInjector {
    drop_every_n: Option<NonZeroU64>,
    emitted: u64,
}

impl ConnectionFaultInjector {
    /// Advances exactly once per attempted outbound frame and says whether
    /// this frame should be dropped.
    pub fn should_drop(&mut self) -> bool {
        self.emitted = self.emitted.wrapping_add(1);
        self.drop_every_n
            .is_some_and(|every| self.emitted.is_multiple_of(every.get()))
    }
}

/// One committed outbox row delivered to the in-process WS fanout.
#[derive(Debug, Clone, PartialEq)]
pub struct WireEvent {
    pub outbox_seq: i64,
    pub event_type: String,
    pub aggregate_type: String,
    pub aggregate_id: uuid::Uuid,
    pub payload: Value,
}

/// Stable notification payload published only after its row commits.
#[derive(Debug, Clone, PartialEq)]
pub struct UserNotifFrame {
    pub id: i64,
    pub source_seq: i64,
    pub notification_type: String,
    pub payload: Value,
}

/// Typed in-process bus prevents market payloads from leaking to user channels.
#[derive(Debug, Clone, PartialEq)]
pub enum BusEvent {
    Market(WireEvent),
    UserNotif {
        user_id: uuid::Uuid,
        frame: UserNotifFrame,
    },
}

#[derive(Clone)]
pub struct OutboxRelay {
    pool: PgPool,
    tx: broadcast::Sender<BusEvent>,
    faults: FaultInjector,
}

impl OutboxRelay {
    #[must_use]
    pub fn new(pool: PgPool, tx: broadcast::Sender<BusEvent>) -> Self {
        Self::with_faults(pool, tx, FaultInjector::default())
    }

    #[must_use]
    pub const fn with_faults(
        pool: PgPool,
        tx: broadcast::Sender<BusEvent>,
        faults: FaultInjector,
    ) -> Self {
        Self { pool, tx, faults }
    }

    /// Claims at most 128 rows, broadcasts them, then marks them published in
    /// the same database transaction. A mark/commit failure causes a safe
    /// duplicate on retry; no active broadcast receivers is still success.
    pub async fn pump_once(&self) -> Result<usize, sqlx::Error> {
        self.pump_once_inner(false).await
    }

    async fn pump_once_inner(&self, rollback_after_broadcast: bool) -> Result<usize, sqlx::Error> {
        let mut db = self.pool.begin().await?;
        let rows = sqlx::query(
            r#"
            select seq, event_type, aggregate_type, aggregate_id, payload
              from events_outbox
             where published_at is null
             order by seq
             limit 128
             for update skip locked
            "#,
        )
        .fetch_all(&mut *db)
        .await?;
        let mut sequences = Vec::with_capacity(rows.len());
        for row in &rows {
            let event = WireEvent {
                outbox_seq: row.try_get("seq")?,
                event_type: row.try_get("event_type")?,
                aggregate_type: row.try_get("aggregate_type")?,
                aggregate_id: row.try_get("aggregate_id")?,
                payload: row.try_get("payload")?,
            };
            sequences.push(event.outbox_seq);
            self.faults.before_relay_delivery().await;
            let _ = self.tx.send(BusEvent::Market(event));
        }
        if rollback_after_broadcast && !sequences.is_empty() {
            return Err(sqlx::Error::Protocol(
                "injected rollback after broadcast".to_string(),
            ));
        }
        if !sequences.is_empty() {
            sqlx::query("update events_outbox set published_at = now() where seq = any($1)")
                .bind(&sequences)
                .execute(&mut *db)
                .await?;
        }
        db.commit().await?;
        Ok(sequences.len())
    }

    /// Runs the singleton relay until its task is cancelled.
    pub async fn run(self, tick: std::time::Duration) {
        let mut interval = tokio::time::interval(tick);
        loop {
            interval.tick().await;
            if let Err(error) = self.pump_once().await {
                eprintln!("outbox relay error: {error}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::HashSet;
    use std::str::FromStr;

    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

    use super::*;

    static INVALID_FAULTS: Result<FaultInjector, FaultConfigError> =
        Err(FaultConfigError::InvalidValue {
            name: "CHAOS_RELAY_DELAY_MS",
            value: String::new(),
        });

    static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

    async fn isolated_pool() -> (PgPool, PgPool, String) {
        let url = std::env::var("DATABASE_URL").unwrap();
        let control = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .unwrap();
        let database = format!("opinions_relay_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("create database {database}")))
            .execute(&control)
            .await
            .unwrap();
        let options = PgConnectOptions::from_str(&url)
            .unwrap()
            .database(&database);
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        (pool, control, database)
    }

    async fn drop_database(pool: PgPool, control: PgPool, database: &str) {
        pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "drop database {database} with (force)"
        )))
        .execute(&control)
        .await
        .unwrap();
        control.close().await;
    }

    async fn append(pool: &PgPool, event_type: &str) -> i64 {
        sqlx::query_scalar(
            r#"
            insert into events_outbox (aggregate_type, aggregate_id, event_type, payload)
            values ('market', $1, $2, '{}') returning seq
            "#,
        )
        .bind(uuid::Uuid::new_v4())
        .bind(event_type)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[test]
    fn wire_event_is_cloneable_for_broadcast() {
        let event = WireEvent {
            outbox_seq: 7,
            event_type: "TradePlaced".into(),
            aggregate_type: "market".into(),
            aggregate_id: uuid::Uuid::nil(),
            payload: serde_json::json!({"trade_seq": 2}),
        };
        assert_eq!(event.clone(), event);
    }

    #[test]
    fn process_fault_result_preserves_a_typed_startup_error() {
        assert!(matches!(
            faults_result(&INVALID_FAULTS),
            Err(FaultConfigError::InvalidValue {
                name: "CHAOS_RELAY_DELAY_MS",
                value,
            }) if value.is_empty()
        ));
    }

    #[tokio::test]
    async fn pump_marks_rows_once_even_without_subscribers() {
        let (pool, control, database) = isolated_pool().await;
        let seq = append(&pool, "MarketSeeded").await;
        let (tx, _) = broadcast::channel(4);
        let relay = OutboxRelay::new(pool.clone(), tx);
        assert_eq!(relay.pump_once().await.unwrap(), 1);
        assert_eq!(relay.pump_once().await.unwrap(), 0);
        let marked: bool =
            sqlx::query_scalar("select published_at is not null from events_outbox where seq = $1")
                .bind(seq)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(marked);
        drop(relay);
        drop_database(pool, control, &database).await;
    }

    #[tokio::test]
    async fn rollback_after_broadcast_retries_the_same_sequence() {
        let (pool, control, database) = isolated_pool().await;
        let seq = append(&pool, "VoteCast").await;
        let (tx, mut receiver) = broadcast::channel(4);
        let relay = OutboxRelay::new(pool.clone(), tx);
        assert!(relay.pump_once_inner(true).await.is_err());
        let expected = WireEvent {
            outbox_seq: seq,
            event_type: "VoteCast".to_string(),
            aggregate_type: "market".to_string(),
            aggregate_id: sqlx::query_scalar("select aggregate_id from events_outbox where seq=$1")
                .bind(seq)
                .fetch_one(&pool)
                .await
                .unwrap(),
            payload: serde_json::json!({}),
        };
        assert_eq!(
            receiver.recv().await.unwrap(),
            BusEvent::Market(expected.clone())
        );
        assert_eq!(relay.pump_once().await.unwrap(), 1);
        assert_eq!(
            receiver.recv().await.unwrap(),
            BusEvent::Market(expected.clone())
        );
        let mut dedupe = HashSet::new();
        assert!(dedupe.insert((expected.outbox_seq, expected.event_type.clone())));
        assert!(!dedupe.insert((expected.outbox_seq, expected.event_type)));
        drop(relay);
        drop(receiver);
        drop_database(pool, control, &database).await;
    }

    #[tokio::test]
    async fn relay_supervisor_keeps_running_after_a_pump_error() {
        let (pool, control, database) = isolated_pool().await;
        pool.close().await;
        let (tx, _) = broadcast::channel(4);
        let relay = OutboxRelay::new(pool.clone(), tx);
        let task = tokio::spawn(relay.run(std::time::Duration::from_millis(1)));
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(
            !task.is_finished(),
            "relay must survive transient database errors"
        );
        task.abort();
        let aborted = task.await.unwrap_err();
        assert!(aborted.is_cancelled());
        drop_database(pool, control, &database).await;
    }

    #[tokio::test]
    async fn noop_fault_injector_is_transparent_and_discloses_empty_set() {
        let faults = FaultInjector::default();
        assert!(process_faults().is_ok());
        assert_eq!(
            faults.disclosure(),
            FaultDisclosure {
                active: Vec::new(),
                relay_delay_ms: None,
                ws_drop_every_n: None,
            }
        );
        faults.before_relay_delivery().await;
        let mut connection = faults.connection();
        assert!(!(0..20).any(|_| connection.should_drop()));
    }

    #[tokio::test]
    async fn fault_config_is_parse_once_typed_and_disclosed() {
        let faults = FaultInjector::parse([
            ("CHAOS_RELAY_DELAY_MS", "1"),
            ("CHAOS_WS_DROP_EVERY_N", "3"),
            ("IGNORED", "anything"),
        ])
        .unwrap();
        assert_eq!(
            faults.disclosure(),
            FaultDisclosure {
                active: vec!["relay_delay", "ws_drop"],
                relay_delay_ms: Some(1),
                ws_drop_every_n: Some(3),
            }
        );
        faults.before_relay_delivery().await;

        let mut first = faults.connection();
        let mut second = faults.connection();
        assert_eq!(
            (0..6).map(|_| first.should_drop()).collect::<Vec<_>>(),
            [false, false, true, false, false, true]
        );
        assert_eq!(
            (0..3).map(|_| second.should_drop()).collect::<Vec<_>>(),
            [false, false, true]
        );
    }

    #[test]
    fn malformed_or_zero_fault_values_fail_closed() {
        for (name, value) in [
            ("CHAOS_RELAY_DELAY_MS", ""),
            ("CHAOS_RELAY_DELAY_MS", "wat"),
            ("CHAOS_RELAY_DELAY_MS", "0"),
            ("CHAOS_WS_DROP_EVERY_N", "wat"),
            ("CHAOS_WS_DROP_EVERY_N", "0"),
        ] {
            let error = FaultInjector::parse([(name, value)]).unwrap_err();
            assert!(error.to_string().contains(name));
        }
    }
}
