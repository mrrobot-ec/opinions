//! Phase 6 ops control-plane roles (plan D24/D26/D27/D29/D30).
//!
//! Task 6.0a owns these SIGNATURES plus behavior-tested Unavailable
//! placeholders (fakes and `PgStore`); waves W1/W2 own the real
//! implementations. There are NO permissive defaults: an ops path invoked
//! before its wave lands fails with the typed
//! [`StoreError::Unavailable`]`("phase6:<area>")`.

use async_trait::async_trait;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::StoreError;
use crate::model::{
    AccountBalanceRow, AdminAction, ConfigChange, ConfigEntry, ConfigProposal, EscrowHistoryRow,
    MarketId, MarketUnwind, PositionRow, ProposalStatus, Receivable, ReceivableMovement,
    ReceivableReconRow, TxnSumRow, UserId, WithdrawalEligibilityView,
};

use super::{
    Committable, IdempotencyGuard, LedgerWriter, MarketReader, OutboxWriter, PositionWriter,
    RealizationWriter, SettlementIo, UserReader,
};

/// Atomic audit writer (D26): every admin mutation's single-transaction
/// effect carries its `audit_insert` in the SAME transaction. Machine actors
/// never call this.
#[async_trait]
pub trait AuditWrite: Send {
    /// # Errors
    /// Backend failures; the phase-6 skeleton returns
    /// [`StoreError::Unavailable`]`("phase6:audit")` until W2 lands.
    async fn audit_insert(&mut self, action: AdminAction) -> Result<(), StoreError>;
}

/// Lock-free, best-effort config read for `PreviewTrade` (D25a): the preview
/// stamps `config_version`; `PlaceTrade` is authoritative at the fence point.
#[async_trait]
pub trait ConfigReads: Send + Sync {
    /// # Errors
    /// Backend failures.
    async fn current_generation(&self) -> Result<i64, StoreError>;

    /// Best-effort snapshot value for lock-free readers (the D25 preview
    /// pause overlay). Defaults to `None`; the W1 watch snapshot overrides.
    async fn snapshot_value(&self, _key: &str) -> Result<Option<serde_json::Value>, StoreError> {
        Ok(None)
    }
}

/// Pre-wave [`ConfigReads`]: a fixed generation (production default 1 — the
/// migration-0008 seed) until the W1 reconciler maintains a watch snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticConfigReads(pub i64);

impl Default for StaticConfigReads {
    fn default() -> Self {
        Self(1)
    }
}

#[async_trait]
impl ConfigReads for StaticConfigReads {
    async fn current_generation(&self) -> Result<i64, StoreError> {
        Ok(self.0)
    }
}

/// Generation-serialized config write transaction (D24): every writer locks
/// the singleton generation row FIRST among config rows (after the proposal
/// row when one is involved), which serializes commit order.
#[async_trait]
pub trait OpsConfigTx:
    AuditWrite + OutboxWriter + crate::ops::config::OpsWriteSupport + Committable
{
    /// Locks the singleton `config_generation` row and returns the current
    /// generation. Writer B blocks behind writer A here.
    async fn lock_generation(&mut self) -> Result<i64, StoreError>;
    /// The complete committed entry set (the prospective-snapshot input).
    async fn config_entries(&mut self) -> Result<Vec<ConfigEntry>, StoreError>;
    /// Applies one patch as `generation`: header row, one change row PER
    /// CHANGED KEY, entry upserts, and the generation-row bump — all
    /// buffered into this transaction.
    async fn apply_changes(
        &mut self,
        generation: i64,
        changes: &[ConfigChange],
        applied_by: &str,
    ) -> Result<(), StoreError>;
    /// Change rows for `key` after `generation`, oldest first (the D25a
    /// market/user-relevant staleness probe rides the `(key, generation)`
    /// index).
    async fn changes_for_key_since(
        &mut self,
        key: &str,
        generation: i64,
    ) -> Result<Vec<ConfigChange>, StoreError>;
    /// Inserts a pending proposal row (unique idempotency key).
    async fn insert_proposal(&mut self, proposal: ConfigProposal) -> Result<(), StoreError>;
    /// Row-locked proposal read for confirm/reject/expire.
    async fn proposal_for_update(&mut self, id: uuid::Uuid) -> Result<ConfigProposal, StoreError>;
    /// Persists a settled proposal (status, confirmer, resulting generation).
    async fn save_proposal(&mut self, proposal: &ConfigProposal) -> Result<(), StoreError>;
}

/// Thin audit-only transaction (D26) for admin mutations whose entire effect
/// IS the audit fact plus out-of-band work (e.g. replay-job authorization).
pub trait OpsAuditTx: AuditWrite + Committable {}
impl<T> OpsAuditTx for T where T: AuditWrite + Committable {}

/// One `REPEATABLE READ, READ ONLY` snapshot over the six ledger identities
/// plus the receivables reconciliation (D27). Dropping the value closes the
/// snapshot; there is deliberately no commit.
#[async_trait]
pub trait InvariantReadTx: Send {
    /// The snapshot's as-of marker.
    async fn as_of(&mut self) -> Result<OffsetDateTime, StoreError>;
    /// Identity 1: transactions whose entries do NOT sum to zero (must be
    /// empty).
    async fn unbalanced_txns(&mut self) -> Result<Vec<TxnSumRow>, StoreError>;
    /// Identities 2–3: every account balance with its owner class (cash
    /// only; receivables are NOT a ledger account class).
    async fn account_balances(&mut self) -> Result<Vec<AccountBalanceRow>, StoreError>;
    /// Identity 4: payment facts without exactly one external leg, or vice
    /// versa (must be empty).
    async fn unpaired_payment_facts(&mut self) -> Result<Vec<uuid::Uuid>, StoreError>;
    /// Identity 5: per-market escrow history residuals with the
    /// `collateral_at_close` fact.
    async fn escrow_history(&mut self) -> Result<Vec<EscrowHistoryRow>, StoreError>;
    /// Identity 6: duplicate terminal job effects, named (must be empty).
    async fn duplicate_job_effects(&mut self) -> Result<Vec<String>, StoreError>;
    /// Identity 7: receivables reconciliation per origin reversal
    /// transaction.
    async fn receivable_reconciliation(&mut self) -> Result<Vec<ReceivableReconRow>, StoreError>;
    /// `DepositSuspense` Σ (observation liabilities).
    async fn suspense_liability_micro(&mut self) -> Result<i64, StoreError> {
        let _ = self;
        Ok(0)
    }
    /// `(BonusReserve balance, unconverted real_money promise)`.
    async fn bonus_reserve_and_promise(&mut self) -> Result<(i64, i64), StoreError> {
        let _ = self;
        Ok((0, 0))
    }
    /// D31 identities (a)–(e): one row per withdrawal with its hold/terminal
    /// attribution. Default = empty so pre-Phase-7 snapshots stay valid; the
    /// Pg adapter overrides with the real projection.
    async fn withdrawal_attribution(
        &mut self,
    ) -> Result<Vec<crate::model::WithdrawalAttributionRow>, StoreError> {
        let _ = self;
        Ok(Vec::new())
    }
}

// ---------------------------------------------------------------------------
// W2 role surface: unwind + receivables + manual-ops writes (D30/D26)
// ---------------------------------------------------------------------------

/// One original ledger transaction of a market, with owner-resolved entries —
/// the reversal enumeration input (D30). A transaction belongs to a market
/// when any entry touches the market's escrow or pool account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerTxnFacts {
    pub txn: Uuid,
    pub kind: domain::ledger::TxnKind,
    pub entries: Vec<LedgerEntryFacts>,
}

/// One owner-resolved entry inside [`LedgerTxnFacts`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerEntryFacts {
    pub account: domain::ledger::AccountId,
    pub owner: crate::model::OwnerRef,
    pub amount_micro: i64,
}

/// A receivable with its DERIVED outstanding (opened − collected −
/// `written_off`) — never a stored balance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceivableOutstanding {
    pub receivable: Receivable,
    pub outstanding_micro: i64,
}

/// Write-off proposal row (D30 / codex r4 NEW-2): a single-principal economic
/// transfer carries the full unwind-grade dual-control protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteOffProposal {
    pub id: Uuid,
    pub receivable: Uuid,
    pub idempotency_key: String,
    pub amount_micro: i64,
    pub proposer_token_id: String,
    pub confirmer_token_id: Option<String>,
    pub reason: String,
    pub status: ProposalStatus,
    pub confirm_not_before: OffsetDateTime,
}

/// Remedial credit proposal row (grok r2 N2): unwind-grade dual control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemedialCreditProposal {
    pub id: Uuid,
    pub market: MarketId,
    pub user: UserId,
    pub idempotency_key: String,
    pub amount_micro: i64,
    pub proposer_token_id: String,
    pub confirmer_token_id: Option<String>,
    pub reason: String,
    pub status: ProposalStatus,
    pub confirm_not_before: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpsJobStatus {
    Pending,
    Executing,
    Done,
    Failed,
}

/// Durable idempotent manual-ops command (D26 replay-job / re-run-fanout):
/// the audited fact is the authorization; leased runners deliver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpsJobCommand {
    pub id: Uuid,
    /// `replay_job` | `refanout`.
    pub kind: String,
    /// Subject reference, e.g. `market:<uuid>` or `user:<uuid>`.
    pub subject: String,
    pub idempotency_key: String,
    pub requested_by: String,
    pub status: OpsJobStatus,
    pub attempts: i32,
    pub lease_expires_at: Option<OffsetDateTime>,
    pub error: Option<String>,
}

/// One redacted audit row (`audit-read` capability; digests only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditPageRow {
    pub id: Uuid,
    pub action: AdminAction,
    pub at: OffsetDateTime,
}

/// Receivable collection surface (D30 / grok r3 NEW-5): deposits auto-collect
/// `min(cash_after_deposit, Σ open receivables)` in the SAME transaction, so
/// this role rides the deposit transaction as well as the unwind one.
#[async_trait]
pub trait ReceivableCollectionIo: Send {
    /// Open receivables (outstanding > 0) for one user, oldest first.
    async fn open_receivables_for_user(
        &mut self,
        user: UserId,
    ) -> Result<Vec<ReceivableOutstanding>, StoreError>;
    /// Appends one movement row (unique idempotency key); outstanding stays
    /// derived, never overwritten.
    async fn insert_receivable_movement(
        &mut self,
        movement: ReceivableMovement,
    ) -> Result<(), StoreError>;
    /// Committed-plus-buffered balance of one account as this transaction
    /// sees it.
    async fn account_balance(
        &mut self,
        account: domain::ledger::AccountId,
    ) -> Result<domain::money::MicroUsd, StoreError>;
}

/// Unwind + dual-control + manual-ops write surface (D30/D26). One role
/// trait so `InMemTx`/`PgTx` implement it once.
#[async_trait]
pub trait OpsWriteIo: Send {
    // -- unwind authority (one row per market) --
    async fn unwind_for_update(
        &mut self,
        market: MarketId,
    ) -> Result<Option<MarketUnwind>, StoreError>;
    async fn insert_unwind(&mut self, unwind: MarketUnwind) -> Result<(), StoreError>;
    async fn save_unwind(&mut self, unwind: &MarketUnwind) -> Result<(), StoreError>;
    /// Records reversal lineage: an original ledger transaction reverses AT
    /// MOST once (DB-unique `reversed_entry_id`).
    async fn record_reversal(
        &mut self,
        reversed_entry: Uuid,
        reversal_txn: Uuid,
        market: MarketId,
    ) -> Result<(), StoreError>;
    /// Opens a receivable (unique per user+market) in the non-cash subledger.
    async fn insert_receivable(&mut self, receivable: Receivable) -> Result<(), StoreError>;
    /// Σ outstanding across ALL open receivables — the house cap gate (past
    /// the named threshold new unwind confirms 422).
    async fn open_receivables_total(&mut self) -> Result<i64, StoreError>;
    /// Every ledger transaction touching this market's escrow or pool
    /// account, with owner-resolved entries (reversal enumeration input).
    async fn market_ledger_txns(
        &mut self,
        market: MarketId,
    ) -> Result<Vec<LedgerTxnFacts>, StoreError>;
    /// Every position row in this market (both outcomes) for zeroing.
    async fn positions_for_market(
        &mut self,
        market: MarketId,
    ) -> Result<Vec<PositionRow>, StoreError>;
    // -- receivable write-off dual control (codex r4 NEW-2) --
    async fn receivable_by_id(
        &mut self,
        id: Uuid,
    ) -> Result<Option<ReceivableOutstanding>, StoreError>;
    async fn write_off_for_update(
        &mut self,
        id: Uuid,
    ) -> Result<Option<WriteOffProposal>, StoreError>;
    async fn write_off_by_key(&mut self, key: &str)
        -> Result<Option<WriteOffProposal>, StoreError>;
    async fn insert_write_off(&mut self, proposal: WriteOffProposal) -> Result<(), StoreError>;
    async fn save_write_off(&mut self, proposal: &WriteOffProposal) -> Result<(), StoreError>;
    /// Σ written-off since `since` (the daily cap input).
    async fn written_off_since(&mut self, since: OffsetDateTime) -> Result<i64, StoreError>;
    // -- remedial credit dual control (grok r2 N2) --
    async fn remedial_for_update(
        &mut self,
        id: Uuid,
    ) -> Result<Option<RemedialCreditProposal>, StoreError>;
    async fn remedial_by_key(
        &mut self,
        key: &str,
    ) -> Result<Option<RemedialCreditProposal>, StoreError>;
    async fn insert_remedial(&mut self, proposal: RemedialCreditProposal)
        -> Result<(), StoreError>;
    async fn save_remedial(&mut self, proposal: &RemedialCreditProposal) -> Result<(), StoreError>;
    /// Σ confirmed remedial credits for one market (per-market cap input).
    async fn remedial_credited_for_market(&mut self, market: MarketId) -> Result<i64, StoreError>;
    /// Σ confirmed remedial credits since `since` (daily house cap input).
    async fn remedial_credited_since(&mut self, since: OffsetDateTime) -> Result<i64, StoreError>;
    // -- durable manual-ops commands (D26) --
    async fn insert_job_command(&mut self, command: OpsJobCommand) -> Result<(), StoreError>;
    async fn job_command_by_key(&mut self, key: &str) -> Result<Option<OpsJobCommand>, StoreError>;
    /// Pending commands plus executing ones whose lease lapsed, oldest first;
    /// claiming stamps `Executing` + the new lease in this transaction.
    async fn due_job_commands(
        &mut self,
        now: OffsetDateTime,
        lease_until: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<OpsJobCommand>, StoreError>;
    async fn save_job_command(&mut self, command: &OpsJobCommand) -> Result<(), StoreError>;
    // -- signup overrides (D28 identity policy; staging arm; audited) --
    async fn set_user_created_at(
        &mut self,
        user: UserId,
        at: OffsetDateTime,
    ) -> Result<(), StoreError>;
    /// Tier is computed by the use case (policy stays in application).
    async fn seed_reputation(
        &mut self,
        user: UserId,
        rep_micro: i64,
        tier: u8,
    ) -> Result<(), StoreError>;
}

/// Dual-controlled unwind + receivables transaction (D30). The reversal is
/// ONE transaction regardless of participant count (Phase 3 precedent).
pub trait UnwindTx:
    IdempotencyGuard
    + MarketReader
    + UserReader
    + LedgerWriter
    + SettlementIo
    + PositionWriter
    + RealizationWriter
    + OutboxWriter
    + OpsWriteIo
    + ReceivableCollectionIo
    + AuditWrite
    + crate::ports::UserLockGuard
    + crate::ports::FeeAllocationReverse
    + Committable
{
}
impl<T> UnwindTx for T where
    T: IdempotencyGuard
        + MarketReader
        + UserReader
        + LedgerWriter
        + SettlementIo
        + PositionWriter
        + RealizationWriter
        + OutboxWriter
        + OpsWriteIo
        + ReceivableCollectionIo
        + AuditWrite
        + crate::ports::UserLockGuard
        + crate::ports::FeeAllocationReverse
        + Committable
{
}

/// Read-only withdrawal guard (D30 / grok r3 NEW-5): no withdrawal use case
/// exists until Phase 7 rails; this role is the guard those rails will call,
/// exposed read-only at `GET /admin/users/{id}/withdrawal_eligibility`.
#[async_trait]
pub trait WithdrawalEligibility: Send + Sync {
    /// # Errors
    /// Backend failures; unknown users are `NotFound`.
    async fn withdrawal_eligibility(
        &self,
        user: UserId,
    ) -> Result<WithdrawalEligibilityView, StoreError>;
}

/// Lock-free admin-plane reads (D26): the audit page and the publish-status
/// probe. Kept to reads that never authorize writes (TOCTOU rule).
#[async_trait]
pub trait OpsQueries: WithdrawalEligibility + Send + Sync {
    /// Redacted audit rows strictly before `before` (None = newest), newest
    /// first, at most `limit`.
    async fn audit_page(
        &self,
        before: Option<OffsetDateTime>,
        limit: u32,
    ) -> Result<Vec<AuditPageRow>, StoreError>;
    /// Latest publication command for one draft (the 202 status URL target).
    async fn publication_command_for_draft(
        &self,
        draft: crate::model::DraftId,
    ) -> Result<Option<super::PublicationCommand>, StoreError>;
}

/// Injected resolution crash point (D29): called EXACTLY ONCE per
/// resolution, after the payout `ledger_apply` and before any subsequent
/// write. The production impl is [`NoopCrashPoint`]; W4 owns the staging
/// adapter that publishes readiness and awaits SIGKILL under the two-factor
/// chaos arm.
#[async_trait]
pub trait ResolutionCrashPoint: Send + Sync {
    /// # Errors
    /// Backend failures from an armed adapter (e.g. the readiness touch).
    async fn fire(&self) -> Result<(), StoreError>;
}

/// Production crash point: does nothing, fails never.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoopCrashPoint;

#[async_trait]
impl ResolutionCrashPoint for NoopCrashPoint {
    async fn fire(&self) -> Result<(), StoreError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    struct BareInvariantReadTx;

    #[async_trait::async_trait]
    impl InvariantReadTx for BareInvariantReadTx {
        async fn as_of(&mut self) -> Result<OffsetDateTime, StoreError> {
            Ok(OffsetDateTime::UNIX_EPOCH)
        }

        async fn unbalanced_txns(&mut self) -> Result<Vec<TxnSumRow>, StoreError> {
            Ok(Vec::new())
        }

        async fn account_balances(&mut self) -> Result<Vec<AccountBalanceRow>, StoreError> {
            Ok(Vec::new())
        }

        async fn unpaired_payment_facts(&mut self) -> Result<Vec<uuid::Uuid>, StoreError> {
            Ok(Vec::new())
        }

        async fn escrow_history(&mut self) -> Result<Vec<EscrowHistoryRow>, StoreError> {
            Ok(Vec::new())
        }

        async fn duplicate_job_effects(&mut self) -> Result<Vec<String>, StoreError> {
            Ok(Vec::new())
        }

        async fn receivable_reconciliation(
            &mut self,
        ) -> Result<Vec<ReceivableReconRow>, StoreError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn static_config_reads_default_to_the_migration_seed_generation() {
        assert_eq!(
            StaticConfigReads::default().current_generation().await,
            Ok(1)
        );
        assert_eq!(StaticConfigReads(7).current_generation().await, Ok(7));
    }

    #[tokio::test]
    async fn the_production_crash_point_is_transparent() {
        assert_eq!(NoopCrashPoint.fire().await, Ok(()));
        assert_eq!(<NoopCrashPoint as Default>::default(), NoopCrashPoint);
    }

    #[tokio::test]
    async fn bare_invariant_snapshots_keep_phase7_dimensions_neutral() {
        let mut snapshot = BareInvariantReadTx;
        assert_eq!(snapshot.as_of().await.unwrap(), OffsetDateTime::UNIX_EPOCH);
        assert!(snapshot.unbalanced_txns().await.unwrap().is_empty());
        assert!(snapshot.account_balances().await.unwrap().is_empty());
        assert!(snapshot.unpaired_payment_facts().await.unwrap().is_empty());
        assert!(snapshot.escrow_history().await.unwrap().is_empty());
        assert!(snapshot.duplicate_job_effects().await.unwrap().is_empty());
        assert!(snapshot
            .receivable_reconciliation()
            .await
            .unwrap()
            .is_empty());
        assert_eq!(snapshot.suspense_liability_micro().await.unwrap(), 0);
        assert_eq!(snapshot.bonus_reserve_and_promise().await.unwrap(), (0, 0));
        assert!(snapshot.withdrawal_attribution().await.unwrap().is_empty());
    }
}
