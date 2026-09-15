use application::error::StoreError;
use application::model::{
    CommentId, MarketId, NewNotification, NotificationRow, OutboxEvent, ResolutionRecipient, UserId,
};
use application::ports::{NotificationQueries, NotificationWriter, NotifyReader};
use async_trait::async_trait;
use domain::amm::Side;
use domain::money::MicroUsd;
use sqlx::postgres::PgRow;
use sqlx::{Postgres, QueryBuilder, Row};

use super::rows::db_error;
use super::store::{PgStore, PgTx};

fn notification_from_row(row: &PgRow) -> Result<NotificationRow, StoreError> {
    Ok(NotificationRow {
        id: row.try_get("id").map_err(db_error)?,
        user: UserId(row.try_get("user_id").map_err(db_error)?),
        notification_type: row.try_get("type").map_err(db_error)?,
        market: row
            .try_get::<Option<uuid::Uuid>, _>("market_id")
            .map_err(db_error)?
            .map(MarketId),
        payload: row.try_get("payload").map_err(db_error)?,
        source_seq: row.try_get("source_seq").map_err(db_error)?,
        read_at: row.try_get("read_at").map_err(db_error)?,
        created_at: row.try_get("created_at").map_err(db_error)?,
    })
}

const NOTIFICATION_COLUMNS: &str =
    "id, user_id, type, market_id, payload, source_seq, read_at, created_at";

#[async_trait]
impl NotifyReader for PgTx {
    async fn lock_notifier_cursor(&mut self) -> Result<i64, StoreError> {
        sqlx::query_scalar(
            "select last_seq from outbox_cursors where consumer = 'notifier' for update",
        )
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?
        .ok_or(StoreError::Invariant("notifier cursor missing"))
    }

    async fn outbox_events_after(
        &mut self,
        last_seq: i64,
        limit: u32,
    ) -> Result<Vec<OutboxEvent>, StoreError> {
        let rows = sqlx::query(
            r#"
            select seq, event_type, aggregate_type, aggregate_id, payload
              from events_outbox
             where seq > $1
             order by seq
             limit $2
            "#,
        )
        .bind(last_seq)
        .bind(i64::from(limit))
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        rows.iter()
            .map(|row| {
                Ok(OutboxEvent {
                    seq: row.try_get("seq").map_err(db_error)?,
                    event_type: row.try_get("event_type").map_err(db_error)?,
                    aggregate_type: row.try_get("aggregate_type").map_err(db_error)?,
                    aggregate_id: row.try_get("aggregate_id").map_err(db_error)?,
                    payload: row.try_get("payload").map_err(db_error)?,
                })
            })
            .collect()
    }

    async fn resolution_recipients(
        &mut self,
        market: MarketId,
        voided: bool,
    ) -> Result<Vec<ResolutionRecipient>, StoreError> {
        let source = if voided { "void" } else { "settlement" };
        let rows = sqlx::query(
            r#"
            with terminal as (
              select user_id,
                     sum(payout_micro)::bigint as payout_total_micro,
                     sum(realized_delta_micro)::bigint as realized_delta_micro
                from realizations
               where market_id = $1 and source = $2
               group by user_id
            ), voters as (
              select v.user_id, vs.score_bp, o.idx
                from votes v
                join outcomes o on o.id = v.outcome_id and o.market_id = v.market_id
                left join vote_scores vs on vs.vote_id = v.id
               where v.market_id = $1
            ), recipients as (
              select user_id from terminal union select user_id from voters
            )
            select r.user_id, (t.user_id is not null) as held,
                   coalesce(t.payout_total_micro, 0)::bigint as payout_total_micro,
                   coalesce(t.realized_delta_micro, 0)::bigint as realized_delta_micro,
                   (v.user_id is not null) as voted, v.score_bp, v.idx
              from recipients r
              left join terminal t on t.user_id = r.user_id
              left join voters v on v.user_id = r.user_id
             order by r.user_id
            "#,
        )
        .bind(market.0)
        .bind(source)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        rows.iter()
            .map(|row| {
                let score: Option<i32> = row.try_get("score_bp").map_err(db_error)?;
                let idx: Option<i32> = row.try_get("idx").map_err(db_error)?;
                Ok(ResolutionRecipient {
                    user: UserId(row.try_get("user_id").map_err(db_error)?),
                    held: row.try_get("held").map_err(db_error)?,
                    payout_total: MicroUsd(row.try_get("payout_total_micro").map_err(db_error)?),
                    realized_delta: MicroUsd(
                        row.try_get("realized_delta_micro").map_err(db_error)?,
                    ),
                    voted: row.try_get("voted").map_err(db_error)?,
                    score_bp: score
                        .map(|value| {
                            u16::try_from(value)
                                .map_err(|_| StoreError::Invariant("invalid vote score"))
                        })
                        .transpose()?,
                    side: idx.map(parse_side).transpose()?,
                })
            })
            .collect()
    }

    async fn parent_author(&mut self, comment: CommentId) -> Result<Option<UserId>, StoreError> {
        let id = sqlx::query_scalar(
            r#"
            select parent.user_id
              from comments child
              join comments parent on parent.id = child.parent_id
             where child.id = $1
            "#,
        )
        .bind(comment.0)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(id.map(UserId))
    }

    async fn users_by_handles(&mut self, handles: &[String]) -> Result<Vec<UserId>, StoreError> {
        let normalized = handles
            .iter()
            .map(|handle| handle.to_lowercase())
            .collect::<Vec<_>>();
        let ids =
            sqlx::query_scalar("select id from users where lower(handle) = any($1) order by id")
                .bind(normalized)
                .fetch_all(&mut *self.tx)
                .await
                .map_err(db_error)?;
        Ok(ids.into_iter().map(UserId).collect())
    }

    async fn notification_count_since(
        &mut self,
        user: UserId,
        notification_type: &str,
        since: time::OffsetDateTime,
    ) -> Result<u32, StoreError> {
        let count: i64 = sqlx::query_scalar(
            "select count(*)::bigint from notifications where user_id = $1 and type = $2 and created_at >= $3",
        )
        .bind(user.0)
        .bind(notification_type)
        .bind(since)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        u32::try_from(count).map_err(|_| StoreError::Invariant("notification count overflow"))
    }
}

fn parse_side(value: i32) -> Result<Side, StoreError> {
    match value {
        0 => Ok(Side::Yes),
        1 => Ok(Side::No),
        _ => Err(StoreError::Invariant("invalid outcome side")),
    }
}

#[async_trait]
impl NotificationWriter for PgTx {
    async fn insert_notifications(
        &mut self,
        notifications: &[NewNotification],
    ) -> Result<Vec<NotificationRow>, StoreError> {
        if notifications.is_empty() {
            return Ok(Vec::new());
        }
        let mut builder = QueryBuilder::<Postgres>::new(
            "insert into notifications (user_id, type, market_id, payload, source_seq, created_at) ",
        );
        builder.push_values(notifications, |mut row, notification| {
            row.push_bind(notification.user.0)
                .push_bind(&notification.notification_type)
                .push_bind(notification.market.map(|market| market.0))
                .push_bind(&notification.payload)
                .push_bind(notification.source_seq)
                .push_bind(notification.created_at);
        });
        builder.push(
            " on conflict (user_id, source_seq) where source_seq is not null do nothing returning ",
        );
        builder.push(NOTIFICATION_COLUMNS);
        let rows = builder
            .build()
            .fetch_all(&mut *self.tx)
            .await
            .map_err(db_error)?;
        rows.iter().map(notification_from_row).collect()
    }

    async fn advance_notifier_cursor(&mut self, seq: i64) -> Result<(), StoreError> {
        let affected =
            sqlx::query("update outbox_cursors set last_seq = $1 where consumer = 'notifier'")
                .bind(seq)
                .execute(&mut *self.tx)
                .await
                .map_err(db_error)?
                .rows_affected();
        if affected == 1 {
            Ok(())
        } else {
            Err(StoreError::Invariant("notifier cursor missing"))
        }
    }
}

#[async_trait]
impl NotificationQueries for PgStore {
    async fn notifications(
        &self,
        user: UserId,
        limit: u32,
        before_id: Option<i64>,
    ) -> Result<Vec<NotificationRow>, StoreError> {
        let sql = format!(
            "select {NOTIFICATION_COLUMNS} from notifications where user_id = $1 and ($2::bigint is null or id < $2) order by id desc limit $3"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(user.0)
            .bind(before_id)
            .bind(i64::from(limit))
            .fetch_all(self.pool_handle())
            .await
            .map_err(db_error)?;
        rows.iter().map(notification_from_row).collect()
    }

    async fn unread_count(&self, user: UserId) -> Result<u32, StoreError> {
        let count: i64 = sqlx::query_scalar(
            "select count(*)::bigint from notifications where user_id = $1 and read_at is null",
        )
        .bind(user.0)
        .fetch_one(self.pool_handle())
        .await
        .map_err(db_error)?;
        u32::try_from(count).map_err(|_| StoreError::Invariant("notification count overflow"))
    }

    async fn mark_notifications_read(
        &self,
        user: UserId,
        ids: &[i64],
        now: time::OffsetDateTime,
    ) -> Result<u32, StoreError> {
        let affected = sqlx::query(
            "update notifications set read_at = $3 where user_id = $1 and id = any($2) and read_at is null",
        )
        .bind(user.0)
        .bind(ids)
        .bind(now)
        .execute(self.pool_handle())
        .await
        .map_err(db_error)?
        .rows_affected();
        u32::try_from(affected).map_err(|_| StoreError::Invariant("notification count overflow"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_recipient_side_vocabulary_is_total() {
        assert_eq!(parse_side(0), Ok(Side::Yes));
        assert_eq!(parse_side(1), Ok(Side::No));
        assert_eq!(
            parse_side(2),
            Err(StoreError::Invariant("invalid outcome side"))
        );
    }
}
