use application::error::StoreError;
use application::model::{MarketId, RealizationFact, RealizationSource};
use application::ports::{RealizationWriter, SeedEconomyIo};
use async_trait::async_trait;
use domain::money::MicroUsd;

use super::rows::db_error;
use super::store::PgTx;

const fn source_name(source: RealizationSource) -> &'static str {
    match source {
        RealizationSource::Sell => "sell",
        RealizationSource::Settlement => "settlement",
        RealizationSource::Void => "void",
    }
}

#[async_trait]
impl RealizationWriter for PgTx {
    async fn insert_realization(&mut self, fact: &RealizationFact) -> Result<bool, StoreError> {
        let result = sqlx::query(
            r#"insert into realizations
                   (user_id, market_id, outcome_id, source, realized_delta_micro, payout_micro, txn_id, created_at)
               values ($1, $2, $3, $4, $5, $6, $7, $8)
               on conflict (txn_id, user_id, outcome_id) do nothing"#,
        )
        .bind(fact.user.0)
        .bind(fact.market.0)
        .bind(fact.outcome.0)
        .bind(source_name(fact.source))
        .bind(fact.realized_delta.0)
        .bind(fact.payout.0)
        .bind(fact.ledger_txn)
        .bind(fact.created_at)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(result.rows_affected() == 1)
    }
}

#[async_trait]
impl SeedEconomyIo for PgTx {
    async fn seeded_market_by_key(&mut self, key: &str) -> Result<Option<MarketId>, StoreError> {
        sqlx::query_scalar(
            r#"select la.owner_id
                 from ledger_transactions lt
                 join ledger_entries le on le.txn_id = lt.id and le.amount_micro > 0
                 join ledger_accounts la on la.id = le.account_id and la.owner_type = 'escrow'
                where lt.idempotency_key = $1 and lt.kind = 'seed'
                limit 1"#,
        )
        .bind(key)
        .fetch_optional(&mut *self.tx)
        .await
        .map(|market| market.map(MarketId))
        .map_err(db_error)
    }

    async fn lock_lp_kill_switch(&mut self) -> Result<(), StoreError> {
        sqlx::query("select pg_advisory_xact_lock(3, hashtext('lp_kill'))")
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
        Ok(())
    }

    async fn lp_pnl_sum(
        &mut self,
        since: time::OffsetDateTime,
        until: time::OffsetDateTime,
    ) -> Result<MicroUsd, StoreError> {
        sqlx::query_scalar(
            r#"select coalesce(sum(lp_pnl_micro), 0)::bigint
                  from markets
                 where settled_at >= $1 and settled_at < $2
                   and lp_pnl_micro is not null"#,
        )
        .bind(since)
        .bind(until)
        .fetch_one(&mut *self.tx)
        .await
        .map(MicroUsd)
        .map_err(db_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realization_source_names_match_the_migration_constraint() {
        assert_eq!(source_name(RealizationSource::Sell), "sell");
        assert_eq!(source_name(RealizationSource::Settlement), "settlement");
        assert_eq!(source_name(RealizationSource::Void), "void");
    }
}
