use application::model::{DraftRow, DraftStatus, PublishStage};
use domain::drafting::{DraftSource, DraftSpec, DraftTier};
use domain::money::{BasisPoints, MicroUsd};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DraftTierDto {
    Daily,
    Flash,
}

impl From<DraftTierDto> for DraftTier {
    fn from(value: DraftTierDto) -> Self {
        match value {
            DraftTierDto::Daily => Self::Daily,
            DraftTierDto::Flash => Self::Flash,
        }
    }
}

impl From<DraftTier> for DraftTierDto {
    fn from(value: DraftTier) -> Self {
        match value {
            DraftTier::Daily => Self::Daily,
            DraftTier::Flash => Self::Flash,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DraftSourceDto {
    Template,
    Llm,
}

impl From<DraftSourceDto> for DraftSource {
    fn from(value: DraftSourceDto) -> Self {
        match value {
            DraftSourceDto::Template => Self::Template,
            DraftSourceDto::Llm => Self::Llm,
        }
    }
}

impl From<DraftSource> for DraftSourceDto {
    fn from(value: DraftSource) -> Self {
        match value {
            DraftSource::Template => Self::Template,
            DraftSource::Llm => Self::Llm,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
pub struct DraftSpecDto {
    pub question: String,
    pub description: String,
    pub video_script: String,
    pub slug: String,
    pub tier: DraftTierDto,
    pub seed_micro: i64,
    pub fee_bps: u16,
    pub min_votes_to_resolve: i32,
    pub open_secs: u64,
    pub hidden_window_secs: u64,
}

impl From<DraftSpecDto> for DraftSpec {
    fn from(value: DraftSpecDto) -> Self {
        Self {
            question: value.question,
            description: value.description,
            video_script: value.video_script,
            slug: value.slug,
            tier: value.tier.into(),
            seed: MicroUsd(value.seed_micro),
            fee: BasisPoints(value.fee_bps),
            min_votes_to_resolve: value.min_votes_to_resolve,
            open_secs: value.open_secs,
            hidden_window_secs: value.hidden_window_secs,
        }
    }
}

impl From<DraftSpec> for DraftSpecDto {
    fn from(value: DraftSpec) -> Self {
        Self {
            question: value.question,
            description: value.description,
            video_script: value.video_script,
            slug: value.slug,
            tier: value.tier.into(),
            seed_micro: value.seed.0,
            fee_bps: value.fee.0,
            min_votes_to_resolve: value.min_votes_to_resolve,
            open_secs: value.open_secs,
            hidden_window_secs: value.hidden_window_secs,
        }
    }
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateDraftRequest {
    pub topics: Vec<String>,
    pub tier: DraftTierDto,
    pub source: DraftSourceDto,
    #[serde(default)]
    pub allow_fallback: bool,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct EditDraftRequest {
    pub reviewer_id: Uuid,
    pub spec: DraftSpecDto,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ReviewDraftRequest {
    pub reviewer_id: Uuid,
}

#[derive(Debug, Clone, Serialize, ToSchema, PartialEq, Eq)]
pub struct DraftDto {
    pub id: Uuid,
    pub spec: DraftSpecDto,
    pub source: DraftSourceDto,
    pub fallback_from: Option<DraftSourceDto>,
    pub status: String,
    pub publish_stage: Option<String>,
    pub published_market_id: Option<Uuid>,
    pub publish_at: Option<time::OffsetDateTime>,
    pub expires_at: time::OffsetDateTime,
    pub created_at: time::OffsetDateTime,
}

impl From<DraftRow> for DraftDto {
    fn from(row: DraftRow) -> Self {
        Self {
            id: row.id.0,
            spec: row.spec.into(),
            source: row.source.into(),
            fallback_from: row.fallback_from.map(Into::into),
            status: status_name(row.status).to_string(),
            publish_stage: row.publish_stage.map(stage_name).map(str::to_string),
            published_market_id: row.published_market.map(|market| market.0),
            publish_at: row.publish_at,
            expires_at: row.expires_at,
            created_at: row.created_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema, PartialEq, Eq)]
pub struct PublishDraftDto {
    pub draft_id: Uuid,
    pub market_id: Uuid,
    pub replayed: bool,
}

const fn status_name(status: DraftStatus) -> &'static str {
    match status {
        DraftStatus::Pending => "pending",
        DraftStatus::Approved => "approved",
        DraftStatus::Rejected => "rejected",
        DraftStatus::Published => "published",
        DraftStatus::Expired => "expired",
    }
}

const fn stage_name(stage: PublishStage) -> &'static str {
    match stage {
        PublishStage::Claimed => "claimed",
        PublishStage::Seeded => "seeded",
        PublishStage::Live => "live",
        PublishStage::JobsEnqueued => "jobs_enqueued",
        PublishStage::Published => "published",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_and_source_wire_vocabularies_round_trip() {
        for tier in [DraftTier::Daily, DraftTier::Flash] {
            let dto: DraftTierDto = tier.into();
            assert_eq!(DraftTier::from(dto), tier);
        }
        for source in [DraftSource::Template, DraftSource::Llm] {
            let dto: DraftSourceDto = source.into();
            assert_eq!(DraftSource::from(dto), source);
        }
    }

    #[test]
    fn draft_specs_statuses_and_publish_stages_have_total_wire_mappings() {
        let dto = DraftSpecDto {
            question: "Will the mapping round-trip?".into(),
            description: "Every field is preserved.".into(),
            video_script: "Show the assertion.".into(),
            slug: "mapping-round-trip".into(),
            tier: DraftTierDto::Flash,
            seed_micro: 2_000_000,
            fee_bps: 125,
            min_votes_to_resolve: 7,
            open_secs: 900,
            hidden_window_secs: 60,
        };
        let domain = DraftSpec::from(dto.clone());
        assert_eq!(DraftSpecDto::from(domain), dto);

        assert_eq!(status_name(DraftStatus::Pending), "pending");
        assert_eq!(status_name(DraftStatus::Approved), "approved");
        assert_eq!(status_name(DraftStatus::Rejected), "rejected");
        assert_eq!(status_name(DraftStatus::Published), "published");
        assert_eq!(status_name(DraftStatus::Expired), "expired");
        assert_eq!(stage_name(PublishStage::Claimed), "claimed");
        assert_eq!(stage_name(PublishStage::Seeded), "seeded");
        assert_eq!(stage_name(PublishStage::Live), "live");
        assert_eq!(stage_name(PublishStage::JobsEnqueued), "jobs_enqueued");
        assert_eq!(stage_name(PublishStage::Published), "published");
    }
}
