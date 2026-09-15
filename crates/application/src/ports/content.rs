//! Content generation and durable publication roles.

use async_trait::async_trait;
use time::{Date, OffsetDateTime};

use crate::error::StoreError;
use crate::model::{
    ArtifactKind, DraftId, DraftRequest, DraftRow, Event, MarketId, ModerationVerdict, UserId,
};

use super::Committable;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedDraft {
    pub spec: domain::drafting::DraftSpec,
    pub source: domain::drafting::DraftSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("draft engine is unavailable")]
pub struct DraftEngineUnavailable;

#[async_trait]
pub trait DraftEngine: Send + Sync {
    async fn generate(
        &self,
        request: &DraftRequest,
    ) -> Result<GeneratedDraft, DraftEngineUnavailable>;
}

#[async_trait]
pub trait ModerationPreflight: Send + Sync {
    async fn screen(&self, body: &str) -> Result<ModerationVerdict, StoreError>;
}

/// Publication command status (D26 / codex r3 NEW-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationCommandStatus {
    Pending,
    Executing,
    Done,
    Failed,
}

/// Durable idempotent publish-now command: `POST /admin/drafts/{id}/publish_now`
/// atomically inserts (command + audit) and returns 202 + a status URL; the
/// existing publisher tick consumes due commands FIRST under its existing
/// lease discipline. The audited fact is the authorization; stage effects
/// remain saga-owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationCommand {
    pub id: uuid::Uuid,
    pub draft: DraftId,
    pub idempotency_key: String,
    pub requested_by: String,
    pub status: PublicationCommandStatus,
    pub attempts: i32,
    pub lease_expires_at: Option<OffsetDateTime>,
    pub result_market: Option<MarketId>,
    pub error: Option<String>,
}

#[async_trait]
pub trait ContentTx: super::AuditWrite + Committable {
    async fn lock_admission(&mut self) -> Result<(), StoreError>;
    /// Serializes publish-now authorization per draft (W2 command flow).
    async fn lock_publication(&mut self, draft: DraftId) -> Result<(), StoreError>;
    /// Serializes the publisher's command-claim pass.
    async fn lock_publication_queue(&mut self) -> Result<(), StoreError>;
    async fn lock_slot_tier(&mut self, tier: domain::drafting::DraftTier)
        -> Result<(), StoreError>;
    async fn lock_lp_kill_switch(&mut self) -> Result<(), StoreError>;
    async fn pending_draft_count(&mut self) -> Result<u32, StoreError>;
    async fn list_drafts(&mut self, limit: u32) -> Result<Vec<DraftRow>, StoreError>;
    async fn insert_draft(&mut self, draft: DraftRow) -> Result<DraftId, StoreError>;
    async fn draft_for_update(&mut self, draft: DraftId) -> Result<DraftRow, StoreError>;
    async fn save_draft(&mut self, draft: &DraftRow) -> Result<(), StoreError>;
    async fn lock_budget_day(&mut self, day: Date) -> Result<(), StoreError>;
    async fn reserved_seed_for_day(&mut self, day: Date) -> Result<i64, StoreError>;
    async fn lp_pnl_sum(
        &mut self,
        since: OffsetDateTime,
        until: OffsetDateTime,
    ) -> Result<i64, StoreError>;
    async fn slot_is_reserved(
        &mut self,
        tier: domain::drafting::DraftTier,
        at: OffsetDateTime,
    ) -> Result<bool, StoreError>;
    async fn due_drafts(
        &mut self,
        now: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<DraftId>, StoreError>;
    async fn due_expiries(
        &mut self,
        now: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<DraftId>, StoreError>;
    async fn reserve_market_id(&mut self, draft: DraftId) -> Result<MarketId, StoreError>;
    async fn set_market_question(
        &mut self,
        market: MarketId,
        question: &str,
    ) -> Result<(), StoreError>;
    async fn enqueue_artifact_jobs(
        &mut self,
        draft: DraftId,
        market: MarketId,
        kinds: &[ArtifactKind],
        available_at: OffsetDateTime,
    ) -> Result<(), StoreError>;
    async fn claim_unfilled_slot(&mut self, slot: OffsetDateTime) -> Result<bool, StoreError>;
    async fn record_reviewer(&mut self, draft: DraftId, user: UserId) -> Result<(), StoreError>;
    /// Inserts an open publication command (at most one open per draft;
    /// unique idempotency key). Returns the stored row; an existing open
    /// command for the draft is `Conflict("publication command")`, a reused
    /// key is `Conflict("publication command key")`.
    async fn insert_publication_command(
        &mut self,
        command: PublicationCommand,
    ) -> Result<(), StoreError>;
    /// Latest command for one draft as this transaction sees it.
    async fn publication_command_by_draft(
        &mut self,
        draft: DraftId,
    ) -> Result<Option<PublicationCommand>, StoreError>;
    /// The command with this idempotency key, if any (replay probe).
    async fn publication_command_by_key(
        &mut self,
        key: &str,
    ) -> Result<Option<PublicationCommand>, StoreError>;
    /// Due commands: pending, or executing with a lapsed lease — oldest
    /// first, at most `limit`. Consumed FIRST in the publisher tick.
    async fn due_publication_commands(
        &mut self,
        now: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<PublicationCommand>, StoreError>;
    /// Persists a status/lease/result change for one command.
    async fn save_publication_command(
        &mut self,
        command: &PublicationCommand,
    ) -> Result<(), StoreError>;
    async fn append(&mut self, event: Event) -> Result<(), StoreError>;
}
