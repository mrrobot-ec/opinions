use domain::drafting::next_flash_slot;
use serde_json::json;

use crate::error::{AppError, StoreError};
use crate::model::{event_type, ContentConfig, Event, LpKillConfig, RepConfig};
use crate::ports::{Clock, Store};

use super::expiry::expire_due;
use super::publish_draft::PublishDraft;
use super::publish_now_command::consume_due_commands;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublisherSweep {
    pub expired: u32,
    pub published: u32,
    pub slot_unfilled: bool,
}

/// Runs the bounded expiry arm, then publishes due drafts through the one saga
/// authority. An empty due set records the most recently lapsed flash slot at
/// most once.
///
/// # Errors
/// Returns publication, overflow, configuration, or store errors from the
/// bounded sweep.
pub async fn publisher_tick<S: Store, C: Clock + ?Sized>(
    store: &S,
    clock: &C,
    config: &ContentConfig,
    rep_config: RepConfig,
    lp_kill_config: LpKillConfig,
) -> Result<PublisherSweep, AppError> {
    let now = clock.now();
    let limit = config.max_pending_drafts;
    let expired = expire_due(store, now, limit).await?;
    // Publish-now commands are consumed FIRST (D26 / codex r3 NEW-4): the
    // authorization is already audited; execution rides the same saga.
    let commanded = consume_due_commands(store, clock, config, rep_config, lp_kill_config).await?;
    let mut query = store.content_tx().await?;
    let due = query.due_drafts(now, limit).await?;
    query.commit().await?;
    let mut published = commanded;
    for draft in &due {
        let saga = PublishDraft::new(store, clock, config, rep_config, lp_kill_config);
        saga.execute(*draft).await?;
        published = published.checked_add(1).ok_or(AppError::Overflow)?;
    }
    let slot_unfilled = if due.is_empty() {
        record_unfilled(store, now, config.flash_cadence_secs).await?
    } else {
        false
    };
    Ok(PublisherSweep {
        expired,
        published,
        slot_unfilled,
    })
}

async fn record_unfilled<S: Store>(
    store: &S,
    now: time::OffsetDateTime,
    cadence_secs: u64,
) -> Result<bool, AppError> {
    let next = next_flash_slot(now.unix_timestamp(), cadence_secs)
        .map_err(|_| AppError::Store(StoreError::Invariant("invalid flash cadence")))?;
    let cadence = i64::try_from(cadence_secs).map_err(|_| AppError::Overflow)?;
    let unix = next.checked_sub(cadence).ok_or(AppError::Overflow)?;
    let slot = time::OffsetDateTime::from_unix_timestamp(unix).map_err(|_| AppError::Overflow)?;
    let mut tx = store.content_tx().await?;
    let claimed = tx.claim_unfilled_slot(slot).await?;
    if claimed {
        let event = Event {
            event_type: event_type::SLOT_UNFILLED,
            aggregate_type: "publication_slot",
            aggregate_id: uuid::Uuid::from_u128(i128::from(unix).cast_unsigned()),
            payload: json!({ "slot_unix": unix, "tier": "flash" }),
        };
        tx.append(event).await?;
    }
    tx.commit().await.map(|()| claimed).map_err(AppError::from)
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
    use crate::model::DraftStatus;

    #[tokio::test]
    async fn empty_slot_and_expiry_are_each_recorded_once() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_006_400).unwrap();
        let clock = FakeClock::at(now);
        let store = InMemoryStore::new();
        let config = ContentConfig {
            draft_ttl_secs: 1,
            ..ContentConfig::default()
        };
        let engine = TemplateDraftEngine::new(config.clone());
        let draft = CreateDraft {
            store: &store,
            clock: &clock,
            config: &config,
            primary: &engine,
            template: &engine,
        }
        .execute(CreateDraftCmd {
            topics: vec!["expires".into()],
            tier: DraftTier::Flash,
            requested_source: DraftSource::Template,
            allow_fallback: false,
        })
        .await
        .unwrap()
        .drafts[0]
            .clone();
        clock.advance(Duration::seconds(1));
        let first = publisher_tick(
            &store,
            &clock,
            &config,
            RepConfig::default(),
            LpKillConfig::default(),
        )
        .await
        .unwrap();
        let second = publisher_tick(
            &store,
            &clock,
            &config,
            RepConfig::default(),
            LpKillConfig::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            first,
            PublisherSweep {
                expired: 1,
                published: 0,
                slot_unfilled: true
            }
        );
        assert_eq!(
            second,
            PublisherSweep {
                expired: 0,
                published: 0,
                slot_unfilled: false
            }
        );
        assert_eq!(store.draft(draft.id).unwrap().status, DraftStatus::Expired);
        assert_eq!(
            store
                .outbox()
                .iter()
                .filter(|event| event.event_type == event_type::SLOT_UNFILLED)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn unfilled_slot_rejects_an_invalid_cadence_without_writing() {
        let store = InMemoryStore::new();
        assert_eq!(
            record_unfilled(&store, OffsetDateTime::UNIX_EPOCH, 0).await,
            Err(AppError::Store(StoreError::Invariant(
                "invalid flash cadence"
            )))
        );
        assert!(store.outbox().is_empty());
    }

    #[tokio::test]
    async fn a_published_slot_is_never_reported_unfilled_on_a_later_tick() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_006_400).unwrap();
        let clock = FakeClock::at(now);
        let store = InMemoryStore::new();
        let config = ContentConfig::default();
        EnsureGenesis { store: &store }
            .execute(EnsureGenesisCmd {
                currency: Currency::Usdc,
                amount: domain::money::MicroUsd(500_000_000),
            })
            .await
            .unwrap();
        let engine = TemplateDraftEngine::new(config.clone());
        let draft = CreateDraft {
            store: &store,
            clock: &clock,
            config: &config,
            primary: &engine,
            template: &engine,
        }
        .execute(CreateDraftCmd {
            topics: vec!["filled flash slot".into()],
            tier: DraftTier::Flash,
            requested_source: DraftSource::Template,
            allow_fallback: false,
        })
        .await
        .unwrap()
        .drafts
        .remove(0);
        let reviewer = store.add_user("slot-curator", now - Duration::days(2), 2);
        let approved = ReviewDraft {
            store: &store,
            clock: &clock,
            config: &config,
            lp_kill_config: LpKillConfig::default(),
        }
        .approve(draft.id, reviewer)
        .await
        .unwrap();
        clock.advance(approved.publish_at.unwrap() - now);

        let first = publisher_tick(
            &store,
            &clock,
            &config,
            RepConfig::default(),
            LpKillConfig::default(),
        )
        .await
        .unwrap();
        let second = publisher_tick(
            &store,
            &clock,
            &config,
            RepConfig::default(),
            LpKillConfig::default(),
        )
        .await
        .unwrap();

        assert_eq!(first.published, 1);
        assert!(!first.slot_unfilled);
        assert!(!second.slot_unfilled);
        assert_eq!(
            store
                .outbox()
                .iter()
                .filter(|event| event.event_type == event_type::SLOT_UNFILLED)
                .count(),
            0
        );
    }
}
