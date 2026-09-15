/// Wall-clock port. Plain sync trait — reading a clock is not I/O that
/// deserves an executor hop (grok m1).
pub trait Clock: Send + Sync {
    fn now(&self) -> OffsetDateTime;
}

/// Transaction factories ONLY — lock-free reads live on [`MarketQueries`]
/// (codex ckpt 2: `Store` must not become a god facade). Each factory opens
/// one atomic unit of work whose writes become observable only at
/// [`Committable::commit`].
#[async_trait]
pub trait Store: Send + Sync {
    /// # Errors
    /// Returns [`StoreError`] when the backend cannot open a transaction.
    async fn trade_tx(&self) -> Result<Box<dyn TradeTx + '_>, StoreError>;
    /// # Errors
    /// Returns [`StoreError`] when the backend cannot open a transaction.
    async fn vote_tx(&self) -> Result<Box<dyn VoteTx + '_>, StoreError>;
    /// # Errors
    /// Returns [`StoreError`] when the backend cannot open a transaction.
    async fn resolve_tx(&self) -> Result<Box<dyn ResolveTx + '_>, StoreError>;
    /// # Errors
    /// Returns [`StoreError`] when the backend cannot open a transaction.
    async fn deposit_tx(&self) -> Result<Box<dyn DepositTx + '_>, StoreError>;
    /// # Errors
    /// Returns [`StoreError`] when the backend cannot open a transaction.
    async fn seed_tx(&self) -> Result<Box<dyn SeedTx + '_>, StoreError>;
    /// # Errors
    /// Returns [`StoreError`] when the backend cannot open a transaction.
    async fn advance_tx(&self) -> Result<Box<dyn AdvanceTx + '_>, StoreError>;
    /// # Errors
    /// Returns [`StoreError`] when the backend cannot open a transaction.
    async fn bootstrap_tx(&self) -> Result<Box<dyn BootstrapTx + '_>, StoreError>;
    /// Opens the report-writing half of the two-transaction integrity sweep.
    async fn integrity_tx(&self) -> Result<Box<dyn IntegrityTx + '_>, StoreError>;
    /// Opens a comment write transaction.
    async fn comment_tx(&self) -> Result<Box<dyn CommentTx + '_>, StoreError>;
    /// Opens one locked-cursor notifier transaction.
    async fn notification_tx(&self) -> Result<Box<dyn NotificationTx + '_>, StoreError>;
    /// Opens the phase-5 draft/publication transaction role.
    async fn content_tx(&self) -> Result<Box<dyn ContentTx + '_>, StoreError>;
    /// Opens the shared phase-5 artifact/moderation job transaction role.
    async fn video_tx(&self) -> Result<Box<dyn VideoTx + '_>, StoreError>;
    /// Opens the generation-serialized config write transaction (D24).
    /// Skeleton behavior until W1: `Unavailable("phase6:ops-config")`.
    async fn ops_config_tx(&self) -> Result<Box<dyn OpsConfigTx + '_>, StoreError>;
    /// Opens a thin audit-only admin transaction (D26).
    /// Skeleton behavior until W2: `Unavailable("phase6:ops-audit")`.
    async fn ops_audit_tx(&self) -> Result<Box<dyn OpsAuditTx + '_>, StoreError>;
    /// Opens one repeatable-read invariant snapshot (D27).
    /// Skeleton behavior until W2: `Unavailable("phase6:invariants")`.
    async fn invariant_read_tx(&self) -> Result<Box<dyn InvariantReadTx + '_>, StoreError>;
    /// Opens the dual-controlled unwind/receivables transaction (D30).
    /// Skeleton behavior until W2: `Unavailable("phase6:unwind")`.
    async fn unwind_tx(&self) -> Result<Box<dyn UnwindTx + '_>, StoreError>;
    /// Phase 7 withdrawal unit of work. Skeleton: `Unavailable("phase7:withdraw")`.
    async fn withdraw_tx(&self) -> Result<Box<dyn money::WithdrawTx + '_>, StoreError>;
    /// Phase 7 deposit admission unit of work. Skeleton: `Unavailable("phase7:deposit-admission")`.
    async fn deposit_admission_tx(
        &self,
    ) -> Result<Box<dyn money::DepositAdmissionTx + '_>, StoreError>;
    /// Phase 7 credit convert unit of work. Skeleton: `Unavailable("phase7:credit-convert")`.
    async fn credit_convert_tx(&self) -> Result<Box<dyn money::CreditConvertTx + '_>, StoreError>;
}
