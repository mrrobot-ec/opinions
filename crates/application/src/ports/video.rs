//! Artifact rendering and leased job roles (Task 5.2).
//!
//! Leasing protocol (codex B4): `claim` is a SHORT transaction — select
//! eligible queued jobs with SKIP-LOCKED semantics, stamp
//! `rendering + claim_token + lease + attempts+1`, and COMMIT before any
//! render work. Rendering happens outside every transaction. Completion —
//! success AND error — is a token-fenced compare-and-swap on
//! `(id, claim_token, status = rendering)`; zero rows means a newer attempt
//! owns the job and the stale result is dropped silently.

use async_trait::async_trait;
use std::path::Path;
use time::{Duration, OffsetDateTime};

use crate::error::StoreError;
use crate::model::{
    ArtifactKind, CommentId, DraftId, JobId, MarketId, MarketRow, ModerationJobRow, OutboxEvent,
    RealizationFact, RenderedArtifact, UserId, VideoJobRow,
};

use super::Committable;

/// Deterministic artifact renderer + atomic artifact persistence. One trait
/// because the frozen wiring injects exactly one render seam
/// (`Phase5Services::renderer`); the filesystem side stays adapter-owned.
#[async_trait]
pub trait Renderer: Send + Sync {
    /// Renders a spec to bytes. Byte identity across fresh instances is the
    /// contract (codex M3).
    async fn render(
        &self,
        spec: &domain::render_spec::RenderSpec,
    ) -> Result<RenderedArtifact, StoreError>;

    /// Atomically persists a leased job's artifact (temp + fsync + rename
    /// under the canonicalized `render_dir`; name derived ONLY from the job
    /// UUID + kind — codex M4). Returns the public asset URL.
    async fn persist(
        &self,
        render_dir: &Path,
        job: JobId,
        kind: ArtifactKind,
        artifact: &RenderedArtifact,
    ) -> Result<String, StoreError>;

    /// Content-addressed share-card cache under the `cards/` subroot:
    /// serves the cached bytes when `address` exists, otherwise renders the
    /// spec, persists atomically, and returns the fresh bytes.
    async fn share_card(
        &self,
        render_dir: &Path,
        address: &str,
        spec: &domain::render_spec::RenderSpec,
    ) -> Result<Vec<u8>, StoreError>;

    /// Loads a previously persisted job artifact (`None` when absent). The
    /// implementation resolves the UUID-derived name itself and must refuse
    /// symlinks and anything outside the canonicalized root.
    async fn load(
        &self,
        render_dir: &Path,
        job: JobId,
        kind: ArtifactKind,
    ) -> Result<Option<Vec<u8>>, StoreError>;
}

#[async_trait]
pub trait VideoTx: Committable {
    async fn enqueue(
        &mut self,
        market: MarketId,
        draft: Option<DraftId>,
        kind: ArtifactKind,
        now: OffsetDateTime,
    ) -> Result<JobId, StoreError>;
    async fn claim(
        &mut self,
        now: OffsetDateTime,
        lease: Duration,
        limit: u32,
    ) -> Result<Vec<VideoJobRow>, StoreError>;
    async fn complete_ready(
        &mut self,
        job: JobId,
        token: uuid::Uuid,
        asset_url: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError>;
    async fn complete_error(
        &mut self,
        job: JobId,
        token: uuid::Uuid,
        error: &str,
        available_at: OffsetDateTime,
        terminal: bool,
    ) -> Result<bool, StoreError>;
    async fn reclaim_expired(
        &mut self,
        now: OffsetDateTime,
        max_attempts: u32,
        limit: u32,
    ) -> Result<u32, StoreError>;
    async fn attach_ready(&mut self, job: JobId) -> Result<bool, StoreError>;
    /// Plain job read (serving + tests); no row lock. Defaulted so wave-peer
    /// test doubles that never read jobs keep compiling (5.2 additions must
    /// not break 5.4's in-flight implementors).
    async fn job(&mut self, job: JobId) -> Result<Option<VideoJobRow>, StoreError> {
        let _ = job;
        Err(StoreError::Unavailable("phase5:video-read"))
    }
    /// Plain market read for render inputs and share cards; no row lock
    /// (claim commits before rendering — the render never holds locks).
    async fn market_row(&mut self, market: MarketId) -> Result<MarketRow, StoreError> {
        let _ = market;
        Err(StoreError::Unavailable("phase5:video-read"))
    }
    /// Realization facts for one (user, market), ordered by
    /// `(created_at, ledger_txn)`; share-card inputs.
    async fn realizations(
        &mut self,
        user: UserId,
        market: MarketId,
    ) -> Result<Vec<RealizationFact>, StoreError> {
        let _ = (user, market);
        Err(StoreError::Unavailable("phase5:video-read"))
    }
    /// Public handle for share cards.
    async fn user_handle(&mut self, user: UserId) -> Result<String, StoreError> {
        let _ = user;
        Err(StoreError::Unavailable("phase5:video-read"))
    }
    async fn lock_moderation_cursor(&mut self) -> Result<i64, StoreError>;
    async fn moderation_events_after(
        &mut self,
        after: i64,
        limit: u32,
    ) -> Result<Vec<OutboxEvent>, StoreError>;
    async fn materialize_moderation_jobs(
        &mut self,
        comments: &[CommentId],
        now: OffsetDateTime,
    ) -> Result<u32, StoreError>;
    async fn save_moderation_cursor(&mut self, seq: i64) -> Result<(), StoreError>;
    async fn claim_moderation(
        &mut self,
        now: OffsetDateTime,
        lease: Duration,
        limit: u32,
    ) -> Result<Vec<ModerationJobRow>, StoreError>;
    /// Moderation twin of [`Self::reclaim_expired`]: flips `running` rows
    /// whose lease has elapsed back to `queued` (or `failed` at the same
    /// `attempts >= max_attempts` boundary as completion), clearing
    /// token/lease. Without it a crash after claim-commit would strand jobs
    /// `running` forever — permanently fail-open (5.4 review B1).
    async fn reclaim_moderation_expired(
        &mut self,
        now: OffsetDateTime,
        max_attempts: u32,
        limit: u32,
    ) -> Result<u32, StoreError>;
    async fn complete_moderation(
        &mut self,
        job: JobId,
        token: uuid::Uuid,
        error: Option<&str>,
        available_at: OffsetDateTime,
        terminal: bool,
    ) -> Result<bool, StoreError>;
}
