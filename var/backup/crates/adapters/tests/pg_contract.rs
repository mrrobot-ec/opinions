#![allow(
    clippy::expect_used,
    clippy::needless_raw_string_hashes,
    clippy::too_many_lines,
    clippy::unwrap_used
)]

use adapters::pg::PgStore;
use application::advance_market::{AdvanceMarket, AdvanceMarketCmd};
use application::comments::{PostComment, PostCommentCmd, ReportComment, ReportCommentCmd};
use application::contract::{
    comment_writer_contract, concurrent_duplicate_key_contract, deposit_writer_contract,
    double_spend_two_tx_contract, integrity_report_contract, ledger_writer_contract,
    seeded_settlement_io_contract, vote_writer_contract,
};
use application::credit_deposit::{
    admit_observed, begin_refund_send, confirm_deposit_refund, observe_finalized_on_rail,
    propose_deposit_command, settle_refund,
};
use application::error::{AppError, StoreError};
use application::model::{
    AdminContext, AdminRole, CommentId, Event, MarketId, OwnerRef, SocialConfig, TradeAction,
    UserId,
};
use application::money::credits::{GrantCredit, GrantCreditCmd};
use application::money::referrals::{
    BindReferral, BindReferralCmd, GrantReferral, GrantReferralCmd,
};
use application::money::{
    AllocationFact, AllocationKind, CreditLotRow, GrantClass, ObservedDeposit,
};
use application::place_trade::{PlaceTrade, PlaceTradeCmd};
use application::ports::{Clock, MarketQueries, NoopCrashPoint, SocialQueries, Store};
use application::resolve_market::{ResolveMarket, ResolveMarketCmd};
use application::seed_market::{SeedMarket, SeedMarketCmd};
use domain::amm::Side;
use domain::ledger::{Currency, Entry, TxnKind};
use domain::market::MarketState;
use domain::money::{BasisPoints, MicroUsd};
use sqlx::Row;
use std::sync::{Arc, OnceLock};
use time::{Duration, OffsetDateTime};
use tokio::time::{sleep, timeout};
use uuid::Uuid;

async fn stateful_money_config_guard() -> tokio::sync::OwnedMutexGuard<()> {
    static LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    Arc::clone(LOCK.get_or_init(|| Arc::new(tokio::sync::Mutex::new(()))))
        .lock_owned()
        .await
}

async fn pg_store() -> Option<PgStore> {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset; skipping PostgreSQL contract test");
        return None;
    };
    Some(PgStore::connect(&url).await.unwrap())
}

async fn seed_money_clear(store: &PgStore, user: UserId, now: OffsetDateTime) {
    sqlx::query("update users set kyc_tier = 2 where id = $1")
        .bind(user.0)
        .execute(store.pool_handle())
        .await
        .unwrap();
    for context in ["geo", "sanctions"] {
        sqlx::query(
            r#"insert into sanction_screenings
                (id, user_id, context, verdict, checked_at, expires_at, policy_version)
               values ($1, $2, $3, 'clear', $4, $5, 'test')"#,
        )
        .bind(Uuid::new_v4())
        .bind(user.0)
        .bind(context)
        .bind(now)
        .bind(now + Duration::hours(4))
        .execute(store.pool_handle())
        .await
        .unwrap();
    }
}

async fn seed_provisional_credit_allocation(
    store: &PgStore,
    fixture: &Fixture,
    amount_micro: i64,
) -> (Uuid, Uuid, Uuid) {
    let now = OffsetDateTime::now_utc();
    let trade_id = Uuid::new_v4();
    sqlx::query(
        r#"insert into trades
            (id, user_id, market_id, outcome_id, side, collateral_micro,
             shares_micro, fee_micro, seq)
           values ($1, $2, $3, $4, 'buy', 1, 1, $5,
                   (select coalesce(max(seq), 0) + 1 from trades where market_id = $3))"#,
    )
    .bind(trade_id)
    .bind(fixture.user.0)
    .bind(fixture.market.0)
    .bind(fixture.yes_outcome)
    .bind(amount_micro)
    .execute(store.pool_handle())
    .await
    .unwrap();

    let lot_id = Uuid::new_v4();
    let allocation_id = Uuid::new_v4();
    let key = format!("credit-race-seed-{}", Uuid::new_v4());
    let mut tx = store.credit_convert_tx().await.unwrap();
    tx.serialize_key(&key).await.unwrap();
    let reserve_before = tx.bonus_reserve_balance().await.unwrap();
    let promised_before = tx.remaining_real_money_promise().await.unwrap();
    let required_reserve = promised_before.checked_add(amount_micro).unwrap();
    let reserve_top_up = required_reserve.saturating_sub(reserve_before).max(0);
    let external_cash = tx
        .account(OwnerRef::External, Currency::Usdc)
        .await
        .unwrap();
    let reserve = tx
        .account(OwnerRef::BonusReserve, Currency::Usdc)
        .await
        .unwrap();
    if reserve_top_up > 0 {
        tx.ledger_apply(
            TxnKind::Seed,
            &format!("{key}:reserve"),
            &[
                Entry {
                    account: external_cash,
                    amount: MicroUsd(-reserve_top_up),
                },
                Entry {
                    account: reserve,
                    amount: MicroUsd(reserve_top_up),
                },
            ],
        )
        .await
        .unwrap();
    }
    let external_credit = tx
        .account(OwnerRef::External, Currency::UsdcCredit)
        .await
        .unwrap();
    let house_credit = tx
        .account(OwnerRef::House, Currency::UsdcCredit)
        .await
        .unwrap();
    let user_credit = tx
        .account(OwnerRef::User(fixture.user), Currency::UsdcCredit)
        .await
        .unwrap();
    tx.ledger_apply(
        TxnKind::Seed,
        &format!("{key}:house-credit"),
        &[
            Entry {
                account: external_credit,
                amount: MicroUsd(-amount_micro),
            },
            Entry {
                account: house_credit,
                amount: MicroUsd(amount_micro),
            },
        ],
    )
    .await
    .unwrap();
    tx.ledger_apply(
        TxnKind::CreditGrant,
        &format!("{key}:grant"),
        &[
            Entry {
                account: house_credit,
                amount: MicroUsd(-amount_micro),
            },
            Entry {
                account: user_credit,
                amount: MicroUsd(amount_micro),
            },
        ],
    )
    .await
    .unwrap();
    tx.insert_credit_lot(&CreditLotRow {
        id: lot_id,
        user: fixture.user,
        source: "pg-race".into(),
        amount_micro,
        granted_at: now,
        grant_class: GrantClass::RealMoney,
        policy_version: "test".into(),
        converted_at: None,
    })
    .await
    .unwrap();
    tx.remember_lot_idempotency(&format!("{key}:grant"), lot_id)
        .await
        .unwrap();
    tx.insert_allocation(&AllocationFact {
        id: allocation_id,
        trade_id,
        lot_id,
        split_seq: 0,
        amount_micro,
        kind: AllocationKind::Allocated,
        source_allocation_id: None,
        idempotency_key: format!("{key}:allocation"),
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();
    (lot_id, trade_id, allocation_id)
}

#[tokio::test]
async fn pg_passes_ledger_writer_contract() {
    let Some(store) = pg_store().await else {
        return;
    };
    ledger_writer_contract(&store).await;
}

#[tokio::test]
async fn pg_passes_concurrent_duplicate_key_contract() {
    let Some(store) = pg_store().await else {
        return;
    };
    concurrent_duplicate_key_contract(&store).await;
}

#[tokio::test]
async fn pg_passes_double_spend_two_tx_contract() {
    let Some(store) = pg_store().await else {
        return;
    };
    double_spend_two_tx_contract(&store).await;
}

#[tokio::test]
async fn pg_passes_vote_writer_contract() {
    let Some(store) = pg_store().await else {
        return;
    };
    vote_writer_contract(&store).await;
}

#[tokio::test]
async fn pg_passes_deposit_writer_contract() {
    let Some(store) = pg_store().await else {
        return;
    };
    deposit_writer_contract(&store).await;
}

#[tokio::test]
async fn pg_credit_finalization_moves_provisional_then_lazy_converts_once() {
    let Some(store) = pg_store().await else {
        return;
    };
    let _stateful_guard = stateful_money_config_guard().await;
    reset_config(&store).await;
    let fixture = market_fixture(&store).await;
    let now = OffsetDateTime::now_utc();
    sqlx::query("update users set kyc_tier = 2 where id = $1")
        .bind(fixture.user.0)
        .execute(store.pool_handle())
        .await
        .unwrap();
    for context in ["geo", "sanctions"] {
        sqlx::query(
            r#"insert into sanction_screenings
                (id, user_id, context, verdict, checked_at, expires_at, policy_version)
               values ($1, $2, $3, 'clear', $4, $5, 'test')"#,
        )
        .bind(Uuid::new_v4())
        .bind(fixture.user.0)
        .bind(context)
        .bind(now)
        .bind(now + Duration::hours(1))
        .execute(store.pool_handle())
        .await
        .unwrap();
    }
    sqlx::query(
        r#"insert into votes
            (id, user_id, market_id, outcome_id, crowd_guess_pct, seq, idempotency_key)
           values ($1, $2, $3, $4, 50, 1, $5)"#,
    )
    .bind(Uuid::new_v4())
    .bind(fixture.user.0)
    .bind(fixture.market.0)
    .bind(fixture.yes_outcome)
    .bind(format!("credit-vote-{}", Uuid::new_v4()))
    .execute(store.pool_handle())
    .await
    .unwrap();

    let mut seed = store.credit_convert_tx().await.unwrap();
    let seed_key = format!("credit-seed-{}", Uuid::new_v4());
    seed.serialize_key(&seed_key).await.unwrap();
    let external_cash = seed
        .account(OwnerRef::External, Currency::Usdc)
        .await
        .unwrap();
    let reserve = seed
        .account(OwnerRef::BonusReserve, Currency::Usdc)
        .await
        .unwrap();
    let external_credit = seed
        .account(OwnerRef::External, Currency::UsdcCredit)
        .await
        .unwrap();
    let house_credit = seed
        .account(OwnerRef::House, Currency::UsdcCredit)
        .await
        .unwrap();
    let user_cash = seed
        .account(OwnerRef::User(fixture.user), Currency::Usdc)
        .await
        .unwrap();
    seed.ledger_apply(
        TxnKind::Seed,
        &seed_key,
        &[
            Entry {
                account: external_cash,
                amount: MicroUsd(-50_040_000),
            },
            Entry {
                account: reserve,
                amount: MicroUsd(40_000),
            },
            Entry {
                account: user_cash,
                amount: MicroUsd(50_000_000),
            },
        ],
    )
    .await
    .unwrap();
    seed.ledger_apply(
        TxnKind::Seed,
        &format!("{seed_key}:credit"),
        &[
            Entry {
                account: external_credit,
                amount: MicroUsd(-40_000),
            },
            Entry {
                account: house_credit,
                amount: MicroUsd(40_000),
            },
        ],
    )
    .await
    .unwrap();
    seed.commit().await.unwrap();

    let first_grant_key = format!("credit-grant-first-{}", Uuid::new_v4());
    let first_grant = GrantCredit { store: &store }
        .execute(GrantCreditCmd {
            user: fixture.user,
            amount: MicroUsd(15_000),
            source: "pg-contract".into(),
            grant_class: GrantClass::RealMoney,
            policy_version: "test".into(),
            idempotency_key: first_grant_key.clone(),
            granted_at: now - Duration::seconds(1),
        })
        .await
        .unwrap();
    assert_eq!(
        GrantCredit { store: &store }
            .execute(GrantCreditCmd {
                user: fixture.user,
                amount: MicroUsd(15_000),
                source: "pg-contract".into(),
                grant_class: GrantClass::RealMoney,
                policy_version: "test".into(),
                idempotency_key: first_grant_key,
                granted_at: now - Duration::seconds(1),
            })
            .await
            .unwrap()
            .lot_id,
        first_grant.lot_id
    );
    GrantCredit { store: &store }
        .execute(GrantCreditCmd {
            user: fixture.user,
            amount: MicroUsd(25_000),
            source: "pg-contract".into(),
            grant_class: GrantClass::RealMoney,
            policy_version: "test".into(),
            idempotency_key: format!("credit-grant-second-{}", Uuid::new_v4()),
            granted_at: now,
        })
        .await
        .unwrap();

    let trade = PlaceTrade {
        store: &store,
        clock: &FixedClock(now),
        rep_config: application::model::RepConfig::default(),
    }
    .execute(PlaceTradeCmd {
        market: fixture.market,
        user: fixture.user,
        side: Side::Yes,
        action: TradeAction::Buy,
        amount_micro: 5_000_000,
        idempotency_key: format!("credit-trade-{}", Uuid::new_v4()),
        run_id: None,
        pending_action_id: None,
        expected_config_version: Some(1),
    })
    .await
    .unwrap();
    assert!(trade.fee.0 >= 40_000, "fixture fee must cover the lot");
    let allocated_amounts: Vec<i64> = sqlx::query_scalar(
        "select amount_micro from credit_fee_allocations where trade_id = $1 and kind = 'allocated' order by amount_micro",
    )
    .bind(trade.trade_id.0)
    .fetch_all(store.pool_handle())
    .await
    .unwrap();
    assert_eq!(allocated_amounts, vec![15_000, 25_000]);

    let finalize_once = |store: PgStore| async move {
        let mut finalize = store.resolve_tx().await.unwrap();
        let moved = finalize
            .finalize_market_fee_allocations(fixture.market)
            .await?;
        finalize.commit().await?;
        Ok::<u32, StoreError>(moved)
    };
    let (left, right) = tokio::join!(finalize_once(store.clone()), finalize_once(store.clone()));
    assert_eq!(left.unwrap() + right.unwrap(), 2);
    let mut replay = store.resolve_tx().await.unwrap();
    assert_eq!(
        replay
            .finalize_market_fee_allocations(fixture.market)
            .await
            .unwrap(),
        0
    );
    replay.commit().await.unwrap();

    let before_convert: i64 = sqlx::query_scalar(
        "select coalesce(sum(amount_micro), 0)::bigint from ledger_entries where account_id = $1",
    )
    .bind(user_cash.0)
    .fetch_one(store.pool_handle())
    .await
    .unwrap();
    let mut convert = store.credit_convert_tx().await.unwrap();
    convert.lock_user(fixture.user).await.unwrap();
    let converted = convert
        .convert_then_collect(fixture.user, now, "pg-credit-lock")
        .await
        .unwrap();
    assert_eq!(converted.converted_micro, 40_000);
    assert_eq!(converted.lots_converted, 2);
    assert_eq!(converted.conversion_txns.len(), 2);
    assert!(converted.conversion_txns.iter().all(|txn| !txn.replayed));
    let original_txns = converted.conversion_txns;
    convert.commit().await.unwrap();
    let mut replay = store.credit_convert_tx().await.unwrap();
    replay.lock_user(fixture.user).await.unwrap();
    let replayed = replay
        .convert_then_collect(fixture.user, now, "pg-credit-lock-replay")
        .await
        .unwrap();
    replay.commit().await.unwrap();
    assert_eq!(replayed.converted_micro, 0);
    assert_eq!(replayed.conversion_txns.len(), 2);
    assert!(replayed.conversion_txns.iter().all(|txn| txn.replayed));
    for original in original_txns {
        let replayed_txn = replayed
            .conversion_txns
            .iter()
            .find(|txn| txn.lot_id == original.lot_id)
            .unwrap();
        assert_eq!(replayed_txn.retire_txn, original.retire_txn);
        assert_eq!(replayed_txn.pay_txn, original.pay_txn);
    }
    let after_convert: i64 = sqlx::query_scalar(
        "select coalesce(sum(amount_micro), 0)::bigint from ledger_entries where account_id = $1",
    )
    .bind(user_cash.0)
    .fetch_one(store.pool_handle())
    .await
    .unwrap();
    assert_eq!(after_convert - before_convert, 40_000);
}

#[tokio::test]
async fn pg_finalize_vs_reverse_race_has_exactly_one_terminal_child() {
    let Some(store) = pg_store().await else {
        return;
    };
    let _stateful_guard = stateful_money_config_guard().await;
    let fixture = market_fixture(&store).await;
    let (_, _, allocation_id) = seed_provisional_credit_allocation(&store, &fixture, 25_000).await;

    let finalize = async {
        let mut tx = store.resolve_tx().await.unwrap();
        let moved = tx.finalize_market_fee_allocations(fixture.market).await?;
        tx.commit().await?;
        Ok::<u32, StoreError>(moved)
    };
    let reverse = async {
        let mut tx = store.unwind_tx().await.unwrap();
        let moved = tx.reverse_market_fee_allocations(fixture.market).await?;
        tx.commit().await?;
        Ok::<u32, StoreError>(moved)
    };
    let (finalized, reversed) = tokio::join!(finalize, reverse);
    assert!(finalized.is_ok() || reversed.is_ok());
    for loser in [&finalized, &reversed] {
        assert!(matches!(loser, Ok(0 | 1) | Err(StoreError::Conflict(_))));
    }

    let terminal_kinds: Vec<String> = sqlx::query_scalar(
        "select kind from credit_fee_allocations where source_allocation_id = $1",
    )
    .bind(allocation_id)
    .fetch_all(store.pool_handle())
    .await
    .unwrap();
    assert_eq!(terminal_kinds.len(), 1);
    assert!(matches!(
        terminal_kinds[0].as_str(),
        "finalized" | "reversed"
    ));
}

#[tokio::test]
async fn pg_convert_vs_unwind_race_never_converts_provisional_progress() {
    let Some(store) = pg_store().await else {
        return;
    };
    let _stateful_guard = stateful_money_config_guard().await;
    let fixture = market_fixture(&store).await;
    let amount_micro = 30_000;
    let (lot_id, _, allocation_id) =
        seed_provisional_credit_allocation(&store, &fixture, amount_micro).await;
    let user_cash = {
        let mut tx = store.credit_convert_tx().await.unwrap();
        let account = tx
            .account(OwnerRef::User(fixture.user), Currency::Usdc)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        account
    };
    let before_cash: i64 = sqlx::query_scalar(
        "select coalesce(sum(amount_micro), 0)::bigint from ledger_entries where account_id = $1",
    )
    .bind(user_cash.0)
    .fetch_one(store.pool_handle())
    .await
    .unwrap();

    let convert = async {
        let mut tx = store.credit_convert_tx().await.unwrap();
        tx.lock_user(fixture.user).await?;
        let receipt = tx
            .convert_then_collect(
                fixture.user,
                OffsetDateTime::now_utc(),
                "convert-unwind-race",
            )
            .await?;
        tx.commit().await?;
        Ok::<_, AppError>(receipt)
    };
    let reverse = async {
        let mut tx = store.unwind_tx().await.unwrap();
        let moved = tx.reverse_market_fee_allocations(fixture.market).await?;
        tx.commit().await?;
        Ok::<u32, StoreError>(moved)
    };
    let (converted, reversed) = tokio::join!(convert, reverse);
    let converted = converted.unwrap();
    assert_eq!(converted.converted_micro, 0);
    assert_eq!(converted.lots_converted, 0);
    assert_eq!(reversed.unwrap(), 1);

    let after_cash: i64 = sqlx::query_scalar(
        "select coalesce(sum(amount_micro), 0)::bigint from ledger_entries where account_id = $1",
    )
    .bind(user_cash.0)
    .fetch_one(store.pool_handle())
    .await
    .unwrap();
    assert_eq!(after_cash, before_cash, "voided progress must not pay +G");
    assert!(sqlx::query_scalar::<_, Option<OffsetDateTime>>(
        "select converted_at from credit_grant_lots where id = $1",
    )
    .bind(lot_id)
    .fetch_one(store.pool_handle())
    .await
    .unwrap()
    .is_none());
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "select kind from credit_fee_allocations where source_allocation_id = $1",
        )
        .bind(allocation_id)
        .fetch_one(store.pool_handle())
        .await
        .unwrap(),
        "reversed"
    );
}

#[tokio::test]
async fn pg_terminal_move_does_not_wait_on_the_conversion_lot_lock() {
    let Some(store) = pg_store().await else {
        return;
    };
    let _stateful_guard = stateful_money_config_guard().await;
    let fixture = market_fixture(&store).await;
    let (_, _, allocation_id) = seed_provisional_credit_allocation(&store, &fixture, 30_000).await;

    let mut convert = store.credit_convert_tx().await.unwrap();
    convert.lock_user(fixture.user).await.unwrap();
    convert.lock_credit_lots(fixture.user).await.unwrap();
    assert_eq!(convert.lots_for_user(fixture.user).await.unwrap().len(), 1);

    let mut reverse = store.unwind_tx().await.unwrap();
    let moved = timeout(std::time::Duration::from_millis(500), async {
        let moved = reverse
            .reverse_market_fee_allocations(fixture.market)
            .await?;
        reverse.commit().await?;
        Ok::<u32, StoreError>(moved)
    })
    .await
    .expect("terminal move must not wait on a non-key lot update")
    .unwrap();
    assert_eq!(moved, 1);

    let receipt = convert
        .convert_then_collect(
            fixture.user,
            OffsetDateTime::now_utc(),
            "convert-after-terminal-move",
        )
        .await
        .unwrap();
    convert.commit().await.unwrap();
    assert_eq!(receipt.converted_micro, 0);
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "select kind from credit_fee_allocations where source_allocation_id = $1",
        )
        .bind(allocation_id)
        .fetch_one(store.pool_handle())
        .await
        .unwrap(),
        "reversed"
    );
}

#[tokio::test]
async fn pg_referral_first_paid_market_grants_both_legs_once() {
    let Some(store) = pg_store().await else {
        return;
    };
    let _stateful_guard = stateful_money_config_guard().await;
    reset_config(&store).await;
    let fixture = market_fixture(&store).await;
    let referrer = UserId(Uuid::new_v4());
    sqlx::query("insert into users (id, handle) values ($1, $2)")
        .bind(referrer.0)
        .bind(format!("referrer-{}", Uuid::new_v4()))
        .execute(store.pool_handle())
        .await
        .unwrap();
    let now = OffsetDateTime::now_utc();
    seed_money_clear(&store, fixture.user, now).await;
    sqlx::query(
        r#"insert into phone_verifications
            (id, user_id, number_hmac, verified_at, provider_ref)
           values ($1, $2, $3, $4, 'test')"#,
    )
    .bind(Uuid::new_v4())
    .bind(fixture.user.0)
    .bind(format!("phone-{}", Uuid::new_v4()))
    .bind(now)
    .execute(store.pool_handle())
    .await
    .unwrap();
    let bind_id = Uuid::new_v4();
    sqlx::query(
        "insert into referral_binds (id, referrer_id, referee_id, bind_key) values ($1, $2, $3, $4)",
    )
    .bind(bind_id)
    .bind(referrer.0)
    .bind(fixture.user.0)
    .bind(format!("paid-bind-{}", Uuid::new_v4()))
    .execute(store.pool_handle())
    .await
    .unwrap();
    for (key, value) in [
        ("feature_referrals", serde_json::json!(true)),
        ("referral_min_notional_micro", serde_json::json!(10_000_000)),
        (
            "credit_referral_referrer_micro",
            serde_json::json!(5_000_000),
        ),
        (
            "credit_referral_referee_micro",
            serde_json::json!(5_000_000),
        ),
        (
            "bonus_mint_daily_cap_micro",
            serde_json::json!(10_000_000_000_i64),
        ),
        ("bonus_structure", serde_json::json!("real_money")),
    ] {
        sqlx::query("update config_entries set value = $2 where key = $1")
            .bind(key)
            .bind(value)
            .execute(store.pool_handle())
            .await
            .unwrap();
    }
    sqlx::query(
        r#"insert into votes
            (id, user_id, market_id, outcome_id, crowd_guess_pct, seq, idempotency_key)
           values ($1, $2, $3, $4, 50, 1, $5)"#,
    )
    .bind(Uuid::new_v4())
    .bind(fixture.user.0)
    .bind(fixture.market.0)
    .bind(fixture.yes_outcome)
    .bind(format!("referral-vote-{}", Uuid::new_v4()))
    .execute(store.pool_handle())
    .await
    .unwrap();

    let mut balances = store.credit_convert_tx().await.unwrap();
    let reserve_before = balances.bonus_reserve_balance().await.unwrap();
    let promised_before = balances.remaining_real_money_promise().await.unwrap();
    drop(balances);
    let reserve_topup = promised_before
        .checked_add(10_000_000)
        .unwrap()
        .saturating_sub(reserve_before)
        .max(0);
    let mut seed = store.credit_convert_tx().await.unwrap();
    let external = seed
        .account(OwnerRef::External, Currency::Usdc)
        .await
        .unwrap();
    let reserve = seed
        .account(OwnerRef::BonusReserve, Currency::Usdc)
        .await
        .unwrap();
    if reserve_topup > 0 {
        seed.ledger_apply(
            TxnKind::Seed,
            &format!("referral-reserve-{}", Uuid::new_v4()),
            &[
                Entry {
                    account: external,
                    amount: MicroUsd(-reserve_topup),
                },
                Entry {
                    account: reserve,
                    amount: MicroUsd(reserve_topup),
                },
            ],
        )
        .await
        .unwrap();
    }
    let external_credit = seed
        .account(OwnerRef::External, Currency::UsdcCredit)
        .await
        .unwrap();
    let house_credit = seed
        .account(OwnerRef::House, Currency::UsdcCredit)
        .await
        .unwrap();
    seed.ledger_apply(
        TxnKind::CreditGrant,
        &format!("referral-house-credit-{}", Uuid::new_v4()),
        &[
            Entry {
                account: external_credit,
                amount: MicroUsd(-10_000_000),
            },
            Entry {
                account: house_credit,
                amount: MicroUsd(10_000_000),
            },
        ],
    )
    .await
    .unwrap();
    let user_cash = seed
        .account(OwnerRef::User(fixture.user), Currency::Usdc)
        .await
        .unwrap();
    seed.ledger_apply(
        TxnKind::Deposit,
        &format!("referral-user-fund-{}", Uuid::new_v4()),
        &[
            Entry {
                account: external,
                amount: MicroUsd(-20_000_000),
            },
            Entry {
                account: user_cash,
                amount: MicroUsd(20_000_000),
            },
        ],
    )
    .await
    .unwrap();
    seed.commit().await.unwrap();

    PlaceTrade {
        store: &store,
        clock: &FixedClock(now),
        rep_config: application::model::RepConfig::default(),
    }
    .execute(PlaceTradeCmd {
        market: fixture.market,
        user: fixture.user,
        side: Side::Yes,
        action: TradeAction::Buy,
        amount_micro: 10_000_000,
        idempotency_key: format!("referral-trade-{}", Uuid::new_v4()),
        run_id: None,
        pending_action_id: None,
        expected_config_version: Some(1),
    })
    .await
    .unwrap();
    sqlx::query("update markets set status = 'paid', settled_at = $2 where id = $1")
        .bind(fixture.market.0)
        .bind(now)
        .execute(store.pool_handle())
        .await
        .unwrap();

    let mut paid_tx = store.resolve_tx().await.unwrap();
    let relevant = paid_tx
        .referral_relevant_users(fixture.market)
        .await
        .unwrap();
    assert!(relevant.contains(&referrer));
    assert!(relevant.contains(&fixture.user));
    for user in &relevant {
        paid_tx.lock_user(*user).await.unwrap();
    }
    assert_eq!(
        paid_tx
            .grant_referrals_on_paid(fixture.market)
            .await
            .unwrap(),
        1
    );
    paid_tx.commit().await.unwrap();

    let command = GrantReferralCmd {
        referee: fixture.user,
        paid_market: fixture.market,
        granted_at: now,
    };
    let first = GrantReferral { store: &store }
        .execute(command.clone())
        .await
        .unwrap();
    assert!(first.replayed);
    assert_eq!(first.bind_id, bind_id);
    let replay = GrantReferral { store: &store }
        .execute(command)
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.ledger_txn, first.ledger_txn);
    let lots: i64 = sqlx::query_scalar(
        "select count(*)::bigint from credit_grant_lots where idempotency_key in ($1, $2)",
    )
    .bind(format!("referral-grant:{bind_id}:referrer"))
    .bind(format!("referral-grant:{bind_id}:referee"))
    .fetch_one(store.pool_handle())
    .await
    .unwrap();
    assert_eq!(lots, 2);
}

#[tokio::test]
async fn pg_reciprocal_referral_bind_race_has_one_winner() {
    let Some(store) = pg_store().await else {
        return;
    };
    let users = [UserId(Uuid::new_v4()), UserId(Uuid::new_v4())];
    let now = OffsetDateTime::now_utc();
    for user in users {
        sqlx::query("insert into users (id, handle) values ($1, $2)")
            .bind(user.0)
            .bind(format!("reciprocal-referral-{}", user.0.simple()))
            .execute(store.pool_handle())
            .await
            .unwrap();
        sqlx::query(
            r#"insert into phone_verifications
                (id, user_id, number_hmac, verified_at, provider_ref)
               values ($1, $2, $3, $4, 'pg-contract')"#,
        )
        .bind(Uuid::new_v4())
        .bind(user.0)
        .bind(format!("phone-{}", Uuid::new_v4()))
        .bind(now)
        .execute(store.pool_handle())
        .await
        .unwrap();
    }

    let left = BindReferral { store: &store };
    let right = BindReferral { store: &store };
    let (a_to_b, b_to_a) = tokio::join!(
        left.execute(BindReferralCmd {
            referrer: users[0],
            referee: users[1],
            bind_key: format!("a-to-b-{}", Uuid::new_v4()),
            idempotency_key: format!("a-to-b-idem-{}", Uuid::new_v4()),
        }),
        right.execute(BindReferralCmd {
            referrer: users[1],
            referee: users[0],
            bind_key: format!("b-to-a-{}", Uuid::new_v4()),
            idempotency_key: format!("b-to-a-idem-{}", Uuid::new_v4()),
        }),
    );
    assert_eq!(usize::from(a_to_b.is_ok()) + usize::from(b_to_a.is_ok()), 1);
    assert_eq!(
        a_to_b.err().or_else(|| b_to_a.err()),
        Some(AppError::ReferralIneligible)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "select count(*)::bigint from referral_binds where referrer_id in ($1, $2) and referee_id in ($1, $2)",
        )
        .bind(users[0].0)
        .bind(users[1].0)
        .fetch_one(store.pool_handle())
        .await
        .unwrap(),
        1
    );
}

#[tokio::test]
async fn pg_deposit_binds_full_observation_and_refunds_only_to_source() {
    let Some(store) = pg_store().await else {
        return;
    };
    let user = UserId(Uuid::new_v4());
    let mut before_snapshot = store.invariant_read_tx().await.unwrap();
    let suspense_before = before_snapshot.suspense_liability_micro().await.unwrap();
    drop(before_snapshot);
    sqlx::query("insert into users (id, handle, kyc_tier) values ($1, $2, 0)")
        .bind(user.0)
        .bind(format!("deposit-refund-{}", Uuid::new_v4()))
        .execute(store.pool_handle())
        .await
        .unwrap();
    let now = OffsetDateTime::now_utc();
    let identity = application::ports::withdraw_fakes::test_rail();
    for context in ["geo", "sanctions"] {
        sqlx::query(
            r#"insert into sanction_screenings
                (id, user_id, context, verdict, checked_at, expires_at, policy_version)
               values ($1, $2, $3, 'clear', $4, $5, 'test')"#,
        )
        .bind(Uuid::new_v4())
        .bind(user.0)
        .bind(context)
        .bind(now)
        .bind(now + Duration::hours(1))
        .execute(store.pool_handle())
        .await
        .unwrap();
    }
    let observation = ObservedDeposit {
        user: Some(user),
        amount: MicroUsd(3_000_000),
        chain_sig: format!("deposit-sig-{}", Uuid::new_v4()),
        source_address: format!("source-{}", Uuid::new_v4()),
        dest_address: identity.treasury_token_account.clone(),
        mint: identity.usdc_mint.clone(),
        slot: 987_654,
    };
    let first = observe_finalized_on_rail(&store, &observation, &identity.fingerprint())
        .await
        .unwrap();
    assert!(!first.replayed);
    assert!(
        observe_finalized_on_rail(&store, &observation, &identity.fingerprint())
            .await
            .unwrap()
            .replayed
    );
    let mut conflicting = observation.clone();
    conflicting.slot += 1;
    assert!(matches!(
        observe_finalized_on_rail(&store, &conflicting, &identity.fingerprint()).await,
        Err(AppError::Store(StoreError::Conflict(
            "deposit observation binding"
        )))
    ));

    let held = admit_observed(
        &store,
        &observation.chain_sig,
        now,
        &AdminContext::Machine,
        false,
    )
    .await
    .unwrap();
    assert_eq!(held.deposit_id, first.deposit_id);
    let mut held_snapshot = store.invariant_read_tx().await.unwrap();
    assert_eq!(
        held_snapshot.suspense_liability_micro().await.unwrap(),
        suspense_before + observation.amount.0
    );
    drop(held_snapshot);
    let proposal = propose_deposit_command(
        &store,
        first.deposit_id,
        true,
        "return source-locked funds".into(),
        now,
        &AdminContext::Admin {
            token_digest: "finance-a".into(),
            role: AdminRole::Finance,
        },
    )
    .await
    .unwrap();
    let approved = confirm_deposit_refund(
        &store,
        proposal.id,
        now,
        &AdminContext::Admin {
            token_digest: "super-b".into(),
            role: AdminRole::Superadmin,
        },
    )
    .await
    .unwrap();
    assert_eq!(approved.dest, observation.source_address);
    let rails = application::ports::withdraw_fakes::FakeRails::default();
    let signer = application::ports::withdraw_send::FakeSigner::new(&format!(
        "deposit-refund-signature-{}",
        first.deposit_id.0
    ));
    begin_refund_send(&store, &observation.chain_sig, now, &rails, &signer)
        .await
        .unwrap();
    let receipt = application::ports::ChainReceipt {
        signature: signer.signature.clone(),
        mint: identity.usdc_mint.clone(),
        source: identity.treasury_token_account.clone(),
        dest_token_account: observation.source_address.clone(),
        delta_micro: observation.amount.0,
        commitment: "finalized".into(),
    };
    let settled = settle_refund(&store, &observation.chain_sig, now, &identity, &receipt)
        .await
        .unwrap();
    assert!(!settled.replayed);
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "select dest from outbound_payments where id = $1 and subject = 'deposit_refund'",
        )
        .bind(approved.payment_id)
        .fetch_one(store.pool_handle())
        .await
        .unwrap(),
        observation.source_address
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("select machine_status from deposits where id = $1")
            .bind(first.deposit_id.0)
            .fetch_one(store.pool_handle())
            .await
            .unwrap(),
        "refunded"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "select count(*)::bigint from outbound_send_attempts where payment_id = $1 and landing_state = 'finalized'",
        )
        .bind(approved.payment_id)
        .fetch_one(store.pool_handle())
        .await
        .unwrap(),
        1
    );
    let mut settled_snapshot = store.invariant_read_tx().await.unwrap();
    assert_eq!(
        settled_snapshot.suspense_liability_micro().await.unwrap(),
        suspense_before
    );
    assert!(!settled_snapshot
        .unpaired_payment_facts()
        .await
        .unwrap()
        .contains(&first.deposit_id.0));
}

#[tokio::test]
async fn pg_invariant_snapshot_attributes_exact_withdrawal_legs_and_attempt() {
    let Some(store) = pg_store().await else {
        return;
    };
    let user = UserId(Uuid::new_v4());
    sqlx::query("insert into users (id, handle) values ($1, $2)")
        .bind(user.0)
        .bind(format!("withdraw-attribution-{}", Uuid::new_v4()))
        .execute(store.pool_handle())
        .await
        .unwrap();
    let amount = MicroUsd(7_000_000);
    let mut ledger = store.trade_tx().await.unwrap();
    ledger
        .serialize_key(&format!("withdraw-attribution:{}", user.0))
        .await
        .unwrap();
    let external = ledger
        .account(OwnerRef::External, Currency::Usdc)
        .await
        .unwrap();
    let user_cash = ledger
        .account(OwnerRef::User(user), Currency::Usdc)
        .await
        .unwrap();
    let withheld = ledger
        .account(OwnerRef::Withheld, Currency::Usdc)
        .await
        .unwrap();
    ledger
        .ledger_apply(
            TxnKind::Deposit,
            &format!("withdraw-attribution-fund:{}", user.0),
            &[
                Entry {
                    account: external,
                    amount: MicroUsd(-amount.0),
                },
                Entry {
                    account: user_cash,
                    amount,
                },
            ],
        )
        .await
        .unwrap();
    let hold_tx = ledger
        .ledger_apply(
            TxnKind::Withdrawal,
            &format!("withdraw-attribution-hold:{}", user.0),
            &[
                Entry {
                    account: user_cash,
                    amount: MicroUsd(-amount.0),
                },
                Entry {
                    account: withheld,
                    amount,
                },
            ],
        )
        .await
        .unwrap();
    let settle_tx = ledger
        .ledger_apply(
            TxnKind::Withdrawal,
            &format!("withdraw-attribution-settle:{}", user.0),
            &[
                Entry {
                    account: withheld,
                    amount: MicroUsd(-amount.0),
                },
                Entry {
                    account: external,
                    amount,
                },
            ],
        )
        .await
        .unwrap();
    ledger.commit().await.unwrap();

    let withdrawal = Uuid::new_v4();
    sqlx::query(
        r#"insert into withdrawals
            (id, user_id, dest_address, amount_micro, status, review_state,
             send_state, hold_tx_id, settle_tx_id, request_fingerprint)
           values ($1, $2, 'dest', $3, 'settled', 'approved', 'finalized',
                   $4, $5, 'fixture')"#,
    )
    .bind(withdrawal)
    .bind(user.0)
    .bind(amount.0)
    .bind(hold_tx)
    .bind(settle_tx)
    .execute(store.pool_handle())
    .await
    .unwrap();
    let payment = Uuid::new_v4();
    sqlx::query(
        r#"insert into outbound_payments
            (id, subject, subject_id, dest, amount_micro, rail_fingerprint)
           values ($1, 'withdrawal', $2, 'dest', $3, 'rail')"#,
    )
    .bind(payment)
    .bind(withdrawal)
    .bind(amount.0)
    .execute(store.pool_handle())
    .await
    .unwrap();
    sqlx::query(
        r#"insert into outbound_send_attempts
            (id, payment_id, attempt_number, signed_tx_bytes, signature,
             last_valid_block_height, landing_state)
           values ($1, $2, 1, $3, $4, 9, 'finalized')"#,
    )
    .bind(Uuid::new_v4())
    .bind(payment)
    .bind(vec![1_u8, 2, 3])
    .bind(format!("withdraw-attribution-signature-{withdrawal}"))
    .execute(store.pool_handle())
    .await
    .unwrap();

    let mut snapshot = store.invariant_read_tx().await.unwrap();
    let rows = snapshot.withdrawal_attribution().await.unwrap();
    let row = rows
        .iter()
        .find(|row| row.withdrawal == withdrawal)
        .unwrap();
    assert!(!row.active);
    assert!(row.has_hold);
    assert!(!row.has_release);
    assert!(row.has_settle);
    assert_eq!(row.finalized_attempts, 1);
}

#[tokio::test]
async fn pg_legacy_admitted_deposit_replays_without_inventing_observation_metadata() {
    let Some(store) = pg_store().await else {
        return;
    };
    let user = UserId(Uuid::new_v4());
    sqlx::query("insert into users (id, handle) values ($1, $2)")
        .bind(user.0)
        .bind(format!("legacy-deposit-{}", Uuid::new_v4()))
        .execute(store.pool_handle())
        .await
        .unwrap();
    let amount = MicroUsd(1_000_000);
    let mut ledger = store.trade_tx().await.unwrap();
    let external = ledger
        .account(OwnerRef::External, Currency::Usdc)
        .await
        .unwrap();
    let user_cash = ledger
        .account(OwnerRef::User(user), Currency::Usdc)
        .await
        .unwrap();
    let legacy_tx = ledger
        .ledger_apply(
            TxnKind::Deposit,
            &format!("legacy-deposit:{}", user.0),
            &[
                Entry {
                    account: external,
                    amount: MicroUsd(-amount.0),
                },
                Entry {
                    account: user_cash,
                    amount,
                },
            ],
        )
        .await
        .unwrap();
    ledger.commit().await.unwrap();
    let signature = format!("legacy-sig-{}", Uuid::new_v4());
    sqlx::query(
        r#"insert into deposits
            (id, user_id, chain_sig, amount_micro, status, txn_id,
             admit_tx_id, machine_status)
           values ($1, $2, $3, $4, 'credited', $5, $5, 'admitted_legacy')"#,
    )
    .bind(Uuid::new_v4())
    .bind(user.0)
    .bind(&signature)
    .bind(amount.0)
    .bind(legacy_tx)
    .execute(store.pool_handle())
    .await
    .unwrap();

    let replay = admit_observed(
        &store,
        &signature,
        OffsetDateTime::now_utc(),
        &AdminContext::Machine,
        false,
    )
    .await
    .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.ledger_txn, Some(legacy_tx));
    let mut invariant = store.invariant_read_tx().await.unwrap();
    assert!(!invariant
        .unpaired_payment_facts()
        .await
        .unwrap()
        .contains(&replay.deposit_id.0));
}

#[tokio::test]
async fn pg_quarantined_legacy_deposit_is_excluded_from_machine_decoding() {
    let Some(store) = pg_store().await else {
        return;
    };
    let user = UserId(Uuid::new_v4());
    sqlx::query("insert into users (id, handle) values ($1, $2)")
        .bind(user.0)
        .bind(format!("quarantined-deposit-{}", Uuid::new_v4()))
        .execute(store.pool_handle())
        .await
        .unwrap();
    let signature = format!("quarantined-sig-{}", Uuid::new_v4());
    let deposit_id = Uuid::new_v4();
    sqlx::query(
        r#"insert into deposits
            (id, user_id, chain_sig, amount_micro, status, machine_status)
           values ($1, $2, $3, 1000000, 'seen', 'quarantined_legacy')"#,
    )
    .bind(deposit_id)
    .bind(user.0)
    .bind(&signature)
    .execute(store.pool_handle())
    .await
    .unwrap();

    let mut machine = store.deposit_admission_tx().await.unwrap();
    assert!(machine
        .deposit_machine_by_sig(&signature)
        .await
        .unwrap()
        .is_none());
    let mut invariant = store.invariant_read_tx().await.unwrap();
    assert!(!invariant
        .unpaired_payment_facts()
        .await
        .unwrap()
        .contains(&deposit_id));
}

#[tokio::test]
async fn pg_concurrent_grants_serialize_global_bonus_reserve_capacity() {
    let Some(store) = pg_store().await else {
        return;
    };
    let _stateful_guard = stateful_money_config_guard().await;
    let users = [UserId(Uuid::new_v4()), UserId(Uuid::new_v4())];
    for user in users {
        sqlx::query("insert into users (id, handle) values ($1, $2)")
            .bind(user.0)
            .bind(format!("reserve-race-{}", user.0.simple()))
            .execute(store.pool_handle())
            .await
            .unwrap();
    }

    let mut capacity = store.credit_convert_tx().await.unwrap();
    let reserve = capacity.bonus_reserve_balance().await.unwrap();
    let promised = capacity.remaining_real_money_promise().await.unwrap();
    capacity.commit().await.unwrap();
    let minimum_race_amount = 5_000_000_i64;
    let available = reserve.checked_sub(promised).unwrap();
    let top_up = (minimum_race_amount - available).max(0);
    let race_amount = available.checked_add(top_up).unwrap();
    assert!(race_amount > 0);

    let mut seed = store.credit_convert_tx().await.unwrap();
    let seed_key = format!("reserve-race-seed-{}", Uuid::new_v4());
    seed.serialize_key(&seed_key).await.unwrap();
    if top_up > 0 {
        let external = seed
            .account(OwnerRef::External, Currency::Usdc)
            .await
            .unwrap();
        let bonus_reserve = seed
            .account(OwnerRef::BonusReserve, Currency::Usdc)
            .await
            .unwrap();
        seed.ledger_apply(
            TxnKind::Seed,
            &format!("{seed_key}:cash"),
            &[
                Entry {
                    account: external,
                    amount: MicroUsd(-top_up),
                },
                Entry {
                    account: bonus_reserve,
                    amount: MicroUsd(top_up),
                },
            ],
        )
        .await
        .unwrap();
    }
    let house_credit_funding = race_amount.checked_mul(2).unwrap();
    let external_credit = seed
        .account(OwnerRef::External, Currency::UsdcCredit)
        .await
        .unwrap();
    let house_credit = seed
        .account(OwnerRef::House, Currency::UsdcCredit)
        .await
        .unwrap();
    seed.ledger_apply(
        TxnKind::Seed,
        &format!("{seed_key}:credit"),
        &[
            Entry {
                account: external_credit,
                amount: MicroUsd(-house_credit_funding),
            },
            Entry {
                account: house_credit,
                amount: MicroUsd(house_credit_funding),
            },
        ],
    )
    .await
    .unwrap();
    seed.commit().await.unwrap();

    let now = OffsetDateTime::now_utc();
    let command = |user: UserId, suffix: &str| GrantCreditCmd {
        user,
        amount: MicroUsd(race_amount),
        source: "reserve-race".into(),
        grant_class: GrantClass::RealMoney,
        policy_version: "test".into(),
        idempotency_key: format!("reserve-race-{suffix}-{}", Uuid::new_v4()),
        granted_at: now,
    };
    let left_grant = GrantCredit { store: &store };
    let right_grant = GrantCredit { store: &store };
    let (left, right) = tokio::join!(
        left_grant.execute(command(users[0], "left")),
        right_grant.execute(command(users[1], "right"))
    );
    assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
    let loser = left.err().or_else(|| right.err()).unwrap();
    assert!(matches!(loser, AppError::InsufficientBonusReserve { .. }));
    let mut coverage = store.credit_convert_tx().await.unwrap();
    assert!(
        coverage.bonus_reserve_balance().await.unwrap()
            >= coverage.remaining_real_money_promise().await.unwrap()
    );
    coverage.commit().await.unwrap();
}

#[tokio::test]
async fn pg_grant_top_up_and_fee_finalization_race_preserves_reserve_coverage() {
    let Some(store) = pg_store().await else {
        return;
    };
    let _stateful_guard = stateful_money_config_guard().await;
    let fixture = market_fixture(&store).await;
    let (_, _, allocation_id) = seed_provisional_credit_allocation(&store, &fixture, 10_000).await;
    let users = [UserId(Uuid::new_v4()), UserId(Uuid::new_v4())];
    for user in users {
        sqlx::query("insert into users (id, handle) values ($1, $2)")
            .bind(user.0)
            .bind(format!("grant-topup-race-{}", user.0.simple()))
            .execute(store.pool_handle())
            .await
            .unwrap();
    }
    sqlx::query(
        "update config_entries set value = '9000000000000000'::jsonb where key = 'bonus_mint_daily_cap_micro'",
    )
    .execute(store.pool_handle())
    .await
    .unwrap();

    let mut snapshot = store.credit_convert_tx().await.unwrap();
    let reserve = snapshot.bonus_reserve_balance().await.unwrap();
    let promise = snapshot.remaining_real_money_promise().await.unwrap();
    snapshot.commit().await.unwrap();
    let available = reserve.checked_sub(promise).unwrap();
    let top_up_micro = 1_000_i64;
    let race_amount = available.checked_add(top_up_micro).unwrap();

    let mut fund = store.credit_convert_tx().await.unwrap();
    let external_credit = fund
        .account(OwnerRef::External, Currency::UsdcCredit)
        .await
        .unwrap();
    let house_credit = fund
        .account(OwnerRef::House, Currency::UsdcCredit)
        .await
        .unwrap();
    fund.ledger_apply(
        TxnKind::Seed,
        &format!("grant-topup-house-credit-{}", Uuid::new_v4()),
        &[
            Entry {
                account: external_credit,
                amount: MicroUsd(-race_amount.checked_mul(2).unwrap()),
            },
            Entry {
                account: house_credit,
                amount: MicroUsd(race_amount.checked_mul(2).unwrap()),
            },
        ],
    )
    .await
    .unwrap();
    fund.commit().await.unwrap();

    let now = OffsetDateTime::now_utc();
    let keys = [
        format!("grant-topup-left-{}", Uuid::new_v4()),
        format!("grant-topup-right-{}", Uuid::new_v4()),
    ];
    let command = |user: UserId, key: &str| GrantCreditCmd {
        user,
        amount: MicroUsd(race_amount),
        source: "grant-topup-race".into(),
        grant_class: GrantClass::RealMoney,
        policy_version: "test".into(),
        idempotency_key: key.into(),
        granted_at: now,
    };
    let grant_left = GrantCredit { store: &store };
    let grant_right = GrantCredit { store: &store };
    let top_up = async {
        let mut tx = store.credit_convert_tx().await.unwrap();
        let external = tx.account(OwnerRef::External, Currency::Usdc).await?;
        let bonus_reserve = tx.account(OwnerRef::BonusReserve, Currency::Usdc).await?;
        tx.ledger_apply(
            TxnKind::Seed,
            &format!("grant-topup-reserve-{}", Uuid::new_v4()),
            &[
                Entry {
                    account: external,
                    amount: MicroUsd(-top_up_micro),
                },
                Entry {
                    account: bonus_reserve,
                    amount: MicroUsd(top_up_micro),
                },
            ],
        )
        .await?;
        tx.commit().await
    };
    let finalize = async {
        let mut tx = store.resolve_tx().await.unwrap();
        let moved = tx.finalize_market_fee_allocations(fixture.market).await?;
        tx.commit().await?;
        Ok::<u32, StoreError>(moved)
    };
    let (_, _, top_up_result, finalized) = tokio::join!(
        grant_left.execute(command(users[0], &keys[0])),
        grant_right.execute(command(users[1], &keys[1])),
        top_up,
        finalize,
    );
    top_up_result.unwrap();
    assert_eq!(finalized.unwrap(), 1);

    let _ = tokio::join!(
        grant_left.execute(command(users[0], &keys[0])),
        grant_right.execute(command(users[1], &keys[1])),
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "select count(*)::bigint from credit_grant_lots where idempotency_key in ($1, $2)",
        )
        .bind(&keys[0])
        .bind(&keys[1])
        .fetch_one(store.pool_handle())
        .await
        .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "select kind from credit_fee_allocations where source_allocation_id = $1",
        )
        .bind(allocation_id)
        .fetch_one(store.pool_handle())
        .await
        .unwrap(),
        "finalized"
    );
    let mut coverage = store.credit_convert_tx().await.unwrap();
    assert!(
        coverage.bonus_reserve_balance().await.unwrap()
            >= coverage.remaining_real_money_promise().await.unwrap()
    );
    coverage.commit().await.unwrap();
}

#[tokio::test]
async fn pg_passes_seeded_settlement_io_contract() {
    let Some(store) = pg_store().await else {
        return;
    };
    // The generic suite deliberately exercises the deterministic EnsureGenesis key.
    // A persistent adapter test database may have replayed and then spent that genesis
    // in an earlier run, so give this test its own balanced house-capital fixture.
    let mut fixture_tx = store.bootstrap_tx().await.unwrap();
    let key = format!("settlement-fixture-{}", Uuid::new_v4());
    fixture_tx.serialize_key(&key).await.unwrap();
    let external = fixture_tx
        .account(OwnerRef::External, Currency::Usdc)
        .await
        .unwrap();
    let house = fixture_tx
        .account(OwnerRef::House, Currency::Usdc)
        .await
        .unwrap();
    fixture_tx
        .ledger_apply(
            TxnKind::Deposit,
            &key,
            &[
                Entry {
                    account: external,
                    amount: MicroUsd(-2_000_000),
                },
                Entry {
                    account: house,
                    amount: MicroUsd(2_000_000),
                },
            ],
        )
        .await
        .unwrap();
    fixture_tx.commit().await.unwrap();
    seeded_settlement_io_contract(&store).await;
}

#[tokio::test]
async fn pg_passes_integrity_report_contract() {
    let Some(store) = pg_store().await else {
        return;
    };
    integrity_report_contract(&store).await;
}

#[tokio::test]
async fn pg_passes_comment_writer_contract() {
    let Some(store) = pg_store().await else {
        return;
    };
    let fixture = market_fixture(&store).await;
    let voter = UserId(Uuid::new_v4());
    sqlx::query("insert into users (id, handle) values ($1, $2)")
        .bind(voter.0)
        .bind(format!("comment-voter-{}", Uuid::new_v4()))
        .execute(store.pool_handle())
        .await
        .unwrap();
    sqlx::query("insert into reputation (user_id, rep_micro, tier) values ($1, 0, 1)")
        .bind(voter.0)
        .execute(store.pool_handle())
        .await
        .unwrap();
    comment_writer_contract(
        &store,
        fixture.market,
        fixture.user,
        voter,
        OffsetDateTime::now_utc(),
    )
    .await;
}

#[tokio::test]
async fn pg_social_read_models_preserve_shadow_scope_and_reporters() {
    let Some(store) = pg_store().await else {
        return;
    };
    let fixture = market_fixture(&store).await;
    sqlx::query("update users set created_at = $2 where id = $1")
        .bind(fixture.user.0)
        .bind(OffsetDateTime::now_utc() - Duration::days(10))
        .execute(store.pool_handle())
        .await
        .unwrap();
    let comment = CommentId(Uuid::new_v4());
    PostComment {
        store: &store,
        clock: &FixedClock(OffsetDateTime::now_utc()),
        config: SocialConfig::default(),
    }
    .execute(PostCommentCmd {
        comment,
        market: fixture.market,
        author: fixture.user,
        parent: None,
        body: "pg social comment".to_string(),
    })
    .await
    .unwrap();
    assert_eq!(
        store
            .comment_view(comment, Some(fixture.user))
            .await
            .unwrap()
            .row
            .id,
        comment
    );
    let reporter = UserId(Uuid::new_v4());
    sqlx::query("insert into users (id, handle, created_at) values ($1,$2,$3)")
        .bind(reporter.0)
        .bind(format!("reporter-{}", reporter.0.simple()))
        .bind(OffsetDateTime::now_utc() - Duration::days(10))
        .execute(store.pool_handle())
        .await
        .unwrap();
    sqlx::query("insert into reputation (user_id, rep_micro, tier) values ($1,0,1)")
        .bind(reporter.0)
        .execute(store.pool_handle())
        .await
        .unwrap();
    ReportComment {
        store: &store,
        clock: &FixedClock(OffsetDateTime::now_utc()),
        config: SocialConfig {
            report_shadow_threshold: 2,
            ..SocialConfig::default()
        },
    }
    .execute(ReportCommentCmd { comment, reporter })
    .await
    .unwrap();
    sqlx::query("update comments set moderation_status='shadow' where id=$1")
        .bind(comment.0)
        .execute(store.pool_handle())
        .await
        .unwrap();
    assert!(store.comment_view(comment, None).await.is_err());
    let rows = store.reported_comments(2, 10).await.unwrap();
    let row = rows
        .iter()
        .find(|row| row.comment.row.id == comment)
        .unwrap();
    assert_eq!(row.report_count, 1);
    assert_eq!(row.reporters.len(), 1);
}

struct FixedClock(OffsetDateTime);

impl Clock for FixedClock {
    fn now(&self) -> OffsetDateTime {
        self.0
    }
}

struct Fixture {
    market: MarketId,
    user: UserId,
    yes_outcome: Uuid,
    slug: String,
}

async fn market_fixture(store: &PgStore) -> Fixture {
    let market = MarketId(Uuid::new_v4());
    let user = UserId(Uuid::new_v4());
    let yes = Uuid::new_v4();
    let no = Uuid::new_v4();
    let pool = Uuid::new_v4();
    let slug = format!("pg-contract-{}", Uuid::new_v4());
    let now = OffsetDateTime::now_utc();
    sqlx::query("insert into users (id, handle) values ($1, $2)")
        .bind(user.0)
        .bind(format!("user-{}", Uuid::new_v4()))
        .execute(store.pool_handle())
        .await
        .unwrap();
    sqlx::query("insert into reputation (user_id, rep_micro, tier) values ($1, 0, 0)")
        .bind(user.0)
        .execute(store.pool_handle())
        .await
        .unwrap();
    sqlx::query(
        r#"
        insert into markets
            (id, slug, question, status, min_votes_to_resolve, opens_at, closes_at, tally_hidden_at)
        values ($1, $2, 'contract?', 'live', 3, $3, $4, $5)
        "#,
    )
    .bind(market.0)
    .bind(&slug)
    .bind(now - Duration::hours(1))
    .bind(now + Duration::hours(2))
    .bind(now + Duration::hours(1))
    .execute(store.pool_handle())
    .await
    .unwrap();
    sqlx::query(
        "insert into outcomes (id, market_id, label, idx) values ($1, $3, 'YES', 0), ($2, $3, 'NO', 1)",
    )
    .bind(yes)
    .bind(no)
    .bind(market.0)
    .execute(store.pool_handle())
    .await
    .unwrap();
    sqlx::query(
        "insert into pools (id, market_id, fee_bps, seeded_micro) values ($1, $2, 100, 1000000000)",
    )
    .bind(pool)
    .bind(market.0)
    .execute(store.pool_handle())
    .await
    .unwrap();
    sqlx::query(
        r#"
        insert into pool_reserves (pool_id, outcome_id, market_id, reserve_micro_shares)
        values ($1, $2, $4, 1000000000), ($1, $3, $4, 1000000000)
        "#,
    )
    .bind(pool)
    .bind(yes)
    .bind(no)
    .bind(market.0)
    .execute(store.pool_handle())
    .await
    .unwrap();
    Fixture {
        market,
        user,
        yes_outcome: yes,
        slug,
    }
}

#[tokio::test]
async fn market_queries_and_trade_roles_round_trip() {
    let Some(store) = pg_store().await else {
        return;
    };
    let _stateful_guard = stateful_money_config_guard().await;
    reset_config(&store).await;
    let fixture = market_fixture(&store).await;
    seed_money_clear(&store, fixture.user, OffsetDateTime::now_utc()).await;
    let rep = store.user_rep(fixture.user).await.unwrap();
    assert_eq!((rep.rep_micro, rep.tier), (0, 0));
    assert_eq!(
        store
            .last_buy_at(
                fixture.user,
                application::model::OutcomeId(fixture.yes_outcome),
            )
            .await
            .unwrap(),
        None
    );
    let id_row = store
        .market_by_ref(&fixture.market.0.to_string())
        .await
        .unwrap();
    let slug_row = store.market_by_ref(&fixture.slug).await.unwrap();
    assert_eq!(id_row, slug_row);
    assert!(store
        .list_markets(Some("live"))
        .await
        .unwrap()
        .iter()
        .any(|row| row.id == fixture.market));
    assert_eq!(
        store.pool(fixture.market).await.unwrap().market,
        fixture.market
    );
    assert!(!store
        .user_voted(fixture.user, fixture.market)
        .await
        .unwrap());

    sqlx::query(
        r#"
        insert into votes
            (id, user_id, market_id, outcome_id, crowd_guess_pct, seq, idempotency_key)
        values ($1, $2, $3, $4, 50, 1, $5)
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(fixture.user.0)
    .bind(fixture.market.0)
    .bind(fixture.yes_outcome)
    .bind(format!("vote-{}", Uuid::new_v4()))
    .execute(store.pool_handle())
    .await
    .unwrap();
    let address = format!("+1{}", fixture.user.0.as_u128());
    sqlx::query(
        "insert into user_channels (user_id, channel, address) values ($1, 'imessage', $2)",
    )
    .bind(fixture.user.0)
    .bind(&address)
    .execute(store.pool_handle())
    .await
    .unwrap();
    assert!(store
        .user_voted(fixture.user, fixture.market)
        .await
        .unwrap());
    assert_eq!(
        store.user_by_channel("imessage", &address).await.unwrap(),
        Some(fixture.user)
    );

    let mut fund = store.trade_tx().await.unwrap();
    let fund_key = format!("fund-{}", Uuid::new_v4());
    fund.serialize_key(&fund_key).await.unwrap();
    let external = fund
        .account(OwnerRef::External, Currency::Usdc)
        .await
        .unwrap();
    let user_account = fund
        .account(OwnerRef::User(fixture.user), Currency::Usdc)
        .await
        .unwrap();
    fund.ledger_apply(
        TxnKind::Deposit,
        &fund_key,
        &[
            Entry {
                account: external,
                amount: MicroUsd(-50_000_000),
            },
            Entry {
                account: user_account,
                amount: MicroUsd(50_000_000),
            },
        ],
    )
    .await
    .unwrap();
    fund.commit().await.unwrap();

    let config_generation: i64 =
        sqlx::query_scalar("select generation from config_generation where singleton = 1")
            .fetch_one(store.pool_handle())
            .await
            .unwrap();
    let key = format!("trade-{}", Uuid::new_v4());
    let cmd = PlaceTradeCmd {
        market: fixture.market,
        user: fixture.user,
        side: Side::Yes,
        action: TradeAction::Buy,
        amount_micro: 5_000_000,
        idempotency_key: key,
        run_id: None,
        pending_action_id: None,
        expected_config_version: Some(config_generation),
    };
    let use_case = PlaceTrade {
        store: &store,
        clock: &FixedClock(OffsetDateTime::now_utc()),
        rep_config: application::model::RepConfig::default(),
    };
    let first = use_case.execute(cmd.clone()).await.unwrap();
    let replay = use_case.execute(cmd).await.unwrap();
    assert_eq!(first.trade_id, replay.trade_id);
    assert!(replay.replayed);
    let positions = store.positions(fixture.user).await.unwrap();
    assert_eq!(positions.len(), 1);
    assert!(positions[0].shares.0 > 0);
    assert_eq!(positions[0].rep_micro, 0);
    assert_eq!(positions[0].tier, 0);
    let holders = store.holders(fixture.market, 10).await.unwrap();
    assert_eq!(holders.yes.len(), 1);
    assert_eq!(holders.yes[0].user, fixture.user);
    assert!(holders.no.is_empty());
    let profile = store.user_profile(fixture.user).await.unwrap();
    assert_eq!(profile.recent_trades.len(), 1);
    assert_eq!(profile.recent_trades[0].action, TradeAction::Buy);
    assert_eq!(profile.recent_votes.len(), 1);
    assert_eq!(profile.recent_votes[0].side, None);
    assert_eq!(profile.avg_score_bp, None);
    assert!(store
        .last_buy_at(
            fixture.user,
            application::model::OutcomeId(fixture.yes_outcome),
        )
        .await
        .unwrap()
        .is_some());
    let mut read_tx = store.trade_tx().await.unwrap();
    read_tx
        .serialize_key(&format!("economy-read-{}", Uuid::new_v4()))
        .await
        .unwrap();
    assert!(read_tx
        .last_buy_at(
            fixture.user,
            application::model::OutcomeId(fixture.yes_outcome),
        )
        .await
        .unwrap()
        .is_some());
    let outbox_count: i64 =
        sqlx::query("select count(*)::bigint as n from events_outbox where aggregate_id = $1")
            .bind(fixture.market.0)
            .fetch_one(store.pool_handle())
            .await
            .unwrap()
            .try_get("n")
            .unwrap();
    assert_eq!(outbox_count, 1);
}

#[tokio::test]
async fn pg_economy_queries_use_immutable_facts_and_split_fee_sources() {
    let Some(store) = pg_store().await else {
        return;
    };
    let fixture = market_fixture(&store).await;
    let unique_second = i64::try_from(fixture.market.0.as_u128() % 1_000_000_000).unwrap();
    let at = OffsetDateTime::from_unix_timestamp(unique_second).unwrap();
    let mut write = store.trade_tx().await.unwrap();
    let key = format!("economy-facts-{}", Uuid::new_v4());
    write.serialize_key(&key).await.unwrap();
    let external = write
        .account(OwnerRef::External, Currency::Usdc)
        .await
        .unwrap();
    let fees = write.account(OwnerRef::Fees, Currency::Usdc).await.unwrap();
    let txn = write
        .ledger_apply(
            TxnKind::Trade,
            &key,
            &[
                Entry {
                    account: external,
                    amount: MicroUsd(-7),
                },
                Entry {
                    account: fees,
                    amount: MicroUsd(7),
                },
            ],
        )
        .await
        .unwrap();
    assert!(write
        .insert_realization(&application::model::RealizationFact {
            user: fixture.user,
            market: fixture.market,
            outcome: application::model::OutcomeId(fixture.yes_outcome),
            source: application::model::RealizationSource::Sell,
            realized_delta: MicroUsd(42),
            payout: MicroUsd(52),
            ledger_txn: txn,
            created_at: at,
        })
        .await
        .unwrap());
    assert!(!write
        .insert_realization(&application::model::RealizationFact {
            user: fixture.user,
            market: fixture.market,
            outcome: application::model::OutcomeId(fixture.yes_outcome),
            source: application::model::RealizationSource::Sell,
            realized_delta: MicroUsd(999),
            payout: MicroUsd(1_009),
            ledger_txn: txn,
            created_at: at,
        })
        .await
        .unwrap());
    write.commit().await.unwrap();
    sqlx::query("update ledger_transactions set created_at = $2 where id = $1")
        .bind(txn)
        .bind(at)
        .execute(store.pool_handle())
        .await
        .unwrap();
    sqlx::query("update reputation set tier = 1 where user_id = $1")
        .bind(fixture.user.0)
        .execute(store.pool_handle())
        .await
        .unwrap();
    let vote = Uuid::new_v4();
    sqlx::query(
        r#"insert into votes
               (id, user_id, market_id, outcome_id, crowd_guess_pct, seq, idempotency_key)
           values ($1, $2, $3, $4, 50, 1, $5)"#,
    )
    .bind(vote)
    .bind(fixture.user.0)
    .bind(fixture.market.0)
    .bind(fixture.yes_outcome)
    .bind(format!("economy-score-{vote}"))
    .execute(store.pool_handle())
    .await
    .unwrap();
    sqlx::query(
        "insert into vote_scores (vote_id, accuracy_bp, majority_bp, score_bp, created_at) values ($1, 8000, 9000, 8250, $2)",
    )
    .bind(vote)
    .bind(at)
    .execute(store.pool_handle())
    .await
    .unwrap();

    let mut payout = store.resolve_tx().await.unwrap();
    let payout_key = format!("economy-dust-{}", Uuid::new_v4());
    payout.serialize_key(&payout_key).await.unwrap();
    let external = payout
        .account(OwnerRef::External, Currency::Usdc)
        .await
        .unwrap();
    let fees = payout
        .account(OwnerRef::Fees, Currency::Usdc)
        .await
        .unwrap();
    let payout_txn = payout
        .ledger_apply(
            TxnKind::Payout,
            &payout_key,
            &[
                Entry {
                    account: external,
                    amount: MicroUsd(-3),
                },
                Entry {
                    account: fees,
                    amount: MicroUsd(3),
                },
            ],
        )
        .await
        .unwrap();
    payout.commit().await.unwrap();
    sqlx::query("update ledger_transactions set created_at = $2 where id = $1")
        .bind(payout_txn)
        .bind(at)
        .execute(store.pool_handle())
        .await
        .unwrap();

    let until = at + Duration::seconds(1);
    let traders = store.top_traders(at, until, 10).await.unwrap();
    assert_eq!(traders.len(), 1);
    assert_eq!(traders[0].realized_pnl_micro, 42);
    assert_eq!(traders[0].realizations, 1);
    assert!(store
        .top_traders(until, until, 10)
        .await
        .unwrap()
        .is_empty());
    let voters = store.top_voters(at, until, 10, 1).await.unwrap();
    assert_eq!(voters.len(), 1);
    assert_eq!(voters[0].avg_score_bp, 8_250);
    assert_eq!(voters[0].markets_scored, 1);
    assert_eq!(voters[0].tier, 1);
    assert!(store.top_voters(at, until, 10, 2).await.unwrap().is_empty());
    assert_eq!(
        store.fee_summary(at, until).await.unwrap(),
        vec![application::model::DailyFeeRow {
            day: at.date(),
            trade_fee_micro: 7,
            payout_dust_micro: 3,
            total_micro: 10,
        }]
    );
}

#[tokio::test]
async fn thousand_voter_settlement_is_bulk_and_finishes_under_two_seconds() {
    let Some(store) = pg_store().await else {
        return;
    };
    let mut capital = store.bootstrap_tx().await.unwrap();
    let capital_key = format!("rep-perf-capital-{}", Uuid::new_v4());
    capital.serialize_key(&capital_key).await.unwrap();
    let external = capital
        .account(OwnerRef::External, Currency::Usdc)
        .await
        .unwrap();
    let house = capital
        .account(OwnerRef::House, Currency::Usdc)
        .await
        .unwrap();
    capital
        .ledger_apply(
            TxnKind::Deposit,
            &capital_key,
            &[
                Entry {
                    account: external,
                    amount: MicroUsd(-2_000_000_000),
                },
                Entry {
                    account: house,
                    amount: MicroUsd(2_000_000_000),
                },
            ],
        )
        .await
        .unwrap();
    capital.commit().await.unwrap();

    let market = MarketId(Uuid::new_v4());
    let now = OffsetDateTime::now_utc();
    let clock = FixedClock(now);
    SeedMarket {
        store: &store,
        clock: &clock,
        rep_config: application::model::RepConfig::default(),
        lp_kill_config: application::model::LpKillConfig::default(),
    }
    .execute(SeedMarketCmd {
        market_id: market,
        slug: format!("rep-perf-{market:?}"),
        min_votes_to_resolve: 1_000,
        closes_at: now + Duration::hours(2),
        tally_hidden_at: now + Duration::hours(1),
        fee: BasisPoints(100),
        seed: MicroUsd(1_000_000_000),
        idempotency_key: format!("rep-perf-seed-{market:?}"),
        force: false,
    })
    .await
    .unwrap();
    AdvanceMarket { store: &store }
        .execute(AdvanceMarketCmd {
            market,
            event: domain::market::MarketEvent::GoLive,
            idempotency_key: format!("rep-perf-live-{market:?}"),
        })
        .await
        .unwrap();
    let yes: Uuid = sqlx::query_scalar("select id from outcomes where market_id = $1 and idx = 0")
        .bind(market.0)
        .fetch_one(store.pool_handle())
        .await
        .unwrap();
    sqlx::query(
        r#"with voters as materialized (
               select gen_random_uuid() as id, n from generate_series(1, 1000) n
           ), users_written as (
               insert into users (id, handle)
               select id, 'perf-' || id::text from voters
           ), reps_written as (
               insert into reputation (user_id, rep_micro, tier)
               select id, 0, 0 from voters
           )
           insert into votes
               (id, user_id, market_id, outcome_id, crowd_guess_pct, seq, idempotency_key)
           select gen_random_uuid(), id, $1, $2, 75, n, 'perf-vote-' || id::text
             from voters"#,
    )
    .bind(market.0)
    .bind(yes)
    .execute(store.pool_handle())
    .await
    .unwrap();
    for (event, label) in [
        (domain::market::MarketEvent::EnterCloseWindow, "closing"),
        (domain::market::MarketEvent::Close, "closed"),
    ] {
        AdvanceMarket { store: &store }
            .execute(AdvanceMarketCmd {
                market,
                event,
                idempotency_key: format!("rep-perf-{label}-{market:?}"),
            })
            .await
            .unwrap();
    }
    let started = std::time::Instant::now();
    let clock = FixedClock(now);
    ResolveMarket {
        crash_point: &NoopCrashPoint,
        actor: AdminContext::Machine,
        store: &store,
        clock: &clock,
        config: application::model::ResolveConfig {
            oi_floor: MicroUsd(0),
        },
        rep_config: application::model::RepConfig::default(),
        integrity_config: application::model::IntegritySweepConfig::default(),
    }
    .execute(ResolveMarketCmd {
        market,
        curator_override: None,
    })
    .await
    .unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    let scored: i64 = sqlx::query_scalar(
        "select count(*)::bigint from vote_scores s join votes v on v.id = s.vote_id where v.market_id = $1",
    )
    .bind(market.0)
    .fetch_one(store.pool_handle())
    .await
    .unwrap();
    assert_eq!(scored, 1_000);
}

#[tokio::test]
async fn lp_kill_switch_serializes_concurrent_seeds_and_force_is_audited() {
    let Some(store) = pg_store().await else {
        return;
    };
    let mut capital = store.bootstrap_tx().await.unwrap();
    let capital_key = format!("lp-kill-capital-{}", Uuid::new_v4());
    capital.serialize_key(&capital_key).await.unwrap();
    let external = capital
        .account(OwnerRef::External, Currency::Usdc)
        .await
        .unwrap();
    let house = capital
        .account(OwnerRef::House, Currency::Usdc)
        .await
        .unwrap();
    capital
        .ledger_apply(
            TxnKind::Deposit,
            &capital_key,
            &[
                Entry {
                    account: external,
                    amount: MicroUsd(-10_000_000),
                },
                Entry {
                    account: house,
                    amount: MicroUsd(10_000_000),
                },
            ],
        )
        .await
        .unwrap();
    capital.commit().await.unwrap();

    let now = OffsetDateTime::now_utc();
    let prior = MarketId(Uuid::new_v4());
    let clock = FixedClock(now);
    let permissive = SeedMarket {
        store: &store,
        clock: &clock,
        rep_config: application::model::RepConfig::default(),
        lp_kill_config: application::model::LpKillConfig::default(),
    };
    permissive
        .execute(SeedMarketCmd {
            market_id: prior,
            slug: format!("lp-prior-{prior:?}"),
            min_votes_to_resolve: 1,
            closes_at: now + Duration::hours(2),
            tally_hidden_at: now + Duration::hours(1),
            fee: BasisPoints(100),
            seed: MicroUsd(1_000_000),
            idempotency_key: format!("lp-prior-{prior:?}"),
            force: false,
        })
        .await
        .unwrap();
    sqlx::query(
        "update markets set status = 'paid', lp_pnl_micro = -100, settled_at = $2 where id = $1",
    )
    .bind(prior.0)
    .bind(now - Duration::days(1))
    .execute(store.pool_handle())
    .await
    .unwrap();

    let breaker = SeedMarket {
        store: &store,
        clock: &clock,
        rep_config: application::model::RepConfig::default(),
        lp_kill_config: application::model::LpKillConfig {
            max_loss_micro: 100,
            window_days: 7,
        },
    };
    let command = |suffix: &str, force| SeedMarketCmd {
        market_id: MarketId(Uuid::new_v4()),
        slug: format!("lp-new-{suffix}-{}", Uuid::new_v4()),
        min_votes_to_resolve: 1,
        closes_at: now + Duration::hours(2),
        tally_hidden_at: now + Duration::hours(1),
        fee: BasisPoints(100),
        seed: MicroUsd(1_000_000),
        idempotency_key: format!("lp-new-{suffix}-{}", Uuid::new_v4()),
        force,
    };
    let (a, b) = tokio::join!(
        breaker.execute(command("a", false)),
        breaker.execute(command("b", false))
    );
    assert_eq!(a.unwrap_err(), AppError::LpPaused);
    assert_eq!(b.unwrap_err(), AppError::LpPaused);

    let forced = command("forced", true);
    let forced_market = forced.market_id;
    breaker.execute(forced).await.unwrap();
    let audited: bool = sqlx::query_scalar(
        "select exists(select 1 from events_outbox where aggregate_id = $1 and event_type = 'LpKillOverride')",
    )
    .bind(forced_market.0)
    .fetch_one(store.pool_handle())
    .await
    .unwrap();
    assert!(audited);
}

#[tokio::test]
async fn pool_for_update_blocks_a_second_locker() {
    let Some(store) = pg_store().await else {
        return;
    };
    let fixture = market_fixture(&store).await;
    let mut first = store.trade_tx().await.unwrap();
    first.pool_for_update(fixture.market).await.unwrap();

    let second_store = store.clone();
    let market = fixture.market;
    let waiter = tokio::spawn(async move {
        let mut second = second_store.trade_tx().await.unwrap();
        second.pool_for_update(market).await.unwrap();
        second.commit().await.unwrap();
    });
    sleep(std::time::Duration::from_millis(100)).await;
    assert!(!waiter.is_finished(), "second pool locker must block");
    first.commit().await.unwrap();
    timeout(std::time::Duration::from_secs(3), waiter)
        .await
        .expect("second locker remained blocked")
        .unwrap();
}

#[tokio::test]
async fn account_owner_shape_and_identity_immutability_are_enforced() {
    let Some(store) = pg_store().await else {
        return;
    };
    let invalid = sqlx::query(
        "insert into ledger_accounts (owner_type, owner_id, currency) values ('user', null, 'usdc')",
    )
    .execute(store.pool_handle())
    .await;
    assert!(invalid.is_err(), "owned account without owner_id must fail");

    let account = Uuid::new_v4();
    sqlx::query(
        "insert into ledger_accounts (id, owner_type, owner_id, currency) values ($1, 'user', $2, 'usdc')",
    )
    .bind(account)
    .bind(Uuid::new_v4())
    .execute(store.pool_handle())
    .await
    .unwrap();
    let reclassify =
        sqlx::query("update ledger_accounts set currency = 'usdc_credit' where id = $1")
            .bind(account)
            .execute(store.pool_handle())
            .await;
    assert!(reclassify.is_err(), "account identity must be immutable");
}

#[tokio::test]
async fn ledger_rejects_cross_currency_netting_before_write() {
    let Some(store) = pg_store().await else {
        return;
    };
    let mut tx = store.trade_tx().await.unwrap();
    let key = format!("cross-currency-{}", Uuid::new_v4());
    tx.serialize_key(&key).await.unwrap();
    let cash = tx
        .account(OwnerRef::External, Currency::Usdc)
        .await
        .unwrap();
    let credit = tx
        .account(OwnerRef::User(UserId(Uuid::new_v4())), Currency::UsdcCredit)
        .await
        .unwrap();
    let result = tx
        .ledger_apply(
            TxnKind::CreditConvert,
            &key,
            &[
                Entry {
                    account: cash,
                    amount: MicroUsd(-5),
                },
                Entry {
                    account: credit,
                    amount: MicroUsd(5),
                },
            ],
        )
        .await;
    assert!(
        matches!(result, Err(StoreError::Ledger(_))),
        "cross-currency netting must be rejected, got {result:?}"
    );
}

#[tokio::test]
async fn concurrent_vote_seq_allocations_are_distinct_and_consecutive() {
    let Some(store) = pg_store().await else {
        return;
    };
    let fixture = market_fixture(&store).await;
    let market = fixture.market;
    let allocate = |store: PgStore| async move {
        let mut tx = store.vote_tx().await.unwrap();
        let seq = tx.allocate_vote_seq(market).await.unwrap();
        tx.commit().await.unwrap();
        seq
    };
    let (a, b) = tokio::join!(allocate(store.clone()), allocate(store));
    assert_ne!(a, b);
    assert_eq!(a.abs_diff(b), 1);
}

#[tokio::test]
async fn resolution_role_persists_outcome_score_and_state() {
    let Some(store) = pg_store().await else {
        return;
    };
    let fixture = market_fixture(&store).await;
    let vote = Uuid::new_v4();
    sqlx::query(
        r#"
        insert into votes
            (id, user_id, market_id, outcome_id, crowd_guess_pct, seq, idempotency_key)
        values ($1, $2, $3, $4, 62, 1, $5)
        "#,
    )
    .bind(vote)
    .bind(fixture.user.0)
    .bind(fixture.market.0)
    .bind(fixture.yes_outcome)
    .bind(format!("resolution-vote-{}", Uuid::new_v4()))
    .execute(store.pool_handle())
    .await
    .unwrap();

    let mut tx = store.resolve_tx().await.unwrap();
    tx.serialize_key(&format!("resolution-role-{}", Uuid::new_v4()))
        .await
        .unwrap();
    let facts = tx.vote_facts(fixture.market).await.unwrap();
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].vote_id, vote);
    assert_eq!(facts[0].side, Side::Yes);
    assert_eq!(facts[0].crowd_guess_pct, 62);
    tx.save_vote_scores(&[application::model::VoteScoreUpdate {
        vote_id: vote,
        score: domain::scoring::VoteScore {
            accuracy_bp: 8_000,
            majority_bp: 10_000,
            score_bp: 8_500,
        },
    }])
    .await
    .unwrap();
    tx.write_outcome_resolution(
        application::model::OutcomeId(fixture.yes_outcome),
        6_000,
        MicroUsd(600_000),
    )
    .await
    .unwrap();
    tx.set_market_state(fixture.market, MarketState::Resolving)
        .await
        .unwrap();
    // D27 identity 5: the collateral fact is a REQUIRED same-transaction
    // write with an affected-row contract.
    tx.set_collateral_at_close(fixture.market, MicroUsd(123_456))
        .await
        .unwrap();
    assert_eq!(
        tx.set_collateral_at_close(MarketId(Uuid::new_v4()), MicroUsd(1))
            .await
            .unwrap_err(),
        StoreError::NotFound("market")
    );
    tx.commit().await.unwrap();

    let row = sqlx::query(
        r#"
        select m.status, m.collateral_at_close_micro, o.final_vote_bps,
               o.redemption_micro, vs.score_bp
          from markets m
          join outcomes o on o.market_id = m.id and o.id = $2
          join vote_scores vs on vs.vote_id = $3
         where m.id = $1
        "#,
    )
    .bind(fixture.market.0)
    .bind(fixture.yes_outcome)
    .bind(vote)
    .fetch_one(store.pool_handle())
    .await
    .unwrap();
    assert_eq!(row.try_get::<String, _>("status").unwrap(), "resolving");
    assert_eq!(
        row.try_get::<Option<i64>, _>("collateral_at_close_micro")
            .unwrap(),
        Some(123_456)
    );
    assert_eq!(row.try_get::<i32, _>("final_vote_bps").unwrap(), 6_000);
    assert_eq!(row.try_get::<i64, _>("redemption_micro").unwrap(), 600_000);
    assert_eq!(row.try_get::<i32, _>("score_bp").unwrap(), 8_500);
}

#[tokio::test]
async fn phase6_w2_factories_open_real_transactions_on_postgres() {
    let Some(store) = pg_store().await else {
        return;
    };
    // W2 landed audit/invariants/unwind; the factories open for real.
    assert!(store.ops_audit_tx().await.is_ok());
    assert!(store.invariant_read_tx().await.is_ok());
    assert!(store.unwind_tx().await.is_ok());
    // The shared write transaction carries the real D26 audit sink; an
    // uncommitted row stays invisible (rollback on drop).
    let mut tx = store.resolve_tx().await.unwrap();
    tx.audit_insert(application::model::AdminAction {
        actor_role: application::model::AdminRole::Curator,
        actor_token_digest: "digest".into(),
        action: "test".into(),
        subject: "market:test".into(),
        before: None,
        after: None,
        reason: None,
    })
    .await
    .unwrap();
    drop(tx);
    // Unknown users are NotFound through the real withdrawal guard.
    let eligibility = application::ports::WithdrawalEligibility::withdrawal_eligibility(
        &store,
        UserId(Uuid::new_v4()),
    )
    .await
    .unwrap_err();
    assert_eq!(eligibility, StoreError::NotFound("user"));
}

// ---------------------------------------------------------------------------
// Wave W1: D24 config plane + D25 fences on PostgreSQL. The config plane is
// SINGLETON state (unlike the uuid-isolated fixtures above), so every test
// first restores the 0008 seed — per-test truncation, serialized by
// RUST_TEST_THREADS=1.
// ---------------------------------------------------------------------------

async fn reset_config(store: &PgStore) {
    let mut tx = store.pool_handle().begin().await.unwrap();
    sqlx::query("delete from config_change_proposals")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("delete from config_changes where generation > 1")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("delete from config_generations where generation > 1")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "update config_generation set generation = 1, updated_by = 'test-reset' where singleton = 1",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("delete from request_fingerprints")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        r#"
        update config_entries set value = seed.value::jsonb
          from (values
            ('sweep_delay_secs', '180'),
            ('trading_paused', 'false'),
            ('trade_fee_bps', '100'),
            ('position_cap_micro_by_tier',
             '[25000000, 50000000, 100000000, 250000000, 500000000]')
          ) as seed(key, value)
         where config_entries.key = seed.key
        "#,
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "delete from config_entries where key like 'market_paused:%' or key like 'voting_paused:%'",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

fn admin(role: application::model::AdminRole, digest: &str) -> application::model::AdminContext {
    application::model::AdminContext::Admin {
        token_digest: digest.to_string(),
        role,
    }
}

#[tokio::test]
async fn pg_config_transaction_exposes_changes_market_times_audit_and_batch_outbox() {
    let Some(store) = pg_store().await else {
        return;
    };
    let _stateful_guard = stateful_money_config_guard().await;
    reset_config(&store).await;
    let fixture = market_fixture(&store).await;
    let mut tx = store.ops_config_tx().await.unwrap();
    assert_eq!(tx.lock_generation().await.unwrap(), 1);
    tx.acquire_exclusive_fences(&["global".into(), format!("market:{}", fixture.market.0)])
        .await
        .unwrap();
    assert_eq!(
        tx.market_times(fixture.market).await.unwrap().unwrap().id,
        fixture.market
    );
    assert!(tx
        .market_times(MarketId(Uuid::new_v4()))
        .await
        .unwrap()
        .is_none());
    let change = application::model::ConfigChange {
        key: "sweep_delay_secs".into(),
        old: Some(serde_json::json!(180)),
        new: serde_json::json!(240),
    };
    tx.apply_changes(2, std::slice::from_ref(&change), "coverage-principal")
        .await
        .unwrap();
    assert_eq!(
        tx.changes_for_key_since("sweep_delay_secs", 1)
            .await
            .unwrap(),
        vec![change.clone()]
    );
    assert_eq!(tx.changes_in_generation(2).await.unwrap(), vec![change]);
    tx.audit_insert(application::model::AdminAction {
        actor_role: application::model::AdminRole::Ops,
        actor_token_digest: "coverage-principal".into(),
        action: "config_coverage".into(),
        subject: "config:2".into(),
        before: None,
        after: Some(serde_json::json!({"generation":2})),
        reason: Some("contract".into()),
    })
    .await
    .unwrap();
    tx.append_batch(&[
        Event {
            event_type: "ConfigChanged",
            aggregate_type: "config",
            aggregate_id: Uuid::new_v4(),
            payload: serde_json::json!({"generation":2}),
        },
        Event {
            event_type: "ConfigChanged",
            aggregate_type: "config",
            aggregate_id: Uuid::new_v4(),
            payload: serde_json::json!({"generation":2,"batch":true}),
        },
    ])
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

// D24 Pg contract (a): writer B BLOCKS behind writer A on the singleton
// generation row and observes A's commit.
#[tokio::test]
async fn pg_config_writer_b_blocks_behind_writer_a_on_the_generation_row() {
    let Some(store) = pg_store().await else {
        return;
    };
    let _stateful_guard = stateful_money_config_guard().await;
    reset_config(&store).await;
    let mut a = store.ops_config_tx().await.unwrap();
    assert_eq!(a.lock_generation().await.unwrap(), 1);

    let store_b = store.clone();
    let b = tokio::spawn(async move {
        let mut tx = store_b.ops_config_tx().await.unwrap();
        tx.lock_generation().await.unwrap()
    });
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    assert!(!b.is_finished(), "B must wait behind A's row lock");

    a.apply_changes(
        2,
        &[application::model::ConfigChange {
            key: "sweep_delay_secs".to_string(),
            old: Some(serde_json::json!(180)),
            new: serde_json::json!(240),
        }],
        "writer-a",
    )
    .await
    .unwrap();
    a.commit().await.unwrap();
    let seen = timeout(std::time::Duration::from_secs(5), b)
        .await
        .expect("B must unblock once A commits")
        .unwrap();
    assert_eq!(seen, 2, "B observes A's committed generation");
}

// D24 Pg contract (b): a missed wake-up heals from the authoritative
// generation on the periodic tick; ordinary wake-ups ride the outbox.
#[tokio::test]
async fn pg_missed_wakeup_heals_from_the_authoritative_generation() {
    use application::ops::reconciler::{ConfigWatch, Reconciler};
    let Some(store) = pg_store().await else {
        return;
    };
    let _stateful_guard = stateful_money_config_guard().await;
    reset_config(&store).await;
    let reconciler = Reconciler {
        io: store.clone(),
        watch: ConfigWatch::new(),
    };
    // Settle the cursor + install the current snapshot.
    reconciler.tick(true).await.unwrap();
    let base = reconciler.watch.generation();

    // The wake path: SetConfig commits a ConfigChanged event with the state.
    let outcome = application::ops::set_config::SetConfig {
        store: &store,
        clock: &FixedClock(OffsetDateTime::now_utc()),
    }
    .execute(application::ops::set_config::SetConfigCmd {
        actor: admin(application::model::AdminRole::Ops, "ops-digest-1"),
        patch: serde_json::json!({"sweep_delay_secs": 240}),
        reason: "pg wake contract".to_string(),
        idempotency_key: format!("pg-wake-{}", Uuid::new_v4()),
        expected_base_generation: None,
    })
    .await
    .unwrap();
    assert_eq!(outcome.generation, base + 1);
    assert!(
        reconciler.tick(false).await.unwrap(),
        "the outbox event wakes the reload"
    );
    assert_eq!(reconciler.watch.generation(), base + 1);
    assert_eq!(
        reconciler.watch.value("sweep_delay_secs"),
        Some(serde_json::json!(240))
    );

    // The MISSED wake: bump the authoritative generation with NO outbox row.
    sqlx::query("insert into config_generations (generation, applied_by) values ($1, 'silent')")
        .bind(base + 2)
        .execute(store.pool_handle())
        .await
        .unwrap();
    sqlx::query("update config_generation set generation = $1 where singleton = 1")
        .bind(base + 2)
        .execute(store.pool_handle())
        .await
        .unwrap();
    assert!(
        !reconciler.tick(false).await.unwrap(),
        "no event, no wake — the snapshot lags"
    );
    assert_eq!(reconciler.watch.generation(), base + 1);
    assert!(
        reconciler.tick(true).await.unwrap(),
        "the heal tick converges on the authoritative generation"
    );
    assert_eq!(reconciler.watch.generation(), base + 2);
}

// D25 on Pg: pause 423 at the fence point, replay precedence over the pause
// AND over relevant drift, fingerprint conflicts, and D25a staleness
// through the two-phase cap change.
#[tokio::test]
async fn pg_place_trade_fence_point_pause_replay_and_staleness() {
    let Some(store) = pg_store().await else {
        return;
    };
    let _stateful_guard = stateful_money_config_guard().await;
    reset_config(&store).await;
    let fixture = market_fixture(&store).await;
    seed_money_clear(&store, fixture.user, OffsetDateTime::now_utc()).await;
    sqlx::query(
        r#"insert into votes
            (id, user_id, market_id, outcome_id, crowd_guess_pct, seq, idempotency_key)
        values ($1, $2, $3, $4, 50, 1, $5)"#,
    )
    .bind(Uuid::new_v4())
    .bind(fixture.user.0)
    .bind(fixture.market.0)
    .bind(fixture.yes_outcome)
    .bind(format!("vote-{}", Uuid::new_v4()))
    .execute(store.pool_handle())
    .await
    .unwrap();
    let mut fund = store.trade_tx().await.unwrap();
    let fund_key = format!("fund-{}", Uuid::new_v4());
    fund.serialize_key(&fund_key).await.unwrap();
    let external = fund
        .account(OwnerRef::External, Currency::Usdc)
        .await
        .unwrap();
    let user_account = fund
        .account(OwnerRef::User(fixture.user), Currency::Usdc)
        .await
        .unwrap();
    fund.ledger_apply(
        TxnKind::Deposit,
        &fund_key,
        &[
            Entry {
                account: external,
                amount: MicroUsd(-50_000_000),
            },
            Entry {
                account: user_account,
                amount: MicroUsd(50_000_000),
            },
        ],
    )
    .await
    .unwrap();
    fund.commit().await.unwrap();

    let use_case = PlaceTrade {
        store: &store,
        clock: &FixedClock(OffsetDateTime::now_utc()),
        rep_config: application::model::RepConfig::default(),
    };
    let buy = |key: &str, amount: i64, version: Option<i64>| PlaceTradeCmd {
        market: fixture.market,
        user: fixture.user,
        side: Side::Yes,
        action: TradeAction::Buy,
        amount_micro: amount,
        idempotency_key: key.to_string(),
        run_id: None,
        pending_action_id: None,
        expected_config_version: version,
    };
    let pre_pause_key = format!("trade-{}", Uuid::new_v4());
    let first = use_case
        .execute(buy(&pre_pause_key, 5_000_000, Some(1)))
        .await
        .unwrap();

    // Trading pause (ops, direct) → fresh trades 423.
    let clock = FixedClock(OffsetDateTime::now_utc());
    application::ops::set_config::SetConfig {
        store: &store,
        clock: &clock,
    }
    .execute(application::ops::set_config::SetConfigCmd {
        actor: admin(application::model::AdminRole::Ops, "ops-digest-1"),
        patch: serde_json::json!({"trading_paused": true}),
        reason: "pg pause contract".to_string(),
        idempotency_key: format!("pg-pause-{}", Uuid::new_v4()),
        expected_base_generation: None,
    })
    .await
    .unwrap();
    let paused = use_case
        .execute(buy(
            &format!("trade-{}", Uuid::new_v4()),
            5_000_000,
            Some(1),
        ))
        .await
        .unwrap_err();
    assert_eq!(paused, AppError::TradingPaused);
    // Replay-after-pause AND replay-after-relevant-drift: the original
    // receipt comes back with no pause or config check.
    let replay = use_case
        .execute(buy(&pre_pause_key, 5_000_000, Some(1)))
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.trade_id, first.trade_id);
    // Same key, different payload → typed 409.
    let conflict = use_case
        .execute(buy(&pre_pause_key, 6_000_000, Some(1)))
        .await
        .unwrap_err();
    assert_eq!(conflict, AppError::IdempotencyConflict);
    application::ops::set_config::SetConfig {
        store: &store,
        clock: &clock,
    }
    .execute(application::ops::set_config::SetConfigCmd {
        actor: admin(application::model::AdminRole::Ops, "ops-digest-1"),
        patch: serde_json::json!({"trading_paused": false}),
        reason: "resume".to_string(),
        idempotency_key: format!("pg-resume-{}", Uuid::new_v4()),
        expected_base_generation: None,
    })
    .await
    .unwrap();

    // D25a: a two-phase cap change (distinct principals) stales a pending
    // preview at this user's tier.
    let generation_before = {
        let mut tx = store.ops_config_tx().await.unwrap();
        tx.lock_generation().await.unwrap()
    };
    let proposal = application::ops::proposals::CreateProposal {
        store: &store,
        clock: &clock,
    }
    .execute(application::ops::proposals::CreateProposalCmd {
        actor: admin(application::model::AdminRole::Finance, "finance-digest-1"),
        patch: Some(serde_json::json!({
            "position_cap_micro_by_tier":
                [30_000_000i64, 50_000_000i64, 100_000_000i64, 250_000_000i64, 500_000_000i64]
        })),
        revert_of_generation: None,
        reason: "raise tier-0 cap".to_string(),
        idempotency_key: format!("pg-prop-{}", Uuid::new_v4()),
    })
    .await
    .unwrap();
    let confirmed = application::ops::proposals::ConfirmProposal {
        store: &store,
        clock: &clock,
    }
    .execute(application::ops::proposals::SettleProposalCmd {
        actor: admin(application::model::AdminRole::Finance, "finance-digest-2"),
        id: proposal.id,
        reason: None,
    })
    .await
    .unwrap();
    assert_eq!(confirmed.resulting_generation, Some(generation_before + 1));

    let stale = use_case
        .execute(buy(
            &format!("trade-{}", Uuid::new_v4()),
            1_000_000,
            Some(generation_before),
        ))
        .await
        .unwrap_err();
    assert_eq!(
        stale,
        AppError::StaleConfig {
            preview_generation: generation_before,
            current_generation: generation_before + 1,
        }
    );
    // A preview at the current generation clears.
    use_case
        .execute(buy(
            &format!("trade-{}", Uuid::new_v4()),
            1_000_000,
            Some(generation_before + 1),
        ))
        .await
        .unwrap();
}

// D25 on Pg: CastVote takes ONLY the voting fence — votes proceed through a
// trading pause; a voting pause 423s and auto-expires at `tally_hidden_at`.
#[tokio::test]
async fn pg_cast_vote_takes_only_the_voting_fence_and_the_pause_expires() {
    use application::cast_vote::{CastVote, CastVoteCmd};
    let Some(store) = pg_store().await else {
        return;
    };
    let _stateful_guard = stateful_money_config_guard().await;
    reset_config(&store).await;
    let fixture = market_fixture(&store).await;
    let address = format!("+1{}", fixture.user.0.as_u128());
    sqlx::query(
        "insert into user_channels (user_id, channel, address) values ($1, 'imessage', $2)",
    )
    .bind(fixture.user.0)
    .bind(&address)
    .execute(store.pool_handle())
    .await
    .unwrap();

    // Trading paused — votes must not care.
    sqlx::query("update config_entries set value = 'true'::jsonb where key = 'trading_paused'")
        .execute(store.pool_handle())
        .await
        .unwrap();
    let now = OffsetDateTime::now_utc();
    seed_money_clear(&store, fixture.user, now).await;
    let vote = |key: &str, guess: u8| CastVoteCmd {
        market: fixture.market,
        user: fixture.user,
        side: Side::Yes,
        crowd_guess_pct: guess,
        idempotency_key: key.to_string(),
        cast_ip: None,
        device_hash: None,
    };
    let config = application::model::VoteIntegrityConfig::default();
    let through_pause_key = format!("vote-{}", Uuid::new_v4());
    let receipt = CastVote {
        store: &store,
        clock: &FixedClock(now),
        config,
    }
    .execute(vote(&through_pause_key, 60))
    .await
    .unwrap();
    assert!(!receipt.replayed, "a trading pause never delays votes");

    // Same key, different payload → typed 409; exact payload replays.
    let conflict = CastVote {
        store: &store,
        clock: &FixedClock(now),
        config,
    }
    .execute(vote(&through_pause_key, 61))
    .await
    .unwrap_err();
    assert_eq!(conflict, AppError::IdempotencyConflict);
    let replayed = CastVote {
        store: &store,
        clock: &FixedClock(now),
        config,
    }
    .execute(vote(&through_pause_key, 60))
    .await
    .unwrap();
    assert!(replayed.replayed);

    // A voting pause 423s a second voter pre-window and auto-expires at
    // `tally_hidden_at` with NO admin action (the fixture hides at +1h,
    // closes at +2h).
    let voter2 = UserId(Uuid::new_v4());
    sqlx::query("insert into users (id, handle) values ($1, $2)")
        .bind(voter2.0)
        .bind(format!("user-{}", Uuid::new_v4()))
        .execute(store.pool_handle())
        .await
        .unwrap();
    sqlx::query(
        "insert into user_channels (user_id, channel, address) values ($1, 'imessage', $2)",
    )
    .bind(voter2.0)
    .bind(format!("+1{}", voter2.0.as_u128()))
    .execute(store.pool_handle())
    .await
    .unwrap();
    seed_money_clear(&store, voter2, now).await;
    sqlx::query("insert into config_entries (key, value) values ($1, 'true'::jsonb)")
        .bind(format!("voting_paused:{}", fixture.market.0))
        .execute(store.pool_handle())
        .await
        .unwrap();
    let paused_vote = |key: &str| CastVoteCmd {
        market: fixture.market,
        user: voter2,
        side: Side::No,
        crowd_guess_pct: 40,
        idempotency_key: key.to_string(),
        cast_ip: None,
        device_hash: None,
    };
    let paused = CastVote {
        store: &store,
        clock: &FixedClock(now),
        config,
    }
    .execute(paused_vote(&format!("vote-{}", Uuid::new_v4())))
    .await
    .unwrap_err();
    assert_eq!(paused, AppError::VotingPaused);
    let after_expiry = CastVote {
        store: &store,
        clock: &FixedClock(now + Duration::minutes(61)),
        config,
    }
    .execute(paused_vote(&format!("vote-{}", Uuid::new_v4())))
    .await
    .unwrap();
    assert_eq!(
        after_expiry.seq, None,
        "the hidden window hides the tally while the expired pause lets the vote through"
    );
}
