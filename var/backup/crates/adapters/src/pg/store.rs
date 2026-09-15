use application::error::StoreError;
use application::model::{
    AdminAction, DailyFeeRow, DueMarket, FlaggedMarketRow, IntegrityReportRow, MarketId, MarketRow,
    MarketSnapshot, OutcomeId, PoolRow, PositionView, PricePoint, ReputationRow, Tally, TapeRow,
    TraderRow, UserId, VoterRow, WithdrawalEligibilityView,
};
use application::ports::{
    AdvanceTx, AuditWrite, BootstrapTx, CommentTx, ContentTx, DepositTx, IntegrityTx,
    InvariantReadTx, MarketQueries, NotificationTx, OpsAuditTx, OpsConfigTx, ResolveTx, SeedTx,
    Store, TradeTx, UnwindTx, VideoTx, VoteTx, WithdrawalEligibility,
};
use async_trait::async_trait;
use domain::money::{MicroShares, MicroUsd};
use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use super::rows::{db_error, market_from_row, pool_from_row, side_from_idx};

#[derive(Clone)]
pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    #[must_use]
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Opens a bounded PostgreSQL connection pool.
    ///
    /// # Errors
    /// Returns [`StoreError::Backend`] if the initial connection cannot be established.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(16)
            .connect(url)
            .await
            .map_err(db_error)?;
        Ok(Self { pool })
    }

    /// Exposes the pool for adapter-level setup and diagnostics. Application use cases
    /// depend only on the port traits.
    #[must_use]
    pub fn pool_handle(&self) -> &PgPool {
        &self.pool
    }

    pub(super) async fn tx(&self) -> Result<PgTx, StoreError> {
        Ok(PgTx {
            tx: self.pool.begin().await.map_err(db_error)?,
        })
    }
}

pub(super) struct PgTx {
    pub(super) tx: Transaction<'static, Postgres>,
}

const MARKET_BY_ID: &str = r#"
    select m.id, m.slug, m.question, m.status, m.min_votes_to_resolve, m.opens_at,
           m.closes_at, m.tally_hidden_at, m.curator_flagged_at, m.integrity_due_at,
           m.poster_asset_url, m.video_asset_url,
           yes_outcome.id as yes_outcome, no_outcome.id as no_outcome
      from markets m
      join outcomes yes_outcome on yes_outcome.market_id = m.id and yes_outcome.idx = 0
      join outcomes no_outcome on no_outcome.market_id = m.id and no_outcome.idx = 1
     where m.id = $1
"#;

const MARKET_BY_SLUG: &str = r#"
    select m.id, m.slug, m.question, m.status, m.min_votes_to_resolve, m.opens_at,
           m.closes_at, m.tally_hidden_at, m.curator_flagged_at, m.integrity_due_at,
           m.poster_asset_url, m.video_asset_url,
           yes_outcome.id as yes_outcome, no_outcome.id as no_outcome
      from markets m
      join outcomes yes_outcome on yes_outcome.market_id = m.id and yes_outcome.idx = 0
      join outcomes no_outcome on no_outcome.market_id = m.id and no_outcome.idx = 1
     where m.slug = $1
"#;

const POOL_BY_MARKET: &str = r#"
    select p.id as pool_id, p.market_id, p.fee_bps,
           yes_reserve.reserve_micro_shares as yes_reserve,
           no_reserve.reserve_micro_shares as no_reserve
      from pools p
      join outcomes yes_outcome on yes_outcome.market_id = p.market_id and yes_outcome.idx = 0
      join outcomes no_outcome on no_outcome.market_id = p.market_id and no_outcome.idx = 1
      join pool_reserves yes_reserve
        on yes_reserve.pool_id = p.id and yes_reserve.outcome_id = yes_outcome.id
       and yes_reserve.market_id = p.market_id
      join pool_reserves no_reserve
        on no_reserve.pool_id = p.id and no_reserve.outcome_id = no_outcome.id
       and no_reserve.market_id = p.market_id
     where p.market_id = $1
"#;

#[async_trait]
impl Store for PgStore {
    async fn trade_tx(&self) -> Result<Box<dyn TradeTx + '_>, StoreError> {
        Ok(Box::new(self.tx().await?))
    }

    async fn vote_tx(&self) -> Result<Box<dyn VoteTx + '_>, StoreError> {
        Ok(Box::new(self.tx().await?))
    }

    async fn resolve_tx(&self) -> Result<Box<dyn ResolveTx + '_>, StoreError> {
        Ok(Box::new(self.tx().await?))
    }

    async fn deposit_tx(&self) -> Result<Box<dyn DepositTx + '_>, StoreError> {
        Ok(Box::new(self.tx().await?))
    }

    async fn seed_tx(&self) -> Result<Box<dyn SeedTx + '_>, StoreError> {
        Ok(Box::new(self.tx().await?))
    }

    async fn advance_tx(&self) -> Result<Box<dyn AdvanceTx + '_>, StoreError> {
        Ok(Box::new(self.tx().await?))
    }

    async fn bootstrap_tx(&self) -> Result<Box<dyn BootstrapTx + '_>, StoreError> {
        Ok(Box::new(self.tx().await?))
    }

    async fn integrity_tx(&self) -> Result<Box<dyn IntegrityTx + '_>, StoreError> {
        Ok(Box::new(self.tx().await?))
    }

    async fn comment_tx(&self) -> Result<Box<dyn CommentTx + '_>, StoreError> {
        Ok(Box::new(self.tx().await?))
    }

    async fn notification_tx(&self) -> Result<Box<dyn NotificationTx + '_>, StoreError> {
        Ok(Box::new(self.tx().await?))
    }

    async fn content_tx(&self) -> Result<Box<dyn ContentTx + '_>, StoreError> {
        super::content_tx::open(self).await
    }

    async fn video_tx(&self) -> Result<Box<dyn VideoTx + '_>, StoreError> {
        super::video_tx::open(self).await
    }

    // Phase 6 skeleton factories (plan §2, Task 6.0a): fail CLOSED with the
    // typed area label until the owning wave lands its `pg/ops_*` module.
    async fn ops_config_tx(&self) -> Result<Box<dyn OpsConfigTx + '_>, StoreError> {
        Ok(Box::new(self.open_ops_config_tx().await?))
    }

    async fn ops_audit_tx(&self) -> Result<Box<dyn OpsAuditTx + '_>, StoreError> {
        Ok(Box::new(self.tx().await?))
    }

    async fn invariant_read_tx(&self) -> Result<Box<dyn InvariantReadTx + '_>, StoreError> {
        Ok(Box::new(super::invariant_read_tx::open(self).await?))
    }

    async fn unwind_tx(&self) -> Result<Box<dyn UnwindTx + '_>, StoreError> {
        Ok(Box::new(self.tx().await?))
    }

    async fn withdraw_tx(
        &self,
    ) -> Result<Box<dyn application::ports::WithdrawTx + '_>, StoreError> {
        let tx = self.pool.begin().await.map_err(db_error)?;
        Ok(Box::new(super::withdraw_tx::PgWithdrawTx::new(tx)))
    }

    async fn deposit_admission_tx(
        &self,
    ) -> Result<Box<dyn application::ports::DepositAdmissionTx + '_>, StoreError> {
        Ok(Box::new(self.tx().await?))
    }

    async fn credit_convert_tx(
        &self,
    ) -> Result<Box<dyn application::ports::CreditConvertTx + '_>, StoreError> {
        Ok(Box::new(self.tx().await?))
    }
}

#[async_trait]
impl WithdrawalEligibility for PgStore {
    async fn withdrawal_eligibility(
        &self,
        user: UserId,
    ) -> Result<WithdrawalEligibilityView, StoreError> {
        super::unwind_tx::withdrawal_eligibility(self, user).await
    }
}

#[async_trait]
impl AuditWrite for PgTx {
    // The real D26 sink (W2): one `admin_actions` row in the SAME
    // transaction as the admin mutation's effect.
    async fn audit_insert(&mut self, action: AdminAction) -> Result<(), StoreError> {
        super::ops_audit_tx::audit_insert(self, action).await
    }
}

#[async_trait]
impl MarketQueries for PgStore {
    async fn market_by_ref(&self, reference: &str) -> Result<MarketRow, StoreError> {
        let row = if let Ok(id) = Uuid::parse_str(reference) {
            sqlx::query(MARKET_BY_ID)
                .bind(id)
                .fetch_optional(&self.pool)
                .await
                .map_err(db_error)?
        } else {
            sqlx::query(MARKET_BY_SLUG)
                .bind(reference)
                .fetch_optional(&self.pool)
                .await
                .map_err(db_error)?
        }
        .ok_or(StoreError::NotFound("market"))?;
        market_from_row(&row)
    }

    async fn list_markets(&self, status: Option<&str>) -> Result<Vec<MarketRow>, StoreError> {
        let sql = format!(
            "{} where ($1::text is null or m.status = $1) order by m.slug",
            super::rows::MARKET_SELECT
        );
        let rows = sqlx::query(&sql)
            .bind(status)
            .fetch_all(&self.pool)
            .await
            .map_err(db_error)?;
        rows.iter().map(market_from_row).collect()
    }

    async fn pool(&self, market: MarketId) -> Result<PoolRow, StoreError> {
        let row = sqlx::query(POOL_BY_MARKET)
            .bind(market.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_error)?
            .ok_or(StoreError::NotFound("pool"))?;
        pool_from_row(&row)
    }

    async fn market_snapshot(
        &self,
        market: MarketId,
        now: time::OffsetDateTime,
    ) -> Result<MarketSnapshot, StoreError> {
        let row = sqlx::query(
            r#"
            select m.status, m.closes_at, m.tally_hidden_at,
                   m.poster_asset_url, m.video_asset_url, p.fee_bps,
                   yr.reserve_micro_shares as yes_reserve,
                   nr.reserve_micro_shares as no_reserve,
                   count(v.id) filter (where o.idx = 0)::bigint as yes_votes,
                   count(v.id) filter (where o.idx = 1)::bigint as no_votes
              from markets m
              join pools p on p.market_id = m.id
              join outcomes yo on yo.market_id = m.id and yo.idx = 0
              join outcomes no on no.market_id = m.id and no.idx = 1
              join pool_reserves yr on yr.pool_id = p.id and yr.outcome_id = yo.id
              join pool_reserves nr on nr.pool_id = p.id and nr.outcome_id = no.id
              left join votes v on v.market_id = m.id
              left join outcomes o on o.id = v.outcome_id and o.market_id = v.market_id
             where m.id = $1
             group by m.status, m.closes_at, m.tally_hidden_at,
                      m.poster_asset_url, m.video_asset_url, p.fee_bps,
                      yr.reserve_micro_shares, nr.reserve_micro_shares
            "#,
        )
        .bind(market.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_error)?
        .ok_or(StoreError::NotFound("market"))?;
        let state = super::rows::market_state(row.try_get("status").map_err(db_error)?)?;
        let pool = domain::amm::Pool::new(
            MicroShares(row.try_get("yes_reserve").map_err(db_error)?),
            MicroShares(row.try_get("no_reserve").map_err(db_error)?),
            domain::money::BasisPoints(
                u16::try_from(row.try_get::<i32, _>("fee_bps").map_err(db_error)?)
                    .map_err(|_| StoreError::Invariant("invalid fee bps"))?,
            ),
        )
        .map_err(|_| StoreError::Invariant("invalid pool"))?;
        let tally_hidden_at = row.try_get("tally_hidden_at").map_err(db_error)?;
        let tally =
            (state == domain::market::MarketState::Live && now < tally_hidden_at).then(|| Tally {
                yes_votes: row.try_get("yes_votes").unwrap_or(0),
                no_votes: row.try_get("no_votes").unwrap_or(0),
            });
        Ok(MarketSnapshot {
            market,
            state,
            price_yes_micro: domain::amm::price_micro(&pool, domain::amm::Side::Yes),
            price_no_micro: domain::amm::price_micro(&pool, domain::amm::Side::No),
            tally,
            closes_at: row.try_get("closes_at").map_err(db_error)?,
            tally_hidden_at,
            under_review: state == domain::market::MarketState::Resolving,
            poster_asset_url: row.try_get("poster_asset_url").map_err(db_error)?,
            video_asset_url: row.try_get("video_asset_url").map_err(db_error)?,
        })
    }

    async fn due_markets(
        &self,
        now: time::OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<DueMarket>, StoreError> {
        let rows = sqlx::query(
            r#"
            select id, status from (
                select id, status, opens_at as due_at from markets
                 where status = 'scheduled' and opens_at <= $1
                union all
                select id, status, tally_hidden_at as due_at from markets
                 where status = 'live' and tally_hidden_at <= $1
                union all
                select id, status, closes_at as due_at from markets
                 where status = 'closing' and closes_at <= $1
                union all
                select id, status, closes_at as due_at from markets
                 where status = 'closed' and curator_flagged_at is null
                union all
                select id, status, integrity_due_at as due_at from markets
                 where status = 'resolving' and curator_flagged_at is null
                   and integrity_due_at <= $1
            ) due
            order by due_at, id
            limit $2
            "#,
        )
        .bind(now)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(db_error)?;
        rows.iter()
            .map(|row| {
                let state: String = row.try_get("status").map_err(db_error)?;
                Ok(DueMarket {
                    market: MarketId(row.try_get("id").map_err(db_error)?),
                    state: super::rows::market_state(&state)?,
                })
            })
            .collect()
    }

    async fn price_history(
        &self,
        market: MarketId,
        bucket_secs: u32,
        since: time::OffsetDateTime,
    ) -> Result<Vec<PricePoint>, StoreError> {
        if !(1..=86_400).contains(&bucket_secs) {
            return Err(StoreError::Invariant("invalid chart bucket"));
        }
        let rows = sqlx::query(
            r#"
            with priced as (
              select to_timestamp(floor(extract(epoch from t.created_at) / $2) * $2) as bucket_start,
                     t.collateral_micro::numeric as weight,
                     case when o.idx = 0 then
                       t.collateral_micro::numeric * 1000000::numeric / t.shares_micro::numeric
                     else
                       1000000::numeric -
                       t.collateral_micro::numeric * 1000000::numeric / t.shares_micro::numeric
                     end as yes_price
                from trades t
                join outcomes o on o.id = t.outcome_id and o.market_id = t.market_id
               where t.market_id = $1 and t.created_at >= $3
            )
            select bucket_start,
                   floor(sum(yes_price * weight) / sum(weight))::text as avg_price,
                   sum(weight)::text as volume,
                   count(*)::bigint as trades
              from priced
             group by bucket_start
             order by bucket_start
            "#,
        )
        .bind(market.0)
        .bind(i64::from(bucket_secs))
        .bind(since)
        .fetch_all(&self.pool)
        .await
        .map_err(db_error)?;
        rows.iter()
            .map(|row| {
                let avg: String = row.try_get("avg_price").map_err(db_error)?;
                let volume: String = row.try_get("volume").map_err(db_error)?;
                let trades: i64 = row.try_get("trades").map_err(db_error)?;
                Ok(PricePoint {
                    bucket_start: row.try_get("bucket_start").map_err(db_error)?,
                    avg_price_micro: avg
                        .parse()
                        .map_err(|_| StoreError::Invariant("chart price overflow"))?,
                    volume_micro: volume
                        .parse()
                        .map_err(|_| StoreError::Invariant("chart volume overflow"))?,
                    trades: u32::try_from(trades)
                        .map_err(|_| StoreError::Invariant("chart count overflow"))?,
                })
            })
            .collect()
    }

    async fn tape(&self, market: MarketId, limit: u32) -> Result<Vec<TapeRow>, StoreError> {
        let rows = sqlx::query(
            r#"
            select u.handle, o.idx, t.side as action, t.collateral_micro,
                   t.created_at, t.seq
              from trades t
              join users u on u.id = t.user_id
              join outcomes o on o.id = t.outcome_id and o.market_id = t.market_id
             where t.market_id = $1
             order by t.seq desc
             limit $2
            "#,
        )
        .bind(market.0)
        .bind(i64::from(limit.min(200)))
        .fetch_all(&self.pool)
        .await
        .map_err(db_error)?;
        rows.iter()
            .map(|row| {
                let action_name: String = row.try_get("action").map_err(db_error)?;
                Ok(TapeRow {
                    handle: row.try_get("handle").map_err(db_error)?,
                    side: side_from_idx(row.try_get("idx").map_err(db_error)?)?,
                    action: super::rows::action(&action_name)?,
                    collateral_micro: row.try_get("collateral_micro").map_err(db_error)?,
                    created_at: row.try_get("created_at").map_err(db_error)?,
                    trade_seq: row.try_get("seq").map_err(db_error)?,
                })
            })
            .collect()
    }

    async fn positions(&self, user: UserId) -> Result<Vec<PositionView>, StoreError> {
        let rows = sqlx::query(
            r#"
            select o.market_id, p.outcome_id, o.idx, p.shares_micro,
                   p.cost_micro, p.realized_pnl_micro, r.rep_micro, r.tier
              from positions p
              join outcomes o on o.id = p.outcome_id
              join reputation r on r.user_id = p.user_id
             where p.user_id = $1
             order by p.outcome_id
            "#,
        )
        .bind(user.0)
        .fetch_all(&self.pool)
        .await
        .map_err(db_error)?;
        rows.iter().map(position_view).collect()
    }

    async fn user_voted(&self, user: UserId, market: MarketId) -> Result<bool, StoreError> {
        sqlx::query_scalar(
            "select exists(select 1 from votes where user_id = $1 and market_id = $2)",
        )
        .bind(user.0)
        .bind(market.0)
        .fetch_one(&self.pool)
        .await
        .map_err(db_error)
    }

    async fn user_by_channel(
        &self,
        channel: &str,
        address: &str,
    ) -> Result<Option<UserId>, StoreError> {
        let id: Option<Uuid> = sqlx::query_scalar(
            "select user_id from user_channels where channel = $1 and address = $2",
        )
        .bind(channel)
        .bind(address)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_error)?;
        Ok(id.map(UserId))
    }

    async fn user_rep(&self, user: UserId) -> Result<ReputationRow, StoreError> {
        let row = sqlx::query("select rep_micro, tier from reputation where user_id = $1")
            .bind(user.0)
            .fetch_optional(&self.pool)
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
        &self,
        user: UserId,
        outcome: OutcomeId,
    ) -> Result<Option<time::OffsetDateTime>, StoreError> {
        sqlx::query_scalar(
            "select max(created_at) from trades where user_id = $1 and outcome_id = $2 and side = 'buy'",
        )
        .bind(user.0)
        .bind(outcome.0)
        .fetch_one(&self.pool)
        .await
        .map_err(db_error)
    }

    async fn flagged_markets(&self) -> Result<Vec<FlaggedMarketRow>, StoreError> {
        let sql = format!(
            r#"select base.*, ir.checks, ir.verdict, ir.created_at as report_created_at
                 from ({}) base
                 left join integrity_reports ir on ir.market_id = base.id
                where base.status = 'resolving' or base.curator_flagged_at is not null
                order by base.id"#,
            super::rows::MARKET_SELECT
        );
        let rows = sqlx::query(&sql)
            .fetch_all(&self.pool)
            .await
            .map_err(db_error)?;
        rows.iter()
            .map(|row| {
                let market = market_from_row(row)?;
                let checks: Option<serde_json::Value> = row.try_get("checks").map_err(db_error)?;
                let report = checks
                    .map(|checks| -> Result<IntegrityReportRow, StoreError> {
                        let verdict: String = row.try_get("verdict").map_err(db_error)?;
                        Ok(IntegrityReportRow {
                            market: market.id,
                            checks,
                            verdict: super::integrity_tx::parse_verdict(&verdict)?,
                            created_at: row.try_get("report_created_at").map_err(db_error)?,
                        })
                    })
                    .transpose()?;
                Ok(FlaggedMarketRow { market, report })
            })
            .collect()
    }

    async fn top_traders(
        &self,
        since: time::OffsetDateTime,
        until: time::OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<TraderRow>, StoreError> {
        let rows = sqlx::query(
            r#"select u.handle, sum(r.realized_delta_micro)::bigint as pnl,
                      count(*)::bigint as realizations
                 from realizations r join users u on u.id = r.user_id
                where r.created_at >= $1 and r.created_at < $2
                group by r.user_id, u.handle
                order by pnl desc, realizations desc, u.handle
                limit $3"#,
        )
        .bind(since)
        .bind(until)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(db_error)?;
        rows.iter()
            .map(|row| {
                let count: i64 = row.try_get("realizations").map_err(db_error)?;
                Ok(TraderRow {
                    handle: row.try_get("handle").map_err(db_error)?,
                    realized_pnl_micro: row.try_get("pnl").map_err(db_error)?,
                    realizations: u32::try_from(count)
                        .map_err(|_| StoreError::Invariant("realization count overflow"))?,
                })
            })
            .collect()
    }

    async fn top_voters(
        &self,
        since: time::OffsetDateTime,
        until: time::OffsetDateTime,
        limit: u32,
        min_scored: u32,
    ) -> Result<Vec<VoterRow>, StoreError> {
        let rows = sqlx::query(
            r#"select u.handle, round(avg(vs.score_bp))::bigint as avg_score_bp,
                      count(distinct v.market_id)::bigint as markets_scored, r.tier
                 from vote_scores vs
                 join votes v on v.id = vs.vote_id
                 join users u on u.id = v.user_id
                 join reputation r on r.user_id = v.user_id
                where vs.created_at >= $1 and vs.created_at < $2 and r.tier >= 1
                group by v.user_id, u.handle, r.tier
               having count(distinct v.market_id) >= $3
                order by avg_score_bp desc, markets_scored desc, u.handle
                limit $4"#,
        )
        .bind(since)
        .bind(until)
        .bind(i64::from(min_scored))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(db_error)?;
        rows.iter()
            .map(|row| {
                let count: i64 = row.try_get("markets_scored").map_err(db_error)?;
                let tier: i32 = row.try_get("tier").map_err(db_error)?;
                Ok(VoterRow {
                    handle: row.try_get("handle").map_err(db_error)?,
                    avg_score_bp: row.try_get("avg_score_bp").map_err(db_error)?,
                    markets_scored: u32::try_from(count)
                        .map_err(|_| StoreError::Invariant("market score count overflow"))?,
                    tier: u8::try_from(tier)
                        .map_err(|_| StoreError::Invariant("invalid reputation tier"))?,
                })
            })
            .collect()
    }

    async fn fee_summary(
        &self,
        since: time::OffsetDateTime,
        until: time::OffsetDateTime,
    ) -> Result<Vec<DailyFeeRow>, StoreError> {
        let rows = sqlx::query(
            r#"select (lt.created_at at time zone 'UTC')::date as day,
                      coalesce(sum(le.amount_micro) filter (where lt.kind = 'trade'), 0)::bigint
                        as trade_fee_micro,
                      coalesce(sum(le.amount_micro) filter (where lt.kind = 'payout'), 0)::bigint
                        as payout_dust_micro
                 from ledger_entries le
                 join ledger_accounts la on la.id = le.account_id
                 join ledger_transactions lt on lt.id = le.txn_id
                where la.owner_type = 'fees' and la.currency = 'usdc'
                  and lt.created_at >= $1 and lt.created_at < $2
                  and lt.kind in ('trade', 'payout')
                group by day order by day"#,
        )
        .bind(since)
        .bind(until)
        .fetch_all(&self.pool)
        .await
        .map_err(db_error)?;
        rows.iter()
            .map(|row| {
                let trade_fee_micro: i64 = row.try_get("trade_fee_micro").map_err(db_error)?;
                let payout_dust_micro: i64 = row.try_get("payout_dust_micro").map_err(db_error)?;
                Ok(DailyFeeRow {
                    day: row.try_get("day").map_err(db_error)?,
                    trade_fee_micro,
                    payout_dust_micro,
                    total_micro: trade_fee_micro
                        .checked_add(payout_dust_micro)
                        .ok_or(StoreError::Invariant("fee summary overflow"))?,
                })
            })
            .collect()
    }
}

fn position_view(row: &PgRow) -> Result<PositionView, StoreError> {
    Ok(PositionView {
        market: MarketId(row.try_get("market_id").map_err(db_error)?),
        outcome: application::model::OutcomeId(row.try_get("outcome_id").map_err(db_error)?),
        side: side_from_idx(row.try_get("idx").map_err(db_error)?)?,
        shares: MicroShares(row.try_get("shares_micro").map_err(db_error)?),
        cost: MicroUsd(row.try_get("cost_micro").map_err(db_error)?),
        realized_pnl: MicroUsd(row.try_get("realized_pnl_micro").map_err(db_error)?),
        rep_micro: row.try_get("rep_micro").map_err(db_error)?,
        tier: u8::try_from(row.try_get::<i32, _>("tier").map_err(db_error)?)
            .map_err(|_| StoreError::Invariant("invalid reputation tier"))?,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use application::model::JobId;
    use application::ports::Store;
    use sqlx::postgres::PgPoolOptions;

    use super::PgStore;

    #[tokio::test]
    async fn phase5_factories_open_real_transactions_that_commit() {
        let url = std::env::var("DATABASE_URL").unwrap();
        let pool = PgPoolOptions::new().connect(&url).await.unwrap();
        let store = PgStore::from_pool(pool);

        let mut content = store.content_tx().await.unwrap();
        content.pending_draft_count().await.unwrap();
        content.commit().await.unwrap();

        let mut video = store.video_tx().await.unwrap();
        let missed_fence = video
            .complete_ready(
                JobId(uuid::Uuid::new_v4()),
                uuid::Uuid::new_v4(),
                "unused://asset",
                time::OffsetDateTime::UNIX_EPOCH,
            )
            .await
            .unwrap();
        assert!(
            !missed_fence,
            "CAS on an unknown job must not report success"
        );
        video.commit().await.unwrap();

        assert!(store.withdraw_tx().await.is_ok());
        assert!(store.deposit_admission_tx().await.is_ok());
        assert!(store.credit_convert_tx().await.is_ok());
    }
}
