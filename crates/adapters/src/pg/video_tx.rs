//! PostgreSQL `VideoTx`: leased artifact jobs + shared moderation-job
//! machinery (Task 5.2).
//!
//! Claim/reclaim use `FOR UPDATE SKIP LOCKED` inside a SHORT transaction the
//! worker commits before rendering; completions (success AND error) are
//! token-fenced CAS updates on `(id, claim_token, status)`, so a stale
//! attempt updates zero rows and is dropped silently. `updated_at` is driven
//! by the caller's logical clock wherever a signature carries one — the
//! renderer side never reads a wall clock.

use application::error::StoreError;
use application::model::{
    ArtifactKind, CommentId, DraftId, JobId, JobStatus, MarketId, MarketRow, ModerationJobRow,
    ModerationJobStatus, OutboxEvent, RealizationFact, RealizationSource, UserId, VideoJobRow,
};
use application::ports::VideoTx;
use application::video::artifacts::kind_name;
use async_trait::async_trait;
use domain::money::MicroUsd;
use sqlx::postgres::PgRow;
use sqlx::Row;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use super::rows::{db_error, market_from_row};
use super::store::{PgStore, PgTx};

pub(super) async fn open(store: &PgStore) -> Result<Box<dyn VideoTx + '_>, StoreError> {
    Ok(Box::new(store.tx().await?))
}

fn parse_status(status: &str) -> Result<JobStatus, StoreError> {
    match status {
        "queued" => Ok(JobStatus::Queued),
        "rendering" => Ok(JobStatus::Rendering),
        "ready" => Ok(JobStatus::Ready),
        "attached" => Ok(JobStatus::Attached),
        "failed" => Ok(JobStatus::Failed),
        _ => Err(StoreError::Invariant("unknown video job status")),
    }
}

fn parse_kind(kind: &str) -> Result<ArtifactKind, StoreError> {
    match kind {
        "market_video" => Ok(ArtifactKind::MarketVideo),
        "poster" => Ok(ArtifactKind::Poster),
        _ => Err(StoreError::Invariant("unknown artifact kind")),
    }
}

/// Legacy 0001 column: still NOT NULL, so new rows carry a stable mapping.
fn legacy_tier(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::MarketVideo => "hero",
        ArtifactKind::Poster => "template",
    }
}

fn attempts_from(row: &PgRow) -> Result<u32, StoreError> {
    let attempts: i32 = row.try_get("attempts").map_err(db_error)?;
    u32::try_from(attempts).map_err(|_| StoreError::Invariant("negative attempts"))
}

fn job_from_row(row: &PgRow) -> Result<VideoJobRow, StoreError> {
    let status: String = row.try_get("status").map_err(db_error)?;
    let kind: String = row.try_get("kind").map_err(db_error)?;
    Ok(VideoJobRow {
        id: JobId(row.try_get("id").map_err(db_error)?),
        market: MarketId(row.try_get("market_id").map_err(db_error)?),
        draft: row
            .try_get::<Option<Uuid>, _>("draft_id")
            .map_err(db_error)?
            .map(DraftId),
        kind: parse_kind(&kind)?,
        status: parse_status(&status)?,
        asset_url: row.try_get("asset_url").map_err(db_error)?,
        available_at: row.try_get("available_at").map_err(db_error)?,
        claim_token: row.try_get("claim_token").map_err(db_error)?,
        lease_expires_at: row.try_get("lease_expires_at").map_err(db_error)?,
        attempts: attempts_from(row)?,
    })
}

fn moderation_status_name(status: ModerationJobStatus) -> &'static str {
    match status {
        ModerationJobStatus::Queued => "queued",
        ModerationJobStatus::Running => "running",
        ModerationJobStatus::Done => "done",
        ModerationJobStatus::Failed => "failed",
    }
}

fn parse_moderation_status(status: &str) -> Result<ModerationJobStatus, StoreError> {
    match status {
        "queued" => Ok(ModerationJobStatus::Queued),
        "running" => Ok(ModerationJobStatus::Running),
        "done" => Ok(ModerationJobStatus::Done),
        "failed" => Ok(ModerationJobStatus::Failed),
        _ => Err(StoreError::Invariant("unknown moderation job status")),
    }
}

fn moderation_from_row(row: &PgRow) -> Result<ModerationJobRow, StoreError> {
    let status: String = row.try_get("status").map_err(db_error)?;
    Ok(ModerationJobRow {
        id: JobId(row.try_get("id").map_err(db_error)?),
        comment: CommentId(row.try_get("comment_id").map_err(db_error)?),
        status: parse_moderation_status(&status)?,
        available_at: row.try_get("available_at").map_err(db_error)?,
        claim_token: row.try_get("claim_token").map_err(db_error)?,
        lease_expires_at: row.try_get("lease_expires_at").map_err(db_error)?,
        attempts: attempts_from(row)?,
    })
}

fn sort_moderation_jobs(jobs: &mut [ModerationJobRow]) {
    jobs.sort_by_key(|job| (job.available_at, job.id.0));
}

fn parse_source(source: &str) -> Result<RealizationSource, StoreError> {
    match source {
        "sell" => Ok(RealizationSource::Sell),
        "settlement" => Ok(RealizationSource::Settlement),
        "void" => Ok(RealizationSource::Void),
        _ => Err(StoreError::Invariant("unknown realization source")),
    }
}

const JOB_COLUMNS: &str = "id, market_id, draft_id, kind, status, asset_url, available_at, \
                           claim_token, lease_expires_at, attempts";

const MODERATION_COLUMNS: &str =
    "id, comment_id, status, available_at, claim_token, lease_expires_at, attempts";

const MARKET_ROW: &str = r"
    select m.id, m.slug, m.question, m.status, m.min_votes_to_resolve, m.opens_at,
           m.closes_at, m.tally_hidden_at, m.curator_flagged_at, m.integrity_due_at,
           m.poster_asset_url, m.video_asset_url,
           yes_outcome.id as yes_outcome, no_outcome.id as no_outcome
      from markets m
      join outcomes yes_outcome on yes_outcome.market_id = m.id and yes_outcome.idx = 0
      join outcomes no_outcome on no_outcome.market_id = m.id and no_outcome.idx = 1
     where m.id = $1
";

impl PgTx {
    async fn job_by(
        &mut self,
        clause: &'static str,
        id: Uuid,
        kind: ArtifactKind,
    ) -> Result<Option<VideoJobRow>, StoreError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "select {JOB_COLUMNS} from video_jobs where {clause}"
        )))
        .bind(id)
        .bind(kind_name(kind))
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        row.as_ref().map(job_from_row).transpose()
    }
}

#[async_trait]
impl VideoTx for PgTx {
    async fn enqueue(
        &mut self,
        market: MarketId,
        draft: Option<DraftId>,
        kind: ArtifactKind,
        now: OffsetDateTime,
    ) -> Result<JobId, StoreError> {
        // Durable issuance key first: one canonical job per (draft, kind)
        // across ALL statuses (codex P5R2 N2).
        if let Some(draft) = draft {
            if let Some(job) = self
                .job_by("draft_id = $1 and kind = $2", draft.0, kind)
                .await?
            {
                return Ok(job.id);
            }
        }
        // Then the active (market, kind) unique (codex B4).
        if let Some(job) = self
            .job_by(
                "market_id = $1 and kind = $2 and status in ('queued','rendering','ready')",
                market.0,
                kind,
            )
            .await?
        {
            return Ok(job.id);
        }
        let market_exists = sqlx::query("select 1 from markets where id = $1")
            .bind(market.0)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)?
            .is_some();
        if !market_exists {
            return Err(StoreError::NotFound("market"));
        }
        // ON CONFLICT DO NOTHING keeps the transaction alive when a racing
        // enqueue commits first; the re-reads below then return its row.
        let inserted = sqlx::query(
            r"insert into video_jobs
                  (market_id, tier, status, kind, draft_id, available_at, created_at, updated_at)
              values ($1, $2, 'queued', $3, $4, $5, $5, $5)
              on conflict do nothing
              returning id",
        )
        .bind(market.0)
        .bind(legacy_tier(kind))
        .bind(kind_name(kind))
        .bind(draft.map(|d| d.0))
        .bind(now)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        if let Some(row) = inserted {
            return Ok(JobId(row.try_get("id").map_err(db_error)?));
        }
        if let Some(draft) = draft {
            if let Some(job) = self
                .job_by("draft_id = $1 and kind = $2", draft.0, kind)
                .await?
            {
                return Ok(job.id);
            }
        }
        self.job_by(
            "market_id = $1 and kind = $2 and status in ('queued','rendering','ready')",
            market.0,
            kind,
        )
        .await?
        .map(|job| job.id)
        .ok_or(StoreError::Invariant("enqueue lost every unique race"))
    }

    async fn claim(
        &mut self,
        now: OffsetDateTime,
        lease: Duration,
        limit: u32,
    ) -> Result<Vec<VideoJobRow>, StoreError> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            r"update video_jobs
                 set status = 'rendering', claim_token = gen_random_uuid(),
                     lease_expires_at = $1, attempts = attempts + 1, updated_at = $2
               where id in (
                   select id from video_jobs
                    where status = 'queued' and available_at <= $2
                    order by available_at, updated_at, id
                    limit $3
                    for update skip locked)
              returning {JOB_COLUMNS}",
        )))
        .bind(now + lease)
        .bind(now)
        .bind(i64::from(limit))
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        let mut jobs = rows
            .iter()
            .map(job_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        jobs.sort_by_key(|job| (job.available_at, job.id.0));
        Ok(jobs)
    }

    async fn complete_ready(
        &mut self,
        job: JobId,
        token: Uuid,
        asset_url: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            r"update video_jobs
                 set status = 'ready', asset_url = $3, available_at = $4,
                     claim_token = null, lease_expires_at = null, error = null, updated_at = $4
               where id = $1 and claim_token = $2 and status = 'rendering'",
        )
        .bind(job.0)
        .bind(token)
        .bind(asset_url)
        .bind(now)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(result.rows_affected() == 1)
    }

    async fn complete_error(
        &mut self,
        job: JobId,
        token: Uuid,
        error: &str,
        available_at: OffsetDateTime,
        terminal: bool,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            r"update video_jobs
                 set status = case when $5 then 'failed' else 'queued' end,
                     error = $3, available_at = $4,
                     claim_token = null, lease_expires_at = null, updated_at = $4
               where id = $1 and claim_token = $2 and status = 'rendering'",
        )
        .bind(job.0)
        .bind(token)
        .bind(error)
        .bind(available_at)
        .bind(terminal)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(result.rows_affected() == 1)
    }

    async fn reclaim_expired(
        &mut self,
        now: OffsetDateTime,
        max_attempts: u32,
        limit: u32,
    ) -> Result<u32, StoreError> {
        let result = sqlx::query(
            r"update video_jobs
                 set status = case when attempts >= $2 then 'failed' else 'queued' end,
                     error = case when attempts >= $2
                                  then coalesce(error, 'lease expired') else error end,
                     claim_token = null, lease_expires_at = null, updated_at = $1
               where id in (
                   select id from video_jobs
                    where status = 'rendering' and lease_expires_at <= $1
                    order by lease_expires_at, id
                    limit $3
                    for update skip locked)",
        )
        .bind(now)
        .bind(i64::from(max_attempts))
        .bind(i64::from(limit))
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        u32::try_from(result.rows_affected())
            .map_err(|_| StoreError::Invariant("reclaim count overflow"))
    }

    async fn attach_ready(&mut self, job: JobId) -> Result<bool, StoreError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "select {JOB_COLUMNS} from video_jobs where id = $1 for update"
        )))
        .bind(job.0)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        let Some(row) = row else { return Ok(false) };
        let row = job_from_row(&row)?;
        if row.status != JobStatus::Ready {
            return Ok(false);
        }
        let Some(url) = row.asset_url else {
            return Err(StoreError::Invariant("ready job without asset url"));
        };
        // Market row lock, then the hot-swap column write (codex B5).
        let locked = sqlx::query("select 1 from markets where id = $1 for update")
            .bind(row.market.0)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)?;
        if locked.is_none() {
            return Err(StoreError::NotFound("market"));
        }
        let column = match row.kind {
            ArtifactKind::Poster => "poster_asset_url",
            ArtifactKind::MarketVideo => "video_asset_url",
        };
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "update markets set {column} = $2 where id = $1"
        )))
        .bind(row.market.0)
        .bind(&url)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        sqlx::query(
            "update video_jobs set status = 'attached', updated_at = available_at where id = $1",
        )
        .bind(job.0)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        sqlx::query(
            r"insert into events_outbox (aggregate_type, aggregate_id, event_type, payload)
              values ('market', $1, 'VideoAttached', $2)",
        )
        .bind(row.market.0)
        .bind(serde_json::json!({ "kind": kind_name(row.kind), "url": url }))
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(true)
    }

    async fn job(&mut self, job: JobId) -> Result<Option<VideoJobRow>, StoreError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "select {JOB_COLUMNS} from video_jobs where id = $1"
        )))
        .bind(job.0)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        row.as_ref().map(job_from_row).transpose()
    }

    async fn market_row(&mut self, market: MarketId) -> Result<MarketRow, StoreError> {
        let row = sqlx::query(MARKET_ROW)
            .bind(market.0)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)?
            .ok_or(StoreError::NotFound("market"))?;
        market_from_row(&row)
    }

    async fn realizations(
        &mut self,
        user: UserId,
        market: MarketId,
    ) -> Result<Vec<RealizationFact>, StoreError> {
        let rows = sqlx::query(
            r"select user_id, market_id, outcome_id, source, realized_delta_micro,
                     payout_micro, txn_id, created_at
                from realizations
               where user_id = $1 and market_id = $2
               order by created_at, txn_id",
        )
        .bind(user.0)
        .bind(market.0)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        rows.iter()
            .map(|row| {
                let source: String = row.try_get("source").map_err(db_error)?;
                Ok(RealizationFact {
                    user: UserId(row.try_get("user_id").map_err(db_error)?),
                    market: MarketId(row.try_get("market_id").map_err(db_error)?),
                    outcome: application::model::OutcomeId(
                        row.try_get("outcome_id").map_err(db_error)?,
                    ),
                    source: parse_source(&source)?,
                    realized_delta: MicroUsd(
                        row.try_get("realized_delta_micro").map_err(db_error)?,
                    ),
                    payout: MicroUsd(row.try_get("payout_micro").map_err(db_error)?),
                    ledger_txn: row.try_get("txn_id").map_err(db_error)?,
                    created_at: row.try_get("created_at").map_err(db_error)?,
                })
            })
            .collect()
    }

    async fn user_handle(&mut self, user: UserId) -> Result<String, StoreError> {
        sqlx::query_scalar("select handle from users where id = $1")
            .bind(user.0)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)?
            .ok_or(StoreError::NotFound("user"))
    }

    async fn lock_moderation_cursor(&mut self) -> Result<i64, StoreError> {
        sqlx::query_scalar(
            "select last_seq from outbox_cursors where consumer = 'moderation' for update",
        )
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?
        .ok_or(StoreError::Invariant("moderation cursor row missing"))
    }

    async fn moderation_events_after(
        &mut self,
        after: i64,
        limit: u32,
    ) -> Result<Vec<OutboxEvent>, StoreError> {
        let rows = sqlx::query(
            r"select seq, aggregate_type, aggregate_id, event_type, payload
                from events_outbox
               where seq > $1
               order by seq
               limit $2",
        )
        .bind(after)
        .bind(i64::from(limit))
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        rows.iter()
            .map(|row| {
                Ok(OutboxEvent {
                    seq: row.try_get("seq").map_err(db_error)?,
                    event_type: row.try_get("event_type").map_err(db_error)?,
                    aggregate_type: row.try_get("aggregate_type").map_err(db_error)?,
                    aggregate_id: row.try_get("aggregate_id").map_err(db_error)?,
                    payload: row.try_get("payload").map_err(db_error)?,
                })
            })
            .collect()
    }

    async fn materialize_moderation_jobs(
        &mut self,
        comments: &[CommentId],
        now: OffsetDateTime,
    ) -> Result<u32, StoreError> {
        let mut created = 0u32;
        for comment in comments {
            // The comment_id unique absorbs replays AND duplicate ids inside
            // one batch (P5R2 N5) — but only committed/visible rows conflict,
            // so dedupe the in-flight batch through the same insert.
            let result = sqlx::query(
                r"insert into moderation_jobs (comment_id, status, available_at, created_at, updated_at)
                  values ($1, 'queued', $2, $2, $2)
                  on conflict (comment_id) do nothing",
            )
            .bind(comment.0)
            .bind(now)
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
            created += u32::try_from(result.rows_affected())
                .map_err(|_| StoreError::Invariant("materialize count overflow"))?;
        }
        Ok(created)
    }

    async fn save_moderation_cursor(&mut self, seq: i64) -> Result<(), StoreError> {
        let result =
            sqlx::query("update outbox_cursors set last_seq = $1 where consumer = 'moderation'")
                .bind(seq)
                .execute(&mut *self.tx)
                .await
                .map_err(db_error)?;
        if result.rows_affected() != 1 {
            return Err(StoreError::Invariant("moderation cursor row missing"));
        }
        Ok(())
    }

    async fn claim_moderation(
        &mut self,
        now: OffsetDateTime,
        lease: Duration,
        limit: u32,
    ) -> Result<Vec<ModerationJobRow>, StoreError> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            r"update moderation_jobs
                 set status = 'running', claim_token = gen_random_uuid(),
                     lease_expires_at = $1, attempts = attempts + 1, updated_at = $2
               where id in (
                   select id from moderation_jobs
                    where status = 'queued' and available_at <= $2
                    order by available_at, updated_at, id
                    limit $3
                    for update skip locked)
              returning {MODERATION_COLUMNS}",
        )))
        .bind(now + lease)
        .bind(now)
        .bind(i64::from(limit))
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        let mut jobs = rows
            .iter()
            .map(moderation_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        sort_moderation_jobs(&mut jobs);
        Ok(jobs)
    }

    async fn reclaim_moderation_expired(
        &mut self,
        now: OffsetDateTime,
        max_attempts: u32,
        limit: u32,
    ) -> Result<u32, StoreError> {
        // Moderation twin of `reclaim_expired`; `moderation_jobs_reclaim_idx`
        // (0007) serves exactly this predicate.
        let result = sqlx::query(
            r"update moderation_jobs
                 set status = case when attempts >= $2 then 'failed' else 'queued' end,
                     error = case when attempts >= $2
                                  then coalesce(error, 'lease expired') else error end,
                     claim_token = null, lease_expires_at = null, updated_at = $1
               where id in (
                   select id from moderation_jobs
                    where status = 'running' and lease_expires_at <= $1
                    order by lease_expires_at, id
                    limit $3
                    for update skip locked)",
        )
        .bind(now)
        .bind(i64::from(max_attempts))
        .bind(i64::from(limit))
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        u32::try_from(result.rows_affected())
            .map_err(|_| StoreError::Invariant("moderation reclaim count overflow"))
    }

    async fn complete_moderation(
        &mut self,
        job: JobId,
        token: Uuid,
        error: Option<&str>,
        available_at: OffsetDateTime,
        terminal: bool,
    ) -> Result<bool, StoreError> {
        let status = match (error, terminal) {
            (None, _) => ModerationJobStatus::Done,
            (Some(_), true) => ModerationJobStatus::Failed,
            (Some(_), false) => ModerationJobStatus::Queued,
        };
        let result = sqlx::query(
            r"update moderation_jobs
                 set status = $3, error = $4, available_at = $5,
                     claim_token = null, lease_expires_at = null, updated_at = $5
               where id = $1 and claim_token = $2 and status = 'running'",
        )
        .bind(job.0)
        .bind(token)
        .bind(moderation_status_name(status))
        .bind(error)
        .bind(available_at)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(result.rows_affected() == 1)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::sync::LazyLock;

    use application::contract::video as suite;
    use application::ports::Store;
    use tokio::sync::{Mutex, MutexGuard};

    use super::super::store::PgStore;
    use super::*;

    /// Connects AND clears the job tables: the leasing suites make global
    /// SKIP-LOCKED claims, so leftover queued jobs from earlier tests or
    /// runs would leak across suites. `opinions_w2` is this worker's
    /// isolated database and these tests run under `RUST_TEST_THREADS=1`,
    /// so the truncate races nothing.
    static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    async fn store() -> (PgStore, MutexGuard<'static, ()>) {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL for PostgreSQL video tests");
        let guard = TEST_LOCK.lock().await;
        let store = PgStore::connect(&url).await.expect("pg connect");
        sqlx::query("truncate table video_jobs, moderation_jobs")
            .execute(store.pool_handle())
            .await
            .expect("reset job tables");
        (store, guard)
    }

    /// Creates a real draft row so the durable issuance key can be exercised
    /// under the `video_jobs.draft_id` foreign key.
    async fn insert_draft(store: &PgStore) -> application::model::DraftId {
        let id: uuid::Uuid = sqlx::query_scalar(
            r"insert into market_drafts
                  (source, tier, status, question, description, video_script, slug,
                   seed_micro, fee_bps, min_votes_to_resolve, open_secs,
                   hidden_window_secs, expires_at)
              values ('template', 'daily', 'approved', 'q', 'd', 'v', $1,
                      1000000, 100, 3, 3600, 300, now() + interval '1 day')
              returning id",
        )
        .bind(format!("video-suite-{}", uuid::Uuid::new_v4()))
        .fetch_one(store.pool_handle())
        .await
        .expect("insert draft fixture");
        application::model::DraftId(id)
    }

    #[test]
    fn video_database_vocabularies_and_claim_order_are_total() {
        for status in [
            JobStatus::Queued,
            JobStatus::Rendering,
            JobStatus::Ready,
            JobStatus::Attached,
            JobStatus::Failed,
        ] {
            let name = match status {
                JobStatus::Queued => "queued",
                JobStatus::Rendering => "rendering",
                JobStatus::Ready => "ready",
                JobStatus::Attached => "attached",
                JobStatus::Failed => "failed",
            };
            assert_eq!(parse_status(name), Ok(status));
        }
        assert!(parse_status("corrupt").is_err());
        assert_eq!(parse_kind("market_video"), Ok(ArtifactKind::MarketVideo));
        assert_eq!(parse_kind("poster"), Ok(ArtifactKind::Poster));
        assert!(parse_kind("corrupt").is_err());

        for status in [
            ModerationJobStatus::Queued,
            ModerationJobStatus::Running,
            ModerationJobStatus::Done,
            ModerationJobStatus::Failed,
        ] {
            assert_eq!(
                parse_moderation_status(moderation_status_name(status)),
                Ok(status)
            );
        }
        assert!(parse_moderation_status("corrupt").is_err());
        assert_eq!(parse_source("sell"), Ok(RealizationSource::Sell));
        assert_eq!(
            parse_source("settlement"),
            Ok(RealizationSource::Settlement)
        );
        assert_eq!(parse_source("void"), Ok(RealizationSource::Void));
        assert!(parse_source("corrupt").is_err());

        let earlier_id = JobId(Uuid::from_u128(1));
        let later_id = JobId(Uuid::from_u128(2));
        let comment = CommentId(Uuid::from_u128(3));
        let row = |id, available_at| ModerationJobRow {
            id,
            comment,
            status: ModerationJobStatus::Queued,
            available_at,
            claim_token: None,
            lease_expires_at: None,
            attempts: 0,
        };
        let mut jobs = [
            row(later_id, OffsetDateTime::UNIX_EPOCH + Duration::seconds(1)),
            row(earlier_id, OffsetDateTime::UNIX_EPOCH),
        ];
        sort_moderation_jobs(&mut jobs);
        assert_eq!([jobs[0].id, jobs[1].id], [earlier_id, later_id]);
    }

    #[tokio::test]
    async fn pg_enqueue_idempotency() {
        let (store, _guard) = store().await;
        let draft = insert_draft(&store).await;
        suite::enqueue_idempotency_contract(&store, draft).await;
    }

    #[tokio::test]
    async fn pg_claim_leasing() {
        let (store, _guard) = store().await;
        suite::claim_leasing_contract(&store).await;
    }

    #[tokio::test]
    async fn pg_completion_cas() {
        let (store, _guard) = store().await;
        suite::completion_cas_contract(&store).await;
    }

    #[tokio::test]
    async fn pg_completion_error() {
        let (store, _guard) = store().await;
        suite::completion_error_contract(&store).await;
    }

    #[tokio::test]
    async fn pg_reclaim_boundary() {
        let (store, _guard) = store().await;
        suite::reclaim_boundary_contract(&store).await;
    }

    #[tokio::test]
    async fn pg_attach_ready() {
        let (store, _guard) = store().await;
        suite::attach_ready_contract(&store).await;
    }

    #[tokio::test]
    async fn pg_video_reads() {
        let (store, _guard) = store().await;
        suite::video_reads_contract(&store).await;
    }

    #[tokio::test]
    async fn pg_moderation_jobs() {
        let (store, _guard) = store().await;
        suite::moderation_jobs_contract(&store).await;
    }

    #[tokio::test]
    async fn pg_video_tx_opens() {
        let (store, _guard) = store().await;
        let tx = store.video_tx().await.expect("video tx opens");
        tx.commit().await.expect("empty commit");
    }
}
