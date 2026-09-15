//! Plain data crossing ports (no SQL- or HTTP-framework types anywhere in
//! this crate; `just deps-check` enforces it).

use domain::amm::Side;
use domain::money::{MicroShares, MicroUsd};
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MarketId(pub uuid::Uuid);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UserId(pub uuid::Uuid);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutcomeId(pub uuid::Uuid);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TradeId(pub uuid::Uuid);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CommentId(pub uuid::Uuid);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DraftId(pub uuid::Uuid);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(pub uuid::Uuid);

/// Stable names written to the transactional outbox.
pub mod event_type {
    pub const DRAFT_CREATED: &str = "DraftCreated";
    pub const DRAFT_APPROVED: &str = "DraftApproved";
    pub const DRAFT_PUBLISHED: &str = "DraftPublished";
    pub const SLOT_UNFILLED: &str = "SlotUnfilled";
    pub const VIDEO_ATTACHED: &str = "VideoAttached";
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketRow {
    pub id: MarketId,
    pub slug: String,
    pub question: String,
    pub state: domain::market::MarketState,
    pub min_votes_to_resolve: i32,
    pub opens_at: OffsetDateTime,
    pub closes_at: OffsetDateTime,
    pub tally_hidden_at: OffsetDateTime,
    pub yes_outcome: OutcomeId,
    pub no_outcome: OutcomeId,
    pub curator_flagged_at: Option<OffsetDateTime>,
    pub integrity_due_at: Option<OffsetDateTime>,
    pub poster_asset_url: Option<String>,
    pub video_asset_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolRow {
    pub market: MarketId,
    pub pool: domain::amm::Pool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeAction {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TradeReceipt {
    pub trade_id: TradeId,
    pub ledger_txn: uuid::Uuid,
    pub side: Side,
    pub action: TradeAction,
    pub shares: MicroShares,
    pub gross: MicroUsd,
    pub fee: MicroUsd,
    pub avg_price_micro: i64,
    pub replayed: bool,
}

/// Insert payload for [`crate::ports::TradeWriter::insert_trade`].
/// `run_id`/`pending_action_id` carry the agent causal chain when present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTrade {
    pub market: MarketId,
    pub user: UserId,
    pub outcome: OutcomeId,
    pub side: Side,
    pub action: TradeAction,
    pub shares: MicroShares,
    pub gross: MicroUsd,
    pub fee: MicroUsd,
    pub avg_price_micro: i64,
    pub ledger_txn: uuid::Uuid,
    pub run_id: Option<uuid::Uuid>,
    pub pending_action_id: Option<uuid::Uuid>,
}

/// Values assigned by the store when a trade row is inserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InsertedTrade {
    pub id: TradeId,
    pub trade_seq: i64,
    pub created_at: OffsetDateTime,
}

/// Transactional-outbox row: events commit with state or not at all.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub event_type: &'static str,
    pub aggregate_type: &'static str,
    pub aggregate_id: uuid::Uuid,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModerationStatus {
    Visible,
    Shadow,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentRow {
    pub id: CommentId,
    pub market: MarketId,
    pub author: UserId,
    pub parent: Option<CommentId>,
    pub body: String,
    pub body_hash: Option<String>,
    pub score: i32,
    pub moderation_status: ModerationStatus,
    pub depth: u8,
    pub reply_count: u32,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewComment {
    pub id: CommentId,
    pub market: MarketId,
    pub author: UserId,
    pub parent: Option<CommentId>,
    pub body: String,
    pub body_hash: String,
    pub moderation_status: ModerationStatus,
    pub depth: u8,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentView {
    pub row: CommentRow,
    pub author_handle: String,
    pub hot_score: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentSort {
    Hot,
    Recent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentCursor {
    Hot {
        as_of: OffsetDateTime,
        hot_score: i64,
        created_at: OffsetDateTime,
        id: CommentId,
    },
    Recent {
        created_at: OffsetDateTime,
        id: CommentId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentPage {
    pub comments: Vec<CommentView>,
    pub next: Option<CommentCursor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommentReceipt {
    pub comment: CommentId,
    pub moderation_status: ModerationStatus,
    pub replayed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommentVoteReceipt {
    pub comment: CommentId,
    pub score: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommentReportReceipt {
    pub comment: CommentId,
    pub report_count: u32,
    pub shadowed: bool,
    pub replayed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutboxEvent {
    pub seq: i64,
    pub event_type: String,
    pub aggregate_type: String,
    pub aggregate_id: uuid::Uuid,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolutionRecipient {
    pub user: UserId,
    pub held: bool,
    pub payout_total: MicroUsd,
    pub realized_delta: MicroUsd,
    pub voted: bool,
    pub score_bp: Option<u16>,
    pub side: Option<domain::amm::Side>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewNotification {
    pub user: UserId,
    pub notification_type: String,
    pub market: Option<MarketId>,
    pub payload: serde_json::Value,
    pub source_seq: i64,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NotificationRow {
    pub id: i64,
    pub user: UserId,
    pub notification_type: String,
    pub market: Option<MarketId>,
    pub payload: serde_json::Value,
    pub source_seq: Option<i64>,
    pub read_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HolderRow {
    pub user: UserId,
    pub handle: String,
    pub tier: u8,
    pub cost: MicroUsd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketHolders {
    pub yes: Vec<HolderRow>,
    pub no: Vec<HolderRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileTradeRow {
    pub market: MarketId,
    pub market_ref: String,
    pub side: Side,
    pub action: TradeAction,
    pub collateral_micro: i64,
    pub created_at: OffsetDateTime,
    pub trade_seq: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileVoteRow {
    pub market: MarketId,
    pub market_question: String,
    pub cast_at: OffsetDateTime,
    pub side: Option<Side>,
    pub score_bp: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserProfile {
    pub user: UserId,
    pub handle: String,
    pub created_at: OffsetDateTime,
    pub rep_micro: i64,
    pub tier: u8,
    pub avg_score_bp: Option<i64>,
    pub markets_scored: u32,
    pub realized_pnl: MicroUsd,
    pub recent_trades: Vec<ProfileTradeRow>,
    pub recent_votes: Vec<ProfileVoteRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportedCommentRow {
    pub comment: CommentView,
    pub report_count: u32,
    pub reporters: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid social config: {0}")]
pub struct SocialConfigError(pub &'static str);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocialConfig {
    pub max_comment_len_chars: u32,
    pub max_links: u8,
    pub max_mentions: u8,
    pub spam_window_secs: u64,
    pub max_comments_per_window: u32,
    pub report_shadow_threshold: u32,
    pub reporter_min_age_secs: u64,
    pub reporter_min_tier: u8,
    pub max_reports_per_window: u32,
    pub report_window_secs: u64,
    pub mention_notifs_per_hour: u32,
    pub max_thread_depth: u8,
}

impl SocialConfig {
    /// Validates every Phase 4 social policy at startup.
    ///
    /// # Errors
    /// Returns the first invalid invariant.
    pub const fn validate(self) -> Result<Self, SocialConfigError> {
        if self.max_comment_len_chars == 0 || self.max_comment_len_chars > 4_000 {
            return Err(SocialConfigError("comment length must be in 1..=4000"));
        }
        if self.max_mentions > 5 {
            return Err(SocialConfigError("maximum mentions must not exceed five"));
        }
        if self.spam_window_secs == 0 || self.report_window_secs == 0 {
            return Err(SocialConfigError(
                "spam and report windows must be positive",
            ));
        }
        if self.max_comments_per_window == 0
            || self.max_reports_per_window == 0
            || self.mention_notifs_per_hour == 0
        {
            return Err(SocialConfigError("velocity limits must be positive"));
        }
        if self.report_shadow_threshold < 2 {
            return Err(SocialConfigError(
                "report shadow threshold must be at least two",
            ));
        }
        if self.reporter_min_tier > 4 {
            return Err(SocialConfigError("reporter tier must be in 0..=4"));
        }
        if self.max_thread_depth == 0 {
            return Err(SocialConfigError("thread depth must be positive"));
        }
        Ok(self)
    }
}

impl Default for SocialConfig {
    fn default() -> Self {
        Self {
            max_comment_len_chars: 2_000,
            max_links: 2,
            max_mentions: 5,
            spam_window_secs: 3_600,
            max_comments_per_window: 20,
            report_shadow_threshold: 3,
            reporter_min_age_secs: 72 * 3_600,
            reporter_min_tier: 1,
            max_reports_per_window: 20,
            report_window_secs: 3_600,
            mention_notifs_per_hour: 20,
            max_thread_depth: 8,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tally {
    pub yes_votes: i64,
    pub no_votes: i64,
}

/// One clock-consistent public market projection used by REST and WS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketSnapshot {
    pub market: MarketId,
    pub state: domain::market::MarketState,
    pub price_yes_micro: i64,
    pub price_no_micro: i64,
    pub tally: Option<Tally>,
    pub closes_at: OffsetDateTime,
    pub tally_hidden_at: OffsetDateTime,
    pub under_review: bool,
    pub poster_asset_url: Option<String>,
    pub video_asset_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftStatus {
    Pending,
    Approved,
    Rejected,
    Published,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishStage {
    Claimed,
    Seeded,
    Live,
    JobsEnqueued,
    Published,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftRow {
    pub id: DraftId,
    pub spec: domain::drafting::DraftSpec,
    pub source: domain::drafting::DraftSource,
    pub fallback_from: Option<domain::drafting::DraftSource>,
    pub status: DraftStatus,
    pub publish_stage: Option<PublishStage>,
    pub published_market: Option<MarketId>,
    pub publish_at: Option<OffsetDateTime>,
    pub expires_at: OffsetDateTime,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    MarketVideo,
    Poster,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Queued,
    Rendering,
    Ready,
    Attached,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoJobRow {
    pub id: JobId,
    pub market: MarketId,
    pub draft: Option<DraftId>,
    pub kind: ArtifactKind,
    pub status: JobStatus,
    pub asset_url: Option<String>,
    pub available_at: OffsetDateTime,
    pub claim_token: Option<uuid::Uuid>,
    pub lease_expires_at: Option<OffsetDateTime>,
    pub attempts: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModerationJobStatus {
    Queued,
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModerationJobRow {
    pub id: JobId,
    pub comment: CommentId,
    pub status: ModerationJobStatus,
    pub available_at: OffsetDateTime,
    pub claim_token: Option<uuid::Uuid>,
    pub lease_expires_at: Option<OffsetDateTime>,
    pub attempts: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModerationVerdict {
    Visible,
    Shadow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftRequest {
    pub topic: String,
    pub tier: domain::drafting::DraftTier,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedArtifact {
    pub bytes: Vec<u8>,
    pub media_type: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentTierConfig {
    pub open_secs: u64,
    pub hidden_window_secs: u64,
    pub seed_micro: i64,
    pub fee_bps: u16,
    pub min_votes_to_resolve: i32,
    pub seed_floor_micro: i64,
    pub min_votes_floor: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentConfig {
    pub flash_cadence_secs: u64,
    pub daily_slots: u16,
    pub draft_ttl_secs: u64,
    pub max_pending_drafts: u32,
    pub max_slot_horizon_secs: u64,
    pub daily_seed_budget_micro: i64,
    pub video_max_attempts: u32,
    pub lease_secs: u64,
    pub backoff_base_secs: u64,
    pub render_dir: std::path::PathBuf,
    pub tier_defaults: [ContentTierConfig; 2],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid content config")]
pub struct ContentConfigError;

impl ContentConfig {
    /// Validates every phase-5 scalar and tier cross-field invariant.
    ///
    /// # Errors
    /// Returns [`ContentConfigError`] when any scalar or tier rule is invalid.
    pub fn validate(self) -> Result<Self, ContentConfigError> {
        let tiers_valid = self.tier_defaults.iter().all(|tier| {
            tier.open_secs > 0
                && tier.hidden_window_secs < tier.open_secs
                && tier.seed_floor_micro > 0
                && tier.seed_micro >= tier.seed_floor_micro
                && tier.min_votes_floor > 0
                && tier.min_votes_to_resolve >= tier.min_votes_floor
                && tier.fee_bps <= 10_000
        });
        if self.flash_cadence_secs == 0
            || self.daily_slots == 0
            || self.draft_ttl_secs == 0
            || self.max_pending_drafts == 0
            || self.max_slot_horizon_secs == 0
            || self.daily_seed_budget_micro <= 0
            || self.video_max_attempts == 0
            || self.lease_secs == 0
            || self.backoff_base_secs == 0
            || self.render_dir.as_os_str().is_empty()
            || !tiers_valid
        {
            return Err(ContentConfigError);
        }
        Ok(self)
    }
}

impl Default for ContentConfig {
    fn default() -> Self {
        let daily = ContentTierConfig {
            open_secs: 86_400,
            hidden_window_secs: 3_600,
            seed_micro: 100_000_000,
            fee_bps: 100,
            min_votes_to_resolve: 10,
            seed_floor_micro: 1_000_000,
            min_votes_floor: 3,
        };
        let flash = ContentTierConfig {
            open_secs: 3_600,
            hidden_window_secs: 300,
            seed_micro: 10_000_000,
            fee_bps: 100,
            min_votes_to_resolve: 3,
            seed_floor_micro: 1_000_000,
            min_votes_floor: 3,
        };
        Self {
            flash_cadence_secs: 3_600,
            daily_slots: 2,
            draft_ttl_secs: 604_800,
            max_pending_drafts: 100,
            max_slot_horizon_secs: 2_592_000,
            daily_seed_budget_micro: 1_000_000_000,
            video_max_attempts: 3,
            lease_secs: 60,
            backoff_base_secs: 5,
            render_dir: std::path::PathBuf::from("var/content"),
            tier_defaults: [daily, flash],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DueMarket {
    pub market: MarketId,
    pub state: domain::market::MarketState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LifecycleCommand {
    pub market: MarketId,
    pub event: domain::market::MarketEvent,
    pub resulting_state: domain::market::MarketState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldingOwner {
    User(UserId),
    Pool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Holding {
    pub account: domain::ledger::AccountId,
    pub owner: HoldingOwner,
    pub outcome: OutcomeId,
    pub side: Side,
    pub shares: MicroShares,
}

/// Immutable profit/loss fact emitted in the same transaction as the money
/// movement which realizes it. Leaderboards consume only these facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RealizationFact {
    pub user: UserId,
    pub market: MarketId,
    pub outcome: OutcomeId,
    pub source: RealizationSource,
    pub realized_delta: MicroUsd,
    pub payout: MicroUsd,
    pub ledger_txn: uuid::Uuid,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RealizationSource {
    Sell,
    Settlement,
    Void,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraderRow {
    pub handle: String,
    pub realized_pnl_micro: i64,
    pub realizations: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoterRow {
    pub handle: String,
    pub avg_score_bp: i64,
    pub markets_scored: u32,
    pub tier: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DailyFeeRow {
    pub day: time::Date,
    pub trade_fee_micro: i64,
    pub payout_dust_micro: i64,
    pub total_micro: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PricePoint {
    pub bucket_start: OffsetDateTime,
    pub avg_price_micro: i64,
    pub volume_micro: i64,
    pub trades: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapeRow {
    pub handle: String,
    pub side: Side,
    pub action: TradeAction,
    pub collateral_micro: i64,
    pub created_at: OffsetDateTime,
    pub trade_seq: i64,
}

impl Tally {
    #[must_use]
    pub fn total(&self) -> i64 {
        self.yes_votes + self.no_votes
    }

    /// bps of YES among cast votes; caller handles `total() == 0` via the
    /// D21 void/curator branch. Also `None` for out-of-range tallies
    /// (negative counts), which a store must never produce.
    #[must_use]
    pub fn actual_yes_bps(&self) -> Option<u16> {
        let total = self.total();
        if total == 0 {
            return None;
        }
        let bps = i128::from(self.yes_votes) * 10_000 / i128::from(total);
        u16::try_from(bps).ok()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionView {
    pub market: MarketId,
    pub outcome: OutcomeId,
    pub side: Side,
    pub shares: MicroShares,
    pub cost: MicroUsd,
    pub realized_pnl: MicroUsd,
    pub rep_micro: i64,
    pub tier: u8,
}

/// Mutable position state, updated exclusively via the M5 formula
/// (see [`crate::ports::PositionWriter`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionRow {
    pub user: UserId,
    pub outcome: OutcomeId,
    pub shares: MicroShares,
    pub cost: MicroUsd,
    pub realized_pnl: MicroUsd,
}

/// Ledger-account owner, resolved by `LedgerWriter::account` to a concrete
/// [`domain::ledger::AccountId`] (get-or-create, unique per owner+currency).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OwnerRef {
    User(UserId),
    MarketEscrow(MarketId),
    MarketPool(MarketId),
    Fees,
    House,
    External,
    Withheld,
    DepositSuspense,
    BonusReserve,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VoteId(pub uuid::Uuid);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DepositId(pub uuid::Uuid);

/// Insert payload for [`crate::ports::VoteWriter::insert_vote`]. The row
/// carries its idempotency key so `vote_by_key` can replay it (votes have no
/// ledger transaction to key off).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewVote {
    pub market: MarketId,
    pub user: UserId,
    pub side: Side,
    pub crowd_guess_pct: u8,
    pub seq: i64,
    pub idempotency_key: String,
    pub created_at: OffsetDateTime,
    pub cast_ip: Option<std::net::IpAddr>,
    pub device_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoteReceipt {
    pub vote_id: VoteId,
    pub market: MarketId,
    pub user: UserId,
    pub side: Side,
    pub crowd_guess_pct: u8,
    pub seq: Option<i64>,
    pub replayed: bool,
}

/// Insert payload for [`crate::ports::DepositWriter::insert_deposit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDeposit {
    pub user: UserId,
    pub amount: MicroUsd,
    pub chain_sig: String,
    pub ledger_txn: uuid::Uuid,
}

/// Insert payload for [`crate::ports::MarketWriter::insert_market`]. The id
/// is caller-generated (client-generated idempotency: a replayed `SeedMarket`
/// can echo the market it created without needing a reader role).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMarket {
    pub id: MarketId,
    pub slug: String,
    pub min_votes_to_resolve: i32,
    pub closes_at: OffsetDateTime,
    pub tally_hidden_at: OffsetDateTime,
}

/// Immutable vote fact for settlement-time scoring: scores are computed IN
/// THE USE CASE via `domain::scoring`, persisted one by one (codex M4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoteFact {
    pub vote_id: uuid::Uuid,
    pub user: UserId,
    pub side: Side,
    pub crowd_guess_pct: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReputationRow {
    pub user: UserId,
    pub rep_micro: i64,
    pub tier: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoteScoreUpdate {
    pub vote_id: uuid::Uuid,
    pub score: domain::scoring::VoteScore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid reputation config: {0}")]
pub struct RepConfigError(pub &'static str);

/// Validated economy policy. Arrays are indexed by tier 0..=4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepConfig {
    pub half_life: domain::reputation::HalfLife,
    pub tier_thresholds_micro: [i64; 4],
    pub position_cap_micro_by_tier: [i64; 5],
    pub fee_discount_bp_by_tier: [u16; 5],
    pub min_fee_bps: u16,
    pub rep_score_min_pot_micro: i64,
    pub discount_flip_window_secs: u64,
    pub leaderboard_min_scored: u32,
}

impl RepConfig {
    /// Validates every local and cross-field invariant before the config can
    /// enter a use case.
    ///
    /// # Errors
    /// Returns [`RepConfigError`] naming the first invalid invariant.
    pub fn validate(self) -> Result<Self, RepConfigError> {
        if self
            .tier_thresholds_micro
            .iter()
            .any(|value| !(0..=1_000_000).contains(value))
            || self
                .tier_thresholds_micro
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
        {
            return Err(RepConfigError(
                "tier thresholds must be strictly ascending in 0..=1,000,000",
            ));
        }
        if self.position_cap_micro_by_tier.iter().any(|cap| *cap < 0)
            || self
                .position_cap_micro_by_tier
                .windows(2)
                .any(|pair| pair[0] > pair[1])
        {
            return Err(RepConfigError(
                "position caps must be nonnegative and nondecreasing",
            ));
        }
        if self
            .fee_discount_bp_by_tier
            .iter()
            .any(|discount| *discount > 10_000)
            || self.min_fee_bps > 10_000
        {
            return Err(RepConfigError(
                "fees and discounts must not exceed 10,000 bps",
            ));
        }
        if self.rep_score_min_pot_micro < 0 {
            return Err(RepConfigError(
                "reputation quality-floor pot must be nonnegative",
            ));
        }
        if self.leaderboard_min_scored == 0 {
            return Err(RepConfigError("leaderboard minimum must be at least one"));
        }
        Ok(self)
    }
}

impl Default for RepConfig {
    fn default() -> Self {
        Self {
            half_life: domain::reputation::HalfLife::H20,
            tier_thresholds_micro: [200_000, 400_000, 600_000, 800_000],
            position_cap_micro_by_tier: [i64::MAX; 5],
            fee_discount_bp_by_tier: [0; 5],
            min_fee_bps: 0,
            rep_score_min_pot_micro: 0,
            discount_flip_window_secs: 0,
            leaderboard_min_scored: 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid LP kill config: {0}")]
pub struct LpKillConfigError(pub &'static str);

/// Seed-time LP loss breaker. Already-live markets are deliberately not
/// affected; continuous exposure control belongs to the Phase 6 control plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LpKillConfig {
    pub max_loss_micro: i64,
    pub window_days: u32,
}

impl LpKillConfig {
    /// # Errors
    /// Both values must be strictly positive.
    pub const fn validate(self) -> Result<Self, LpKillConfigError> {
        if self.max_loss_micro <= 0 {
            return Err(LpKillConfigError("maximum loss must be positive"));
        }
        if self.window_days == 0 {
            return Err(LpKillConfigError("window days must be positive"));
        }
        Ok(self)
    }
}

impl Default for LpKillConfig {
    fn default() -> Self {
        Self {
            max_loss_micro: i64::MAX,
            window_days: 30,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid integrity sweep config: {0}")]
pub struct IntegritySweepConfigError(pub &'static str);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntegritySweepConfig {
    pub payout_hold_threshold_micro: i64,
    pub sweep_delay_secs: u64,
    pub burst_window_secs: u64,
    pub prior_horizon_windows: u32,
    pub burst_multiplier_ppm: u32,
    pub young_account_age_secs: u64,
    pub young_account_share_max_ppm: u32,
    pub subnet_share_max_ppm: u32,
    pub device_share_max_ppm: u32,
    pub min_votes_for_ratios: u32,
    pub min_metadata_coverage_ppm: u32,
}

impl IntegritySweepConfig {
    /// # Errors
    /// Returns [`IntegritySweepConfigError`] for the first invalid field.
    pub fn validate(self) -> Result<Self, IntegritySweepConfigError> {
        if self.payout_hold_threshold_micro <= 0 {
            return Err(IntegritySweepConfigError("hold threshold must be positive"));
        }
        if self.sweep_delay_secs == 0
            || self.burst_window_secs == 0
            || self.prior_horizon_windows == 0
        {
            return Err(IntegritySweepConfigError(
                "delay, burst window, and prior horizon must be positive",
            ));
        }
        if self.burst_multiplier_ppm > 100_000_000 {
            return Err(IntegritySweepConfigError(
                "burst multiplier must not exceed 100,000,000 ppm",
            ));
        }
        if [
            self.young_account_share_max_ppm,
            self.subnet_share_max_ppm,
            self.device_share_max_ppm,
            self.min_metadata_coverage_ppm,
        ]
        .iter()
        .any(|value| *value > 1_000_000)
        {
            return Err(IntegritySweepConfigError(
                "share and coverage fields must not exceed 1,000,000 ppm",
            ));
        }
        if self.min_votes_for_ratios == 0 {
            return Err(IntegritySweepConfigError(
                "minimum votes for ratios must be positive",
            ));
        }
        Ok(self)
    }

    #[must_use]
    pub const fn thresholds(self) -> domain::integrity::SweepThresholds {
        domain::integrity::SweepThresholds {
            burst_multiplier_ppm: self.burst_multiplier_ppm,
            young_account_share_max_ppm: self.young_account_share_max_ppm,
            subnet_share_max_ppm: self.subnet_share_max_ppm,
            device_share_max_ppm: self.device_share_max_ppm,
            min_votes_for_ratios: self.min_votes_for_ratios,
            min_metadata_coverage_ppm: self.min_metadata_coverage_ppm,
        }
    }
}

impl Default for IntegritySweepConfig {
    fn default() -> Self {
        Self {
            payout_hold_threshold_micro: i64::MAX,
            sweep_delay_secs: 180,
            burst_window_secs: 60,
            prior_horizon_windows: 4,
            burst_multiplier_ppm: 2_000_000,
            young_account_age_secs: 72 * 3_600,
            young_account_share_max_ppm: 500_000,
            subnet_share_max_ppm: 600_000,
            device_share_max_ppm: 600_000,
            min_votes_for_ratios: 4,
            min_metadata_coverage_ppm: 500_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct IntegrityReportRow {
    pub market: MarketId,
    pub checks: serde_json::Value,
    pub verdict: domain::integrity::Verdict,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FlaggedMarketRow {
    pub market: MarketRow,
    pub report: Option<IntegrityReportRow>,
}

/// Typed config injected into `ResolveMarket` (codex M4): the D21 auto-void
/// branch fires only when open interest is below this floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolveConfig {
    pub oi_floor: MicroUsd,
}

// ---------------------------------------------------------------------------
// Phase 6 ops control plane (D24/D26/D27/D30) — plain data crossing the ops
// ports. Real use cases land with waves W1/W2; Task 6.0a owns these shapes.
// ---------------------------------------------------------------------------

/// Admin role vocabulary (D26). Roles are capability SETS, no hierarchy —
/// the adapter's matrix decides what each role may touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AdminRole {
    Curator,
    Ops,
    Finance,
    Superadmin,
}

impl AdminRole {
    /// Stable lowercase name persisted in audit rows and proposal rows.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Curator => "curator",
            Self::Ops => "ops",
            Self::Finance => "finance",
            Self::Superadmin => "superadmin",
        }
    }
}

/// Typed actor threaded through shared use cases (D26): machine paths
/// (scheduler, publisher) stay `Machine` and never write audit rows; every
/// admin-authenticated mutation carries the authenticated principal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminContext {
    Machine,
    Admin {
        /// SHA-256 digest of the presented token — never the token itself.
        token_digest: String,
        role: AdminRole,
    },
}

impl AdminContext {
    #[must_use]
    pub const fn is_admin(&self) -> bool {
        matches!(self, Self::Admin { .. })
    }
}

/// One audit row (D26): every admin mutation inserts exactly one of these in
/// the SAME transaction as its effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminAction {
    pub actor_role: AdminRole,
    /// SHA-256 digest of the acting token — redacted vocabulary only.
    pub actor_token_digest: String,
    /// Stable machine-readable action name, e.g. `resolve_market`.
    pub action: String,
    /// Subject reference, e.g. `market:<uuid>` or `config:<key>`.
    pub subject: String,
    pub before: Option<serde_json::Value>,
    pub after: Option<serde_json::Value>,
    pub reason: Option<String>,
}

/// One committed config entry (D24 read path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigEntry {
    pub key: String,
    pub value: serde_json::Value,
}

/// One per-key change row inside a generation (D24: a multi-key patch shares
/// one generation; `old` is retained so emergency revert pre-fills from
/// history).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigChange {
    pub key: String,
    pub old: Option<serde_json::Value>,
    pub new: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposalStatus {
    Pending,
    Confirmed,
    Rejected,
    Expired,
}

/// Durable two-phase proposal row for sensitive config keys (D24).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigProposal {
    pub id: uuid::Uuid,
    pub idempotency_key: String,
    pub patch: serde_json::Value,
    pub patch_hash: String,
    pub base_generation: i64,
    pub proposer_token_id: String,
    pub proposer_role: AdminRole,
    pub reason: String,
    pub status: ProposalStatus,
    pub expires_at: OffsetDateTime,
    pub confirmer_token_id: Option<String>,
    pub resulting_generation: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnwindStage {
    Proposed,
    Confirmed,
    Applied,
    Rejected,
    Expired,
}

/// Unwind authority row (D30): dual-controlled, ≥T delay, one per market.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketUnwind {
    pub market: MarketId,
    pub unwind_key: String,
    pub stage: UnwindStage,
    pub proposer_token_id: String,
    pub confirmer_token_id: Option<String>,
    pub reason: String,
    pub confirm_not_before: OffsetDateTime,
    pub reversal_txn: Option<uuid::Uuid>,
}

/// Non-cash receivable fact (D30): opened when an unwind reversal finds
/// insufficient user cash. NEVER a ledger account class — identity 3 stays
/// cash-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Receivable {
    pub id: uuid::Uuid,
    pub market: MarketId,
    pub user: UserId,
    pub origin_reversal_txn: uuid::Uuid,
    pub opened_micro: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceivableMovementKind {
    Opened,
    Collected,
    WrittenOff,
}

/// Append-only receivable movement; outstanding is DERIVED, never overwritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivableMovement {
    pub id: uuid::Uuid,
    pub receivable: uuid::Uuid,
    pub kind: ReceivableMovementKind,
    pub amount_micro: i64,
    pub actor: String,
    pub cash_txn: Option<uuid::Uuid>,
    pub idempotency_key: String,
}

/// Read-only withdrawal guard view (D30 / grok r3 NEW-5): open receivable +
/// zero cash ⇒ blocked; deposits auto-collect and clear it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WithdrawalEligibilityView {
    pub user: UserId,
    pub cash_micro: i64,
    pub open_receivables_micro: i64,
    pub eligible: bool,
}

/// Identity 1 read row: a ledger transaction whose entries do not sum to
/// zero (the set must be empty).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxnSumRow {
    pub txn: uuid::Uuid,
    pub sum_micro: i64,
}

/// Identities 2–3 read row: one account balance with its owner class and
/// currency. Currency is required because external mirroring is an identity
/// within each currency, not across the aggregate ledger. The balance is
/// deliberately wider than a ledger entry because one account may aggregate
/// multiple individually valid `i64` entries beyond the `i64` range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountBalanceRow {
    pub owner_type: domain::ledger::OwnerType,
    pub currency: domain::ledger::Currency,
    pub balance_micro: i128,
}

/// Identity 5 read row: per-market escrow history residual (must be zero)
/// against the `collateral_at_close` fact stamped by resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EscrowHistoryRow {
    pub market: MarketId,
    pub residual_micro: i64,
    pub collateral_at_close_micro: Option<i64>,
}

/// Identity 7 read row: receivable reconciliation per ORIGIN reversal
/// transaction (Σ opened = that tx's house shortfall legs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceivableReconRow {
    pub origin_reversal_txn: uuid::Uuid,
    pub opened_micro: i64,
    pub house_shortfall_micro: i64,
    pub collected_micro: i64,
    pub written_off_micro: i64,
}

/// One withdrawal's money attribution for D31 identities (a)–(e): the hold is
/// `active` while the row is unsent or sent-unsettled (its amount must be in
/// `balance(Withheld)`); terminal rows carry exactly one of release XOR settle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// The identity projection is honestly four independent booleans; packing them
// into an enum would hide exactly the illegal combinations the sweep hunts.
#[allow(clippy::struct_excessive_bools)]
pub struct WithdrawalAttributionRow {
    pub withdrawal: uuid::Uuid,
    pub amount_micro: i64,
    pub active: bool,
    pub has_hold: bool,
    pub has_release: bool,
    pub has_settle: bool,
    pub finalized_attempts: u32,
}

/// Phase 7 user status (D34). Read under `lock_user`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserStatus {
    Active,
    ShadowLimited,
    Banned,
}

/// Coarse withdrawal status (ops.md combination table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WithdrawalStatus {
    Queued,
    RiskHold,
    Sent,
    Settled,
    Denied,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WithdrawalReviewState {
    Screening,
    ReviewRequired,
    ApprovalProposed,
    Approved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WithdrawalSendState {
    Unsent,
    Sending,
    Broadcast,
    Finalized,
    DefinitiveFailed,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepositMachineStatus {
    ObservedFinalized,
    AdmissionPending,
    Admitted,
    AdmittedLegacy,
    ComplianceHold,
    RefundApproved,
    RefundSending,
    Refunded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoteIntegrityConfig {
    pub max_votes_per_window: u32,
    pub window_secs: u64,
    pub near_close_secs: u64,
    pub min_account_age_secs: u64,
}

impl Default for VoteIntegrityConfig {
    fn default() -> Self {
        Self {
            max_votes_per_window: 30,
            window_secs: 3_600,
            near_close_secs: 600,
            min_account_age_secs: 72 * 3_600,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase7_status_vocab_covers_the_machines() {
        assert_ne!(UserStatus::Active, UserStatus::ShadowLimited);
        assert_ne!(UserStatus::ShadowLimited, UserStatus::Banned);
        assert_ne!(WithdrawalStatus::Queued, WithdrawalStatus::Failed);
        assert_ne!(
            WithdrawalReviewState::Screening,
            WithdrawalReviewState::Approved
        );
        assert_ne!(
            WithdrawalSendState::Unsent,
            WithdrawalSendState::DefinitiveFailed
        );
        assert_ne!(
            DepositMachineStatus::AdmittedLegacy,
            DepositMachineStatus::Refunded
        );
    }

    #[test]
    fn admin_roles_have_stable_names_and_actor_typing_is_explicit() {
        assert_eq!(
            [
                AdminRole::Curator.name(),
                AdminRole::Ops.name(),
                AdminRole::Finance.name(),
                AdminRole::Superadmin.name(),
            ],
            ["curator", "ops", "finance", "superadmin"]
        );
        assert!(!AdminContext::Machine.is_admin());
        assert!(AdminContext::Admin {
            token_digest: "d".into(),
            role: AdminRole::Ops,
        }
        .is_admin());
    }

    #[test]
    fn tally_totals_and_bps() {
        let t = Tally {
            yes_votes: 3,
            no_votes: 1,
        };
        assert_eq!(t.total(), 4);
        assert_eq!(t.actual_yes_bps(), Some(7_500));
    }

    #[test]
    fn empty_tally_has_no_bps() {
        let t = Tally {
            yes_votes: 0,
            no_votes: 0,
        };
        assert_eq!(t.total(), 0);
        assert_eq!(t.actual_yes_bps(), None);
    }

    #[test]
    fn unanimous_tallies_hit_the_bounds() {
        let yes = Tally {
            yes_votes: 5,
            no_votes: 0,
        };
        let no = Tally {
            yes_votes: 0,
            no_votes: 5,
        };
        assert_eq!(yes.actual_yes_bps(), Some(10_000));
        assert_eq!(no.actual_yes_bps(), Some(0));
    }

    #[test]
    fn corrupt_negative_tally_yields_none() {
        let t = Tally {
            yes_votes: -3,
            no_votes: 4,
        };
        assert_eq!(t.actual_yes_bps(), None);
    }

    #[test]
    fn reputation_config_validates_every_array_invariant() {
        assert!(RepConfig::default().validate().is_ok());
        for config in [
            RepConfig {
                tier_thresholds_micro: [1, 1, 2, 3],
                ..RepConfig::default()
            },
            RepConfig {
                tier_thresholds_micro: [-1, 1, 2, 3],
                ..RepConfig::default()
            },
            RepConfig {
                position_cap_micro_by_tier: [0, 2, 1, 3, 4],
                ..RepConfig::default()
            },
            RepConfig {
                position_cap_micro_by_tier: [-1, 0, 1, 2, 3],
                ..RepConfig::default()
            },
            RepConfig {
                fee_discount_bp_by_tier: [0, 0, 0, 0, 10_001],
                ..RepConfig::default()
            },
            RepConfig {
                min_fee_bps: 10_001,
                ..RepConfig::default()
            },
            RepConfig {
                rep_score_min_pot_micro: -1,
                ..RepConfig::default()
            },
            RepConfig {
                leaderboard_min_scored: 0,
                ..RepConfig::default()
            },
        ] {
            assert!(config.validate().is_err());
        }
        assert!(RepConfig {
            discount_flip_window_secs: 0,
            rep_score_min_pot_micro: 0,
            ..RepConfig::default()
        }
        .validate()
        .is_ok());
    }

    #[test]
    fn integrity_config_rejects_every_invalid_family() {
        assert!(IntegritySweepConfig::default().validate().is_ok());
        for config in [
            IntegritySweepConfig {
                payout_hold_threshold_micro: 0,
                ..IntegritySweepConfig::default()
            },
            IntegritySweepConfig {
                sweep_delay_secs: 0,
                ..IntegritySweepConfig::default()
            },
            IntegritySweepConfig {
                burst_window_secs: 0,
                ..IntegritySweepConfig::default()
            },
            IntegritySweepConfig {
                prior_horizon_windows: 0,
                ..IntegritySweepConfig::default()
            },
            IntegritySweepConfig {
                burst_multiplier_ppm: 100_000_001,
                ..IntegritySweepConfig::default()
            },
            IntegritySweepConfig {
                young_account_share_max_ppm: 1_000_001,
                ..IntegritySweepConfig::default()
            },
            IntegritySweepConfig {
                subnet_share_max_ppm: 1_000_001,
                ..IntegritySweepConfig::default()
            },
            IntegritySweepConfig {
                device_share_max_ppm: 1_000_001,
                ..IntegritySweepConfig::default()
            },
            IntegritySweepConfig {
                min_metadata_coverage_ppm: 1_000_001,
                ..IntegritySweepConfig::default()
            },
            IntegritySweepConfig {
                min_votes_for_ratios: 0,
                ..IntegritySweepConfig::default()
            },
        ] {
            assert!(config.validate().is_err());
        }
        assert_eq!(
            IntegritySweepConfig::default()
                .thresholds()
                .min_votes_for_ratios,
            4
        );
    }

    #[test]
    fn social_config_rejects_every_invalid_family() {
        assert!(SocialConfig::default().validate().is_ok());
        for config in [
            SocialConfig {
                max_comment_len_chars: 0,
                ..SocialConfig::default()
            },
            SocialConfig {
                max_comment_len_chars: 4_001,
                ..SocialConfig::default()
            },
            SocialConfig {
                max_mentions: 6,
                ..SocialConfig::default()
            },
            SocialConfig {
                spam_window_secs: 0,
                ..SocialConfig::default()
            },
            SocialConfig {
                report_window_secs: 0,
                ..SocialConfig::default()
            },
            SocialConfig {
                max_comments_per_window: 0,
                ..SocialConfig::default()
            },
            SocialConfig {
                max_reports_per_window: 0,
                ..SocialConfig::default()
            },
            SocialConfig {
                mention_notifs_per_hour: 0,
                ..SocialConfig::default()
            },
            SocialConfig {
                report_shadow_threshold: 1,
                ..SocialConfig::default()
            },
            SocialConfig {
                reporter_min_tier: 5,
                ..SocialConfig::default()
            },
            SocialConfig {
                max_thread_depth: 0,
                ..SocialConfig::default()
            },
        ] {
            assert!(config.validate().is_err());
        }
    }

    #[test]
    fn lp_kill_config_requires_positive_operands() {
        assert!(LpKillConfig::default().validate().is_ok());
        assert_eq!(
            LpKillConfig {
                max_loss_micro: 0,
                window_days: 1,
            }
            .validate(),
            Err(LpKillConfigError("maximum loss must be positive"))
        );
        assert_eq!(
            LpKillConfig {
                max_loss_micro: 1,
                window_days: 0,
            }
            .validate(),
            Err(LpKillConfigError("window days must be positive"))
        );
    }

    #[test]
    fn content_config_validates_every_scalar_and_tier_invariant() {
        let valid = ContentConfig::default();
        assert_eq!(valid.clone().validate(), Ok(valid.clone()));

        let mut invalid = Vec::new();
        macro_rules! invalid_scalar {
            ($field:ident, $value:expr) => {{
                let mut config = valid.clone();
                config.$field = $value;
                invalid.push(config);
            }};
        }
        invalid_scalar!(flash_cadence_secs, 0);
        invalid_scalar!(daily_slots, 0);
        invalid_scalar!(draft_ttl_secs, 0);
        invalid_scalar!(max_pending_drafts, 0);
        invalid_scalar!(max_slot_horizon_secs, 0);
        invalid_scalar!(daily_seed_budget_micro, 0);
        invalid_scalar!(video_max_attempts, 0);
        invalid_scalar!(lease_secs, 0);
        invalid_scalar!(backoff_base_secs, 0);
        invalid_scalar!(render_dir, std::path::PathBuf::new());

        for mutate in [
            |tier: &mut ContentTierConfig| tier.open_secs = 0,
            |tier: &mut ContentTierConfig| tier.hidden_window_secs = tier.open_secs,
            |tier: &mut ContentTierConfig| tier.seed_floor_micro = 0,
            |tier: &mut ContentTierConfig| tier.seed_micro = tier.seed_floor_micro - 1,
            |tier: &mut ContentTierConfig| tier.min_votes_floor = 0,
            |tier: &mut ContentTierConfig| {
                tier.min_votes_to_resolve = tier.min_votes_floor - 1;
            },
            |tier: &mut ContentTierConfig| tier.fee_bps = 10_001,
        ] {
            let mut config = valid.clone();
            mutate(&mut config.tier_defaults[0]);
            invalid.push(config);
        }

        assert_eq!(invalid.len(), 17);
        for config in invalid {
            assert_eq!(config.validate(), Err(ContentConfigError));
        }
    }
}
