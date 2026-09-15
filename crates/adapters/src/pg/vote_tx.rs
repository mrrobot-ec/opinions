use application::error::StoreError;
use application::model::{MarketId, NewVote, VoteId, VoteReceipt};
use application::ports::VoteWriter;
use async_trait::async_trait;
use domain::amm::Side;
use sqlx::Row;
use uuid::Uuid;

use super::rows::{db_error, side_from_idx, unique_violation};
use super::store::PgTx;

#[async_trait]
impl VoteWriter for PgTx {
    async fn allocate_vote_seq(&mut self, market: MarketId) -> Result<i64, StoreError> {
        sqlx::query_scalar(
            r#"
            update markets set vote_seq_counter = vote_seq_counter + 1
             where id = $1 returning vote_seq_counter
            "#,
        )
        .bind(market.0)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?
        .ok_or(StoreError::NotFound("market"))
    }

    async fn insert_vote(&mut self, vote: NewVote) -> Result<VoteId, StoreError> {
        let outcome: Option<Uuid> =
            sqlx::query_scalar("select id from outcomes where market_id = $1 and idx = $2")
                .bind(vote.market.0)
                .bind(side_idx(vote.side))
                .fetch_optional(&mut *self.tx)
                .await
                .map_err(db_error)?;
        let outcome = outcome.ok_or(StoreError::NotFound("outcome"))?;
        let id = VoteId(Uuid::new_v4());
        let result = sqlx::query(
            r#"
            insert into votes
                (id, user_id, market_id, outcome_id, crowd_guess_pct, seq, idempotency_key,
                 created_at, cast_ip, device_hash)
            values ($1, $2, $3, $4, $5, $6, $7, $8, $9::inet, $10)
            "#,
        )
        .bind(id.0)
        .bind(vote.user.0)
        .bind(vote.market.0)
        .bind(outcome)
        .bind(i32::from(vote.crowd_guess_pct))
        .bind(vote.seq)
        .bind(vote.idempotency_key)
        .bind(vote.created_at)
        .bind(vote.cast_ip.map(|ip| ip.to_string()))
        .bind(vote.device_hash)
        .execute(&mut *self.tx)
        .await;
        match result {
            Ok(_) => Ok(id),
            Err(error) if unique_violation(&error) => Err(StoreError::Conflict("vote")),
            Err(error) => Err(db_error(error)),
        }
    }

    async fn vote_by_key(
        &mut self,
        key: &str,
        now: time::OffsetDateTime,
    ) -> Result<Option<VoteReceipt>, StoreError> {
        let row = sqlx::query(
            r#"
            select v.id, v.user_id, v.market_id, o.idx, v.crowd_guess_pct, v.seq,
                   m.status, m.tally_hidden_at
              from votes v
              join outcomes o on o.id = v.outcome_id and o.market_id = v.market_id
              join markets m on m.id = v.market_id
             where v.idempotency_key = $1
            "#,
        )
        .bind(key)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        row.map(|row| {
            let guess: i32 = row.try_get("crowd_guess_pct").map_err(db_error)?;
            Ok(VoteReceipt {
                vote_id: VoteId(row.try_get("id").map_err(db_error)?),
                market: MarketId(row.try_get("market_id").map_err(db_error)?),
                user: application::model::UserId(row.try_get("user_id").map_err(db_error)?),
                side: side_from_idx(row.try_get("idx").map_err(db_error)?)?,
                crowd_guess_pct: u8::try_from(guess)
                    .map_err(|_| StoreError::Invariant("invalid crowd guess"))?,
                seq: {
                    let state: String = row.try_get("status").map_err(db_error)?;
                    let hidden_at: time::OffsetDateTime =
                        row.try_get("tally_hidden_at").map_err(db_error)?;
                    let terminal = matches!(state.as_str(), "resolved" | "paid" | "voided");
                    (terminal || now < hidden_at)
                        .then(|| row.try_get("seq").map_err(db_error))
                        .transpose()?
                },
                replayed: false,
            })
        })
        .transpose()
    }
}

const fn side_idx(side: Side) -> i32 {
    match side {
        Side::Yes => 0,
        Side::No => 1,
    }
}
