#[async_trait]
pub trait CommentWriter: Send {
    /// Read by caller-generated id after the request-key lock for replay.
    async fn comment(&mut self, id: CommentId) -> Result<Option<CommentRow>, StoreError>;
    /// Locks a comment row through transaction end.
    async fn comment_for_update(&mut self, id: CommentId) -> Result<CommentRow, StoreError>;
    async fn recent_same_hash(
        &mut self,
        author: UserId,
        hash: &str,
        since: OffsetDateTime,
    ) -> Result<u32, StoreError>;
    async fn author_posts_since(
        &mut self,
        author: UserId,
        since: OffsetDateTime,
    ) -> Result<u32, StoreError>;
    async fn insert_comment(&mut self, comment: NewComment) -> Result<(), StoreError>;
    async fn bump_reply_count(&mut self, parent: CommentId) -> Result<(), StoreError>;
    /// Returns `true` only when the unique vote row was inserted.
    async fn insert_comment_vote(
        &mut self,
        comment: CommentId,
        user: UserId,
        value: i16,
    ) -> Result<bool, StoreError>;
    async fn adjust_comment_score(
        &mut self,
        comment: CommentId,
        delta: i16,
    ) -> Result<i32, StoreError>;
    /// Returns `true` only when the reporter's unique row was inserted.
    async fn insert_comment_report(
        &mut self,
        comment: CommentId,
        reporter: UserId,
        created_at: OffsetDateTime,
    ) -> Result<bool, StoreError>;
    async fn comment_report_count(&mut self, comment: CommentId) -> Result<u32, StoreError>;
    async fn reporter_reports_since(
        &mut self,
        reporter: UserId,
        since: OffsetDateTime,
    ) -> Result<u32, StoreError>;
    async fn set_comment_status(
        &mut self,
        comment: CommentId,
        status: crate::model::ModerationStatus,
    ) -> Result<(), StoreError>;
    async fn delete_comment_reports(&mut self, comment: CommentId) -> Result<u32, StoreError>;
}

pub trait CommentTx:
    IdempotencyGuard
    + UserLockGuard
    + MarketReader
    + CommentWriter
    + UserReader
    + VoteReader
    + OutboxWriter
    + AuditWrite
    + Committable
{
}

#[async_trait]
pub trait NotifyReader: Send {
    /// Locks the seeded notifier cursor through transaction end.
    async fn lock_notifier_cursor(&mut self) -> Result<i64, StoreError>;
    /// Plain immutable ordered read; implementations must not lock outbox rows.
    async fn outbox_events_after(
        &mut self,
        last_seq: i64,
        limit: u32,
    ) -> Result<Vec<OutboxEvent>, StoreError>;
    /// One row per terminal participant, with sell facts excluded.
    async fn resolution_recipients(
        &mut self,
        market: MarketId,
        voided: bool,
    ) -> Result<Vec<ResolutionRecipient>, StoreError>;
    async fn parent_author(&mut self, comment: CommentId) -> Result<Option<UserId>, StoreError>;
    async fn users_by_handles(&mut self, handles: &[String]) -> Result<Vec<UserId>, StoreError>;
    async fn notification_count_since(
        &mut self,
        user: UserId,
        notification_type: &str,
        since: OffsetDateTime,
    ) -> Result<u32, StoreError>;
}

#[async_trait]
pub trait NotificationWriter: Send {
    /// Bulk insert with the partial-index conflict predicate named exactly.
    async fn insert_notifications(
        &mut self,
        notifications: &[NewNotification],
    ) -> Result<Vec<NotificationRow>, StoreError>;
    async fn advance_notifier_cursor(&mut self, seq: i64) -> Result<(), StoreError>;
}

pub trait NotificationTx: NotifyReader + NotificationWriter + Committable {}
impl<T> NotificationTx for T where T: NotifyReader + NotificationWriter + Committable {}
impl<T> CommentTx for T where
    T: IdempotencyGuard
        + UserLockGuard
        + MarketReader
        + CommentWriter
        + UserReader
        + VoteReader
        + OutboxWriter
        + AuditWrite
        + Committable
{
}

/// Read-model surface for stable comment pagination.
#[async_trait]
pub trait SocialQueries: Send + Sync {
    async fn comments(
        &self,
        market: MarketId,
        sort: CommentSort,
        viewer: Option<UserId>,
        limit: u32,
        cursor: Option<CommentCursor>,
        now: OffsetDateTime,
    ) -> Result<CommentPage, StoreError>;
    async fn comment_view(
        &self,
        comment: CommentId,
        viewer: Option<UserId>,
    ) -> Result<CommentView, StoreError>;
    async fn holders(&self, market: MarketId, limit: u32) -> Result<MarketHolders, StoreError>;
    async fn user_profile(&self, user: UserId) -> Result<UserProfile, StoreError>;
    async fn reported_comments(
        &self,
        threshold: u32,
        limit: u32,
    ) -> Result<Vec<ReportedCommentRow>, StoreError>;
}

#[async_trait]
pub trait NotificationQueries: Send + Sync {
    async fn notifications(
        &self,
        user: UserId,
        limit: u32,
        before_id: Option<i64>,
    ) -> Result<Vec<NotificationRow>, StoreError>;
    async fn unread_count(&self, user: UserId) -> Result<u32, StoreError>;
    /// Marks only rows owned by `user`; returns the affected count.
    async fn mark_notifications_read(
        &self,
        user: UserId,
        ids: &[i64],
        now: OffsetDateTime,
    ) -> Result<u32, StoreError>;
}
