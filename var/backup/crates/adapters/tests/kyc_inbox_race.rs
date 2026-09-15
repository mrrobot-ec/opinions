//! Real-Postgres races on the atomic KYC inbox handler, made deterministic.
//!
//! `accept_and_apply_inboxed_kyc` reads the inbox row, decides the algebra,
//! then takes the user lock and inserts. The read happens BEFORE any lock that
//! covers the inbox key, and the transaction is READ COMMITTED, so two
//! simultaneous deliveries for one `(provider, event_id)` can both observe
//! "absent" and both decide `Accepted`. The loser then reaches `inbox_insert`,
//! whose deterministic `inbox_subject_id(provider, event_id)` primary key
//! collides, and `conflict_or` surfaces a raw
//! `StoreError::Conflict("inbox event")`.
//!
//! ## Why these tests hold a table lock
//!
//! Simply firing two futures at once does NOT reliably reproduce this. Measured
//! on this database, the divergent-payload pair reproduced roughly one run in
//! five: four runs in five the two transactions never overlapped the
//! `inbox_get` → `inbox_insert` window and the suite passed over a live bug.
//! Repeating the pair N times only trades a large false-green probability for a
//! smaller one; it never removes it, and a concurrency gate that is right most
//! of the time is worse than none, because its green gets attributed to
//! whatever landed last.
//!
//! So the overlap is forced rather than hoped for. A control transaction holds
//! `LOCK TABLE compliance_decisions IN SHARE MODE`, which conflicts with the
//! ROW EXCLUSIVE an INSERT needs while leaving `SELECT` (ACCESS SHARE), row
//! locks on `users`, and advisory locks free. Both deliveries therefore get
//! past their reads and park at the write. The test then waits until Postgres
//! itself reports two lock-waiting backends — read from `pg_stat_activity`,
//! never guessed from a sleep — and only then releases the blocker.
//!
//! That barrier is correct both before and after the fix: today both deliveries
//! wait on the INSERT; once the inbox is serialized by an advisory lock taken
//! before the read, one waits on the INSERT and the other on the advisory lock.
//! Either way there are two waiters, so the wait condition does not itself
//! encode the defect.

use adapters::pg::{PgComplianceStore, PgStore};
use application::error::AppError;
use application::model::UserId;
use application::money::admin::{payload_hash, InboxOutcome, InboxRecord};
use application::money::kyc::accept_and_apply_inboxed_kyc;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

mod common;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;
type Fallible<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Delivered = Result<InboxOutcome, AppError>;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// Fixed instant: these suites assert lifecycle, never elapsed time.
fn at() -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(1_700_000_000)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
}

struct Scratch {
    store: Arc<PgComplianceStore>,
    pool: sqlx::PgPool,
    control: sqlx::PgPool,
    database: String,
    user: UserId,
}

async fn scratch(label: &str) -> Fallible<Scratch> {
    let url = common::database_url();
    let control = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await?;
    let database = format!("opinions_kyc_race_{label}_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("create database {database}"))
        .execute(&control)
        .await?;
    let options = PgConnectOptions::from_str(&url)?.database(&database);
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await?;
    MIGRATOR.run(&pool).await?;
    let user = Uuid::new_v4();
    sqlx::query("insert into users (id, handle) values ($1, $2)")
        .bind(user)
        .bind(format!("kyc-{user}"))
        .execute(&pool)
        .await?;
    let store = Arc::new(PgComplianceStore::from_store(&PgStore::from_pool(
        pool.clone(),
    )));
    Ok(Scratch {
        store,
        pool,
        control,
        database,
        user: UserId(user),
    })
}

async fn teardown(scratch: Scratch) -> TestResult {
    scratch.pool.close().await;
    sqlx::query(&format!("drop database {} with (force)", scratch.database))
        .execute(&scratch.control)
        .await?;
    scratch.control.close().await;
    Ok(())
}

fn record(user: UserId, event_id: &str, tier: i32) -> InboxRecord {
    let body = format!(r#"{{"event_id":"{event_id}","to_tier":{tier}}}"#);
    InboxRecord {
        provider: "persona".into(),
        event_id: event_id.to_string(),
        payload_hash: payload_hash(body.as_bytes()),
        payload: serde_json::json!({ "event_id": event_id, "to_tier": tier }),
        user_id: Some(user),
        received_at: at(),
    }
}

fn deliver(
    store: &Arc<PgComplianceStore>,
    record: InboxRecord,
    to_tier: i32,
    provider_ref: &str,
) -> tokio::task::JoinHandle<Delivered> {
    let store = Arc::clone(store);
    let provider_ref = provider_ref.to_string();
    tokio::spawn(async move {
        accept_and_apply_inboxed_kyc(
            &*store,
            record,
            to_tier,
            Some(provider_ref),
            None,
            "7".to_string(),
            at(),
        )
        .await
    })
}

/// Take the barrier: blocks INSERTs on `compliance_decisions`, leaves reads,
/// row locks and advisory locks alone.
async fn barrier(pool: &sqlx::PgPool) -> Fallible<sqlx::pool::PoolConnection<sqlx::Postgres>> {
    let mut connection = pool.acquire().await?;
    sqlx::query("begin").execute(&mut *connection).await?;
    sqlx::query("lock table compliance_decisions in share mode")
        .execute(&mut *connection)
        .await?;
    Ok(connection)
}

/// Run both deliveries with their writes forced to overlap, then release.
///
/// Fails loudly rather than degrading into a plain race if the two backends
/// never park: a suite that cannot establish its own precondition must say so,
/// not report a green it did not earn.
async fn with_forced_overlap(
    scratch: &Scratch,
    first: tokio::task::JoinHandle<Delivered>,
    second: tokio::task::JoinHandle<Delivered>,
    mut blocker: sqlx::pool::PoolConnection<sqlx::Postgres>,
) -> Fallible<(Delivered, Delivered)> {
    let mut waited_ms = 0_u64;
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "select count(*) from pg_stat_activity
              where datname = current_database()
                and wait_event_type = 'Lock'
                and pid <> pg_backend_pid()",
        )
        .fetch_one(&scratch.pool)
        .await?;
        if waiting >= 2 {
            break;
        }
        if waited_ms > 10_000 {
            return Err(format!(
                "both deliveries were expected to park on a lock so their writes overlap; \
                 only {waiting} backend(s) ever waited. Without that overlap this suite is a \
                 coin flip, so it fails rather than reporting a meaningless green."
            )
            .into());
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        waited_ms += 20;
    }
    sqlx::query("rollback").execute(&mut *blocker).await?;
    drop(blocker);
    Ok((first.await?, second.await?))
}

async fn count(pool: &sqlx::PgPool, sql: &str, user: Uuid) -> Fallible<i64> {
    Ok(sqlx::query_scalar(sql).bind(user).fetch_one(pool).await?)
}

/// Two identical deliveries — the ordinary at-least-once duplicate every
/// webhook provider sends. One must win; the other must be an idempotent
/// `Replay`, never an error the provider is asked to retry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_simultaneous_identical_deliveries_converge_to_accepted_plus_replay() -> TestResult {
    let scratch = scratch("dup").await?;
    let blocker = barrier(&scratch.pool).await?;

    let (a, b) = with_forced_overlap(
        &scratch,
        deliver(
            &scratch.store,
            record(scratch.user, "evt-race", 2),
            2,
            "sess-race",
        ),
        deliver(
            &scratch.store,
            record(scratch.user, "evt-race", 2),
            2,
            "sess-race",
        ),
        blocker,
    )
    .await?;

    let mut outcomes = Vec::new();
    for (label, result) in [("a", a), ("b", b)] {
        match result {
            Ok(outcome) => outcomes.push(outcome),
            Err(error) => {
                return Err(format!(
                    "delivery {label} failed; an identical duplicate must be a Replay, never an \
                     error the provider has to retry: {error:?}"
                )
                .into())
            }
        }
    }
    outcomes.sort_by_key(|outcome| match outcome {
        InboxOutcome::Accepted => 0,
        InboxOutcome::Replay => 1,
        InboxOutcome::Conflict => 2,
    });
    assert_eq!(
        outcomes,
        vec![InboxOutcome::Accepted, InboxOutcome::Replay],
        "exactly one delivery may win; the other is an idempotent replay"
    );

    let user = scratch.user.0;
    assert_eq!(
        count(
            &scratch.pool,
            "select count(*) from compliance_decisions
              where subject_type = 'inbox' and payload->>'user_id' = $1::text",
            user,
        )
        .await?,
        1,
        "exactly one inbox row"
    );
    assert_eq!(
        count(
            &scratch.pool,
            "select count(*) from kyc_events where user_id = $1",
            user,
        )
        .await?,
        1,
        "the effect ran exactly once; a second kyc_events row is a double-apply"
    );
    let tier: i32 = sqlx::query_scalar("select kyc_tier from users where id = $1")
        .bind(user)
        .fetch_one(&scratch.pool)
        .await?;
    assert_eq!(tier, 2, "the tier the delivery asked for was applied");

    teardown(scratch).await
}

/// The SAME `(provider, event_id)` with DIFFERENT payloads, for DIFFERENT
/// users. This is the case that refutes "the user lock serializes it": the two
/// deliveries touch different `users` rows, so nothing in the effect path
/// orders them. Only the inbox key is shared.
///
/// D33's verdict for same-key/different-hash is a typed `ProposalConflict` —
/// the outcome `inbox_algebra` computes, and the ONLY error the webhook pages
/// on. A raw `StoreError::Conflict("inbox event")` escaping from the
/// primary-key collision is not the same thing: two deliveries claiming one
/// event id with genuinely divergent payloads is exactly the
/// tampering-or-provider-bug signal an operator should be woken for, and that
/// page is skipped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_same_key_different_payloads_conflict_through_the_algebra() -> TestResult {
    let scratch = scratch("hashclash").await?;
    let other = Uuid::new_v4();
    sqlx::query("insert into users (id, handle) values ($1, $2)")
        .bind(other)
        .bind(format!("kyc-other-{other}"))
        .execute(&scratch.pool)
        .await?;
    let other = UserId(other);
    let blocker = barrier(&scratch.pool).await?;

    let (a, b) = with_forced_overlap(
        &scratch,
        deliver(
            &scratch.store,
            record(scratch.user, "evt-clash", 1),
            1,
            "sess-a",
        ),
        deliver(&scratch.store, record(other, "evt-clash", 2), 2, "sess-b"),
        blocker,
    )
    .await?;

    let loser = match (a, b) {
        (Ok(InboxOutcome::Accepted), Err(error)) | (Err(error), Ok(InboxOutcome::Accepted)) => {
            error
        }
        (first, second) => {
            return Err(format!(
                "exactly one delivery may be Accepted and the other must be refused: \
                 {first:?} / {second:?}"
            )
            .into())
        }
    };
    assert!(
        matches!(
            loser,
            AppError::ProposalConflict("inbox payload hash conflict")
        ),
        "the loser must reach the typed algebra verdict the webhook pages on, not a raw \
         primary-key collision: got {loser:?}"
    );

    let rows: i64 = sqlx::query_scalar("select count(*) from kyc_events")
        .fetch_one(&scratch.pool)
        .await?;
    assert_eq!(rows, 1, "the refused delivery must not have applied a tier");

    teardown(scratch).await
}
