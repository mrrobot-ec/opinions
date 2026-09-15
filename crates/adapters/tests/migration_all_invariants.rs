//! Schema invariants that must hold after the FULL migration chain.
//!
//! Sibling `migration_0011_deposit_identity.rs` pins history: what 0011 alone
//! produces, including that it ships `deposits_observation_identity` `NOT
//! VALID`. This suite pins the present: once every migration has run, the
//! schema must carry no unvalidated constraint and must make the D35 dedup
//! rule structural rather than advisory.
//!
//! Both matter and neither replaces the other — the historical test proves the
//! grandfathering was safe at the moment it ran, this one proves the escape
//! hatches were closed afterwards.

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::str::FromStr;
use uuid::Uuid;

mod common;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;
type Fallible<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

struct Migrated {
    pool: sqlx::PgPool,
    control: sqlx::PgPool,
    database: String,
}

async fn fully_migrated(label: &str) -> Fallible<Migrated> {
    let url = common::database_url();
    let control = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await?;
    let database = format!(
        "opinions_all_migrations_{label}_{}",
        Uuid::new_v4().simple()
    );
    sqlx::query(sqlx::AssertSqlSafe(format!("create database {database}")))
        .execute(&control)
        .await?;
    let options = PgConnectOptions::from_str(&url)?.database(&database);
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await?;
    MIGRATOR.run(&pool).await?;
    Ok(Migrated {
        pool,
        control,
        database,
    })
}

async fn teardown(migrated: Migrated) -> TestResult {
    migrated.pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "drop database {} with (force)",
        migrated.database
    )))
    .execute(&migrated.control)
    .await?;
    migrated.control.close().await;
    Ok(())
}

/// A `NOT VALID` constraint is enforced on every INSERT and UPDATE, but it is
/// not trusted for the rows already there, and `pg_constraint.convalidated`
/// is the only durable record of that distinction. Leaving one unvalidated
/// forever means nobody ever proved the existing rows satisfy it. 0011 shipped
/// `deposits_observation_identity` `NOT VALID` to protect rows it had just
/// grandfathered; a later migration must close that out.
#[tokio::test]
async fn the_full_migration_chain_leaves_no_unvalidated_constraint() -> TestResult {
    let migrated = fully_migrated("convalidated").await?;

    let unvalidated: Vec<(String, String)> = sqlx::query_as(
        "select conrelid::regclass::text, conname
           from pg_constraint
          where connamespace = 'public'::regnamespace and not convalidated
          order by conname",
    )
    .fetch_all(&migrated.pool)
    .await?;
    assert!(
        unvalidated.is_empty(),
        "every constraint must be validated once the chain has run; unvalidated: {unvalidated:?}"
    );

    // Named explicitly so the assertion above cannot pass by the constraint
    // having been dropped instead of validated.
    let convalidated: Option<bool> = sqlx::query_scalar(
        "select convalidated from pg_constraint where conname = 'deposits_observation_identity'",
    )
    .fetch_optional(&migrated.pool)
    .await?;
    assert_eq!(
        convalidated,
        Some(true),
        "deposits_observation_identity must still exist AND be validated"
    );

    teardown(migrated).await
}

/// D35 makes "dedup within an open incident" normative. `raise` cannot deliver
/// that with a read followed by an insert — two concurrent callers both read
/// nothing. The rule has to be structural, and it has to be PARTIAL: a
/// resolved episode is deliberately kept as its own row, so a plain
/// `unique (incident_key)` would forbid the recurrence the spec requires.
#[tokio::test]
async fn one_open_incident_per_key_is_enforced_by_a_partial_unique_index() -> TestResult {
    let migrated = fully_migrated("alertdedup").await?;

    let key = "invariant_breach:suite:ep-1";
    let insert = |id: Uuid, status: &'static str| {
        sqlx::query(
            "insert into alert_outbox (id, incident_key, severity, body, status)
             values ($1, $2, 'crit', 'drift', $3)",
        )
        .bind(id)
        .bind(key)
        .bind(status)
    };

    insert(Uuid::new_v4(), "open")
        .execute(&migrated.pool)
        .await?;
    // A second OPEN incident for the same key is the duplicate-page race.
    assert!(
        insert(Uuid::new_v4(), "open")
            .execute(&migrated.pool)
            .await
            .is_err(),
        "a second open incident for one key must be refused by the database"
    );
    // `acked` still dedups a recurrence, so it is inside the index predicate.
    assert!(
        insert(Uuid::new_v4(), "acked")
            .execute(&migrated.pool)
            .await
            .is_err(),
        "an acked incident still holds the key: a recurrence must not open a second one"
    );

    // Once resolved, the key is free again — the recurrence-after-recovery rule
    // D35 requires, and the reason the index must be partial.
    sqlx::query(
        "update alert_outbox set status = 'resolved', resolved_at = now() where incident_key = $1",
    )
    .bind(key)
    .execute(&migrated.pool)
    .await?;
    insert(Uuid::new_v4(), "open")
        .execute(&migrated.pool)
        .await?;
    let rows: i64 = sqlx::query_scalar("select count(*) from alert_outbox where incident_key = $1")
        .bind(key)
        .fetch_one(&migrated.pool)
        .await?;
    assert_eq!(
        rows, 2,
        "the resolved episode is kept and the recurrence is its own row"
    );
    // ...and many resolved episodes may coexist, so the index cannot cover them.
    sqlx::query("update alert_outbox set status = 'resolved' where incident_key = $1")
        .bind(key)
        .execute(&migrated.pool)
        .await?;
    insert(Uuid::new_v4(), "resolved")
        .execute(&migrated.pool)
        .await?;

    teardown(migrated).await
}
