//! Curator edit, reject, and approve transitions.

use domain::drafting::{next_daily_slot, next_flash_slot, DraftSpec, DraftTier};
use serde_json::json;
use time::{OffsetDateTime, UtcOffset};

use crate::error::AppError;
use crate::model::{
    event_type, ContentConfig, ContentTierConfig, DraftId, DraftRow, DraftStatus, Event,
    LpKillConfig, UserId,
};
use crate::ports::{Clock, Store};

use super::create_draft::invalid_draft;
use super::expiry::expire_locked;
use super::template_engine::tier_config;

pub struct ReviewDraft<'a, S: Store + ?Sized, C: Clock + ?Sized> {
    pub store: &'a S,
    pub clock: &'a C,
    pub config: &'a ContentConfig,
    pub lp_kill_config: LpKillConfig,
}

impl<S: Store + ?Sized, C: Clock + ?Sized> ReviewDraft<'_, S, C> {
    /// Replaces an unexpired pending draft's specification.
    ///
    /// # Errors
    /// Returns validation, state, expiry, reviewer, or store errors.
    pub async fn edit(
        &self,
        draft: DraftId,
        reviewer: UserId,
        spec: DraftSpec,
    ) -> Result<DraftRow, AppError> {
        spec.validate().map_err(invalid_draft)?;
        let mut tx = self.store.content_tx().await?;
        let mut row = tx.draft_for_update(draft).await?;
        if expire_locked(&mut row, self.clock.now()) {
            tx.save_draft(&row).await?;
            tx.commit().await?;
            return Err(AppError::DraftExpired);
        }
        ensure_pending(&row)?;
        row.spec = spec;
        tx.save_draft(&row).await?;
        tx.record_reviewer(draft, reviewer).await?;
        tx.commit().await?;
        Ok(row)
    }

    /// Rejects an unexpired pending draft.
    ///
    /// # Errors
    /// Returns state, expiry, reviewer, or store errors.
    pub async fn reject(&self, draft: DraftId, reviewer: UserId) -> Result<DraftRow, AppError> {
        self.reject_as(draft, reviewer, &crate::model::AdminContext::Machine)
            .await
    }

    /// D26 actor threading: the rejection and its audit fact commit in ONE
    /// transaction for admin actors.
    ///
    /// # Errors
    /// As [`Self::reject`].
    pub async fn reject_as(
        &self,
        draft: DraftId,
        reviewer: UserId,
        actor: &crate::model::AdminContext,
    ) -> Result<DraftRow, AppError> {
        let mut tx = self.store.content_tx().await?;
        let mut row = tx.draft_for_update(draft).await?;
        if expire_locked(&mut row, self.clock.now()) {
            tx.save_draft(&row).await?;
            tx.commit().await?;
            return Err(AppError::DraftExpired);
        }
        ensure_pending(&row)?;
        row.status = DraftStatus::Rejected;
        tx.save_draft(&row).await?;
        tx.record_reviewer(draft, reviewer).await?;
        if let Some(audit) = crate::ops::audit::audit_for(
            actor,
            "reject_draft",
            format!("draft:{}", draft.0),
            None,
            Some(serde_json::json!({ "status": "rejected" })),
            None,
        ) {
            tx.audit_insert(audit).await?;
        }
        tx.commit().await?;
        Ok(row)
    }

    /// Validates floors and atomically reserves capital and a publication slot.
    ///
    /// # Errors
    /// Returns validation, state, expiry, breaker, budget, slot, or store errors.
    pub async fn approve(&self, draft: DraftId, reviewer: UserId) -> Result<DraftRow, AppError> {
        self.approve_as(draft, reviewer, &crate::model::AdminContext::Machine)
            .await
    }

    /// D26 actor threading: the approval (capital + slot reservation) and
    /// its audit fact commit in ONE transaction for admin actors.
    ///
    /// # Errors
    /// As [`Self::approve`].
    pub async fn approve_as(
        &self,
        draft: DraftId,
        reviewer: UserId,
        actor: &crate::model::AdminContext,
    ) -> Result<DraftRow, AppError> {
        let now = self.clock.now();
        let mut tx = self.store.content_tx().await?;
        let mut row = tx.draft_for_update(draft).await?;
        if expire_locked(&mut row, now) {
            tx.save_draft(&row).await?;
            tx.commit().await?;
            return Err(AppError::DraftExpired);
        }
        ensure_pending(&row)?;
        let tier = tier_config(self.config, row.spec.tier);
        validate_for_publish(&row, tier)?;

        tx.lock_lp_kill_switch().await?;
        let since = now - time::Duration::days(i64::from(self.lp_kill_config.window_days));
        let lp_pnl = tx.lp_pnl_sum(since, now).await?;
        if lp_pnl <= -self.lp_kill_config.max_loss_micro {
            return Err(AppError::LpPaused);
        }

        tx.lock_slot_tier(row.spec.tier).await?;
        let slot = reserve_slot(&mut *tx, row.spec.tier, now, self.config).await?;
        let day = slot.to_offset(UtcOffset::UTC).date();
        tx.lock_budget_day(day).await?;
        let reserved = tx.reserved_seed_for_day(day).await?;
        if reserved
            .checked_add(row.spec.seed.0)
            .ok_or(AppError::Overflow)?
            > self.config.daily_seed_budget_micro
        {
            return Err(AppError::DailySeedBudgetExceeded);
        }

        row.status = DraftStatus::Approved;
        row.publish_at = Some(slot);
        tx.save_draft(&row).await?;
        tx.record_reviewer(draft, reviewer).await?;
        if let Some(audit) = crate::ops::audit::audit_for(
            actor,
            "approve_draft",
            format!("draft:{}", draft.0),
            None,
            Some(json!({ "publish_at": slot.unix_timestamp(), "seed_micro": row.spec.seed.0 })),
            None,
        ) {
            tx.audit_insert(audit).await?;
        }
        tx.append(Event {
            event_type: event_type::DRAFT_APPROVED,
            aggregate_type: "draft",
            aggregate_id: draft.0,
            payload: json!({
                "publish_at": slot.unix_timestamp(),
                "seed_micro": row.spec.seed.0,
                "tier": tier_name(row.spec.tier),
            }),
        })
        .await?;
        tx.commit().await?;
        Ok(row)
    }
}

pub(super) fn validate_for_publish(
    row: &DraftRow,
    tier: ContentTierConfig,
) -> Result<(), AppError> {
    row.spec
        .validate_floors(tier.seed_floor_micro, tier.min_votes_floor)
        .map_err(invalid_draft)
}

fn ensure_pending(row: &DraftRow) -> Result<(), AppError> {
    match row.status {
        DraftStatus::Pending => Ok(()),
        DraftStatus::Expired => Err(AppError::DraftExpired),
        DraftStatus::Approved | DraftStatus::Rejected | DraftStatus::Published => {
            Err(AppError::DraftNotPending)
        }
    }
}

async fn reserve_slot(
    tx: &mut dyn crate::ports::ContentTx,
    tier: DraftTier,
    now: OffsetDateTime,
    config: &ContentConfig,
) -> Result<OffsetDateTime, AppError> {
    let horizon_secs =
        i64::try_from(config.max_slot_horizon_secs).map_err(|_| AppError::Overflow)?;
    let horizon = now
        .checked_add(time::Duration::seconds(horizon_secs))
        .ok_or(AppError::Overflow)?;
    let mut slot = next_slot(tier, now, config)?;
    while slot <= horizon {
        if !tx.slot_is_reserved(tier, slot).await? {
            return Ok(slot);
        }
        slot = next_slot(tier, slot, config)?;
    }
    Err(AppError::NoSlotFree)
}

fn next_slot(
    tier: DraftTier,
    after: OffsetDateTime,
    config: &ContentConfig,
) -> Result<OffsetDateTime, AppError> {
    let unix = match tier {
        DraftTier::Daily => next_daily_slot(after.unix_timestamp(), config.daily_slots),
        DraftTier::Flash => next_flash_slot(after.unix_timestamp(), config.flash_cadence_secs),
    }
    .map_err(invalid_draft)?;
    OffsetDateTime::from_unix_timestamp(unix).map_err(|_| AppError::Overflow)
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
    use time::{Duration, OffsetDateTime};

    use super::*;
    use crate::content::create_draft::{CreateDraft, CreateDraftCmd};
    use crate::content::template_engine::TemplateDraftEngine;
    use crate::fakes::{FakeClock, InMemoryStore};
    use crate::model::{AdminContext, AdminRole, ContentConfig, DraftStatus, LpKillConfig};
    use crate::ports::{Clock, OpsQueries};

    async fn pending(
        store: &InMemoryStore,
        clock: &FakeClock,
        config: &ContentConfig,
        topic: &str,
        tier: DraftTier,
    ) -> crate::model::DraftRow {
        let engine = TemplateDraftEngine::new(config.clone());
        CreateDraft {
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
        .remove(0)
    }

    #[tokio::test]
    async fn approval_reserves_and_bumps_slots_then_edit_and_reject_are_pending_only() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_006_400).unwrap();
        let clock = FakeClock::at(now);
        let store = InMemoryStore::new();
        let config = ContentConfig::default();
        let reviewer = store.add_user("curator", now - Duration::days(10), 2);
        let first = pending(&store, &clock, &config, "first", DraftTier::Flash).await;
        let second = pending(&store, &clock, &config, "second", DraftTier::Flash).await;
        let rejected = pending(&store, &clock, &config, "third", DraftTier::Daily).await;
        let machine_rejected = pending(&store, &clock, &config, "fourth", DraftTier::Daily).await;
        let review = ReviewDraft {
            store: &store,
            clock: &clock,
            config: &config,
            lp_kill_config: LpKillConfig::default(),
        };

        let mut edited = rejected.spec.clone();
        edited.question = "Will the edited question pass?".into();
        let edited_row = review
            .edit(rejected.id, reviewer, edited.clone())
            .await
            .unwrap();
        assert_eq!(edited_row.spec, edited);
        let actor = AdminContext::Admin {
            token_digest: "curator-token".into(),
            role: AdminRole::Curator,
        };
        assert_eq!(
            review
                .reject_as(rejected.id, reviewer, &actor)
                .await
                .unwrap()
                .status,
            DraftStatus::Rejected
        );
        assert_eq!(
            review.edit(rejected.id, reviewer, edited).await,
            Err(crate::error::AppError::DraftNotPending)
        );
        assert_eq!(
            review
                .reject(machine_rejected.id, reviewer)
                .await
                .unwrap()
                .status,
            DraftStatus::Rejected
        );

        let a = review.approve_as(first.id, reviewer, &actor).await.unwrap();
        let b = review.approve(second.id, reviewer).await.unwrap();
        assert_eq!(a.status, DraftStatus::Approved);
        assert!(b.publish_at.unwrap() > a.publish_at.unwrap());
        assert_eq!(
            b.publish_at.unwrap() - a.publish_at.unwrap(),
            Duration::seconds(i64::try_from(config.flash_cadence_secs).unwrap())
        );
        let actions: Vec<String> = store
            .audit_page(None, 10)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.action.action)
            .collect();
        assert!(actions.contains(&"reject_draft".to_string()));
        assert!(actions.contains(&"approve_draft".to_string()));
    }

    #[tokio::test]
    async fn approval_refuses_floors_lp_breaker_budget_and_exhausted_horizon() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_006_400).unwrap();
        let clock = FakeClock::at(now);
        let store = InMemoryStore::new();
        let reviewer = store.add_user("curator", now - Duration::days(10), 2);
        let mut config = ContentConfig::default();
        config.daily_seed_budget_micro = config.tier_defaults[1].seed_micro;
        config.max_slot_horizon_secs = config.flash_cadence_secs;
        let first = pending(&store, &clock, &config, "first", DraftTier::Flash).await;
        let second = pending(&store, &clock, &config, "second", DraftTier::Flash).await;
        let review = ReviewDraft {
            store: &store,
            clock: &clock,
            config: &config,
            lp_kill_config: LpKillConfig::default(),
        };
        review.approve(first.id, reviewer).await.unwrap();
        assert_eq!(
            review.approve(second.id, reviewer).await,
            Err(crate::error::AppError::NoSlotFree)
        );

        let mut budget_config = config.clone();
        budget_config.max_slot_horizon_secs *= 2;
        let review = ReviewDraft {
            config: &budget_config,
            ..review
        };
        assert_eq!(
            review.approve(second.id, reviewer).await,
            Err(crate::error::AppError::DailySeedBudgetExceeded)
        );

        let low = pending(&store, &clock, &budget_config, "low", DraftTier::Daily).await;
        let mut low_spec = low.spec.clone();
        low_spec.seed.0 = budget_config.tier_defaults[0].seed_floor_micro - 1;
        review.edit(low.id, reviewer, low_spec).await.unwrap();
        assert!(matches!(
            review.approve(low.id, reviewer).await,
            Err(crate::error::AppError::InvalidDraft(_))
        ));

        let paused = pending(&store, &clock, &budget_config, "paused", DraftTier::Daily).await;
        store.record_lp_result(
            crate::model::MarketId(uuid::Uuid::new_v4()),
            domain::money::MicroUsd(-LpKillConfig::default().max_loss_micro),
            now - Duration::hours(1),
        );
        assert_eq!(
            review.approve(paused.id, reviewer).await,
            Err(crate::error::AppError::LpPaused)
        );
    }

    #[tokio::test]
    async fn day_budget_and_expiry_transitions_serialize_under_races() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_006_400).unwrap();
        let clock = FakeClock::at(now);
        let store = InMemoryStore::new();
        let reviewer = store.add_user("curator", now - Duration::days(10), 2);
        let mut config = ContentConfig::default();
        config.daily_seed_budget_micro = config.tier_defaults[1].seed_micro;
        let first = pending(&store, &clock, &config, "budget-a", DraftTier::Flash).await;
        let second = pending(&store, &clock, &config, "budget-b", DraftTier::Flash).await;
        let left = ReviewDraft {
            store: &store,
            clock: &clock,
            config: &config,
            lp_kill_config: LpKillConfig::default(),
        };
        let right = ReviewDraft { ..left };
        let (a, b) = tokio::join!(
            left.approve(first.id, reviewer),
            right.approve(second.id, reviewer)
        );
        assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
        assert_eq!(
            usize::from(a == Err(crate::error::AppError::DailySeedBudgetExceeded))
                + usize::from(b == Err(crate::error::AppError::DailySeedBudgetExceeded)),
            1
        );

        let mut expiry_config = config.clone();
        expiry_config.draft_ttl_secs = 1;
        let expiring = pending(
            &store,
            &clock,
            &expiry_config,
            "expiry-race",
            DraftTier::Daily,
        )
        .await;
        clock.advance(Duration::seconds(1));
        let review = ReviewDraft {
            store: &store,
            clock: &clock,
            config: &expiry_config,
            lp_kill_config: LpKillConfig::default(),
        };
        let (sweep, approve) = tokio::join!(
            crate::content::expiry::expire_draft(&store, expiring.id, clock.now()),
            review.approve(expiring.id, reviewer)
        );
        assert!(matches!(sweep, Ok(true | false)));
        assert_eq!(approve, Err(crate::error::AppError::DraftExpired));
        assert_eq!(
            store.draft(expiring.id).unwrap().status,
            DraftStatus::Expired
        );
    }

    #[tokio::test]
    async fn each_curator_action_persists_expiry_at_the_boundary() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_006_400).unwrap();
        let clock = FakeClock::at(now);
        let store = InMemoryStore::new();
        let reviewer = store.add_user("expiry-curator", now - Duration::days(10), 2);
        let config = ContentConfig {
            draft_ttl_secs: 1,
            ..ContentConfig::default()
        };
        let edit = pending(&store, &clock, &config, "expired-edit", DraftTier::Daily).await;
        let reject = pending(&store, &clock, &config, "expired-reject", DraftTier::Daily).await;
        let approve = pending(&store, &clock, &config, "expired-approve", DraftTier::Daily).await;
        clock.advance(Duration::seconds(1));
        let review = ReviewDraft {
            store: &store,
            clock: &clock,
            config: &config,
            lp_kill_config: LpKillConfig::default(),
        };

        assert_eq!(
            review.edit(edit.id, reviewer, edit.spec).await,
            Err(crate::error::AppError::DraftExpired)
        );
        assert_eq!(
            review.reject(reject.id, reviewer).await,
            Err(crate::error::AppError::DraftExpired)
        );
        assert_eq!(
            review.approve(approve.id, reviewer).await,
            Err(crate::error::AppError::DraftExpired)
        );
        for id in [edit.id, reject.id, approve.id] {
            assert_eq!(store.draft(id).unwrap().status, DraftStatus::Expired);
        }
    }
}
