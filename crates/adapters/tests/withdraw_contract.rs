//! W1 Pg contract + chaos sketches. Cannot skip: an absent `DATABASE_URL`
//! or unreachable database FAILS the suite (see `tests/common`).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::too_many_lines)]

use adapters::pg::{PgStore, PgWithdrawStore};
use application::credit_deposit::{
    admit_observed, confirm_deposit_refund, observe_finalized, propose_deposit_command,
    reevaluate_held,
};
use application::model::{AdminContext, AdminRole, Event, ProposalStatus};
use application::model::{MarketId, TradeAction, UserStatus};
use application::money::ObservedDeposit;
use application::place_trade::{PlaceTrade, PlaceTradeCmd};
use application::ports::withdraw_decide::{decide_machine, DecideCmd, DecideWithdraw};
use application::ports::withdraw_fakes::{dest_a, dest_b, FakeScreen, FakeWithdrawStore};
use application::ports::withdraw_reconcile::ReconcileWithdraw;
use application::ports::withdraw_request::{RequestClock, RequestWithdraw};
use application::ports::withdraw_send::{FakeSigner, SendWithdraw};
use application::ports::withdraw_settle::SettleWithdraw;
use application::ports::{
    identity_a_holds, identity_b_holds, identity_c_holds, identity_d_holds, identity_e_replay_ok,
    ChainReceipt, Combo, LandingState, MoneyProposal, OutboundAttemptRow, OutboundPaymentRow,
    OutboundSubject, RequestWithdrawCmd, ScreenVerdict, WithdrawStore, WithdrawalId,
    WithdrawalReceipt,
};
use domain::amm::Side;
use domain::money::MicroUsd;
use std::net::IpAddr;
use time::Duration;
use uuid::Uuid;

mod common;

fn finance() -> AdminContext {
    AdminContext::Admin {
        token_digest: "fin-a".into(),
        role: AdminRole::Finance,
    }
}

fn superadmin() -> AdminContext {
    AdminContext::Admin {
        token_digest: "super-b".into(),
        role: AdminRole::Superadmin,
    }
}

fn screen_at(now: time::OffsetDateTime) -> FakeScreen {
    FakeScreen {
        verdict: ScreenVerdict::Clear {
            checked_at: now,
            expires_at: now + Duration::hours(24),
            policy_version: "1".into(),
        },
        fail: false,
    }
}

async fn pg_pool() -> sqlx::PgPool {
    // Isolation without silence: this suite TRUNCATEs shared tables, so it runs
    // against its own scratch database derived from DATABASE_URL rather than the
    // caller's. There is no skip path: every setup failure PANICS (see
    // tests/common), because a Pg suite that silently passes without a database
    // is worse than none.
    static ONCE: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
    static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");
    let url = ONCE
        .get_or_init(|| async {
            let base = common::database_url();
            let (root, _) = base
                .rsplit_once('/')
                .expect("DATABASE_URL must name a database");
            let name = "opinions_suite_withdraw";
            let admin = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect(&base)
                .await
                .expect("connect to DATABASE_URL");
            let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
                "drop database if exists {name} with (force)"
            )))
            .execute(&admin)
            .await;
            sqlx::query(sqlx::AssertSqlSafe(format!("create database {name}")))
                .execute(&admin)
                .await
                .expect("create the suite scratch database");
            admin.close().await;
            let url = format!("{root}/{name}");
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect(&url)
                .await
                .expect("connect to the suite scratch database");
            MIGRATOR
                .run(&pool)
                .await
                .expect("apply migrations to the suite scratch database");
            pool.close().await;
            url
        })
        .await
        .clone();
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .expect("connect to the suite scratch database")
}

fn pg_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

async fn reset_pg_user(pool: &sqlx::PgPool, cash_micro: i64) -> application::model::UserId {
    sqlx::query(
        "truncate table users, ledger_transactions, ledger_accounts, request_fingerprints, \
         outbound_payments, money_command_proposals, events_outbox, notifications, \
         admin_actions restart identity cascade",
    )
    .execute(pool)
    .await
    .unwrap();
    let user = application::model::UserId(Uuid::new_v4());
    sqlx::query("insert into users (id, handle, kyc_tier, status) values ($1,$2,2,'active')")
        .bind(user.0)
        .bind(format!("w1-{}", user.0))
        .execute(pool)
        .await
        .unwrap();
    let external: Uuid = sqlx::query_scalar(
        "insert into ledger_accounts (owner_type, owner_id, currency) \
         values ('external',null,'usdc') returning id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let user_account: Uuid = sqlx::query_scalar(
        "insert into ledger_accounts (owner_type, owner_id, currency) \
         values ('user',$1,'usdc') returning id",
    )
    .bind(user.0)
    .fetch_one(pool)
    .await
    .unwrap();
    let mut funding_tx = pool.begin().await.unwrap();
    let funding = Uuid::new_v4();
    sqlx::query(
        "insert into ledger_transactions (id, kind, idempotency_key) \
         values ($1,'deposit',$2)",
    )
    .bind(funding)
    .bind(format!("w1-fund-{}", user.0))
    .execute(&mut *funding_tx)
    .await
    .unwrap();
    for (account, amount) in [(external, -cash_micro), (user_account, cash_micro)] {
        sqlx::query(
            "insert into ledger_entries (txn_id, account_id, amount_micro) values ($1,$2,$3)",
        )
        .bind(funding)
        .bind(account)
        .bind(amount)
        .execute(&mut *funding_tx)
        .await
        .unwrap();
    }
    funding_tx.commit().await.unwrap();
    user
}

async fn seed_ready_credit_lot(
    pool: &sqlx::PgPool,
    user: application::model::UserId,
    amount_micro: i64,
) -> Uuid {
    let external_cash: Uuid = sqlx::query_scalar(
        "select id from ledger_accounts where owner_type='external' and currency='usdc'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let external_credit: Uuid = sqlx::query_scalar(
        "insert into ledger_accounts (owner_type,owner_id,currency) \
         values ('external',null,'usdc_credit') returning id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let house_cash: Uuid = sqlx::query_scalar(
        "insert into ledger_accounts (owner_type,owner_id,currency) \
         values ('house',null,'usdc') returning id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let house_credit: Uuid = sqlx::query_scalar(
        "insert into ledger_accounts (owner_type,owner_id,currency) \
         values ('house',null,'usdc_credit') returning id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let reserve: Uuid = sqlx::query_scalar(
        "insert into ledger_accounts (owner_type,owner_id,currency) \
         values ('bonus_reserve',null,'usdc') returning id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let user_credit: Uuid = sqlx::query_scalar(
        "insert into ledger_accounts (owner_type,owner_id,currency) \
         values ('user',$1,'usdc_credit') returning id",
    )
    .bind(user.0)
    .fetch_one(pool)
    .await
    .unwrap();

    let mut tx = pool.begin().await.unwrap();
    for (kind, key, entries) in [
        (
            "deposit",
            "withdraw-convert-genesis",
            vec![
                (external_cash, -amount_micro),
                (house_cash, amount_micro),
                (external_credit, -amount_micro),
                (house_credit, amount_micro),
            ],
        ),
        (
            "reversal",
            "withdraw-convert-reserve",
            vec![(house_cash, -amount_micro), (reserve, amount_micro)],
        ),
        (
            "credit_grant",
            "withdraw-convert-grant",
            vec![(house_credit, -amount_micro), (user_credit, amount_micro)],
        ),
    ] {
        let txn = Uuid::new_v4();
        sqlx::query("insert into ledger_transactions (id,kind,idempotency_key) values ($1,$2,$3)")
            .bind(txn)
            .bind(kind)
            .bind(key)
            .execute(&mut *tx)
            .await
            .unwrap();
        for (account, amount) in entries {
            sqlx::query(
                "insert into ledger_entries (txn_id,account_id,amount_micro) values ($1,$2,$3)",
            )
            .bind(txn)
            .bind(account)
            .bind(amount)
            .execute(&mut *tx)
            .await
            .unwrap();
        }
    }
    tx.commit().await.unwrap();

    let lot = Uuid::new_v4();
    sqlx::query(
        "insert into credit_grant_lots \
         (id,user_id,source,amount_micro,grant_class,policy_version,idempotency_key) \
         values ($1,$2,'contract',$3,'real_money','contract','withdraw-convert-lot')",
    )
    .bind(lot)
    .bind(user.0)
    .bind(amount_micro)
    .execute(pool)
    .await
    .unwrap();
    let allocation = Uuid::new_v4();
    let trade = Uuid::new_v4();
    sqlx::query(
        "insert into credit_fee_allocations \
         (id,trade_id,lot_id,split_seq,amount_micro,kind,idempotency_key) \
         values ($1,$2,$3,0,$4,'allocated','withdraw-convert-allocated')",
    )
    .bind(allocation)
    .bind(trade)
    .bind(lot)
    .bind(amount_micro)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "insert into credit_fee_allocations \
         (trade_id,lot_id,split_seq,amount_micro,kind,source_allocation_id,idempotency_key) \
         values ($1,$2,0,$3,'finalized',$4,'withdraw-convert-finalized')",
    )
    .bind(trade)
    .bind(lot)
    .bind(amount_micro)
    .bind(allocation)
    .execute(pool)
    .await
    .unwrap();
    lot
}

async fn seed_trade_market(pool: &sqlx::PgPool, user: application::model::UserId) -> MarketId {
    let market = MarketId(Uuid::new_v4());
    let yes = Uuid::new_v4();
    let no = Uuid::new_v4();
    let market_pool = Uuid::new_v4();
    let now = time::OffsetDateTime::now_utc();
    sqlx::query("insert into reputation (user_id, rep_micro, tier) values ($1,0,0)")
        .bind(user.0)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "insert into markets \
         (id,slug,question,status,min_votes_to_resolve,opens_at,closes_at,tally_hidden_at) \
         values ($1,$2,'w1 concurrency','live',3,$3,$4,$5)",
    )
    .bind(market.0)
    .bind(format!("w1-concurrency-{}", market.0))
    .bind(now - Duration::hours(1))
    .bind(now + Duration::hours(2))
    .bind(now + Duration::hours(1))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "insert into outcomes (id,market_id,label,idx) \
         values ($1,$3,'YES',0),($2,$3,'NO',1)",
    )
    .bind(yes)
    .bind(no)
    .bind(market.0)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "insert into pools (id,market_id,fee_bps,seeded_micro) \
         values ($1,$2,100,1000000000)",
    )
    .bind(market_pool)
    .bind(market.0)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "insert into pool_reserves (pool_id,outcome_id,market_id,reserve_micro_shares) \
         values ($1,$2,$4,1000000000),($1,$3,$4,1000000000)",
    )
    .bind(market_pool)
    .bind(yes)
    .bind(no)
    .bind(market.0)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "insert into votes \
         (id,user_id,market_id,outcome_id,crowd_guess_pct,seq,idempotency_key) \
         values ($1,$2,$3,$4,50,1,$5)",
    )
    .bind(Uuid::new_v4())
    .bind(user.0)
    .bind(market.0)
    .bind(yes)
    .bind(format!("w1-concurrency-vote-{}", market.0))
    .execute(pool)
    .await
    .unwrap();
    market
}

#[tokio::test]
async fn identities_a_through_e_on_the_happy_path() {
    let store = FakeWithdrawStore::new();
    let user = store.seed_user(UserStatus::Active, 2);
    store.credit(user, 20_000_000);
    let now = store.now();
    let screen = screen_at(now);
    let receipt = RequestWithdraw {
        store: &store,
        clock: &RequestClock(now),
        geo: &screen,
        sanctions: &screen,
    }
    .execute(RequestWithdrawCmd {
        user,
        amount_micro: 5_000_000,
        dest: dest_a(),
        client_ip: Some(IpAddr::from([203, 0, 113, 4])),
        idempotency_key: Some("id-a".into()),
    })
    .await
    .unwrap();
    let id = receipt.id.unwrap();
    decide_machine(&store, &RequestClock(now), id)
        .await
        .unwrap();
    DecideWithdraw {
        store: &store,
        clock: &RequestClock(now),
        actor: finance(),
    }
    .execute(DecideCmd::FinanceApprove {
        id,
        reason: "ok".into(),
    })
    .await
    .unwrap();
    let rails = application::ports::withdraw_fakes::FakeRails::default();
    let signer = FakeSigner::new("sig-contract");
    let identity = application::ports::withdraw_fakes::test_rail();
    SendWithdraw {
        store: &store,
        clock: &RequestClock(now),
        rails: &rails,
        signer: &signer,
        identity: &identity,
    }
    .execute(id)
    .await
    .unwrap();
    {
        let mut tx = store.withdraw_tx().await.unwrap();
        let mut row = tx.withdrawal_for_update(id).await.unwrap();
        let from = row.combo;
        row.combo = Combo::W9;
        tx.cas_withdrawal(id, from, &row).await.unwrap();
        let mut attempt = store.attempts().into_iter().next().unwrap();
        attempt.landing_state = LandingState::Finalized;
        tx.save_attempt(&attempt).await.unwrap();
        tx.commit().await.unwrap();
    }
    SettleWithdraw {
        store: &store,
        clock: &RequestClock(now),
        identity: &identity,
    }
    .execute(
        id,
        ChainReceipt {
            signature: "sig-contract".into(),
            mint: identity.usdc_mint.clone(),
            source: identity.treasury_token_account.clone(),
            dest_token_account: dest_a(),
            delta_micro: 5_000_000,
            commitment: "finalized".into(),
        },
    )
    .await
    .unwrap();
    assert!(identity_a_holds(store.withheld(), &store.withdrawals()));
    assert!(identity_b_holds(&store.withdrawals()));
    assert!(identity_c_holds(&store.withdrawals()));
    assert!(identity_d_holds(&store.attempts(), true));
    assert!(identity_e_replay_ok(1));
}

#[tokio::test]
async fn kill_between_hold_and_decision_recovers_via_machine_decide() {
    let store = FakeWithdrawStore::new();
    let user = store.seed_user(UserStatus::Active, 2);
    store.credit(user, 20_000_000);
    let now = store.now();
    let screen = screen_at(now);
    let receipt = RequestWithdraw {
        store: &store,
        clock: &RequestClock(now),
        geo: &screen,
        sanctions: &screen,
    }
    .execute(RequestWithdrawCmd {
        user,
        amount_micro: 5_000_000,
        dest: dest_b(),
        client_ip: Some(IpAddr::from([203, 0, 113, 5])),
        idempotency_key: Some("kill-hold".into()),
    })
    .await
    .unwrap();
    assert_eq!(receipt.combo, Some(Combo::W1));
    // Crash: decision never ran. Recovery is the machine pass.
    let recovered = decide_machine(&store, &RequestClock(now), receipt.id.unwrap())
        .await
        .unwrap();
    assert_eq!(recovered.combo, Combo::W4);
    assert_eq!(store.withheld(), 5_000_000);
}

#[tokio::test]
async fn withdraw_send_crash_does_not_create_a_second_attempt() {
    let store = FakeWithdrawStore::new();
    let user = store.seed_user(UserStatus::Active, 2);
    store.credit(user, 20_000_000);
    let now = store.now();
    let screen = screen_at(now);
    let receipt = RequestWithdraw {
        store: &store,
        clock: &RequestClock(now),
        geo: &screen,
        sanctions: &screen,
    }
    .execute(RequestWithdrawCmd {
        user,
        amount_micro: 5_000_000,
        dest: dest_a(),
        client_ip: Some(IpAddr::from([203, 0, 113, 6])),
        idempotency_key: Some("crash-send".into()),
    })
    .await
    .unwrap();
    let id = receipt.id.unwrap();
    decide_machine(&store, &RequestClock(now), id)
        .await
        .unwrap();
    DecideWithdraw {
        store: &store,
        clock: &RequestClock(now),
        actor: finance(),
    }
    .execute(DecideCmd::FinanceApprove {
        id,
        reason: "go".into(),
    })
    .await
    .unwrap();
    let rails = application::ports::withdraw_fakes::FakeRails::default();
    let signer = FakeSigner::new("sig-crash");
    let identity = application::ports::withdraw_fakes::test_rail();
    SendWithdraw {
        store: &store,
        clock: &RequestClock(now),
        rails: &rails,
        signer: &signer,
        identity: &identity,
    }
    .execute(id)
    .await
    .unwrap();
    {
        let mut tx = store.withdraw_tx().await.unwrap();
        let mut row = tx.withdrawal_for_update(id).await.unwrap();
        let from = row.combo;
        row.combo = Combo::W3;
        tx.cas_withdrawal(id, from, &row).await.unwrap();
        let mut attempt = store.attempts().into_iter().next().unwrap();
        attempt.landing_state = LandingState::Prepared;
        attempt.lease_expires_at = Some(time::OffsetDateTime::from_unix_timestamp(1).unwrap());
        tx.save_attempt(&attempt).await.unwrap();
        tx.commit().await.unwrap();
    }
    signer.set_presence(application::ports::SignaturePresence::Present);
    SendWithdraw {
        store: &store,
        clock: &RequestClock(now),
        rails: &rails,
        signer: &signer,
        identity: &identity,
    }
    .execute(id)
    .await
    .unwrap();
    assert_eq!(store.attempts().len(), 1);
}

#[tokio::test]
async fn pg_round_trip_replays_the_original_receipt_and_conserves_withheld() {
    let _serial = pg_test_lock().lock().await;
    let pool = pg_pool().await;
    let user = reset_pg_user(&pool, 20_000_000).await;

    let store = PgStore::from_pool(pool.clone());
    let now = time::OffsetDateTime::now_utc();
    let screen = screen_at(now);
    let command = application::ports::RequestWithdrawCmd {
        user,
        amount_micro: 5_000_000,
        dest: dest_a(),
        client_ip: Some(IpAddr::from([203, 0, 113, 44])),
        idempotency_key: Some("pg-round-trip".into()),
    };
    let receipt = RequestWithdraw {
        store: &store,
        clock: &RequestClock(now),
        geo: &screen,
        sanctions: &screen,
    }
    .execute(command.clone())
    .await
    .unwrap();
    assert_eq!(receipt.combo, Some(Combo::W1));
    let hold_tx_id = receipt.hold_tx_id.unwrap();
    let hold_legs: Vec<(String, i64)> = sqlx::query_as(
        "select a.owner_type, e.amount_micro \
           from ledger_entries e join ledger_accounts a on a.id=e.account_id \
          where e.txn_id=$1 order by e.amount_micro",
    )
    .bind(hold_tx_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        hold_legs,
        vec![("user".into(), -5_000_000), ("withheld".into(), 5_000_000)]
    );
    let replay = RequestWithdraw {
        store: &store,
        clock: &RequestClock(now),
        geo: &screen,
        sanctions: &screen,
    }
    .execute(command)
    .await
    .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.id, receipt.id);
    assert_eq!(replay.combo, Some(Combo::W1));

    let id = receipt.id.unwrap();
    let held = decide_machine(&store, &RequestClock(now), id)
        .await
        .unwrap();
    assert_eq!(held.combo, Combo::W4);
    DecideWithdraw {
        store: &store,
        clock: &RequestClock(now),
        actor: finance(),
    }
    .execute(DecideCmd::FinanceApprove {
        id,
        reason: "pg contract".into(),
    })
    .await
    .unwrap();
    let rails = application::ports::withdraw_fakes::FakeRails::default();
    let signer = FakeSigner::new("pg-sig");
    let identity = application::ports::withdraw_fakes::test_rail();
    *rails.fail_broadcast.lock() = true;
    let first_send = SendWithdraw {
        store: &store,
        clock: &RequestClock(now),
        rails: &rails,
        signer: &signer,
        identity: &identity,
    }
    .execute(id)
    .await;
    assert!(
        matches!(
            first_send,
            Err(application::error::AppError::Store(
                application::error::StoreError::Backend(_)
            ))
        ),
        "unexpected first-send result: {first_send:?}"
    );
    let send_state: String = sqlx::query_scalar("select send_state from withdrawals where id = $1")
        .bind(id.0)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(send_state, "sending");
    let attempt_count: i64 = sqlx::query_scalar("select count(*) from outbound_send_attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(attempt_count, 1);
    *rails.fail_broadcast.lock() = false;
    let recovered = SendWithdraw {
        store: &store,
        clock: &RequestClock(now + Duration::seconds(31)),
        rails: &rails,
        signer: &signer,
        identity: &identity,
    }
    .execute(id)
    .await
    .unwrap();
    assert_eq!(recovered.combo, Combo::W6);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("select count(*) from outbound_send_attempts")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    let chain_receipt = ChainReceipt {
        signature: "pg-sig".into(),
        mint: identity.usdc_mint.clone(),
        source: identity.treasury_token_account.clone(),
        dest_token_account: dest_a(),
        delta_micro: 5_000_000,
        commitment: "finalized".into(),
    };
    ReconcileWithdraw {
        store: &store,
        clock: &RequestClock(now),
        identity: &identity,
    }
    .observe_finalized(id, chain_receipt.clone())
    .await
    .unwrap();
    let settled = SettleWithdraw {
        store: &store,
        clock: &RequestClock(now),
        identity: &identity,
    }
    .execute(id, chain_receipt.clone())
    .await
    .unwrap();
    assert_eq!(settled.combo, Combo::W10);
    let settle_tx_id = settled.settle_tx_id.unwrap();
    let settle_legs: Vec<(String, i64)> = sqlx::query_as(
        "select a.owner_type, e.amount_micro \
           from ledger_entries e join ledger_accounts a on a.id=e.account_id \
          where e.txn_id=$1 order by e.amount_micro",
    )
    .bind(settle_tx_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        settle_legs,
        vec![
            ("withheld".into(), -5_000_000),
            ("external".into(), 5_000_000)
        ]
    );
    let replayed_settle = SettleWithdraw {
        store: &store,
        clock: &RequestClock(now),
        identity: &identity,
    }
    .execute(id, chain_receipt)
    .await
    .unwrap();
    assert_eq!(replayed_settle.settle_tx_id, Some(settle_tx_id));
    let terminal_effects: i64 =
        sqlx::query_scalar("select count(*) from ledger_transactions where idempotency_key=$1")
            .bind(format!("withdraw-settle:{}", id.0))
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(terminal_effects, 1);
    let finalized_attempts: i64 = sqlx::query_scalar(
        "select count(*) from outbound_send_attempts \
          where landing_state='finalized'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(finalized_attempts, 1);
    let withheld: i64 = sqlx::query_scalar(
        "select coalesce(sum(e.amount_micro),0)::bigint \
         from ledger_accounts a left join ledger_entries e on e.account_id=a.id \
         where a.owner_type='withheld' and a.currency='usdc'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(withheld, 0);
    let notifications: i64 =
        sqlx::query_scalar("select count(*) from notifications where type='withdrawal_settled'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(notifications, 1);
}

#[tokio::test]
async fn pg_dust_settlements_never_warm_a_destination() {
    let _serial = pg_test_lock().lock().await;
    let pool = pg_pool().await;
    let user = reset_pg_user(&pool, 20_000_000).await;
    let funding_tx: Uuid = sqlx::query_scalar("select id from ledger_transactions limit 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let dest = dest_b();
    sqlx::query(
        r"insert into withdrawals
             (id,user_id,dest_address,amount_micro,status,review_state,send_state,
              hold_tx_id,settle_tx_id,request_fingerprint,risk_reasons,
              created_at,requested_at,decided_at,sent_at,settled_at)
           select gen_random_uuid(),$1,$2,4000000,'settled','approved','finalized',
                  $3,$3,'dust-' || n::text,'[]'::jsonb,
                  now() - interval '100 hours',now() - interval '100 hours',
                  now() - interval '100 hours',now() - interval '100 hours',
                  now() - interval '100 hours'
             from generate_series(1,25) n",
    )
    .bind(user.0)
    .bind(&dest)
    .bind(funding_tx)
    .execute(&pool)
    .await
    .unwrap();

    let now = time::OffsetDateTime::now_utc();
    let screen = screen_at(now);
    let store = PgWithdrawStore::new(pool.clone());
    let receipt = RequestWithdraw {
        store: &store,
        clock: &RequestClock(now),
        geo: &screen,
        sanctions: &screen,
    }
    .execute(RequestWithdrawCmd {
        user,
        amount_micro: 5_000_000,
        dest,
        client_ip: Some(IpAddr::from([203, 0, 113, 55])),
        idempotency_key: Some("pg-dust-warmth".into()),
    })
    .await
    .unwrap();

    let decided = decide_machine(&store, &RequestClock(now), receipt.id.unwrap())
        .await
        .unwrap();
    assert_eq!(decided.combo, Combo::W4);
    assert!(decided
        .risk_reasons
        .iter()
        .any(|reason| reason == "dest_not_warm"));
}

#[tokio::test]
async fn pg_concurrent_same_client_key_allows_exactly_one_intent() {
    let _serial = pg_test_lock().lock().await;
    let pool = pg_pool().await;
    let user = reset_pg_user(&pool, 30_000_000).await;
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("select pg_advisory_xact_lock(2, hashtext($1))")
        .bind(user.0.to_string())
        .execute(&mut *blocker)
        .await
        .unwrap();

    let now = time::OffsetDateTime::now_utc();
    let spawn_request = |amount_micro: i64, dest: String| {
        let store = PgWithdrawStore::new(pool.clone());
        let screen = screen_at(now);
        tokio::spawn(async move {
            RequestWithdraw {
                store: &store,
                clock: &RequestClock(now),
                geo: &screen,
                sanctions: &screen,
            }
            .execute(RequestWithdrawCmd {
                user,
                amount_micro,
                dest,
                client_ip: Some(IpAddr::from([203, 0, 113, 55])),
                idempotency_key: Some("same-client-key".into()),
            })
            .await
        })
    };
    let first = spawn_request(5_000_000, dest_a());
    let second = spawn_request(6_000_000, dest_b());
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    blocker.commit().await.unwrap();

    let results = [first.await.unwrap(), second.await.unwrap()];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(
                result,
                Err(application::error::AppError::IdempotencyConflict)
            ))
            .count(),
        1
    );
    let withdrawals: i64 = sqlx::query_scalar("select count(*) from withdrawals")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(withdrawals, 1);
    let idempotency_rows: i64 = sqlx::query_scalar(
        "select count(*) from request_fingerprints where idempotency_key like 'withdraw-idem:%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(idempotency_rows, 1);
}

#[tokio::test]
async fn pg_withdraw_auto_collects_the_receivable_before_holding_cash() {
    let _serial = pg_test_lock().lock().await;
    let pool = pg_pool().await;
    let user = reset_pg_user(&pool, 20_000_000).await;
    let market = Uuid::new_v4();
    sqlx::query(
        "insert into markets \
         (id, slug, question, status, min_votes_to_resolve, opens_at, closes_at, tally_hidden_at) \
         values ($1,$2,'w1 receivable','voided',3,now(),now(),now())",
    )
    .bind(market)
    .bind(format!("w1-recv-{market}"))
    .execute(&pool)
    .await
    .unwrap();
    let audit: Uuid = sqlx::query_scalar(
        "insert into admin_actions \
         (actor_role, actor_token_digest, action, subject, reason) \
         values ('superadmin','test','unwind_confirm',$1,'contract') returning id",
    )
    .bind(format!("market:{market}"))
    .fetch_one(&pool)
    .await
    .unwrap();
    let receivable = Uuid::new_v4();
    sqlx::query(
        "insert into receivables \
         (id, market_id, user_id, origin_reversal_txn_id, opened_micro) \
         values ($1,$2,$3,$4,3000000)",
    )
    .bind(receivable)
    .bind(market)
    .bind(user.0)
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "insert into receivable_movements \
         (receivable_id, kind, amount_micro, actor, audit_id, idempotency_key) \
         values ($1,'opened',3000000,'test',$2,$3)",
    )
    .bind(receivable)
    .bind(audit)
    .bind(format!("w1-open-{receivable}"))
    .execute(&pool)
    .await
    .unwrap();

    let now = time::OffsetDateTime::now_utc();
    let screen = screen_at(now);
    let store = PgWithdrawStore::new(pool.clone());
    let mut lien_read = store.withdraw_tx().await.unwrap();
    let open = lien_read.open_receivables(user).await.unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].id, receivable);
    assert_eq!(open[0].outstanding_micro, 3_000_000);
    drop(lien_read);
    let receipt = RequestWithdraw {
        store: &store,
        clock: &RequestClock(now),
        geo: &screen,
        sanctions: &screen,
    }
    .execute(RequestWithdrawCmd {
        user,
        amount_micro: 5_000_000,
        dest: dest_a(),
        client_ip: Some(IpAddr::from([203, 0, 113, 77])),
        idempotency_key: Some("pg-auto-collect".into()),
    })
    .await
    .unwrap();
    assert_eq!(receipt.combo, Some(Combo::W1));
    let outstanding: i64 = sqlx::query_scalar(
        "select (r.opened_micro - coalesce(sum(m.amount_micro) \
           filter (where m.kind in ('collected','written_off')),0))::bigint \
           from receivables r left join receivable_movements m on m.receivable_id=r.id \
          where r.id=$1 group by r.opened_micro",
    )
    .bind(receivable)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(outstanding, 0);
    let collection_cash_txn: Option<Uuid> = sqlx::query_scalar(
        "select cash_txn_id from receivable_movements \
          where receivable_id=$1 and kind='collected'",
    )
    .bind(receivable)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(collection_cash_txn.is_some());
    let withheld: i64 = sqlx::query_scalar(
        "select coalesce(sum(e.amount_micro),0)::bigint \
           from ledger_accounts a join ledger_entries e on e.account_id=a.id \
          where a.owner_type='withheld' and a.currency='usdc'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(withheld, 5_000_000);
}

#[tokio::test]
async fn pg_withdraw_lock_converts_a_ready_credit_lot_before_holding_cash() {
    let _serial = pg_test_lock().lock().await;
    let pool = pg_pool().await;
    let user = reset_pg_user(&pool, 20_000_000).await;
    let lot = seed_ready_credit_lot(&pool, user, 200_000).await;
    let now = time::OffsetDateTime::now_utc();
    let screen = screen_at(now);
    let store = PgWithdrawStore::new(pool.clone());

    let receipt = RequestWithdraw {
        store: &store,
        clock: &RequestClock(now),
        geo: &screen,
        sanctions: &screen,
    }
    .execute(RequestWithdrawCmd {
        user,
        amount_micro: 5_000_000,
        dest: dest_a(),
        client_ip: Some(IpAddr::from([203, 0, 113, 78])),
        idempotency_key: Some("pg-withdraw-convert".into()),
    })
    .await
    .unwrap();

    assert_eq!(receipt.combo, Some(Combo::W1));
    let converted_at: Option<time::OffsetDateTime> =
        sqlx::query_scalar("select converted_at from credit_grant_lots where id=$1")
            .bind(lot)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        converted_at.is_some(),
        "the accepted D32 contract requires the next user-lock transaction to convert a ready lot"
    );
    let conversion_txns: i64 = sqlx::query_scalar(
        "select count(*) from ledger_transactions \
         where idempotency_key in ($1,$2)",
    )
    .bind(format!("credit-convert:{lot}"))
    .bind(format!("credit-convert-pay:{lot}"))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(conversion_txns, 2);
}

#[tokio::test]
async fn pg_withdraw_reads_the_live_catalog_including_zero_disable() {
    let _serial = pg_test_lock().lock().await;
    let pool = pg_pool().await;
    sqlx::query(
        "update config_entries set value = case key \
           when 'pause_withdrawals' then 'true'::jsonb \
           when 'withdraw_auto_approve_micro' then '0'::jsonb else value end \
         where key in ('pause_withdrawals','withdraw_auto_approve_micro')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let store = PgWithdrawStore::new(pool.clone());
    let mut tx = store.withdraw_tx().await.unwrap();
    let limits = tx.limits().await.unwrap();
    assert!(limits.pause_withdrawals);
    assert_eq!(limits.auto_approve_micro, 0);
    drop(tx);
    sqlx::query(
        "update config_entries set value = case key \
           when 'pause_withdrawals' then 'false'::jsonb \
           when 'withdraw_auto_approve_micro' then '50000000'::jsonb else value end \
         where key in ('pause_withdrawals','withdraw_auto_approve_micro')",
    )
    .execute(&pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn pg_adapter_fail_closed_edges_and_supertraits_are_executable() {
    let _serial = pg_test_lock().lock().await;
    let pool = pg_pool().await;
    let user = reset_pg_user(&pool, 20_000_000).await;
    let _ = adapters::http::routes::withdraw::router::<PgStore>();
    let _ = adapters::http::routes::withdraw::admin_router::<PgStore>();
    let _ = adapters::rails::transport::RailTransport::new().unwrap();
    sqlx::query("insert into request_fingerprints (idempotency_key, fingerprint) values ($1,$2)")
        .bind("withdraw-fp:malformed")
        .bind("not-json")
        .execute(&pool)
        .await
        .unwrap();

    let store = PgWithdrawStore::new(pool);
    let mut tx = store.withdraw_tx().await.unwrap();
    assert!(matches!(
        tx.lookup_fingerprint_tx("malformed").await,
        Err(application::error::StoreError::Invariant(
            "intent receipt is not JSON"
        ))
    ));

    let replay_receipt = WithdrawalReceipt {
        id: Some(WithdrawalId(Uuid::new_v4())),
        user,
        dest: dest_a(),
        amount_micro: 5_000_000,
        combo: Some(Combo::W1),
        hold_tx_id: Some(Uuid::new_v4()),
        replayed: false,
        refused: false,
        refuse_code: None,
        refuse_message: None,
    };
    tx.persist_intent(&replay_receipt).await.unwrap();
    let replay_fingerprint = application::ports::intent_fingerprint(
        replay_receipt.user,
        replay_receipt.amount_micro,
        &replay_receipt.dest,
    );
    assert_eq!(
        tx.lookup_fingerprint_tx(&replay_fingerprint).await.unwrap(),
        Some(replay_receipt)
    );

    tx.persist_screening(user, "withdraw", &ScreenVerdict::Indeterminate)
        .await
        .unwrap();
    assert_eq!(
        tx.latest_screening(user, "withdraw").await.unwrap(),
        Some(ScreenVerdict::Indeterminate)
    );

    let event = Event {
        event_type: "W1Coverage",
        aggregate_type: "withdrawal",
        aggregate_id: Uuid::new_v4(),
        payload: serde_json::json!({"covered": true}),
    };
    tx.append_batch(&[event.clone(), event]).await.unwrap();

    let now = time::OffsetDateTime::now_utc();
    let proposal = MoneyProposal {
        id: Uuid::new_v4(),
        kind: "withdraw_approve".into(),
        subject_id: Uuid::new_v4(),
        payload_hash: "payload".into(),
        proposer_token_id: "finance-a".into(),
        confirmer_token_id: None,
        reason: "coverage".into(),
        status: ProposalStatus::Pending,
        confirm_not_before: now + Duration::minutes(15),
        expires_at: now + Duration::minutes(30),
        replay_key: format!("w1-proposal-{}", Uuid::new_v4()),
    };
    tx.insert_proposal(&proposal).await.unwrap();
    assert_eq!(
        tx.proposal_by_replay(&proposal.replay_key).await.unwrap(),
        Some(proposal)
    );
    assert!(tx
        .proposal_by_replay("missing-replay")
        .await
        .unwrap()
        .is_none());
    assert!(matches!(
        tx.record_finance_approve(1).await,
        Err(application::error::StoreError::Invariant(
            "finance approval must be recorded against a withdrawal"
        ))
    ));

    let hold_key = format!("w1-duplicate-hold:{}", user.0);
    tx.apply_hold(user, 1_000_000, &hold_key).await.unwrap();
    assert!(matches!(
        tx.apply_hold(user, 1_000_000, &hold_key).await,
        Err(application::error::StoreError::DuplicateKey)
    ));

    let payment = OutboundPaymentRow {
        id: Uuid::new_v4(),
        subject: OutboundSubject::DepositRefund,
        subject_id: Uuid::new_v4(),
        dest: dest_b(),
        amount_micro: 1_000_000,
        rail_fingerprint: "w1-lineage".into(),
    };
    tx.insert_outbound_payment(&payment).await.unwrap();
    let mut first_attempt = OutboundAttemptRow {
        id: Uuid::new_v4(),
        payment_id: payment.id,
        attempt_number: 1,
        replaces_attempt_id: None,
        signed_tx_bytes: vec![1, 2, 3],
        signature: format!("w1-first-{}", payment.id),
        last_valid_block_height: 100,
        landing_state: LandingState::Prepared,
        lease_expires_at: Some(now + Duration::minutes(1)),
        evidence: None,
    };
    tx.insert_attempt(&first_attempt).await.unwrap();
    first_attempt.landing_state = LandingState::DefinitiveFailed;
    first_attempt.lease_expires_at = None;
    tx.save_attempt(&first_attempt).await.unwrap();
    let replacement = OutboundAttemptRow {
        id: Uuid::new_v4(),
        payment_id: payment.id,
        attempt_number: 2,
        replaces_attempt_id: Some(first_attempt.id),
        signed_tx_bytes: vec![4, 5, 6],
        signature: format!("w1-second-{}", payment.id),
        last_valid_block_height: 200,
        landing_state: LandingState::Prepared,
        lease_expires_at: Some(now + Duration::minutes(2)),
        evidence: Some(serde_json::json!({"replacement": true})),
    };
    tx.insert_attempt(&replacement).await.unwrap();
    assert_eq!(tx.attempts_for(payment.id).await.unwrap().len(), 2);
    let invalid_replacement = OutboundAttemptRow {
        id: Uuid::new_v4(),
        payment_id: payment.id,
        attempt_number: 3,
        replaces_attempt_id: Some(replacement.id),
        signed_tx_bytes: vec![7, 8, 9],
        signature: format!("w1-third-{}", payment.id),
        last_valid_block_height: 300,
        landing_state: LandingState::Prepared,
        lease_expires_at: Some(now + Duration::minutes(3)),
        evidence: None,
    };
    assert!(matches!(
        tx.insert_attempt(&invalid_replacement).await,
        Err(application::error::StoreError::Invariant(
            "outbound replacement lineage is invalid"
        ))
    ));
}

#[tokio::test]
async fn pg_large_withdrawal_requires_a_distinct_second_principal() {
    let _serial = pg_test_lock().lock().await;
    let pool = pg_pool().await;
    let user = reset_pg_user(&pool, 700_000_000).await;
    let now = time::OffsetDateTime::now_utc();
    let screen = screen_at(now);
    let store = PgWithdrawStore::new(pool.clone());
    let receipt = RequestWithdraw {
        store: &store,
        clock: &RequestClock(now),
        geo: &screen,
        sanctions: &screen,
    }
    .execute(RequestWithdrawCmd {
        user,
        amount_micro: 500_000_000,
        dest: dest_a(),
        client_ip: Some(IpAddr::from([203, 0, 113, 88])),
        idempotency_key: Some("pg-dual".into()),
    })
    .await
    .unwrap();
    let id = receipt.id.unwrap();
    decide_machine(&store, &RequestClock(now), id)
        .await
        .unwrap();
    let single = DecideWithdraw {
        store: &store,
        clock: &RequestClock(now),
        actor: finance(),
    }
    .execute(DecideCmd::FinanceApprove {
        id,
        reason: "single must fail".into(),
    })
    .await;
    assert!(matches!(
        single,
        Err(application::error::AppError::AdminForbidden(_))
    ));
    let proposed = DecideWithdraw {
        store: &store,
        clock: &RequestClock(now),
        actor: finance(),
    }
    .execute(DecideCmd::ProposeDual {
        id,
        reason: "large withdrawal".into(),
    })
    .await
    .unwrap();
    assert_eq!(proposed.combo, Combo::W5);
    let same = DecideWithdraw {
        store: &store,
        clock: &RequestClock(now + Duration::minutes(16)),
        actor: finance(),
    }
    .execute(DecideCmd::ConfirmDual { id })
    .await;
    assert!(matches!(
        same,
        Err(application::error::AppError::AdminForbidden(_))
    ));
    let confirmed = DecideWithdraw {
        store: &store,
        clock: &RequestClock(now + Duration::minutes(16)),
        actor: AdminContext::Admin {
            token_digest: "root-b".into(),
            role: AdminRole::Superadmin,
        },
    }
    .execute(DecideCmd::ConfirmDual { id })
    .await
    .unwrap();
    assert_eq!(confirmed.combo, Combo::W2);
    let proposal: (String, Option<String>) = sqlx::query_as(
        "select status, confirmer_token_id from money_command_proposals where subject_id=$1",
    )
    .bind(id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(proposal.0, "confirmed");
    assert!(proposal.1.is_some());
    let audits: i64 = sqlx::query_scalar(
        "select count(*) from admin_actions where subject=$1 \
          and action in ('withdraw_propose_dual','withdraw_confirm_dual')",
    )
    .bind(format!("withdrawal:{}", id.0))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audits, 2);
    let missing_reasons: i64 = sqlx::query_scalar(
        "select count(*) from admin_actions where subject=$1 \
          and action in ('withdraw_propose_dual','withdraw_confirm_dual') \
          and reason is null",
    )
    .bind(format!("withdrawal:{}", id.0))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(missing_reasons, 0);
}

#[tokio::test]
async fn pg_active_self_exclusion_survives_cooling_off_until_dual_control_lift() {
    let _serial = pg_test_lock().lock().await;
    let pool = pg_pool().await;
    let user = reset_pg_user(&pool, 20_000_000).await;
    let now = time::OffsetDateTime::now_utc();
    let exclusion_id = Uuid::new_v4();
    sqlx::query(
        "insert into self_exclusions \
         (id, user_id, starts_at, cooling_off_until, lifted_at) \
         values ($1,$2,$3,$4,null)",
    )
    .bind(exclusion_id)
    .bind(user.0)
    .bind(now - Duration::hours(48))
    .bind(now - Duration::hours(24))
    .execute(&pool)
    .await
    .unwrap();

    let store = PgWithdrawStore::new(pool.clone());
    let screen = screen_at(now);
    let receipt = RequestWithdraw {
        store: &store,
        clock: &RequestClock(now),
        geo: &screen,
        sanctions: &screen,
    }
    .execute(RequestWithdrawCmd {
        user,
        amount_micro: 5_000_000,
        dest: dest_a(),
        client_ip: Some(IpAddr::from([203, 0, 113, 90])),
        idempotency_key: Some("pg-active-self-exclusion".into()),
    })
    .await
    .unwrap();
    let reasons: serde_json::Value =
        sqlx::query_scalar("select risk_reasons from withdrawals where id=$1")
            .bind(receipt.id.unwrap().0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(reasons.as_array().is_some_and(|items| items
        .iter()
        .any(|item| item.as_str() == Some("self_exclusion_new_dest"))));

    let source_observation = ObservedDeposit {
        user: Some(user),
        amount: MicroUsd(1_000_000),
        chain_sig: format!("w1-self-exclusion-source-{}", Uuid::new_v4()),
        source_address: dest_b(),
        dest_address: dest_a(),
        mint: "w1-usdc".into(),
        slot: 99,
    };
    observe_finalized(&PgStore::from_pool(pool.clone()), &source_observation)
        .await
        .unwrap();
    let source_receipt = RequestWithdraw {
        store: &store,
        clock: &RequestClock(now),
        geo: &screen,
        sanctions: &screen,
    }
    .execute(RequestWithdrawCmd {
        user,
        amount_micro: 5_000_000,
        dest: dest_b(),
        client_ip: Some(IpAddr::from([203, 0, 113, 90])),
        idempotency_key: Some("pg-self-exclusion-source".into()),
    })
    .await
    .unwrap();
    let source_reasons: serde_json::Value =
        sqlx::query_scalar("select risk_reasons from withdrawals where id=$1")
            .bind(source_receipt.id.unwrap().0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!source_reasons.as_array().is_some_and(|items| items
        .iter()
        .any(|item| item.as_str() == Some("self_exclusion_new_dest"))));
    assert!(source_reasons.as_array().is_some_and(|items| items
        .iter()
        .any(|item| item.as_str() == Some("dest_is_refund"))));

    sqlx::query("update self_exclusions set lifted_at=$2 where id=$1")
        .bind(exclusion_id)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
    let after_lift = RequestWithdraw {
        store: &store,
        clock: &RequestClock(now),
        geo: &screen,
        sanctions: &screen,
    }
    .execute(RequestWithdrawCmd {
        user,
        amount_micro: 6_000_000,
        dest: dest_b(),
        client_ip: Some(IpAddr::from([203, 0, 113, 90])),
        idempotency_key: Some("pg-lifted-self-exclusion".into()),
    })
    .await
    .unwrap();
    let reasons_after_lift: serde_json::Value =
        sqlx::query_scalar("select risk_reasons from withdrawals where id=$1")
            .bind(after_lift.id.unwrap().0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!reasons_after_lift.as_array().is_some_and(|items| items
        .iter()
        .any(|item| item.as_str() == Some("self_exclusion_new_dest"))));
}

#[tokio::test]
async fn pg_sanctions_hit_holds_the_withdrawal_and_opens_the_account_flag() {
    let _serial = pg_test_lock().lock().await;
    let pool = pg_pool().await;
    let user = reset_pg_user(&pool, 20_000_000).await;
    let now = time::OffsetDateTime::now_utc();
    let clear = screen_at(now);
    let hit = FakeScreen {
        verdict: ScreenVerdict::Hit,
        fail: false,
    };
    let store = PgWithdrawStore::new(pool.clone());
    let receipt = RequestWithdraw {
        store: &store,
        clock: &RequestClock(now),
        geo: &clear,
        sanctions: &hit,
    }
    .execute(RequestWithdrawCmd {
        user,
        amount_micro: 5_000_000,
        dest: dest_a(),
        client_ip: Some(IpAddr::from([203, 0, 113, 91])),
        idempotency_key: Some("pg-sanctions-hit".into()),
    })
    .await
    .unwrap();
    assert!(!receipt.refused);
    assert_eq!(receipt.combo, Some(Combo::W1));
    let open_hits: i64 = sqlx::query_scalar(
        "select count(*) from aml_flags \
          where user_id=$1 and rule='sanctions_hit' and status='open'",
    )
    .bind(user.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(open_hits, 1);
    let reasons: serde_json::Value =
        sqlx::query_scalar("select risk_reasons from withdrawals where id=$1")
            .bind(receipt.id.unwrap().0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(reasons.as_array().is_some_and(|items| {
        items.iter().any(|item| item.as_str() == Some("aml_open"))
            && items
                .iter()
                .any(|item| item.as_str() == Some("sanctions_not_clear"))
    }));
}

#[tokio::test]
async fn pg_fourth_in_band_withdrawal_flags_inside_the_hold_transaction() {
    let _serial = pg_test_lock().lock().await;
    let pool = pg_pool().await;
    let user = reset_pg_user(&pool, 1_000_000_000).await;
    let now = time::OffsetDateTime::now_utc();
    let screen = screen_at(now);
    let store = PgWithdrawStore::new(pool.clone());
    let mut ids = Vec::new();
    for (index, amount) in [100_000_000, 101_000_000, 102_000_000, 103_000_000]
        .into_iter()
        .enumerate()
    {
        let receipt = RequestWithdraw {
            store: &store,
            clock: &RequestClock(
                now + Duration::minutes(i64::try_from(index).expect("four-element index")),
            ),
            geo: &screen,
            sanctions: &screen,
        }
        .execute(RequestWithdrawCmd {
            user,
            amount_micro: amount,
            dest: dest_a(),
            client_ip: Some(IpAddr::from([203, 0, 113, 95])),
            idempotency_key: Some(format!("pg-aml-band-{index}")),
        })
        .await
        .unwrap();
        ids.push(receipt.id.unwrap());
    }
    let reasons: serde_json::Value =
        sqlx::query_scalar("select risk_reasons from withdrawals where id=$1")
            .bind(ids[3].0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(reasons
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item.as_str() == Some("aml_open"))));
    let flags: i64 = sqlx::query_scalar(
        "select count(*) from aml_flags \
          where user_id=$1 and rule='structuring' and status='open'",
    )
    .bind(user.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(flags, 1);
}

#[tokio::test]
async fn pg_deny_has_one_exact_withheld_reversal_under_retry() {
    let _serial = pg_test_lock().lock().await;
    let pool = pg_pool().await;
    let user = reset_pg_user(&pool, 20_000_000).await;
    let now = time::OffsetDateTime::now_utc();
    let screen = screen_at(now);
    let store = PgWithdrawStore::new(pool.clone());
    let receipt = RequestWithdraw {
        store: &store,
        clock: &RequestClock(now),
        geo: &screen,
        sanctions: &screen,
    }
    .execute(RequestWithdrawCmd {
        user,
        amount_micro: 5_000_000,
        dest: dest_a(),
        client_ip: Some(IpAddr::from([203, 0, 113, 92])),
        idempotency_key: Some("pg-deny".into()),
    })
    .await
    .unwrap();
    let id = receipt.id.unwrap();
    decide_machine(&store, &RequestClock(now), id)
        .await
        .unwrap();
    let denied = DecideWithdraw {
        store: &store,
        clock: &RequestClock(now),
        actor: finance(),
    }
    .execute(DecideCmd::Deny {
        id,
        reason: "risk rejected".into(),
    })
    .await
    .unwrap();
    assert_eq!(denied.combo, Combo::W12);
    let release_tx_id = denied.release_tx_id.unwrap();
    let release_legs: Vec<(String, i64)> = sqlx::query_as(
        "select a.owner_type, e.amount_micro \
           from ledger_entries e join ledger_accounts a on a.id=e.account_id \
          where e.txn_id=$1 order by e.amount_micro",
    )
    .bind(release_tx_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        release_legs,
        vec![("withheld".into(), -5_000_000), ("user".into(), 5_000_000)]
    );
    let retry = DecideWithdraw {
        store: &store,
        clock: &RequestClock(now),
        actor: finance(),
    }
    .execute(DecideCmd::Deny {
        id,
        reason: "risk rejected".into(),
    })
    .await;
    assert!(matches!(
        retry,
        Err(application::error::AppError::IllegalTransition)
    ));
    let terminal_effects: i64 =
        sqlx::query_scalar("select count(*) from ledger_transactions where idempotency_key=$1")
            .bind(format!("withdraw-release:{}", id.0))
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(terminal_effects, 1);
}

#[tokio::test]
async fn pg_admission_and_refund_approval_race_has_one_winner_from_w1() {
    let _serial = pg_test_lock().lock().await;
    let pool = pg_pool().await;
    let user = reset_pg_user(&pool, 1_000_000).await;
    sqlx::query("update users set kyc_tier=0 where id=$1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
    let now = time::OffsetDateTime::now_utc();
    for context in ["geo", "sanctions"] {
        sqlx::query(
            "insert into sanction_screenings \
             (id,user_id,context,verdict,checked_at,expires_at,policy_version) \
             values ($1,$2,$3,'clear',$4,$5,'w1-race')",
        )
        .bind(Uuid::new_v4())
        .bind(user.0)
        .bind(context)
        .bind(now)
        .bind(now + Duration::hours(1))
        .execute(&pool)
        .await
        .unwrap();
    }
    let observation = ObservedDeposit {
        user: Some(user),
        amount: MicroUsd(5_000_000),
        chain_sig: format!("w1-admit-refund-{}", Uuid::new_v4()),
        source_address: dest_b(),
        dest_address: dest_a(),
        mint: "w1-usdc".into(),
        slot: 99,
    };
    let store = PgStore::from_pool(pool.clone());
    let finalized = observe_finalized(&store, &observation).await.unwrap();
    admit_observed(
        &store,
        &observation.chain_sig,
        now,
        &AdminContext::Machine,
        false,
    )
    .await
    .unwrap();
    sqlx::query("update users set kyc_tier=2 where id=$1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
    let proposal = propose_deposit_command(
        &store,
        finalized.deposit_id,
        true,
        "race source refund".into(),
        now,
        &finance(),
    )
    .await
    .unwrap();

    let admission_store = store.clone();
    let admission_sig = observation.chain_sig.clone();
    let admission =
        tokio::spawn(async move { reevaluate_held(&admission_store, &admission_sig, now).await });
    let refund_store = store.clone();
    let proposal_id = proposal.id;
    let refund = tokio::spawn(async move {
        confirm_deposit_refund(&refund_store, proposal_id, now, &superadmin()).await
    });
    let (admission, refund) = (admission.await.unwrap(), refund.await.unwrap());
    assert_eq!(
        usize::from(admission.is_ok()) + usize::from(refund.is_ok()),
        1
    );
    let status: String =
        sqlx::query_scalar("select machine_status from deposits where chain_sig=$1")
            .bind(&observation.chain_sig)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(matches!(status.as_str(), "admitted" | "refund_approved"));
    let selected_paths: i64 = sqlx::query_scalar(
        "select (case when admit_tx_id is null then 0 else 1 end \
                + case when exists(select 1 from outbound_payments \
                     where subject='deposit_refund' and subject_id=deposits.id) \
                       then 1 else 0 end)::bigint \
           from deposits where chain_sig=$1",
    )
    .bind(&observation.chain_sig)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(selected_paths, 1);
}

#[tokio::test]
async fn pg_concurrent_withdraw_trade_and_deposit_serialize_and_conserve() {
    let _serial = pg_test_lock().lock().await;
    let pool = pg_pool().await;
    let user = reset_pg_user(&pool, 30_000_000).await;
    let market = seed_trade_market(&pool, user).await;
    let now = time::OffsetDateTime::now_utc();
    for context in ["geo", "sanctions"] {
        sqlx::query(
            "insert into sanction_screenings \
             (id,user_id,context,verdict,checked_at,expires_at,policy_version) \
             values ($1,$2,$3,'clear',$4,$5,'w1-concurrency')",
        )
        .bind(Uuid::new_v4())
        .bind(user.0)
        .bind(context)
        .bind(now)
        .bind(now + Duration::hours(1))
        .execute(&pool)
        .await
        .unwrap();
    }
    let deposit = ObservedDeposit {
        user: Some(user),
        amount: MicroUsd(5_000_000),
        chain_sig: format!("w1-concurrent-deposit-{}", Uuid::new_v4()),
        source_address: dest_b(),
        dest_address: dest_a(),
        mint: "w1-usdc".into(),
        slot: 100,
    };
    let store = PgStore::from_pool(pool.clone());
    observe_finalized(&store, &deposit).await.unwrap();
    let generation: i64 =
        sqlx::query_scalar("select generation from config_generation where singleton=1")
            .fetch_one(&pool)
            .await
            .unwrap();

    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("select pg_advisory_xact_lock(2, hashtext($1))")
        .bind(user.0.to_string())
        .execute(&mut *blocker)
        .await
        .unwrap();

    let withdraw_store = PgWithdrawStore::new(pool.clone());
    let screen = screen_at(now);
    let withdraw = tokio::spawn(async move {
        RequestWithdraw {
            store: &withdraw_store,
            clock: &RequestClock(now),
            geo: &screen,
            sanctions: &screen,
        }
        .execute(RequestWithdrawCmd {
            user,
            amount_micro: 5_000_000,
            dest: dest_a(),
            client_ip: Some(IpAddr::from([203, 0, 113, 93])),
            idempotency_key: Some("pg-concurrent-withdraw".into()),
        })
        .await
    });
    let trade_store = store.clone();
    let trade = tokio::spawn(async move {
        PlaceTrade {
            store: &trade_store,
            clock: &RequestClock(now),
            rep_config: application::model::RepConfig::default(),
        }
        .execute(PlaceTradeCmd {
            market,
            user,
            side: Side::Yes,
            action: TradeAction::Buy,
            amount_micro: 5_000_000,
            idempotency_key: "pg-concurrent-trade".into(),
            run_id: None,
            pending_action_id: None,
            expected_config_version: Some(generation),
        })
        .await
    });
    let deposit_store = store.clone();
    let deposit_sig = deposit.chain_sig.clone();
    let admission = tokio::spawn(async move {
        admit_observed(
            &deposit_store,
            &deposit_sig,
            now,
            &AdminContext::Machine,
            false,
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    blocker.commit().await.unwrap();

    let completed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        (
            withdraw.await.unwrap(),
            trade.await.unwrap(),
            admission.await.unwrap(),
        )
    })
    .await
    .expect("user-lock contenders must not deadlock");
    assert!(completed.0.is_ok(), "withdraw failed: {:?}", completed.0);
    assert!(completed.1.is_ok(), "trade failed: {:?}", completed.1);
    assert!(completed.2.is_ok(), "deposit failed: {:?}", completed.2);
    let ledger_sum: i64 =
        sqlx::query_scalar("select coalesce(sum(amount_micro),0)::bigint from ledger_entries")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(ledger_sum, 0);
    let negative_internal: i64 = sqlx::query_scalar(
        "select count(*) from ( \
           select a.id from ledger_accounts a join ledger_entries e on e.account_id=a.id \
            where a.owner_type <> 'external' group by a.id \
           having sum(e.amount_micro) < 0 \
         ) bad",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(negative_internal, 0);
    let withheld: i64 = sqlx::query_scalar(
        "select coalesce(sum(e.amount_micro),0)::bigint \
           from ledger_accounts a join ledger_entries e on e.account_id=a.id \
          where a.owner_type='withheld' and a.currency='usdc'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(withheld, 5_000_000);
    let deposit_status: String =
        sqlx::query_scalar("select machine_status from deposits where chain_sig=$1")
            .bind(&deposit.chain_sig)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(deposit_status, "admitted");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("select count(*) from trades where user_id=$1")
            .bind(user.0)
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn pg_decision_takes_user_lock_before_withdrawal_row_lock() {
    let _serial = pg_test_lock().lock().await;
    let pool = pg_pool().await;
    let user = reset_pg_user(&pool, 20_000_000).await;
    let now = time::OffsetDateTime::now_utc();
    let screen = screen_at(now);
    let store = PgWithdrawStore::new(pool.clone());
    let receipt = RequestWithdraw {
        store: &store,
        clock: &RequestClock(now),
        geo: &screen,
        sanctions: &screen,
    }
    .execute(RequestWithdrawCmd {
        user,
        amount_micro: 5_000_000,
        dest: dest_a(),
        client_ip: Some(IpAddr::from([203, 0, 113, 94])),
        idempotency_key: Some("pg-lock-order-decision".into()),
    })
    .await
    .unwrap();
    let id = receipt.id.unwrap();

    // Model unwind/payout: user lock first, then a withdrawal row lock.
    let mut coordinator_tx = pool.begin().await.unwrap();
    sqlx::query("select pg_advisory_xact_lock(2, hashtext($1))")
        .bind(user.0.to_string())
        .execute(&mut *coordinator_tx)
        .await
        .unwrap();
    let decide_store = PgWithdrawStore::new(pool.clone());
    let decide =
        tokio::spawn(async move { decide_machine(&decide_store, &RequestClock(now), id).await });

    let mut waiting_on_user = false;
    for _ in 0..100 {
        waiting_on_user = sqlx::query_scalar(
            "select exists(select 1 from pg_locks \
              where locktype='advisory' and not granted)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        if waiting_on_user {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(waiting_on_user, "decision never reached the user lock");

    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        sqlx::query("select id from withdrawals where id=$1 for update")
            .bind(id.0)
            .execute(&mut *coordinator_tx),
    )
    .await
    .expect("user-first coordinator must not deadlock on the withdrawal row")
    .unwrap();
    coordinator_tx.commit().await.unwrap();
    let decided = tokio::time::timeout(std::time::Duration::from_secs(3), decide)
        .await
        .expect("decision must resume after the user lock releases")
        .unwrap()
        .unwrap();
    assert_eq!(decided.combo, Combo::W4);
}

#[tokio::test]
async fn pg_migration_enforces_outbound_and_pending_proposal_uniqueness() {
    let _serial = pg_test_lock().lock().await;
    let pool = pg_pool().await;
    let indexes: Vec<String> = sqlx::query_scalar(
        "select indexname from pg_indexes where schemaname='public' \
          and tablename in ('outbound_send_attempts','money_command_proposals')",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    for required in [
        "outbound_send_attempts_one_live_uk",
        "outbound_send_attempts_one_finalized_uk",
        "outbound_send_attempts_signature_uk",
        "outbound_send_attempts_replacement_uk",
        "money_command_proposals_one_pending_subject_kind_uk",
    ] {
        assert!(
            indexes.iter().any(|name| name == required),
            "0011 is missing {required}"
        );
    }
}
