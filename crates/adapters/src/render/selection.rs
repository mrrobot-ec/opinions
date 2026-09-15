//! The production renderer seam (Task 5.2): deterministic SVG serialization
//! plus atomic filesystem persistence behind the [`Renderer`] port.

use std::path::Path;
use std::sync::Arc;

use application::error::StoreError;
use application::model::{ArtifactKind, JobId, RenderedArtifact};
use application::ports::Renderer;
use application::video::artifacts::{asset_file_name, asset_url};
use async_trait::async_trait;

use super::artifacts::{read_under_root, write_atomic};
use super::share_card::serve_or_render;
use super::svg;

/// Stateless by construction: byte identity across fresh instances is the
/// deterministic-renderer contract (codex M3).
pub struct SvgRenderer;

#[async_trait]
impl Renderer for SvgRenderer {
    async fn render(
        &self,
        spec: &domain::render_spec::RenderSpec,
    ) -> Result<RenderedArtifact, StoreError> {
        Ok(svg::artifact(spec))
    }

    async fn persist(
        &self,
        render_dir: &Path,
        job: JobId,
        kind: ArtifactKind,
        artifact: &RenderedArtifact,
    ) -> Result<String, StoreError> {
        write_atomic(render_dir, &asset_file_name(job, kind), &artifact.bytes)?;
        Ok(asset_url(job))
    }

    async fn share_card(
        &self,
        render_dir: &Path,
        address: &str,
        spec: &domain::render_spec::RenderSpec,
    ) -> Result<Vec<u8>, StoreError> {
        serve_or_render(render_dir, address, spec)
    }

    async fn load(
        &self,
        render_dir: &Path,
        job: JobId,
        kind: ArtifactKind,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        read_under_root(render_dir, &asset_file_name(job, kind))
    }
}

/// Construction seam used by the frozen wiring.
#[must_use]
pub fn renderer() -> Arc<dyn Renderer> {
    Arc::new(SvgRenderer)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::path::PathBuf;
    use uuid::Uuid;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir()
            .join("opinions-w2-render")
            .join(Uuid::new_v4().to_string());
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn renders_persists_and_loads_byte_identically_across_instances() {
        let dir = scratch();
        let spec = domain::render_spec::poster_spec("Will it work?");
        let job = JobId(Uuid::new_v4());

        let first = SvgRenderer.render(&spec).await.unwrap();
        let second = SvgRenderer.render(&spec).await.unwrap();
        assert_eq!(first, second, "fresh instances render identical bytes");
        assert_eq!(first.media_type, "image/svg+xml");

        let url = SvgRenderer
            .persist(&dir, job, ArtifactKind::Poster, &first)
            .await
            .unwrap();
        assert_eq!(url, format!("/assets/{}.svg", job.0));
        let loaded = SvgRenderer
            .load(&dir, job, ArtifactKind::Poster)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded, first.bytes);
        // The kind participates in the on-disk name: a different kind for
        // the same job id is a different (absent) artifact.
        assert_eq!(
            SvgRenderer
                .load(&dir, job, ArtifactKind::MarketVideo)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn share_card_path_round_trips_through_the_cache() {
        let dir = scratch();
        let spec = domain::render_spec::poster_spec("q");
        let address = domain::render_spec::sha256_hex(b"selection-card");
        let bytes = SvgRenderer.share_card(&dir, &address, &spec).await.unwrap();
        assert_eq!(
            SvgRenderer.share_card(&dir, &address, &spec).await.unwrap(),
            bytes
        );
    }

    #[tokio::test]
    async fn production_seam_returns_the_svg_renderer() {
        let renderer = renderer();
        let artifact = renderer
            .render(&domain::render_spec::poster_spec("seam"))
            .await
            .unwrap();
        assert!(!artifact.bytes.is_empty());
    }
}
