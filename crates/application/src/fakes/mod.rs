//! In-memory port implementations honoring the write-path semantics the
//! Postgres adapter provides: per-key serialization (advisory-lock analogue),
//! row-style locks held to transaction end, buffered writes that become
//! observable only at commit, and atomicity on failure. Money lives in
//! [`domain::ledger::Balances`] so the fake cannot drift from domain rules.
//!
//! Divergence to know about: `LedgerWriter::account` get-or-create is durable
//! even if the tx later rolls back (like a sequence allocation). A
//! zero-balance account is unobservable through the ports, so the contract
//! suites cannot tell the difference — but [`InMemoryStore::snapshot`] can,
//! so test fixtures pre-create the accounts they touch.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::Hash;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex as PlMutex;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use uuid::Uuid;

use domain::amm::{Pool, Side};
use domain::ledger::{AccountId, Balances, Currency, Entry, OwnerType, Transaction, TxnKind};
use domain::market::MarketState;
use domain::money::{BasisPoints, MicroShares, MicroUsd};
use time::OffsetDateTime;

use crate::error::StoreError;
use crate::model::{
    CommentCursor, CommentId, CommentPage, CommentRow, CommentSort, CommentView, DailyFeeRow,
    DepositId, DraftRow, DueMarket, Event, FlaggedMarketRow, Holding, HoldingOwner, InsertedTrade,
    IntegrityReportRow, IntegritySweepConfig, LifecycleCommand, MarketHolders, MarketId, MarketRow,
    MarketSnapshot, ModerationJobRow, ModerationStatus, NewComment, NewDeposit, NewMarket,
    NewNotification, NewTrade, NewVote, NotificationRow, OutboxEvent, OutcomeId, OwnerRef, PoolRow,
    PositionRow, PositionView, PricePoint, ProfileTradeRow, ProfileVoteRow, RealizationFact,
    RealizationSource, ReportedCommentRow, ReputationRow, ResolutionRecipient, Tally, TapeRow,
    TradeId, TradeReceipt, TraderRow, UserId, UserProfile, VideoJobRow, VoteFact, VoteId,
    VoteReceipt, VoteScoreUpdate, VoterRow,
};
use crate::ports::{
    AdvanceTx, BootstrapTx, Clock, CommentTx, CommentWriter, Committable, ContentTx, DepositAmlIo,
    DepositTx, DepositWriter, IdempotencyGuard, IntegritySweepIo, IntegrityTx, InvariantReadTx,
    LedgerWriter, LifecycleCommandWriter, MarketQueries, MarketReader, MarketWriter,
    NotificationQueries, NotificationTx, NotificationWriter, NotifyReader, OpsAuditTx, OpsConfigTx,
    OutboxWriter, PoolWriter, PositionWriter, RealizationWriter, ResolveTx, SeedEconomyIo, SeedTx,
    SettlementIo, SocialQueries, Store, TradeEconomyReader, TradeTx, TradeWriter, UnwindTx,
    UserLockGuard, UserReader, UserWriter, VideoTx, VoteReader, VoteTx, VoteWriter,
};

include!("state.rs");
include!("market.rs");
include!("social.rs");
include!("core.rs");
include!("credit.rs");

mod compliance;
mod content;
mod ops;
mod ops_config;
mod video;

pub use crate::ports::UnavailableMoney;
pub use compliance::{FakeComplianceStore, RecordingPhone};
pub use content::{UnavailableDraftEngine, UnavailableModerationPreflight};
pub use ops::{UnavailableInvariantReadTx, UnavailableOpsAuditTx, UnavailableUnwindTx};
pub use video::UnavailableRenderer;
