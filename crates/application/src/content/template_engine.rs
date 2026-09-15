use std::sync::Arc;

use async_trait::async_trait;
use domain::drafting::{DraftSource, DraftSpec, DraftTier};
use domain::money::{BasisPoints, MicroUsd};

use crate::model::{ContentConfig, ContentTierConfig, DraftRequest};
use crate::ports::{DraftEngine, DraftEngineUnavailable, GeneratedDraft};

/// Local deterministic generator used directly and as the honest LLM
/// fallback. It never performs network I/O.
#[derive(Debug, Clone)]
pub struct TemplateDraftEngine {
    config: ContentConfig,
}

impl TemplateDraftEngine {
    #[must_use]
    pub fn new(config: ContentConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl DraftEngine for TemplateDraftEngine {
    async fn generate(
        &self,
        request: &DraftRequest,
    ) -> Result<GeneratedDraft, DraftEngineUnavailable> {
        let topic = request.topic.trim();
        let defaults = tier_config(&self.config, request.tier);
        Ok(GeneratedDraft {
            spec: DraftSpec {
                question: if topic.ends_with('?') {
                    topic.to_string()
                } else {
                    format!("Will {topic}?")
                },
                description: format!("Curator-provided topic: {topic}"),
                video_script: format!("Consider the evidence and vote: {topic}"),
                slug: slugify(topic),
                tier: request.tier,
                seed: MicroUsd(defaults.seed_micro),
                fee: BasisPoints(defaults.fee_bps),
                min_votes_to_resolve: defaults.min_votes_to_resolve,
                open_secs: defaults.open_secs,
                hidden_window_secs: defaults.hidden_window_secs,
            },
            source: DraftSource::Template,
        })
    }
}

#[must_use]
pub fn tier_config(config: &ContentConfig, tier: DraftTier) -> ContentTierConfig {
    match tier {
        DraftTier::Daily => config.tier_defaults[0],
        DraftTier::Flash => config.tier_defaults[1],
    }
}

fn slugify(topic: &str) -> String {
    let mut slug = String::new();
    let mut separated = false;
    for ch in topic.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            separated = false;
        } else if !slug.is_empty() && !separated {
            slug.push('-');
            separated = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "market".to_string()
    } else {
        slug
    }
}

/// Stable construction seam used by shared wiring.
#[must_use]
pub fn draft_engine() -> Arc<dyn DraftEngine> {
    Arc::new(TemplateDraftEngine::new(ContentConfig::default()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[tokio::test]
    async fn template_generation_is_deterministic_and_uses_tier_defaults() {
        let engine = TemplateDraftEngine::new(ContentConfig::default());
        let request = DraftRequest {
            topic: "  Local Transit 2030?! ".into(),
            tier: DraftTier::Flash,
        };
        let first = engine.generate(&request).await.unwrap();
        let second = engine.generate(&request).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(first.source, DraftSource::Template);
        assert_eq!(first.spec.slug, "local-transit-2030");
        assert_eq!(
            first.spec.seed.0,
            ContentConfig::default().tier_defaults[1].seed_micro
        );
    }

    #[tokio::test]
    async fn punctuation_only_topic_has_a_safe_deterministic_slug() {
        let engine = TemplateDraftEngine::new(ContentConfig::default());
        let generated = engine
            .generate(&DraftRequest {
                topic: " !!! ".into(),
                tier: DraftTier::Daily,
            })
            .await
            .unwrap();
        assert_eq!(generated.spec.slug, "market");
        assert_eq!(generated.source, DraftSource::Template);
    }

    #[tokio::test]
    async fn shared_factory_constructs_a_working_template_engine() {
        let draft = draft_engine()
            .generate(&DraftRequest {
                topic: "Factory behavior".into(),
                tier: DraftTier::Flash,
            })
            .await
            .unwrap();
        assert_eq!(draft.source, DraftSource::Template);
        assert_eq!(draft.spec.tier, DraftTier::Flash);
        assert_eq!(draft.spec.slug, "factory-behavior");
    }
}
