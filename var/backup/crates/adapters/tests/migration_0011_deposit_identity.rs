//! 0011 deposit-machine grandfathering contract (D31 / codex-p7r3 B6).
//!
//! `deposits_observation_identity` ships `NOT VALID`, and 0011's comment
//! justifies that marker as protecting the legacy rows the same migration
//! re-labels. This suite pins what the marker actually buys against real
//! `PostgreSQL`, because `NOT VALID` is easy to misread as "not enforced":
//!
//!   1. The backfill labels every pre-0011 status exactly once
//!      (`credited` → `admitted_legacy`, `seen|confirmed` → `quarantined_legacy`).
//!   2. Despite `NOT VALID`, the CHECK is enforced on every INSERT — a
//!      machine-status row without full chain identity is refused.
//!   3. Despite `NOT VALID`, it is enforced on every UPDATE too, so a
//!      quarantined legacy row can never be promoted into the machine without
//!      the source/dest/mint/slot/fingerprint identity it never recorded.
//!      That promotion is the one path by which an unbacked liability could
//!      enter the deposit machine, and it is closed.
//!   4. The predicate itself exempts all three legacy shapes, so
//!      `VALIDATE CONSTRAINT` succeeds WITH those rows present. The marker is
//!      therefore not load-bearing: a future validating migration is safe, and
//!      dropping the marker cannot retro-fail grandfathered rows.
//!
//! If (2) or (3) ever regress, an operator "un-quarantining" a legacy row
//! silently mints a machine liability with no chain evidence behind it.

use std::str::FromStr;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use uuid::Uuid;

mod common;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;
type Fallible<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// The three pre-0011 statuses, and the label 0011 must grandfather each into.
const LEGACY_STATUSES: [(&str, &str); 3] = [
    ("credited", "admitted_legacy"),
    ("confirmed", "quarantined_legacy"),
    ("seen", "quarantined_legacy"),
];

struct Fixture {
    pool: sqlx::PgPool,
    control: sqlx::PgPool,
    database: String,
    user: Uuid,
    /// `(pre-0011 status, deposit id)`, in `LEGACY_STATUSES` order.
    legacy: Vec<(&'static str, Uuid)>,
}

async fn isolated_pre_money_database() -> Fallible<(sqlx::PgPool, sqlx::PgPool, String)> {
    let url = common::database_url();
    let control = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await?;
    let database = format!("opinions_deposit_identity_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("create database {database}"))
        .execute(&control)
        .await?;
    let options = PgConnectOptions::from_str(&url)?.database(&database);
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await?;
    for migration in [
        include_str!("../../../migrations/0001_init.sql"),
        include_str!("../../../migrations/0002_ledger_triggers.sql"),
        include_str!("../../../migrations/0003_accounts_identity.sql"),
        include_str!("../../../migrations/0004_scheduler.sql"),
        include_str!("../../../migrations/0005_economy_integrity.sql"),
        include_str!("../../../migrations/0006_social.sql"),
        include_str!("../../../migrations/0007_content.sql"),
        include_str!("../../../migrations/0008_ops.sql"),
        include_str!("../../../migrations/0009_request_fingerprints.sql"),
        include_str!("../../../migrations/0010_w2_manual_ops.sql"),
    ] {
        sqlx::raw_sql(migration).execute(&pool).await?;
    }
    Ok((pool, control, database))
}

/// Pre-0011 rows carry no chain metadata to re-derive — exactly the situation
/// the grandfathering rule was written for — then 0011 is applied over them.
async fn migrated_fixture() -> Fallible<Fixture> {
    let (pool, control, database) = isolated_pre_money_database().await?;
    let user = Uuid::new_v4();
    sqlx::query("insert into users (id, handle) values ($1, $2)")
        .bind(user)
        .bind(format!("deposit-{user}"))
        .execute(&pool)
        .await?;
    let mut legacy = Vec::new();
    for (status, _) in LEGACY_STATUSES {
        let id = Uuid::new_v4();
        sqlx::query(
            "insert into deposits (id, user_id, chain_sig, amount_micro, status)
             values ($1, $2, $3, 1000000, $4)",
        )
        .bind(id)
        .bind(user)
        .bind(format!("legacy-{status}-{id}"))
        .bind(status)
        .execute(&pool)
        .await?;
        legacy.push((status, id));
    }
    sqlx::raw_sql(include_str!("../../../migrations/0011_money.sql"))
        .execute(&pool)
        .await?;
    Ok(Fixture {
        pool,
        control,
        database,
        user,
        legacy,
    })
}

async fn teardown(fixture: Fixture) -> TestResult {
    fixture.pool.close().await;
    sqlx::query(&format!("drop database {} with (force)", fixture.database))
        .execute(&fixture.control)
        .await?;
    fixture.control.close().await;
    Ok(())
}

async fn machine_status(pool: &sqlx::PgPool, id: Uuid) -> Fallible<Option<String>> {
    sqlx::query_scalar("select machine_status from deposits where id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .map_err(Into::into)
}

#[tokio::test]
async fn migration_0011_grandfathers_every_pre_0011_deposit_status() -> TestResult {
    let fixture = migrated_fixture().await?;

    for ((status, expected), (seeded, id)) in LEGACY_STATUSES.iter().zip(&fixture.legacy) {
        assert_eq!(status, seeded);
        assert_eq!(
            machine_status(&fixture.pool, *id).await?.as_deref(),
            Some(*expected),
            "pre-0011 {status} row must be grandfathered as {expected}"
        );
    }

    // The marker is what it claims to be — pinned so a change is deliberate.
    let convalidated: bool = sqlx::query_scalar(
        "select convalidated from pg_constraint where conname = 'deposits_observation_identity'",
    )
    .fetch_one(&fixture.pool)
    .await?;
    assert!(
        !convalidated,
        "0011 ships the constraint NOT VALID; the sibling test pins that it is still enforced"
    );

    // The predicate exempts every grandfathered shape by itself, so the
    // constraint validates cleanly WITH the legacy rows still present: the
    // NOT VALID marker buys nothing the predicate does not already give.
    let untouched: i64 = sqlx::query_scalar(
        "select count(*) from deposits
          where machine_status in ('admitted_legacy', 'quarantined_legacy')
            and source_address is null",
    )
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(
        untouched, 3,
        "all three legacy rows carry no chain identity"
    );
    let validated =
        sqlx::query("alter table deposits validate constraint deposits_observation_identity")
            .execute(&fixture.pool)
            .await;
    assert!(
        validated.is_ok(),
        "the grandfathering predicate makes VALIDATE safe with legacy rows present: {validated:?}"
    );

    teardown(fixture).await
}

#[tokio::test]
async fn observation_identity_is_enforced_on_insert_and_update_despite_not_valid() -> TestResult {
    let fixture = migrated_fixture().await?;

    // NOT VALID does NOT mean unenforced: a fresh machine row without the full
    // observation identity is refused at INSERT.
    let rejected = sqlx::query(
        "insert into deposits (id, user_id, chain_sig, amount_micro, status, machine_status)
         values ($1, $2, 'no-identity', 1000000, 'observed_finalized', 'observed_finalized')",
    )
    .bind(Uuid::new_v4())
    .bind(fixture.user)
    .execute(&fixture.pool)
    .await;
    assert!(
        rejected.is_err(),
        "a machine-status deposit with no chain identity must never be insertable"
    );

    // ...nor at UPDATE: the quarantine cannot be lifted into the machine
    // without the identity the row never recorded.
    let (_, quarantined) = fixture
        .legacy
        .iter()
        .find(|(status, _)| *status == "seen")
        .copied()
        .ok_or("the fixture always seeds a 'seen' legacy deposit")?;
    let promoted = sqlx::query(
        "update deposits set status = 'observed_finalized',
                             machine_status = 'observed_finalized'
          where id = $1",
    )
    .bind(quarantined)
    .execute(&fixture.pool)
    .await;
    assert!(
        promoted.is_err(),
        "un-quarantining a legacy row without chain identity must be refused"
    );
    assert_eq!(
        machine_status(&fixture.pool, quarantined).await?.as_deref(),
        Some("quarantined_legacy"),
        "the refused promotion left the row quarantined"
    );

    // The SAME promotion WITH a full identity is legal. This is the
    // non-tautology guard: the rejection above is caused by the missing chain
    // evidence, not by the status label or some unrelated constraint.
    sqlx::query(
        "update deposits
            set status = 'observed_finalized', machine_status = 'observed_finalized',
                source_address = 'src', dest_address = 'dst', mint = 'usdc',
                observed_slot = 7, rail_fingerprint = 'rail:test'
          where id = $1",
    )
    .bind(quarantined)
    .execute(&fixture.pool)
    .await?;
    assert_eq!(
        machine_status(&fixture.pool, quarantined).await?.as_deref(),
        Some("observed_finalized"),
        "with evidence the promotion succeeds"
    );

    teardown(fixture).await
}
