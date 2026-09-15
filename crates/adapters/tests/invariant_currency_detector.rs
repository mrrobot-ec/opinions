//! Detector completeness: the invariant sweep must not be weaker than the
//! database trigger it backstops.
//!
//! `migrations/0002_ledger_triggers.sql` installs `ledger_entries_balanced`,
//! which groups by `la.currency` and refuses any transaction that is non-zero
//! in ANY currency. The sweep's identities 1 and 3 used to be currency-BLIND —
//! `group by txn_id` and an aggregate `-external` vs `internal` — so a
//! transaction that nets to zero overall while being `+n` in `usdc` and `-n` in
//! `usdc_credit` was invisible to both.
//!
//! **Chronology, stated so this is not read as more than it is.** That gap is
//! not reachable through any writer: the trigger refuses the commit, and
//! `PgTx::ledger_apply` validates per currency in Rust before it even tries. I
//! reported it as REFUTED-as-reachable and CONFIRMED-as-a-defence-in-depth gap;
//! the lead ruled that the locked Phase-6 authority defines the identities per
//! currency, making it a contract question rather than a reachability one. So
//! this suite is adapter parity evidence for an already-adjudicated finding —
//! not a claim that I observed a chronological red through a normal code path.
//!
//! The only way to exercise a backstop is to create the state the front line
//! prevents, so these tests plant the drift with
//! `session_replication_role = replica`, which suppresses triggers — the same
//! technique `w2_ops_contract.rs` already uses to plant a rogue unbalanced
//! transaction. Planting it is the point: if the trigger is ever dropped, a
//! backup restored without it, or a privileged operator writes directly, the
//! sweep is the last line, and a last line that cannot see the drift is not
//! one.

use adapters::pg::PgStore;
use application::ports::Store;
use domain::ledger::{Currency, OwnerType};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::str::FromStr;
use uuid::Uuid;

mod common;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;
type Fallible<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

struct Scratch {
    store: PgStore,
    pool: sqlx::PgPool,
    control: sqlx::PgPool,
    database: String,
}

async fn scratch(label: &str) -> Fallible<Scratch> {
    let url = common::database_url();
    let control = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await?;
    let database = format!("opinions_currency_{label}_{}", Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("create database {database}")))
        .execute(&control)
        .await?;
    let options = PgConnectOptions::from_str(&url)?.database(&database);
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await?;
    MIGRATOR.run(&pool).await?;
    Ok(Scratch {
        store: PgStore::from_pool(pool.clone()),
        pool,
        control,
        database,
    })
}

async fn teardown(scratch: Scratch) -> TestResult {
    scratch.pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "drop database {} with (force)",
        scratch.database
    )))
    .execute(&scratch.control)
    .await?;
    scratch.control.close().await;
    Ok(())
}

async fn account(pool: &sqlx::PgPool, owner: &str, currency: &str) -> Fallible<Uuid> {
    let owner_id = if owner == "user" {
        let user = Uuid::new_v4();
        sqlx::query("insert into users (id, handle) values ($1, $2)")
            .bind(user)
            .bind(format!("cur-{user}"))
            .execute(pool)
            .await?;
        Some(user)
    } else {
        None
    };
    Ok(sqlx::query_scalar(
        "insert into ledger_accounts (owner_type, owner_id, currency) values ($1, $2, $3)
         returning id",
    )
    .bind(owner)
    .bind(owner_id)
    .bind(currency)
    .fetch_one(pool)
    .await?)
}

/// Plant entries with triggers suppressed. This is how a backstop is tested:
/// the state exists precisely because the front line failed.
async fn plant(scratch: &Scratch, entries: &[(Uuid, i64)]) -> Fallible<Uuid> {
    let txn = Uuid::new_v4();
    let mut connection = scratch.pool.acquire().await?;
    sqlx::query("set session_replication_role = replica")
        .execute(&mut *connection)
        .await?;
    sqlx::query(
        "insert into ledger_transactions (id, kind, idempotency_key) values ($1,'trade',$2)",
    )
    .bind(txn)
    .bind(format!("planted-{txn}"))
    .execute(&mut *connection)
    .await?;
    for (account, amount) in entries {
        sqlx::query(
            "insert into ledger_entries (txn_id, account_id, amount_micro) values ($1,$2,$3)",
        )
        .bind(txn)
        .bind(account)
        .bind(amount)
        .execute(&mut *connection)
        .await?;
    }
    sqlx::query("set session_replication_role = origin")
        .execute(&mut *connection)
        .await?;
    drop(connection);
    Ok(txn)
}

/// Identity 1. A transaction whose entries sum to zero OVERALL but not within
/// each currency is unbalanced, and `group by txn_id` alone reports it clean.
#[tokio::test]
async fn identity_one_catches_a_transaction_balanced_only_in_aggregate() -> TestResult {
    let scratch = scratch("txn").await?;
    let usdc = account(&scratch.pool, "user", "usdc").await?;
    let credit = account(&scratch.pool, "user", "usdc_credit").await?;

    // +7 usdc and -7 usdc_credit: overall zero, per-currency broken.
    let planted = plant(&scratch, &[(usdc, 7), (credit, -7)]).await?;
    // Control: `sum(amount_micro) group by txn_id` — the pre-fix predicate —
    // sees nothing, which is exactly why the sweep needed the currency.
    let aggregate: i64 = sqlx::query_scalar(
        "select coalesce(sum(amount_micro), 0)::bigint from ledger_entries where txn_id = $1",
    )
    .bind(planted)
    .fetch_one(&scratch.pool)
    .await?;
    assert_eq!(
        aggregate, 0,
        "the planted transaction is invisible to a currency-blind sum"
    );

    let mut snapshot = scratch.store.invariant_read_tx().await?;
    let unbalanced = snapshot.unbalanced_txns().await?;
    drop(snapshot);
    assert!(
        unbalanced.iter().any(|row| row.txn == planted),
        "identity 1 must report a transaction unbalanced within a currency"
    );

    teardown(scratch).await
}

/// Identity 3. External must mirror internal WITHIN each currency; equal and
/// opposite drift across two currencies cancels in the aggregate.
#[tokio::test]
async fn identity_three_sees_equal_and_opposite_drift_across_currencies() -> TestResult {
    let scratch = scratch("mirror").await?;
    let external_usdc = account(&scratch.pool, "external", "usdc").await?;
    let user_usdc = account(&scratch.pool, "user", "usdc").await?;
    let external_credit = account(&scratch.pool, "external", "usdc_credit").await?;

    // usdc: external -100 / user +100 is correct mirroring.
    plant(&scratch, &[(external_usdc, -100), (user_usdc, 100)]).await?;
    // Now break BOTH currencies so that the AGGREGATE still cancels:
    //   usdc        -external = 100, internal = 105  -> drifted by +5
    //   usdc_credit -external =   5, internal =   0  -> drifted by -5
    //   whole ledger: -(-100 + -5) = 105 == 105 + 0  -> looks perfect
    // That is the blind spot: two real breaks that hide each other.
    plant(&scratch, &[(user_usdc, 5)]).await?;
    plant(&scratch, &[(external_credit, -5)]).await?;

    let mut snapshot = scratch.store.invariant_read_tx().await?;
    let balances = snapshot.account_balances().await?;
    drop(snapshot);

    // The aggregate the old identity computed: it nets out, so the sweep passed.
    let external: i128 = balances
        .iter()
        .filter(|row| row.owner_type == OwnerType::External)
        .map(|row| row.balance_micro)
        .sum();
    let internal: i128 = balances
        .iter()
        .filter(|row| row.owner_type != OwnerType::External)
        .map(|row| row.balance_micro)
        .sum();
    assert_eq!(
        -external, internal,
        "the planted drift is invisible in aggregate — this is the blind spot"
    );

    // Per currency, both are broken, and the row type now carries the currency
    // that makes that visible.
    for currency in [Currency::Usdc, Currency::UsdcCredit] {
        let external: i128 = balances
            .iter()
            .filter(|row| row.currency == currency && row.owner_type == OwnerType::External)
            .map(|row| row.balance_micro)
            .sum();
        let internal: i128 = balances
            .iter()
            .filter(|row| row.currency == currency && row.owner_type != OwnerType::External)
            .map(|row| row.balance_micro)
            .sum();
        assert_ne!(
            -external, internal,
            "{currency:?} is drifted and must not be reported as mirroring"
        );
    }

    teardown(scratch).await
}

/// The trigger is still the front line. Without `session_replication_role` the
/// same write is refused, so the planting above is not evidence that ordinary
/// code can produce this state — it cannot.
#[tokio::test]
async fn the_ledger_trigger_still_refuses_the_state_these_tests_plant() -> TestResult {
    let scratch = scratch("trigger").await?;
    let usdc = account(&scratch.pool, "user", "usdc").await?;
    let credit = account(&scratch.pool, "user", "usdc_credit").await?;

    let txn = Uuid::new_v4();
    let mut connection = scratch.pool.acquire().await?;
    sqlx::query("begin").execute(&mut *connection).await?;
    sqlx::query(
        "insert into ledger_transactions (id, kind, idempotency_key) values ($1,'trade',$2)",
    )
    .bind(txn)
    .bind(format!("refused-{txn}"))
    .execute(&mut *connection)
    .await?;
    for (account, amount) in [(usdc, 7_i64), (credit, -7_i64)] {
        sqlx::query(
            "insert into ledger_entries (txn_id, account_id, amount_micro) values ($1,$2,$3)",
        )
        .bind(txn)
        .bind(account)
        .bind(amount)
        .execute(&mut *connection)
        .await?;
    }
    // The constraint trigger is DEFERRABLE INITIALLY DEFERRED, so it fires here.
    let committed = sqlx::query("commit").execute(&mut *connection).await;
    assert!(
        committed.is_err(),
        "ledger_entries_balanced must refuse a per-currency-unbalanced commit"
    );
    drop(connection);

    let rows: i64 = sqlx::query_scalar("select count(*) from ledger_entries where txn_id = $1")
        .bind(txn)
        .fetch_one(&scratch.pool)
        .await?;
    assert_eq!(rows, 0, "the refused transaction left nothing behind");

    teardown(scratch).await
}
