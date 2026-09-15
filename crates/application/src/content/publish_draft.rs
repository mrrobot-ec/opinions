//! Crash-recoverable publication saga.

use domain::drafting::DraftTier;
use domain::market::MarketEvent;
use serde_json::json;

use crate::advance_market::{AdvanceMarket, AdvanceMarketCmd};
use crate::error::{AppError, StoreError};
use crate::model::{
    event_type, ArtifactKind, ContentConfig, DraftId, DraftRow, DraftStatus, Event, LpKillConfig,
    MarketId, PublishStage, RepConfig,
};
use crate::ports::{Clock, Store};
use crate::seed_market::{SeedMarket, SeedMarketCmd};

use super::review_draft::validate_for_publish;
use super::template_engine::tier_config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishReceipt {
    pub draft: DraftId,
    pub market: MarketId,
    pub replayed: bool,
}

pub struct PublishDraft<'a, S: Store, C: Clock + ?Sized> {
    store: &'a S,
    clock: &'a C,
    config: &'a ContentConfig,
    rep_config: RepConfig,
    lp_kill_config: LpKillConfig,
}

impl<'a, S: Store, C: Clock + ?Sized> PublishDraft<'a, S, C> {
    #[must_use]
    pub const fn new(
        store: &'a S,
        clock: &'a C,
        config: &'a ContentConfig,
        rep_config: RepConfig,
        lp_kill_config: LpKillConfig,
    ) -> Self {
        Self {
            store,
            clock,
            config,
            rep_config,
            lp_kill_config,
        }
    }

    /// Runs the single publication authority to completion. Each persisted
    /// stage is committed independently, so re-entry resumes rather than
    /// repeating a non-idempotent effect.
    ///
    /// # Errors
    /// Returns validation, lifecycle, ledger, or store errors from the current
    /// durable saga stage.
    pub async fn execute(&self, draft: DraftId) -> Result<PublishReceipt, AppError> {
        let initially_published = self
            .store
            .content_tx()
            .await?
            .draft_for_update(draft)
            .await?
            .status
            == DraftStatus::Published;
        loop {
            if let Some(market) = self.resume_once(draft).await? {
                return Ok(PublishReceipt {
                    draft,
                    market,
                    replayed: initially_published,
                });
            }
        }
    }

    async fn resume_once(&self, draft: DraftId) -> Result<Option<MarketId>, AppError> {
        let mut tx = self.store.content_tx().await?;
        let mut row = tx.draft_for_update(draft).await?;
        if row.status == DraftStatus::Published {
            let market = published_market(&row)?;
            tx.commit().await?;
            return Ok(Some(market));
        }
        if row.status == DraftStatus::Expired {
            return Err(AppError::DraftExpired);
        }
        if row.status != DraftStatus::Approved {
            return Err(AppError::DraftNotApproved);
        }
        validate_for_publish(&row, tier_config(self.config, row.spec.tier))?;
        if row.publish_stage.is_none() {
            let market = tx.reserve_market_id(draft).await?;
            row.published_market = Some(market);
            row.publish_stage = Some(PublishStage::Claimed);
            tx.save_draft(&row).await?;
            tx.commit().await?;
            return Ok(None);
        }
        let stage = row
            .publish_stage
            .ok_or(AppError::Store(StoreError::Invariant(
                "claimed publication has no stage",
            )))?;
        tx.commit().await?;

        match stage {
            PublishStage::Claimed => self.seed(&row).await?,
            PublishStage::Seeded => self.go_live(&row).await?,
            PublishStage::Live => self.enqueue_jobs(&row).await?,
            PublishStage::JobsEnqueued => self.finish(&row).await?,
            PublishStage::Published => {
                return Err(AppError::Store(StoreError::Invariant(
                    "published stage has non-published status",
                )));
            }
        }
        Ok(None)
    }

    async fn seed(&self, row: &DraftRow) -> Result<(), AppError> {
        let market = published_market(row)?;
        let open = i64::try_from(row.spec.open_secs).map_err(|_| AppError::Overflow)?;
        let hidden = i64::try_from(row.spec.hidden_window_secs).map_err(|_| AppError::Overflow)?;
        let closes_at = self
            .clock
            .now()
            .checked_add(time::Duration::seconds(open))
            .ok_or(AppError::Overflow)?;
        let tally_hidden_at = closes_at
            .checked_sub(time::Duration::seconds(hidden))
            .ok_or(AppError::Overflow)?;
        SeedMarket {
            store: self.store,
            clock: self.clock,
            rep_config: self.rep_config,
            lp_kill_config: self.lp_kill_config,
        }
        .execute(SeedMarketCmd {
            market_id: market,
            slug: row.spec.slug.clone(),
            min_votes_to_resolve: row.spec.min_votes_to_resolve,
            closes_at,
            tally_hidden_at,
            fee: row.spec.fee,
            seed: row.spec.seed,
            idempotency_key: seed_key(row.id),
            force: false,
        })
        .await?;
        let mut content = self.store.content_tx().await?;
        content
            .set_market_question(market, &row.spec.question)
            .await?;
        content.commit().await?;
        self.advance_stage(row.id, PublishStage::Claimed, PublishStage::Seeded)
            .await
    }

    async fn go_live(&self, row: &DraftRow) -> Result<(), AppError> {
        let market = published_market(row)?;
        AdvanceMarket { store: self.store }
            .execute(AdvanceMarketCmd {
                market,
                event: MarketEvent::GoLive,
                idempotency_key: go_live_key(row.id),
            })
            .await?;
        self.advance_stage(row.id, PublishStage::Seeded, PublishStage::Live)
            .await
    }

    async fn enqueue_jobs(&self, row: &DraftRow) -> Result<(), AppError> {
        let market = published_market(row)?;
        let kinds: &[ArtifactKind] = match row.spec.tier {
            DraftTier::Flash => &[ArtifactKind::Poster],
            DraftTier::Daily => &[ArtifactKind::Poster, ArtifactKind::MarketVideo],
        };
        let mut tx = self.store.content_tx().await?;
        let current = tx.draft_for_update(row.id).await?;
        if current.publish_stage == Some(PublishStage::Live) {
            tx.enqueue_artifact_jobs(row.id, market, kinds, self.clock.now())
                .await?;
        }
        tx.commit().await?;
        self.advance_stage(row.id, PublishStage::Live, PublishStage::JobsEnqueued)
            .await
    }

    async fn finish(&self, row: &DraftRow) -> Result<(), AppError> {
        let market = published_market(row)?;
        let mut tx = self.store.content_tx().await?;
        let mut current = tx.draft_for_update(row.id).await?;
        if current.publish_stage == Some(PublishStage::JobsEnqueued) {
            current.status = DraftStatus::Published;
            current.publish_stage = Some(PublishStage::Published);
            tx.save_draft(&current).await?;
            tx.append(Event {
                event_type: event_type::DRAFT_PUBLISHED,
                aggregate_type: "draft",
                aggregate_id: row.id.0,
                payload: json!({
                    "market_id": market.0,
                    "tier": tier_name(row.spec.tier),
                }),
            })
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn advance_stage(
        &self,
        draft: DraftId,
        expected: PublishStage,
        next: PublishStage,
    ) -> Result<(), AppError> {
        let mut tx = self.store.content_tx().await?;
        let mut row = tx.draft_for_update(draft).await?;
        if row.publish_stage == Some(expected) {
            row.publish_stage = Some(next);
            tx.save_draft(&row).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

fn published_market(row: &DraftRow) -> Result<MarketId, AppError> {
    row.published_market
        .ok_or(AppError::Store(StoreError::Invariant(
            "publication stage without market id",
        )))
}

fn seed_key(draft: DraftId) -> String {
    format!("draft:{}:seed", draft.0)
}

fn go_live_key(draft: DraftId) -> String {
    format!("draft:{}:golive", draft.0)
}

const fn tier_name(tier: DraftTier) -> &'static str {
    match tier {
        DraftTier::Daily => "daily",
        DraftTier::Flash => "flash",
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use domain::drafting::{DraftSource, DraftTier};
    use domain::ledger::Currency;
    use time::{Duration, OffsetDateTime};

    use super::*;
    use crate::content::create_draft::{CreateDraft, CreateDraftCmd};
    use crate::content::review_draft::ReviewDraft;
    use crate::content::template_engine::TemplateDraftEngine;
    use crate::ensure_genesis::{EnsureGenesis, EnsureGenesisCmd};
    use crate::fakes::{FakeClock, InMemoryStore};
    use crate::model::{ContentConfig, DraftStatus, JobStatus, LpKillConfig, OwnerRef, RepConfig};
    use crate::ports::{Clock, Store};

    async fn approved(
        store: &InMemoryStore,
        clock: &FakeClock,
        config: &ContentConfig,
        topic: &str,
        tier: DraftTier,
    ) -> crate::model::DraftRow {
        let engine = TemplateDraftEngine::new(config.clone());
        let draft = CreateDraft {
            store,
            clock,
            config,
            primary: &engine,
            template: &engine,
        }
        .execute(CreateDraftCmd {
            topics: vec![topic.into()],
            tier,
            requested_source: DraftSource::Template,
            allow_fallback: false,
        })
        .await
        .unwrap()
        .drafts
        .remove(0);
        let reviewer = store.add_user(topic, clock.now() - Duration::days(2), 2);
        ReviewDraft {
            store,
            clock,
            config,
            lp_kill_config: LpKillConfig::default(),
        }
        .approve(draft.id, reviewer)
        .await
        .unwrap()
    }

    async fn capitalized(store: &InMemoryStore, amount: i64) {
        EnsureGenesis { store }
            .execute(EnsureGenesisCmd {
                currency: Currency::Usdc,
                amount: domain::money::MicroUsd(amount),
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn full_saga_is_replay_safe_and_tier_jobs_are_exact() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_006_400).unwrap();
        let clock = FakeClock::at(now);
        let store = InMemoryStore::new();
        capitalized(&store, 1_000_000_000).await;
        let config = ContentConfig::default();
        let daily = approved(&store, &clock, &config, "daily", DraftTier::Daily).await;
        let flash = approved(&store, &clock, &config, "flash", DraftTier::Flash).await;
        let saga = PublishDraft::new(
            &store,
            &clock,
            &config,
            RepConfig::default(),
            LpKillConfig::default(),
        );
        let first = saga.execute(daily.id).await.unwrap();
        let second = saga.execute(flash.id).await.unwrap();
        assert!(!first.replayed && !second.replayed);
        assert_eq!(
            store.draft(daily.id).unwrap().status,
            DraftStatus::Published
        );
        assert_eq!(store.content_jobs(daily.id).len(), 2);
        assert_eq!(store.content_jobs(flash.id).len(), 1);
        assert!(store
            .content_jobs(daily.id)
            .iter()
            .all(|job| job.status == JobStatus::Queued));
        let balance = store.balance_of(OwnerRef::House, Currency::Usdc).unwrap();
        let replay = saga.execute(daily.id).await.unwrap();
        assert!(replay.replayed);
        assert_eq!(
            store.balance_of(OwnerRef::House, Currency::Usdc),
            Some(balance)
        );
        assert_eq!(store.content_jobs(daily.id).len(), 2);
        assert_eq!(
            store
                .outbox()
                .iter()
                .filter(|event| event.event_type == crate::model::event_type::DRAFT_PUBLISHED)
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn concurrent_sweep_and_publish_now_converge_on_one_market() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_006_400).unwrap();
        let clock = FakeClock::at(now);
        let store = InMemoryStore::new();
        capitalized(&store, 500_000_000).await;
        let config = ContentConfig::default();
        let draft = approved(&store, &clock, &config, "race", DraftTier::Flash).await;
        let a = PublishDraft::new(
            &store,
            &clock,
            &config,
            RepConfig::default(),
            LpKillConfig::default(),
        );
        let b = PublishDraft::new(
            &store,
            &clock,
            &config,
            RepConfig::default(),
            LpKillConfig::default(),
        );
        let (left, right) = tokio::join!(a.execute(draft.id), b.execute(draft.id));
        assert_eq!(left.unwrap().market, right.unwrap().market);
        assert_eq!(store.content_jobs(draft.id).len(), 1);
        assert_eq!(
            store
                .outbox()
                .iter()
                .filter(|event| event.event_type == crate::model::event_type::DRAFT_PUBLISHED)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn resumes_after_every_effect_stage_and_revalidates_before_claim() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_006_400).unwrap();
        let clock = FakeClock::at(now);
        let store = InMemoryStore::new();
        capitalized(&store, 1_000_000_000).await;
        let config = ContentConfig::default();
        let saga = PublishDraft::new(
            &store,
            &clock,
            &config,
            RepConfig::default(),
            LpKillConfig::default(),
        );

        let claim_gap = approved(&store, &clock, &config, "claim-gap", DraftTier::Daily).await;
        assert_eq!(saga.resume_once(claim_gap.id).await.unwrap(), None);
        assert_eq!(
            store.draft(claim_gap.id).unwrap().publish_stage,
            Some(crate::model::PublishStage::Claimed)
        );
        saga.execute(claim_gap.id).await.unwrap();

        let seed_gap = approved(&store, &clock, &config, "seed-gap", DraftTier::Daily).await;
        saga.resume_once(seed_gap.id).await.unwrap();
        let claimed = store.draft(seed_gap.id).unwrap();
        manual_seed(&store, &clock, &claimed).await;
        saga.execute(seed_gap.id).await.unwrap();

        let live_gap = approved(&store, &clock, &config, "live-gap", DraftTier::Daily).await;
        saga.resume_once(live_gap.id).await.unwrap();
        saga.resume_once(live_gap.id).await.unwrap();
        let seeded = store.draft(live_gap.id).unwrap();
        crate::advance_market::AdvanceMarket { store: &store }
            .execute(crate::advance_market::AdvanceMarketCmd {
                market: seeded.published_market.unwrap(),
                event: domain::market::MarketEvent::GoLive,
                idempotency_key: go_live_key(live_gap.id),
            })
            .await
            .unwrap();
        saga.execute(live_gap.id).await.unwrap();

        let jobs_gap = approved(&store, &clock, &config, "jobs-gap", DraftTier::Flash).await;
        saga.resume_once(jobs_gap.id).await.unwrap();
        saga.resume_once(jobs_gap.id).await.unwrap();
        saga.resume_once(jobs_gap.id).await.unwrap();
        let live = store.draft(jobs_gap.id).unwrap();
        let mut tx = store.content_tx().await.unwrap();
        tx.enqueue_artifact_jobs(
            jobs_gap.id,
            live.published_market.unwrap(),
            &[crate::model::ArtifactKind::Poster],
            now,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        saga.execute(jobs_gap.id).await.unwrap();
        assert_eq!(store.content_jobs(jobs_gap.id).len(), 1);

        let invalid = approved(&store, &clock, &config, "invalid-final", DraftTier::Flash).await;
        let mut tx = store.content_tx().await.unwrap();
        let mut corrupt = tx.draft_for_update(invalid.id).await.unwrap();
        corrupt.spec.seed.0 = config.tier_defaults[1].seed_floor_micro - 1;
        tx.save_draft(&corrupt).await.unwrap();
        tx.commit().await.unwrap();
        assert!(matches!(
            saga.execute(invalid.id).await,
            Err(crate::error::AppError::InvalidDraft(_))
        ));

        let pending = {
            let engine = TemplateDraftEngine::new(config.clone());
            CreateDraft {
                store: &store,
                clock: &clock,
                config: &config,
                primary: &engine,
                template: &engine,
            }
            .execute(CreateDraftCmd {
                topics: vec!["pending".into()],
                tier: DraftTier::Flash,
                requested_source: DraftSource::Template,
                allow_fallback: false,
            })
            .await
            .unwrap()
            .drafts[0]
                .clone()
        };
        assert_eq!(
            saga.execute(pending.id).await,
            Err(crate::error::AppError::DraftNotApproved)
        );
    }

    #[tokio::test]
    async fn expired_corrupt_and_stale_stage_rows_fail_or_noop_explicitly() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_006_400).unwrap();
        let clock = FakeClock::at(now);
        let store = InMemoryStore::new();
        capitalized(&store, 1_000_000_000).await;
        let config = ContentConfig::default();
        let saga = PublishDraft::new(
            &store,
            &clock,
            &config,
            RepConfig::default(),
            LpKillConfig::default(),
        );

        let expired = approved(&store, &clock, &config, "expired-stage", DraftTier::Flash).await;
        let mut tx = store.content_tx().await.unwrap();
        let mut expired_row = tx.draft_for_update(expired.id).await.unwrap();
        expired_row.status = DraftStatus::Expired;
        tx.save_draft(&expired_row).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(saga.execute(expired.id).await, Err(AppError::DraftExpired));

        let stale = approved(&store, &clock, &config, "stale-stage", DraftTier::Flash).await;
        let market = MarketId(uuid::Uuid::new_v4());
        let mut tx = store.content_tx().await.unwrap();
        let mut stale_row = tx.draft_for_update(stale.id).await.unwrap();
        stale_row.published_market = Some(market);
        stale_row.publish_stage = Some(PublishStage::JobsEnqueued);
        tx.save_draft(&stale_row).await.unwrap();
        tx.commit().await.unwrap();
        saga.enqueue_jobs(&stale_row).await.unwrap();
        assert!(store.content_jobs(stale.id).is_empty());

        let mut tx = store.content_tx().await.unwrap();
        let mut corrupt = tx.draft_for_update(stale.id).await.unwrap();
        corrupt.publish_stage = Some(PublishStage::Published);
        tx.save_draft(&corrupt).await.unwrap();
        tx.commit().await.unwrap();
        saga.finish(&corrupt).await.unwrap();
        assert!(matches!(
            saga.resume_once(stale.id).await,
            Err(AppError::Store(StoreError::Invariant(
                "published stage has non-published status"
            )))
        ));
    }

    async fn manual_seed(store: &InMemoryStore, clock: &FakeClock, row: &crate::model::DraftRow) {
        let closes_at = clock.now() + Duration::seconds(i64::try_from(row.spec.open_secs).unwrap());
        crate::seed_market::SeedMarket {
            store,
            clock,
            rep_config: RepConfig::default(),
            lp_kill_config: LpKillConfig::default(),
        }
        .execute(crate::seed_market::SeedMarketCmd {
            market_id: row.published_market.unwrap(),
            slug: row.spec.slug.clone(),
            min_votes_to_resolve: row.spec.min_votes_to_resolve,
            closes_at,
            tally_hidden_at: closes_at
                - Duration::seconds(i64::try_from(row.spec.hidden_window_secs).unwrap()),
            fee: row.spec.fee,
            seed: row.spec.seed,
            idempotency_key: seed_key(row.id),
            force: false,
        })
        .await
        .unwrap();
    }
}
