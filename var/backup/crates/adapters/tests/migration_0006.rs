#![allow(clippy::unwrap_used)]

use std::str::FromStr;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::Row;
use uuid::Uuid;

mod common;

async fn isolated_pre_social_database() -> (sqlx::PgPool, sqlx::PgPool, String) {
    let url = common::database_url();
    let control = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    let database = format!("opinions_social_migration_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("create database {database}"))
        .execute(&control)
        .await
        .unwrap();
    let options = PgConnectOptions::from_str(&url)
        .unwrap()
        .database(&database);
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await
        .unwrap();
    for migration in [
        include_str!("../../../migrations/0001_init.sql"),
        include_str!("../../../migrations/0002_ledger_triggers.sql"),
        include_str!("../../../migrations/0003_accounts_identity.sql"),
        include_str!("../../../migrations/0004_scheduler.sql"),
        include_str!("../../../migrations/0005_economy_integrity.sql"),
    ] {
        sqlx::raw_sql(migration).execute(&pool).await.unwrap();
    }
    (pool, control, database)
}

async fn drop_database(pool: sqlx::PgPool, control: sqlx::PgPool, database: &str) {
    pool.close().await;
    sqlx::query(&format!("drop database {database} with (force)"))
        .execute(&control)
        .await
        .unwrap();
    control.close().await;
}

#[tokio::test]
async fn migration_0006_backfills_nested_threads_and_enforces_same_market_parents() {
    let (pool, control, database) = isolated_pre_social_database().await;
    let user = Uuid::new_v4();
    let market = Uuid::new_v4();
    let other_market = Uuid::new_v4();
    let root = Uuid::new_v4();
    let child = Uuid::new_v4();
    let grandchild = Uuid::new_v4();
    sqlx::query("insert into users (id, handle) values ($1, $2)")
        .bind(user)
        .bind(format!("migration-{user}"))
        .execute(&pool)
        .await
        .unwrap();
    for (id, slug) in [
        (market, "migration-social"),
        (other_market, "migration-other"),
    ] {
        sqlx::query(
            "insert into markets (id, slug, question, status, min_votes_to_resolve) values ($1, $2, 'q', 'draft', 1)",
        )
        .bind(id)
        .bind(format!("{slug}-{id}"))
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query(
        r"
        insert into comments (id, market_id, user_id, parent_id, body)
        values ($1, $4, $5, null, 'root'),
               ($2, $4, $5, $1, 'child'),
               ($3, $4, $5, $2, 'grandchild')
        ",
    )
    .bind(root)
    .bind(child)
    .bind(grandchild)
    .bind(market)
    .bind(user)
    .execute(&pool)
    .await
    .unwrap();

    sqlx::raw_sql(include_str!("../../../migrations/0006_social.sql"))
        .execute(&pool)
        .await
        .unwrap();

    let rows =
        sqlx::query("select id, depth, reply_count, body_hash from comments order by depth, id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 3);
    let by_id: std::collections::HashMap<Uuid, (i32, i32, Option<String>)> = rows
        .iter()
        .map(|row| {
            (
                row.get("id"),
                (
                    row.get("depth"),
                    row.get("reply_count"),
                    row.get("body_hash"),
                ),
            )
        })
        .collect();
    assert_eq!(by_id[&root], (0, 1, None));
    assert_eq!(by_id[&child], (1, 1, None));
    assert_eq!(by_id[&grandchild], (2, 0, None));
    let cross_market = sqlx::query(
        "insert into comments (market_id, user_id, parent_id, body) values ($1, $2, $3, 'bad')",
    )
    .bind(other_market)
    .bind(user)
    .bind(root)
    .execute(&pool)
    .await;
    assert!(cross_market.is_err());
    let cursor: i64 =
        sqlx::query_scalar("select last_seq from outbox_cursors where consumer = 'notifier'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(cursor, 0);
    drop_database(pool, control, &database).await;
}

#[tokio::test]
async fn postgres_hot_score_is_value_identical_to_domain_on_the_pinned_grid() {
    let url = common::database_url();
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let scores = [i32::MIN, -1, 0, 1, i32::MAX];
    let ages = [0_i64, 3_599, 3_600, 7_199, 1_000_000_000, i64::MAX];
    let mut rust_values = Vec::new();
    let mut sql_values = Vec::new();
    for score in scores {
        for age in ages {
            let sql: Option<i64> = sqlx::query_scalar(
                r"
                select case when $2::bigint < 0 then null else
                  trunc(
                    ((greatest($1::bigint, 0) + 1)::numeric * 1000000::numeric) /
                    (((($2::bigint / 3600) + 2)::numeric) *
                     ((($2::bigint / 3600) + 2)::numeric))
                  )::bigint
                end
                ",
            )
            .bind(score)
            .bind(age)
            .fetch_one(&pool)
            .await
            .unwrap();
            let rust = domain::ranking::hot_score(score, age).unwrap();
            assert_eq!(sql, Some(rust), "score={score}, age={age}");
            rust_values.push((rust, score, age));
            sql_values.push((sql.unwrap(), score, age));
        }
    }
    rust_values.sort_unstable();
    sql_values.sort_unstable();
    assert_eq!(rust_values, sql_values);
    assert_eq!(
        domain::ranking::hot_score(0, -1),
        Err(domain::ranking::RankError::FutureTimestamp)
    );
    let future: Option<i64> = sqlx::query_scalar(
        "select case when $1::bigint < 0 then null else trunc(1::numeric)::bigint end",
    )
    .bind(-1_i64)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(future, None);
    pool.close().await;
}
