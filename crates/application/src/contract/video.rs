//! `VideoTx` contract suites (Task 5.2): leasing, token-fenced completion,
//! reclaim boundaries, enqueue idempotency, attach hot-swap, share-card
//! reads, and the shared moderation-job machinery. Generic over [`Store`] —
//! the same assertions run against `InMemoryStore` here and `PgStore` from
//! the adapters crate.

use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::error::StoreError;
use crate::model::{
    ArtifactKind, CommentId, DraftId, JobStatus, MarketId, ModerationJobStatus, ModerationStatus,
    NewComment, NewMarket, RealizationFact, RealizationSource, UserId,
};
use crate::ports::Store;

use super::unique_key;

fn at(unix: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(unix).unwrap()
}

const T0: i64 = 1_700_000_000;

async fn video_market<S: Store + ?Sized>(store: &S) -> MarketId {
    let mut tx = store.seed_tx().await.unwrap();
    let key = unique_key("video-market");
    tx.serialize_key(&key).await.unwrap();
    let market = tx
        .insert_market(NewMarket {
            id: MarketId(Uuid::new_v4()),
            slug: format!("video-{key}"),
            min_votes_to_resolve: 3,
            closes_at: at(T0) + Duration::hours(2),
            tally_hidden_at: at(T0) + Duration::hours(1),
        })
        .await
        .unwrap();
    tx.commit().await.unwrap();
    market
}

async fn video_user<S: Store + ?Sized>(store: &S) -> UserId {
    let mut tx = store.bootstrap_tx().await.unwrap();
    let key = unique_key("video-user");
    tx.serialize_key(&key).await.unwrap();
    let user = tx.insert_user(&key).await.unwrap();
    tx.commit().await.unwrap();
    user
}

async fn enqueue<S: Store + ?Sized>(
    store: &S,
    market: MarketId,
    draft: Option<DraftId>,
    kind: ArtifactKind,
    now: OffsetDateTime,
) -> crate::model::JobId {
    let mut tx = store.video_tx().await.unwrap();
    let job = tx.enqueue(market, draft, kind, now).await.unwrap();
    tx.commit().await.unwrap();
    job
}

async fn claim_one<S: Store + ?Sized>(
    store: &S,
    now: OffsetDateTime,
    lease_secs: i64,
) -> crate::model::VideoJobRow {
    let mut tx = store.video_tx().await.unwrap();
    let mut jobs = tx
        .claim(now, Duration::seconds(lease_secs), 1)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(jobs.len(), 1, "expected exactly one claimable job");
    jobs.pop().unwrap()
}

async fn job_row<S: Store + ?Sized>(
    store: &S,
    job: crate::model::JobId,
) -> crate::model::VideoJobRow {
    let mut tx = store.video_tx().await.unwrap();
    let row = tx.job(job).await.unwrap().unwrap();
    tx.commit().await.unwrap();
    row
}

/// Enqueue is idempotent per the durable issuance key `(draft, kind)` across
/// ALL statuses (codex P5R2 N2) and per the active `(market, kind)` unique.
/// `draft` must reference a REAL draft row on FK-enforcing backends; the
/// caller supplies it (the fake accepts any id, Postgres pre-creates one).
pub async fn enqueue_idempotency_contract<S: Store + ?Sized>(store: &S, draft: DraftId) {
    let market = video_market(store).await;
    let now = at(T0);

    let poster = enqueue(store, market, Some(draft), ArtifactKind::Poster, now).await;
    // Replay with the draft key returns the SAME canonical job.
    assert_eq!(
        enqueue(store, market, Some(draft), ArtifactKind::Poster, now).await,
        poster
    );
    // The active (market, kind) unique absorbs draft-less enqueues too.
    assert_eq!(
        enqueue(store, market, None, ArtifactKind::Poster, now).await,
        poster
    );
    // A different kind is a different canonical job (queued strictly later
    // so the claim below deterministically takes the poster first).
    let video = enqueue(
        store,
        market,
        Some(draft),
        ArtifactKind::MarketVideo,
        at(T0 + 60),
    )
    .await;
    assert_ne!(video, poster);

    // Drive the poster job terminal: the draft key STILL returns it (a job
    // that failed before the saga persisted jobs_enqueued is never re-issued)
    // while the draft-less active unique no longer blocks a fresh job.
    let claimed = claim_one(store, now, 60).await;
    assert_eq!(claimed.id, poster);
    {
        let mut tx = store.video_tx().await.unwrap();
        assert!(tx
            .complete_error(poster, claimed.claim_token.unwrap(), "boom", now, true)
            .await
            .unwrap());
        tx.commit().await.unwrap();
    }
    assert_eq!(job_row(store, poster).await.status, JobStatus::Failed);
    assert_eq!(
        enqueue(store, market, Some(draft), ArtifactKind::Poster, now).await,
        poster
    );
    let fresh = enqueue(store, market, None, ArtifactKind::Poster, now).await;
    assert_ne!(fresh, poster);

    // Unknown market is a typed rejection.
    let mut tx = store.video_tx().await.unwrap();
    assert!(matches!(
        tx.enqueue(MarketId(Uuid::new_v4()), None, ArtifactKind::Poster, now)
            .await,
        Err(StoreError::NotFound("market"))
    ));
}

/// Claim stamps `rendering + token + lease + attempts+1`, respects
/// `available_at`, claims oldest-first, and two concurrent claimers get
/// disjoint jobs (SKIP LOCKED semantics).
pub async fn claim_leasing_contract<S: Store + ?Sized>(store: &S) {
    let market_a = video_market(store).await;
    let market_b = video_market(store).await;
    let market_c = video_market(store).await;
    let first = enqueue(store, market_a, None, ArtifactKind::Poster, at(T0)).await;
    let second = enqueue(store, market_b, None, ArtifactKind::Poster, at(T0 + 10)).await;
    // Not yet available: must not be claimable.
    let future = enqueue(
        store,
        market_c,
        None,
        ArtifactKind::Poster,
        at(T0) + Duration::hours(6),
    )
    .await;

    let now = at(T0 + 60);
    // Two claimers with open transactions: disjoint claims.
    let mut tx1 = store.video_tx().await.unwrap();
    let mut tx2 = store.video_tx().await.unwrap();
    let batch1 = tx1.claim(now, Duration::seconds(60), 1).await.unwrap();
    let batch2 = tx2.claim(now, Duration::seconds(60), 1).await.unwrap();
    tx1.commit().await.unwrap();
    tx2.commit().await.unwrap();
    assert_eq!(batch1.len(), 1);
    assert_eq!(batch2.len(), 1);
    // Oldest available first, and never the same job twice.
    assert_eq!(batch1[0].id, first);
    assert_eq!(batch2[0].id, second);

    for row in [&batch1[0], &batch2[0]] {
        assert_eq!(row.status, JobStatus::Rendering);
        assert_eq!(row.attempts, 1);
        assert!(row.claim_token.is_some());
        assert_eq!(row.lease_expires_at, Some(now + Duration::seconds(60)));
    }

    // The future job stays untouched; an exhausted queue claims nothing.
    let mut tx = store.video_tx().await.unwrap();
    assert!(tx
        .claim(now, Duration::seconds(60), 8)
        .await
        .unwrap()
        .is_empty());
    tx.commit().await.unwrap();
    assert_eq!(job_row(store, future).await.status, JobStatus::Queued);
}

/// Success completion is a token-fenced CAS: wrong token drops, right token
/// lands `ready` + asset URL + cleared lease/token, and a reclaimed job's
/// stale attempt can never complete (success OR error path — P5R2 N3).
pub async fn completion_cas_contract<S: Store + ?Sized>(store: &S) {
    let market = video_market(store).await;
    let job = enqueue(store, market, None, ArtifactKind::Poster, at(T0)).await;
    let claimed = claim_one(store, at(T0 + 1), 60).await;
    let token = claimed.claim_token.unwrap();

    // Wrong token: dropped silently, nothing changes.
    {
        let mut tx = store.video_tx().await.unwrap();
        assert!(!tx
            .complete_ready(job, Uuid::new_v4(), "/assets/x.svg", at(T0 + 2))
            .await
            .unwrap());
        assert!(!tx
            .complete_error(job, Uuid::new_v4(), "boom", at(T0 + 2), false)
            .await
            .unwrap());
        tx.commit().await.unwrap();
    }
    assert_eq!(job_row(store, job).await.status, JobStatus::Rendering);

    // Right token: ready with the asset URL, lease and token cleared.
    {
        let mut tx = store.video_tx().await.unwrap();
        assert!(tx
            .complete_ready(job, token, "/assets/a.svg", at(T0 + 3))
            .await
            .unwrap());
        tx.commit().await.unwrap();
    }
    let row = job_row(store, job).await;
    assert_eq!(row.status, JobStatus::Ready);
    assert_eq!(row.asset_url.as_deref(), Some("/assets/a.svg"));
    assert!(row.claim_token.is_none());
    assert!(row.lease_expires_at.is_none());

    // A ready job is no longer completable (status fence).
    {
        let mut tx = store.video_tx().await.unwrap();
        assert!(!tx
            .complete_ready(job, token, "/assets/b.svg", at(T0 + 4))
            .await
            .unwrap());
        tx.commit().await.unwrap();
    }

    // Stale-attempt drop: claim, expire, reclaim, re-claim under a new
    // token — the FIRST token's success and error completions both drop.
    let market2 = video_market(store).await;
    let job2 = enqueue(store, market2, None, ArtifactKind::Poster, at(T0)).await;
    let stale = claim_one(store, at(T0 + 10), 30).await;
    assert_eq!(stale.id, job2);
    let stale_token = stale.claim_token.unwrap();
    {
        let mut tx = store.video_tx().await.unwrap();
        assert_eq!(
            tx.reclaim_expired(at(T0 + 41), 3, 8).await.unwrap(),
            1,
            "expired lease reclaims"
        );
        tx.commit().await.unwrap();
    }
    let fresh = claim_one(store, at(T0 + 42), 60).await;
    assert_eq!(fresh.id, job2);
    let fresh_token = fresh.claim_token.unwrap();
    assert_ne!(stale_token, fresh_token);
    {
        let mut tx = store.video_tx().await.unwrap();
        assert!(!tx
            .complete_ready(job2, stale_token, "/assets/stale.svg", at(T0 + 43))
            .await
            .unwrap());
        assert!(!tx
            .complete_error(job2, stale_token, "stale", at(T0 + 43), false)
            .await
            .unwrap());
        assert!(tx
            .complete_ready(job2, fresh_token, "/assets/fresh.svg", at(T0 + 44))
            .await
            .unwrap());
        tx.commit().await.unwrap();
    }
    let row2 = job_row(store, job2).await;
    assert_eq!(row2.status, JobStatus::Ready);
    assert_eq!(row2.asset_url.as_deref(), Some("/assets/fresh.svg"));
}

/// Error completion requeues with the caller's pinned backoff and cleared
/// lease/token, or goes terminal; attempts accumulate across claims.
pub async fn completion_error_contract<S: Store + ?Sized>(store: &S) {
    let market = video_market(store).await;
    let job = enqueue(store, market, None, ArtifactKind::Poster, at(T0)).await;

    let first = claim_one(store, at(T0 + 1), 60).await;
    assert_eq!(first.attempts, 1);
    let retry_at = at(T0 + 100);
    {
        let mut tx = store.video_tx().await.unwrap();
        assert!(tx
            .complete_error(job, first.claim_token.unwrap(), "boom", retry_at, false)
            .await
            .unwrap());
        tx.commit().await.unwrap();
    }
    let row = job_row(store, job).await;
    assert_eq!(row.status, JobStatus::Queued);
    assert_eq!(row.available_at, retry_at);
    assert!(row.claim_token.is_none());
    assert!(row.lease_expires_at.is_none());
    assert_eq!(row.attempts, 1);

    // Not claimable before the backoff elapses.
    {
        let mut tx = store.video_tx().await.unwrap();
        assert!(tx
            .claim(at(T0 + 50), Duration::seconds(60), 8)
            .await
            .unwrap()
            .is_empty());
        tx.commit().await.unwrap();
    }

    let second = claim_one(store, retry_at, 60).await;
    assert_eq!(second.attempts, 2);
    {
        let mut tx = store.video_tx().await.unwrap();
        assert!(tx
            .complete_error(
                job,
                second.claim_token.unwrap(),
                "boom again",
                at(T0 + 500),
                true,
            )
            .await
            .unwrap());
        tx.commit().await.unwrap();
    }
    let row = job_row(store, job).await;
    assert_eq!(row.status, JobStatus::Failed);
    assert!(row.claim_token.is_none());
    assert!(row.lease_expires_at.is_none());
}

/// Expired-lease reclaim applies the identical `>=` attempts boundary and
/// clears lease/token (max = 1 → one attempt ever); unexpired leases and
/// the batch limit are respected.
pub async fn reclaim_boundary_contract<S: Store + ?Sized>(store: &S) {
    let market_a = video_market(store).await;
    let market_b = video_market(store).await;
    let job_a = enqueue(store, market_a, None, ArtifactKind::Poster, at(T0)).await;
    let job_b = enqueue(store, market_b, None, ArtifactKind::Poster, at(T0 + 1)).await;

    // Claim both under a 30s lease.
    {
        let mut tx = store.video_tx().await.unwrap();
        let jobs = tx
            .claim(at(T0 + 2), Duration::seconds(30), 8)
            .await
            .unwrap();
        assert_eq!(jobs.len(), 2);
        tx.commit().await.unwrap();
    }

    // Before expiry: nothing to reclaim.
    {
        let mut tx = store.video_tx().await.unwrap();
        assert_eq!(tx.reclaim_expired(at(T0 + 20), 3, 8).await.unwrap(), 0);
        tx.commit().await.unwrap();
    }

    // After expiry with a limit of 1: exactly one reclaim per call, ordered
    // by lease expiry.
    {
        let mut tx = store.video_tx().await.unwrap();
        assert_eq!(tx.reclaim_expired(at(T0 + 60), 3, 1).await.unwrap(), 1);
        assert_eq!(tx.reclaim_expired(at(T0 + 60), 3, 8).await.unwrap(), 1);
        tx.commit().await.unwrap();
    }
    for job in [job_a, job_b] {
        let row = job_row(store, job).await;
        assert_eq!(row.status, JobStatus::Queued);
        assert!(row.claim_token.is_none());
        assert!(row.lease_expires_at.is_none());
        assert_eq!(row.attempts, 1);
    }

    // Boundary: with max_attempts = 1 an expired first attempt is terminal.
    let claimed = claim_one(store, at(T0 + 61), 10).await;
    let terminal_job = claimed.id;
    {
        let mut tx = store.video_tx().await.unwrap();
        assert_eq!(tx.reclaim_expired(at(T0 + 100), 1, 8).await.unwrap(), 1);
        tx.commit().await.unwrap();
    }
    let row = job_row(store, terminal_job).await;
    assert_eq!(row.status, JobStatus::Failed);
    assert!(row.claim_token.is_none());
    assert!(row.lease_expires_at.is_none());
}

/// `AttachReady`: under the market row lock the asset column fills by kind, the
/// job flips `ready → attached`, and a versioned `VideoAttached` event lands
/// in the outbox — all in one commit. Non-ready jobs refuse to attach.
pub async fn attach_ready_contract<S: Store + ?Sized>(store: &S) {
    let market = video_market(store).await;
    let poster = enqueue(store, market, None, ArtifactKind::Poster, at(T0)).await;

    // Queued and missing jobs refuse.
    {
        let mut tx = store.video_tx().await.unwrap();
        assert!(!tx.attach_ready(poster).await.unwrap());
        assert!(!tx
            .attach_ready(crate::model::JobId(Uuid::new_v4()))
            .await
            .unwrap());
        tx.commit().await.unwrap();
    }

    let claimed = claim_one(store, at(T0 + 1), 60).await;
    let cursor_before = {
        let mut tx = store.video_tx().await.unwrap();
        let events = tx.moderation_events_after(0, 1_000).await.unwrap();
        tx.commit().await.unwrap();
        events.last().map_or(0, |event| event.seq)
    };
    // Completion + attach ride one transaction (codex B5).
    {
        let mut tx = store.video_tx().await.unwrap();
        assert!(tx
            .complete_ready(
                poster,
                claimed.claim_token.unwrap(),
                "/assets/p.svg",
                at(T0 + 2)
            )
            .await
            .unwrap());
        let ready = tx.job(poster).await.unwrap().unwrap();
        assert_eq!(ready.status, JobStatus::Ready);
        assert_eq!(ready.asset_url.as_deref(), Some("/assets/p.svg"));
        assert_eq!(ready.available_at, at(T0 + 2));
        assert_eq!(ready.claim_token, None);
        assert_eq!(ready.lease_expires_at, None);
        assert!(tx.attach_ready(poster).await.unwrap());
        assert_eq!(
            tx.job(poster).await.unwrap().unwrap().status,
            JobStatus::Attached
        );
        tx.commit().await.unwrap();
    }
    assert_eq!(job_row(store, poster).await.status, JobStatus::Attached);
    {
        let mut tx = store.video_tx().await.unwrap();
        let row = tx.market_row(market).await.unwrap();
        assert_eq!(row.poster_asset_url.as_deref(), Some("/assets/p.svg"));
        assert_eq!(row.video_asset_url, None);
        let events = tx
            .moderation_events_after(cursor_before, 1_000)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let attached: Vec<_> = events
            .iter()
            .filter(|event| event.event_type == "VideoAttached")
            .collect();
        assert_eq!(attached.len(), 1);
        assert_eq!(attached[0].aggregate_id, market.0);
        assert_eq!(attached[0].aggregate_type, "market");
        assert_eq!(attached[0].payload["kind"], "poster");
        assert_eq!(attached[0].payload["url"], "/assets/p.svg");
    }

    // An attached job cannot attach twice.
    {
        let mut tx = store.video_tx().await.unwrap();
        assert!(!tx.attach_ready(poster).await.unwrap());
        tx.commit().await.unwrap();
    }

    // The market_video kind fills the other column.
    let video = enqueue(store, market, None, ArtifactKind::MarketVideo, at(T0 + 3)).await;
    let claimed = claim_one(store, at(T0 + 4), 60).await;
    assert_eq!(claimed.id, video);
    {
        let mut tx = store.video_tx().await.unwrap();
        assert!(tx
            .complete_ready(
                video,
                claimed.claim_token.unwrap(),
                "/assets/v.svg",
                at(T0 + 5)
            )
            .await
            .unwrap());
        assert!(tx.attach_ready(video).await.unwrap());
        tx.commit().await.unwrap();
    }
    {
        let mut tx = store.video_tx().await.unwrap();
        let row = tx.market_row(market).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(row.poster_asset_url.as_deref(), Some("/assets/p.svg"));
        assert_eq!(row.video_asset_url.as_deref(), Some("/assets/v.svg"));
    }
}

/// Share-card reads: handles, ordered realization facts, and typed
/// not-found rejections.
pub async fn video_reads_contract<S: Store + ?Sized>(store: &S) {
    let market = video_market(store).await;
    let user = video_user(store).await;

    let mut tx = store.video_tx().await.unwrap();
    assert!(tx
        .job(crate::model::JobId(Uuid::new_v4()))
        .await
        .unwrap()
        .is_none());
    assert!(matches!(
        tx.market_row(MarketId(Uuid::new_v4())).await,
        Err(StoreError::NotFound("market"))
    ));
    assert!(matches!(
        tx.user_handle(UserId(Uuid::new_v4())).await,
        Err(StoreError::NotFound("user"))
    ));
    assert!(!tx.user_handle(user).await.unwrap().is_empty());
    assert!(tx.realizations(user, market).await.unwrap().is_empty());
    let yes_outcome = tx.market_row(market).await.unwrap().yes_outcome;
    tx.commit().await.unwrap();

    // Mint two REAL ledger transactions through the ports (realizations
    // reference them by FK on backends that enforce it).
    let mut txns = Vec::new();
    for _ in 0..2 {
        let key = unique_key("video-fact-txn");
        let mut tx = store.trade_tx().await.unwrap();
        tx.serialize_key(&key).await.unwrap();
        let external = tx
            .account(
                crate::model::OwnerRef::External,
                domain::ledger::Currency::Usdc,
            )
            .await
            .unwrap();
        let account = tx
            .account(
                crate::model::OwnerRef::User(user),
                domain::ledger::Currency::Usdc,
            )
            .await
            .unwrap();
        let txn = tx
            .ledger_apply(
                domain::ledger::TxnKind::Deposit,
                &key,
                &[
                    domain::ledger::Entry {
                        account: external,
                        amount: domain::money::MicroUsd(-5),
                    },
                    domain::ledger::Entry {
                        account,
                        amount: domain::money::MicroUsd(5),
                    },
                ],
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        txns.push(txn);
    }

    // Insert two facts with out-of-order timestamps; reads return them
    // ordered by (created_at, ledger_txn).
    let later = RealizationFact {
        user,
        market,
        outcome: yes_outcome,
        source: RealizationSource::Settlement,
        realized_delta: domain::money::MicroUsd(2_000_000),
        payout: domain::money::MicroUsd(12_000_000),
        ledger_txn: txns[0],
        created_at: at(T0 + 500),
    };
    let earlier = RealizationFact {
        source: RealizationSource::Sell,
        realized_delta: domain::money::MicroUsd(1_000_000),
        payout: domain::money::MicroUsd(3_000_000),
        ledger_txn: txns[1],
        created_at: at(T0 + 100),
        ..later
    };
    for fact in [&later, &earlier] {
        let mut tx = store.resolve_tx().await.unwrap();
        tx.serialize_key(&unique_key("video-fact")).await.unwrap();
        assert!(tx.insert_realization(fact).await.unwrap());
        tx.commit().await.unwrap();
    }
    let mut tx = store.video_tx().await.unwrap();
    let facts = tx.realizations(user, market).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(facts.len(), 2);
    assert_eq!(facts[0].created_at, earlier.created_at);
    assert_eq!(facts[1].created_at, later.created_at);
}

async fn video_comment<S: Store + ?Sized>(
    store: &S,
    market: MarketId,
    author: UserId,
) -> CommentId {
    let mut tx = store.comment_tx().await.unwrap();
    let key = unique_key("video-comment");
    tx.serialize_key(&key).await.unwrap();
    let comment = CommentId(Uuid::new_v4());
    tx.insert_comment(NewComment {
        id: comment,
        market,
        author,
        parent: None,
        body: format!("body {key}"),
        body_hash: format!("hash-{key}"),
        moderation_status: ModerationStatus::Visible,
        depth: 0,
        created_at: at(T0),
    })
    .await
    .unwrap();
    tx.append(crate::model::Event {
        event_type: "CommentPosted",
        aggregate_type: "comment",
        aggregate_id: comment.0,
        payload: serde_json::json!({ "comment_id": comment.0 }),
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();
    comment
}

/// The shared moderation-job machinery: cursor lock/save, idempotent
/// materialization (the unique absorbs replays), disjoint leased claims, and
/// token-fenced completion for done/requeue/terminal.
#[allow(clippy::too_many_lines)]
pub async fn moderation_jobs_contract<S: Store + ?Sized>(store: &S) {
    let market = video_market(store).await;
    let author = video_user(store).await;
    let comment_a = video_comment(store, market, author).await;
    let comment_b = video_comment(store, market, author).await;

    // Cursor: lock, read, advance, and read back — one serialized consumer.
    {
        let mut tx = store.video_tx().await.unwrap();
        let cursor = tx.lock_moderation_cursor().await.unwrap();
        assert!(cursor >= 0);
        tx.save_moderation_cursor(cursor + 7).await.unwrap();
        tx.commit().await.unwrap();
        let mut tx = store.video_tx().await.unwrap();
        assert_eq!(tx.lock_moderation_cursor().await.unwrap(), cursor + 7);
        tx.commit().await.unwrap();
    }

    // Materialization is idempotent per comment (replay absorbs), atomic
    // with the cursor advance.
    {
        let mut tx = store.video_tx().await.unwrap();
        tx.lock_moderation_cursor().await.unwrap();
        let created = tx
            .materialize_moderation_jobs(&[comment_a, comment_a, comment_b], at(T0 + 1))
            .await
            .unwrap();
        assert_eq!(created, 2);
        tx.save_moderation_cursor(50).await.unwrap();
        tx.commit().await.unwrap();
    }
    {
        let mut tx = store.video_tx().await.unwrap();
        tx.lock_moderation_cursor().await.unwrap();
        assert_eq!(
            tx.materialize_moderation_jobs(&[comment_a, comment_b], at(T0 + 2))
                .await
                .unwrap(),
            0,
            "replayed materialization must absorb"
        );
        tx.commit().await.unwrap();
    }

    // Two claimers get disjoint moderation jobs.
    let now = at(T0 + 10);
    let mut tx1 = store.video_tx().await.unwrap();
    let mut tx2 = store.video_tx().await.unwrap();
    let batch1 = tx1
        .claim_moderation(now, Duration::seconds(60), 1)
        .await
        .unwrap();
    let batch2 = tx2
        .claim_moderation(now, Duration::seconds(60), 1)
        .await
        .unwrap();
    tx1.commit().await.unwrap();
    tx2.commit().await.unwrap();
    assert_eq!((batch1.len(), batch2.len()), (1, 1));
    assert_ne!(batch1[0].id, batch2[0].id);
    for row in [&batch1[0], &batch2[0]] {
        assert_eq!(row.status, ModerationJobStatus::Running);
        assert_eq!(row.attempts, 1);
        assert!(row.claim_token.is_some());
        assert_eq!(row.lease_expires_at, Some(now + Duration::seconds(60)));
    }

    // Token-fenced completion: stale token drops; success lands Done.
    let (first, second) = (&batch1[0], &batch2[0]);
    {
        let mut tx = store.video_tx().await.unwrap();
        assert!(!tx
            .complete_moderation(first.id, Uuid::new_v4(), None, at(T0 + 11), false)
            .await
            .unwrap());
        assert!(tx
            .complete_moderation(
                first.id,
                first.claim_token.unwrap(),
                None,
                at(T0 + 11),
                false
            )
            .await
            .unwrap());
        tx.commit().await.unwrap();
    }
    // Error non-terminal requeues with the pinned availability and cleared
    // lease/token; a fresh claim then errors terminally to Failed.
    let retry_at = at(T0 + 200);
    {
        let mut tx = store.video_tx().await.unwrap();
        assert!(tx
            .complete_moderation(
                second.id,
                second.claim_token.unwrap(),
                Some("preflight unavailable"),
                retry_at,
                false,
            )
            .await
            .unwrap());
        tx.commit().await.unwrap();
    }
    {
        let mut tx = store.video_tx().await.unwrap();
        // Before the retry time nothing claims.
        assert!(tx
            .claim_moderation(at(T0 + 20), Duration::seconds(60), 8)
            .await
            .unwrap()
            .is_empty());
        let requeued = tx
            .claim_moderation(retry_at, Duration::seconds(60), 8)
            .await
            .unwrap();
        assert_eq!(requeued.len(), 1);
        assert_eq!(requeued[0].id, second.id);
        assert_eq!(requeued[0].attempts, 2);
        assert!(tx
            .complete_moderation(
                second.id,
                requeued[0].claim_token.unwrap(),
                Some("hard failure"),
                retry_at,
                true,
            )
            .await
            .unwrap());
        tx.commit().await.unwrap();
    }

    // Expired-lease reclaim (5.4 review B1): a crashed claimer's `running`
    // rows come back — queued below the attempts boundary, failed at it —
    // honoring the limit in lease-expiry order; a live lease is never
    // touched.
    let comment_c = video_comment(store, market, author).await;
    let comment_d = video_comment(store, market, author).await;
    {
        let mut tx = store.video_tx().await.unwrap();
        tx.lock_moderation_cursor().await.unwrap();
        assert_eq!(
            tx.materialize_moderation_jobs(&[comment_c, comment_d], at(T0 + 300))
                .await
                .unwrap(),
            2
        );
        tx.save_moderation_cursor(51).await.unwrap();
        tx.commit().await.unwrap();
    }
    let (job_c, job_d) = {
        let mut tx = store.video_tx().await.unwrap();
        let claimed = tx
            .claim_moderation(at(T0 + 301), Duration::seconds(10), 8)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(claimed.len(), 2);
        assert_eq!((claimed[0].attempts, claimed[1].attempts), (1, 1));
        (claimed[0].id, claimed[1].id)
    };
    {
        // Live leases: nothing to reclaim, nothing to claim.
        let mut tx = store.video_tx().await.unwrap();
        assert_eq!(
            tx.reclaim_moderation_expired(at(T0 + 305), 3, 8)
                .await
                .unwrap(),
            0
        );
        assert!(tx
            .claim_moderation(at(T0 + 305), Duration::seconds(10), 8)
            .await
            .unwrap()
            .is_empty());
        tx.commit().await.unwrap();
    }
    {
        // Expired below the boundary: the limit bounds each call, then the
        // rows are re-claimable with cleared token/lease and the next
        // attempt number.
        let mut tx = store.video_tx().await.unwrap();
        assert_eq!(
            tx.reclaim_moderation_expired(at(T0 + 311), 3, 1)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            tx.reclaim_moderation_expired(at(T0 + 311), 3, 8)
                .await
                .unwrap(),
            1
        );
        let reclaimed = tx
            .claim_moderation(at(T0 + 311), Duration::seconds(10), 8)
            .await
            .unwrap();
        assert_eq!(reclaimed.len(), 2);
        let ids: std::collections::BTreeSet<_> = reclaimed.iter().map(|job| job.id).collect();
        assert!(ids.contains(&job_c) && ids.contains(&job_d));
        assert_eq!((reclaimed[0].attempts, reclaimed[1].attempts), (2, 2));
        tx.commit().await.unwrap();
    }
    {
        // `comment_d`'s attempt completes; `comment_c`'s crashes again and
        // expires AT the boundary: terminal — it never claims again.
        let mut tx = store.video_tx().await.unwrap();
        let running = tx
            .claim_moderation(at(T0 + 311), Duration::seconds(10), 8)
            .await
            .unwrap();
        assert!(running.is_empty(), "both rows are already leased");
        tx.commit().await.unwrap();
    }
    {
        let mut tx = store.video_tx().await.unwrap();
        assert_eq!(
            tx.reclaim_moderation_expired(at(T0 + 322), 2, 8)
                .await
                .unwrap(),
            2
        );
        assert!(tx
            .claim_moderation(at(T0 + 322), Duration::seconds(10), 8)
            .await
            .unwrap()
            .is_empty());
        tx.commit().await.unwrap();
    }

    // Cursor events read: the outbox is visible through the port with
    // strictly increasing sequences and honors the limit.
    let mut tx = store.video_tx().await.unwrap();
    let events = tx.moderation_events_after(0, 5).await.unwrap();
    assert!(events.len() <= 5);
    for pair in events.windows(2) {
        assert!(pair[0].seq < pair[1].seq);
    }
    tx.commit().await.unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::{InMemoryStore, UnavailableRenderer};
    use crate::model::{ModerationJobRow, OutboxEvent, RenderedArtifact, VideoJobRow};
    use crate::ports::{Committable, Renderer, VideoTx};
    use async_trait::async_trait;

    #[tokio::test]
    async fn fake_enqueue_idempotency() {
        // The fake enforces no draft FK: any id exercises the issuance key.
        enqueue_idempotency_contract(&InMemoryStore::default(), DraftId(Uuid::new_v4())).await;
    }

    #[tokio::test]
    async fn fake_claim_leasing() {
        claim_leasing_contract(&InMemoryStore::default()).await;
    }

    #[tokio::test]
    async fn fake_completion_cas() {
        completion_cas_contract(&InMemoryStore::default()).await;
    }

    #[tokio::test]
    async fn fake_completion_error() {
        completion_error_contract(&InMemoryStore::default()).await;
    }

    #[tokio::test]
    async fn fake_reclaim_boundary() {
        reclaim_boundary_contract(&InMemoryStore::default()).await;
    }

    #[tokio::test]
    async fn fake_attach_ready() {
        attach_ready_contract(&InMemoryStore::default()).await;
    }

    #[tokio::test]
    async fn fake_video_reads() {
        video_reads_contract(&InMemoryStore::default()).await;
    }

    #[tokio::test]
    async fn fake_moderation_jobs() {
        moderation_jobs_contract(&InMemoryStore::default()).await;
    }

    #[tokio::test]
    async fn fake_save_cursor_requires_lock() {
        let store = InMemoryStore::default();
        let mut tx = store.video_tx().await.unwrap();
        assert!(matches!(
            tx.save_moderation_cursor(9).await,
            Err(StoreError::Invariant("moderation cursor not locked"))
        ));
    }

    #[tokio::test]
    async fn unavailable_renderer_reports_typed_unavailability() {
        let spec = domain::render_spec::poster_spec("q");
        assert_eq!(
            UnavailableRenderer.render(&spec).await,
            Err(StoreError::Unavailable("phase5:renderer"))
        );
        assert_eq!(
            UnavailableRenderer
                .persist(
                    std::path::Path::new("/tmp/none"),
                    crate::model::JobId(Uuid::new_v4()),
                    ArtifactKind::Poster,
                    &RenderedArtifact {
                        bytes: Vec::new(),
                        media_type: String::new(),
                    },
                )
                .await,
            Err(StoreError::Unavailable("phase5:renderer"))
        );
        assert_eq!(
            UnavailableRenderer
                .share_card(std::path::Path::new("/tmp/none"), "addr", &spec)
                .await,
            Err(StoreError::Unavailable("phase5:renderer"))
        );
        assert_eq!(
            UnavailableRenderer
                .load(
                    std::path::Path::new("/tmp/none"),
                    crate::model::JobId(Uuid::new_v4()),
                    ArtifactKind::Poster,
                )
                .await,
            Err(StoreError::Unavailable("phase5:renderer"))
        );
    }

    /// A minimal implementor exercising the defaulted read methods — the
    /// wave-compatibility surface for peer test doubles.
    struct DefaultOnlyTx {
        complete_ready: bool,
        attach_ready: bool,
    }

    #[async_trait]
    impl Committable for DefaultOnlyTx {
        async fn commit(self: Box<Self>) -> Result<(), StoreError> {
            Ok(())
        }
    }

    #[async_trait]
    impl VideoTx for DefaultOnlyTx {
        async fn enqueue(
            &mut self,
            _market: MarketId,
            _draft: Option<DraftId>,
            _kind: ArtifactKind,
            _now: OffsetDateTime,
        ) -> Result<crate::model::JobId, StoreError> {
            Err(StoreError::Unavailable("test"))
        }
        async fn claim(
            &mut self,
            _now: OffsetDateTime,
            _lease: Duration,
            _limit: u32,
        ) -> Result<Vec<VideoJobRow>, StoreError> {
            Err(StoreError::Unavailable("test"))
        }
        async fn complete_ready(
            &mut self,
            _job: crate::model::JobId,
            _token: Uuid,
            _asset_url: &str,
            _now: OffsetDateTime,
        ) -> Result<bool, StoreError> {
            Ok(self.complete_ready)
        }
        async fn complete_error(
            &mut self,
            _job: crate::model::JobId,
            _token: Uuid,
            _error: &str,
            _available_at: OffsetDateTime,
            _terminal: bool,
        ) -> Result<bool, StoreError> {
            Err(StoreError::Unavailable("test"))
        }
        async fn reclaim_expired(
            &mut self,
            _now: OffsetDateTime,
            _max_attempts: u32,
            _limit: u32,
        ) -> Result<u32, StoreError> {
            Err(StoreError::Unavailable("test"))
        }
        async fn attach_ready(&mut self, _job: crate::model::JobId) -> Result<bool, StoreError> {
            Ok(self.attach_ready)
        }
        async fn lock_moderation_cursor(&mut self) -> Result<i64, StoreError> {
            Err(StoreError::Unavailable("test"))
        }
        async fn moderation_events_after(
            &mut self,
            _after: i64,
            _limit: u32,
        ) -> Result<Vec<OutboxEvent>, StoreError> {
            Err(StoreError::Unavailable("test"))
        }
        async fn materialize_moderation_jobs(
            &mut self,
            _comments: &[CommentId],
            _now: OffsetDateTime,
        ) -> Result<u32, StoreError> {
            Err(StoreError::Unavailable("test"))
        }
        async fn save_moderation_cursor(&mut self, _seq: i64) -> Result<(), StoreError> {
            Err(StoreError::Unavailable("test"))
        }
        async fn claim_moderation(
            &mut self,
            _now: OffsetDateTime,
            _lease: Duration,
            _limit: u32,
        ) -> Result<Vec<ModerationJobRow>, StoreError> {
            Err(StoreError::Unavailable("test"))
        }
        async fn reclaim_moderation_expired(
            &mut self,
            _now: OffsetDateTime,
            _max_attempts: u32,
            _limit: u32,
        ) -> Result<u32, StoreError> {
            Err(StoreError::Unavailable("test"))
        }
        async fn complete_moderation(
            &mut self,
            _job: crate::model::JobId,
            _token: Uuid,
            _error: Option<&str>,
            _available_at: OffsetDateTime,
            _terminal: bool,
        ) -> Result<bool, StoreError> {
            Err(StoreError::Unavailable("test"))
        }
    }

    #[tokio::test]
    async fn minimal_tx_surface_reports_typed_results_and_attachment_invariants() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let job = crate::model::JobId(Uuid::new_v4());
        let market = MarketId(Uuid::new_v4());
        let mut tx = DefaultOnlyTx {
            complete_ready: false,
            attach_ready: false,
        };
        assert_eq!(
            tx.enqueue(market, None, ArtifactKind::Poster, now).await,
            Err(StoreError::Unavailable("test"))
        );
        assert_eq!(
            tx.claim(now, Duration::seconds(1), 1).await,
            Err(StoreError::Unavailable("test"))
        );
        assert!(!crate::video::attach_ready::complete_and_attach(
            &mut tx,
            job,
            Uuid::new_v4(),
            "/assets/stale.svg",
            now,
        )
        .await
        .unwrap());
        assert_eq!(
            tx.complete_error(job, Uuid::new_v4(), "error", now, false)
                .await,
            Err(StoreError::Unavailable("test"))
        );
        assert_eq!(
            tx.reclaim_expired(now, 3, 1).await,
            Err(StoreError::Unavailable("test"))
        );
        assert_eq!(
            tx.reclaim_moderation_expired(now, 3, 1).await,
            Err(StoreError::Unavailable("test"))
        );
        assert!(matches!(
            tx.job(job).await,
            Err(StoreError::Unavailable("phase5:video-read"))
        ));
        assert!(matches!(
            tx.market_row(market).await,
            Err(StoreError::Unavailable("phase5:video-read"))
        ));
        assert!(matches!(
            tx.realizations(UserId(Uuid::new_v4()), MarketId(Uuid::new_v4()))
                .await,
            Err(StoreError::Unavailable("phase5:video-read"))
        ));
        assert!(matches!(
            tx.user_handle(UserId(Uuid::new_v4())).await,
            Err(StoreError::Unavailable("phase5:video-read"))
        ));
        assert_eq!(
            tx.lock_moderation_cursor().await,
            Err(StoreError::Unavailable("test"))
        );
        assert_eq!(
            tx.moderation_events_after(0, 1).await,
            Err(StoreError::Unavailable("test"))
        );
        assert_eq!(
            tx.materialize_moderation_jobs(&[], now).await,
            Err(StoreError::Unavailable("test"))
        );
        assert_eq!(
            tx.save_moderation_cursor(1).await,
            Err(StoreError::Unavailable("test"))
        );
        assert_eq!(
            tx.claim_moderation(now, Duration::seconds(1), 1).await,
            Err(StoreError::Unavailable("test"))
        );
        assert_eq!(
            tx.complete_moderation(job, Uuid::new_v4(), Some("error"), now, true)
                .await,
            Err(StoreError::Unavailable("test"))
        );
        Box::new(tx).commit().await.unwrap();

        let mut inconsistent = DefaultOnlyTx {
            complete_ready: true,
            attach_ready: false,
        };
        assert_eq!(
            crate::video::attach_ready::complete_and_attach(
                &mut inconsistent,
                job,
                Uuid::new_v4(),
                "/assets/inconsistent.svg",
                now,
            )
            .await,
            Err(StoreError::Invariant("completed job did not attach"))
        );
    }
}
