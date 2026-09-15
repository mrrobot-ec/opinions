// ---------------------------------------------------------------------------
// Markets
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MarketSummaryDto {
    pub id: Uuid,
    pub slug: String,
    pub question: String,
    pub state: MarketStateDto,
    pub yes_outcome_id: Uuid,
    pub no_outcome_id: Uuid,
    pub price_yes_micro: i64,
    pub price_no_micro: i64,
    pub tally_yes: Option<i64>,
    pub tally_no: Option<i64>,
    pub closes_at: time::OffsetDateTime,
    pub tally_hidden_at: time::OffsetDateTime,
    pub server_now: Option<time::OffsetDateTime>,
    pub under_review: bool,
    pub poster_asset_url: Option<String>,
    pub video_asset_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PricePointDto {
    pub bucket_start: time::OffsetDateTime,
    pub avg_price_micro: i64,
    pub volume_micro: i64,
    pub trades: u32,
}

impl From<application::model::PricePoint> for PricePointDto {
    fn from(point: application::model::PricePoint) -> Self {
        Self {
            bucket_start: point.bucket_start,
            avg_price_micro: point.avg_price_micro,
            volume_micro: point.volume_micro,
            trades: point.trades,
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TapeRowDto {
    pub handle: String,
    pub side: SideDto,
    pub action: TradeActionDto,
    pub collateral_micro: i64,
    pub created_at: time::OffsetDateTime,
    pub trade_seq: i64,
}

impl From<application::model::TapeRow> for TapeRowDto {
    fn from(row: application::model::TapeRow) -> Self {
        Self {
            handle: row.handle,
            side: row.side.into(),
            action: row.action.into(),
            collateral_micro: row.collateral_micro,
            created_at: row.created_at,
            trade_seq: row.trade_seq,
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema, PartialEq, Eq)]
pub struct TraderRowDto {
    pub handle: String,
    pub realized_pnl_micro: i64,
    pub realizations: u32,
}

impl From<application::model::TraderRow> for TraderRowDto {
    fn from(row: application::model::TraderRow) -> Self {
        Self {
            handle: row.handle,
            realized_pnl_micro: row.realized_pnl_micro,
            realizations: row.realizations,
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema, PartialEq, Eq)]
pub struct VoterRowDto {
    pub handle: String,
    pub avg_score_bp: i64,
    pub markets_scored: u32,
    pub tier: u8,
}

impl From<application::model::VoterRow> for VoterRowDto {
    fn from(row: application::model::VoterRow) -> Self {
        Self {
            handle: row.handle,
            avg_score_bp: row.avg_score_bp,
            markets_scored: row.markets_scored,
            tier: row.tier,
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema, PartialEq, Eq)]
pub struct DailyFeeRowDto {
    pub day: String,
    pub trade_fee_micro: i64,
    pub payout_dust_micro: i64,
    pub total_micro: i64,
}

impl From<application::model::DailyFeeRow> for DailyFeeRowDto {
    fn from(row: application::model::DailyFeeRow) -> Self {
        Self {
            day: row.day.to_string(),
            trade_fee_micro: row.trade_fee_micro,
            payout_dust_micro: row.payout_dust_micro,
            total_micro: row.total_micro,
        }
    }
}

impl MarketSummaryDto {
    #[must_use]
    pub fn from_row(row: &MarketRow, pool: &domain::amm::Pool) -> Self {
        Self {
            id: row.id.0,
            slug: row.slug.clone(),
            question: row.question.clone(),
            state: row.state.into(),
            yes_outcome_id: row.yes_outcome.0,
            no_outcome_id: row.no_outcome.0,
            price_yes_micro: domain::amm::price_micro(pool, domain::amm::Side::Yes),
            price_no_micro: domain::amm::price_micro(pool, domain::amm::Side::No),
            tally_yes: None,
            tally_no: None,
            closes_at: row.closes_at,
            tally_hidden_at: row.tally_hidden_at,
            server_now: None,
            under_review: row.state == domain::market::MarketState::Resolving,
            poster_asset_url: row.poster_asset_url.clone(),
            video_asset_url: row.video_asset_url.clone(),
        }
    }

    #[must_use]
    pub fn from_snapshot(
        row: &MarketRow,
        snapshot: &MarketSnapshot,
        now: time::OffsetDateTime,
    ) -> Self {
        Self {
            id: row.id.0,
            slug: row.slug.clone(),
            question: row.question.clone(),
            state: snapshot.state.into(),
            yes_outcome_id: row.yes_outcome.0,
            no_outcome_id: row.no_outcome.0,
            price_yes_micro: snapshot.price_yes_micro,
            price_no_micro: snapshot.price_no_micro,
            tally_yes: snapshot.tally.map(|tally| tally.yes_votes),
            tally_no: snapshot.tally.map(|tally| tally.no_votes),
            closes_at: snapshot.closes_at,
            tally_hidden_at: snapshot.tally_hidden_at,
            server_now: Some(now),
            under_review: snapshot.under_review,
            poster_asset_url: snapshot.poster_asset_url.clone(),
            video_asset_url: snapshot.video_asset_url.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Trades
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PreviewTradeRequest {
    pub user_id: Uuid,
    pub market_ref: String,
    pub side: SideDto,
    pub action: TradeActionDto,
    pub amount_micro: i64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TradePreviewDto {
    pub market_id: Uuid,
    pub side: SideDto,
    pub action: TradeActionDto,
    pub shares_micro: i64,
    pub gross_micro: i64,
    pub fee_micro: i64,
    pub avg_price_micro: i64,
    /// Config generation this preview quoted against (D25a). Echo it back
    /// as `expected_config_version` when placing the trade.
    pub config_version: i64,
}

impl From<TradePreview> for TradePreviewDto {
    fn from(p: TradePreview) -> Self {
        Self {
            market_id: p.market.0,
            side: p.side.into(),
            action: p.action.into(),
            shares_micro: p.shares.0,
            gross_micro: p.gross.0,
            fee_micro: p.fee.0,
            avg_price_micro: p.avg_price_micro,
            config_version: p.config_version,
        }
    }
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PlaceTradeRequest {
    pub user_id: Uuid,
    pub market_ref: String,
    pub side: SideDto,
    pub action: TradeActionDto,
    pub amount_micro: i64,
    pub idempotency_key: String,
    #[serde(default)]
    pub run_id: Option<Uuid>,
    #[serde(default)]
    pub pending_action_id: Option<Uuid>,
    /// The preview's `config_version` echo (D36: required).
    pub expected_config_version: i64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TradeReceiptDto {
    pub trade_id: Uuid,
    pub ledger_txn: Uuid,
    pub side: SideDto,
    pub action: TradeActionDto,
    pub shares_micro: i64,
    pub gross_micro: i64,
    pub fee_micro: i64,
    pub avg_price_micro: i64,
    pub replayed: bool,
}

impl From<TradeReceipt> for TradeReceiptDto {
    fn from(r: TradeReceipt) -> Self {
        Self {
            trade_id: r.trade_id.0,
            ledger_txn: r.ledger_txn,
            side: r.side.into(),
            action: r.action.into(),
            shares_micro: r.shares.0,
            gross_micro: r.gross.0,
            fee_micro: r.fee.0,
            avg_price_micro: r.avg_price_micro,
            replayed: r.replayed,
        }
    }
}

// ---------------------------------------------------------------------------
// Votes (wire shape; use case lands with Task 1.2)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CastVoteRequest {
    pub user_id: Uuid,
    pub market_ref: String,
    pub side: SideDto,
    pub crowd_guess_pct: u8,
    pub idempotency_key: String,
    #[serde(default)]
    pub run_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct VoteReceiptDto {
    pub vote_id: Uuid,
    pub market_id: Uuid,
    pub seq: Option<i64>,
    pub side: SideDto,
    pub crowd_guess_pct: u8,
    pub replayed: bool,
}

impl From<application::model::VoteReceipt> for VoteReceiptDto {
    fn from(r: application::model::VoteReceipt) -> Self {
        Self {
            vote_id: r.vote_id.0,
            market_id: r.market.0,
            seq: r.seq,
            side: r.side.into(),
            crowd_guess_pct: r.crowd_guess_pct,
            replayed: r.replayed,
        }
    }
}

// ---------------------------------------------------------------------------
// Positions / identity
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PositionDto {
    pub market_id: Uuid,
    pub outcome_id: Uuid,
    pub side: SideDto,
    pub shares_micro: i64,
    pub cost_micro: i64,
    pub realized_pnl_micro: i64,
    pub rep_micro: i64,
    pub tier: u8,
}

impl From<PositionView> for PositionDto {
    fn from(p: PositionView) -> Self {
        Self {
            market_id: p.market.0,
            outcome_id: p.outcome.0,
            side: p.side.into(),
            shares_micro: p.shares.0,
            cost_micro: p.cost.0,
            realized_pnl_micro: p.realized_pnl.0,
            rep_micro: p.rep_micro,
            tier: p.tier,
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct UserIdDto {
    pub user_id: Uuid,
}
