//! Error vocabulary crossing the ports: adapter failures (`StoreError`) and
//! business-rule rejections (`AppError`). Domain types and plain data only.

use thiserror::Error;

/// Failures surfaced by a port implementation (fake or Postgres).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum StoreError {
    #[error("not found: {0}")]
    NotFound(&'static str),
    /// An idempotency key was reused inside `ledger_apply`. Under the
    /// guard-first write sequence this is an invariant violation (a bug),
    /// never a recovery path (codex P1R2 B2).
    #[error("duplicate idempotency key")]
    DuplicateKey,
    #[error("ledger rejected transaction: {0}")]
    Ledger(#[from] domain::ledger::LedgerError),
    /// The store observed a state that the write protocol makes impossible.
    #[error("store invariant violated: {0}")]
    Invariant(&'static str),
    /// A uniqueness constraint other than an idempotency key (e.g. one vote
    /// per user+market, one channel link per address, one deposit per
    /// chain signature).
    #[error("uniqueness conflict: {0}")]
    Conflict(&'static str),
    #[error("database integrity failure: {0}")]
    Integrity(String),
    #[error("backend failure: {0}")]
    Backend(String),
    /// A skeleton-phase component that a later task owns was invoked. The
    /// payload names the phase and area, e.g. `phase6:ops-config`.
    #[error("component unavailable: {0}")]
    Unavailable(&'static str),
}

/// Business-rule rejections produced by use cases.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum AppError {
    /// Market is not `Live` (D22: only live markets trade).
    #[error("market is not open for trading")]
    MarketNotOpen,
    /// The tally-hidden freeze window has begun (D22 full freeze).
    #[error("trading is frozen for this market")]
    TradingFrozen,
    /// The vote-gate (D4): a user must vote before trading a market.
    #[error("user must vote on this market before trading")]
    VoteRequired,
    /// Sell exceeds the caller's position (M5 position accounting).
    #[error("position has insufficient shares")]
    InsufficientShares,
    #[error("position cap exceeded for tier {tier}: cap is {cap_micro} micro-USD")]
    PositionCapExceeded { cap_micro: i64, tier: u8 },
    #[error("market base fee {base_bps} bps is below the configured floor {min_bps} bps")]
    SeedFeeBelowMinimum { base_bps: u16, min_bps: u16 },
    /// The payer's balance cannot cover the operation (e.g. seeding from an
    /// uncapitalized house — run `EnsureGenesis` first).
    #[error("insufficient funds")]
    InsufficientFunds,
    /// One vote per (user, market) — D4.
    #[error("user already voted on this market")]
    AlreadyVoted,
    /// The voting window has closed (`now >= closes_at`), even if the market
    /// row lags in `Live`/`Closing` (codex P1R2).
    #[error("voting window has closed")]
    VotingClosed,
    /// `crowd_guess_pct` must be at most 100.
    #[error("crowd guess must be at most 100 percent")]
    InvalidCrowdGuess,
    #[error("an iMessage-linked identity is required to vote")]
    PhoneVerificationRequired,
    #[error("vote velocity limit exceeded")]
    VoteVelocityExceeded,
    #[error("account is too young to vote near market close")]
    AccountTooYoungNearClose,
    #[error("comment is not visible")]
    CommentNotVisible,
    #[error("comment vote already exists")]
    CommentAlreadyVoted,
    #[error("comment vote value must be -1 or +1")]
    InvalidCommentVote,
    #[error("comment thread is too deep")]
    ThreadTooDeep,
    #[error("comment report requires an older account or higher reputation tier")]
    ReporterNotQualified,
    #[error("comment report velocity limit exceeded")]
    ReportVelocityExceeded,
    #[error("comment was blocked: {0}")]
    CommentBlocked(&'static str),
    #[error("draft is invalid: {0}")]
    InvalidDraft(&'static str),
    #[error("draft is not pending review")]
    DraftNotPending,
    #[error("draft is not approved for publication")]
    DraftNotApproved,
    #[error("draft has expired")]
    DraftExpired,
    #[error("pending draft limit reached")]
    PendingDraftLimit,
    #[error("no publication slot is free in the configured horizon")]
    NoSlotFree,
    #[error("daily seed budget would be exceeded")]
    DailySeedBudgetExceeded,
    #[error("artifact job is not ready")]
    JobNotReady,
    #[error("artifact is not available")]
    ArtifactNotFound,
    /// The requested lifecycle edge is not legal from the market's current
    /// state (HTTP 409 at the adapter).
    #[error("illegal market lifecycle transition")]
    IllegalTransition,
    /// `AdvanceMarket` accepts ONLY non-financial events (P1R2 B4): state
    /// changes with money consequences must ride the conservation-checked
    /// settlement transaction in `ResolveMarket`.
    #[error("this lifecycle event must go through ResolveMarket")]
    UseResolveMarket,
    /// D21: votes below the resolve minimum but open interest at/above the
    /// floor — no silent auto-void; a curator must decide.
    #[error("low participation with material open interest: curator decision required")]
    NeedsCuratorDecision,
    /// Curator overrides are legal only for a market explicitly flagged by
    /// the scheduler.
    #[error("curator override is not allowed for this market")]
    CuratorOverrideNotAllowed,
    #[error("market is under automated review")]
    UnderReview,
    #[error("market requires an explicit curator decision")]
    CuratorRequired,
    /// D23 seed-time LP loss breaker. Existing live markets remain open.
    #[error("market seeding is paused by the LP loss limit")]
    LpPaused,
    /// D25a: a preview-relevant config field for THIS market and user changed
    /// since the previewed generation (HTTP 409).
    #[error(
        "config changed since preview: previewed generation {preview_generation}, \
         current generation {current_generation}"
    )]
    StaleConfig {
        preview_generation: i64,
        current_generation: i64,
    },
    /// D25 replay precedence: an idempotency key was reused with a DIFFERENT
    /// canonical request fingerprint (HTTP 409).
    #[error("idempotency key was reused with a different request")]
    IdempotencyConflict,
    /// D25: a trading fence (global or per-market) is in force (HTTP 423).
    #[error("trading is paused")]
    TradingPaused,
    /// D25: a per-market voting pause is in force (HTTP 423). Auto-expires at
    /// `tally_hidden_at`.
    #[error("voting is paused for this market")]
    VotingPaused,
    /// D24: a config proposal cannot proceed (base generation moved
    /// incompatibly, already settled, expired, or same-principal confirm).
    #[error("config proposal conflict: {0}")]
    ProposalConflict(&'static str),
    /// D30: the operation is blocked while the user has an open receivable.
    #[error("user has an open receivable of {outstanding_micro} micro-USD")]
    ReceivableOpen { outstanding_micro: i64 },
    /// D26: the authenticated admin actor may not perform this operation
    /// (HTTP 403) — e.g. a same-token confirmation, an unlisted role, or a
    /// sensitive key sent to the direct write path.
    #[error("admin actor is not permitted: {0}")]
    AdminForbidden(&'static str),
    /// D24: a config patch failed typed whole-snapshot validation (HTTP 422).
    #[error("config value rejected for {key}: {reason}")]
    ConfigInvalid { key: String, reason: &'static str },
    /// D36: `PlaceTrade` omitted `expected_config_version` (HTTP 422).
    #[error("expected_config_version is required")]
    ExpectedConfigVersionRequired,
    /// D33 fail-closed money gate (HTTP 403).
    #[error("money mutation forbidden: {0}")]
    MoneyForbidden(&'static str),
    /// D32 deposit pause (HTTP 423).
    #[error("deposits are paused")]
    DepositsPaused,
    /// D32 admission refused; funds remain in suspense (HTTP 409).
    #[error("deposit held for compliance: {reason}")]
    ComplianceHold { reason: &'static str },
    /// D32 `BonusReserve` cannot cover the stamped redemption promise.
    #[error("bonus reserve {reserve_micro} is below promised liability {promised_micro}")]
    InsufficientBonusReserve {
        reserve_micro: i64,
        promised_micro: i64,
    },
    /// D32 referral bind/grant refused.
    #[error("referral is not eligible")]
    ReferralIneligible,
    #[error("arithmetic overflow")]
    Overflow,
    #[error(transparent)]
    Amm(#[from] domain::amm::AmmError),
    #[error(transparent)]
    Resolution(#[from] domain::resolution::ResolutionError),
    #[error(transparent)]
    Scoring(#[from] domain::scoring::ScoringError),
    #[error(transparent)]
    Store(#[from] StoreError),
}
