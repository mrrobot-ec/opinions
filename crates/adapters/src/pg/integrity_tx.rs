use application::error::StoreError;
use application::model::{IntegrityReportRow, IntegritySweepConfig, MarketId};
use application::ports::IntegritySweepIo;
use async_trait::async_trait;
use sqlx::Row;

use super::rows::db_error;
use super::store::PgTx;

#[async_trait]
impl IntegritySweepIo for PgTx {
    async fn vote_stats(
        &mut self,
        market: MarketId,
        config: IntegritySweepConfig,
    ) -> Result<domain::integrity::VoteStats, StoreError> {
        let window = i64::try_from(config.burst_window_secs)
            .map_err(|_| StoreError::Invariant("burst window overflow"))?;
        let age = i64::try_from(config.young_account_age_secs)
            .map_err(|_| StoreError::Invariant("young-account age overflow"))?;
        let horizon = i64::from(config.prior_horizon_windows);
        let row = sqlx::query(
            r#"
            with market_window as (
              select closes_at,
                     closes_at - $2 * interval '1 second' as last_start,
                     closes_at - ($2 * ($3 + 1)) * interval '1 second' as prior_start
                from markets where id = $1
            ), base as materialized (
              select v.created_at, v.cast_ip, v.device_hash, u.created_at as user_created_at,
                     mw.closes_at, mw.last_start, mw.prior_start
                from votes v join users u on u.id = v.user_id
                cross join market_window mw where v.market_id = $1
            ), subnet_counts as (
              select case family(cast_ip) when 4 then set_masklen(cast_ip, 24)::text
                          else set_masklen(cast_ip, 64)::text end as prefix,
                     count(*)::bigint as n
                from base where cast_ip is not null group by prefix
            ), device_counts as (
              select device_hash, count(*)::bigint as n
                from base where device_hash is not null group by device_hash
            )
            select count(*)::bigint as total,
                   count(*) filter (where created_at >= last_start and created_at < closes_at)::bigint as last_window,
                   count(*) filter (where created_at >= prior_start and created_at < last_start)::bigint as prior_total,
                   count(*) filter (where created_at - user_created_at < $4 * interval '1 second')::bigint as young,
                   count(cast_ip)::bigint as subnet_observed,
                   coalesce((select max(n) from subnet_counts), 0)::bigint as top_subnet,
                   count(device_hash)::bigint as device_observed,
                   coalesce((select max(n) from device_counts), 0)::bigint as top_device
              from base
            "#,
        )
        .bind(market.0)
        .bind(window)
        .bind(horizon)
        .bind(age)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(domain::integrity::VoteStats {
            total: count(&row, "total")?,
            last_window: count(&row, "last_window")?,
            prior_windows_total: count(&row, "prior_total")?,
            prior_horizon_windows: config.prior_horizon_windows,
            young_accounts: count(&row, "young")?,
            subnet_observed: count(&row, "subnet_observed")?,
            top_subnet_count: count(&row, "top_subnet")?,
            device_observed: count(&row, "device_observed")?,
            top_device_count: count(&row, "top_device")?,
        })
    }

    async fn insert_integrity_report(
        &mut self,
        report: &IntegrityReportRow,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            r#"insert into integrity_reports (market_id, checks, verdict, created_at)
               values ($1, $2, $3, $4) on conflict (market_id) do nothing"#,
        )
        .bind(report.market.0)
        .bind(&report.checks)
        .bind(verdict_name(report.verdict))
        .bind(report.created_at)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(result.rows_affected() == 1)
    }

    async fn integrity_report(
        &mut self,
        market: MarketId,
    ) -> Result<Option<IntegrityReportRow>, StoreError> {
        read_report(self, market).await
    }
}

pub(super) async fn read_report(
    tx: &mut PgTx,
    market: MarketId,
) -> Result<Option<IntegrityReportRow>, StoreError> {
    let row = sqlx::query(
        "select checks, verdict, created_at from integrity_reports where market_id = $1",
    )
    .bind(market.0)
    .fetch_optional(&mut *tx.tx)
    .await
    .map_err(db_error)?;
    row.map(|row| {
        let verdict: String = row.try_get("verdict").map_err(db_error)?;
        Ok(IntegrityReportRow {
            market,
            checks: row.try_get("checks").map_err(db_error)?,
            verdict: parse_verdict(&verdict)?,
            created_at: row.try_get("created_at").map_err(db_error)?,
        })
    })
    .transpose()
}

fn count(row: &sqlx::postgres::PgRow, name: &str) -> Result<u32, StoreError> {
    u32::try_from(row.try_get::<i64, _>(name).map_err(db_error)?)
        .map_err(|_| StoreError::Invariant("vote stats count overflow"))
}

const fn verdict_name(verdict: domain::integrity::Verdict) -> &'static str {
    match verdict {
        domain::integrity::Verdict::Pass => "pass",
        domain::integrity::Verdict::Flag => "flag",
    }
}

pub(super) fn parse_verdict(value: &str) -> Result<domain::integrity::Verdict, StoreError> {
    match value {
        "pass" => Ok(domain::integrity::Verdict::Pass),
        "flag" => Ok(domain::integrity::Verdict::Flag),
        _ => Err(StoreError::Invariant("unknown integrity verdict")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_database_vocabulary_is_total() {
        for verdict in [
            domain::integrity::Verdict::Pass,
            domain::integrity::Verdict::Flag,
        ] {
            assert_eq!(parse_verdict(verdict_name(verdict)), Ok(verdict));
        }
        assert_eq!(
            parse_verdict("unknown"),
            Err(StoreError::Invariant("unknown integrity verdict"))
        );
    }
}
