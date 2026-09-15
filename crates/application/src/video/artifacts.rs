//! Artifact naming contract (codex M4): names derive ONLY from the job UUID
//! and kind — never from user input — so serving can resolve a job to exactly
//! one disk name and traversal is structurally impossible.

use crate::model::{ArtifactKind, JobId};

/// Stable media type for every phase-5 artifact (SVG this phase; real video
/// vendors are explicitly deferred).
pub const SVG_MEDIA_TYPE: &str = "image/svg+xml";

#[must_use]
pub fn kind_name(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::MarketVideo => "market_video",
        ArtifactKind::Poster => "poster",
    }
}

/// On-disk file name under the canonicalized render dir.
#[must_use]
pub fn asset_file_name(job: JobId, kind: ArtifactKind) -> String {
    format!("{}_{}.svg", job.0, kind_name(kind))
}

/// Public URL served by `GET /assets/{job_id}.svg`.
#[must_use]
pub fn asset_url(job: JobId) -> String {
    format!("/assets/{}.svg", job.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn names_derive_only_from_job_uuid_and_kind() {
        let job = JobId(Uuid::from_u128(7));
        assert_eq!(
            asset_file_name(job, ArtifactKind::Poster),
            "00000000-0000-0000-0000-000000000007_poster.svg"
        );
        assert_eq!(
            asset_file_name(job, ArtifactKind::MarketVideo),
            "00000000-0000-0000-0000-000000000007_market_video.svg"
        );
        assert_eq!(
            asset_url(job),
            "/assets/00000000-0000-0000-0000-000000000007.svg"
        );
    }

    #[test]
    fn kind_names_match_the_database_vocabulary() {
        assert_eq!(kind_name(ArtifactKind::Poster), "poster");
        assert_eq!(kind_name(ArtifactKind::MarketVideo), "market_video");
    }
}
