//! Locked-cursor notification materializer. Database rows are authoritative;
//! live frames are a post-commit, best-effort acceleration path.

use std::sync::Arc;

use application::error::StoreError;
use application::model::SocialConfig;
use application::notify::{materialize, NotifyPolicy};
use application::ports::{Clock, Store};
use tokio::sync::broadcast;

use crate::relay::{BusEvent, UserNotifFrame};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotifierReport {
    pub fetched: u32,
    pub inserted: u32,
    pub dropped_mentions: u32,
}

#[derive(Clone)]
pub struct OutboxNotifier<S> {
    store: S,
    clock: Arc<dyn Clock>,
    bus: broadcast::Sender<BusEvent>,
    social: SocialConfig,
    admin_handles: Vec<String>,
}

impl<S> OutboxNotifier<S>
where
    S: Store + Clone,
{
    #[must_use]
    pub fn new(
        store: S,
        clock: Arc<dyn Clock>,
        bus: broadcast::Sender<BusEvent>,
        social: SocialConfig,
        admin_handles: Vec<String>,
    ) -> Self {
        Self {
            store,
            clock,
            bus,
            social,
            admin_handles,
        }
    }

    pub async fn pump_once(&self) -> Result<NotifierReport, StoreError> {
        self.pump_once_inner(FailurePoint::None).await
    }

    async fn pump_once_inner(&self, failure: FailurePoint) -> Result<NotifierReport, StoreError> {
        let now = self.clock.now();
        let mut tx = self.store.notification_tx().await?;
        let cursor = tx.lock_notifier_cursor().await?;
        let events = tx.outbox_events_after(cursor, 128).await?;
        if events.is_empty() {
            tx.commit().await?;
            return Ok(NotifierReport {
                fetched: 0,
                inserted: 0,
                dropped_mentions: 0,
            });
        }
        if failure == FailurePoint::BeforeInsert {
            return Err(StoreError::Invariant(
                "injected notifier failure before insert",
            ));
        }
        let policy = NotifyPolicy {
            social: self.social,
            admin_handles: &self.admin_handles,
        };
        let mut pending = Vec::new();
        let mut dropped_mentions = 0_u32;
        for event in &events {
            let result = materialize(&mut *tx, event, now, &policy).await?;
            dropped_mentions = dropped_mentions
                .checked_add(result.dropped_mentions)
                .ok_or(StoreError::Invariant("dropped mention count overflow"))?;
            pending.extend(result.notifications);
        }
        let inserted = tx.insert_notifications(&pending).await?;
        if failure == FailurePoint::AfterInsert {
            return Err(StoreError::Invariant(
                "injected notifier failure after insert",
            ));
        }
        let max_seq = events
            .last()
            .map(|event| event.seq)
            .ok_or(StoreError::Invariant(
                "nonempty notifier batch lost max sequence",
            ))?;
        tx.advance_notifier_cursor(max_seq).await?;
        tx.commit().await?;
        if failure == FailurePoint::AfterCommit {
            return Err(StoreError::Invariant(
                "injected notifier crash after commit",
            ));
        }
        for row in &inserted {
            let source_seq = row.source_seq.ok_or(StoreError::Invariant(
                "materialized notification missing source",
            ))?;
            let _ = self.bus.send(BusEvent::UserNotif {
                user_id: row.user.0,
                frame: UserNotifFrame {
                    id: row.id,
                    source_seq,
                    notification_type: row.notification_type.clone(),
                    payload: row.payload.clone(),
                },
            });
        }
        Ok(NotifierReport {
            fetched: u32::try_from(events.len())
                .map_err(|_| StoreError::Invariant("notifier batch count overflow"))?,
            inserted: u32::try_from(inserted.len())
                .map_err(|_| StoreError::Invariant("notification insert count overflow"))?,
            dropped_mentions,
        })
    }

    pub async fn run(self, tick: std::time::Duration) {
        let mut interval = tokio::time::interval(tick);
        loop {
            interval.tick().await;
            if let Err(error) = self.pump_once().await {
                eprintln!("notification materializer error: {error}");
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailurePoint {
    None,
    BeforeInsert,
    AfterInsert,
    AfterCommit,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::str::FromStr;

    use application::fakes::FakeClock;
    use application::model::{CommentId, UserId};
    use application::ports::NotificationQueries;
    use serde_json::json;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use sqlx::PgPool;
    use time::OffsetDateTime;

    use super::*;
    use crate::pg::PgStore;

    static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

    async fn isolated_store() -> (PgStore, PgPool, PgPool, String) {
        let url = std::env::var("DATABASE_URL").unwrap();
        let control = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .unwrap();
        let database = format!("opinions_notifier_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("create database {database}")))
            .execute(&control)
            .await
            .unwrap();
        let options = PgConnectOptions::from_str(&url)
            .unwrap()
            .database(&database);
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await
            .unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        (PgStore::from_pool(pool.clone()), pool, control, database)
    }

    async fn drop_database(pool: PgPool, control: PgPool, database: &str) {
        pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "drop database {database} with (force)"
        )))
        .execute(&control)
        .await
        .unwrap();
        control.close().await;
    }

    async fn user(pool: &PgPool, handle: &str) -> UserId {
        UserId(
            sqlx::query_scalar("insert into users (handle) values ($1) returning id")
                .bind(handle)
                .fetch_one(pool)
                .await
                .unwrap(),
        )
    }

    async fn market(pool: &PgPool) -> uuid::Uuid {
        sqlx::query_scalar(
            r#"
            insert into markets
                (slug, question, status, min_votes_to_resolve, opens_at, closes_at, tally_hidden_at)
            values ($1, 'question', 'paid', 1, now(), now(), now()) returning id
            "#,
        )
        .bind(format!("notifier-{}", uuid::Uuid::new_v4().simple()))
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn append(
        pool: &PgPool,
        event_type: &str,
        aggregate: uuid::Uuid,
        payload: serde_json::Value,
    ) -> i64 {
        sqlx::query_scalar(
            "insert into events_outbox (aggregate_type, aggregate_id, event_type, payload) values ('test', $1, $2, $3) returning seq",
        )
        .bind(aggregate)
        .bind(event_type)
        .bind(payload)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    fn notifier(store: PgStore, bus: broadcast::Sender<BusEvent>) -> OutboxNotifier<PgStore> {
        OutboxNotifier::new(
            store,
            Arc::new(FakeClock::at(OffsetDateTime::UNIX_EPOCH)),
            bus,
            SocialConfig::default(),
            vec!["Admin".to_string()],
        )
    }

    #[tokio::test]
    async fn cursor_rollbacks_replay_and_post_commit_frames_are_honest() {
        let (store, pool, control, database) = isolated_store().await;
        let recipient = user(&pool, "recipient").await;
        let author = user(&pool, "author").await;
        let comment = uuid::Uuid::new_v4();
        let market = market(&pool).await;
        let seq = append(
            &pool,
            "CommentPosted",
            comment,
            json!({
                "market_id": market,
                "comment_id": comment,
                "author_id": author.0,
                "parent_author_id": recipient.0,
            }),
        )
        .await;
        let (bus, mut frames) = broadcast::channel(8);
        let notifier = notifier(store.clone(), bus);

        for failure in [FailurePoint::BeforeInsert, FailurePoint::AfterInsert] {
            assert!(notifier.pump_once_inner(failure).await.is_err());
            assert_eq!(store.unread_count(recipient).await.unwrap(), 0);
            let cursor: i64 =
                sqlx::query_scalar("select last_seq from outbox_cursors where consumer='notifier'")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(cursor, 0);
        }

        assert!(notifier
            .pump_once_inner(FailurePoint::AfterCommit)
            .await
            .is_err());
        assert_eq!(store.unread_count(recipient).await.unwrap(), 1);
        assert!(frames.try_recv().is_err());
        assert_eq!(notifier.pump_once().await.unwrap().fetched, 0);
        let rows = store.notifications(recipient, 10, None).await.unwrap();
        assert_eq!((rows.len(), rows[0].source_seq), (1, Some(seq)));
        assert!(store
            .notifications(recipient, 10, Some(rows[0].id))
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .mark_notifications_read(author, &[rows[0].id], OffsetDateTime::UNIX_EPOCH)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .mark_notifications_read(recipient, &[rows[0].id], OffsetDateTime::UNIX_EPOCH)
                .await
                .unwrap(),
            1
        );
        assert_eq!(store.unread_count(recipient).await.unwrap(), 0);

        drop(notifier);
        drop(frames);
        drop(store);
        drop_database(pool, control, &database).await;
    }

    #[tokio::test]
    async fn concurrent_pumps_serialize_unknown_advances_and_user_frame_is_typed() {
        let (store, pool, control, database) = isolated_store().await;
        let admin = user(&pool, "ADMIN").await;
        let market = market(&pool).await;
        let unknown = append(&pool, "FutureEvent", market, json!({})).await;
        let curator = append(&pool, "CuratorNeeded", market, json!({})).await;
        let (bus, mut frames) = broadcast::channel(8);
        let notifier = notifier(store.clone(), bus);
        let (left, right) = tokio::join!(notifier.pump_once(), notifier.pump_once());
        let totals = left.unwrap().fetched + right.unwrap().fetched;
        assert_eq!(totals, 2);
        let row = store
            .notifications(admin, 10, None)
            .await
            .unwrap()
            .remove(0);
        assert_eq!(
            frames.recv().await.unwrap(),
            BusEvent::UserNotif {
                user_id: admin.0,
                frame: UserNotifFrame {
                    id: row.id,
                    source_seq: curator,
                    notification_type: "curator_needed".to_string(),
                    payload: json!({"market_id":market.to_string()}),
                },
            }
        );
        assert!(curator > unknown);
        assert_eq!(store.unread_count(admin).await.unwrap(), 1);

        drop(notifier);
        drop(frames);
        drop(store);
        drop_database(pool, control, &database).await;
    }

    #[tokio::test]
    async fn empty_startup_and_supervisor_behavior_are_explicit() {
        let (store, pool, control, database) = isolated_store().await;
        let (bus, _) = broadcast::channel(2);
        let notifier = notifier(store.clone(), bus);
        assert_eq!(
            notifier.pump_once().await.unwrap(),
            NotifierReport {
                fetched: 0,
                inserted: 0,
                dropped_mentions: 0,
            }
        );
        pool.close().await;
        let task = tokio::spawn(notifier.run(std::time::Duration::from_millis(1)));
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(!task.is_finished());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        drop(store);
        drop_database(pool, control, &database).await;
    }

    #[tokio::test]
    async fn mention_fanout_reads_parent_and_velocity_without_locking_outbox_rows() {
        let (store, pool, control, database) = isolated_store().await;
        let author = user(&pool, "mention-author").await;
        let parent_author = user(&pool, "mention-parent").await;
        let recipient = user(&pool, "mention-recipient").await;
        let market = market(&pool).await;
        let parent = uuid::Uuid::new_v4();
        let comment = uuid::Uuid::new_v4();
        sqlx::query(
            "insert into comments (id,market_id,user_id,body) values ($1,$3,$4,'parent'),($2,$3,$5,'child')",
        )
        .bind(parent)
        .bind(comment)
        .bind(market)
        .bind(parent_author.0)
        .bind(author.0)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("update comments set parent_id = $1, depth = 1 where id = $2")
            .bind(parent)
            .bind(comment)
            .execute(&pool)
            .await
            .unwrap();
        let seq = append(
            &pool,
            "MentionCreated",
            comment,
            json!({
                "market_id": market,
                "comment_id": comment,
                "mentioned_user_id": recipient.0,
                "author_id": author.0,
            }),
        )
        .await;

        let mut row_lock = pool.begin().await.unwrap();
        sqlx::query("select seq from events_outbox where seq = $1 for update")
            .bind(seq)
            .fetch_one(&mut *row_lock)
            .await
            .unwrap();
        let (bus, _) = broadcast::channel(2);
        let report = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            notifier(store.clone(), bus).pump_once(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(report.inserted, 1);
        let rows = store.notifications(recipient, 10, None).await.unwrap();
        assert_eq!((rows.len(), rows[0].source_seq), (1, Some(seq)));
        row_lock.rollback().await.unwrap();

        let mut tx = store.notification_tx().await.unwrap();
        assert_eq!(
            tx.parent_author(CommentId(comment)).await.unwrap(),
            Some(parent_author)
        );
        assert_eq!(
            tx.notification_count_since(recipient, "mention", OffsetDateTime::UNIX_EPOCH)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            tx.users_by_handles(&["MENTION-RECIPIENT".to_string()])
                .await
                .unwrap(),
            vec![recipient]
        );
        assert!(tx.insert_notifications(&[]).await.unwrap().is_empty());
        tx.commit().await.unwrap();

        drop(store);
        drop_database(pool, control, &database).await;
    }

    #[tokio::test]
    async fn terminal_facts_aggregate_both_outcomes_and_enrich_holder_voters() {
        let (store, pool, control, database) = isolated_store().await;
        let holder = user(&pool, "holder").await;
        let voter = user(&pool, "voter").await;
        let market = market(&pool).await;
        let yes = uuid::Uuid::new_v4();
        let no = uuid::Uuid::new_v4();
        sqlx::query(
            "insert into outcomes (id, market_id, label, idx) values ($1,$3,'YES',0),($2,$3,'NO',1)",
        )
        .bind(yes)
        .bind(no)
        .bind(market)
        .execute(&pool)
        .await
        .unwrap();
        for (who, outcome, seq, score) in [(holder, yes, 1_i64, 8_000_i32), (voter, no, 2, 6_000)] {
            let vote: uuid::Uuid = sqlx::query_scalar(
                "insert into votes (user_id, market_id, outcome_id, crowd_guess_pct, seq, idempotency_key) values ($1,$2,$3,50,$4,$5) returning id",
            )
            .bind(who.0)
            .bind(market)
            .bind(outcome)
            .bind(seq)
            .bind(format!("vote-{market}-{seq}"))
            .fetch_one(&pool)
            .await
            .unwrap();
            sqlx::query("insert into vote_scores (vote_id, accuracy_bp, majority_bp, score_bp) values ($1,$2,10000,$2)")
                .bind(vote)
                .bind(score)
                .execute(&pool)
                .await
                .unwrap();
        }
        let mut tx = pool.begin().await.unwrap();
        let external: uuid::Uuid = sqlx::query_scalar(
            "insert into ledger_accounts (owner_type, currency) values ('external','usdc') returning id",
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        let fees: uuid::Uuid = sqlx::query_scalar(
            "insert into ledger_accounts (owner_type, currency) values ('fees','usdc') returning id",
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        let ledger: uuid::Uuid = sqlx::query_scalar(
            "insert into ledger_transactions (kind, idempotency_key) values ('payout',$1) returning id",
        )
        .bind(format!("notif-ledger-{market}"))
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        sqlx::query("insert into ledger_entries (txn_id, account_id, amount_micro) values ($1,$2,-1),($1,$3,1)")
            .bind(ledger)
            .bind(external)
            .bind(fees)
            .execute(&mut *tx)
            .await
            .unwrap();
        for (outcome, payout, delta) in [(yes, 70_i64, 20_i64), (no, 30, -10)] {
            sqlx::query("insert into realizations (user_id,market_id,outcome_id,source,realized_delta_micro,payout_micro,txn_id) values ($1,$2,$3,'settlement',$4,$5,$6)")
                .bind(holder.0)
                .bind(market)
                .bind(outcome)
                .bind(delta)
                .bind(payout)
                .bind(ledger)
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();
        append(
            &pool,
            "MarketResolved",
            market,
            json!({"redemption_yes_micro": 700_000, "redemption_no_micro": 300_000}),
        )
        .await;
        let (bus, _) = broadcast::channel(8);
        let report = notifier(store.clone(), bus).pump_once().await.unwrap();
        assert_eq!((report.fetched, report.inserted), (1, 2));
        let holder_row = store
            .notifications(holder, 10, None)
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(holder_row.notification_type, "resolution_trade");
        assert_eq!(holder_row.payload["payout_total_micro"], 100);
        assert_eq!(holder_row.payload["realized_delta_micro"], 10);
        assert_eq!(holder_row.payload["score_bp"], 8_000);
        let voter_row = store
            .notifications(voter, 10, None)
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(voter_row.notification_type, "resolution_vote");
        assert_eq!(voter_row.payload["side"], "no");

        drop(store);
        drop_database(pool, control, &database).await;
    }

    #[tokio::test]
    async fn thousand_participant_resolution_is_one_bulk_write_under_two_seconds() {
        let (store, pool, control, database) = isolated_store().await;
        let market = market(&pool).await;
        let outcome = uuid::Uuid::new_v4();
        sqlx::query("insert into outcomes (id, market_id, label, idx) values ($1,$2,'YES',0)")
            .bind(outcome)
            .bind(market)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "insert into users (handle) select $1 || n::text from generate_series(1,1000) n",
        )
        .bind(format!("perf-{market}-"))
        .execute(&pool)
        .await
        .unwrap();
        let mut tx = pool.begin().await.unwrap();
        let external: uuid::Uuid = sqlx::query_scalar(
            "insert into ledger_accounts (owner_type, currency) values ('external','usdc') returning id",
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        let fees: uuid::Uuid = sqlx::query_scalar(
            "insert into ledger_accounts (owner_type, currency) values ('fees','usdc') returning id",
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        let ledger: uuid::Uuid = sqlx::query_scalar(
            "insert into ledger_transactions (kind, idempotency_key) values ('payout',$1) returning id",
        )
        .bind(format!("perf-{market}"))
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        sqlx::query("insert into ledger_entries (txn_id, account_id, amount_micro) values ($1,$2,-1),($1,$3,1)")
            .bind(ledger)
            .bind(external)
            .bind(fees)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query(
            r#"
            insert into realizations
                (user_id, market_id, outcome_id, source, realized_delta_micro, payout_micro, txn_id)
            select id, $1, $2, 'settlement', 1, 1, $3
              from users where handle like $4
            "#,
        )
        .bind(market)
        .bind(outcome)
        .bind(ledger)
        .bind(format!("perf-{market}-%"))
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        append(
            &pool,
            "MarketResolved",
            market,
            json!({"redemption_yes_micro": 1_000_000, "redemption_no_micro": 0}),
        )
        .await;
        let (bus, _) = broadcast::channel(2);
        let started = std::time::Instant::now();
        let report = notifier(store.clone(), bus).pump_once().await.unwrap();
        let elapsed = started.elapsed();
        assert_eq!(report.inserted, 1_000);
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "1,000-participant materialization exceeded 2s: {elapsed:?}"
        );

        drop(store);
        drop_database(pool, control, &database).await;
    }
}
