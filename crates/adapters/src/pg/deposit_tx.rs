use application::error::StoreError;
use application::model::{DepositId, MarketId, NewDeposit, NewMarket, OwnerRef, UserId};
use application::ports::{DepositWriter, LedgerWriter, MarketWriter, UserWriter};
use async_trait::async_trait;
use domain::ledger::Currency;
use domain::market::MarketState;
use domain::money::{BasisPoints, MicroUsd};
use uuid::Uuid;

use super::resolve_tx;
use super::rows::{db_error, unique_violation};
use super::store::PgTx;

#[async_trait]
impl DepositWriter for PgTx {
    async fn deposit_by_sig(&mut self, chain_sig: &str) -> Result<Option<DepositId>, StoreError> {
        sqlx::query_scalar("select id from deposits where chain_sig = $1")
            .bind(chain_sig)
            .fetch_optional(&mut *self.tx)
            .await
            .map(|id| id.map(DepositId))
            .map_err(db_error)
    }

    async fn insert_deposit(&mut self, deposit: NewDeposit) -> Result<DepositId, StoreError> {
        let id = DepositId(Uuid::new_v4());
        let result = sqlx::query(
            r#"
            insert into deposits
                (id, user_id, chain_sig, amount_micro, status, txn_id,
                 machine_status, admit_tx_id)
            values ($1, $2, $3, $4, 'admitted_legacy', $5,
                    'admitted_legacy', $5)
            "#,
        )
        .bind(id.0)
        .bind(deposit.user.0)
        .bind(deposit.chain_sig)
        .bind(deposit.amount.0)
        .bind(deposit.ledger_txn)
        .execute(&mut *self.tx)
        .await;
        match result {
            Ok(_) => Ok(id),
            Err(error) if unique_violation(&error) => {
                Err(StoreError::Conflict("deposit chain signature"))
            }
            Err(error) => Err(db_error(error)),
        }
    }
}

#[async_trait]
impl MarketWriter for PgTx {
    async fn insert_market(&mut self, market: NewMarket) -> Result<MarketId, StoreError> {
        let result = sqlx::query(
            r#"
            insert into markets
                (id, slug, question, status, min_votes_to_resolve,
                 opens_at, closes_at, tally_hidden_at)
            values ($1, $2, $2, 'draft', $3, now(), $4, $5)
            "#,
        )
        .bind(market.id.0)
        .bind(market.slug)
        .bind(market.min_votes_to_resolve)
        .bind(market.closes_at)
        .bind(market.tally_hidden_at)
        .execute(&mut *self.tx)
        .await;
        if let Err(error) = result {
            return if unique_violation(&error) {
                Err(StoreError::Conflict("market"))
            } else {
                Err(db_error(error))
            };
        }
        sqlx::query(
            r#"
            insert into outcomes (id, market_id, label, idx)
            values ($1, $3, 'YES', 0), ($2, $3, 'NO', 1)
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(Uuid::new_v4())
        .bind(market.id.0)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(market.id)
    }

    async fn create_pool(
        &mut self,
        market: MarketId,
        fee: BasisPoints,
        seeded: MicroUsd,
    ) -> Result<(), StoreError> {
        let pool = Uuid::new_v4();
        sqlx::query(
            "insert into pools (id, market_id, fee_bps, seeded_micro) values ($1, $2, $3, $4)",
        )
        .bind(pool)
        .bind(market.0)
        .bind(i32::from(fee.0))
        .bind(seeded.0)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        LedgerWriter::account(self, OwnerRef::MarketPool(market), Currency::Usdc).await?;
        let result = sqlx::query(
            r#"
            insert into pool_reserves (pool_id, outcome_id, market_id, reserve_micro_shares)
            select $1, o.id, $2, $3 from outcomes o
             where o.market_id = $2 and o.idx in (0, 1)
            "#,
        )
        .bind(pool)
        .bind(market.0)
        .bind(seeded.0)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        if result.rows_affected() != 2 {
            return Err(StoreError::Invariant(
                "market does not have exactly two outcomes",
            ));
        }
        Ok(())
    }

    async fn set_market_state(
        &mut self,
        market: MarketId,
        state: MarketState,
    ) -> Result<(), StoreError> {
        resolve_tx::set_market_state(self, market, state).await
    }
}

#[async_trait]
impl UserWriter for PgTx {
    async fn insert_user(&mut self, handle: &str) -> Result<UserId, StoreError> {
        let id = UserId(Uuid::new_v4());
        let result = sqlx::query(
            r#"with inserted as (
                   insert into users (id, handle) values ($1, $2) returning id
               )
               insert into reputation (user_id, rep_micro, tier)
               select id, 0, 0 from inserted"#,
        )
        .bind(id.0)
        .bind(handle)
        .execute(&mut *self.tx)
        .await;
        match result {
            Ok(_) => Ok(id),
            Err(error) if unique_violation(&error) => Err(StoreError::Conflict("user handle")),
            Err(error) => Err(db_error(error)),
        }
    }

    async fn link_channel(
        &mut self,
        user: UserId,
        channel: &str,
        address: &str,
    ) -> Result<(), StoreError> {
        let result = sqlx::query(
            "insert into user_channels (user_id, channel, address) values ($1, $2, $3)",
        )
        .bind(user.0)
        .bind(channel)
        .bind(address)
        .execute(&mut *self.tx)
        .await;
        match result {
            Ok(_) => Ok(()),
            Err(error) if unique_violation(&error) => Err(StoreError::Conflict("channel link")),
            Err(error) => Err(db_error(error)),
        }
    }
}
