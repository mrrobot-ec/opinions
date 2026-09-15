//! Ports: small role traits (ISP). Every implementation — Postgres and the
//! in-memory fake — passes the same contract suites in [`crate::contract`].
//!
//! Locking protocol for every write use case (codex-p1r1 B1/B2/B3, fixed
//! order): (1) [`IdempotencyGuard::serialize_key`] FIRST, before any read;
//! (2) [`MarketReader::market_for_update`] and re-validate under the row
//! lock; (3) [`PoolWriter::pool_for_update`] / `position_for_update` as
//! needed; (4) [`LedgerWriter::ledger_apply`] locks accounts in account-id
//! order. Deviating from this order is a review-blocker.

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::error::StoreError;
use crate::model::{
    CommentCursor, CommentId, CommentPage, CommentRow, CommentSort, CommentView, DepositId,
    DueMarket, Event, FlaggedMarketRow, Holding, InsertedTrade, IntegrityReportRow,
    IntegritySweepConfig, LifecycleCommand, MarketHolders, MarketId, MarketRow, MarketSnapshot,
    NewComment, NewDeposit, NewMarket, NewNotification, NewTrade, NewVote, NotificationRow,
    OutboxEvent, OutcomeId, OwnerRef, PoolRow, PositionRow, PositionView, PricePoint,
    ReportedCommentRow, ReputationRow, ResolutionRecipient, Tally, TapeRow, TradeReceipt, UserId,
    UserProfile, VoteFact, VoteId, VoteReceipt, VoteScoreUpdate,
};
use domain::money::{BasisPoints, MicroUsd};

mod money;
include!("core.rs");
include!("market.rs");
include!("social.rs");
mod content;
mod ops;
mod video;

pub use content::*;
pub use money::*;
pub use ops::*;
pub use video::*;
