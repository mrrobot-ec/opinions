//! Postgres `AlertStore` — D35's durable alert outbox (W4).
//!
//! The incident key is three parts (`detector:subject:episode`) stored in one
//! text column. Only the first two separators are structural: a D35 episode
//! legally contains a colon (`residual:<n>` from the reconciliation detector),
//! so encoding joins on `:` and decoding splits with `splitn(3, ':')`.
//!
//! `last_paged_at` is a column of its own and never an alias of `updated_at`:
//! `pending_delivery` is `status in ('open','acked') and last_paged_at is
//! null`, while `ack`/`resolve` save without paging. Aliasing the two would let
//! an operator ack retire an undelivered incident from the at-least-once queue,
//! which is also why `acked` is INSIDE the queue rather than outside it: an ack
//! means "seen in the console", which is not the page the incident is still
//! owed. Only `resolved` leaves, and it leaves because the condition is over.
//!
//! Dedup is structural, not advisory: `alert_outbox_one_open_per_key`
//! (migration 0012) is a PARTIAL unique index over `('open','acked')`, so
//! `insert_if_absent` is atomic across connections while a resolved episode
//! still leaves the key free for D35's recurrence-after-recovery re-page.

use async_trait::async_trait;
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};

use application::error::StoreError;
use application::ops::alerts::{AlertStore, Incident, IncidentKey, IncidentStatus};

use super::rows::db_error;
use super::store::PgStore;

/// Pool wrapper; does not edit frozen `PgStore` factories.
#[derive(Clone)]
pub struct PgAlertStore {
    pool: PgPool,
}

impl PgAlertStore {
    #[must_use]
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    #[must_use]
    pub fn from_store(store: &PgStore) -> Self {
        Self {
            pool: store.pool_handle().clone(),
        }
    }
}

const fn status_name(status: IncidentStatus) -> &'static str {
    match status {
        IncidentStatus::Open => "open",
        IncidentStatus::Acked => "acked",
        IncidentStatus::Resolved => "resolved",
    }
}

/// The CHECK constraint on `alert_outbox.status` makes any other value
/// unreachable, so an unknown one is a store invariant violation.
fn parse_status(raw: &str) -> Result<IncidentStatus, StoreError> {
    match raw {
        "open" => Ok(IncidentStatus::Open),
        "acked" => Ok(IncidentStatus::Acked),
        "resolved" => Ok(IncidentStatus::Resolved),
        _ => Err(StoreError::Invariant("unknown alert incident status")),
    }
}

/// Splits into exactly three parts so an episode may contain `:`.
fn decode_key(encoded: &str) -> Result<IncidentKey, StoreError> {
    let mut parts = encoded.splitn(3, ':');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(detector), Some(subject), Some(episode)) => {
            Ok(IncidentKey::new(detector, subject, episode))
        }
        _ => Err(StoreError::Invariant("malformed alert incident key")),
    }
}

fn incident_from_row(row: &PgRow) -> Result<Incident, StoreError> {
    let attempts: i32 = row.try_get("delivery_attempts").map_err(db_error)?;
    Ok(Incident {
        id: row.try_get("id").map_err(db_error)?,
        key: decode_key(
            row.try_get::<String, _>("incident_key")
                .map_err(db_error)?
                .as_str(),
        )?,
        severity: row.try_get("severity").map_err(db_error)?,
        body: row.try_get("body").map_err(db_error)?,
        status: parse_status(
            row.try_get::<String, _>("status")
                .map_err(db_error)?
                .as_str(),
        )?,
        delivery_attempts: u32::try_from(attempts).unwrap_or(0),
        last_paged_at: row.try_get("last_paged_at").map_err(db_error)?,
        acked_at: row.try_get("acked_at").map_err(db_error)?,
        resolved_at: row.try_get("resolved_at").map_err(db_error)?,
    })
}

const SELECT_COLUMNS: &str = "id, incident_key, severity, body, status, \
     delivery_attempts, last_paged_at, acked_at, resolved_at";

#[async_trait]
impl AlertStore for PgAlertStore {
    /// `acked` still dedups a recurrence; `resolved` never does, so a
    /// recurrence after recovery opens a new episode row (D35).
    async fn find_open(&self, key: &IncidentKey) -> Result<Option<Incident>, StoreError> {
        let sql = format!(
            "select {SELECT_COLUMNS} from alert_outbox \
              where incident_key = $1 and status in ('open','acked') \
              order by created_at desc limit 1"
        );
        let row = sqlx::query(&sql)
            .bind(key.encoded())
            .fetch_optional(&self.pool)
            .await
            .map_err(db_error)?;
        row.as_ref().map(incident_from_row).transpose()
    }

    async fn insert(&self, incident: Incident) -> Result<(), StoreError> {
        sqlx::query(
            "insert into alert_outbox \
               (id, incident_key, severity, body, status, delivery_attempts, \
                last_paged_at, acked_at, resolved_at) \
             values ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
        )
        .bind(incident.id)
        .bind(incident.key.encoded())
        .bind(&incident.severity)
        .bind(&incident.body)
        .bind(status_name(incident.status))
        .bind(i32::try_from(incident.delivery_attempts).unwrap_or(i32::MAX))
        .bind(incident.last_paged_at)
        .bind(incident.acked_at)
        .bind(incident.resolved_at)
        .execute(&self.pool)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    /// Atomic open-or-get. The atomicity is the partial unique index
    /// `alert_outbox_one_open_per_key` (migration 0012), NOT this function:
    /// `find_open` and `insert` are separate autocommit statements on separate
    /// pooled connections, so a read-then-insert would let two concurrent
    /// raisers both see "absent", open two incidents and page twice — against
    /// D35's normative "dedup within an open incident".
    ///
    /// The index is deliberately PARTIAL over `('open','acked')`. A resolved
    /// episode is kept as its own row so a recurrence after recovery re-pages,
    /// so a total `unique (incident_key)` would forbid exactly the behaviour
    /// D35 requires.
    ///
    /// The loser re-reads rather than trusting `RETURNING`: `on conflict do
    /// nothing` returns no row, and the winner's incident is what the caller
    /// must return so it knows not to page. A conflict with no readable winner
    /// means the winner resolved in between, which is a genuine race the caller
    /// should retry rather than a state to invent.
    async fn insert_if_absent(&self, incident: Incident) -> Result<Option<Incident>, StoreError> {
        let inserted = sqlx::query(
            "insert into alert_outbox \
               (id, incident_key, severity, body, status, delivery_attempts, \
                last_paged_at, acked_at, resolved_at) \
             values ($1,$2,$3,$4,$5,$6,$7,$8,$9) \
             on conflict (incident_key) where status in ('open','acked') do nothing",
        )
        .bind(incident.id)
        .bind(incident.key.encoded())
        .bind(&incident.severity)
        .bind(&incident.body)
        .bind(status_name(incident.status))
        .bind(i32::try_from(incident.delivery_attempts).unwrap_or(i32::MAX))
        .bind(incident.last_paged_at)
        .bind(incident.acked_at)
        .bind(incident.resolved_at)
        .execute(&self.pool)
        .await
        .map_err(db_error)?;
        if inserted.rows_affected() == 1 {
            return Ok(None);
        }
        self.find_open(&incident.key)
            .await?
            .map(Some)
            .ok_or(StoreError::Conflict("alert incident raised concurrently"))
    }

    /// Updates in place by id. The incident key is immutable, so it is not in
    /// the SET list — a save can never re-key an episode.
    async fn save(&self, incident: &Incident) -> Result<(), StoreError> {
        let result = sqlx::query(
            "update alert_outbox \
                set severity = $2, body = $3, status = $4, delivery_attempts = $5, \
                    last_paged_at = $6, acked_at = $7, resolved_at = $8, updated_at = now() \
              where id = $1",
        )
        .bind(incident.id)
        .bind(&incident.severity)
        .bind(&incident.body)
        .bind(status_name(incident.status))
        .bind(i32::try_from(incident.delivery_attempts).unwrap_or(i32::MAX))
        .bind(incident.last_paged_at)
        .bind(incident.acked_at)
        .bind(incident.resolved_at)
        .execute(&self.pool)
        .await
        .map_err(db_error)?;
        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound("alert incident"));
        }
        Ok(())
    }

    /// At-least-once: every live incident that never observed a page. Oldest
    /// first, so a backlog drains in the order it was raised.
    ///
    /// `acked` is INSIDE the queue, not outside it: an ack is an operator
    /// saying "seen in the console", which is not the page this incident is
    /// still owed. Restricting the queue to `status = 'open'` would let an ack
    /// retire an undelivered incident — the exact aliasing `last_paged_at`
    /// exists to prevent. Only `resolved` leaves the queue, and it leaves
    /// because the condition is over.
    async fn pending_delivery(&self) -> Result<Vec<Incident>, StoreError> {
        let sql = format!(
            "select {SELECT_COLUMNS} from alert_outbox \
              where status in ('open','acked') and last_paged_at is null \
              order by created_at, id"
        );
        let rows = sqlx::query(&sql)
            .fetch_all(&self.pool)
            .await
            .map_err(db_error)?;
        rows.iter().map(incident_from_row).collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    async fn incident_row(
        pool: &PgPool,
        incident_key: &str,
        status: &str,
    ) -> Result<PgRow, sqlx::Error> {
        sqlx::query(
            r#"
            select $1::uuid as id, $2::text as incident_key, 'critical'::text as severity,
                   'decoder contract'::text as body, $3::text as status,
                   1::int as delivery_attempts, null::timestamptz as last_paged_at,
                   null::timestamptz as acked_at, null::timestamptz as resolved_at
            "#,
        )
        .bind(uuid::Uuid::new_v4())
        .bind(incident_key)
        .bind(status)
        .fetch_one(pool)
        .await
    }

    #[test]
    fn key_and_status_codecs_are_total_and_reject_impossible_rows() {
        for status in [
            IncidentStatus::Open,
            IncidentStatus::Acked,
            IncidentStatus::Resolved,
        ] {
            assert_eq!(parse_status(status_name(status)).unwrap(), status);
        }
        assert!(matches!(
            parse_status("exploded"),
            Err(StoreError::Invariant("unknown alert incident status"))
        ));

        // An episode legally carries the separator; only the first two
        // colons are structural.
        let key = IncidentKey::new("reconciliation_residual", "cut-1", "residual:-1");
        assert_eq!(decode_key(&key.encoded()).unwrap(), key);
        let plain = IncidentKey::new("invariant_breach", "suite", "ep-1");
        assert_eq!(decode_key(&plain.encoded()).unwrap(), plain);
        assert!(matches!(
            decode_key("invariant_breach:suite"),
            Err(StoreError::Invariant("malformed alert incident key"))
        ));
        assert!(matches!(
            decode_key("bare"),
            Err(StoreError::Invariant("malformed alert incident key"))
        ));
        assert!(SELECT_COLUMNS.contains("last_paged_at"));
    }

    #[tokio::test]
    async fn incident_rows_propagate_key_and_status_corruption(
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let url = std::env::var("DATABASE_URL")?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await?;

        let malformed_key = incident_row(&pool, "missing-separators", "open").await?;
        assert_eq!(
            incident_from_row(&malformed_key),
            Err(StoreError::Invariant("malformed alert incident key"))
        );
        let valid_key = IncidentKey::new("invariant_breach", "cut-1", "episode-1").encoded();
        let malformed_status = incident_row(&pool, &valid_key, "forgotten").await?;
        assert_eq!(
            incident_from_row(&malformed_status),
            Err(StoreError::Invariant("unknown alert incident status"))
        );

        pool.close().await;
        Ok(())
    }
}
