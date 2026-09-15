//! In-memory `VideoTx` honoring the leasing write-path semantics the
//! Postgres adapter provides (Task 5.2).
//!
//! Divergence to know about (mirrors the documented account-allocation
//! divergence): `claim`/`reclaim_expired`/`claim_moderation` write their
//! lease stamps durably at call time instead of buffering to commit. The
//! worker protocol REQUIRES the claim transaction to commit before any
//! render work, so a committed-at-claim fake is observationally identical to
//! Postgres `FOR UPDATE SKIP LOCKED` under that protocol — and it makes two
//! concurrent claimers naturally disjoint. Everything else (enqueue,
//! token-fenced completions, attach, moderation materialization, cursor)
//! buffers and re-validates under the state lock at commit, exactly like the
//! unique-index / CAS re-checks a racing Postgres transaction performs.

use async_trait::async_trait;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use time::{Duration, OffsetDateTime};
use tokio::sync::OwnedMutexGuard;
use uuid::Uuid;

use crate::error::StoreError;
use crate::model::{
    event_type, ArtifactKind, CommentId, DraftId, Event, JobId, JobStatus, MarketId, MarketRow,
    ModerationJobRow, ModerationJobStatus, OutboxEvent, RealizationFact, RenderedArtifact, UserId,
    VideoJobRow,
};
use crate::ports::{Committable, Renderer, VideoTx};

use super::{InMemoryStore, Shared};

/// Reserved idempotency-key namespace standing in for the moderation cursor
/// row lock (real keys look like `draft:<id>:seed`, never `cursor:*`).
const MODERATION_CURSOR_KEY: &str = "cursor:moderation";

#[allow(clippy::unnecessary_wraps)]
pub(super) fn open(store: &InMemoryStore) -> Result<Box<dyn VideoTx + '_>, StoreError> {
    Ok(Box::new(FakeVideoTx::new(Arc::clone(&store.shared))))
}

impl FakeVideoTx {
    fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            market_guards: HashMap::new(),
            cursor_guard: None,
            pending_inserts: Vec::new(),
            pending_completions: Vec::new(),
            pending_attaches: Vec::new(),
            pending_moderation_inserts: Vec::new(),
            pending_moderation_completions: Vec::new(),
            pending_cursor: None,
        }
    }
}

/// Buffered token-fenced completion; re-verified against committed state at
/// commit so a lost race is dropped silently (the Postgres `WHERE` clause).
struct Completion {
    job: Uuid,
    token: Uuid,
    status: JobStatus,
    asset_url: Option<String>,
    error: Option<String>,
    available_at: Option<OffsetDateTime>,
}

struct ModerationCompletion {
    job: Uuid,
    token: Uuid,
    status: ModerationJobStatus,
    available_at: Option<OffsetDateTime>,
}

struct Attach {
    job: Uuid,
    market: Uuid,
    kind: ArtifactKind,
    url: String,
}

struct FakeVideoTx {
    shared: Arc<Shared>,
    market_guards: HashMap<Uuid, OwnedMutexGuard<()>>,
    cursor_guard: Option<OwnedMutexGuard<()>>,
    pending_inserts: Vec<VideoJobRow>,
    pending_completions: Vec<Completion>,
    pending_attaches: Vec<Attach>,
    pending_moderation_inserts: Vec<ModerationJobRow>,
    pending_moderation_completions: Vec<ModerationCompletion>,
    pending_cursor: Option<i64>,
}

use crate::video::artifacts::kind_name;

fn active(status: JobStatus) -> bool {
    matches!(
        status,
        JobStatus::Queued | JobStatus::Rendering | JobStatus::Ready
    )
}

impl FakeVideoTx {
    /// This transaction's view of a job: own pending writes over committed.
    fn effective_job(&self, id: Uuid) -> Option<VideoJobRow> {
        let committed = self.shared.state.lock().phase5.video_jobs.get(&id).cloned();
        let mut row = committed.or_else(|| {
            self.pending_inserts
                .iter()
                .find(|job| job.id.0 == id)
                .cloned()
        })?;
        for completion in &self.pending_completions {
            if completion.job == id {
                row.status = completion.status;
                if let Some(url) = &completion.asset_url {
                    row.asset_url = Some(url.clone());
                }
                if let Some(available_at) = completion.available_at {
                    row.available_at = available_at;
                }
                row.claim_token = None;
                row.lease_expires_at = None;
            }
        }
        for attach in &self.pending_attaches {
            if attach.job == id {
                row.status = JobStatus::Attached;
            }
        }
        Some(row)
    }

    async fn lock_market(&mut self, market: Uuid) {
        if !self.market_guards.contains_key(&market) {
            let handle = self.shared.market_locks.handle(&market);
            let guard = handle.lock_owned().await;
            self.market_guards.insert(market, guard);
        }
    }
}

#[async_trait]
impl VideoTx for FakeVideoTx {
    async fn enqueue(
        &mut self,
        market: MarketId,
        draft: Option<DraftId>,
        kind: ArtifactKind,
        now: OffsetDateTime,
    ) -> Result<JobId, StoreError> {
        {
            let st = self.shared.state.lock();
            let committed = st.phase5.video_jobs.values();
            let pending = self.pending_inserts.iter();
            for job in committed.chain(pending) {
                // Durable issuance key: one canonical job per (draft, kind)
                // across ALL statuses (codex P5R2 N2).
                if let (Some(mine), Some(theirs)) = (draft, job.draft) {
                    if mine == theirs && job.kind == kind {
                        return Ok(job.id);
                    }
                }
                // Active-market uniqueness (codex B4).
                if job.market == market && job.kind == kind && active(job.status) {
                    return Ok(job.id);
                }
            }
            if !st.markets.contains_key(&market.0) {
                return Err(StoreError::NotFound("market"));
            }
        }
        let job = VideoJobRow {
            id: JobId(Uuid::new_v4()),
            market,
            draft,
            kind,
            status: JobStatus::Queued,
            asset_url: None,
            available_at: now,
            claim_token: None,
            lease_expires_at: None,
            attempts: 0,
        };
        let id = job.id;
        self.pending_inserts.push(job);
        Ok(id)
    }

    async fn claim(
        &mut self,
        now: OffsetDateTime,
        lease: Duration,
        limit: u32,
    ) -> Result<Vec<VideoJobRow>, StoreError> {
        let mut st = self.shared.state.lock();
        let mut eligible: Vec<Uuid> = st
            .phase5
            .video_jobs
            .values()
            .filter(|job| job.status == JobStatus::Queued && job.available_at <= now)
            .map(|job| job.id.0)
            .collect();
        eligible.sort_by_key(|id| {
            let job = &st.phase5.video_jobs[id];
            (job.available_at, job.id.0)
        });
        let mut claimed = Vec::new();
        for id in eligible.into_iter().take(limit as usize) {
            let job = st
                .phase5
                .video_jobs
                .get_mut(&id)
                .ok_or(StoreError::Invariant("claim lost a job row"))?;
            job.status = JobStatus::Rendering;
            job.claim_token = Some(Uuid::new_v4());
            job.lease_expires_at = Some(now + lease);
            job.attempts += 1;
            claimed.push(job.clone());
        }
        Ok(claimed)
    }

    async fn complete_ready(
        &mut self,
        job: JobId,
        token: Uuid,
        asset_url: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let matched = {
            let st = self.shared.state.lock();
            st.phase5.video_jobs.get(&job.0).is_some_and(|row| {
                row.status == JobStatus::Rendering && row.claim_token == Some(token)
            })
        };
        if !matched {
            return Ok(false);
        }
        self.pending_completions.push(Completion {
            job: job.0,
            token,
            status: JobStatus::Ready,
            asset_url: Some(asset_url.to_string()),
            error: None,
            available_at: Some(now),
        });
        Ok(true)
    }

    async fn complete_error(
        &mut self,
        job: JobId,
        token: Uuid,
        error: &str,
        available_at: OffsetDateTime,
        terminal: bool,
    ) -> Result<bool, StoreError> {
        let matched = {
            let st = self.shared.state.lock();
            st.phase5.video_jobs.get(&job.0).is_some_and(|row| {
                row.status == JobStatus::Rendering && row.claim_token == Some(token)
            })
        };
        if !matched {
            return Ok(false);
        }
        self.pending_completions.push(Completion {
            job: job.0,
            token,
            status: if terminal {
                JobStatus::Failed
            } else {
                JobStatus::Queued
            },
            asset_url: None,
            error: Some(error.to_string()),
            available_at: Some(available_at),
        });
        Ok(true)
    }

    async fn reclaim_expired(
        &mut self,
        now: OffsetDateTime,
        max_attempts: u32,
        limit: u32,
    ) -> Result<u32, StoreError> {
        let mut st = self.shared.state.lock();
        let mut expired: Vec<Uuid> = st
            .phase5
            .video_jobs
            .values()
            .filter(|job| {
                job.status == JobStatus::Rendering
                    && job.lease_expires_at.is_some_and(|lease| lease <= now)
            })
            .map(|job| job.id.0)
            .collect();
        expired.sort_by_key(|id| {
            let job = &st.phase5.video_jobs[id];
            (job.lease_expires_at, job.id.0)
        });
        let mut reclaimed = 0u32;
        for id in expired.into_iter().take(limit as usize) {
            let job = st
                .phase5
                .video_jobs
                .get_mut(&id)
                .ok_or(StoreError::Invariant("reclaim lost a job row"))?;
            // The identical `>=` boundary as failure completion (P5R2 N3):
            // `max = 1` means exactly one attempt, ever.
            job.status = if job.attempts >= max_attempts {
                JobStatus::Failed
            } else {
                JobStatus::Queued
            };
            job.claim_token = None;
            job.lease_expires_at = None;
            reclaimed += 1;
        }
        Ok(reclaimed)
    }

    async fn attach_ready(&mut self, job: JobId) -> Result<bool, StoreError> {
        let Some(row) = self.effective_job(job.0) else {
            return Ok(false);
        };
        if row.status != JobStatus::Ready {
            return Ok(false);
        }
        let Some(url) = row.asset_url else {
            return Err(StoreError::Invariant("ready job without asset url"));
        };
        // Market row lock, then buffer the column write + event (codex B5).
        self.lock_market(row.market.0).await;
        if !self.shared.state.lock().markets.contains_key(&row.market.0) {
            return Err(StoreError::NotFound("market"));
        }
        self.pending_attaches.push(Attach {
            job: job.0,
            market: row.market.0,
            kind: row.kind,
            url,
        });
        Ok(true)
    }

    async fn job(&mut self, job: JobId) -> Result<Option<VideoJobRow>, StoreError> {
        Ok(self.effective_job(job.0))
    }

    async fn market_row(&mut self, market: MarketId) -> Result<MarketRow, StoreError> {
        let mut row = self
            .shared
            .state
            .lock()
            .markets
            .get(&market.0)
            .cloned()
            .ok_or(StoreError::NotFound("market"))?;
        for attach in &self.pending_attaches {
            if attach.market == market.0 {
                match attach.kind {
                    ArtifactKind::Poster => row.poster_asset_url = Some(attach.url.clone()),
                    ArtifactKind::MarketVideo => row.video_asset_url = Some(attach.url.clone()),
                }
            }
        }
        Ok(row)
    }

    async fn realizations(
        &mut self,
        user: UserId,
        market: MarketId,
    ) -> Result<Vec<RealizationFact>, StoreError> {
        let mut facts: Vec<RealizationFact> = self
            .shared
            .state
            .lock()
            .realizations
            .iter()
            .filter(|fact| fact.user == user && fact.market == market)
            .copied()
            .collect();
        facts.sort_by_key(|fact| (fact.created_at, fact.ledger_txn));
        Ok(facts)
    }

    async fn user_handle(&mut self, user: UserId) -> Result<String, StoreError> {
        self.shared
            .state
            .lock()
            .users
            .get(&user.0)
            .cloned()
            .ok_or(StoreError::NotFound("user"))
    }

    async fn lock_moderation_cursor(&mut self) -> Result<i64, StoreError> {
        if self.cursor_guard.is_none() {
            let handle = self
                .shared
                .key_locks
                .handle(&MODERATION_CURSOR_KEY.to_string());
            self.cursor_guard = Some(handle.lock_owned().await);
        }
        Ok(self.shared.state.lock().phase5.moderation_cursor)
    }

    async fn moderation_events_after(
        &mut self,
        after: i64,
        limit: u32,
    ) -> Result<Vec<OutboxEvent>, StoreError> {
        let st = self.shared.state.lock();
        let mut events = Vec::new();
        for (index, event) in st.outbox.iter().enumerate() {
            let seq = i64::try_from(index)
                .map_err(|_| StoreError::Invariant("outbox sequence overflow"))?
                + 1;
            if seq > after {
                events.push(OutboxEvent {
                    seq,
                    event_type: event.event_type.to_string(),
                    aggregate_type: event.aggregate_type.to_string(),
                    aggregate_id: event.aggregate_id,
                    payload: event.payload.clone(),
                });
                if events.len() >= limit as usize {
                    break;
                }
            }
        }
        Ok(events)
    }

    async fn materialize_moderation_jobs(
        &mut self,
        comments: &[CommentId],
        now: OffsetDateTime,
    ) -> Result<u32, StoreError> {
        let mut created = 0u32;
        for comment in comments {
            let exists = {
                let st = self.shared.state.lock();
                st.phase5
                    .moderation_jobs
                    .values()
                    .any(|job| job.comment == *comment)
            } || self
                .pending_moderation_inserts
                .iter()
                .any(|job| job.comment == *comment);
            if exists {
                continue;
            }
            self.pending_moderation_inserts.push(ModerationJobRow {
                id: JobId(Uuid::new_v4()),
                comment: *comment,
                status: ModerationJobStatus::Queued,
                available_at: now,
                claim_token: None,
                lease_expires_at: None,
                attempts: 0,
            });
            created += 1;
        }
        Ok(created)
    }

    async fn save_moderation_cursor(&mut self, seq: i64) -> Result<(), StoreError> {
        if self.cursor_guard.is_none() {
            return Err(StoreError::Invariant("moderation cursor not locked"));
        }
        self.pending_cursor = Some(seq);
        Ok(())
    }

    async fn claim_moderation(
        &mut self,
        now: OffsetDateTime,
        lease: Duration,
        limit: u32,
    ) -> Result<Vec<ModerationJobRow>, StoreError> {
        let mut st = self.shared.state.lock();
        let mut eligible: Vec<Uuid> = st
            .phase5
            .moderation_jobs
            .values()
            .filter(|job| job.status == ModerationJobStatus::Queued && job.available_at <= now)
            .map(|job| job.id.0)
            .collect();
        eligible.sort_by_key(|id| {
            let job = &st.phase5.moderation_jobs[id];
            (job.available_at, job.id.0)
        });
        let mut claimed = Vec::new();
        for id in eligible.into_iter().take(limit as usize) {
            let job = st
                .phase5
                .moderation_jobs
                .get_mut(&id)
                .ok_or(StoreError::Invariant("claim lost a moderation job"))?;
            job.status = ModerationJobStatus::Running;
            job.claim_token = Some(Uuid::new_v4());
            job.lease_expires_at = Some(now + lease);
            job.attempts += 1;
            claimed.push(*job);
        }
        Ok(claimed)
    }

    async fn reclaim_moderation_expired(
        &mut self,
        now: OffsetDateTime,
        max_attempts: u32,
        limit: u32,
    ) -> Result<u32, StoreError> {
        let mut st = self.shared.state.lock();
        let mut expired: Vec<Uuid> = st
            .phase5
            .moderation_jobs
            .values()
            .filter(|job| {
                job.status == ModerationJobStatus::Running
                    && job.lease_expires_at.is_some_and(|lease| lease <= now)
            })
            .map(|job| job.id.0)
            .collect();
        expired.sort_by_key(|id| {
            let job = &st.phase5.moderation_jobs[id];
            (job.lease_expires_at, job.id.0)
        });
        let mut reclaimed = 0u32;
        for id in expired.into_iter().take(limit as usize) {
            let job = st
                .phase5
                .moderation_jobs
                .get_mut(&id)
                .ok_or(StoreError::Invariant("reclaim lost a moderation job"))?;
            // The identical `>=` boundary as failure completion.
            job.status = if job.attempts >= max_attempts {
                ModerationJobStatus::Failed
            } else {
                ModerationJobStatus::Queued
            };
            job.claim_token = None;
            job.lease_expires_at = None;
            reclaimed += 1;
        }
        Ok(reclaimed)
    }

    async fn complete_moderation(
        &mut self,
        job: JobId,
        token: Uuid,
        error: Option<&str>,
        available_at: OffsetDateTime,
        terminal: bool,
    ) -> Result<bool, StoreError> {
        let matched = {
            let st = self.shared.state.lock();
            st.phase5.moderation_jobs.get(&job.0).is_some_and(|row| {
                row.status == ModerationJobStatus::Running && row.claim_token == Some(token)
            })
        };
        if !matched {
            return Ok(false);
        }
        let status = match (error, terminal) {
            (None, _) => ModerationJobStatus::Done,
            (Some(_), true) => ModerationJobStatus::Failed,
            (Some(_), false) => ModerationJobStatus::Queued,
        };
        self.pending_moderation_completions
            .push(ModerationCompletion {
                job: job.0,
                token,
                status,
                available_at: (status == ModerationJobStatus::Queued).then_some(available_at),
            });
        Ok(true)
    }
}

#[async_trait]
impl Committable for FakeVideoTx {
    async fn commit(self: Box<Self>) -> Result<(), StoreError> {
        let mut st = self.shared.state.lock();
        let mut next = st.clone();
        for job in &self.pending_inserts {
            // Unique re-checks under the state lock: a racing transaction's
            // committed row wins and this insert becomes DO NOTHING.
            let lost = next.phase5.video_jobs.values().any(|existing| {
                let draft_dup = matches!(
                    (job.draft, existing.draft),
                    (Some(mine), Some(theirs)) if mine == theirs
                ) && existing.kind == job.kind;
                let active_dup = existing.market == job.market
                    && existing.kind == job.kind
                    && active(existing.status);
                draft_dup || active_dup
            });
            if !lost {
                next.phase5.video_jobs.insert(job.id.0, job.clone());
            }
        }
        for completion in &self.pending_completions {
            // Token-fenced CAS re-check: apply only when the committed row is
            // still `rendering` under this token (a reclaim or newer attempt
            // between buffer and commit wins, and this write drops).
            let Some(row) = next.phase5.video_jobs.get_mut(&completion.job) else {
                continue;
            };
            if row.status != JobStatus::Rendering || row.claim_token != Some(completion.token) {
                continue;
            }
            row.status = completion.status;
            if let Some(url) = &completion.asset_url {
                row.asset_url = Some(url.clone());
            }
            if let Some(available_at) = completion.available_at {
                row.available_at = available_at;
            }
            let _ = &completion.error; // errors are backend-only columns
            row.claim_token = None;
            row.lease_expires_at = None;
        }
        for attach in &self.pending_attaches {
            let Some(row) = next.phase5.video_jobs.get_mut(&attach.job) else {
                continue;
            };
            // Attach rides the same commit as its completion; if the CAS
            // above dropped, the job is not Ready and the attach drops too.
            if row.status != JobStatus::Ready {
                continue;
            }
            row.status = JobStatus::Attached;
            let market = next
                .markets
                .get_mut(&attach.market)
                .ok_or(StoreError::Invariant("attach for unknown market"))?;
            match attach.kind {
                ArtifactKind::Poster => market.poster_asset_url = Some(attach.url.clone()),
                ArtifactKind::MarketVideo => market.video_asset_url = Some(attach.url.clone()),
            }
            next.outbox.push(Event {
                event_type: event_type::VIDEO_ATTACHED,
                aggregate_type: "market",
                aggregate_id: attach.market,
                payload: serde_json::json!({
                    "kind": kind_name(attach.kind),
                    "url": attach.url,
                }),
            });
        }
        for job in &self.pending_moderation_inserts {
            let duplicate = next
                .phase5
                .moderation_jobs
                .values()
                .any(|existing| existing.comment == job.comment);
            if !duplicate {
                next.phase5.moderation_jobs.insert(job.id.0, *job);
            }
        }
        for completion in &self.pending_moderation_completions {
            let Some(row) = next.phase5.moderation_jobs.get_mut(&completion.job) else {
                continue;
            };
            if row.status != ModerationJobStatus::Running
                || row.claim_token != Some(completion.token)
            {
                continue;
            }
            row.status = completion.status;
            if let Some(available_at) = completion.available_at {
                row.available_at = available_at;
            }
            row.claim_token = None;
            row.lease_expires_at = None;
        }
        if let Some(seq) = self.pending_cursor {
            next.phase5.moderation_cursor = seq;
        }
        *st = next;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn transaction_view_filters_other_writes_and_reuses_one_market_lock() {
        let store = InMemoryStore::new();
        let market = MarketId(Uuid::new_v4());
        let target = JobId(Uuid::new_v4());
        let other = JobId(Uuid::new_v4());
        let now = OffsetDateTime::UNIX_EPOCH;
        let mut tx = FakeVideoTx::new(Arc::clone(&store.shared));
        tx.pending_inserts.push(VideoJobRow {
            id: target,
            market,
            draft: None,
            kind: ArtifactKind::Poster,
            status: JobStatus::Queued,
            asset_url: None,
            available_at: now,
            claim_token: None,
            lease_expires_at: None,
            attempts: 0,
        });
        tx.pending_completions.push(Completion {
            job: other.0,
            token: Uuid::new_v4(),
            status: JobStatus::Failed,
            asset_url: None,
            error: Some("other".into()),
            available_at: None,
        });
        assert_eq!(
            tx.effective_job(target.0).unwrap().status,
            JobStatus::Queued
        );
        tx.pending_completions.push(Completion {
            job: target.0,
            token: Uuid::new_v4(),
            status: JobStatus::Ready,
            asset_url: Some("/assets/target.svg".into()),
            error: None,
            available_at: Some(now + Duration::seconds(1)),
        });
        tx.pending_attaches.push(Attach {
            job: other.0,
            market: market.0,
            kind: ArtifactKind::Poster,
            url: "/assets/other.svg".into(),
        });
        tx.pending_attaches.push(Attach {
            job: target.0,
            market: market.0,
            kind: ArtifactKind::Poster,
            url: "/assets/target.svg".into(),
        });
        let effective = tx.effective_job(target.0).unwrap();
        assert_eq!(effective.status, JobStatus::Attached);
        assert_eq!(effective.asset_url.as_deref(), Some("/assets/target.svg"));
        assert_eq!(effective.available_at, now + Duration::seconds(1));

        tx.lock_market(market.0).await;
        tx.lock_market(market.0).await;
        assert_eq!(tx.market_guards.len(), 1);
    }
}

/// Explicit skeleton dependency for wiring seams that predate a configured
/// renderer; every method reports typed unavailability.
pub struct UnavailableRenderer;

#[async_trait]
impl Renderer for UnavailableRenderer {
    async fn render(
        &self,
        _spec: &domain::render_spec::RenderSpec,
    ) -> Result<RenderedArtifact, StoreError> {
        Err(StoreError::Unavailable("phase5:renderer"))
    }

    async fn persist(
        &self,
        _render_dir: &Path,
        _job: JobId,
        _kind: ArtifactKind,
        _artifact: &RenderedArtifact,
    ) -> Result<String, StoreError> {
        Err(StoreError::Unavailable("phase5:renderer"))
    }

    async fn share_card(
        &self,
        _render_dir: &Path,
        _address: &str,
        _spec: &domain::render_spec::RenderSpec,
    ) -> Result<Vec<u8>, StoreError> {
        Err(StoreError::Unavailable("phase5:renderer"))
    }

    async fn load(
        &self,
        _render_dir: &Path,
        _job: JobId,
        _kind: ArtifactKind,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        Err(StoreError::Unavailable("phase5:renderer"))
    }
}
