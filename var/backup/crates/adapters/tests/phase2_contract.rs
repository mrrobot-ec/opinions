#![allow(
    clippy::expect_used,
    clippy::needless_raw_string_hashes,
    clippy::too_many_lines,
    clippy::unwrap_used
)]

use std::sync::Arc;

use adapters::http::{router, AppState};
use adapters::pg::PgStore;
use adapters::relay::OutboxRelay;
use application::advance_market::{AdvanceMarket, AdvanceMarketCmd};
use application::cast_vote::{CastVote, CastVoteCmd};
use application::create_user::{CreateUser, CreateUserCmd};
use application::credit_deposit::{CreditDeposit, CreditDepositCmd};
use application::ensure_genesis::{EnsureGenesis, EnsureGenesisCmd};
use application::error::{AppError, StoreError};
use application::model::AdminContext;
use application::model::{
    Event, MarketId, ResolveConfig, TradeAction, UserId, VoteIntegrityConfig,
};
use application::place_trade::{PlaceTrade, PlaceTradeCmd};
use application::ports::{Clock, MarketQueries, NoopCrashPoint, Store};
use application::resolve_market::{ResolveMarket, ResolveMarketCmd};
use application::seed_market::{SeedMarket, SeedMarketCmd};
use domain::amm::Side;
use domain::ledger::Currency;
use domain::market::MarketEvent;
use domain::money::{BasisPoints, MicroUsd};
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use tokio::sync::oneshot;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::{connect, Message};
use uuid::Uuid;

mod common;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

struct TestDb {
    store: PgStore,
    control: PgPool,
    database: String,
}

impl TestDb {
    async fn new(label: &str) -> Self {
        let url = common::database_url();
        let control = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .unwrap();
        let database = format!("opinions_{label}_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("create database {database}"))
            .execute(&control)
            .await
            .unwrap();
        let (base, query) = url
            .split_once('?')
            .map_or((url.as_str(), None), |parts| (parts.0, Some(parts.1)));
        let root = base.rsplit_once('/').unwrap().0;
        let test_url = query.map_or_else(
            || format!("{root}/{database}"),
            |query| format!("{root}/{database}?{query}"),
        );
        let store = PgStore::connect(&test_url).await.unwrap();
        MIGRATOR.run(store.pool_handle()).await.unwrap();
        Self {
            store,
            control,
            database,
        }
    }

    async fn cleanup(self) {
        self.store.pool_handle().close().await;
        sqlx::query(&format!("drop database {} with (force)", self.database))
            .execute(&self.control)
            .await
            .unwrap();
        self.control.close().await;
    }
}

#[derive(Clone)]
struct FixedClock(OffsetDateTime);

impl Clock for FixedClock {
    fn now(&self) -> OffsetDateTime {
        self.0
    }
}

#[derive(Clone, Copy)]
struct MarketFixture {
    market: MarketId,
    yes: Uuid,
    no: Uuid,
}

async fn insert_market(
    store: &PgStore,
    state: &str,
    opens_at: OffsetDateTime,
    tally_hidden_at: OffsetDateTime,
    closes_at: OffsetDateTime,
) -> MarketFixture {
    let market = MarketId(Uuid::new_v4());
    let yes = Uuid::new_v4();
    let no = Uuid::new_v4();
    let pool = Uuid::new_v4();
    let slug = format!("phase2-{}", market.0.simple());
    sqlx::query(
        r#"
        insert into markets
            (id, slug, question, status, min_votes_to_resolve,
             opens_at, closes_at, tally_hidden_at)
        values ($1, $2, $3, $4, 2, $5, $6, $7)
        "#,
    )
    .bind(market.0)
    .bind(&slug)
    .bind(format!("Will {slug} resolve?"))
    .bind(state)
    .bind(opens_at)
    .bind(closes_at)
    .bind(tally_hidden_at)
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
    MarketFixture { market, yes, no }
}

async fn insert_user(store: &PgStore, created_at: OffsetDateTime, phone: bool) -> UserId {
    let user = UserId(Uuid::new_v4());
    sqlx::query("insert into users (id, handle, created_at) values ($1, $2, $3)")
        .bind(user.0)
        .bind(format!("user-{}", user.0.simple()))
        .bind(created_at)
        .execute(store.pool_handle())
        .await
        .unwrap();
    sqlx::query("insert into reputation (user_id, rep_micro, tier) values ($1, 0, 0)")
        .bind(user.0)
        .execute(store.pool_handle())
        .await
        .unwrap();
    if phone {
        sqlx::query(
            "insert into user_channels (user_id, channel, address) values ($1, 'imessage', $2)",
        )
        .bind(user.0)
        .bind(format!("+1{}", &user.0.simple().to_string()[..10]))
        .execute(store.pool_handle())
        .await
        .unwrap();
    }
    user
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
               values ($1, $2, $3, 'clear', $4, $5, 'phase2-test')"#,
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

async fn insert_vote(store: &PgStore, fixture: MarketFixture, user: UserId, seq: i64) {
    sqlx::query(
        r#"
        insert into votes
            (id, user_id, market_id, outcome_id, crowd_guess_pct, seq, idempotency_key)
        values ($1, $2, $3, $4, 60, $5, $6)
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(user.0)
    .bind(fixture.market.0)
    .bind(fixture.yes)
    .bind(seq)
    .bind(format!("fixture-vote-{}", Uuid::new_v4()))
    .execute(store.pool_handle())
    .await
    .unwrap();
}

#[tokio::test]
async fn lifecycle_commands_are_durable_and_failed_attempts_retry() {
    let db = TestDb::new("lifecycle").await;
    let now = OffsetDateTime::now_utc();
    let fixture = insert_market(
        &db.store,
        "scheduled",
        now - Duration::hours(1),
        now + Duration::hours(1),
        now + Duration::hours(2),
    )
    .await;
    let use_case = AdvanceMarket { store: &db.store };
    let command = AdvanceMarketCmd {
        market: fixture.market,
        event: MarketEvent::GoLive,
        idempotency_key: format!("race:{}:live", fixture.market.0),
    };
    let (first, second) =
        tokio::join!(use_case.execute(command.clone()), use_case.execute(command));
    let (first, second) = (first.unwrap(), second.unwrap());
    assert!(first.replayed ^ second.replayed);

    let close = AdvanceMarketCmd {
        market: fixture.market,
        event: MarketEvent::Close,
        idempotency_key: format!("retry:{}:close", fixture.market.0),
    };
    assert_eq!(
        use_case.execute(close.clone()).await.unwrap_err(),
        AppError::IllegalTransition
    );
    use_case
        .execute(AdvanceMarketCmd {
            market: fixture.market,
            event: MarketEvent::EnterCloseWindow,
            idempotency_key: format!("prepare:{}:closing", fixture.market.0),
        })
        .await
        .unwrap();
    assert!(!use_case.execute(close).await.unwrap().replayed);
    let commands: i64 =
        sqlx::query_scalar("select count(*)::bigint from lifecycle_commands where market_id = $1")
            .bind(fixture.market.0)
            .fetch_one(db.store.pool_handle())
            .await
            .unwrap();
    assert_eq!(commands, 3);
    db.cleanup().await;
}

#[tokio::test]
async fn curator_flag_has_one_atomic_winner_and_one_event() {
    let db = TestDb::new("curator").await;
    let now = OffsetDateTime::now_utc();
    let fixture = insert_market(
        &db.store,
        "closed",
        now - Duration::hours(3),
        now - Duration::hours(2),
        now - Duration::hours(1),
    )
    .await;
    sqlx::query("update markets set status = 'resolving' where id = $1")
        .bind(fixture.market.0)
        .execute(db.store.pool_handle())
        .await
        .unwrap();
    let flag = |store: PgStore, key: String| async move {
        let mut tx = store.resolve_tx().await.unwrap();
        tx.serialize_key(&key).await.unwrap();
        tx.market_for_update(fixture.market).await.unwrap();
        let won = tx.flag_curator_needed(fixture.market).await.unwrap();
        if won {
            tx.append(Event {
                event_type: "CuratorNeeded",
                aggregate_type: "market",
                aggregate_id: fixture.market.0,
                payload: serde_json::json!({"market_id": fixture.market.0}),
            })
            .await
            .unwrap();
        }
        tx.commit().await.unwrap();
        won
    };
    let (first, second) = tokio::join!(
        flag(db.store.clone(), format!("flag-a-{}", fixture.market.0)),
        flag(db.store.clone(), format!("flag-b-{}", fixture.market.0))
    );
    assert!(first ^ second);
    let events: i64 = sqlx::query_scalar(
        "select count(*)::bigint from events_outbox where aggregate_id = $1 and event_type = 'CuratorNeeded'",
    )
    .bind(fixture.market.0)
    .fetch_one(db.store.pool_handle())
    .await
    .unwrap();
    assert_eq!(events, 1);
    db.cleanup().await;
}

#[tokio::test]
async fn user_lock_enforces_the_cross_market_vote_boundary() {
    let db = TestDb::new("vote_lock").await;
    let now = OffsetDateTime::now_utc();
    let first = insert_market(
        &db.store,
        "live",
        now - Duration::hours(1),
        now + Duration::hours(1),
        now + Duration::hours(2),
    )
    .await;
    let second = insert_market(
        &db.store,
        "live",
        now - Duration::hours(1),
        now + Duration::hours(1),
        now + Duration::hours(2),
    )
    .await;
    let user = insert_user(&db.store, now - Duration::hours(100), true).await;
    seed_money_clear(&db.store, user, now).await;
    let clock = FixedClock(now);
    let use_case = CastVote {
        store: &db.store,
        clock: &clock,
        config: VoteIntegrityConfig {
            max_votes_per_window: 1,
            ..VoteIntegrityConfig::default()
        },
    };
    let command = |market, key: &str| CastVoteCmd {
        market,
        user,
        side: Side::Yes,
        crowd_guess_pct: 60,
        idempotency_key: key.to_string(),
        cast_ip: None,
        device_hash: None,
    };
    let (left, right) = tokio::join!(
        use_case.execute(command(first.market, "last-slot-a")),
        use_case.execute(command(second.market, "last-slot-b"))
    );
    assert!(left.is_ok() ^ right.is_ok());
    let failure = if left.is_err() { left } else { right }.unwrap_err();
    assert_eq!(failure, AppError::VoteVelocityExceeded);
    let votes: i64 = sqlx::query_scalar("select count(*)::bigint from votes where user_id = $1")
        .bind(user.0)
        .fetch_one(db.store.pool_handle())
        .await
        .unwrap();
    assert_eq!(votes, 1);
    db.cleanup().await;
}

#[tokio::test]
async fn snapshot_due_queries_chart_and_tape_obey_phase2_rules() {
    let db = TestDb::new("queries").await;
    let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
    let live = insert_market(
        &db.store,
        "live",
        now - Duration::hours(1),
        now,
        now + Duration::hours(1),
    )
    .await;
    let user = insert_user(&db.store, now - Duration::hours(100), true).await;
    insert_vote(&db.store, live, user, 1).await;
    let visible = db
        .store
        .market_snapshot(live.market, now - Duration::SECOND)
        .await
        .unwrap();
    assert_eq!(visible.tally.unwrap().yes_votes, 1);
    assert!(db
        .store
        .market_snapshot(live.market, now)
        .await
        .unwrap()
        .tally
        .is_none());
    let market_row = db
        .store
        .market_by_ref(&live.market.0.to_string())
        .await
        .unwrap();
    assert!(market_row.question.starts_with("Will phase2-"));

    for (state, opens, hidden, closes) in [
        (
            "scheduled",
            now - Duration::minutes(4),
            now + Duration::hours(1),
            now + Duration::hours(2),
        ),
        (
            "closing",
            now - Duration::hours(2),
            now - Duration::hours(1),
            now - Duration::minutes(2),
        ),
        (
            "closed",
            now - Duration::hours(3),
            now - Duration::hours(2),
            now - Duration::hours(1),
        ),
    ] {
        insert_market(&db.store, state, opens, hidden, closes).await;
    }
    let due = db.store.due_markets(now, 256).await.unwrap();
    assert_eq!(due.len(), 4);

    let trades = [
        (live.yes, "buy", 400_i64, 1_000_i64, 1_i64, now),
        (live.no, "sell", 100, 500, 2, now + Duration::seconds(10)),
        (live.yes, "buy", 900, 1_000, 3, now + Duration::seconds(65)),
        (
            live.yes,
            "buy",
            999_999_999_999_999,
            1_000_000_000_000_000,
            4,
            now + Duration::seconds(125),
        ),
    ];
    for (outcome, action, collateral, shares, seq, created_at) in trades {
        sqlx::query(
            r#"
            insert into trades
                (id, user_id, market_id, outcome_id, side, collateral_micro,
                 shares_micro, fee_micro, seq, created_at)
            values ($1, $2, $3, $4, $5, $6, $7, 0, $8, $9)
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(user.0)
        .bind(live.market.0)
        .bind(outcome)
        .bind(action)
        .bind(collateral)
        .bind(shares)
        .bind(seq)
        .bind(created_at)
        .execute(db.store.pool_handle())
        .await
        .unwrap();
    }
    let history = db.store.price_history(live.market, 60, now).await.unwrap();
    assert_eq!(history.len(), 3);
    assert_eq!(history[0].avg_price_micro, 480_000);
    assert_eq!(history[0].volume_micro, 500);
    assert_eq!(history[0].trades, 2);
    assert_eq!(history[1].avg_price_micro, 900_000);
    assert_eq!(history[2].avg_price_micro, 999_999);

    sqlx::query(
        r#"
        insert into trades
            (id, user_id, market_id, outcome_id, side, collateral_micro,
             shares_micro, fee_micro, seq, created_at)
        select gen_random_uuid(), $1, $2, $3, 'buy', 1, 2, 0, n,
               $4 + n * interval '1 second'
          from generate_series(5, 209) as n
        "#,
    )
    .bind(user.0)
    .bind(live.market.0)
    .bind(live.yes)
    .bind(now + Duration::minutes(10))
    .execute(db.store.pool_handle())
    .await
    .unwrap();
    let tape = db.store.tape(live.market, 500).await.unwrap();
    assert_eq!(tape.len(), 200);
    assert_eq!(tape[0].trade_seq, 209);
    assert_eq!(tape.last().unwrap().trade_seq, 10);
    assert_eq!(db.store.tape(live.market, 2).await.unwrap().len(), 2);
    db.cleanup().await;
}

#[tokio::test]
async fn settlement_projects_payouts_into_positions_and_replays_cleanly() {
    let db = TestDb::new("settlement").await;
    let now = OffsetDateTime::now_utc();
    let clock = FixedClock(now);
    EnsureGenesis { store: &db.store }
        .execute(EnsureGenesisCmd {
            currency: Currency::Usdc,
            amount: MicroUsd(2_000_000_000),
        })
        .await
        .unwrap();
    let market = MarketId(Uuid::new_v4());
    SeedMarket {
        store: &db.store,
        clock: &clock,
        rep_config: application::model::RepConfig::default(),
        lp_kill_config: application::model::LpKillConfig::default(),
    }
    .execute(SeedMarketCmd {
        market_id: market,
        slug: format!("settlement-{}", market.0.simple()),
        min_votes_to_resolve: 1,
        closes_at: now + Duration::hours(2),
        tally_hidden_at: now + Duration::hours(1),
        fee: BasisPoints(0),
        seed: MicroUsd(1_000_000_000),
        idempotency_key: format!("seed-{}", market.0),
        force: false,
    })
    .await
    .unwrap();
    AdvanceMarket { store: &db.store }
        .execute(AdvanceMarketCmd {
            market,
            event: MarketEvent::GoLive,
            idempotency_key: format!("live-{}", market.0),
        })
        .await
        .unwrap();
    let user = insert_user(&db.store, now - Duration::hours(100), true).await;
    seed_money_clear(&db.store, user, now).await;
    CastVote {
        store: &db.store,
        clock: &clock,
        config: VoteIntegrityConfig::default(),
    }
    .execute(CastVoteCmd {
        market,
        user,
        side: Side::Yes,
        crowd_guess_pct: 80,
        idempotency_key: format!("vote-{}", user.0),
        cast_ip: None,
        device_hash: None,
    })
    .await
    .unwrap();
    CreditDeposit { store: &db.store }
        .execute(CreditDepositCmd {
            user,
            amount: MicroUsd(100_000_000),
            chain_sig: format!("deposit-{}", user.0),
            idempotency_key: format!("deposit-{}", user.0),
        })
        .await
        .unwrap();
    PlaceTrade {
        store: &db.store,
        clock: &clock,
        rep_config: application::model::RepConfig::default(),
    }
    .execute(PlaceTradeCmd {
        market,
        user,
        side: Side::Yes,
        action: TradeAction::Buy,
        amount_micro: 50_000_000,
        idempotency_key: format!("trade-{}", user.0),
        run_id: None,
        pending_action_id: None,
        expected_config_version: Some(1),
    })
    .await
    .unwrap();
    let before = db.store.positions(user).await.unwrap()[0];
    let trade_payload: Value = sqlx::query_scalar(
        "select payload from events_outbox where aggregate_id = $1 and event_type = 'TradePlaced'",
    )
    .bind(market.0)
    .fetch_one(db.store.pool_handle())
    .await
    .unwrap();
    assert!(trade_payload
        .get("handle")
        .and_then(Value::as_str)
        .is_some());
    assert!(trade_payload
        .get("trade_seq")
        .and_then(Value::as_i64)
        .is_some());
    assert!(trade_payload
        .get("created_at")
        .and_then(Value::as_str)
        .is_some());

    for (event, key) in [
        (MarketEvent::EnterCloseWindow, "closing"),
        (MarketEvent::Close, "closed"),
    ] {
        AdvanceMarket { store: &db.store }
            .execute(AdvanceMarketCmd {
                market,
                event,
                idempotency_key: format!("{key}-{market:?}"),
            })
            .await
            .unwrap();
    }
    let resolver = ResolveMarket {
        crash_point: &NoopCrashPoint,
        actor: AdminContext::Machine,
        store: &db.store,
        clock: &clock,
        config: ResolveConfig {
            oi_floor: MicroUsd(0),
        },
        rep_config: application::model::RepConfig::default(),
        integrity_config: application::model::IntegritySweepConfig::default(),
    };
    let first = resolver
        .execute(ResolveMarketCmd {
            market,
            curator_override: None,
        })
        .await
        .unwrap();
    let fact: (String, i64) = sqlx::query_as(
        "select source, realized_delta_micro from realizations where txn_id = $1 and user_id = $2",
    )
    .bind(first.ledger_txn.unwrap())
    .bind(user.0)
    .fetch_one(db.store.pool_handle())
    .await
    .unwrap();
    assert_eq!(fact.0, "settlement");
    let leaders = db
        .store
        .top_traders(now - Duration::seconds(1), now + Duration::seconds(1), 10)
        .await
        .unwrap();
    assert!(leaders.iter().any(|row| row.realized_pnl_micro == fact.1));
    let lp: (Option<i64>, Option<OffsetDateTime>) =
        sqlx::query_as("select lp_pnl_micro, settled_at from markets where id = $1")
            .bind(market.0)
            .fetch_one(db.store.pool_handle())
            .await
            .unwrap();
    assert!(lp.0.is_some());
    assert_eq!(lp.1, Some(now));
    let settled = db.store.positions(user).await.unwrap()[0];
    assert_eq!(settled.shares.0, 0);
    assert_eq!(settled.cost.0, 0);
    assert_ne!(settled.realized_pnl.0, before.realized_pnl.0);
    let replay = resolver
        .execute(ResolveMarketCmd {
            market,
            curator_override: None,
        })
        .await
        .unwrap();
    assert!(replay.replayed);
    let realization_count: i64 =
        sqlx::query_scalar("select count(*)::bigint from realizations where market_id = $1")
            .bind(market.0)
            .fetch_one(db.store.pool_handle())
            .await
            .unwrap();
    assert_eq!(realization_count, 1, "settlement replay is a unique no-op");
    assert_eq!(db.store.positions(user).await.unwrap()[0], settled);
    assert_eq!(first.ledger_txn, replay.ledger_txn);
    db.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tcp_websocket_gets_snapshot_then_live_price_and_trade() {
    let db = TestDb::new("websocket").await;
    let now = OffsetDateTime::now_utc();
    let fixture = insert_market(
        &db.store,
        "live",
        now - Duration::hours(1),
        now + Duration::hours(1),
        now + Duration::hours(2),
    )
    .await;
    let user = insert_user(&db.store, now - Duration::hours(100), true).await;
    seed_money_clear(&db.store, user, now).await;
    insert_vote(&db.store, fixture, user, 1).await;
    let ignored_market = insert_market(
        &db.store,
        "live",
        now - Duration::hours(1),
        now + Duration::hours(1),
        now + Duration::hours(2),
    )
    .await;
    CreditDeposit { store: &db.store }
        .execute(CreditDepositCmd {
            user,
            amount: MicroUsd(50_000_000),
            chain_sig: format!("ws-deposit-{}", user.0),
            idempotency_key: format!("ws-deposit-{}", user.0),
        })
        .await
        .unwrap();
    sqlx::query("update events_outbox set published_at = now()")
        .execute(db.store.pool_handle())
        .await
        .unwrap();

    let state = AppState::with_tokens(
        db.store.clone(),
        Arc::new(FixedClock(now)),
        "demo-token",
        "admin-token",
    );
    let event_sender = state.event_sender();
    let relay = OutboxRelay::new(db.store.pool_handle().clone(), state.event_sender());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    let (ready_tx, ready_rx) = oneshot::channel();
    let market = fixture.market.0;
    let client = tokio::task::spawn_blocking(move || {
        let (mut socket, _) = connect(format!("ws://{address}/ws").as_str()).unwrap();
        socket
            .send(Message::Text(
                serde_json::json!({"op":"subscribe", "market_id":market})
                    .to_string()
                    .into(),
            ))
            .unwrap();
        let snapshot: Value =
            serde_json::from_str(&socket.read().unwrap().into_text().unwrap()).unwrap();
        socket
            .send(Message::Text(
                serde_json::json!({"op":"unsubscribe", "market_id":market})
                    .to_string()
                    .into(),
            ))
            .unwrap();
        socket
            .send(Message::Text(
                serde_json::json!({"op":"subscribe", "market_id":market})
                    .to_string()
                    .into(),
            ))
            .unwrap();
        let resnapshot: Value =
            serde_json::from_str(&socket.read().unwrap().into_text().unwrap()).unwrap();
        assert_eq!(resnapshot, snapshot);
        ready_tx.send(snapshot).unwrap();
        let first: Value =
            serde_json::from_str(&socket.read().unwrap().into_text().unwrap()).unwrap();
        let second: Value =
            serde_json::from_str(&socket.read().unwrap().into_text().unwrap()).unwrap();
        [first, second]
    });
    let snapshot = timeout(std::time::Duration::from_secs(5), ready_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot["type"], "snapshot");
    assert_eq!(snapshot["market_id"], market.to_string());
    assert_eq!(snapshot["v"], 1);
    event_sender
        .send(adapters::relay::BusEvent::Market(
            adapters::relay::WireEvent {
                outbox_seq: -1,
                event_type: "MarketSeeded".to_string(),
                aggregate_type: "market".to_string(),
                aggregate_id: ignored_market.market.0,
                payload: serde_json::json!({}),
            },
        ))
        .unwrap();
    let trade_clock = FixedClock(now);
    PlaceTrade {
        store: &db.store,
        clock: &trade_clock,
        rep_config: application::model::RepConfig::default(),
    }
    .execute(PlaceTradeCmd {
        market: fixture.market,
        user,
        side: Side::Yes,
        action: TradeAction::Buy,
        amount_micro: 5_000_000,
        idempotency_key: format!("ws-trade-{}", user.0),
        run_id: None,
        pending_action_id: None,
        expected_config_version: Some(1),
    })
    .await
    .unwrap();
    assert_eq!(relay.pump_once().await.unwrap(), 1);
    assert_eq!(relay.pump_once().await.unwrap(), 0);
    let frames = timeout(std::time::Duration::from_secs(5), client)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frames[0]["type"], "price");
    assert_eq!(frames[1]["type"], "trade");
    assert_eq!(frames[0]["outbox_seq"], frames[1]["outbox_seq"]);
    assert_eq!(frames[1]["handle"], format!("user-{}", user.0.simple()));
    assert!(frames[1].get("trade_seq").is_some());
    server.abort();
    let _ = server.await;
    drop(relay);
    db.cleanup().await;
}

#[tokio::test]
async fn public_query_errors_and_owner_shapes_are_checked() {
    let db = TestDb::new("query_errors").await;
    let unknown = MarketId(Uuid::new_v4());
    assert!(db
        .store
        .market_snapshot(unknown, OffsetDateTime::now_utc())
        .await
        .is_err());
    assert!(db
        .store
        .price_history(unknown, 0, OffsetDateTime::UNIX_EPOCH)
        .await
        .is_err());
    assert!(db.store.tape(unknown, 50).await.unwrap().is_empty());
    let missing = db.store.positions(UserId(Uuid::new_v4())).await.unwrap();
    assert!(missing.is_empty());
    assert!(db
        .store
        .user_by_channel("imessage", "+15550000000")
        .await
        .unwrap()
        .is_none());
    db.cleanup().await;
}

#[tokio::test]
async fn pg_identity_missing_user_and_curator_clear_paths_are_observable() {
    let db = TestDb::new("identity_paths").await;
    let creator = CreateUser { store: &db.store };
    let user = creator
        .execute(CreateUserCmd::plain(
            &format!("identity-{}", Uuid::new_v4().simple()),
            Some(("imessage".to_string(), "+15550009999".to_string())),
            &format!("identity-{}", Uuid::new_v4()),
        ))
        .await
        .unwrap();
    assert_eq!(
        db.store
            .user_by_channel("imessage", "+15550009999")
            .await
            .unwrap(),
        Some(user)
    );
    let duplicate = creator
        .execute(CreateUserCmd::plain(
            &format!("duplicate-{}", Uuid::new_v4().simple()),
            Some(("imessage".to_string(), "+15550009999".to_string())),
            &format!("identity-{}", Uuid::new_v4()),
        ))
        .await
        .unwrap();
    assert_eq!(duplicate, user);

    let mut vote_tx = db.store.vote_tx().await.unwrap();
    assert_eq!(
        vote_tx.user_created_at(UserId(Uuid::new_v4())).await,
        Err(StoreError::NotFound("user"))
    );
    drop(vote_tx);

    let now = OffsetDateTime::now_utc();
    let market = insert_market(
        &db.store,
        "closed",
        now - Duration::hours(3),
        now - Duration::hours(2),
        now - Duration::hours(1),
    )
    .await
    .market;
    sqlx::query("update markets set curator_flagged_at = now() where id = $1")
        .bind(market.0)
        .execute(db.store.pool_handle())
        .await
        .unwrap();
    let mut resolve_tx = db.store.resolve_tx().await.unwrap();
    resolve_tx.clear_curator_flag(market).await.unwrap();
    resolve_tx.commit().await.unwrap();
    let cleared: bool =
        sqlx::query_scalar("select curator_flagged_at is null from markets where id = $1")
            .bind(market.0)
            .fetch_one(db.store.pool_handle())
            .await
            .unwrap();
    assert!(cleared);
    db.cleanup().await;
}
