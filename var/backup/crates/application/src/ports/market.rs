#[async_trait]
pub trait MarketReader: Send {
    /// Row-locked read (`SELECT ... FOR UPDATE`) — every authority check in a
    /// write use case reads the market through THIS, never through `Store`
    /// views (codex B2).
    ///
    /// # Errors
    /// [`StoreError::NotFound`] for an unknown market; backend failures.
    async fn market_for_update(&mut self, m: MarketId) -> Result<MarketRow, StoreError>;
}

#[async_trait]
pub trait IdempotencyGuard: Send {
    /// `pg_advisory_xact_lock(1, hashtext(key))` — MUST be the first call in
    /// every write tx (codex B3): it serializes duplicate requests so the
    /// read-or-create that follows is race-free.
    ///
    /// # Errors
    /// Backend failures only; blocking on a concurrent holder is not an error.
    async fn serialize_key(&mut self, key: &str) -> Result<(), StoreError>;
}

#[async_trait]
pub trait VoteReader: Send {
    /// In-transaction read used by write-path authorization (the D4
    /// vote-gate). Lock-free view reads live on [`MarketQueries`].
    ///
    /// # Errors
    /// Backend failures.
    async fn user_voted(&mut self, u: UserId, m: MarketId) -> Result<bool, StoreError>;
    /// # Errors
    /// Backend failures.
    async fn tally(&mut self, m: MarketId) -> Result<Tally, StoreError>;
    /// Counts this user's votes across all markets in the rolling window.
    async fn votes_count_since(
        &mut self,
        u: UserId,
        since: OffsetDateTime,
    ) -> Result<u32, StoreError>;
    /// Returns the account creation timestamp.
    async fn user_created_at(&mut self, u: UserId) -> Result<OffsetDateTime, StoreError>;
    /// Checks for a specific linked channel type.
    async fn user_has_channel(&mut self, u: UserId, channel: &str) -> Result<bool, StoreError>;
}

#[async_trait]
pub trait UserLockGuard: Send {
    /// Serializes cross-market vote limits for one user. Called after replay
    /// lookup and before the market row lock.
    async fn lock_user(&mut self, u: UserId) -> Result<(), StoreError>;
}

#[async_trait]
pub trait PoolWriter: Send {
    /// Row-locked read of the market's pool reserves.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when the market has no pool; backend failures.
    async fn pool_for_update(&mut self, m: MarketId) -> Result<PoolRow, StoreError>;
    /// # Errors
    /// Backend failures.
    async fn save_reserves(
        &mut self,
        m: MarketId,
        pool: &domain::amm::Pool,
    ) -> Result<(), StoreError>;
}

#[async_trait]
pub trait LedgerWriter: Send {
    /// # Errors
    /// Backend failures.
    async fn txn_by_key(&mut self, key: &str) -> Result<Option<uuid::Uuid>, StoreError>;
    /// Get-or-create (unique-indexed per migration 0003): resolving the same
    /// owner+currency twice yields the same account.
    ///
    /// # Errors
    /// Backend failures.
    async fn account(
        &mut self,
        owner: OwnerRef,
        currency: domain::ledger::Currency,
    ) -> Result<domain::ledger::AccountId, StoreError>;
    /// Locks every touched account row FOR UPDATE **in account-id order**
    /// (deadlock-free), aggregates entries per account, validates
    /// post-balances (non-External >= 0) under the locks, then inserts
    /// header + entries (codex B1). DB triggers stay the backstop.
    ///
    /// # Errors
    /// [`StoreError::Ledger`] for domain rejections (unbalanced, insufficient
    /// funds, ...); [`StoreError::DuplicateKey`] if `key` was already used —
    /// under the guard-first sequence that is an invariant violation, never a
    /// recovery path (codex P1R2 B2).
    async fn ledger_apply(
        &mut self,
        kind: domain::ledger::TxnKind,
        key: &str,
        entries: &[domain::ledger::Entry],
    ) -> Result<uuid::Uuid, StoreError>;
}

#[async_trait]
pub trait TradeWriter: Send {
    /// # Errors
    /// Backend failures.
    async fn insert_trade(&mut self, t: NewTrade) -> Result<InsertedTrade, StoreError>;
    /// # Errors
    /// Backend failures.
    async fn trade_by_ledger_txn(
        &mut self,
        txn: uuid::Uuid,
    ) -> Result<Option<TradeReceipt>, StoreError>;
}

#[async_trait]
pub trait UserReader: Send {
    /// Returns the public handle used by tape events.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when the user does not exist.
    async fn handle(&mut self, u: UserId) -> Result<String, StoreError>;
    /// Resolves a normalized handle for mention fanout.
    async fn user_by_handle(&mut self, handle: &str) -> Result<Option<UserId>, StoreError>;
    /// Reputation tier used by the reporter-quality floor.
    async fn user_tier(&mut self, u: UserId) -> Result<u8, StoreError>;
}

/// Position accounting rule (codex M5 — the ONLY formula, tested for
/// partial/full/insufficient):
///
/// ```text
/// BUY:  shares += q.shares_out; cost += gross
/// SELL: require shares >= s else AppError::InsufficientShares;
///       cost_relieved = floor(cost * s / shares);
///       realized_pnl += net_proceeds - cost_relieved;
///       shares -= s; cost -= cost_relieved
/// ```
#[async_trait]
pub trait PositionWriter: Send {
    /// Row-locked read of one (user, outcome) position.
    ///
    /// # Errors
    /// Backend failures.
    async fn position_for_update(
        &mut self,
        u: UserId,
        o: OutcomeId,
    ) -> Result<Option<PositionRow>, StoreError>;
    /// # Errors
    /// Backend failures.
    async fn save_position(&mut self, p: PositionRow) -> Result<(), StoreError>;
}

#[async_trait]
pub trait TradeEconomyReader: Send {
    /// Locked-transaction reputation read after the class-2 user lock.
    async fn user_rep(&mut self, u: UserId) -> Result<ReputationRow, StoreError>;
    /// Most recent buy timestamp for this position, or `None` if never.
    async fn last_buy_at(
        &mut self,
        u: UserId,
        o: OutcomeId,
    ) -> Result<Option<OffsetDateTime>, StoreError>;
    /// Aggregate open cost basis across both outcomes in one market.
    async fn market_position_cost(
        &mut self,
        u: UserId,
        m: MarketId,
    ) -> Result<MicroUsd, StoreError>;
}

#[async_trait]
pub trait OutboxWriter: Send {
    /// Appends to the transactional outbox: the event commits with the state
    /// change or not at all.
    ///
    /// # Errors
    /// Backend failures.
    async fn append(&mut self, e: Event) -> Result<(), StoreError>;
    /// Set-based outbox append used by settlement regardless of voter count.
    async fn append_batch(&mut self, events: &[Event]) -> Result<(), StoreError>;
}

#[async_trait]
pub trait Committable: Send {
    /// Consumes the transaction; on error nothing became observable.
    ///
    /// # Errors
    /// Backend failures; deferred validation (e.g. the DB balance triggers).
    async fn commit(self: Box<Self>) -> Result<(), StoreError>;
}

/// Role-composition aliases: these exist ONLY as `Store` factory return types
/// with blanket impls (grok-p1r1 M5) — a fake or Pg tx implements the role
/// traits once and gets every alias free. Every write-tx alias includes
/// [`IdempotencyGuard`]; its `serialize_key` call is step 1 of the locking
/// protocol.
pub trait TradeTx:
    IdempotencyGuard
    + UserLockGuard
    + MarketReader
    + VoteReader
    + PoolWriter
    + LedgerWriter
    + TradeWriter
    + UserReader
    + PositionWriter
    + TradeEconomyReader
    + RealizationWriter
    + OutboxWriter
    + crate::ops::config::FencePoint
    + crate::money::CreditIo
    + Committable
{
}
impl<T> TradeTx for T where
    T: IdempotencyGuard
        + UserLockGuard
        + MarketReader
        + VoteReader
        + PoolWriter
        + LedgerWriter
        + TradeWriter
        + UserReader
        + PositionWriter
        + TradeEconomyReader
        + RealizationWriter
        + OutboxWriter
        + crate::ops::config::FencePoint
        + crate::money::CreditIo
        + Committable
{
}

#[async_trait]
pub trait VoteWriter: Send {
    /// Locked counter `UPDATE ... RETURNING`: strictly increasing per market;
    /// the lock is held to transaction end, serializing voters on one market.
    ///
    /// # Errors
    /// Backend failures.
    async fn allocate_vote_seq(&mut self, m: MarketId) -> Result<i64, StoreError>;
    /// Unique on (user, market) and on the idempotency key.
    ///
    /// # Errors
    /// [`StoreError::Conflict`] when the user already voted on the market;
    /// backend failures.
    async fn insert_vote(&mut self, v: NewVote) -> Result<VoteId, StoreError>;
    /// # Errors
    /// Backend failures.
    async fn vote_by_key(
        &mut self,
        key: &str,
        now: OffsetDateTime,
    ) -> Result<Option<VoteReceipt>, StoreError>;
}

pub trait VoteTx:
    IdempotencyGuard
    + UserLockGuard
    + MarketReader
    + VoteReader
    + VoteWriter
    + OutboxWriter
    + LedgerWriter
    + crate::ops::config::FencePoint
    + crate::money::CreditIo
    + Committable
{
}
impl<T> VoteTx for T where
    T: IdempotencyGuard
        + UserLockGuard
        + MarketReader
        + VoteReader
        + VoteWriter
        + OutboxWriter
        + LedgerWriter
        + crate::ops::config::FencePoint
        + crate::money::CreditIo
        + Committable
{
}

#[async_trait]
pub trait SettlementIo: Send {
    /// Every outstanding holding of the market: user positions + POOL
    /// INVENTORY as the pool's own account. `domain::resolution` rejects the
    /// set as `HoldingsIncomplete` unless each side's total equals the minted
    /// sets — omitting the pool is a hard error, never dust.
    ///
    /// # Errors
    /// Backend failures; [`StoreError::Invariant`] for orphaned holdings.
    async fn holdings(&mut self, m: MarketId) -> Result<Vec<Holding>, StoreError>;
    /// Locked read of the market escrow account's actual balance —
    /// `settle_market`'s escrow argument comes from HERE, never recomputed
    /// from assumptions (codex B4).
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when the market has no escrow account.
    async fn escrow_balance(&mut self, m: MarketId) -> Result<MicroUsd, StoreError>;
    /// # Errors
    /// Backend failures.
    async fn write_outcome_resolution(
        &mut self,
        o: OutcomeId,
        final_bps: u16,
        redemption: MicroUsd,
    ) -> Result<(), StoreError>;
    /// Immutable vote facts out; scores computed IN THE USE CASE via
    /// `domain::scoring`, persisted one by one — policy stays in application,
    /// not the adapter (codex M4).
    ///
    /// # Errors
    /// Backend failures.
    async fn vote_facts(&mut self, m: MarketId) -> Result<Vec<VoteFact>, StoreError>;
    /// # Errors
    /// Backend failures; [`StoreError::Invariant`] for an unknown vote.
    async fn save_vote_scores(&mut self, batch: &[VoteScoreUpdate]) -> Result<(), StoreError>;
    /// Immutable voter ids read before the market lock; callers sort them.
    async fn voter_ids(&mut self, m: MarketId) -> Result<Vec<UserId>, StoreError>;
    /// Acquires class-2 advisory locks and row locks in canonical UUID order.
    async fn reps_for_update(
        &mut self,
        users_sorted: &[UserId],
    ) -> Result<Vec<ReputationRow>, StoreError>;
    /// One set-based reputation update.
    async fn save_reps(&mut self, batch: &[ReputationRow]) -> Result<(), StoreError>;
    /// Raw state write — guarded by `domain::market::transition` in the use
    /// case, never called with an unvalidated edge.
    ///
    /// # Errors
    /// Backend failures.
    async fn set_market_state(
        &mut self,
        m: MarketId,
        s: domain::market::MarketState,
    ) -> Result<(), StoreError>;
    /// Money at stake in user positions (cost basis) — the D21 floor
    /// comparison input.
    ///
    /// # Errors
    /// Backend failures.
    async fn open_interest(&mut self, m: MarketId) -> Result<MicroUsd, StoreError>;
    /// Atomically flags a market for curator review. Returns `true` only to
    /// the transaction that changed NULL to a timestamp.
    ///
    /// # Errors
    /// Backend failures.
    async fn flag_curator_needed(&mut self, m: MarketId) -> Result<bool, StoreError>;
    /// Clears a curator flag in the same transaction as settlement.
    ///
    /// # Errors
    /// Backend failures.
    async fn clear_curator_flag(&mut self, m: MarketId) -> Result<(), StoreError>;
    /// Stored sweep authority visible to settlement.
    async fn integrity_report(
        &mut self,
        m: MarketId,
    ) -> Result<Option<IntegrityReportRow>, StoreError>;
    /// Schedules or clears the review deadline in the locked market row.
    async fn set_integrity_due_at(
        &mut self,
        m: MarketId,
        due_at: Option<OffsetDateTime>,
    ) -> Result<(), StoreError>;
    /// Original collateral deposited when the pool was created.
    async fn pool_seeded_micro(&mut self, m: MarketId) -> Result<MicroUsd, StoreError>;
    /// Stores the LP result and the settlement time atomically with payout.
    async fn set_lp_result(
        &mut self,
        m: MarketId,
        pnl: MicroUsd,
        settled_at: OffsetDateTime,
    ) -> Result<(), StoreError>;
    /// Records the escrow balance captured for settlement in the SAME
    /// resolution transaction (D27 identity 5's `collateral_at_close` fact).
    /// REQUIRED — a settlement that skips it is a review-blocker.
    async fn set_collateral_at_close(
        &mut self,
        m: MarketId,
        collateral: MicroUsd,
    ) -> Result<(), StoreError>;
}

#[async_trait]
pub trait RealizationWriter: Send {
    /// Insert-once immutable fact. `false` means the unique replay key already
    /// existed; callers can safely continue the idempotent transaction.
    async fn insert_realization(
        &mut self,
        fact: &crate::model::RealizationFact,
    ) -> Result<bool, StoreError>;
}

pub trait ResolveTx:
    IdempotencyGuard
    + MarketReader
    + VoteReader
    + LedgerWriter
    + SettlementIo
    + PositionWriter
    + RealizationWriter
    + OutboxWriter
    + LifecycleCommandWriter
    + AuditWrite
    + UserLockGuard
    + money::FeeAllocationFinalize
    + money::ReferralPaidFinalize
    + Committable
{
}
impl<T> ResolveTx for T where
    T: IdempotencyGuard
        + MarketReader
        + VoteReader
        + LedgerWriter
        + SettlementIo
        + PositionWriter
        + RealizationWriter
        + OutboxWriter
        + LifecycleCommandWriter
        + AuditWrite
        + UserLockGuard
        + money::FeeAllocationFinalize
        + money::ReferralPaidFinalize
        + Committable
{
}

#[async_trait]
pub trait DepositWriter: Send {
    /// # Errors
    /// Backend failures.
    async fn deposit_by_sig(&mut self, chain_sig: &str) -> Result<Option<DepositId>, StoreError>;
    /// Unique on the chain signature.
    ///
    /// # Errors
    /// [`StoreError::Conflict`] on a duplicate signature; backend failures.
    async fn insert_deposit(&mut self, d: NewDeposit) -> Result<DepositId, StoreError>;
}

pub trait DepositTx:
    IdempotencyGuard
    + UserLockGuard
    + LedgerWriter
    + DepositWriter
    + OutboxWriter
    + ReceivableCollectionIo
    + AuditWrite
    + crate::money::CreditIo
    + Committable
{
}
impl<T> DepositTx for T where
    T: IdempotencyGuard
        + UserLockGuard
        + LedgerWriter
        + DepositWriter
        + OutboxWriter
        + ReceivableCollectionIo
        + AuditWrite
        + crate::money::CreditIo
        + Committable
{
}

#[async_trait]
pub trait MarketWriter: Send {
    /// Inserts the market row (in `Draft`) plus its two outcome rows.
    ///
    /// # Errors
    /// [`StoreError::Conflict`] when the caller-generated id already exists.
    async fn insert_market(&mut self, m: NewMarket) -> Result<MarketId, StoreError>;
    /// Creates the pools row AND the pool's ledger account
    /// ([`OwnerRef::MarketPool`]) — reserves cannot exist without their owner
    /// (codex P1R2 B3).
    ///
    /// # Errors
    /// Backend failures.
    async fn create_pool(
        &mut self,
        m: MarketId,
        fee: BasisPoints,
        seeded: MicroUsd,
    ) -> Result<(), StoreError>;
    /// Raw state write — guarded by `domain::market::transition` in the use
    /// case.
    ///
    /// # Errors
    /// Backend failures.
    async fn set_market_state(
        &mut self,
        m: MarketId,
        s: domain::market::MarketState,
    ) -> Result<(), StoreError>;
}

pub trait SeedTx:
    IdempotencyGuard
    + MarketWriter
    + PoolWriter
    + LedgerWriter
    + SeedEconomyIo
    + OutboxWriter
    + Committable
{
}
impl<T> SeedTx for T where
    T: IdempotencyGuard
        + MarketWriter
        + PoolWriter
        + LedgerWriter
        + SeedEconomyIo
        + OutboxWriter
        + Committable
{
}

#[async_trait]
pub trait SeedEconomyIo: Send {
    /// Market escrow credited by the committed seed transaction for `key`.
    async fn seeded_market_by_key(&mut self, key: &str) -> Result<Option<MarketId>, StoreError>;
    /// Class-3 global advisory lock. Every seed holds it through commit.
    async fn lock_lp_kill_switch(&mut self) -> Result<(), StoreError>;
    /// Sum of settled LP results over the exact half-open window.
    async fn lp_pnl_sum(
        &mut self,
        since: OffsetDateTime,
        until: OffsetDateTime,
    ) -> Result<MicroUsd, StoreError>;
}

#[async_trait]
pub trait UserWriter: Send {
    /// # Errors
    /// Backend failures.
    async fn insert_user(&mut self, handle: &str) -> Result<UserId, StoreError>;
    /// Unique on (channel, address).
    ///
    /// # Errors
    /// [`StoreError::Conflict`] when the address is already linked.
    async fn link_channel(
        &mut self,
        u: UserId,
        channel: &str,
        address: &str,
    ) -> Result<(), StoreError>;
}

pub trait AdvanceTx:
    IdempotencyGuard
    + MarketReader
    + MarketWriter
    + LifecycleCommandWriter
    + OutboxWriter
    + AuditWrite
    + Committable
{
}
impl<T> AdvanceTx for T where
    T: IdempotencyGuard
        + MarketReader
        + MarketWriter
        + LifecycleCommandWriter
        + OutboxWriter
        + AuditWrite
        + Committable
{
}

#[async_trait]
pub trait LifecycleCommandWriter: Send {
    /// Looks up a durable lifecycle receipt under the already-held key lock.
    ///
    /// # Errors
    /// Backend failures.
    async fn lifecycle_command(
        &mut self,
        key: &str,
    ) -> Result<Option<LifecycleCommand>, StoreError>;
    /// Records the transition in the same transaction as state and outbox.
    ///
    /// # Errors
    /// Backend failures or a duplicate invariant violation.
    async fn record_lifecycle_command(
        &mut self,
        key: &str,
        command: LifecycleCommand,
    ) -> Result<(), StoreError>;
}

pub trait BootstrapTx:
    IdempotencyGuard + LedgerWriter + UserWriter + OutboxWriter + OpsWriteIo + AuditWrite + Committable
{
}

#[async_trait]
pub trait IntegritySweepIo: Send {
    /// One set-based aggregation over immutable votes for a resolving market.
    async fn vote_stats(
        &mut self,
        m: MarketId,
        config: IntegritySweepConfig,
    ) -> Result<domain::integrity::VoteStats, StoreError>;
    /// Insert-once report. Returns whether this transaction inserted it.
    async fn insert_integrity_report(
        &mut self,
        report: &IntegrityReportRow,
    ) -> Result<bool, StoreError>;
    /// Reads the converged stored report after insert-on-conflict.
    async fn integrity_report(
        &mut self,
        m: MarketId,
    ) -> Result<Option<IntegrityReportRow>, StoreError>;
}

pub trait IntegrityTx: IdempotencyGuard + MarketReader + IntegritySweepIo + Committable {}
impl<T> IntegrityTx for T where T: IdempotencyGuard + MarketReader + IntegritySweepIo + Committable {}
impl<T> BootstrapTx for T where
    T: IdempotencyGuard + LedgerWriter + UserWriter + OutboxWriter + OpsWriteIo + AuditWrite + Committable
{
}

/// Store views split (codex ckpt 2): lock-free reads for HTTP GET routes and
/// `PreviewTrade` ONLY. A use case that consults these for authorization is a
/// review-blocker (TOCTOU) — write-path checks go through the row-locked
/// reads on the tx roles above. `PgStore` implements both [`Store`] and
/// [`MarketQueries`]; handlers get each by its own bound.
#[async_trait]
pub trait MarketQueries: Send + Sync {
    /// Resolves a market by UUID string or slug.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] for an unknown reference.
    async fn market_by_ref(&self, r: &str) -> Result<MarketRow, StoreError>;
    /// # Errors
    /// Backend failures.
    async fn list_markets(&self, status: Option<&str>) -> Result<Vec<MarketRow>, StoreError>;
    /// # Errors
    /// [`StoreError::NotFound`] when the market has no pool.
    async fn pool(&self, m: MarketId) -> Result<PoolRow, StoreError>;
    /// Returns a clock-consistent public snapshot. Tally visibility is decided
    /// inside the implementation using `now`.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when the market or pool does not exist.
    async fn market_snapshot(
        &self,
        m: MarketId,
        now: OffsetDateTime,
    ) -> Result<MarketSnapshot, StoreError>;
    /// Returns a bounded set of scheduler work due at `now`.
    ///
    /// # Errors
    /// Backend failures.
    async fn due_markets(
        &self,
        now: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<DueMarket>, StoreError>;
    /// Collateral-weighted YES-normalized price history.
    async fn price_history(
        &self,
        m: MarketId,
        bucket_secs: u32,
        since: OffsetDateTime,
    ) -> Result<Vec<PricePoint>, StoreError>;
    /// Public trade tape, newest first.
    async fn tape(&self, m: MarketId, limit: u32) -> Result<Vec<TapeRow>, StoreError>;
    /// # Errors
    /// Backend failures.
    async fn positions(&self, u: UserId) -> Result<Vec<PositionView>, StoreError>;
    /// # Errors
    /// Backend failures.
    async fn user_voted(&self, u: UserId, m: MarketId) -> Result<bool, StoreError>;
    /// # Errors
    /// Backend failures.
    async fn user_by_channel(
        &self,
        channel: &str,
        address: &str,
    ) -> Result<Option<UserId>, StoreError>;
    /// Lock-free preview reputation read.
    async fn user_rep(&self, u: UserId) -> Result<ReputationRow, StoreError>;
    /// Most recent buy timestamp for this position, or `None` if never.
    async fn last_buy_at(
        &self,
        u: UserId,
        o: OutcomeId,
    ) -> Result<Option<OffsetDateTime>, StoreError>;
    /// Admin-only review inbox.
    async fn flagged_markets(&self) -> Result<Vec<FlaggedMarketRow>, StoreError>;
    /// Settled/realized `PnL` in the half-open window — not mark-to-market.
    async fn top_traders(
        &self,
        since: OffsetDateTime,
        until: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<crate::model::TraderRow>, StoreError>;
    /// Average vote score for eligible tiered voters in the half-open window.
    async fn top_voters(
        &self,
        since: OffsetDateTime,
        until: OffsetDateTime,
        limit: u32,
        min_scored: u32,
    ) -> Result<Vec<crate::model::VoterRow>, StoreError>;
    /// Fees-account revenue split by the originating ledger transaction kind.
    async fn fee_summary(
        &self,
        since: OffsetDateTime,
        until: OffsetDateTime,
    ) -> Result<Vec<crate::model::DailyFeeRow>, StoreError>;
}
