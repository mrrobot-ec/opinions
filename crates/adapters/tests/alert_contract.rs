//! W4 D35 durable alert outbox contract. Runs against its own scratch
//! database derived from `DATABASE_URL`, so a stray run can never truncate
//! another lane's schema. It cannot skip: an absent database FAILS the suite.
//!
//! Every `PgAlertStore` method is exercised against real SQL: the incident-key
//! encode/decode round trip (episodes legally contain `:`), `find_open` over
//! `open|acked` but never `resolved`, the full `Incident` round trip including
//! the three bookkeeping timestamps, the at-least-once `pending_delivery`
//! queue, and the ack-does-not-mark-delivered rule that makes `last_paged_at`
//! a column of its own rather than an alias of `updated_at`.
//!
//! It also proves D35's "dedup within an open incident" end to end against two
//! genuinely concurrent `raise` calls, not merely against the DDL.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use adapters::pg::{PgAlertStore, PgStore};
use application::ops::alerts::{
    AlertStore, Incident, IncidentKey, IncidentManager, IncidentStatus, RecordingAlerter,
    DETECTOR_INVARIANT, DETECTOR_RECON,
};
use application::ports::Store;
use time::OffsetDateTime;
use uuid::Uuid;

mod common;

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
            let name = "opinions_suite_alert";
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
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect to the suite scratch database")
}

fn pg_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

async fn reset(pool: &sqlx::PgPool) {
    sqlx::query("truncate table alert_outbox restart identity cascade")
        .execute(pool)
        .await
        .unwrap();
}

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

fn incident(key: IncidentKey, at: OffsetDateTime) -> Incident {
    Incident {
        id: Uuid::new_v4(),
        key,
        severity: "crit".into(),
        body: "signed residual 1 micro at the pinned cut".into(),
        status: IncidentStatus::Open,
        delivery_attempts: 1,
        last_paged_at: Some(at),
        acked_at: None,
        resolved_at: None,
    }
}

#[tokio::test]
async fn pg_alert_outbox_round_trips_every_incident_field_and_key_part() {
    let pool = pg_pool().await;
    let _guard = pg_test_lock().lock().await;
    reset(&pool).await;
    let store = PgAlertStore::from_pool(pool.clone());

    // The episode legally contains a colon (`residual:<n>` from D35's
    // reconciliation detector), so decoding must split into exactly 3 parts.
    let key = IncidentKey::new(DETECTOR_RECON, "cut-1786686052", "residual:1");
    assert_eq!(
        key.encoded(),
        "reconciliation_residual:cut-1786686052:residual:1"
    );
    assert!(store.find_open(&key).await.unwrap().is_none());

    let opened = incident(key.clone(), now());
    store.insert(opened.clone()).await.unwrap();
    let found = store.find_open(&key).await.unwrap().expect("open incident");
    assert_eq!(found.id, opened.id);
    assert_eq!(
        found.key, key,
        "detector/subject/episode survive one text column"
    );
    assert_eq!(found.severity, "crit");
    assert_eq!(found.body, opened.body);
    assert_eq!(found.status, IncidentStatus::Open);
    assert_eq!(found.delivery_attempts, 1);
    assert_eq!(found.last_paged_at, Some(now()));
    assert_eq!(found.acked_at, None);
    assert_eq!(found.resolved_at, None);

    // An acked incident is still open for dedup; a resolved one is not, so a
    // recurrence after recovery re-pages instead of deduping into the corpse.
    let mut acked = found;
    acked.status = IncidentStatus::Acked;
    acked.acked_at = Some(now() + time::Duration::seconds(30));
    store.save(&acked).await.unwrap();
    let reread = store.find_open(&key).await.unwrap().expect("acked is open");
    assert_eq!(reread.status, IncidentStatus::Acked);
    assert_eq!(reread.acked_at, acked.acked_at);

    let mut resolved = reread;
    resolved.status = IncidentStatus::Resolved;
    resolved.resolved_at = Some(now() + time::Duration::seconds(90));
    store.save(&resolved).await.unwrap();
    assert!(
        store.find_open(&key).await.unwrap().is_none(),
        "a resolved incident never dedups a recurrence"
    );
    let count: i64 = sqlx::query_scalar("select count(*) from alert_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        count, 1,
        "save updates in place; it never inserts a second row"
    );
}

#[tokio::test]
async fn pg_pending_delivery_is_at_least_once_and_an_ack_never_marks_delivered() {
    let pool = pg_pool().await;
    let _guard = pg_test_lock().lock().await;
    reset(&pool).await;
    let store = PgAlertStore::from_pool(pool.clone());

    // An incident enqueued without a page (crash between insert and page) is
    // the whole point of the durable outbox.
    let undelivered = IncidentKey::new(DETECTOR_INVARIANT, "suite", "ep-1");
    let mut row = incident(undelivered.clone(), now());
    row.delivery_attempts = 0;
    row.last_paged_at = None;
    store.insert(row).await.unwrap();

    // A paged incident is not pending.
    let delivered = IncidentKey::new(DETECTOR_INVARIANT, "suite", "ep-2");
    store
        .insert(incident(delivered.clone(), now()))
        .await
        .unwrap();

    let pending = store.pending_delivery().await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].key, undelivered);
    assert_eq!(pending[0].last_paged_at, None);

    // THE REGRESSION THIS COLUMN EXISTS FOR: acking an unpaged incident must
    // NOT retire it from the redelivery queue. If `last_paged_at` aliased
    // `updated_at`, this save would silently drop the page forever.
    let mut acked = pending[0].clone();
    acked.status = IncidentStatus::Acked;
    acked.acked_at = Some(now() + time::Duration::seconds(5));
    store.save(&acked).await.unwrap();
    let after_ack = store.find_open(&undelivered).await.unwrap().unwrap();
    assert_eq!(
        after_ack.last_paged_at, None,
        "an ack is not a page: last_paged_at is not updated_at"
    );

    // The manager's pump drives it end to end against real SQL.
    reset(&pool).await;
    let alerter = RecordingAlerter::new();
    let manager = IncidentManager {
        store: PgAlertStore::from_pool(pool.clone()),
        alerter: &alerter,
    };
    let key = IncidentKey::new(DETECTOR_RECON, "cut-2", "residual:1");
    let raised = manager
        .raise(key.clone(), "crit", "1 micro", now())
        .await
        .unwrap();
    assert_eq!(raised.delivery_attempts, 1);
    assert_eq!(alerter.len(), 1);
    // Dedup: a second raise inside the open incident does not page again.
    let deduped = manager
        .raise(key.clone(), "crit", "1 micro", now())
        .await
        .unwrap();
    assert_eq!(deduped.id, raised.id);
    assert_eq!(alerter.len(), 1);
    // It was paged at raise, so nothing is pending.
    assert_eq!(manager.deliver_pending(now()).await.unwrap(), 0);
    manager.ack(&key, now()).await.unwrap();
    manager.resolve(&key, now()).await.unwrap();
    assert!(
        matches!(
            manager.ack(&key, now()).await,
            Err(application::error::StoreError::NotFound("alert incident"))
        ),
        "a resolved incident cannot be acked"
    );
    // Recurrence after recovery re-pages as a new incident row.
    let recurrence = manager
        .raise(key.clone(), "crit", "1 micro again", now())
        .await
        .unwrap();
    assert_ne!(recurrence.id, raised.id);
    assert_eq!(alerter.len(), 2);
    let rows: i64 = sqlx::query_scalar("select count(*) from alert_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        rows, 2,
        "the resolved episode is kept; the recurrence is its own row"
    );
}

#[tokio::test]
async fn pg_alert_store_is_constructible_from_the_shared_store_handle() {
    let pool = pg_pool().await;
    let _guard = pg_test_lock().lock().await;
    reset(&pool).await;
    let store = PgStore::from_pool(pool.clone());
    let alerts = PgAlertStore::from_store(&store);
    let key = IncidentKey::new(DETECTOR_INVARIANT, "suite", "ep-shared");
    alerts.insert(incident(key.clone(), now())).await.unwrap();
    assert!(alerts.find_open(&key).await.unwrap().is_some());
    // The composition in main.rs clones one handle per consumer.
    let cloned = alerts.clone();
    assert!(cloned.find_open(&key).await.unwrap().is_some());
    let _: &dyn Store = &store;
}

/// Shared, cloneable page counter. `RecordingAlerter` cannot be moved into two
/// tasks, and the point of this test is that BOTH raises see the same alerter:
/// counting pages per-manager would hide a duplicate page entirely.
#[derive(Clone, Default)]
struct CountingAlerter {
    pages: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl application::ports::Alerter for CountingAlerter {
    async fn page(
        &self,
        _severity: &str,
        _key: &str,
        _body: &str,
    ) -> Result<(), application::error::StoreError> {
        self.pages.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

/// Two concurrent `raise` calls for ONE incident key must open ONE incident and
/// emit ONE page.
///
/// D35 makes "dedup within an open incident" normative, and the old
/// `find_open` + `insert` could not deliver it: those are two autocommit
/// statements on two pooled connections, so both callers read "absent", both
/// insert, and the operator is paged twice for one condition. Raising the
/// isolation level cannot help — there is no shared transaction to isolate.
///
/// The overlap is FORCED, not hoped for. A control transaction holds
/// `LOCK TABLE alert_outbox IN SHARE MODE`, which conflicts with the ROW
/// EXCLUSIVE an INSERT needs while leaving `SELECT` free, so both raises get
/// past their reads and park at the write. The test waits until Postgres itself
/// reports two lock-waiting backends before releasing. Without that barrier
/// this assertion would pass most of the time whether or not the bug were
/// present, which is worse than not having it.
///
/// This is the end-to-end proof that the DDL test cannot give: it exercises
/// `IncidentManager::raise` -> `PgAlertStore::insert_if_absent` -> the partial
/// unique index -> the loser's re-read, and asserts the observable contract
/// (one row, one page, both callers holding the winner) rather than the schema.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_raises_open_one_incident_and_page_once() {
    let pool = pg_pool().await;
    let _guard = pg_test_lock().lock().await;
    reset(&pool).await;

    let key = IncidentKey::new(DETECTOR_INVARIANT, "suite", "concurrent");
    let alerter = CountingAlerter::default();

    // Barrier: blocks INSERTs on alert_outbox, leaves the reads alone.
    let mut blocker = pool.acquire().await.unwrap();
    sqlx::query("begin").execute(&mut *blocker).await.unwrap();
    sqlx::query("lock table alert_outbox in share mode")
        .execute(&mut *blocker)
        .await
        .unwrap();

    let raise = |pool: sqlx::PgPool, alerter: CountingAlerter, key: IncidentKey| {
        tokio::spawn(async move {
            IncidentManager {
                store: PgAlertStore::from_pool(pool),
                alerter,
            }
            .raise(key, "crit", "drift", now())
            .await
        })
    };
    let first = raise(pool.clone(), alerter.clone(), key.clone());
    let second = raise(pool.clone(), alerter.clone(), key.clone());

    let mut waited_ms = 0_u64;
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "select count(*) from pg_stat_activity
              where datname = current_database()
                and wait_event_type = 'Lock'
                and pid <> pg_backend_pid()",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        if waiting >= 2 {
            break;
        }
        assert!(
            waited_ms <= 10_000,
            "both raises were expected to park on the alert_outbox write so they overlap; \
             only {waiting} backend(s) ever waited. Without that overlap this assertion is a \
             coin flip, so it fails rather than reporting a green it did not earn."
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        waited_ms += 20;
    }
    sqlx::query("rollback")
        .execute(&mut *blocker)
        .await
        .unwrap();
    drop(blocker);

    let a = first.await.unwrap().expect("first raise");
    let b = second.await.unwrap().expect("second raise");
    assert_eq!(
        a.id, b.id,
        "both callers must end up holding the SAME incident; two ids means two open incidents"
    );

    let rows: i64 = sqlx::query_scalar(
        "select count(*) from alert_outbox
          where incident_key = $1 and status in ('open','acked')",
    )
    .bind(key.encoded())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rows, 1, "exactly one live incident for the key");
    assert_eq!(
        alerter.pages.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one condition is one page; the deduped caller must not page again"
    );
}
