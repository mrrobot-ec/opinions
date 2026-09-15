use application::error::StoreError;
use application::model::{
    CommentCursor, CommentId, CommentPage, CommentRow, CommentSort, CommentView, HolderRow,
    MarketHolders, MarketId, ModerationStatus, NewComment, ProfileTradeRow, ProfileVoteRow,
    ReportedCommentRow, TradeAction, UserId, UserProfile,
};
use application::ports::{CommentWriter, SocialQueries};
use async_trait::async_trait;
use domain::amm::Side;
use domain::money::MicroUsd;
use sqlx::postgres::PgRow;
use sqlx::Row;

use super::rows::{db_error, unique_violation};
use super::store::{PgStore, PgTx};

fn status_name(status: ModerationStatus) -> &'static str {
    match status {
        ModerationStatus::Visible => "visible",
        ModerationStatus::Shadow => "shadow",
        ModerationStatus::Blocked => "blocked",
    }
}

fn parse_status(status: &str) -> Result<ModerationStatus, StoreError> {
    match status {
        "visible" => Ok(ModerationStatus::Visible),
        "shadow" => Ok(ModerationStatus::Shadow),
        "blocked" => Ok(ModerationStatus::Blocked),
        _ => Err(StoreError::Invariant("unknown comment moderation status")),
    }
}

fn comment_from_row(row: &PgRow) -> Result<CommentRow, StoreError> {
    let depth: i32 = row.try_get("depth").map_err(db_error)?;
    let reply_count: i32 = row.try_get("reply_count").map_err(db_error)?;
    let status: String = row.try_get("moderation_status").map_err(db_error)?;
    Ok(CommentRow {
        id: CommentId(row.try_get("id").map_err(db_error)?),
        market: MarketId(row.try_get("market_id").map_err(db_error)?),
        author: UserId(row.try_get("user_id").map_err(db_error)?),
        parent: row
            .try_get::<Option<uuid::Uuid>, _>("parent_id")
            .map_err(db_error)?
            .map(CommentId),
        body: row.try_get("body").map_err(db_error)?,
        body_hash: row.try_get("body_hash").map_err(db_error)?,
        score: row.try_get("score").map_err(db_error)?,
        moderation_status: parse_status(&status)?,
        depth: u8::try_from(depth).map_err(|_| StoreError::Invariant("invalid comment depth"))?,
        reply_count: u32::try_from(reply_count)
            .map_err(|_| StoreError::Invariant("invalid comment reply count"))?,
        created_at: row.try_get("created_at").map_err(db_error)?,
    })
}

const COMMENT_SELECT: &str = r#"
    select id, market_id, user_id, parent_id, body, body_hash, score,
           moderation_status, depth, reply_count, created_at
      from comments
"#;

#[async_trait]
impl CommentWriter for PgTx {
    async fn comment(&mut self, id: CommentId) -> Result<Option<CommentRow>, StoreError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{COMMENT_SELECT} where id = $1"
        )))
        .bind(id.0)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        row.as_ref().map(comment_from_row).transpose()
    }

    async fn comment_for_update(&mut self, id: CommentId) -> Result<CommentRow, StoreError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{COMMENT_SELECT} where id = $1 for update"
        )))
        .bind(id.0)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?
        .ok_or(StoreError::NotFound("comment"))?;
        comment_from_row(&row)
    }

    async fn recent_same_hash(
        &mut self,
        author: UserId,
        hash: &str,
        since: time::OffsetDateTime,
    ) -> Result<u32, StoreError> {
        let count: i64 = sqlx::query_scalar(
            "select count(*)::bigint from comments where user_id = $1 and body_hash = $2 and created_at >= $3",
        )
        .bind(author.0)
        .bind(hash)
        .bind(since)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        u32::try_from(count).map_err(|_| StoreError::Invariant("comment count overflow"))
    }

    async fn author_posts_since(
        &mut self,
        author: UserId,
        since: time::OffsetDateTime,
    ) -> Result<u32, StoreError> {
        let count: i64 = sqlx::query_scalar(
            "select count(*)::bigint from comments where user_id = $1 and created_at >= $2",
        )
        .bind(author.0)
        .bind(since)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        u32::try_from(count).map_err(|_| StoreError::Invariant("comment count overflow"))
    }

    async fn insert_comment(&mut self, comment: NewComment) -> Result<(), StoreError> {
        let result = sqlx::query(
            r#"
            insert into comments
                (id, market_id, user_id, parent_id, body, body_hash,
                 moderation_status, depth, created_at)
            values ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            "#,
        )
        .bind(comment.id.0)
        .bind(comment.market.0)
        .bind(comment.author.0)
        .bind(comment.parent.map(|id| id.0))
        .bind(comment.body)
        .bind(comment.body_hash)
        .bind(status_name(comment.moderation_status))
        .bind(i32::from(comment.depth))
        .bind(comment.created_at)
        .execute(&mut *self.tx)
        .await;
        match result {
            Ok(_) => Ok(()),
            Err(error) if unique_violation(&error) => Err(StoreError::Conflict("comment")),
            Err(error) => Err(db_error(error)),
        }
    }

    async fn bump_reply_count(&mut self, parent: CommentId) -> Result<(), StoreError> {
        let affected =
            sqlx::query("update comments set reply_count = reply_count + 1 where id = $1")
                .bind(parent.0)
                .execute(&mut *self.tx)
                .await
                .map_err(db_error)?
                .rows_affected();
        if affected == 1 {
            Ok(())
        } else {
            Err(StoreError::NotFound("comment"))
        }
    }

    async fn insert_comment_vote(
        &mut self,
        comment: CommentId,
        user: UserId,
        value: i16,
    ) -> Result<bool, StoreError> {
        let inserted: Option<i32> = sqlx::query_scalar(
            r#"
            insert into comment_votes (comment_id, user_id, value)
            values ($1, $2, $3) on conflict do nothing returning 1
            "#,
        )
        .bind(comment.0)
        .bind(user.0)
        .bind(value)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(inserted.is_some())
    }

    async fn adjust_comment_score(
        &mut self,
        comment: CommentId,
        delta: i16,
    ) -> Result<i32, StoreError> {
        sqlx::query_scalar("update comments set score = score + $2 where id = $1 returning score")
            .bind(comment.0)
            .bind(i32::from(delta))
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)?
            .ok_or(StoreError::NotFound("comment"))
    }

    async fn insert_comment_report(
        &mut self,
        comment: CommentId,
        reporter: UserId,
        created_at: time::OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let inserted: Option<i32> = sqlx::query_scalar(
            r#"
            insert into comment_reports (comment_id, reporter_id, created_at)
            values ($1, $2, $3) on conflict do nothing returning 1
            "#,
        )
        .bind(comment.0)
        .bind(reporter.0)
        .bind(created_at)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(inserted.is_some())
    }

    async fn comment_report_count(&mut self, comment: CommentId) -> Result<u32, StoreError> {
        let count: i64 = sqlx::query_scalar(
            "select count(*)::bigint from comment_reports where comment_id = $1",
        )
        .bind(comment.0)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        u32::try_from(count).map_err(|_| StoreError::Invariant("comment report count overflow"))
    }

    async fn reporter_reports_since(
        &mut self,
        reporter: UserId,
        since: time::OffsetDateTime,
    ) -> Result<u32, StoreError> {
        let count: i64 = sqlx::query_scalar(
            "select count(*)::bigint from comment_reports where reporter_id = $1 and created_at >= $2",
        )
        .bind(reporter.0)
        .bind(since)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        u32::try_from(count).map_err(|_| StoreError::Invariant("comment report count overflow"))
    }

    async fn set_comment_status(
        &mut self,
        comment: CommentId,
        status: ModerationStatus,
    ) -> Result<(), StoreError> {
        let affected = sqlx::query("update comments set moderation_status = $2 where id = $1")
            .bind(comment.0)
            .bind(status_name(status))
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?
            .rows_affected();
        if affected == 1 {
            Ok(())
        } else {
            Err(StoreError::NotFound("comment"))
        }
    }

    async fn delete_comment_reports(&mut self, comment: CommentId) -> Result<u32, StoreError> {
        let affected = sqlx::query("delete from comment_reports where comment_id = $1")
            .bind(comment.0)
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?
            .rows_affected();
        u32::try_from(affected).map_err(|_| StoreError::Invariant("comment report count overflow"))
    }
}

fn views(rows: Vec<PgRow>) -> Result<Vec<CommentView>, StoreError> {
    rows.iter()
        .map(|row| {
            Ok(CommentView {
                row: comment_from_row(row)?,
                author_handle: row.try_get("author_handle").map_err(db_error)?,
                hot_score: row.try_get("hot_score").map_err(db_error)?,
            })
        })
        .collect()
}

#[async_trait]
impl SocialQueries for PgStore {
    async fn comments(
        &self,
        market: MarketId,
        sort: CommentSort,
        viewer: Option<UserId>,
        limit: u32,
        cursor: Option<CommentCursor>,
        now: time::OffsetDateTime,
    ) -> Result<CommentPage, StoreError> {
        let fetch_limit = i64::from(limit) + 1;
        let (mut rows, cursor_as_of) = match (sort, cursor) {
            (CommentSort::Hot, None) => {
                let rows = hot_rows(self, market, viewer, fetch_limit, now, None).await?;
                (rows, now)
            }
            (
                CommentSort::Hot,
                Some(CommentCursor::Hot {
                    as_of,
                    hot_score,
                    created_at,
                    id,
                }),
            ) => {
                let rows = hot_rows(
                    self,
                    market,
                    viewer,
                    fetch_limit,
                    as_of,
                    Some((hot_score, created_at, id)),
                )
                .await?;
                (rows, as_of)
            }
            (CommentSort::Recent, None) => (
                recent_rows(self, market, viewer, fetch_limit, None).await?,
                now,
            ),
            (CommentSort::Recent, Some(CommentCursor::Recent { created_at, id })) => (
                recent_rows(self, market, viewer, fetch_limit, Some((created_at, id))).await?,
                now,
            ),
            _ => return Err(StoreError::Invariant("cursor sort mismatch")),
        };
        let has_more = rows.len() > usize::try_from(limit).unwrap_or(usize::MAX);
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let next = if has_more {
            rows.last().map(|row| match sort {
                CommentSort::Hot => CommentCursor::Hot {
                    as_of: cursor_as_of,
                    hot_score: row.hot_score,
                    created_at: row.row.created_at,
                    id: row.row.id,
                },
                CommentSort::Recent => CommentCursor::Recent {
                    created_at: row.row.created_at,
                    id: row.row.id,
                },
            })
        } else {
            None
        };
        Ok(CommentPage {
            comments: rows,
            next,
        })
    }

    async fn comment_view(
        &self,
        comment: CommentId,
        viewer: Option<UserId>,
    ) -> Result<CommentView, StoreError> {
        let row = sqlx::query(
            r#"
            select c.*, u.handle as author_handle, 0::bigint as hot_score
              from comments c join users u on u.id = c.user_id
             where c.id = $1
               and (c.moderation_status = 'visible' or c.user_id = $2)
            "#,
        )
        .bind(comment.0)
        .bind(viewer.map(|user| user.0))
        .fetch_optional(self.pool_handle())
        .await
        .map_err(db_error)?
        .ok_or(StoreError::NotFound("comment"))?;
        views(vec![row])?
            .pop()
            .ok_or(StoreError::Invariant("comment view disappeared"))
    }

    async fn holders(&self, market: MarketId, limit: u32) -> Result<MarketHolders, StoreError> {
        let rows = sqlx::query(
            r#"
            with ranked as (
              select o.idx, p.user_id, u.handle, r.tier, p.cost_micro,
                     row_number() over (
                       partition by o.idx order by p.cost_micro desc, p.user_id
                     ) as rank
                from outcomes o
                join positions p on p.outcome_id = o.id
                join users u on u.id = p.user_id
                join reputation r on r.user_id = p.user_id
               where o.market_id = $1 and p.cost_micro <> 0
            )
            select idx, user_id, handle, tier, cost_micro
              from ranked where rank <= $2 order by idx, cost_micro desc, user_id
            "#,
        )
        .bind(market.0)
        .bind(i64::from(limit))
        .fetch_all(self.pool_handle())
        .await
        .map_err(db_error)?;
        let mut holders = MarketHolders {
            yes: Vec::new(),
            no: Vec::new(),
        };
        for row in rows {
            let tier: i32 = row.try_get("tier").map_err(db_error)?;
            let holder = HolderRow {
                user: UserId(row.try_get("user_id").map_err(db_error)?),
                handle: row.try_get("handle").map_err(db_error)?,
                tier: u8::try_from(tier)
                    .map_err(|_| StoreError::Invariant("invalid reputation tier"))?,
                cost: MicroUsd(row.try_get("cost_micro").map_err(db_error)?),
            };
            match row.try_get::<i32, _>("idx").map_err(db_error)? {
                0 => holders.yes.push(holder),
                1 => holders.no.push(holder),
                _ => return Err(StoreError::Invariant("invalid outcome side")),
            }
        }
        Ok(holders)
    }

    async fn user_profile(&self, user: UserId) -> Result<UserProfile, StoreError> {
        let base = profile_base(self, user).await?;
        let recent_trades = profile_trades(self, user).await?;
        let recent_votes = profile_votes(self, user).await?;
        let tier: i32 = base.try_get("tier").map_err(db_error)?;
        let markets_scored: i64 = base.try_get("markets_scored").map_err(db_error)?;
        Ok(UserProfile {
            user,
            handle: base.try_get("handle").map_err(db_error)?,
            created_at: base.try_get("created_at").map_err(db_error)?,
            rep_micro: base.try_get("rep_micro").map_err(db_error)?,
            tier: u8::try_from(tier)
                .map_err(|_| StoreError::Invariant("invalid reputation tier"))?,
            avg_score_bp: base.try_get("avg_score_bp").map_err(db_error)?,
            markets_scored: u32::try_from(markets_scored)
                .map_err(|_| StoreError::Invariant("profile score count overflow"))?,
            realized_pnl: MicroUsd(base.try_get("realized_pnl_micro").map_err(db_error)?),
            recent_trades,
            recent_votes,
        })
    }

    async fn reported_comments(
        &self,
        threshold: u32,
        limit: u32,
    ) -> Result<Vec<ReportedCommentRow>, StoreError> {
        let rows = sqlx::query(
            r#"
            select c.*, author.handle as author_handle, 0::bigint as hot_score,
                   count(cr.reporter_id)::bigint report_count,
                   coalesce(array_agg(reporter.handle order by reporter.handle)
                     filter (where reporter.handle is not null), '{}') reporters
              from comments c join users author on author.id = c.user_id
              left join comment_reports cr on cr.comment_id = c.id
              left join users reporter on reporter.id = cr.reporter_id
             group by c.id, author.handle
            having count(cr.reporter_id) >= $1 or c.moderation_status = 'shadow'
             order by c.created_at desc, c.id desc limit $2
            "#,
        )
        .bind(i64::from(threshold))
        .bind(i64::from(limit))
        .fetch_all(self.pool_handle())
        .await
        .map_err(db_error)?;
        rows.iter()
            .map(|row| {
                let count: i64 = row.try_get("report_count").map_err(db_error)?;
                Ok(ReportedCommentRow {
                    comment: CommentView {
                        row: comment_from_row(row)?,
                        author_handle: row.try_get("author_handle").map_err(db_error)?,
                        hot_score: 0,
                    },
                    report_count: u32::try_from(count)
                        .map_err(|_| StoreError::Invariant("comment report count overflow"))?,
                    reporters: row.try_get("reporters").map_err(db_error)?,
                })
            })
            .collect()
    }
}

async fn profile_base(store: &PgStore, user: UserId) -> Result<PgRow, StoreError> {
    sqlx::query(
        r#"
        select u.handle, u.created_at, r.rep_micro, r.tier,
               scores.avg_score_bp, coalesce(scores.markets_scored, 0)::bigint markets_scored,
               coalesce(realized.realized_pnl_micro, 0)::bigint realized_pnl_micro
          from users u join reputation r on r.user_id = u.id
          left join (
            select v.user_id, round(avg(vs.score_bp))::bigint avg_score_bp,
                   count(*)::bigint markets_scored
              from votes v join vote_scores vs on vs.vote_id = v.id group by v.user_id
          ) scores on scores.user_id = u.id
          left join (
            select user_id, sum(realized_delta_micro)::bigint realized_pnl_micro
              from realizations group by user_id
          ) realized on realized.user_id = u.id
         where u.id = $1
        "#,
    )
    .bind(user.0)
    .fetch_optional(store.pool_handle())
    .await
    .map_err(db_error)?
    .ok_or(StoreError::NotFound("user"))
}

async fn profile_trades(store: &PgStore, user: UserId) -> Result<Vec<ProfileTradeRow>, StoreError> {
    let rows = sqlx::query(
        r#"
        select t.market_id, m.slug, o.idx, t.side as action,
               t.collateral_micro as gross_micro, t.created_at, t.seq
          from trades t join markets m on m.id = t.market_id
          join outcomes o on o.id = t.outcome_id and o.market_id = t.market_id
         where t.user_id = $1 order by t.created_at desc, t.id desc limit 20
        "#,
    )
    .bind(user.0)
    .fetch_all(store.pool_handle())
    .await
    .map_err(db_error)?;
    rows.iter()
        .map(|row| {
            Ok(ProfileTradeRow {
                market: MarketId(row.try_get("market_id").map_err(db_error)?),
                market_ref: row.try_get("slug").map_err(db_error)?,
                side: parse_side(row.try_get("idx").map_err(db_error)?)?,
                action: parse_action(&row.try_get::<String, _>("action").map_err(db_error)?)?,
                collateral_micro: row.try_get("gross_micro").map_err(db_error)?,
                created_at: row.try_get("created_at").map_err(db_error)?,
                trade_seq: row.try_get("seq").map_err(db_error)?,
            })
        })
        .collect()
}

async fn profile_votes(store: &PgStore, user: UserId) -> Result<Vec<ProfileVoteRow>, StoreError> {
    let rows = sqlx::query(
        r#"
        select v.market_id, m.question, v.created_at,
               case when m.status in ('resolved','paid','voided') then o.idx end as visible_idx,
               case when m.status in ('resolved','paid') then vs.score_bp end as visible_score
          from votes v join markets m on m.id = v.market_id
          join outcomes o on o.id = v.outcome_id and o.market_id = v.market_id
          left join vote_scores vs on vs.vote_id = v.id
         where v.user_id = $1 order by v.created_at desc, v.id desc limit 20
        "#,
    )
    .bind(user.0)
    .fetch_all(store.pool_handle())
    .await
    .map_err(db_error)?;
    rows.iter()
        .map(|row| {
            let idx: Option<i32> = row.try_get("visible_idx").map_err(db_error)?;
            let visible_score: Option<i32> = row.try_get("visible_score").map_err(db_error)?;
            Ok(ProfileVoteRow {
                market: MarketId(row.try_get("market_id").map_err(db_error)?),
                market_question: row.try_get("question").map_err(db_error)?,
                cast_at: row.try_get("created_at").map_err(db_error)?,
                side: idx.map(parse_side).transpose()?,
                score_bp: visible_score.map(parse_score).transpose()?,
            })
        })
        .collect()
}

fn parse_side(idx: i32) -> Result<Side, StoreError> {
    match idx {
        0 => Ok(Side::Yes),
        1 => Ok(Side::No),
        _ => Err(StoreError::Invariant("invalid outcome side")),
    }
}

fn parse_action(action: &str) -> Result<TradeAction, StoreError> {
    match action {
        "buy" => Ok(TradeAction::Buy),
        "sell" => Ok(TradeAction::Sell),
        _ => Err(StoreError::Invariant("invalid trade action")),
    }
}

fn parse_score(score: i32) -> Result<u16, StoreError> {
    u16::try_from(score).map_err(|_| StoreError::Invariant("invalid vote score"))
}

async fn hot_rows(
    store: &PgStore,
    market: MarketId,
    viewer: Option<UserId>,
    limit: i64,
    as_of: time::OffsetDateTime,
    seek: Option<(i64, time::OffsetDateTime, CommentId)>,
) -> Result<Vec<CommentView>, StoreError> {
    let (seek_score, seek_created, seek_id) = seek
        .map_or((None, None, None), |(score, created, id)| {
            (Some(score), Some(created), Some(id.0))
        });
    let rows = sqlx::query(
        r#"
        with candidates as (
          select c.*, u.handle as author_handle
            from comments c join users u on u.id = c.user_id
           where c.market_id = $1 and c.created_at <= $2
             and (c.moderation_status = 'visible'
                  or (c.moderation_status = 'shadow' and c.user_id = $3))
           order by c.created_at desc, c.id desc limit 500
        ), scored as (
          select candidates.*,
                 trunc(((greatest(score, 0)::numeric + 1) * 1000000)
                   / power((trunc(extract(epoch from ($2 - created_at)) / 3600)::bigint + 2)::numeric, 2))::bigint
                   as hot_score
            from candidates
        )
        select * from scored
         where $4::bigint is null or (hot_score, created_at, id) < ($4, $5, $6)
         order by hot_score desc, created_at desc, id desc limit $7
        "#,
    )
    .bind(market.0)
    .bind(as_of)
    .bind(viewer.map(|user| user.0))
    .bind(seek_score)
    .bind(seek_created)
    .bind(seek_id)
    .bind(limit)
    .fetch_all(store.pool_handle())
    .await
    .map_err(db_error)?;
    views(rows)
}

async fn recent_rows(
    store: &PgStore,
    market: MarketId,
    viewer: Option<UserId>,
    limit: i64,
    seek: Option<(time::OffsetDateTime, CommentId)>,
) -> Result<Vec<CommentView>, StoreError> {
    let (seek_created, seek_id) =
        seek.map_or((None, None), |(created, id)| (Some(created), Some(id.0)));
    let rows = sqlx::query(
        r#"
        select c.*, u.handle as author_handle, 0::bigint as hot_score
          from comments c join users u on u.id = c.user_id
         where c.market_id = $1
           and (c.moderation_status = 'visible'
                or (c.moderation_status = 'shadow' and c.user_id = $2))
           and ($3::timestamptz is null or (c.created_at, c.id) < ($3, $4))
         order by c.created_at desc, c.id desc limit $5
        "#,
    )
    .bind(market.0)
    .bind(viewer.map(|user| user.0))
    .bind(seek_created)
    .bind(seek_id)
    .bind(limit)
    .fetch_all(store.pool_handle())
    .await
    .map_err(db_error)?;
    views(rows)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn comment_status_database_vocabulary_is_total() {
        for (status, name) in [
            (ModerationStatus::Visible, "visible"),
            (ModerationStatus::Shadow, "shadow"),
            (ModerationStatus::Blocked, "blocked"),
        ] {
            assert_eq!(status_name(status), name);
            assert_eq!(parse_status(name).unwrap(), status);
        }
        assert_eq!(
            parse_status("surprise"),
            Err(StoreError::Invariant("unknown comment moderation status"))
        );
    }

    #[test]
    fn profile_database_vocabulary_rejects_corruption() {
        assert_eq!(parse_side(0), Ok(Side::Yes));
        assert_eq!(parse_side(1), Ok(Side::No));
        assert_eq!(
            parse_side(2),
            Err(StoreError::Invariant("invalid outcome side"))
        );
        assert_eq!(parse_action("buy"), Ok(TradeAction::Buy));
        assert_eq!(parse_action("sell"), Ok(TradeAction::Sell));
        assert_eq!(
            parse_action("hold"),
            Err(StoreError::Invariant("invalid trade action"))
        );
        assert_eq!(parse_score(10_000), Ok(10_000));
        assert_eq!(
            parse_score(-1),
            Err(StoreError::Invariant("invalid vote score"))
        );
    }
}
