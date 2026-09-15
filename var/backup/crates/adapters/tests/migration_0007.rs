#![allow(clippy::too_many_lines, clippy::unwrap_used)]

use std::str::FromStr;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use uuid::Uuid;

mod common;

async fn isolated_pre_content_database() -> (sqlx::PgPool, sqlx::PgPool, String) {
    let url = common::database_url();
    let control = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    let database = format!("opinions_content_migration_{}", Uuid::new_v4().simple());
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
        include_str!("../../../migrations/0006_social.sql"),
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
async fn migration_0007_installs_content_jobs_assets_and_moderation_constraints() {
    let (pool, control, database) = isolated_pre_content_database().await;
    let user = Uuid::new_v4();
    let market = Uuid::new_v4();
    let comment = Uuid::new_v4();
    sqlx::query("insert into users (id, handle) values ($1, $2)")
        .bind(user)
        .bind(format!("content-{user}"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "insert into markets (id, slug, question, status, min_votes_to_resolve) values ($1, $2, 'q', 'draft', 3)",
    )
    .bind(market)
    .bind(format!("content-{market}"))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("insert into comments (id, market_id, user_id, body) values ($1, $2, $3, 'body')")
        .bind(comment)
        .bind(market)
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();

    sqlx::raw_sql(include_str!("../../../migrations/0007_content.sql"))
        .execute(&pool)
        .await
        .unwrap();

    let draft = Uuid::new_v4();
    let insert_draft = r"insert into market_drafts
        (id, source, tier, status, publish_stage, published_market_id, question,
         description, video_script, slug, seed_micro, fee_bps,
         min_votes_to_resolve, open_secs, hidden_window_secs, publish_at, expires_at)
        values ($1, 'template', 'daily', 'approved', 'claimed', $2, 'q', 'd', 'v',
                $3, 1000000, 100, 3, 3600, 300, '2030-01-01 UTC', '2030-01-02 UTC')";
    sqlx::query(insert_draft)
        .bind(draft)
        .bind(market)
        .bind(format!("draft-{draft}"))
        .execute(&pool)
        .await
        .unwrap();
    assert!(sqlx::query(insert_draft)
        .bind(Uuid::new_v4())
        .bind(market)
        .bind("duplicate-slot")
        .execute(&pool)
        .await
        .is_err());

    sqlx::query(
        "insert into video_jobs (market_id, tier, status, kind, draft_id) values ($1, 'hero', 'failed', 'poster', $2)",
    )
    .bind(market)
    .bind(draft)
    .execute(&pool)
    .await
    .unwrap();
    assert!(sqlx::query(
        "insert into video_jobs (market_id, tier, status, kind, draft_id) values ($1, 'hero', 'queued', 'poster', $2)",
    )
    .bind(market)
    .bind(draft)
    .execute(&pool)
    .await
    .is_err());
    sqlx::query(
        "insert into video_jobs (market_id, tier, status, kind) values ($1, 'hero', 'queued', 'market_video')",
    )
    .bind(market)
    .execute(&pool)
    .await
    .unwrap();
    assert!(sqlx::query(
        "insert into video_jobs (market_id, tier, status, kind) values ($1, 'hero', 'ready', 'market_video')",
    )
    .bind(market)
    .execute(&pool)
    .await
    .is_err());
    assert!(sqlx::query(
        "insert into video_jobs (market_id, tier, status, kind) values ($1, 'hero', 'queued', 'share_card')",
    )
    .bind(market)
    .execute(&pool)
    .await
    .is_err());

    sqlx::query("insert into moderation_jobs (comment_id) values ($1)")
        .bind(comment)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        sqlx::query("insert into moderation_jobs (comment_id) values ($1)")
            .bind(comment)
            .execute(&pool)
            .await
            .is_err()
    );
    let cursor: i64 =
        sqlx::query_scalar("select last_seq from outbox_cursors where consumer = 'moderation'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(cursor, 0);
    let asset_columns: i64 = sqlx::query_scalar(
        "select count(*) from information_schema.columns where table_name = 'markets' and column_name in ('poster_asset_url', 'video_asset_url')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(asset_columns, 2);

    drop_database(pool, control, &database).await;
}
