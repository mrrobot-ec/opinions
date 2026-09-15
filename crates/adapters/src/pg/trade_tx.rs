use std::collections::HashMap;

use application::error::StoreError;
use application::model::{
    Event, InsertedTrade, MarketId, MarketRow, NewTrade, OutcomeId, OwnerRef, PoolRow, PositionRow,
    ReputationRow, Tally, TradeId, TradeReceipt, UserId,
};
use application::ports::{
    Committable, IdempotencyGuard, LedgerWriter, MarketReader, OutboxWriter, PoolWriter,
    PositionWriter, TradeEconomyReader, TradeWriter, UserLockGuard, UserReader, VoteReader,
};
use async_trait::async_trait;
use domain::amm::Pool;
use domain::ledger::{AccountId, Currency, Entry, LedgerError, OwnerType, Transaction, TxnKind};
use domain::money::{MicroShares, MicroUsd};
use sqlx::Row;
use uuid::Uuid;

use super::rows::{
    action, action_name, currency, currency_name, db_error, market_from_row, owner_type,
    pool_from_row, side_from_idx, txn_kind, unique_violation,
};
use super::store::PgTx;

#[async_trait]
impl IdempotencyGuard for PgTx {
    async fn serialize_key(&mut self, key: &str) -> Result<(), StoreError> {
        sqlx::query("select pg_advisory_xact_lock(1, hashtext($1))")
            .bind(key)
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
        Ok(())
    }
}

#[async_trait]
impl UserLockGuard for PgTx {
    async fn lock_user(&mut self, user: UserId) -> Result<(), StoreError> {
        sqlx::query("select pg_advisory_xact_lock(2, hashtext($1))")
            .bind(user.0.to_string())
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
        Ok(())
    }
}

#[async_trait]
impl MarketReader for PgTx {
    async fn market_for_update(&mut self, market: MarketId) -> Result<MarketRow, StoreError> {
        let sql = format!(
            "{} where m.id = $1 for update of m",
            super::rows::MARKET_SELECT
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(market.0)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)?
            .ok_or(StoreError::NotFound("market"))?;
        market_from_row(&row)
    }
}

#[async_trait]
impl VoteReader for PgTx {
    async fn user_voted(&mut self, user: UserId, market: MarketId) -> Result<bool, StoreError> {
        sqlx::query_scalar(
            "select exists(select 1 from votes where user_id = $1 and market_id = $2)",
        )
        .bind(user.0)
        .bind(market.0)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)
    }

    async fn tally(&mut self, market: MarketId) -> Result<Tally, StoreError> {
        let row = sqlx::query(
            r#"
            select count(*) filter (where o.idx = 0)::bigint as yes_votes,
                   count(*) filter (where o.idx = 1)::bigint as no_votes
              from votes v
              join outcomes o on o.id = v.outcome_id and o.market_id = v.market_id
             where v.market_id = $1
            "#,
        )
        .bind(market.0)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(Tally {
            yes_votes: row.try_get("yes_votes").map_err(db_error)?,
            no_votes: row.try_get("no_votes").map_err(db_error)?,
        })
    }

    async fn votes_count_since(
        &mut self,
        user: UserId,
        since: time::OffsetDateTime,
    ) -> Result<u32, StoreError> {
        let count: i64 = sqlx::query_scalar(
            "select count(*)::bigint from votes where user_id = $1 and created_at >= $2",
        )
        .bind(user.0)
        .bind(since)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        u32::try_from(count).map_err(|_| StoreError::Invariant("vote count overflow"))
    }

    async fn user_created_at(&mut self, user: UserId) -> Result<time::OffsetDateTime, StoreError> {
        sqlx::query_scalar("select created_at from users where id = $1")
            .bind(user.0)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)?
            .ok_or(StoreError::NotFound("user"))
    }

    async fn user_has_channel(&mut self, user: UserId, channel: &str) -> Result<bool, StoreError> {
        sqlx::query_scalar(
            "select exists(select 1 from user_channels where user_id = $1 and channel = $2)",
        )
        .bind(user.0)
        .bind(channel)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)
    }
}

#[async_trait]
impl PoolWriter for PgTx {
    async fn pool_for_update(&mut self, market: MarketId) -> Result<PoolRow, StoreError> {
        let sql = format!(
            "{} where p.market_id = $1 for update of p, yes_reserve, no_reserve",
            super::rows::POOL_SELECT
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(market.0)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)?
            .ok_or(StoreError::NotFound("pool"))?;
        pool_from_row(&row)
    }

    async fn save_reserves(&mut self, market: MarketId, pool: &Pool) -> Result<(), StoreError> {
        let result = sqlx::query(
            r#"
            update pool_reserves pr
               set reserve_micro_shares = case o.idx when 0 then $2 else $3 end
              from outcomes o
             where pr.outcome_id = o.id
               and pr.market_id = $1
               and o.market_id = $1
               and o.idx in (0, 1)
            "#,
        )
        .bind(market.0)
        .bind(pool.yes.0)
        .bind(pool.no.0)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        if result.rows_affected() != 2 {
            return Err(StoreError::Invariant(
                "pool does not have exactly two reserves",
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl LedgerWriter for PgTx {
    async fn txn_by_key(&mut self, key: &str) -> Result<Option<Uuid>, StoreError> {
        sqlx::query_scalar("select id from ledger_transactions where idempotency_key = $1")
            .bind(key)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)
    }

    async fn account(
        &mut self,
        owner: OwnerRef,
        account_currency: Currency,
    ) -> Result<AccountId, StoreError> {
        let (kind, owner_id) = owner_parts(owner);
        let currency = currency_name(account_currency);
        match owner {
            OwnerRef::User(_) | OwnerRef::MarketEscrow(_) | OwnerRef::MarketPool(_) => {
                sqlx::query(
                    r#"
                    insert into ledger_accounts (owner_type, owner_id, currency)
                    values ($1, $2, $3)
                    on conflict (owner_type, owner_id, currency)
                      where owner_type in ('user','pool','escrow') do nothing
                    "#,
                )
                .bind(kind)
                .bind(owner_id)
                .bind(currency)
                .execute(&mut *self.tx)
                .await
                .map_err(db_error)?;
            }
            OwnerRef::Fees
            | OwnerRef::House
            | OwnerRef::Withheld
            | OwnerRef::DepositSuspense
            | OwnerRef::BonusReserve => {
                sqlx::query(
                    r#"
                    insert into ledger_accounts (owner_type, owner_id, currency)
                    values ($1, null, $2)
                    on conflict (owner_type, currency)
                      where owner_type in ('fees','house','withheld','deposit_suspense','bonus_reserve') do nothing
                    "#,
                )
                .bind(kind)
                .bind(currency)
                .execute(&mut *self.tx)
                .await
                .map_err(db_error)?;
            }
            OwnerRef::External => {
                sqlx::query(
                    r#"
                    insert into ledger_accounts (owner_type, owner_id, currency)
                    values ('external', null, $1)
                    on conflict (currency) where owner_type = 'external' do nothing
                    "#,
                )
                .bind(currency)
                .execute(&mut *self.tx)
                .await
                .map_err(db_error)?;
            }
        }
        let id = sqlx::query_scalar(
            r#"
            select id from ledger_accounts
             where owner_type = $1 and owner_id is not distinct from $2 and currency = $3
            "#,
        )
        .bind(kind)
        .bind(owner_id)
        .bind(currency)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?
        .ok_or(StoreError::Invariant("account upsert returned no account"))?;
        Ok(AccountId(id))
    }

    async fn ledger_apply(
        &mut self,
        kind: TxnKind,
        key: &str,
        entries: &[Entry],
    ) -> Result<Uuid, StoreError> {
        Transaction::new(kind, entries.to_vec()).map_err(StoreError::Ledger)?;
        let deltas = aggregate(entries)?;
        let mut ids: Vec<Uuid> = deltas.keys().map(|account| account.0).collect();
        ids.sort_unstable();

        let locked: Vec<Uuid> = sqlx::query_scalar(
            "select id from ledger_accounts where id = any($1) order by id for update",
        )
        .bind(&ids)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        if locked.len() != ids.len() {
            return Err(StoreError::Ledger(LedgerError::UnknownAccount));
        }

        let rows = sqlx::query(
            r#"
            select la.id, la.owner_type, la.currency,
                   coalesce((select sum(le.amount_micro)::bigint
                               from ledger_entries le where le.account_id = la.id), 0)::bigint
                     as balance_micro
              from ledger_accounts la
             where la.id = any($1)
            "#,
        )
        .bind(&ids)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;

        let mut accounts = HashMap::with_capacity(rows.len());
        for row in rows {
            let id: Uuid = row.try_get("id").map_err(db_error)?;
            let owner = owner_type(&row.try_get::<String, _>("owner_type").map_err(db_error)?)?;
            let currency = currency(&row.try_get::<String, _>("currency").map_err(db_error)?)?;
            let balance: i64 = row.try_get("balance_micro").map_err(db_error)?;
            accounts.insert(AccountId(id), (owner, currency, balance));
        }
        validate_posts(&deltas, &accounts)?;

        let id = Uuid::new_v4();
        let inserted = sqlx::query(
            "insert into ledger_transactions (id, kind, idempotency_key) values ($1, $2, $3)",
        )
        .bind(id)
        .bind(txn_kind(kind))
        .bind(key)
        .execute(&mut *self.tx)
        .await;
        if let Err(error) = inserted {
            return if unique_violation(&error) {
                Err(StoreError::DuplicateKey)
            } else {
                Err(db_error(error))
            };
        }
        for entry in entries {
            sqlx::query(
                "insert into ledger_entries (txn_id, account_id, amount_micro) values ($1, $2, $3)",
            )
            .bind(id)
            .bind(entry.account.0)
            .bind(entry.amount.0)
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
        }
        Ok(id)
    }
}

#[async_trait]
impl TradeWriter for PgTx {
    async fn insert_trade(&mut self, trade: NewTrade) -> Result<InsertedTrade, StoreError> {
        let idx: i32 =
            sqlx::query_scalar("select idx from outcomes where id = $1 and market_id = $2")
                .bind(trade.outcome.0)
                .bind(trade.market.0)
                .fetch_optional(&mut *self.tx)
                .await
                .map_err(db_error)?
                .ok_or(StoreError::NotFound("outcome"))?;
        if side_from_idx(idx)? != trade.side {
            return Err(StoreError::Invariant("trade side does not match outcome"));
        }
        let seq: i64 = sqlx::query_scalar(
            "select coalesce(max(seq), 0)::bigint + 1 from trades where market_id = $1",
        )
        .bind(trade.market.0)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        let id = TradeId(Uuid::new_v4());
        let created_at: time::OffsetDateTime = sqlx::query_scalar(
            r#"
            insert into trades
                (id, user_id, market_id, outcome_id, run_id, pending_action_id, txn_id,
                 side, collateral_micro, shares_micro, fee_micro, seq)
            values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
            returning created_at
            "#,
        )
        .bind(id.0)
        .bind(trade.user.0)
        .bind(trade.market.0)
        .bind(trade.outcome.0)
        .bind(trade.run_id)
        .bind(trade.pending_action_id)
        .bind(trade.ledger_txn)
        .bind(action_name(trade.action))
        .bind(trade.gross.0)
        .bind(trade.shares.0)
        .bind(trade.fee.0)
        .bind(seq)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(InsertedTrade {
            id,
            trade_seq: seq,
            created_at,
        })
    }

    async fn trade_by_ledger_txn(&mut self, txn: Uuid) -> Result<Option<TradeReceipt>, StoreError> {
        let row = sqlx::query(
            r#"
            select t.id, t.txn_id, o.idx, t.side, t.shares_micro,
                   t.collateral_micro, t.fee_micro
              from trades t
              join outcomes o on o.id = t.outcome_id and o.market_id = t.market_id
             where t.txn_id = $1
            "#,
        )
        .bind(txn)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        row.as_ref().map(trade_receipt).transpose()
    }
}

#[async_trait]
impl UserReader for PgTx {
    async fn handle(&mut self, user: UserId) -> Result<String, StoreError> {
        sqlx::query_scalar("select handle from users where id = $1")
            .bind(user.0)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)?
            .ok_or(StoreError::NotFound("user"))
    }

    async fn user_by_handle(&mut self, handle: &str) -> Result<Option<UserId>, StoreError> {
        sqlx::query_scalar("select id from users where lower(handle) = lower($1)")
            .bind(handle)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)
            .map(|id| id.map(UserId))
    }

    async fn user_tier(&mut self, user: UserId) -> Result<u8, StoreError> {
        let tier: i32 = sqlx::query_scalar("select tier from reputation where user_id = $1")
            .bind(user.0)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)?
            .ok_or(StoreError::NotFound("reputation"))?;
        u8::try_from(tier).map_err(|_| StoreError::Invariant("invalid reputation tier"))
    }
}

#[async_trait]
impl PositionWriter for PgTx {
    async fn position_for_update(
        &mut self,
        user: UserId,
        outcome: OutcomeId,
    ) -> Result<Option<PositionRow>, StoreError> {
        let row = sqlx::query(
            r#"
            select shares_micro, cost_micro, realized_pnl_micro
              from positions where user_id = $1 and outcome_id = $2 for update
            "#,
        )
        .bind(user.0)
        .bind(outcome.0)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        row.map(|row| {
            Ok(PositionRow {
                user,
                outcome,
                shares: MicroShares(row.try_get("shares_micro").map_err(db_error)?),
                cost: MicroUsd(row.try_get("cost_micro").map_err(db_error)?),
                realized_pnl: MicroUsd(row.try_get("realized_pnl_micro").map_err(db_error)?),
            })
        })
        .transpose()
    }

    async fn save_position(&mut self, position: PositionRow) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            insert into positions
                (user_id, outcome_id, shares_micro, cost_micro, realized_pnl_micro)
            values ($1, $2, $3, $4, $5)
            on conflict (user_id, outcome_id) do update set
                shares_micro = excluded.shares_micro,
                cost_micro = excluded.cost_micro,
                realized_pnl_micro = excluded.realized_pnl_micro
            "#,
        )
        .bind(position.user.0)
        .bind(position.outcome.0)
        .bind(position.shares.0)
        .bind(position.cost.0)
        .bind(position.realized_pnl.0)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(())
    }
}

#[async_trait]
impl TradeEconomyReader for PgTx {
    async fn user_rep(&mut self, user: UserId) -> Result<ReputationRow, StoreError> {
        let row = sqlx::query("select rep_micro, tier from reputation where user_id = $1")
            .bind(user.0)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)?
            .ok_or(StoreError::NotFound("reputation"))?;
        Ok(ReputationRow {
            user,
            rep_micro: row.try_get("rep_micro").map_err(db_error)?,
            tier: u8::try_from(row.try_get::<i32, _>("tier").map_err(db_error)?)
                .map_err(|_| StoreError::Invariant("invalid reputation tier"))?,
        })
    }

    async fn last_buy_at(
        &mut self,
        user: UserId,
        outcome: OutcomeId,
    ) -> Result<Option<time::OffsetDateTime>, StoreError> {
        sqlx::query_scalar(
            "select max(created_at) from trades where user_id = $1 and outcome_id = $2 and side = 'buy'",
        )
        .bind(user.0)
        .bind(outcome.0)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)
    }

    async fn market_position_cost(
        &mut self,
        user: UserId,
        market: MarketId,
    ) -> Result<MicroUsd, StoreError> {
        sqlx::query_scalar(
            r#"select coalesce(sum(p.cost_micro), 0)::bigint
                 from positions p join outcomes o on o.id = p.outcome_id
                where p.user_id = $1 and o.market_id = $2"#,
        )
        .bind(user.0)
        .bind(market.0)
        .fetch_one(&mut *self.tx)
        .await
        .map(MicroUsd)
        .map_err(db_error)
    }
}

#[async_trait]
impl OutboxWriter for PgTx {
    async fn append(&mut self, event: Event) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            insert into events_outbox (aggregate_type, aggregate_id, event_type, payload)
            values ($1, $2, $3, $4)
            "#,
        )
        .bind(event.aggregate_type)
        .bind(event.aggregate_id)
        .bind(event.event_type)
        .bind(event.payload)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn append_batch(&mut self, events: &[Event]) -> Result<(), StoreError> {
        if events.is_empty() {
            return Ok(());
        }
        let aggregate_types: Vec<&str> = events.iter().map(|event| event.aggregate_type).collect();
        let aggregate_ids: Vec<Uuid> = events.iter().map(|event| event.aggregate_id).collect();
        let event_types: Vec<&str> = events.iter().map(|event| event.event_type).collect();
        let payloads: Vec<serde_json::Value> =
            events.iter().map(|event| event.payload.clone()).collect();
        sqlx::query(
            r#"insert into events_outbox (aggregate_type, aggregate_id, event_type, payload)
               select * from unnest($1::text[], $2::uuid[], $3::text[], $4::jsonb[])"#,
        )
        .bind(aggregate_types)
        .bind(aggregate_ids)
        .bind(event_types)
        .bind(payloads)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(())
    }
}

#[async_trait]
impl Committable for PgTx {
    async fn commit(self: Box<Self>) -> Result<(), StoreError> {
        let Self { tx } = *self;
        tx.commit().await.map_err(db_error)
    }
}

fn owner_parts(owner: OwnerRef) -> (&'static str, Option<Uuid>) {
    match owner {
        OwnerRef::User(user) => ("user", Some(user.0)),
        OwnerRef::MarketEscrow(market) => ("escrow", Some(market.0)),
        OwnerRef::MarketPool(market) => ("pool", Some(market.0)),
        OwnerRef::Fees => ("fees", None),
        OwnerRef::House => ("house", None),
        OwnerRef::External => ("external", None),
        OwnerRef::Withheld => ("withheld", None),
        OwnerRef::DepositSuspense => ("deposit_suspense", None),
        OwnerRef::BonusReserve => ("bonus_reserve", None),
    }
}

fn aggregate(entries: &[Entry]) -> Result<HashMap<AccountId, i128>, StoreError> {
    let mut deltas = HashMap::new();
    for entry in entries {
        let delta = deltas.entry(entry.account).or_insert(0_i128);
        *delta = delta
            .checked_add(i128::from(entry.amount.0))
            .ok_or(StoreError::Ledger(LedgerError::Overflow))?;
    }
    Ok(deltas)
}

fn validate_posts(
    deltas: &HashMap<AccountId, i128>,
    accounts: &HashMap<AccountId, (OwnerType, Currency, i64)>,
) -> Result<(), StoreError> {
    let mut currency_sums = HashMap::<Currency, i128>::new();
    for (account, delta) in deltas {
        let (_, account_currency, _) = accounts
            .get(account)
            .ok_or(StoreError::Ledger(LedgerError::UnknownAccount))?;
        let sum = currency_sums.entry(*account_currency).or_default();
        *sum = sum
            .checked_add(*delta)
            .ok_or(StoreError::Ledger(LedgerError::Overflow))?;
    }
    for (currency, sum_micro) in currency_sums {
        if sum_micro != 0 {
            return Err(StoreError::Ledger(LedgerError::UnbalancedCurrency {
                currency,
                sum_micro,
            }));
        }
    }
    for (account, delta) in deltas {
        let (owner, _, balance) = accounts
            .get(account)
            .ok_or(StoreError::Ledger(LedgerError::UnknownAccount))?;
        let post = i128::from(*balance)
            .checked_add(*delta)
            .ok_or(StoreError::Ledger(LedgerError::Overflow))?;
        i64::try_from(post).map_err(|_| StoreError::Ledger(LedgerError::Overflow))?;
        if post < 0 && *owner != OwnerType::External {
            return Err(StoreError::Ledger(LedgerError::InsufficientFunds {
                account: *account,
            }));
        }
    }
    Ok(())
}

fn trade_receipt(row: &sqlx::postgres::PgRow) -> Result<TradeReceipt, StoreError> {
    let shares: i64 = row.try_get("shares_micro").map_err(db_error)?;
    let gross: i64 = row.try_get("collateral_micro").map_err(db_error)?;
    let avg = i128::from(gross)
        .checked_mul(1_000_000)
        .ok_or(StoreError::Invariant("trade average price overflow"))?
        / i128::from(shares);
    Ok(TradeReceipt {
        trade_id: TradeId(row.try_get("id").map_err(db_error)?),
        ledger_txn: row.try_get("txn_id").map_err(db_error)?,
        side: side_from_idx(row.try_get("idx").map_err(db_error)?)?,
        action: action(&row.try_get::<String, _>("side").map_err(db_error)?)?,
        shares: MicroShares(shares),
        gross: MicroUsd(gross),
        fee: MicroUsd(row.try_get("fee_micro").map_err(db_error)?),
        avg_price_micro: i64::try_from(avg)
            .map_err(|_| StoreError::Invariant("trade average price overflow"))?,
        replayed: false,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::pg::PgStore;

    #[test]
    fn posting_validation_rejects_an_i64_balance_overflow() {
        let account = AccountId(Uuid::new_v4());
        let counterparty = AccountId(Uuid::new_v4());
        let deltas = HashMap::from([(account, 1_i128), (counterparty, -1_i128)]);
        let accounts = HashMap::from([
            (account, (OwnerType::User, Currency::Usdc, i64::MAX)),
            (counterparty, (OwnerType::External, Currency::Usdc, 0)),
        ]);

        assert_eq!(
            validate_posts(&deltas, &accounts),
            Err(StoreError::Ledger(LedgerError::Overflow))
        );
    }

    #[tokio::test]
    async fn deferred_balance_trigger_surfaces_as_integrity_on_commit() {
        let url = std::env::var("DATABASE_URL").unwrap();
        let store = PgStore::connect(&url).await.unwrap();
        let mut tx = store.tx().await.unwrap();
        let key = format!("trigger-probe-{}", Uuid::new_v4());
        tx.serialize_key(&key).await.unwrap();
        let external = tx
            .account(OwnerRef::External, Currency::Usdc)
            .await
            .unwrap();
        let user = tx
            .account(OwnerRef::User(UserId(Uuid::new_v4())), Currency::Usdc)
            .await
            .unwrap();
        let ledger_txn = tx
            .ledger_apply(
                TxnKind::Deposit,
                &key,
                &[
                    Entry {
                        account: external,
                        amount: MicroUsd(-5),
                    },
                    Entry {
                        account: user,
                        amount: MicroUsd(5),
                    },
                ],
            )
            .await
            .unwrap();

        // Adapter-internal corruption probe: public ledger_apply accepted a valid
        // transaction; this extra raw leg proves the DB backstop rejects a buggy writer.
        sqlx::query(
            "insert into ledger_entries (txn_id, account_id, amount_micro) values ($1, $2, 1)",
        )
        .bind(ledger_txn)
        .bind(external.0)
        .execute(&mut *tx.tx)
        .await
        .unwrap();
        let result = Box::new(tx).commit().await;
        assert!(matches!(result, Err(StoreError::Integrity(_))));
    }
}
