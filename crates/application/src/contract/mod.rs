//! Store-generic contract suites (LSP as executable tests, codex M2/P1R2):
//! suites are generic over the [`Store`] itself and open transactions through
//! it. Every suite runs identically against `InMemoryStore` (Task 1.1) and
//! `PgStore` (Task 1.3) by passing the store; concurrency suites open two
//! transactions from the same store. Helpers take `&mut (dyn RoleTrait + '_)`
//! so suites never name a boxed-future type.
//!
//! These functions assert with panics by design — they ARE the tests.
#![allow(
    clippy::unwrap_used,
    clippy::missing_panics_doc,
    clippy::items_after_test_module
)]

use domain::amm::Side;
use domain::ledger::{Currency, Entry, LedgerError, TxnKind};
use domain::money::{BasisPoints, MicroShares, MicroUsd};
use uuid::Uuid;

use crate::ensure_genesis::{genesis_key, EnsureGenesis, EnsureGenesisCmd};
use crate::error::StoreError;
use crate::model::{
    CommentId, CommentSort, IntegrityReportRow, IntegritySweepConfig, MarketId, ModerationStatus,
    NewComment, NewDeposit, NewMarket, NewVote, OwnerRef, UserId,
};
use crate::ports::{LedgerWriter, MarketQueries, SettlementIo, SocialQueries, Store, TradeTx};
use crate::seed_market::{SeedMarket, SeedMarketCmd};

include!("market.rs");
include!("social.rs");
include!("core.rs");

pub mod content;
pub mod video;

fn unique_key(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4())
}
