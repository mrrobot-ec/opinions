//! Leased artifact worker (Task 5.2, codex B4).
//!
//! One tick: (1) SHORT claim transaction — reclaim expired leases, claim up
//! to a batch of queued jobs (stamping `rendering + token + lease +
//! attempts+1`), read render inputs, COMMIT; (2) render each job OUTSIDE any
//! transaction; (3) per job, a fresh short transaction applies the
//! token-fenced completion — success attaches under the market row lock in
//! the same commit, failure requeues with saturating backoff or goes
//! terminal on the `>=` attempts boundary.

use time::Duration;

use crate::error::StoreError;
use crate::model::{ArtifactKind, ContentConfig, MarketRow, VideoJobRow};
use crate::ports::{Clock, Renderer, Store};

use super::attach_ready::complete_and_attach;
use super::jobs::{backoff_available_at, is_terminal, CLAIM_BATCH, RECLAIM_BATCH};

fn spec_for(kind: ArtifactKind, question: &str) -> domain::render_spec::RenderSpec {
    match kind {
        ArtifactKind::Poster => domain::render_spec::poster_spec(question),
        ArtifactKind::MarketVideo => domain::render_spec::video_spec(question),
    }
}

/// Stable worker entry point wired by `main.rs`.
///
/// # Errors
/// Backend failures; render failures are absorbed into per-job requeue /
/// terminal-failure bookkeeping, never a tick error.
pub async fn worker_tick<S: Store + ?Sized, C: Clock + ?Sized>(
    store: &S,
    clock: &C,
    renderer: &dyn Renderer,
    config: &ContentConfig,
) -> Result<(), StoreError> {
    let now = clock.now();
    let lease = Duration::seconds(i64::try_from(config.lease_secs).unwrap_or(i64::MAX));
    let mut tx = store.video_tx().await?;
    tx.reclaim_expired(now, config.video_max_attempts, RECLAIM_BATCH)
        .await?;
    let jobs = tx.claim(now, lease, CLAIM_BATCH).await?;
    let mut markets = Vec::with_capacity(jobs.len());
    for job in &jobs {
        markets.push(tx.market_row(job.market).await?);
    }
    // The claim commits BEFORE any render work; leases fence the rest.
    tx.commit().await?;

    for (job, market) in jobs.into_iter().zip(markets) {
        render_one(store, clock, renderer, config, job, &market).await?;
    }
    Ok(())
}

async fn render_one<S: Store + ?Sized, C: Clock + ?Sized>(
    store: &S,
    clock: &C,
    renderer: &dyn Renderer,
    config: &ContentConfig,
    job: VideoJobRow,
    market: &MarketRow,
) -> Result<(), StoreError> {
    let token = job
        .claim_token
        .ok_or(StoreError::Invariant("claimed job missing token"))?;
    let spec = spec_for(job.kind, &market.question);
    // Render + persist happen OUTSIDE any transaction.
    let result = match renderer.render(&spec).await {
        Ok(artifact) => {
            renderer
                .persist(&config.render_dir, job.id, job.kind, &artifact)
                .await
        }
        Err(error) => Err(error),
    };
    match result {
        Ok(asset_url) => {
            let now = clock.now();
            let mut tx = store.video_tx().await?;
            // A lost CAS means a newer attempt owns the job: drop silently.
            complete_and_attach(tx.as_mut(), job.id, token, &asset_url, now).await?;
            tx.commit().await
        }
        Err(error) => {
            let now = clock.now();
            let mut tx = store.video_tx().await?;
            tx.complete_error(
                job.id,
                token,
                &error.to_string(),
                backoff_available_at(now, config.backoff_base_secs, job.attempts),
                is_terminal(job.attempts, config.video_max_attempts),
            )
            .await?;
            tx.commit().await
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    #![allow(clippy::unwrap_used)]

    use async_trait::async_trait;
    use parking_lot::Mutex;
    use std::path::{Path, PathBuf};
    use uuid::Uuid;

    use crate::error::StoreError;
    use crate::fakes::InMemoryStore;
    use crate::model::{ArtifactKind, JobId, MarketId, NewMarket, RenderedArtifact, UserId};
    use crate::ports::{Renderer, Store};
    use crate::video::artifacts::{asset_url, SVG_MEDIA_TYPE};

    /// Deterministic in-memory renderer: bytes derive only from the spec, so
    /// a retry after a crash is byte-identical by construction.
    #[derive(Default)]
    pub(crate) struct MemRenderer {
        pub(crate) fail_renders: Mutex<u32>,
        pub(crate) persisted: Mutex<Vec<(JobId, ArtifactKind, Vec<u8>)>>,
        pub(crate) cards: Mutex<Vec<(PathBuf, String, Vec<u8>)>>,
    }

    pub(crate) fn spec_bytes(spec: &domain::render_spec::RenderSpec) -> Vec<u8> {
        format!("{spec:?}").into_bytes()
    }

    #[async_trait]
    impl Renderer for MemRenderer {
        async fn render(
            &self,
            spec: &domain::render_spec::RenderSpec,
        ) -> Result<RenderedArtifact, StoreError> {
            let mut failures = self.fail_renders.lock();
            if *failures > 0 {
                *failures -= 1;
                return Err(StoreError::Backend("render exploded".into()));
            }
            Ok(RenderedArtifact {
                bytes: spec_bytes(spec),
                media_type: SVG_MEDIA_TYPE.to_string(),
            })
        }

        async fn persist(
            &self,
            _render_dir: &Path,
            job: JobId,
            kind: ArtifactKind,
            artifact: &RenderedArtifact,
        ) -> Result<String, StoreError> {
            self.persisted
                .lock()
                .push((job, kind, artifact.bytes.clone()));
            Ok(asset_url(job))
        }

        async fn share_card(
            &self,
            render_dir: &Path,
            address: &str,
            spec: &domain::render_spec::RenderSpec,
        ) -> Result<Vec<u8>, StoreError> {
            let mut cards = self.cards.lock();
            if let Some((_, _, bytes)) = cards
                .iter()
                .find(|(dir, cached, _)| dir == render_dir && cached == address)
            {
                return Ok(bytes.clone());
            }
            let bytes = spec_bytes(spec);
            cards.push((render_dir.to_path_buf(), address.to_string(), bytes.clone()));
            Ok(bytes)
        }

        async fn load(
            &self,
            _render_dir: &Path,
            job: JobId,
            kind: ArtifactKind,
        ) -> Result<Option<Vec<u8>>, StoreError> {
            Ok(self
                .persisted
                .lock()
                .iter()
                .rev()
                .find(|(id, of_kind, _)| *id == job && *of_kind == kind)
                .map(|(_, _, bytes)| bytes.clone()))
        }
    }

    #[tokio::test]
    async fn recording_renderer_loads_the_latest_matching_artifact_only() {
        let renderer = MemRenderer::default();
        let job = JobId(Uuid::new_v4());
        let other = JobId(Uuid::new_v4());
        let dir = std::path::Path::new("/tmp/content-render-test");
        renderer.persisted.lock().extend([
            (job, ArtifactKind::Poster, b"old".to_vec()),
            (other, ArtifactKind::Poster, b"other".to_vec()),
            (job, ArtifactKind::MarketVideo, b"video".to_vec()),
            (job, ArtifactKind::Poster, b"new".to_vec()),
        ]);
        assert_eq!(
            renderer.load(dir, job, ArtifactKind::Poster).await.unwrap(),
            Some(b"new".to_vec())
        );
        assert_eq!(
            renderer
                .load(dir, JobId(Uuid::new_v4()), ArtifactKind::Poster)
                .await
                .unwrap(),
            None
        );
    }

    pub(crate) async fn insert_market(store: &InMemoryStore) -> MarketId {
        let mut tx = store.seed_tx().await.unwrap();
        let key = format!("video-market-{}", Uuid::new_v4());
        tx.serialize_key(&key).await.unwrap();
        let now = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let market = tx
            .insert_market(NewMarket {
                id: MarketId(Uuid::new_v4()),
                slug: format!("video-{key}"),
                min_votes_to_resolve: 3,
                closes_at: now + time::Duration::hours(2),
                tally_hidden_at: now + time::Duration::hours(1),
            })
            .await
            .unwrap();
        tx.commit().await.unwrap();
        market
    }

    pub(crate) async fn insert_user(store: &InMemoryStore, handle: &str) -> UserId {
        let mut tx = store.bootstrap_tx().await.unwrap();
        tx.serialize_key(&format!("video-user-{handle}"))
            .await
            .unwrap();
        let user = tx.insert_user(handle).await.unwrap();
        tx.commit().await.unwrap();
        user
    }

    pub(crate) async fn enqueue_job(
        store: &InMemoryStore,
        market: MarketId,
        kind: ArtifactKind,
        now: time::OffsetDateTime,
    ) -> JobId {
        let mut tx = store.video_tx().await.unwrap();
        let job = tx.enqueue(market, None, kind, now).await.unwrap();
        tx.commit().await.unwrap();
        job
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::test_support::{enqueue_job, insert_market, MemRenderer};
    use crate::fakes::{FakeClock, InMemoryStore};
    use crate::model::{ArtifactKind, ContentConfig, JobStatus};
    use crate::ports::{Clock, Store};
    use time::{Duration, OffsetDateTime};

    fn config() -> ContentConfig {
        ContentConfig::default()
    }

    fn start() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    async fn job_row(store: &InMemoryStore, job: crate::model::JobId) -> crate::model::VideoJobRow {
        let mut tx = store.video_tx().await.unwrap();
        let row = tx.job(job).await.unwrap().unwrap();
        tx.commit().await.unwrap();
        row
    }

    #[tokio::test]
    async fn tick_renders_attaches_and_emits_asset_event() {
        let store = InMemoryStore::default();
        let clock = FakeClock::at(start());
        let renderer = MemRenderer::default();
        let market = insert_market(&store).await;
        let job = enqueue_job(&store, market, ArtifactKind::Poster, start()).await;

        super::worker_tick(&store, &clock, &renderer, &config())
            .await
            .unwrap();

        let row = job_row(&store, job).await;
        assert_eq!(row.status, JobStatus::Attached);
        assert_eq!(row.attempts, 1);
        assert_eq!(
            row.asset_url.as_deref(),
            Some(format!("/assets/{}.svg", job.0).as_str())
        );
        assert!(row.claim_token.is_none());
        assert!(row.lease_expires_at.is_none());

        let mut tx = store.video_tx().await.unwrap();
        let market_row = tx.market_row(market).await.unwrap();
        assert_eq!(market_row.poster_asset_url, row.asset_url);
        assert_eq!(market_row.video_asset_url, None);
        let events = tx.moderation_events_after(0, 100).await.unwrap();
        tx.commit().await.unwrap();
        let attached: Vec<_> = events
            .iter()
            .filter(|event| event.event_type == "VideoAttached")
            .collect();
        assert_eq!(attached.len(), 1);
        assert_eq!(attached[0].aggregate_id, market.0);
        assert_eq!(attached[0].payload["kind"], "poster");
        assert_eq!(attached[0].payload["url"], row.asset_url.clone().unwrap());
        assert_eq!(renderer.persisted.lock().len(), 1);
    }

    #[tokio::test]
    async fn tick_with_market_video_kind_fills_video_column() {
        let store = InMemoryStore::default();
        let clock = FakeClock::at(start());
        let renderer = MemRenderer::default();
        let market = insert_market(&store).await;
        enqueue_job(&store, market, ArtifactKind::MarketVideo, start()).await;

        super::worker_tick(&store, &clock, &renderer, &config())
            .await
            .unwrap();

        let mut tx = store.video_tx().await.unwrap();
        let market_row = tx.market_row(market).await.unwrap();
        tx.commit().await.unwrap();
        assert!(market_row.video_asset_url.is_some());
        assert_eq!(market_row.poster_asset_url, None);
    }

    #[tokio::test]
    async fn render_failure_requeues_with_backoff_then_succeeds() {
        let store = InMemoryStore::default();
        let clock = FakeClock::at(start());
        let renderer = MemRenderer::default();
        *renderer.fail_renders.lock() = 1;
        let market = insert_market(&store).await;
        let job = enqueue_job(&store, market, ArtifactKind::Poster, start()).await;
        let config = config();

        super::worker_tick(&store, &clock, &renderer, &config)
            .await
            .unwrap();
        let row = job_row(&store, job).await;
        assert_eq!(row.status, JobStatus::Queued);
        assert_eq!(row.attempts, 1);
        // Pinned backoff: base * 2^min(attempts, 8) with attempts = 1.
        let backoff = i64::try_from(config.backoff_base_secs * 2).unwrap();
        assert_eq!(row.available_at, start() + Duration::seconds(backoff));
        assert!(row.claim_token.is_none());

        // Not yet available: an immediate tick claims nothing.
        super::worker_tick(&store, &clock, &renderer, &config)
            .await
            .unwrap();
        assert_eq!(job_row(&store, job).await.status, JobStatus::Queued);

        clock.advance(Duration::seconds(backoff));
        super::worker_tick(&store, &clock, &renderer, &config)
            .await
            .unwrap();
        let row = job_row(&store, job).await;
        assert_eq!(row.status, JobStatus::Attached);
        assert_eq!(row.attempts, 2);
    }

    #[tokio::test]
    async fn terminal_failure_on_the_inclusive_attempts_boundary() {
        let store = InMemoryStore::default();
        let clock = FakeClock::at(start());
        let renderer = MemRenderer::default();
        *renderer.fail_renders.lock() = 10;
        let market = insert_market(&store).await;
        let job = enqueue_job(&store, market, ArtifactKind::Poster, start()).await;
        // max_attempts = 1 means exactly one attempt, ever.
        let config = ContentConfig {
            video_max_attempts: 1,
            ..config()
        };

        super::worker_tick(&store, &clock, &renderer, &config)
            .await
            .unwrap();
        let row = job_row(&store, job).await;
        assert_eq!(row.status, JobStatus::Failed);
        assert_eq!(row.attempts, 1);

        // Terminal jobs never come back, no matter how far time advances.
        clock.advance(Duration::hours(24));
        super::worker_tick(&store, &clock, &renderer, &config)
            .await
            .unwrap();
        assert_eq!(job_row(&store, job).await.status, JobStatus::Failed);
    }

    #[tokio::test]
    async fn crashed_render_is_reclaimed_and_retried_byte_identically() {
        let store = InMemoryStore::default();
        let clock = FakeClock::at(start());
        let renderer = MemRenderer::default();
        let market = insert_market(&store).await;
        let job = enqueue_job(&store, market, ArtifactKind::Poster, start()).await;
        let config = config();
        let lease = i64::try_from(config.lease_secs).unwrap();

        // Simulate a worker that claimed, rendered, and crashed before the
        // completion transaction: the claim committed, nothing else did.
        {
            let mut tx = store.video_tx().await.unwrap();
            let claimed = tx
                .claim(clock.now(), Duration::seconds(lease), 8)
                .await
                .unwrap();
            assert_eq!(claimed.len(), 1);
            tx.commit().await.unwrap();
            let spec = domain::render_spec::poster_spec("video-question");
            renderer.persisted.lock().push((
                job,
                ArtifactKind::Poster,
                super::test_support::spec_bytes(&spec),
            ));
        }
        assert_eq!(job_row(&store, job).await.status, JobStatus::Rendering);

        // Before the lease expires nothing is claimable.
        super::worker_tick(&store, &clock, &renderer, &config)
            .await
            .unwrap();
        assert_eq!(job_row(&store, job).await.status, JobStatus::Rendering);

        clock.advance(Duration::seconds(lease));
        super::worker_tick(&store, &clock, &renderer, &config)
            .await
            .unwrap();
        let row = job_row(&store, job).await;
        assert_eq!(row.status, JobStatus::Attached);
        assert_eq!(row.attempts, 2);
        // Determinism: the retry rendered byte-identical output for the spec.
        let persisted = renderer.persisted.lock().clone();
        assert_eq!(persisted.len(), 2);
        let question_spec = {
            let mut tx = store.video_tx().await.unwrap();
            let market_row = tx.market_row(market).await.unwrap();
            domain::render_spec::poster_spec(&market_row.question)
        };
        assert_eq!(
            persisted[1].2,
            super::test_support::spec_bytes(&question_spec)
        );
    }

    #[tokio::test]
    async fn empty_tick_is_a_no_op() {
        let store = InMemoryStore::default();
        let clock = FakeClock::at(start());
        let renderer = MemRenderer::default();
        super::worker_tick(&store, &clock, &renderer, &config())
            .await
            .unwrap();
        assert!(renderer.persisted.lock().is_empty());
    }
}
