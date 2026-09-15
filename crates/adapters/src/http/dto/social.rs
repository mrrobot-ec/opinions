// ---------------------------------------------------------------------------
// Social / profiles / notifications
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateUserRequest {
    pub handle: String,
    pub channel: String,
    pub address: String,
    pub created_at_override: Option<time::OffsetDateTime>,
    pub rep_seed_micro: Option<i64>,
}

#[derive(Debug, Clone, Serialize, ToSchema, PartialEq, Eq)]
pub struct CommentDto {
    pub id: Uuid,
    pub market_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub author_id: Uuid,
    pub author_handle: String,
    pub body: String,
    pub depth: u8,
    pub score: i32,
    pub reply_count: u32,
    pub moderation_status: String,
    pub created_at: time::OffsetDateTime,
}

impl From<CommentView> for CommentDto {
    fn from(view: CommentView) -> Self {
        Self {
            id: view.row.id.0,
            market_id: view.row.market.0,
            parent_id: view.row.parent.map(|id| id.0),
            author_id: view.row.author.0,
            author_handle: view.author_handle,
            body: view.row.body,
            depth: view.row.depth,
            score: view.row.score,
            reply_count: view.row.reply_count,
            moderation_status: moderation_name(view.row.moderation_status).to_string(),
            created_at: view.row.created_at,
        }
    }
}

fn moderation_name(status: ModerationStatus) -> &'static str {
    match status {
        ModerationStatus::Visible => "visible",
        ModerationStatus::Shadow => "shadow",
        ModerationStatus::Blocked => "blocked",
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CommentPageDto {
    pub comments: Vec<CommentDto>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PostCommentRequest {
    pub user_id: Uuid,
    pub body: String,
    pub parent_id: Option<Uuid>,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct VoteCommentRequest {
    pub user_id: Uuid,
    pub value: i16,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CommentVoteDto {
    pub score: i32,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ReportCommentRequest {
    pub user_id: Uuid,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CommentReportDto {
    pub reported: bool,
    pub report_count: u32,
}

#[derive(Debug, Clone, Serialize, ToSchema, PartialEq, Eq)]
pub struct HolderDto {
    pub user_id: Uuid,
    pub handle: String,
    pub tier: u8,
    /// Ranked by committed capital; not mark-to-market value.
    pub cost_micro: i64,
}

impl From<HolderRow> for HolderDto {
    fn from(row: HolderRow) -> Self {
        Self {
            user_id: row.user.0,
            handle: row.handle,
            tier: row.tier,
            cost_micro: row.cost.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct HoldersDto {
    pub yes: Vec<HolderDto>,
    pub no: Vec<HolderDto>,
}

impl From<MarketHolders> for HoldersDto {
    fn from(rows: MarketHolders) -> Self {
        Self {
            yes: rows.yes.into_iter().map(HolderDto::from).collect(),
            no: rows.no.into_iter().map(HolderDto::from).collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ProfileTradeDto {
    pub market_id: Uuid,
    pub market_ref: String,
    pub side: SideDto,
    pub action: TradeActionDto,
    pub collateral_micro: i64,
    pub created_at: time::OffsetDateTime,
    pub trade_seq: i64,
}

impl From<ProfileTradeRow> for ProfileTradeDto {
    fn from(row: ProfileTradeRow) -> Self {
        Self {
            market_id: row.market.0,
            market_ref: row.market_ref,
            side: row.side.into(),
            action: row.action.into(),
            collateral_micro: row.collateral_micro,
            created_at: row.created_at,
            trade_seq: row.trade_seq,
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ProfileVoteDto {
    pub market_id: Uuid,
    pub market_question: String,
    pub cast_at: time::OffsetDateTime,
    pub side: Option<SideDto>,
    pub score_bp: Option<u16>,
}

impl From<ProfileVoteRow> for ProfileVoteDto {
    fn from(row: ProfileVoteRow) -> Self {
        Self {
            market_id: row.market.0,
            market_question: row.market_question,
            cast_at: row.cast_at,
            side: row.side.map(SideDto::from),
            score_bp: row.score_bp,
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct VoterSummaryDto {
    pub avg_score_bp: Option<i64>,
    pub markets_scored: u32,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct UserProfileDto {
    pub user_id: Uuid,
    pub handle: String,
    pub created_at: time::OffsetDateTime,
    pub rep_micro: i64,
    pub tier: u8,
    pub voter: VoterSummaryDto,
    pub realized_pnl_micro: i64,
    pub recent_trades: Vec<ProfileTradeDto>,
    pub recent_votes: Vec<ProfileVoteDto>,
}

impl From<UserProfile> for UserProfileDto {
    fn from(profile: UserProfile) -> Self {
        Self {
            user_id: profile.user.0,
            handle: profile.handle,
            created_at: profile.created_at,
            rep_micro: profile.rep_micro,
            tier: profile.tier,
            voter: VoterSummaryDto {
                avg_score_bp: profile.avg_score_bp,
                markets_scored: profile.markets_scored,
            },
            realized_pnl_micro: profile.realized_pnl.0,
            recent_trades: profile
                .recent_trades
                .into_iter()
                .map(ProfileTradeDto::from)
                .collect(),
            recent_votes: profile
                .recent_votes
                .into_iter()
                .map(ProfileVoteDto::from)
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct NotificationDto {
    pub id: i64,
    #[serde(rename = "type")]
    pub notification_type: String,
    pub market_id: Option<Uuid>,
    pub payload: serde_json::Value,
    pub read_at: Option<time::OffsetDateTime>,
    pub created_at: time::OffsetDateTime,
    pub source_seq: Option<i64>,
}

impl From<NotificationRow> for NotificationDto {
    fn from(row: NotificationRow) -> Self {
        Self {
            id: row.id,
            notification_type: row.notification_type,
            market_id: row.market.map(|market| market.0),
            payload: row.payload,
            read_at: row.read_at,
            created_at: row.created_at,
            source_seq: row.source_seq,
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct NotificationPageDto {
    pub notifications: Vec<NotificationDto>,
    pub unread_count: u32,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct MarkNotificationsReadRequest {
    pub ids: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct UpdatedDto {
    pub updated: u32,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct UnreadCountDto {
    pub unread_count: u32,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ReportedCommentDto {
    pub comment: CommentDto,
    pub report_count: u32,
    pub reporters: Vec<String>,
}

impl From<ReportedCommentRow> for ReportedCommentDto {
    fn from(row: ReportedCommentRow) -> Self {
        Self {
            comment: row.comment.into(),
            report_count: row.report_count,
            reporters: row.reporters,
        }
    }
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ModerateCommentRequest {
    pub status: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ModeratedCommentDto {
    pub status: String,
}
