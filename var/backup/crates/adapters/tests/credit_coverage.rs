//! Focused `PostgreSQL` contracts for credit/deposit adapter seams that are not
//! naturally reached through the end-to-end use-case suites.

use adapters::pg::PgStore;
use application::model::{DepositId, UserId};
use application::ports::{OutboundSubject, Store};
use sqlx::postgres::{PgPool, PgPoolOptions};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

struct CreditFixture {
    user: UserId,
    trade_id: Uuid,
    live_allocation_id: Uuid,
    now: OffsetDateTime,
}

async fn scratch_pool() -> TestResult<Option<PgPool>> {
    let Ok(base) = std::env::var("DATABASE_URL") else {
        return Ok(None);
    };
    let (root, _) = base.rsplit_once('/').ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "DATABASE_URL must name a database",
        )
    })?;
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&base)
        .await?;
    sqlx::query("drop database if exists opinions_suite_credit_coverage with (force)")
        .execute(&admin)
        .await?;
    sqlx::query("create database opinions_suite_credit_coverage")
        .execute(&admin)
        .await?;
    admin.close().await;

    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&format!("{root}/opinions_suite_credit_coverage"))
        .await?;
    MIGRATOR.run(&pool).await?;
    Ok(Some(pool))
}

async fn seed_credit_facts(pool: &PgPool) -> TestResult<CreditFixture> {
    let user = UserId(Uuid::new_v4());
    sqlx::query("insert into users (id, handle, kyc_tier, status) values ($1,$2,2,'active')")
        .bind(user.0)
        .bind(format!("credit-coverage-{}", user.0))
        .execute(pool)
        .await?;
    let now = OffsetDateTime::from_unix_timestamp(1_700_000_000)?;
    let live_lot = Uuid::new_v4();
    for (lot_id, amount, grant_class, converted_at) in [
        (live_lot, 100_i64, "real_money", None),
        (Uuid::new_v4(), 23_i64, "real_money", Some(now)),
        (Uuid::new_v4(), 41_i64, "sweeps", None),
    ] {
        sqlx::query(
            "insert into credit_grant_lots \
             (id,user_id,source,amount_micro,granted_at,grant_class,policy_version,converted_at) \
             values ($1,$2,'coverage-contract',$3,$4,$5,'policy-1',$6)",
        )
        .bind(lot_id)
        .bind(user.0)
        .bind(amount)
        .bind(now)
        .bind(grant_class)
        .bind(converted_at)
        .execute(pool)
        .await?;
    }

    let trade_id = Uuid::new_v4();
    let finalized_source = Uuid::new_v4();
    let live_allocation_id = Uuid::new_v4();
    for (id, split_seq, amount, key) in [
        (finalized_source, 0_i32, 7_i64, "allocated-finalized"),
        (live_allocation_id, 1_i32, 11_i64, "allocated-live"),
    ] {
        sqlx::query(
            "insert into credit_fee_allocations \
             (id,trade_id,lot_id,split_seq,amount_micro,kind,idempotency_key) \
             values ($1,$2,$3,$4,$5,'allocated',$6)",
        )
        .bind(id)
        .bind(trade_id)
        .bind(live_lot)
        .bind(split_seq)
        .bind(amount)
        .bind(key)
        .execute(pool)
        .await?;
    }
    sqlx::query(
        "insert into credit_fee_allocations \
         (id,trade_id,lot_id,split_seq,amount_micro,kind,source_allocation_id,idempotency_key) \
         values ($1,$2,$3,0,7,'finalized',$4,'terminal-finalized')",
    )
    .bind(Uuid::new_v4())
    .bind(trade_id)
    .bind(live_lot)
    .bind(finalized_source)
    .execute(pool)
    .await?;
    sqlx::query(
        "insert into self_exclusions (id,user_id,starts_at,cooling_off_until) \
         values ($1,$2,$3,$4)",
    )
    .bind(Uuid::new_v4())
    .bind(user.0)
    .bind(now)
    .bind(now + Duration::hours(1))
    .execute(pool)
    .await?;

    Ok(CreditFixture {
        user,
        trade_id,
        live_allocation_id,
        now,
    })
}

async fn assert_credit_queries(store: &PgStore, fixture: &CreditFixture) -> TestResult {
    let mut tx = store.credit_convert_tx().await?;
    let live = tx.live_allocations_for_trade(fixture.trade_id).await?;
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].id, fixture.live_allocation_id);
    assert_eq!(live[0].amount_micro, 11);
    assert!(tx.self_excluded(fixture.user, fixture.now).await?);
    assert!(
        !tx.self_excluded(fixture.user, fixture.now + Duration::hours(2))
            .await?
    );

    assert_eq!(tx.referral_code_for_user(fixture.user).await?, None);
    assert_eq!(tx.referral_code_owner("COVERAGE-REF").await?, None);
    tx.insert_referral_code(fixture.user, "COVERAGE-REF")
        .await?;
    assert_eq!(
        tx.referral_code_for_user(fixture.user).await?,
        Some("COVERAGE-REF".into())
    );
    assert_eq!(
        tx.referral_code_owner("COVERAGE-REF").await?,
        Some(fixture.user)
    );
    tx.commit().await?;
    Ok(())
}

async fn assert_refund_outbound(store: &PgStore) -> TestResult {
    let deposit = DepositId(Uuid::new_v4());
    let mut tx = store.deposit_admission_tx().await?;
    assert_eq!(tx.refund_payment_for_deposit(deposit).await?, None);
    let payment_id = tx
        .insert_refund_payment(deposit, "source-address", 29, "rail-v1")
        .await?;
    let refund = tx
        .refund_payment_for_deposit(deposit)
        .await?
        .ok_or_else(|| std::io::Error::other("refund payment missing after insert"))?;
    assert_eq!(refund.id, payment_id);
    assert_eq!(refund.deposit, deposit);
    assert_eq!(refund.dest, "source-address");
    assert_eq!(refund.amount_micro, 29);
    assert_eq!(refund.rail_fingerprint, "rail-v1");

    let outbound = tx
        .outbound_by_subject(OutboundSubject::DepositRefund, deposit.0)
        .await?
        .ok_or_else(|| std::io::Error::other("generic outbound lookup missed refund"))?;
    assert_eq!(outbound.id, payment_id);
    assert_eq!(outbound.subject, OutboundSubject::DepositRefund);
    assert_eq!(outbound.subject_id, deposit.0);
    assert_eq!(outbound.dest, "source-address");
    assert_eq!(outbound.amount_micro, 29);
    assert_eq!(outbound.rail_fingerprint, "rail-v1");
    tx.commit().await?;
    Ok(())
}

async fn assert_reserve_snapshot(store: &PgStore) -> TestResult {
    let mut snapshot = store.invariant_read_tx().await?;
    assert_eq!(snapshot.bonus_reserve_and_promise().await?, (0, 100));
    Ok(())
}

#[tokio::test]
async fn pg_credit_refund_and_invariant_queries_preserve_exact_contracts() -> TestResult {
    let Some(pool) = scratch_pool().await? else {
        return Ok(());
    };
    let fixture = seed_credit_facts(&pool).await?;
    let store = PgStore::from_pool(pool.clone());

    assert_credit_queries(&store, &fixture).await?;
    assert_refund_outbound(&store).await?;
    assert_reserve_snapshot(&store).await?;

    pool.close().await;
    Ok(())
}
