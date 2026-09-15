//! Draft generation and admission.

use domain::drafting::{DraftSource, DraftTier, DraftingError};
use serde_json::json;

use crate::error::{AppError, StoreError};
use crate::model::{
    event_type, ContentConfig, DraftId, DraftRequest, DraftRow, DraftStatus, Event,
};
use crate::ports::{Clock, DraftEngine, GeneratedDraft, Store};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateDraftCmd {
    pub topics: Vec<String>,
    pub tier: DraftTier,
    pub requested_source: DraftSource,
    pub allow_fallback: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateDraftReceipt {
    pub drafts: Vec<DraftRow>,
}

pub struct CreateDraft<'a, S: Store + ?Sized, C: Clock + ?Sized> {
    pub store: &'a S,
    pub clock: &'a C,
    pub config: &'a ContentConfig,
    pub primary: &'a dyn DraftEngine,
    pub template: &'a dyn DraftEngine,
}

impl<S: Store + ?Sized, C: Clock + ?Sized> CreateDraft<'_, S, C> {
    /// Generates every requested draft before taking the admission lock, then
    /// inserts the batch atomically under the pending-count lock.
    ///
    /// # Errors
    /// Returns validation, generator availability, capacity, overflow, or
    /// store errors without partially inserting the requested batch.
    pub async fn execute(&self, cmd: CreateDraftCmd) -> Result<CreateDraftReceipt, AppError> {
        if cmd.topics.is_empty() {
            return Err(AppError::InvalidDraft("topic batch is empty"));
        }
        let now = self.clock.now();
        let ttl = i64::try_from(self.config.draft_ttl_secs).map_err(|_| AppError::Overflow)?;
        let expires_at = now
            .checked_add(time::Duration::seconds(ttl))
            .ok_or(AppError::Overflow)?;
        let mut drafts = Vec::with_capacity(cmd.topics.len());
        for topic in &cmd.topics {
            let request = DraftRequest {
                topic: topic.clone(),
                tier: cmd.tier,
            };
            let (generated, fallback_from) = self.generate(&request, &cmd).await?;
            validate_generated(&generated, cmd.tier)?;
            drafts.push(DraftRow {
                id: DraftId(uuid::Uuid::new_v4()),
                spec: generated.spec,
                source: generated.source,
                fallback_from,
                status: DraftStatus::Pending,
                publish_stage: None,
                published_market: None,
                publish_at: None,
                expires_at,
                created_at: now,
            });
        }

        let mut tx = self.store.content_tx().await?;
        tx.lock_admission().await?;
        let existing = tx.pending_draft_count().await?;
        let requested = u32::try_from(drafts.len()).map_err(|_| AppError::Overflow)?;
        if existing.checked_add(requested).ok_or(AppError::Overflow)?
            > self.config.max_pending_drafts
        {
            return Err(AppError::PendingDraftLimit);
        }
        for draft in &drafts {
            tx.insert_draft(draft.clone()).await?;
            tx.append(Event {
                event_type: event_type::DRAFT_CREATED,
                aggregate_type: "draft",
                aggregate_id: draft.id.0,
                payload: json!({
                    "source": source_name(draft.source),
                    "fallback_from": draft.fallback_from.map(source_name),
                    "tier": tier_name(draft.spec.tier),
                    "expires_at": draft.expires_at.unix_timestamp(),
                }),
            })
            .await?;
        }
        tx.commit().await?;
        Ok(CreateDraftReceipt { drafts })
    }

    async fn generate(
        &self,
        request: &DraftRequest,
        cmd: &CreateDraftCmd,
    ) -> Result<(GeneratedDraft, Option<DraftSource>), AppError> {
        let engine = match cmd.requested_source {
            DraftSource::Template => self.template,
            DraftSource::Llm => self.primary,
        };
        match engine.generate(request).await {
            Ok(generated) => Ok((generated, None)),
            Err(_) if cmd.requested_source == DraftSource::Llm && cmd.allow_fallback => self
                .template
                .generate(request)
                .await
                .map(|generated| (generated, Some(DraftSource::Llm)))
                .map_err(|_| unavailable()),
            Err(_) => Err(unavailable()),
        }
    }
}

fn validate_generated(
    generated: &GeneratedDraft,
    requested_tier: DraftTier,
) -> Result<(), AppError> {
    if generated.spec.tier != requested_tier {
        return Err(AppError::InvalidDraft("engine changed requested tier"));
    }
    generated.spec.validate().map_err(invalid_draft)
}

pub(super) fn invalid_draft(error: DraftingError) -> AppError {
    AppError::InvalidDraft(match error {
        DraftingError::EmptyQuestion => "question is empty",
        DraftingError::EmptyDescription => "description is empty",
        DraftingError::EmptyVideoScript => "video script is empty",
        DraftingError::EmptySlug => "slug is empty",
        DraftingError::InvalidSeed => "seed is invalid",
        DraftingError::InvalidFee => "fee is invalid",
        DraftingError::InvalidMinVotes => "minimum votes are invalid",
        DraftingError::InvalidWindow => "publication window is invalid",
        DraftingError::SeedBelowFloor => "seed is below tier floor",
        DraftingError::VotesBelowFloor => "minimum votes are below tier floor",
        DraftingError::InvalidSlots
        | DraftingError::InvalidCadence
        | DraftingError::TimestampOverflow => "slot configuration is invalid",
    })
}

const fn source_name(source: DraftSource) -> &'static str {
    match source {
        DraftSource::Template => "template",
        DraftSource::Llm => "llm",
    }
}

const fn tier_name(tier: DraftTier) -> &'static str {
    match tier {
        DraftTier::Daily => "daily",
        DraftTier::Flash => "flash",
    }
}

fn unavailable() -> AppError {
    AppError::Store(StoreError::Unavailable("phase5:draft-engine"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use async_trait::async_trait;
    use domain::drafting::{DraftSource, DraftSpec, DraftTier};
    use domain::money::{BasisPoints, MicroUsd};
    use time::OffsetDateTime;

    use super::*;
    use crate::fakes::{FakeClock, InMemoryStore};
    use crate::ports::{DraftEngineUnavailable, GeneratedDraft};

    struct Engine(Result<GeneratedDraft, DraftEngineUnavailable>);

    #[async_trait]
    impl crate::ports::DraftEngine for Engine {
        async fn generate(
            &self,
            _request: &crate::model::DraftRequest,
        ) -> Result<GeneratedDraft, DraftEngineUnavailable> {
            self.0.clone()
        }
    }

    fn generated(source: DraftSource) -> GeneratedDraft {
        GeneratedDraft {
            spec: DraftSpec {
                question: "Will the bridge open?".into(),
                description: "Track construction.".into(),
                video_script: "Review milestones.".into(),
                slug: "bridge-open".into(),
                tier: DraftTier::Daily,
                seed: MicroUsd(2_000_000),
                fee: BasisPoints(100),
                min_votes_to_resolve: 3,
                open_secs: 86_400,
                hidden_window_secs: 3_600,
            },
            source,
        }
    }

    #[tokio::test]
    async fn records_actual_source_and_explicit_llm_fallback_for_a_batch() {
        let store = InMemoryStore::new();
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let clock = FakeClock::at(now);
        let primary = Engine(Err(DraftEngineUnavailable));
        let template = Engine(Ok(generated(DraftSource::Template)));
        let config = crate::model::ContentConfig::default();
        let receipt = CreateDraft {
            store: &store,
            clock: &clock,
            config: &config,
            primary: &primary,
            template: &template,
        }
        .execute(CreateDraftCmd {
            topics: vec!["bridge".into(), "rail".into()],
            tier: DraftTier::Daily,
            requested_source: DraftSource::Llm,
            allow_fallback: true,
        })
        .await
        .unwrap();

        assert_eq!(receipt.drafts.len(), 2);
        assert!(receipt.drafts.iter().all(|draft| {
            draft.source == DraftSource::Template
                && draft.fallback_from == Some(DraftSource::Llm)
                && draft.expires_at
                    == now + time::Duration::seconds(i64::try_from(config.draft_ttl_secs).unwrap())
        }));
        assert_eq!(store.outbox().len(), 2);
    }

    #[tokio::test]
    async fn unavailable_without_permission_and_batch_backpressure_write_nothing() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(OffsetDateTime::UNIX_EPOCH);
        let unavailable = Engine(Err(DraftEngineUnavailable));
        let template = Engine(Ok(generated(DraftSource::Template)));
        let config = crate::model::ContentConfig {
            max_pending_drafts: 1,
            ..crate::model::ContentConfig::default()
        };
        let uc = CreateDraft {
            store: &store,
            clock: &clock,
            config: &config,
            primary: &unavailable,
            template: &template,
        };
        let before = store.snapshot();
        assert!(matches!(
            uc.execute(CreateDraftCmd {
                topics: vec!["one".into()],
                tier: DraftTier::Daily,
                requested_source: DraftSource::Llm,
                allow_fallback: false,
            })
            .await,
            Err(crate::error::AppError::Store(
                crate::error::StoreError::Unavailable("phase5:draft-engine")
            ))
        ));
        assert_eq!(store.snapshot(), before);

        assert!(matches!(
            CreateDraft {
                store: &store,
                clock: &clock,
                config: &config,
                primary: &unavailable,
                template: &unavailable,
            }
            .execute(CreateDraftCmd {
                topics: vec!["fallback also unavailable".into()],
                tier: DraftTier::Daily,
                requested_source: DraftSource::Llm,
                allow_fallback: true,
            })
            .await,
            Err(crate::error::AppError::Store(
                crate::error::StoreError::Unavailable("phase5:draft-engine")
            ))
        ));
        assert_eq!(store.snapshot(), before);

        assert_eq!(
            CreateDraft {
                store: &store,
                clock: &clock,
                config: &config,
                primary: &template,
                template: &template,
            }
            .execute(CreateDraftCmd {
                topics: vec!["one".into(), "two".into()],
                tier: DraftTier::Daily,
                requested_source: DraftSource::Template,
                allow_fallback: false,
            })
            .await,
            Err(crate::error::AppError::PendingDraftLimit)
        );
        assert_eq!(store.snapshot(), before);
    }

    #[tokio::test]
    async fn concurrent_batch_admission_never_overfills_the_pending_limit() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(OffsetDateTime::UNIX_EPOCH);
        let engine = Engine(Ok(generated(DraftSource::Template)));
        let config = crate::model::ContentConfig {
            max_pending_drafts: 1,
            ..crate::model::ContentConfig::default()
        };
        let left = CreateDraft {
            store: &store,
            clock: &clock,
            config: &config,
            primary: &engine,
            template: &engine,
        };
        let right = CreateDraft { ..left };
        let command = || CreateDraftCmd {
            topics: vec!["one".into()],
            tier: DraftTier::Daily,
            requested_source: DraftSource::Template,
            allow_fallback: false,
        };
        let (a, b) = tokio::join!(left.execute(command()), right.execute(command()));
        assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
        assert_eq!(
            usize::from(a == Err(crate::error::AppError::PendingDraftLimit))
                + usize::from(b == Err(crate::error::AppError::PendingDraftLimit)),
            1
        );
        assert_eq!(store.drafts().len(), 1);
    }

    #[tokio::test]
    async fn rejects_empty_overflowing_and_tier_mutating_generation_without_writes() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(OffsetDateTime::UNIX_EPOCH);
        let engine = Engine(Ok(generated(DraftSource::Llm)));
        let config = crate::model::ContentConfig::default();
        let use_case = CreateDraft {
            store: &store,
            clock: &clock,
            config: &config,
            primary: &engine,
            template: &engine,
        };
        assert_eq!(
            use_case
                .execute(CreateDraftCmd {
                    topics: Vec::new(),
                    tier: DraftTier::Daily,
                    requested_source: DraftSource::Llm,
                    allow_fallback: false,
                })
                .await,
            Err(AppError::InvalidDraft("topic batch is empty"))
        );

        let mut overflow = config.clone();
        overflow.draft_ttl_secs = u64::MAX;
        assert_eq!(
            CreateDraft {
                store: &store,
                clock: &clock,
                config: &overflow,
                primary: &engine,
                template: &engine,
            }
            .execute(CreateDraftCmd {
                topics: vec!["overflow".into()],
                tier: DraftTier::Daily,
                requested_source: DraftSource::Llm,
                allow_fallback: false,
            })
            .await,
            Err(AppError::Overflow)
        );

        assert!(matches!(
            use_case
                .execute(CreateDraftCmd {
                    topics: vec!["wrong tier".into()],
                    tier: DraftTier::Flash,
                    requested_source: DraftSource::Llm,
                    allow_fallback: false,
                })
                .await,
            Err(AppError::InvalidDraft("engine changed requested tier"))
        ));
        assert!(store.drafts().is_empty());
    }

    #[tokio::test]
    async fn successful_llm_generation_preserves_actual_source_without_fallback_marker() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(OffsetDateTime::UNIX_EPOCH);
        let engine = Engine(Ok(generated(DraftSource::Llm)));
        let config = crate::model::ContentConfig::default();
        let draft = CreateDraft {
            store: &store,
            clock: &clock,
            config: &config,
            primary: &engine,
            template: &engine,
        }
        .execute(CreateDraftCmd {
            topics: vec!["bridge".into()],
            tier: DraftTier::Daily,
            requested_source: DraftSource::Llm,
            allow_fallback: false,
        })
        .await
        .unwrap()
        .drafts
        .remove(0);
        assert_eq!(draft.source, DraftSource::Llm);
        assert_eq!(draft.fallback_from, None);
    }

    #[test]
    fn every_domain_drafting_error_has_a_stable_application_message() {
        let cases = [
            (DraftingError::EmptyQuestion, "question is empty"),
            (DraftingError::EmptyDescription, "description is empty"),
            (DraftingError::EmptyVideoScript, "video script is empty"),
            (DraftingError::EmptySlug, "slug is empty"),
            (DraftingError::InvalidSeed, "seed is invalid"),
            (DraftingError::InvalidFee, "fee is invalid"),
            (DraftingError::InvalidMinVotes, "minimum votes are invalid"),
            (
                DraftingError::InvalidWindow,
                "publication window is invalid",
            ),
            (DraftingError::SeedBelowFloor, "seed is below tier floor"),
            (
                DraftingError::VotesBelowFloor,
                "minimum votes are below tier floor",
            ),
            (DraftingError::InvalidSlots, "slot configuration is invalid"),
            (
                DraftingError::InvalidCadence,
                "slot configuration is invalid",
            ),
            (
                DraftingError::TimestampOverflow,
                "slot configuration is invalid",
            ),
        ];
        for (error, message) in cases {
            assert_eq!(invalid_draft(error), AppError::InvalidDraft(message));
        }
    }
}
