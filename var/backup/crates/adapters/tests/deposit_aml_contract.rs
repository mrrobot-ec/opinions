//! D34 deposit-side AML, against real `PostgreSQL`.
//!
//! Withdrawals have had a pinned AML evaluation since W1; deposit admission did
//! not, so money could be admitted without the structuring/velocity check that
//! the symmetric path applies. `DepositAmlIo::evaluate_deposit_aml_candidate`
//! closes that, and this suite pins the three things that are easy to get
//! subtly wrong on the DEPOSIT side specifically:
//!
//!   1. **No double count.** Unlike a withdrawal, the `deposits` row already
//!      exists when admission runs — observation inserted it. If the derived
//!      leg set includes the candidate's own row, every deposit counts itself
//!      as both history and candidate and is pushed one step further into the
//!      structuring band, so the Nth deposit flags when only N-1 legs exist.
//!   2. **The band's floor is load-bearing** (codex-p7r4 B1): structuring
//!      counts only legs in `[floor, threshold)`, so $25 onramp dust must never
//!      accumulate into a flag no matter how many legs there are.
//!   3. **Replay is idempotent, and a disagreeing candidate is a conflict.**
//!      Admission is retried; re-evaluating must not raise a second flag, and a
//!      call whose arguments contradict the persisted row must be typed, not
//!      silently evaluated as a different candidate for the same deposit.
//!
//! Policy comes from `config_entries`, seeded by 0011 as floor $100, threshold
//! $500, n = 4, window 24h.

use adapters::pg::PgStore;
use application::error::StoreError;
use application::model::{DepositId, UserId};
use application::ports::Store;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::str::FromStr;
use uuid::Uuid;

mod common;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;
type Fallible<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// Seeded by 0011: structuring counts legs in `[1e8, 5e8)`, four of them flag.
const IN_BAND_MICRO: i64 = 200_000_000; // $200
const BELOW_FLOOR_MICRO: i64 = 25_000_000; // $25 onramp dust
const SOURCE: &str = "SoUrCeAddr111";

struct Scratch {
    store: PgStore,
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
    let database = format!("opinions_deposit_aml_{label}_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("create database {database}"))
        .execute(&control)
        .await?;
    let options = PgConnectOptions::from_str(&url)?.database(&database);
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await?;
    MIGRATOR.run(&pool).await?;
    let user = Uuid::new_v4();
    sqlx::query("insert into users (id, handle) values ($1, $2)")
        .bind(user)
        .bind(format!("aml-{user}"))
        .execute(&pool)
        .await?;
    Ok(Scratch {
        store: PgStore::from_pool(pool.clone()),
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

/// One observed deposit with the full chain identity `0011`'s
/// `deposits_observation_identity` requires of any machine-status row.
async fn observed(scratch: &Scratch, amount_micro: i64) -> Fallible<DepositId> {
    let id = Uuid::new_v4();
    sqlx::query(
        "insert into deposits
            (id, user_id, chain_sig, amount_micro, status, machine_status,
             source_address, dest_address, mint, observed_slot, rail_fingerprint)
         values ($1, $2, $3, $4, 'observed_finalized', 'observed_finalized',
                 $5, 'treasury', 'usdc', 7, 'rail:test')",
    )
    .bind(id)
    .bind(scratch.user.0)
    .bind(format!("sig-{id}"))
    .bind(amount_micro)
    .bind(SOURCE)
    .execute(&scratch.pool)
    .await?;
    Ok(DepositId(id))
}

/// Evaluate one candidate in its own admission transaction and commit, the way
/// `credit_deposit` does.
async fn evaluate(
    scratch: &Scratch,
    deposit: DepositId,
    amount_micro: i64,
) -> Result<bool, StoreError> {
    let mut tx = scratch.store.deposit_admission_tx().await?;
    let held = tx
        .evaluate_deposit_aml_candidate(
            deposit,
            scratch.user,
            SOURCE,
            amount_micro,
            time::OffsetDateTime::now_utc(),
        )
        .await?;
    tx.commit().await?;
    Ok(held)
}

async fn open_flags(pool: &sqlx::PgPool, user: Uuid) -> Fallible<i64> {
    Ok(
        sqlx::query_scalar("select count(*) from aml_flags where user_id = $1 and status = 'open'")
            .bind(user)
            .fetch_one(pool)
            .await?,
    )
}

#[tokio::test]
async fn the_fourth_in_band_deposit_flags_and_holds_admission() -> TestResult {
    let scratch = scratch("band").await?;

    // Three prior in-band legs. Each is evaluated as it arrives, exactly as
    // admission would, so the run also proves the first three do NOT flag.
    for index in 0..3 {
        let deposit = observed(&scratch, IN_BAND_MICRO).await?;
        let held = evaluate(&scratch, deposit, IN_BAND_MICRO).await?;
        assert!(
            !held,
            "leg {index} of 3 is below the structuring n; nothing may be held yet"
        );
        assert_eq!(open_flags(&scratch.pool, scratch.user.0).await?, 0);
    }

    // The fourth completes the band. THIS is the double-count guard: if the
    // candidate's own row were counted as history too, the third call above
    // would already have flagged.
    let fourth = observed(&scratch, IN_BAND_MICRO).await?;
    assert!(
        evaluate(&scratch, fourth, IN_BAND_MICRO).await?,
        "the fourth in-band leg must raise a flag and hold admission"
    );
    assert_eq!(
        open_flags(&scratch.pool, scratch.user.0).await?,
        1,
        "one open flag per rule, not one per evaluation"
    );

    // Replay: admission is retried, and re-evaluating the SAME deposit must not
    // raise a second flag or count the leg twice.
    assert!(
        evaluate(&scratch, fourth, IN_BAND_MICRO).await?,
        "a replay still reports the open flag, so admission stays held"
    );
    assert_eq!(
        open_flags(&scratch.pool, scratch.user.0).await?,
        1,
        "a replayed evaluation must not duplicate the flag"
    );

    teardown(scratch).await
}

#[tokio::test]
async fn dust_below_the_structuring_floor_never_accumulates_into_a_flag() -> TestResult {
    let scratch = scratch("floor").await?;

    // Six $25 legs: well past n = 4, and all below the $100 floor. The floor is
    // what makes the band executable (codex-p7r4 B1) — without it ordinary
    // onramp dust would flag every small-balance user.
    for index in 0..6 {
        let deposit = observed(&scratch, BELOW_FLOOR_MICRO).await?;
        assert!(
            !evaluate(&scratch, deposit, BELOW_FLOOR_MICRO).await?,
            "dust leg {index} must not flag"
        );
    }
    assert_eq!(
        open_flags(&scratch.pool, scratch.user.0).await?,
        0,
        "below-floor legs are outside the structuring band entirely"
    );

    teardown(scratch).await
}

#[tokio::test]
async fn a_candidate_that_disagrees_with_its_persisted_row_is_a_typed_conflict() -> TestResult {
    let scratch = scratch("binding").await?;
    let deposit = observed(&scratch, IN_BAND_MICRO).await?;

    // The deposits row IS the durable candidate binding. A call claiming a
    // different amount for the same deposit is not a second candidate to
    // evaluate; it is a contradiction, and evaluating it would let a caller
    // choose which amount the AML band sees.
    let mut tx = scratch.store.deposit_admission_tx().await?;
    let mismatched = tx
        .evaluate_deposit_aml_candidate(
            deposit,
            scratch.user,
            SOURCE,
            IN_BAND_MICRO + 1,
            time::OffsetDateTime::now_utc(),
        )
        .await;
    assert!(
        matches!(
            mismatched,
            Err(StoreError::Conflict("deposit aml candidate"))
        ),
        "a disagreeing amount must be a typed conflict, got {mismatched:?}"
    );
    drop(tx);

    // A deposit that does not exist at all is NotFound, never a silent pass.
    let mut tx = scratch.store.deposit_admission_tx().await?;
    let missing = tx
        .evaluate_deposit_aml_candidate(
            DepositId(Uuid::new_v4()),
            scratch.user,
            SOURCE,
            IN_BAND_MICRO,
            time::OffsetDateTime::now_utc(),
        )
        .await;
    assert!(
        matches!(missing, Err(StoreError::NotFound("deposit"))),
        "an unknown deposit must be NotFound, got {missing:?}"
    );
    drop(tx);

    teardown(scratch).await
}
