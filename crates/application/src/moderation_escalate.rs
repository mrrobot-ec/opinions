//! Two-stage durable LLM moderation escalation (codex P5R2 N5).
//!
//! Comment posting stays deterministic-sync (the Phase 4 path is untouched).
//! **Stage 1** — the `moderation` outbox cursor (seeded in migration 0007)
//! does only what the proven cursor pattern allows atomically: lock cursor →
//! read `CommentPosted` events → materialize idempotent `moderation_jobs`
//! rows → advance cursor, in one transaction with **zero remote calls**.
//! **Stage 2** — a leased job runner with the same semantics as video jobs
//! (short SKIP-LOCKED claim, remote preflight outside any lock or
//! transaction, token-fenced CAS completion including errors, `>=` attempts
//! boundary) applies the escalation on a [`ModerationVerdict::Shadow`]
//! verdict. The severity join lives here, in application code:
//! visible→shadow ONLY — never unshadow, never Blocked.
//!
//! Engine availability is probed with [`AVAILABILITY_PROBE`] between the
//! stages: adapters answer the empty probe locally (no outbound request), so
//! an unavailable or misconfigured engine leaves materialized jobs sitting
//! queued — no claims, no attempt burn — while the typed error surfaces to
//! the caller (and thereby fails startup preflight when the runner flag is
//! enabled without a configured engine). Brief-public-then-shadow is
//! accepted and stated: a comment is visible between the deterministic
//! Phase 4 admission and this asynchronous escalation.

use serde::Deserialize;
use time::{Duration, OffsetDateTime, PrimitiveDateTime};

use crate::error::StoreError;
use crate::model::{
    CommentId, ContentConfig, ModerationJobRow, ModerationStatus, ModerationVerdict,
};
use crate::ports::{Clock, ModerationPreflight, Store};

/// Calling convention between the runner and every [`ModerationPreflight`]
/// implementation: the empty probe is answered locally (never a remote
/// call). A configured engine returns `Ok`; an unavailable or misconfigured
/// one returns its typed error, which keeps jobs queued without burning
/// attempts.
pub const AVAILABILITY_PROBE: &str = "";

/// One cursor batch per tick, mirroring the notifier consumer's bound.
const MATERIALIZE_BATCH: u32 = 128;
/// One lease batch per tick.
const CLAIM_BATCH: u32 = 16;
/// Expired-lease reclaims per tick (the video worker's bound).
const RECLAIM_BATCH: u32 = 32;
/// Exponent cap in `backoff_base_secs * 2^min(attempts, CAP)` (video-job
/// pinned formula).
const BACKOFF_EXPONENT_CAP: u32 = 8;

/// The severity join: the only legal escalation is visible→shadow. A verdict
/// can never loosen (`Shadow`→`Visible`) and never hardens to `Blocked`.
#[must_use]
pub fn escalated_status(
    current: ModerationStatus,
    verdict: ModerationVerdict,
) -> Option<ModerationStatus> {
    match (current, verdict) {
        (ModerationStatus::Visible, ModerationVerdict::Shadow) => Some(ModerationStatus::Shadow),
        (
            ModerationStatus::Visible | ModerationStatus::Shadow | ModerationStatus::Blocked,
            ModerationVerdict::Visible | ModerationVerdict::Shadow,
        ) => None,
    }
}

/// Stable runner entry point wired by `main` behind `MODERATION_RUNNER_ENABLED`.
///
/// Runs Stage 1 (materialize; zero remote calls), probes engine
/// availability, then runs Stage 2 (lease → screen → escalate/complete).
///
/// # Errors
/// Store failures from either stage, the typed unavailability or
/// configuration error of an unusable engine (jobs stay queued), or the
/// first per-job invariant/completion failure of the batch (the batch tail
/// is still processed; per-job WORK failures are absorbed into the fenced
/// completion instead). A job whose completion failed stays leased until
/// its lease expires.
pub async fn runner_tick<S: Store + ?Sized, C: Clock + ?Sized>(
    store: &S,
    clock: &C,
    preflight: &dyn ModerationPreflight,
    config: &ContentConfig,
) -> Result<(), StoreError> {
    materialize_stage(store, clock).await?;
    preflight.screen(AVAILABILITY_PROBE).await?;
    run_stage(store, clock, preflight, config).await
}

#[derive(Deserialize)]
struct CommentPostedPayload {
    comment_id: uuid::Uuid,
}

/// Stage 1: one atomic cursor transaction — lock, read, materialize, advance.
async fn materialize_stage<S: Store + ?Sized, C: Clock + ?Sized>(
    store: &S,
    clock: &C,
) -> Result<(), StoreError> {
    let mut tx = store.video_tx().await?;
    let cursor = tx.lock_moderation_cursor().await?;
    let events = tx
        .moderation_events_after(cursor, MATERIALIZE_BATCH)
        .await?;
    let Some(last_seq) = events.last().map(|event| event.seq) else {
        return Ok(());
    };
    let mut comments = Vec::new();
    for event in &events {
        if event.event_type == "CommentPosted" {
            let payload: CommentPostedPayload = serde_json::from_value(event.payload.clone())
                .map_err(|_| StoreError::Invariant("invalid CommentPosted moderation event"))?;
            comments.push(CommentId(payload.comment_id));
        }
    }
    if !comments.is_empty() {
        tx.materialize_moderation_jobs(&comments, clock.now())
            .await?;
    }
    tx.save_moderation_cursor(last_seq).await?;
    tx.commit().await
}

/// Stage 2: short claim transaction, then per-job screen/escalate/complete.
async fn run_stage<S: Store + ?Sized, C: Clock + ?Sized>(
    store: &S,
    clock: &C,
    preflight: &dyn ModerationPreflight,
    config: &ContentConfig,
) -> Result<(), StoreError> {
    let lease = Duration::seconds(i64::try_from(config.lease_secs).unwrap_or(i64::MAX));
    let claimed = {
        let mut tx = store.video_tx().await?;
        // Crashed claimers first (video-worker order): their expired
        // `running` rows requeue — or fail at the attempts boundary —
        // before this tick's claim.
        tx.reclaim_moderation_expired(clock.now(), config.video_max_attempts, RECLAIM_BATCH)
            .await?;
        let jobs = tx.claim_moderation(clock.now(), lease, CLAIM_BATCH).await?;
        tx.commit().await?;
        jobs
    };
    // A per-job failure (missing-token invariant or the completion
    // transaction itself) is contained to that job: the batch tail still
    // completes, and the first error fails the tick for observability. The
    // failed job's lease runs out on its own.
    let mut first_error = None;
    for job in claimed {
        if let Err(error) = process_job(store, clock, preflight, config, &job).await {
            let _ = first_error.get_or_insert(error);
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// The fallible per-job work (read, screen, escalate), isolated so that ANY
/// error it raises — not just a screen rejection — is absorbed into the
/// token-fenced completion (parity with video render failures). `Ok(Some)`
/// is a terminal completion note; `Ok(None)` is success or nothing-to-do.
async fn job_work<S: Store + ?Sized>(
    store: &S,
    preflight: &dyn ModerationPreflight,
    job: &ModerationJobRow,
) -> Result<Option<&'static str>, StoreError> {
    // Plain read, dropped before the remote call: the preflight runs outside
    // any lock or transaction.
    let comment = {
        let mut tx = store.comment_tx().await?;
        tx.comment(job.comment).await?
    };
    let Some(row) = comment else {
        return Ok(Some("comment row is missing"));
    };
    if row.moderation_status != ModerationStatus::Visible {
        // Already at least as tight as any verdict could make it: only-tighten
        // means there is nothing to do, so the remote call is skipped.
        return Ok(None);
    }
    let verdict = preflight.screen(&row.body).await?;
    if escalated_status(row.moderation_status, verdict).is_some() {
        apply_escalation(store, job.comment, verdict).await?;
    }
    Ok(None)
}

async fn process_job<S: Store + ?Sized, C: Clock + ?Sized>(
    store: &S,
    clock: &C,
    preflight: &dyn ModerationPreflight,
    config: &ContentConfig,
    job: &ModerationJobRow,
) -> Result<(), StoreError> {
    let token = job
        .claim_token
        .ok_or(StoreError::Invariant("claimed moderation job has no token"))?;
    match job_work(store, preflight, job).await {
        Ok(Some(note)) => complete(store, job, token, Some(note), clock.now(), true).await,
        Ok(None) => complete(store, job, token, None, clock.now(), true).await,
        Err(error) => {
            let terminal = job.attempts >= config.video_max_attempts;
            let available_at = backoff_until(clock.now(), config.backoff_base_secs, job.attempts);
            let message = error.to_string();
            complete(store, job, token, Some(&message), available_at, terminal).await
        }
    }
}

/// Re-checks the severity join under the comment row lock before writing.
async fn apply_escalation<S: Store + ?Sized>(
    store: &S,
    comment: CommentId,
    verdict: ModerationVerdict,
) -> Result<(), StoreError> {
    let mut tx = store.comment_tx().await?;
    let row = tx.comment_for_update(comment).await?;
    let Some(next) = escalated_status(row.moderation_status, verdict) else {
        return Ok(());
    };
    tx.set_comment_status(comment, next).await?;
    tx.commit().await
}

/// Token-fenced CAS completion; a stale token (a newer attempt owns the job)
/// is dropped silently.
async fn complete<S: Store + ?Sized>(
    store: &S,
    job: &ModerationJobRow,
    token: uuid::Uuid,
    error: Option<&str>,
    available_at: OffsetDateTime,
    terminal: bool,
) -> Result<(), StoreError> {
    let mut tx = store.video_tx().await?;
    let _fresh = tx
        .complete_moderation(job.id, token, error, available_at, terminal)
        .await?;
    tx.commit().await
}

/// `now + backoff_base_secs * 2^min(attempts, 8)`, saturating (the pinned
/// video-job formula).
fn backoff_until(now: OffsetDateTime, base_secs: u64, attempts: u32) -> OffsetDateTime {
    let factor = 1_i64 << attempts.min(BACKOFF_EXPONENT_CAP);
    let secs = i64::try_from(base_secs)
        .unwrap_or(i64::MAX)
        .saturating_mul(factor);
    now.checked_add(Duration::seconds(secs))
        .unwrap_or_else(|| PrimitiveDateTime::MAX.assume_utc())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex as StdMutex};

    use async_trait::async_trait;
    use serde_json::json;
    use time::OffsetDateTime;
    use uuid::Uuid;

    use super::*;
    use crate::fakes::{FakeClock, InMemoryStore, UnavailableModerationPreflight};
    use crate::model::{JobId, MarketId, ModerationJobStatus, NewComment, OutboxEvent, UserId};
    use crate::ports::{
        AdvanceTx, BootstrapTx, CommentTx, Committable, ContentTx, DepositTx, IntegrityTx,
        NotificationTx, ResolveTx, SeedTx, TradeTx, VideoTx, VoteTx,
    };

    const T0: OffsetDateTime = OffsetDateTime::UNIX_EPOCH;

    fn absent<T>() -> Result<T, StoreError> {
        Err(StoreError::Unavailable("moderation-escalate test double"))
    }

    #[derive(Default)]
    struct ModState {
        jobs: BTreeMap<Uuid, ModerationJobRow>,
        errors: BTreeMap<Uuid, String>,
        cursor: i64,
        events: Vec<OutboxEvent>,
        fail_cursor_saves: u32,
        keep_jobs_on_failed_save: bool,
        fail_comment_txs: u32,
        fail_completes: u32,
    }

    impl ModState {
        fn job_for_comment(&self, comment: CommentId) -> Option<ModerationJobRow> {
            self.jobs
                .values()
                .find(|job| job.comment == comment)
                .copied()
        }
    }

    /// Moderation-capable [`Store`]: the Phase 1–4 `InMemoryStore` serves every
    /// role except `video_tx`, which is replaced by a moderation double with
    /// leased-claim semantics (claims and CAS completions apply immediately,
    /// modeling locked rows; materialize/cursor writes are buffered and only
    /// become observable at commit).
    #[derive(Clone)]
    struct TestStore {
        inner: InMemoryStore,
        moderation: Arc<StdMutex<ModState>>,
    }

    impl TestStore {
        fn new() -> Self {
            Self {
                inner: InMemoryStore::new(),
                moderation: Arc::new(StdMutex::new(ModState::default())),
            }
        }

        fn push_event(&self, seq: i64, event_type: &str, payload: serde_json::Value) {
            self.moderation.lock().unwrap().events.push(OutboxEvent {
                seq,
                event_type: event_type.to_string(),
                aggregate_type: "comment".to_string(),
                aggregate_id: Uuid::new_v4(),
                payload,
            });
        }

        fn push_comment_posted(&self, seq: i64, comment: CommentId) {
            self.push_event(
                seq,
                "CommentPosted",
                json!({
                    "market_id": Uuid::new_v4(),
                    "comment_id": comment.0,
                    "author_id": Uuid::new_v4(),
                    "parent_author_id": null,
                }),
            );
        }

        async fn insert_comment(&self, status: ModerationStatus) -> CommentId {
            let id = CommentId(Uuid::new_v4());
            let mut tx = self.inner.comment_tx().await.unwrap();
            tx.insert_comment(NewComment {
                id,
                market: MarketId(Uuid::new_v4()),
                author: UserId(Uuid::new_v4()),
                parent: None,
                body: format!("is comment {id:?} fishy?"),
                body_hash: format!("hash-{}", id.0),
                moderation_status: status,
                depth: 0,
                created_at: T0,
            })
            .await
            .unwrap();
            tx.commit().await.unwrap();
            id
        }

        async fn comment_status(&self, id: CommentId) -> ModerationStatus {
            let mut tx = self.inner.comment_tx().await.unwrap();
            tx.comment(id).await.unwrap().unwrap().moderation_status
        }

        fn job(&self, comment: CommentId) -> ModerationJobRow {
            self.moderation
                .lock()
                .unwrap()
                .job_for_comment(comment)
                .unwrap()
        }

        fn job_count(&self) -> usize {
            self.moderation.lock().unwrap().jobs.len()
        }

        fn cursor(&self) -> i64 {
            self.moderation.lock().unwrap().cursor
        }
    }

    #[async_trait]
    impl crate::ports::Store for TestStore {
        async fn trade_tx(&self) -> Result<Box<dyn TradeTx + '_>, StoreError> {
            self.inner.trade_tx().await
        }
        async fn vote_tx(&self) -> Result<Box<dyn VoteTx + '_>, StoreError> {
            self.inner.vote_tx().await
        }
        async fn resolve_tx(&self) -> Result<Box<dyn ResolveTx + '_>, StoreError> {
            self.inner.resolve_tx().await
        }
        async fn deposit_tx(&self) -> Result<Box<dyn DepositTx + '_>, StoreError> {
            self.inner.deposit_tx().await
        }
        async fn seed_tx(&self) -> Result<Box<dyn SeedTx + '_>, StoreError> {
            self.inner.seed_tx().await
        }
        async fn advance_tx(&self) -> Result<Box<dyn AdvanceTx + '_>, StoreError> {
            self.inner.advance_tx().await
        }
        async fn bootstrap_tx(&self) -> Result<Box<dyn BootstrapTx + '_>, StoreError> {
            self.inner.bootstrap_tx().await
        }
        async fn integrity_tx(&self) -> Result<Box<dyn IntegrityTx + '_>, StoreError> {
            self.inner.integrity_tx().await
        }
        async fn comment_tx(&self) -> Result<Box<dyn CommentTx + '_>, StoreError> {
            {
                let mut state = self.moderation.lock().unwrap();
                if state.fail_comment_txs > 0 {
                    state.fail_comment_txs -= 1;
                    return Err(StoreError::Backend("injected comment tx failure".into()));
                }
            }
            self.inner.comment_tx().await
        }
        async fn notification_tx(&self) -> Result<Box<dyn NotificationTx + '_>, StoreError> {
            self.inner.notification_tx().await
        }
        async fn content_tx(&self) -> Result<Box<dyn ContentTx + '_>, StoreError> {
            self.inner.content_tx().await
        }
        async fn video_tx(&self) -> Result<Box<dyn VideoTx + '_>, StoreError> {
            Ok(Box::new(TestVideoTx {
                state: Arc::clone(&self.moderation),
                staged_jobs: Vec::new(),
                staged_cursor: None,
            }))
        }
        async fn ops_config_tx(
            &self,
        ) -> Result<Box<dyn crate::ports::OpsConfigTx + '_>, StoreError> {
            self.inner.ops_config_tx().await
        }
        async fn ops_audit_tx(&self) -> Result<Box<dyn crate::ports::OpsAuditTx + '_>, StoreError> {
            self.inner.ops_audit_tx().await
        }
        async fn invariant_read_tx(
            &self,
        ) -> Result<Box<dyn crate::ports::InvariantReadTx + '_>, StoreError> {
            self.inner.invariant_read_tx().await
        }
        async fn unwind_tx(&self) -> Result<Box<dyn crate::ports::UnwindTx + '_>, StoreError> {
            self.inner.unwind_tx().await
        }
        async fn withdraw_tx(&self) -> Result<Box<dyn crate::ports::WithdrawTx + '_>, StoreError> {
            self.inner.withdraw_tx().await
        }
        async fn deposit_admission_tx(
            &self,
        ) -> Result<Box<dyn crate::ports::DepositAdmissionTx + '_>, StoreError> {
            self.inner.deposit_admission_tx().await
        }
        async fn credit_convert_tx(
            &self,
        ) -> Result<Box<dyn crate::ports::CreditConvertTx + '_>, StoreError> {
            self.inner.credit_convert_tx().await
        }
    }

    #[tokio::test]
    async fn phase6_ops_factories_pass_through_to_the_wave_implementations() {
        let store = TestStore::new();
        // W1 landed the REAL config transaction (coordinator integration):
        // the factory now opens and serializes on the generation row.
        let mut config = store.ops_config_tx().await.unwrap();
        assert!(config.lock_generation().await.is_ok());
        // W2 landed audit/invariants/unwind: the pass-through factories open
        // the real transactions.
        let audit = store.ops_audit_tx().await.unwrap();
        assert_eq!(audit.commit().await, Ok(()));
        let mut invariants = store.invariant_read_tx().await.unwrap();
        assert!(invariants.as_of().await.is_ok());
        let mut unwind = store.unwind_tx().await.unwrap();
        assert_eq!(unwind.open_receivables_total().await, Ok(0));
        drop((config, invariants, unwind));
        assert!(matches!(
            store.withdraw_tx().await,
            Err(StoreError::Unavailable("phase7:withdraw"))
        ));
        // deposit_admission_tx is intentionally REAL as of W3 (Phase 7 wave):
        // the factory opens a live transaction, so the placeholder assertion
        // is gone; committing the empty tx proves the pass-through works.
        assert!(store.deposit_admission_tx().await.is_ok());
        // credit_convert_tx is likewise real as of W3.
        assert!(store.credit_convert_tx().await.is_ok());
    }

    struct TestVideoTx {
        state: Arc<StdMutex<ModState>>,
        staged_jobs: Vec<ModerationJobRow>,
        staged_cursor: Option<i64>,
    }

    #[async_trait]
    impl Committable for TestVideoTx {
        async fn commit(self: Box<Self>) -> Result<(), StoreError> {
            let mut state = self.state.lock().unwrap();
            for job in self.staged_jobs {
                state.jobs.insert(job.id.0, job);
            }
            if let Some(cursor) = self.staged_cursor {
                state.cursor = cursor;
            }
            Ok(())
        }
    }

    #[async_trait]
    impl VideoTx for TestVideoTx {
        async fn enqueue(
            &mut self,
            _market: MarketId,
            _draft: Option<crate::model::DraftId>,
            _kind: crate::model::ArtifactKind,
            _now: OffsetDateTime,
        ) -> Result<JobId, StoreError> {
            absent()
        }
        async fn claim(
            &mut self,
            _now: OffsetDateTime,
            _lease: Duration,
            _limit: u32,
        ) -> Result<Vec<crate::model::VideoJobRow>, StoreError> {
            absent()
        }
        async fn complete_ready(
            &mut self,
            _job: JobId,
            _token: Uuid,
            _asset_url: &str,
            _now: OffsetDateTime,
        ) -> Result<bool, StoreError> {
            absent()
        }
        async fn complete_error(
            &mut self,
            _job: JobId,
            _token: Uuid,
            _error: &str,
            _available_at: OffsetDateTime,
            _terminal: bool,
        ) -> Result<bool, StoreError> {
            absent()
        }
        async fn reclaim_expired(
            &mut self,
            _now: OffsetDateTime,
            _max_attempts: u32,
            _limit: u32,
        ) -> Result<u32, StoreError> {
            absent()
        }
        async fn attach_ready(&mut self, _job: JobId) -> Result<bool, StoreError> {
            absent()
        }

        async fn lock_moderation_cursor(&mut self) -> Result<i64, StoreError> {
            Ok(self.state.lock().unwrap().cursor)
        }

        async fn moderation_events_after(
            &mut self,
            after: i64,
            limit: u32,
        ) -> Result<Vec<OutboxEvent>, StoreError> {
            let state = self.state.lock().unwrap();
            Ok(state
                .events
                .iter()
                .filter(|event| event.seq > after)
                .take(usize::try_from(limit).unwrap())
                .cloned()
                .collect())
        }

        async fn materialize_moderation_jobs(
            &mut self,
            comments: &[CommentId],
            now: OffsetDateTime,
        ) -> Result<u32, StoreError> {
            let state = self.state.lock().unwrap();
            let mut fresh = 0;
            for comment in comments {
                let exists = state.jobs.values().any(|job| job.comment == *comment)
                    || self.staged_jobs.iter().any(|job| job.comment == *comment);
                if exists {
                    continue; // the `comment_id unique` constraint absorbs replays
                }
                self.staged_jobs.push(ModerationJobRow {
                    id: JobId(Uuid::new_v4()),
                    comment: *comment,
                    status: ModerationJobStatus::Queued,
                    available_at: now,
                    claim_token: None,
                    lease_expires_at: None,
                    attempts: 0,
                });
                fresh += 1;
            }
            Ok(fresh)
        }

        async fn save_moderation_cursor(&mut self, seq: i64) -> Result<(), StoreError> {
            let mut state = self.state.lock().unwrap();
            if state.fail_cursor_saves > 0 {
                state.fail_cursor_saves -= 1;
                if state.keep_jobs_on_failed_save {
                    // Simulate the torn state a non-atomic store could leave:
                    // materialized jobs committed, cursor not advanced.
                    for job in self.staged_jobs.drain(..) {
                        state.jobs.insert(job.id.0, job);
                    }
                }
                return Err(StoreError::Backend("injected cursor save failure".into()));
            }
            self.staged_cursor = Some(seq);
            Ok(())
        }

        async fn claim_moderation(
            &mut self,
            now: OffsetDateTime,
            lease: Duration,
            limit: u32,
        ) -> Result<Vec<ModerationJobRow>, StoreError> {
            // Applied immediately under the state lock: models the short
            // SKIP-LOCKED claim transaction other runners observe at once.
            let mut state = self.state.lock().unwrap();
            let mut due: Vec<Uuid> = state
                .jobs
                .values()
                .filter(|job| job.status == ModerationJobStatus::Queued && job.available_at <= now)
                .map(|job| (job.available_at, job.id.0))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .map(|(_, id)| id)
                .collect();
            due.truncate(usize::try_from(limit).unwrap());
            let mut claimed = Vec::new();
            for id in due {
                let job = state.jobs.get_mut(&id).unwrap();
                job.status = ModerationJobStatus::Running;
                job.claim_token = Some(Uuid::new_v4());
                job.lease_expires_at = now.checked_add(lease);
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
            // Applied immediately, like `claim_moderation`.
            let mut state = self.state.lock().unwrap();
            let expired: Vec<Uuid> = state
                .jobs
                .values()
                .filter(|job| {
                    job.status == ModerationJobStatus::Running
                        && job.lease_expires_at.is_some_and(|lease| lease <= now)
                })
                .map(|job| (job.lease_expires_at, job.id.0))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .take(usize::try_from(limit).unwrap())
                .map(|(_, id)| id)
                .collect();
            let mut reclaimed = 0;
            for id in expired {
                let job = state.jobs.get_mut(&id).unwrap();
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
            let mut state = self.state.lock().unwrap();
            if state.fail_completes > 0 {
                state.fail_completes -= 1;
                return Err(StoreError::Backend("injected completion failure".into()));
            }
            let Some(row) = state.jobs.get_mut(&job.0) else {
                return Ok(false);
            };
            if row.status != ModerationJobStatus::Running || row.claim_token != Some(token) {
                return Ok(false); // stale token: a newer attempt owns the job
            }
            row.claim_token = None;
            row.lease_expires_at = None;
            row.status = match (error, terminal) {
                (None, _) => ModerationJobStatus::Done,
                (Some(_), true) => ModerationJobStatus::Failed,
                (Some(_), false) => {
                    row.available_at = available_at;
                    ModerationJobStatus::Queued
                }
            };
            if let Some(message) = error {
                state.errors.insert(job.0, message.to_string());
            }
            Ok(true)
        }
    }

    /// Scripted preflight honoring the probe contract: `""` is answered
    /// locally with `Ok(Visible)`; every real body gets `verdict`.
    struct ScriptedPreflight {
        verdict: Result<ModerationVerdict, ()>,
        calls: Arc<StdMutex<Vec<String>>>,
    }

    impl ScriptedPreflight {
        fn shadowing() -> Self {
            Self::new(Ok(ModerationVerdict::Shadow))
        }

        fn visible() -> Self {
            Self::new(Ok(ModerationVerdict::Visible))
        }

        /// Models the adapter rejecting an injection-shaped/invalid response.
        fn rejecting() -> Self {
            Self::new(Err(()))
        }

        fn new(verdict: Result<ModerationVerdict, ()>) -> Self {
            Self {
                verdict,
                calls: Arc::new(StdMutex::new(Vec::new())),
            }
        }

        fn bodies_screened(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|body| !body.is_empty())
                .cloned()
                .collect()
        }
    }

    #[async_trait]
    impl ModerationPreflight for ScriptedPreflight {
        async fn screen(&self, body: &str) -> Result<ModerationVerdict, StoreError> {
            self.calls.lock().unwrap().push(body.to_string());
            if body == AVAILABILITY_PROBE {
                return Ok(ModerationVerdict::Visible);
            }
            self.verdict.map_err(|()| {
                StoreError::Backend("llm moderation response rejected: injection-shaped".into())
            })
        }
    }

    async fn tick_at(
        store: &TestStore,
        preflight: &dyn ModerationPreflight,
        now: OffsetDateTime,
    ) -> Result<(), StoreError> {
        runner_tick(
            store,
            &FakeClock::at(now),
            preflight,
            &ContentConfig::default(),
        )
        .await
    }

    async fn tick(
        store: &TestStore,
        preflight: &dyn ModerationPreflight,
    ) -> Result<(), StoreError> {
        tick_at(store, preflight, T0).await
    }

    #[test]
    fn severity_join_is_only_tighten() {
        use ModerationStatus as S;
        use ModerationVerdict as V;
        assert_eq!(escalated_status(S::Visible, V::Shadow), Some(S::Shadow));
        assert_eq!(escalated_status(S::Visible, V::Visible), None);
        assert_eq!(escalated_status(S::Shadow, V::Shadow), None);
        assert_eq!(escalated_status(S::Shadow, V::Visible), None);
        assert_eq!(escalated_status(S::Blocked, V::Shadow), None);
        assert_eq!(escalated_status(S::Blocked, V::Visible), None);
    }

    #[test]
    fn backoff_doubles_from_post_claim_attempts_and_saturates() {
        let base = 5;
        for (attempts, secs) in [(0, 5), (1, 10), (2, 20), (3, 40), (8, 1_280), (30, 1_280)] {
            assert_eq!(
                backoff_until(T0, base, attempts),
                T0 + Duration::seconds(secs),
                "attempts {attempts}"
            );
        }
        // Saturation never panics and never goes backwards.
        assert!(backoff_until(T0, u64::MAX, 30) > T0);
    }

    #[tokio::test]
    async fn materializes_jobs_advances_cursor_and_completes_done() {
        let store = TestStore::new();
        let comment = store.insert_comment(ModerationStatus::Visible).await;
        store.push_comment_posted(7, comment);
        store.push_event(8, "MarketResolved", json!({}));

        let preflight = ScriptedPreflight::visible();
        tick(&store, &preflight).await.unwrap();

        assert_eq!(store.cursor(), 8);
        assert_eq!(store.job_count(), 1);
        assert_eq!(store.job(comment).status, ModerationJobStatus::Done);
        assert_eq!(
            store.comment_status(comment).await,
            ModerationStatus::Visible
        );
        assert_eq!(preflight.bodies_screened().len(), 1);

        // Nothing new: the next tick is a no-op.
        tick(&store, &preflight).await.unwrap();
        assert_eq!(store.job_count(), 1);
        assert_eq!(preflight.bodies_screened().len(), 1);
    }

    #[tokio::test]
    async fn non_comment_events_advance_cursor_without_jobs() {
        let store = TestStore::new();
        store.push_event(3, "MarketResolved", json!({}));
        tick(&store, &ScriptedPreflight::visible()).await.unwrap();
        assert_eq!(store.cursor(), 3);
        assert_eq!(store.job_count(), 0);
    }

    #[tokio::test]
    async fn malformed_comment_posted_payload_fails_closed() {
        let store = TestStore::new();
        store.push_event(1, "CommentPosted", json!({ "unexpected": true }));
        let error = tick(&store, &ScriptedPreflight::visible())
            .await
            .unwrap_err();
        assert_eq!(
            error,
            StoreError::Invariant("invalid CommentPosted moderation event")
        );
        assert_eq!(store.cursor(), 0);
        assert_eq!(store.job_count(), 0);
    }

    #[tokio::test]
    async fn unavailable_engine_materializes_then_idles_jobs_queued() {
        let store = TestStore::new();
        let comment = store.insert_comment(ModerationStatus::Visible).await;
        store.push_comment_posted(1, comment);

        for _ in 0..2 {
            let error = tick(&store, &UnavailableModerationPreflight)
                .await
                .unwrap_err();
            assert_eq!(
                error,
                StoreError::Unavailable("phase5:moderation-preflight")
            );
            // Stage 1 ran; stage 2 never claimed: no attempt burn, ever.
            assert_eq!(store.cursor(), 1);
            let job = store.job(comment);
            assert_eq!(job.status, ModerationJobStatus::Queued);
            assert_eq!(job.attempts, 0);
        }
    }

    #[tokio::test]
    async fn cursor_save_failure_is_atomic_and_replay_is_absorbed() {
        // Case 1: the store is atomic — nothing becomes observable.
        let store = TestStore::new();
        let comment = store.insert_comment(ModerationStatus::Visible).await;
        store.push_comment_posted(5, comment);
        store.moderation.lock().unwrap().fail_cursor_saves = 1;

        let preflight = ScriptedPreflight::visible();
        let error = tick(&store, &preflight).await.unwrap_err();
        assert_eq!(
            error,
            StoreError::Backend("injected cursor save failure".into())
        );
        assert_eq!((store.job_count(), store.cursor()), (0, 0));

        // The healed replay materializes exactly once.
        tick(&store, &preflight).await.unwrap();
        assert_eq!((store.job_count(), store.cursor()), (1, 5));

        // Case 2: even a torn crash BETWEEN materialize and advance (jobs
        // committed, cursor not) is absorbed on replay by the comment_id
        // uniqueness — no duplicate job is ever created.
        let torn = TestStore::new();
        let torn_comment = torn.insert_comment(ModerationStatus::Visible).await;
        torn.push_comment_posted(9, torn_comment);
        {
            let mut state = torn.moderation.lock().unwrap();
            state.fail_cursor_saves = 1;
            state.keep_jobs_on_failed_save = true;
        }
        let error = tick(&torn, &preflight).await.unwrap_err();
        assert_eq!(
            error,
            StoreError::Backend("injected cursor save failure".into())
        );
        assert_eq!((torn.job_count(), torn.cursor()), (1, 0));

        tick(&torn, &preflight).await.unwrap();
        assert_eq!((torn.job_count(), torn.cursor()), (1, 9));
    }

    #[tokio::test]
    async fn shadow_verdict_escalates_visible_comment_exactly_once() {
        let store = TestStore::new();
        let comment = store.insert_comment(ModerationStatus::Visible).await;
        store.push_comment_posted(1, comment);

        let preflight = ScriptedPreflight::shadowing();
        tick(&store, &preflight).await.unwrap();

        assert_eq!(
            store.comment_status(comment).await,
            ModerationStatus::Shadow
        );
        assert_eq!(store.job(comment).status, ModerationJobStatus::Done);
        assert_eq!(preflight.bodies_screened().len(), 1);
    }

    #[tokio::test]
    async fn already_tightened_comments_skip_the_remote_call() {
        for status in [ModerationStatus::Shadow, ModerationStatus::Blocked] {
            let store = TestStore::new();
            let comment = store.insert_comment(status).await;
            store.push_comment_posted(1, comment);

            let preflight = ScriptedPreflight::shadowing();
            tick(&store, &preflight).await.unwrap();

            // Never unshadow, never touch Blocked — and no remote call at all.
            assert_eq!(store.comment_status(comment).await, status);
            assert_eq!(store.job(comment).status, ModerationJobStatus::Done);
            assert!(preflight.bodies_screened().is_empty());
        }
    }

    #[tokio::test]
    async fn escalation_recheck_under_lock_is_only_tighten() {
        // A comment that tightens between the unlocked read and the locked
        // apply is left alone by the re-check.
        let store = TestStore::new();
        let comment = store.insert_comment(ModerationStatus::Blocked).await;
        apply_escalation(&store, comment, ModerationVerdict::Shadow)
            .await
            .unwrap();
        assert_eq!(
            store.comment_status(comment).await,
            ModerationStatus::Blocked
        );
    }

    #[tokio::test]
    async fn rejected_responses_back_off_then_fail_terminal_without_flipping() {
        let store = TestStore::new();
        let comment = store.insert_comment(ModerationStatus::Visible).await;
        store.push_comment_posted(1, comment);
        let config = ContentConfig {
            video_max_attempts: 2,
            ..ContentConfig::default()
        };
        let preflight = ScriptedPreflight::rejecting();
        let clock = FakeClock::at(T0);

        // Attempt 1: claimed (attempts=1), rejected, requeued with backoff.
        runner_tick(&store, &clock, &preflight, &config)
            .await
            .unwrap();
        let job = store.job(comment);
        assert_eq!(job.status, ModerationJobStatus::Queued);
        assert_eq!(job.attempts, 1);
        assert_eq!(job.available_at, T0 + Duration::seconds(10)); // 5 * 2^1
        assert!(store
            .moderation
            .lock()
            .unwrap()
            .errors
            .get(&job.id.0)
            .unwrap()
            .contains("injection-shaped"));

        // Not yet due: an immediate tick claims nothing.
        runner_tick(&store, &clock, &preflight, &config)
            .await
            .unwrap();
        assert_eq!(store.job(comment).attempts, 1);

        // Attempt 2 hits the >= boundary: terminal failure, and the
        // injection-shaped response never flipped the verdict.
        let later = FakeClock::at(T0 + Duration::seconds(20));
        runner_tick(&store, &later, &preflight, &config)
            .await
            .unwrap();
        let job = store.job(comment);
        assert_eq!(job.status, ModerationJobStatus::Failed);
        assert_eq!(job.attempts, 2);
        assert_eq!(
            store.comment_status(comment).await,
            ModerationStatus::Visible
        );
    }

    #[tokio::test]
    async fn per_job_read_failure_requeues_fenced_and_the_batch_tail_still_completes() {
        let store = TestStore::new();
        let first = store.insert_comment(ModerationStatus::Visible).await;
        let second = store.insert_comment(ModerationStatus::Visible).await;
        store.push_comment_posted(1, first);
        store.push_comment_posted(2, second);
        store.moderation.lock().unwrap().fail_comment_txs = 1;

        // A per-job work failure (here: the comment read) is absorbed into the
        // token-fenced completion — the tick itself succeeds and the rest of
        // the claimed batch is processed, not orphaned.
        let preflight = ScriptedPreflight::shadowing();
        tick(&store, &preflight).await.unwrap();

        let jobs = [store.job(first), store.job(second)];
        let requeued = jobs
            .iter()
            // The failed job is requeued, not left running.
            .find(|job| job.status == ModerationJobStatus::Queued)
            .unwrap();
        let done = jobs
            .iter()
            // The batch tail still completes.
            .find(|job| job.status == ModerationJobStatus::Done)
            .unwrap();
        assert_eq!(requeued.attempts, 1);
        assert_eq!(requeued.available_at, T0 + Duration::seconds(10)); // 5 * 2^1
        assert!(requeued.claim_token.is_none());
        assert!(store
            .moderation
            .lock()
            .unwrap()
            .errors
            .get(&requeued.id.0)
            .unwrap()
            .contains("injected comment tx failure"));
        // Only the surviving job was screened; its comment tightened.
        assert_eq!(preflight.bodies_screened().len(), 1);
        assert_eq!(
            store.comment_status(done.comment).await,
            ModerationStatus::Shadow
        );
        assert_eq!(
            store.comment_status(requeued.comment).await,
            ModerationStatus::Visible
        );

        // Once due again the requeued job screens and tightens normally.
        tick_at(&store, &preflight, T0 + Duration::seconds(10))
            .await
            .unwrap();
        assert_eq!(
            store.job(requeued.comment).status,
            ModerationJobStatus::Done
        );
        assert_eq!(
            store.comment_status(requeued.comment).await,
            ModerationStatus::Shadow
        );
    }

    #[tokio::test]
    async fn expired_lease_is_reclaimed_and_the_job_screens_again() {
        let store = TestStore::new();
        let comment = store.insert_comment(ModerationStatus::Visible).await;
        store.push_comment_posted(1, comment);
        // Tick 1 "crashes" after the screen (Visible verdict: nothing was
        // applied): the completion fails, stranding the job `running` with
        // a live token (lease_secs default: 60).
        store.moderation.lock().unwrap().fail_completes = 1;
        let preflight = ScriptedPreflight::visible();
        tick(&store, &preflight).await.unwrap_err();
        assert_eq!(store.job(comment).status, ModerationJobStatus::Running);

        // Before expiry nothing changes: the lease still fences the job.
        tick_at(&store, &preflight, T0 + Duration::seconds(59))
            .await
            .unwrap();
        assert_eq!(store.job(comment).status, ModerationJobStatus::Running);
        assert_eq!(preflight.bodies_screened().len(), 1);

        // After expiry the SAME tick reclaims, re-claims, screens again, and
        // completes: the durable second screen survives the crash.
        tick_at(&store, &preflight, T0 + Duration::seconds(61))
            .await
            .unwrap();
        let job = store.job(comment);
        assert_eq!(job.status, ModerationJobStatus::Done);
        assert_eq!(job.attempts, 2);
        assert!(job.claim_token.is_none());
        assert_eq!(preflight.bodies_screened().len(), 2);
        assert_eq!(
            store.comment_status(comment).await,
            ModerationStatus::Visible
        );
    }

    #[tokio::test]
    async fn reclaim_at_the_attempts_boundary_is_terminal() {
        let store = TestStore::new();
        let comment = store.insert_comment(ModerationStatus::Visible).await;
        store.push_comment_posted(1, comment);
        let config = ContentConfig {
            video_max_attempts: 1,
            ..ContentConfig::default()
        };
        store.moderation.lock().unwrap().fail_completes = 1;
        let preflight = ScriptedPreflight::visible();
        runner_tick(&store, &FakeClock::at(T0), &preflight, &config)
            .await
            .unwrap_err();
        assert_eq!(store.job(comment).status, ModerationJobStatus::Running);

        // attempts (1) >= max (1): the reclaim is terminal — no new claim,
        // no further remote call, and the comment's status is untouched.
        runner_tick(
            &store,
            &FakeClock::at(T0 + Duration::seconds(61)),
            &preflight,
            &config,
        )
        .await
        .unwrap();
        let job = store.job(comment);
        assert_eq!(job.status, ModerationJobStatus::Failed);
        assert_eq!(job.attempts, 1);
        assert!(job.claim_token.is_none());
        assert_eq!(preflight.bodies_screened().len(), 1);
        assert_eq!(
            store.comment_status(comment).await,
            ModerationStatus::Visible
        );
    }

    #[tokio::test]
    async fn completion_failure_is_contained_to_its_job_and_still_fails_the_tick() {
        let store = TestStore::new();
        let first = store.insert_comment(ModerationStatus::Visible).await;
        let second = store.insert_comment(ModerationStatus::Visible).await;
        store.push_comment_posted(1, first);
        store.push_comment_posted(2, second);
        store.moderation.lock().unwrap().fail_completes = 1;

        // The completion transaction itself failing is tick-fatal for
        // observability, but it must not abandon the rest of the batch.
        let preflight = ScriptedPreflight::shadowing();
        let error = tick(&store, &preflight).await.unwrap_err();
        assert_eq!(
            error,
            StoreError::Backend("injected completion failure".into())
        );

        let jobs = [store.job(first), store.job(second)];
        let orphaned = jobs
            .iter()
            // The failed completion leaves its own job leased.
            .find(|job| job.status == ModerationJobStatus::Running)
            .unwrap();
        let done = jobs
            .iter()
            // The batch tail still completes.
            .find(|job| job.status == ModerationJobStatus::Done)
            .unwrap();
        // Both were screened and tightened before their completions ran.
        assert_eq!(preflight.bodies_screened().len(), 2);
        for job in [orphaned, done] {
            assert_eq!(
                store.comment_status(job.comment).await,
                ModerationStatus::Shadow
            );
        }
        // The orphaned lease is reclaim's problem (B1), not this tick's.
        assert!(orphaned.claim_token.is_some());
    }

    #[tokio::test]
    async fn two_concurrent_runners_claim_disjoint_jobs() {
        // 20 jobs > the 16-job claim batch: with immediate claim visibility
        // the two runners split the queue between them.
        let store = TestStore::new();
        let mut comments = Vec::new();
        for seq in 0..20_i64 {
            let comment = store.insert_comment(ModerationStatus::Visible).await;
            store.push_comment_posted(seq + 1, comment);
            comments.push(comment);
        }
        // Materialize first (idle tick: engine unavailable, so no claims).
        tick(&store, &UnavailableModerationPreflight)
            .await
            .unwrap_err();
        assert_eq!(store.job_count(), 20);

        let first = ScriptedPreflight::shadowing();
        let second = ScriptedPreflight {
            verdict: Ok(ModerationVerdict::Shadow),
            calls: Arc::clone(&first.calls),
        };
        let (a, b) = tokio::join!(tick(&store, &first), tick(&store, &second));
        a.unwrap();
        b.unwrap();

        // Every job done exactly once: 20 distinct bodies screened in total
        // across both runners — claims were disjoint.
        let mut screened = first.bodies_screened();
        screened.sort_unstable();
        screened.dedup();
        assert_eq!(screened.len(), 20);
        assert_eq!(first.bodies_screened().len(), 20);
        for comment in comments {
            assert_eq!(
                store.comment_status(comment).await,
                ModerationStatus::Shadow
            );
            assert_eq!(store.job(comment).status, ModerationJobStatus::Done);
        }
    }

    #[tokio::test]
    async fn vanished_comment_fails_terminal() {
        let store = TestStore::new();
        let ghost = CommentId(Uuid::new_v4());
        store.push_comment_posted(1, ghost);

        tick(&store, &ScriptedPreflight::shadowing()).await.unwrap();

        let job = store.job(ghost);
        assert_eq!(job.status, ModerationJobStatus::Failed);
        assert_eq!(
            store
                .moderation
                .lock()
                .unwrap()
                .errors
                .get(&job.id.0)
                .unwrap(),
            "comment row is missing"
        );
    }

    #[tokio::test]
    async fn stale_token_completion_is_dropped_silently() {
        let store = TestStore::new();
        let comment = store.insert_comment(ModerationStatus::Visible).await;
        store.push_comment_posted(1, comment);
        tick(&store, &UnavailableModerationPreflight)
            .await
            .unwrap_err();

        // Claim with token A, then simulate a newer attempt overwriting it.
        let claimed = {
            let mut tx = store.video_tx().await.unwrap();
            let jobs = tx
                .claim_moderation(T0, Duration::seconds(60), 16)
                .await
                .unwrap();
            tx.commit().await.unwrap();
            jobs
        };
        let stale = claimed[0];
        let newer_token = Uuid::new_v4();
        store
            .moderation
            .lock()
            .unwrap()
            .jobs
            .get_mut(&stale.id.0)
            .unwrap()
            .claim_token = Some(newer_token);

        // The runner's completion with the stale token must not error and
        // must leave the newer owner untouched.
        complete(&store, &stale, stale.claim_token.unwrap(), None, T0, true)
            .await
            .unwrap();
        let job = store.job(comment);
        assert_eq!(job.status, ModerationJobStatus::Running);
        assert_eq!(job.claim_token, Some(newer_token));
    }

    #[tokio::test]
    async fn missing_claim_token_is_an_invariant_failure() {
        let store = TestStore::new();
        let job = ModerationJobRow {
            id: JobId(Uuid::new_v4()),
            comment: CommentId(Uuid::new_v4()),
            status: ModerationJobStatus::Running,
            available_at: T0,
            claim_token: None,
            lease_expires_at: None,
            attempts: 1,
        };
        let error = process_job(
            &store,
            &FakeClock::at(T0),
            &ScriptedPreflight::visible(),
            &ContentConfig::default(),
            &job,
        )
        .await
        .unwrap_err();
        assert_eq!(
            error,
            StoreError::Invariant("claimed moderation job has no token")
        );
    }

    #[tokio::test]
    async fn test_double_surface_is_total() {
        // Delegation factories reach the Phase 1–4 fake …
        let store = TestStore::new();
        assert!(store.trade_tx().await.is_ok());
        assert!(store.vote_tx().await.is_ok());
        assert!(store.resolve_tx().await.is_ok());
        assert!(store.deposit_tx().await.is_ok());
        assert!(store.seed_tx().await.is_ok());
        assert!(store.advance_tx().await.is_ok());
        assert!(store.bootstrap_tx().await.is_ok());
        assert!(store.integrity_tx().await.is_ok());
        assert!(store.notification_tx().await.is_ok());
        let _content = store.content_tx().await;
        // … and the video-job half of the double stays explicitly absent.
        let mut tx = store.video_tx().await.unwrap();
        let market = MarketId(Uuid::new_v4());
        assert!(tx
            .enqueue(market, None, crate::model::ArtifactKind::Poster, T0)
            .await
            .is_err());
        assert!(tx.claim(T0, Duration::seconds(1), 1).await.is_err());
        assert!(tx
            .complete_ready(JobId(Uuid::new_v4()), Uuid::new_v4(), "url", T0)
            .await
            .is_err());
        assert!(tx
            .complete_error(JobId(Uuid::new_v4()), Uuid::new_v4(), "e", T0, false)
            .await
            .is_err());
        assert!(tx.reclaim_expired(T0, 3, 1).await.is_err());
        assert!(tx.attach_ready(JobId(Uuid::new_v4())).await.is_err());
        // Completing an unknown job is a stale no-op, not an error.
        assert!(!tx
            .complete_moderation(JobId(Uuid::new_v4()), Uuid::new_v4(), None, T0, true)
            .await
            .unwrap());
    }
}
