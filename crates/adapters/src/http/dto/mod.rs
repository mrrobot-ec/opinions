//! Wire DTOs — mirror application models; **no domain type serializes directly**.

use application::model::{
    CommentView, HolderRow, MarketHolders, MarketRow, MarketSnapshot, ModerationStatus,
    NotificationRow, PositionView, ProfileTradeRow, ProfileVoteRow, ReportedCommentRow,
    TradeAction, TradeReceipt, UserProfile,
};
use application::preview_trade::TradePreview;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

include!("market.rs");
include!("social.rs");
include!("core.rs");

pub mod compliance;
pub mod content;
pub mod ops_admin;
pub mod ops_config;
pub mod video;
pub mod withdraw;
#[allow(unused_imports)]
pub use compliance::*;
#[allow(unused_imports)]
pub use content::*;
#[allow(unused_imports)]
pub use ops_admin::*;
#[allow(unused_imports)]
pub use ops_config::*;
#[allow(unused_imports)]
pub use video::*;
pub use withdraw::*;
