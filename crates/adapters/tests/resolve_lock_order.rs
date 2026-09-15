//! Regression: class-2 user locks must be ONE globally ascending run.
//!
//! `docs/plans/phase3-economy-integrity.md:13` pins the global order as
//! "`serialize_key` (advisory class 1) → **user locks in canonical UUID
//! ascending order** (class 2) → `market_for_update` → row locks" and calls
//! deadlock-freedom "by construction". The construction only holds if a
//! transaction acquires its class-2 locks in a *single* ascending run.
//!
//! `ResolveMarket` briefly took them in two independently sorted runs — the
//! voter set through `reps_for_update`, then the referral set through
//! `lock_user`. Two ascending runs concatenated are not one ascending run, and
//! because referral-relevant users are not a subset of voters (a referrer need
//! never have voted), two markets resolving concurrently could invert on a
//! shared pair. Observed against real `PostgreSQL`:
//!
//! ```text
//! ERROR:  deadlock detected
//! DETAIL:  Process A waits for ExclusiveLock on advisory lock [.,2,.,2];
//!          blocked by process B.
//!          Process B waits for ExclusiveLock on advisory lock [.,2,.,2];
//!          blocked by process A.
//! ```
//!
//! One resolution aborts with SQLSTATE 40P01. This suite pins both directions:
//! two runs deadlock, one merged ascending run does not. It drives the real
//! adapter roles (`SettlementIo::reps_for_update` and `UserLockGuard::lock_user`
//! on `PgTx`), so it fails if either implementation stops ordering.

use std::str::FromStr;
use std::sync::Arc;

use adapters::pg::PgStore;
use application::model::UserId;
use application::ports::Store;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use uuid::Uuid;

mod common;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

struct Scratch {
    store: PgStore,
    control: sqlx::PgPool,
    database: String,
    /// Two users with reputation rows, ordered by UUID: `(low, high)`.
    low: UserId,
    high: UserId,
}

async fn scratch(label: &str) -> Result<Scratch, Box<dyn std::error::Error + Send + Sync>> {
    let url = common::database_url();
    let control = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await?;
    let database = format!("opinions_lock_order_{label}_{}", Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("create database {database}")))
        .execute(&control)
        .await?;
    let options = PgConnectOptions::from_str(&url)?.database(&database);
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await?;
    MIGRATOR.run(&pool).await?;

    // Two users whose UUID order is known, each with the reputation row
    // `reps_for_update` requires.
    let mut ids = [Uuid::new_v4(), Uuid::new_v4()];
    ids.sort_unstable();
    for id in ids {
        sqlx::query("insert into users (id, handle) values ($1, $2)")
            .bind(id)
            .bind(format!("lock-{id}"))
            .execute(&pool)
            .await?;
        sqlx::query("insert into reputation (user_id, rep_micro, tier) values ($1, 0, 0)")
            .bind(id)
            .execute(&pool)
            .await?;
    }
    Ok(Scratch {
        store: PgStore::from_pool(pool),
        control,
        database,
        low: UserId(ids[0]),
        high: UserId(ids[1]),
    })
}

async fn teardown(scratch: Scratch) -> TestResult {
    scratch.store.pool_handle().close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "drop database {} with (force)",
        scratch.database
    )))
    .execute(&scratch.control)
    .await?;
    scratch.control.close().await;
    Ok(())
}

/// Two class-2 runs, exactly as `ResolveMarket` took them: the sorted voter set
/// through `reps_for_update`, then a referral user through `lock_user`.
async fn two_runs(
    store: &PgStore,
    first: UserId,
    second: UserId,
    barrier: &tokio::sync::Barrier,
) -> Result<(), application::error::StoreError> {
    let mut tx = store.resolve_tx().await?;
    tx.reps_for_update(&[first]).await?;
    // Both transactions now hold exactly one lock; only then does either ask
    // for the other's. Without this rendezvous the interleaving is a race and
    // the test would be flaky rather than deterministic.
    barrier.wait().await;
    tx.lock_user(second).await?;
    tx.commit().await
}

/// One merged ascending run over the same pair.
///
/// Deliberately NO rendezvous here. A barrier placed after the run would
/// livelock the test rather than test anything: the first transaction holds
/// both locks and waits at the barrier, while the second cannot reach the
/// barrier because it is blocked acquiring the first lock. Ordered acquisition
/// is exactly what makes these two serialise instead of interleaving, so the
/// only honest assertion is that both complete.
async fn one_run(
    store: &PgStore,
    users_sorted: [UserId; 2],
) -> Result<(), application::error::StoreError> {
    let mut tx = store.resolve_tx().await?;
    tx.reps_for_update(&users_sorted).await?;
    tx.commit().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_separately_sorted_class_2_runs_deadlock() -> TestResult {
    let scratch = scratch("two_runs").await?;
    let barrier = Arc::new(tokio::sync::Barrier::new(2));

    // Mirrors two markets resolving at once: M1's voter run holds `high` and
    // its referral run wants `low`; M2's holds `low` and wants `high`.
    let (a, b) = tokio::join!(
        two_runs(&scratch.store, scratch.high, scratch.low, &barrier),
        two_runs(&scratch.store, scratch.low, scratch.high, &barrier),
    );

    let aborted = match (a, b) {
        (Err(error), Ok(())) | (Ok(()), Err(error)) => error,
        (Err(first), Err(second)) => {
            return Err(format!(
                "both transactions failed; the detector aborts exactly one: {first:?} / {second:?}"
            )
            .into())
        }
        (Ok(()), Ok(())) => {
            return Err("two separately sorted class-2 runs must deadlock; both committed".into())
        }
    };
    let message = aborted.to_string().to_ascii_lowercase();
    assert!(
        message.contains("deadlock"),
        "the abort must be PostgreSQL's deadlock detector (SQLSTATE 40P01), got: {message}"
    );

    teardown(scratch).await
}

/// The positive control, and the shape the fix must keep: merging both sets
/// into ONE ascending run removes the cycle entirely. Without this the suite
/// would only prove that Postgres can deadlock, not that ordering fixes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_merged_ascending_class_2_run_never_deadlocks() -> TestResult {
    let scratch = scratch("one_run").await?;
    let sorted = [scratch.low, scratch.high];

    let (a, b) = tokio::join!(
        one_run(&scratch.store, sorted),
        one_run(&scratch.store, sorted),
    );

    assert!(
        a.is_ok() && b.is_ok(),
        "one globally ascending run serialises without a cycle, got {a:?} / {b:?}"
    );

    teardown(scratch).await
}
