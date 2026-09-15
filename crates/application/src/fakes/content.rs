use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use domain::drafting::DraftTier;
use time::{Date, OffsetDateTime, UtcOffset};
use tokio::sync::OwnedMutexGuard;
use uuid::Uuid;

use crate::error::StoreError;
use crate::model::{
    ArtifactKind, DraftId, DraftRequest, DraftRow, DraftStatus, Event, JobId, JobStatus, MarketId,
    ModerationVerdict, UserId, VideoJobRow,
};
use crate::ports::{
    Committable, ContentTx, DraftEngine, DraftEngineUnavailable, GeneratedDraft,
    ModerationPreflight,
};

use super::InMemoryStore;

#[allow(clippy::unnecessary_wraps)]
pub(super) fn open(store: &InMemoryStore) -> Result<Box<dyn ContentTx + '_>, StoreError> {
    Ok(Box::new(FakeContentTx::new(store)))
}

struct FakeContentTx<'a> {
    store: &'a InMemoryStore,
    guards: Vec<OwnedMutexGuard<()>>,
    locked_keys: BTreeSet<String>,
    lp_guard: Option<OwnedMutexGuard<()>>,
    drafts: BTreeMap<Uuid, DraftRow>,
    inserts: BTreeSet<Uuid>,
    jobs: BTreeMap<Uuid, VideoJobRow>,
    market_questions: BTreeMap<Uuid, String>,
    slot_events: BTreeSet<i64>,
    events: Vec<Event>,
    phase6: super::ops::Phase6Pending,
}

impl<'a> FakeContentTx<'a> {
    fn new(store: &'a InMemoryStore) -> Self {
        Self {
            store,
            guards: Vec::new(),
            locked_keys: BTreeSet::new(),
            lp_guard: None,
            drafts: BTreeMap::new(),
            inserts: BTreeSet::new(),
            jobs: BTreeMap::new(),
            market_questions: BTreeMap::new(),
            slot_events: BTreeSet::new(),
            events: Vec::new(),
            phase6: super::ops::Phase6Pending::default(),
        }
    }

    async fn lock_key(&mut self, key: String) {
        if self.locked_keys.insert(key.clone()) {
            self.guards
                .push(self.store.shared.key_locks.handle(&key).lock_owned().await);
        }
    }

    fn draft(&self, id: DraftId) -> Option<DraftRow> {
        self.drafts.get(&id.0).cloned().or_else(|| {
            self.store
                .shared
                .state
                .lock()
                .phase5
                .drafts
                .get(&id.0)
                .cloned()
        })
    }
}

#[async_trait]
impl ContentTx for FakeContentTx<'_> {
    async fn lock_admission(&mut self) -> Result<(), StoreError> {
        self.lock_key("content:admission".to_string()).await;
        Ok(())
    }

    async fn lock_publication(&mut self, draft: DraftId) -> Result<(), StoreError> {
        self.lock_key(format!("publish-now:{}", draft.0)).await;
        Ok(())
    }

    async fn lock_publication_queue(&mut self) -> Result<(), StoreError> {
        self.lock_key("publication-commands".to_string()).await;
        Ok(())
    }

    async fn lock_slot_tier(&mut self, tier: DraftTier) -> Result<(), StoreError> {
        self.lock_key(format!("content:slot:{tier:?}")).await;
        Ok(())
    }

    async fn lock_lp_kill_switch(&mut self) -> Result<(), StoreError> {
        if self.lp_guard.is_none() {
            self.lp_guard = Some(self.store.shared.lp_kill_lock.clone().lock_owned().await);
        }
        Ok(())
    }

    async fn pending_draft_count(&mut self) -> Result<u32, StoreError> {
        let committed = self
            .store
            .shared
            .state
            .lock()
            .phase5
            .drafts
            .values()
            .filter(|row| row.status == DraftStatus::Pending)
            .count();
        let inserted = self
            .inserts
            .iter()
            .filter(|id| {
                self.drafts
                    .get(id)
                    .is_some_and(|row| row.status == DraftStatus::Pending)
            })
            .count();
        u32::try_from(committed + inserted)
            .map_err(|_| StoreError::Invariant("pending draft count overflow"))
    }

    async fn list_drafts(&mut self, limit: u32) -> Result<Vec<DraftRow>, StoreError> {
        let mut rows: Vec<_> = self
            .store
            .shared
            .state
            .lock()
            .phase5
            .drafts
            .values()
            .cloned()
            .collect();
        rows.sort_unstable_by_key(|row| (row.publish_at, row.created_at, row.id));
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(rows)
    }

    async fn insert_draft(&mut self, draft: DraftRow) -> Result<DraftId, StoreError> {
        if self.draft(draft.id).is_some() {
            return Err(StoreError::Conflict("draft id"));
        }
        self.inserts.insert(draft.id.0);
        self.drafts.insert(draft.id.0, draft.clone());
        Ok(draft.id)
    }

    async fn draft_for_update(&mut self, draft: DraftId) -> Result<DraftRow, StoreError> {
        self.lock_key(format!("content:draft:{}", draft.0)).await;
        self.draft(draft).ok_or(StoreError::NotFound("draft"))
    }

    async fn save_draft(&mut self, draft: &DraftRow) -> Result<(), StoreError> {
        if self.draft(draft.id).is_none() {
            return Err(StoreError::NotFound("draft"));
        }
        self.drafts.insert(draft.id.0, draft.clone());
        Ok(())
    }

    async fn lock_budget_day(&mut self, day: Date) -> Result<(), StoreError> {
        self.lock_key(format!("content:budget:{day}")).await;
        Ok(())
    }

    async fn reserved_seed_for_day(&mut self, day: Date) -> Result<i64, StoreError> {
        let state = self.store.shared.state.lock();
        let committed = state.phase5.drafts.values().filter(|row| {
            matches!(row.status, DraftStatus::Approved | DraftStatus::Published)
                && row.publish_at.is_some_and(|at| utc_day(at) == day)
        });
        let mut total = 0_i64;
        for row in committed.chain(self.drafts.values().filter(|row| {
            self.inserts.contains(&row.id.0)
                && matches!(row.status, DraftStatus::Approved | DraftStatus::Published)
                && row.publish_at.is_some_and(|at| utc_day(at) == day)
        })) {
            total = total
                .checked_add(row.spec.seed.0)
                .ok_or(StoreError::Invariant("reserved seed overflow"))?;
        }
        Ok(total)
    }

    async fn lp_pnl_sum(
        &mut self,
        since: OffsetDateTime,
        until: OffsetDateTime,
    ) -> Result<i64, StoreError> {
        self.store
            .shared
            .state
            .lock()
            .lp_results
            .values()
            .filter(|(_, at)| *at >= since && *at < until)
            .try_fold(0_i64, |sum, (pnl, _)| {
                sum.checked_add(pnl.0)
                    .ok_or(StoreError::Invariant("LP PnL overflow"))
            })
    }

    async fn slot_is_reserved(
        &mut self,
        tier: DraftTier,
        at: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let state = self.store.shared.state.lock();
        Ok(state
            .phase5
            .drafts
            .values()
            .chain(self.drafts.values())
            .any(|row| row.spec.tier == tier && row.publish_at == Some(at)))
    }

    async fn due_drafts(
        &mut self,
        now: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<DraftId>, StoreError> {
        let mut rows: Vec<_> = self
            .store
            .shared
            .state
            .lock()
            .phase5
            .drafts
            .values()
            .filter(|row| row.status == DraftStatus::Approved)
            .filter(|row| row.publish_at.is_some_and(|at| at <= now))
            .map(|row| (row.publish_at, row.created_at, row.id))
            .collect();
        rows.sort_unstable();
        Ok(rows
            .into_iter()
            .take(usize::try_from(limit).unwrap_or(usize::MAX))
            .map(|(_, _, id)| id)
            .collect())
    }

    async fn due_expiries(
        &mut self,
        now: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<DraftId>, StoreError> {
        let mut rows: Vec<_> = self
            .store
            .shared
            .state
            .lock()
            .phase5
            .drafts
            .values()
            .filter(|row| row.status == DraftStatus::Pending && row.expires_at <= now)
            .map(|row| (row.expires_at, row.id))
            .collect();
        rows.sort_unstable();
        Ok(rows
            .into_iter()
            .take(usize::try_from(limit).unwrap_or(usize::MAX))
            .map(|(_, id)| id)
            .collect())
    }

    async fn reserve_market_id(&mut self, draft: DraftId) -> Result<MarketId, StoreError> {
        let mut row = self.draft(draft).ok_or(StoreError::NotFound("draft"))?;
        let market = row
            .published_market
            .unwrap_or_else(|| MarketId(Uuid::new_v4()));
        row.published_market = Some(market);
        self.drafts.insert(draft.0, row);
        Ok(market)
    }

    async fn set_market_question(
        &mut self,
        market: MarketId,
        question: &str,
    ) -> Result<(), StoreError> {
        if !self
            .store
            .shared
            .state
            .lock()
            .markets
            .contains_key(&market.0)
        {
            return Err(StoreError::NotFound("market"));
        }
        self.market_questions.insert(market.0, question.to_string());
        Ok(())
    }

    async fn enqueue_artifact_jobs(
        &mut self,
        draft: DraftId,
        market: MarketId,
        kinds: &[ArtifactKind],
        available_at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        let state = self.store.shared.state.lock();
        for kind in kinds {
            let exists = state
                .phase5
                .video_jobs
                .values()
                .chain(self.jobs.values())
                .any(|job| job.draft == Some(draft) && job.kind == *kind);
            if !exists {
                let id = JobId(Uuid::new_v4());
                self.jobs.insert(
                    id.0,
                    VideoJobRow {
                        id,
                        market,
                        draft: Some(draft),
                        kind: *kind,
                        status: JobStatus::Queued,
                        asset_url: None,
                        available_at,
                        claim_token: None,
                        lease_expires_at: None,
                        attempts: 0,
                    },
                );
            }
        }
        Ok(())
    }

    async fn claim_unfilled_slot(&mut self, slot: OffsetDateTime) -> Result<bool, StoreError> {
        let key = slot.unix_timestamp();
        self.lock_key(format!("content:unfilled:{key}")).await;
        let state = self.store.shared.state.lock();
        let occupied = state
            .phase5
            .drafts
            .values()
            .chain(self.drafts.values())
            .any(|row| {
                row.spec.tier == DraftTier::Flash
                    && row.publish_at == Some(slot)
                    && matches!(row.status, DraftStatus::Approved | DraftStatus::Published)
            });
        let claimed =
            !occupied && !state.phase5.slot_events.contains(&key) && self.slot_events.insert(key);
        Ok(claimed)
    }

    async fn record_reviewer(&mut self, _draft: DraftId, user: UserId) -> Result<(), StoreError> {
        if !self.store.shared.state.lock().users.contains_key(&user.0) {
            return Err(StoreError::NotFound("user"));
        }
        Ok(())
    }

    async fn append(&mut self, event: Event) -> Result<(), StoreError> {
        self.events.push(event);
        Ok(())
    }

    async fn insert_publication_command(
        &mut self,
        command: crate::ports::PublicationCommand,
    ) -> Result<(), StoreError> {
        let state = self.store.shared.state.lock();
        let key_taken = state
            .phase6
            .publication_command_keys
            .contains_key(&command.idempotency_key)
            || self
                .phase6
                .publication_inserts
                .iter()
                .any(|c| c.idempotency_key == command.idempotency_key);
        if key_taken {
            return Err(StoreError::Conflict("publication command key"));
        }
        let open = state.phase6.open_command_for_draft(command.draft).is_some()
            || self
                .phase6
                .publication_inserts
                .iter()
                .any(|c| is_open_command_for(c, command.draft));
        if open {
            return Err(StoreError::Conflict("publication command"));
        }
        drop(state);
        self.phase6.publication_inserts.push(command);
        Ok(())
    }

    async fn publication_command_by_draft(
        &mut self,
        draft: DraftId,
    ) -> Result<Option<crate::ports::PublicationCommand>, StoreError> {
        if let Some(pending) = self
            .phase6
            .publication_saves
            .iter()
            .rev()
            .chain(self.phase6.publication_inserts.iter().rev())
            .find(|c| c.draft == draft)
        {
            return Ok(Some(pending.clone()));
        }
        let state = self.store.shared.state.lock();
        let mut rows: Vec<&crate::ports::PublicationCommand> = state
            .phase6
            .publication_commands
            .values()
            .filter(|c| c.draft == draft)
            .collect();
        rows.sort_by_key(|c| c.id);
        Ok(rows.last().map(|c| (*c).clone()))
    }

    async fn publication_command_by_key(
        &mut self,
        key: &str,
    ) -> Result<Option<crate::ports::PublicationCommand>, StoreError> {
        if let Some(pending) = self
            .phase6
            .publication_inserts
            .iter()
            .find(|c| c.idempotency_key == key)
        {
            return Ok(Some(pending.clone()));
        }
        let state = self.store.shared.state.lock();
        Ok(state
            .phase6
            .publication_command_keys
            .get(key)
            .and_then(|id| state.phase6.publication_commands.get(id))
            .cloned())
    }

    async fn due_publication_commands(
        &mut self,
        now: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<crate::ports::PublicationCommand>, StoreError> {
        use crate::ports::PublicationCommandStatus as S;
        let state = self.store.shared.state.lock();
        let mut due: Vec<crate::ports::PublicationCommand> = state
            .phase6
            .publication_commands
            .values()
            .filter(|c| match c.status {
                S::Pending => true,
                S::Executing => c.lease_expires_at.is_none_or(|lease| lease <= now),
                S::Done | S::Failed => false,
            })
            .cloned()
            .collect();
        due.sort_by_key(|c| c.id);
        due.truncate(limit as usize);
        Ok(due)
    }

    async fn save_publication_command(
        &mut self,
        command: &crate::ports::PublicationCommand,
    ) -> Result<(), StoreError> {
        self.phase6.publication_saves.push(command.clone());
        Ok(())
    }
}

fn is_open_command_for(command: &crate::ports::PublicationCommand, draft: DraftId) -> bool {
    command.draft == draft
        && matches!(
            command.status,
            crate::ports::PublicationCommandStatus::Pending
                | crate::ports::PublicationCommandStatus::Executing
        )
}

#[async_trait]
impl crate::ports::AuditWrite for FakeContentTx<'_> {
    async fn audit_insert(&mut self, action: crate::model::AdminAction) -> Result<(), StoreError> {
        // Real D26 sink (W2): buffered, applied atomically at commit.
        self.phase6.audits.push(action);
        Ok(())
    }
}

#[async_trait]
impl Committable for FakeContentTx<'_> {
    async fn commit(self: Box<Self>) -> Result<(), StoreError> {
        let mut state = self.store.shared.state.lock();
        let mut next = state.clone();
        for id in &self.inserts {
            if next.phase5.drafts.contains_key(id) {
                return Err(StoreError::Conflict("draft id"));
            }
        }
        for row in self.drafts.values() {
            if let Some(at) = row.publish_at {
                let conflict = next.phase5.drafts.values().any(|existing| {
                    existing.id != row.id
                        && existing.spec.tier == row.spec.tier
                        && existing.publish_at == Some(at)
                });
                if conflict {
                    return Err(StoreError::Conflict("draft slot"));
                }
            }
            next.phase5.drafts.insert(row.id.0, row.clone());
        }
        for job in self.jobs.values() {
            if !next
                .phase5
                .video_jobs
                .values()
                .any(|existing| existing.draft == job.draft && existing.kind == job.kind)
            {
                next.phase5.video_jobs.insert(job.id.0, job.clone());
            }
        }
        for (market, question) in self.market_questions {
            let row = next
                .markets
                .get_mut(&market)
                .ok_or(StoreError::NotFound("market"))?;
            row.question = question;
        }
        next.phase5.slot_events.extend(self.slot_events);
        super::ops::apply_phase6(&mut next, &self.phase6)?;
        next.outbox.extend(self.events);
        *state = next;
        Ok(())
    }
}

fn utc_day(at: OffsetDateTime) -> Date {
    at.to_offset(UtcOffset::UTC).date()
}

impl InMemoryStore {
    #[must_use]
    pub fn draft(&self, id: DraftId) -> Option<DraftRow> {
        self.shared.state.lock().phase5.drafts.get(&id.0).cloned()
    }

    #[must_use]
    pub fn drafts(&self) -> Vec<DraftRow> {
        self.shared
            .state
            .lock()
            .phase5
            .drafts
            .values()
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn content_jobs(&self, draft: DraftId) -> Vec<VideoJobRow> {
        self.shared
            .state
            .lock()
            .phase5
            .video_jobs
            .values()
            .filter(|job| job.draft == Some(draft))
            .cloned()
            .collect()
    }
}

/// Explicit skeleton dependency used until the Task 5.1 engine lands.
pub struct UnavailableDraftEngine;

#[async_trait]
impl DraftEngine for UnavailableDraftEngine {
    async fn generate(
        &self,
        _request: &DraftRequest,
    ) -> Result<GeneratedDraft, DraftEngineUnavailable> {
        Err(DraftEngineUnavailable)
    }
}

/// Explicit skeleton dependency used until the Task 5.4 preflight lands.
pub struct UnavailableModerationPreflight;

#[async_trait]
impl ModerationPreflight for UnavailableModerationPreflight {
    async fn screen(&self, _body: &str) -> Result<ModerationVerdict, StoreError> {
        Err(StoreError::Unavailable("phase5:moderation-preflight"))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use domain::drafting::{DraftSource, DraftSpec};
    use domain::money::{BasisPoints, MicroUsd};

    use super::*;
    use crate::ports::Store;

    fn draft(id: DraftId, status: DraftStatus, seed: i64, at: OffsetDateTime) -> DraftRow {
        DraftRow {
            id,
            spec: DraftSpec {
                question: "Will fake branches behave?".into(),
                description: "Behavioral fake coverage.".into(),
                video_script: "Verify the transaction.".into(),
                slug: format!("fake-{}", id.0),
                tier: DraftTier::Daily,
                seed: MicroUsd(seed),
                fee: BasisPoints(100),
                min_votes_to_resolve: 3,
                open_secs: 60,
                hidden_window_secs: 10,
            },
            source: DraftSource::Template,
            fallback_from: None,
            status,
            publish_stage: None,
            published_market: None,
            publish_at: matches!(status, DraftStatus::Approved | DraftStatus::Published)
                .then_some(at),
            expires_at: at + time::Duration::hours(1),
            created_at: at,
        }
    }

    #[tokio::test]
    async fn pending_counts_and_budget_include_transaction_local_insertions() {
        let store = InMemoryStore::new();
        let at = OffsetDateTime::UNIX_EPOCH;
        let pending = draft(DraftId(Uuid::new_v4()), DraftStatus::Pending, 7, at);
        let approved = draft(DraftId(Uuid::new_v4()), DraftStatus::Approved, 11, at);
        let mut tx = store.content_tx().await.unwrap();
        tx.lock_admission().await.unwrap();
        tx.lock_admission().await.unwrap();
        tx.insert_draft(pending.clone()).await.unwrap();
        tx.insert_draft(approved.clone()).await.unwrap();
        assert_eq!(tx.pending_draft_count().await.unwrap(), 1);
        assert_eq!(tx.reserved_seed_for_day(utc_day(at)).await.unwrap(), 11);
        assert_eq!(
            tx.insert_draft(pending).await,
            Err(StoreError::Conflict("draft id"))
        );
        tx.commit().await.unwrap();
        assert_eq!(store.drafts().len(), 2);
    }

    #[tokio::test]
    async fn publication_commands_are_unique_visible_in_tx_and_lease_aware() {
        use crate::ports::{PublicationCommand, PublicationCommandStatus as S};

        let store = InMemoryStore::new();
        let draft = DraftId(Uuid::new_v4());
        let command = PublicationCommand {
            id: Uuid::new_v4(),
            draft,
            idempotency_key: "pub-key".into(),
            requested_by: "curator".into(),
            status: S::Pending,
            attempts: 0,
            lease_expires_at: None,
            result_market: None,
            error: None,
        };
        let mut tx = store.content_tx().await.unwrap();
        tx.insert_publication_command(command.clone())
            .await
            .unwrap();
        assert_eq!(
            tx.publication_command_by_key("pub-key").await.unwrap(),
            Some(command.clone())
        );
        assert_eq!(
            tx.publication_command_by_draft(draft).await.unwrap(),
            Some(command.clone())
        );
        assert!(tx
            .insert_publication_command(command.clone())
            .await
            .is_err());
        assert!(tx
            .insert_publication_command(PublicationCommand {
                id: Uuid::new_v4(),
                idempotency_key: "other-key".into(),
                ..command.clone()
            })
            .await
            .is_err());
        tx.commit().await.unwrap();

        let now = OffsetDateTime::UNIX_EPOCH;
        let mut claim = store.content_tx().await.unwrap();
        assert_eq!(
            claim.due_publication_commands(now, 10).await.unwrap().len(),
            1
        );
        let mut executing = command.clone();
        executing.status = S::Executing;
        executing.lease_expires_at = Some(now + time::Duration::seconds(10));
        claim.save_publication_command(&executing).await.unwrap();
        claim.commit().await.unwrap();
        let mut before = store.content_tx().await.unwrap();
        assert!(before
            .due_publication_commands(now, 10)
            .await
            .unwrap()
            .is_empty());
        let mut after = store.content_tx().await.unwrap();
        assert_eq!(
            after
                .due_publication_commands(now + time::Duration::seconds(10), 10)
                .await
                .unwrap()
                .len(),
            1
        );

        let second_store = InMemoryStore::new();
        let mut executing = command.clone();
        executing.status = S::Executing;
        let mut pending = second_store.content_tx().await.unwrap();
        pending
            .insert_publication_command(executing.clone())
            .await
            .unwrap();
        assert!(pending
            .insert_publication_command(PublicationCommand {
                id: Uuid::new_v4(),
                idempotency_key: "executing-conflict".into(),
                ..executing
            })
            .await
            .is_err());
        assert!(is_open_command_for(&command, draft));
        assert!(!is_open_command_for(&command, DraftId(Uuid::new_v4())));
        let mut done = command;
        done.status = S::Done;
        assert!(!is_open_command_for(&done, draft));
    }
}
