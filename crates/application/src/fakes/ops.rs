//! Phase 6 ops fakes (plan §2, wave W2): real in-memory implementations of
//! the audit / unwind / receivables / manual-ops roles, honoring the shared
//! fake semantics — row-style locks held to transaction end, buffered writes
//! observable only at commit, atomicity on failure.
//!
//! W1's config area lives in `fakes/ops_config.rs`.

use async_trait::async_trait;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::StoreError;
use crate::model::{
    AccountBalanceRow, AdminAction, DraftId, EscrowHistoryRow, MarketId, MarketUnwind, PositionRow,
    Receivable, ReceivableMovement, ReceivableMovementKind, ReceivableReconRow, TxnSumRow, UserId,
    WithdrawalEligibilityView,
};
use crate::ports::PublicationCommand;
use crate::ports::{
    AuditPageRow, InvariantReadTx, LedgerEntryFacts, LedgerTxnFacts, OpsJobCommand, OpsQueries,
    OpsWriteIo, ReceivableCollectionIo, ReceivableOutstanding, RemedialCreditProposal,
    WriteOffProposal,
};

use super::{owner_type, InMemTx, InMemoryStore, State};

/// Retired 6.0a fail-closed placeholders. The frozen `fakes/mod.rs` re-export
/// keeps the names public; nothing constructs them since the real
/// implementations landed with W2.
pub struct UnavailableOpsAuditTx;
pub struct UnavailableInvariantReadTx;
pub struct UnavailableUnwindTx;

// ---------------------------------------------------------------------------
// Phase 6 committed state + pending buffers
// ---------------------------------------------------------------------------

/// One committed audit row: insertion sequence is the authoritative order.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct StoredAdminAction {
    pub(super) id: Uuid,
    pub(super) seq: i64,
    pub(super) action: AdminAction,
    pub(super) at: OffsetDateTime,
}

/// Committed phase-6 ops state, living inside the shared [`State`] so every
/// write applies under the same single lock as the rest of the fake.
#[derive(Default, Clone)]
pub(super) struct Phase6Committed {
    pub(super) audit_seq: i64,
    pub(super) admin_actions: Vec<StoredAdminAction>,
    pub(super) unwinds: std::collections::BTreeMap<Uuid, MarketUnwind>,
    pub(super) unwind_keys: std::collections::BTreeSet<String>,
    /// reversed original txn → (reversal txn, market).
    pub(super) reversals: std::collections::BTreeMap<Uuid, (Uuid, Uuid)>,
    pub(super) receivables: std::collections::BTreeMap<Uuid, Receivable>,
    /// Receivable ids in commit/insertion order, mirroring Pg `(created_at,id)`.
    pub(super) receivable_order: Vec<Uuid>,
    pub(super) movements: Vec<ReceivableMovement>,
    pub(super) movement_keys: std::collections::BTreeSet<String>,
    pub(super) write_offs: std::collections::BTreeMap<Uuid, WriteOffProposal>,
    pub(super) write_off_keys: std::collections::BTreeMap<String, Uuid>,
    pub(super) remedials: std::collections::BTreeMap<Uuid, RemedialCreditProposal>,
    pub(super) remedial_keys: std::collections::BTreeMap<String, Uuid>,
    pub(super) job_commands: std::collections::BTreeMap<Uuid, OpsJobCommand>,
    pub(super) job_command_keys: std::collections::BTreeMap<String, Uuid>,
    pub(super) publication_commands: std::collections::BTreeMap<Uuid, PublicationCommand>,
    pub(super) publication_command_keys: std::collections::BTreeMap<String, Uuid>,
}

impl Phase6Committed {
    pub(super) fn outstanding(&self, receivable: Uuid) -> i64 {
        self.movements
            .iter()
            .filter(|m| m.receivable == receivable)
            .fold(0_i64, |sum, m| match m.kind {
                ReceivableMovementKind::Opened => sum.saturating_add(m.amount_micro),
                ReceivableMovementKind::Collected | ReceivableMovementKind::WrittenOff => {
                    sum.saturating_sub(m.amount_micro)
                }
            })
    }

    pub(super) fn open_command_for_draft(&self, draft: DraftId) -> Option<&PublicationCommand> {
        use crate::ports::PublicationCommandStatus as S;
        self.publication_commands
            .values()
            .find(|c| c.draft == draft && matches!(c.status, S::Pending | S::Executing))
    }
}

/// Buffered phase-6 writes for one in-flight transaction.
#[derive(Default)]
pub(super) struct Phase6Pending {
    pub(super) audits: Vec<AdminAction>,
    pub(super) unwind_inserts: Vec<MarketUnwind>,
    pub(super) unwind_saves: Vec<MarketUnwind>,
    pub(super) reversals: Vec<(Uuid, Uuid, Uuid)>,
    pub(super) receivable_inserts: Vec<Receivable>,
    pub(super) movement_inserts: Vec<ReceivableMovement>,
    pub(super) write_off_inserts: Vec<WriteOffProposal>,
    pub(super) write_off_saves: Vec<WriteOffProposal>,
    pub(super) remedial_inserts: Vec<RemedialCreditProposal>,
    pub(super) remedial_saves: Vec<RemedialCreditProposal>,
    pub(super) job_inserts: Vec<OpsJobCommand>,
    pub(super) job_saves: Vec<OpsJobCommand>,
    pub(super) publication_inserts: Vec<PublicationCommand>,
    pub(super) publication_saves: Vec<PublicationCommand>,
}

/// Applies one transaction's phase-6 buffers to the staged `next` state —
/// called from the frozen `InMemTx::commit` under the single state lock, so
/// the whole commit stays atomic.
#[allow(clippy::too_many_lines)]
pub(super) fn apply_phase6(next: &mut State, pending: &Phase6Pending) -> Result<(), StoreError> {
    let now = OffsetDateTime::now_utc();
    for action in &pending.audits {
        let seq = next.phase6.audit_seq.saturating_add(1);
        next.phase6.audit_seq = seq;
        next.phase6.admin_actions.push(StoredAdminAction {
            id: Uuid::new_v4(),
            seq,
            action: action.clone(),
            at: now,
        });
    }
    for unwind in &pending.unwind_inserts {
        if next.phase6.unwinds.contains_key(&unwind.market.0) {
            return Err(StoreError::Conflict("market unwind"));
        }
        if !next.phase6.unwind_keys.insert(unwind.unwind_key.clone()) {
            return Err(StoreError::Conflict("unwind key"));
        }
        next.phase6.unwinds.insert(unwind.market.0, unwind.clone());
    }
    for unwind in &pending.unwind_saves {
        let row = next
            .phase6
            .unwinds
            .get_mut(&unwind.market.0)
            .ok_or(StoreError::Invariant("save for unknown unwind"))?;
        *row = unwind.clone();
    }
    for (reversed, reversal, market) in &pending.reversals {
        if next
            .phase6
            .reversals
            .insert(*reversed, (*reversal, *market))
            .is_some()
        {
            return Err(StoreError::Conflict("reversal lineage"));
        }
    }
    for receivable in &pending.receivable_inserts {
        let duplicate = next.phase6.receivables.values().any(|existing| {
            existing.user == receivable.user && existing.market == receivable.market
        });
        if duplicate || next.phase6.receivables.contains_key(&receivable.id) {
            return Err(StoreError::Conflict("receivable"));
        }
        next.phase6.receivable_order.push(receivable.id);
        next.phase6.receivables.insert(receivable.id, *receivable);
    }
    for movement in &pending.movement_inserts {
        if movement.amount_micro <= 0 {
            return Err(StoreError::Invariant("non-positive receivable movement"));
        }
        if !next.phase6.receivables.contains_key(&movement.receivable) {
            return Err(StoreError::Invariant("movement for unknown receivable"));
        }
        if !next
            .phase6
            .movement_keys
            .insert(movement.idempotency_key.clone())
        {
            return Err(StoreError::Conflict("receivable movement key"));
        }
        next.phase6.movements.push(movement.clone());
    }
    for proposal in &pending.write_off_inserts {
        if next.phase6.write_offs.contains_key(&proposal.id)
            || next
                .phase6
                .write_off_keys
                .contains_key(&proposal.idempotency_key)
        {
            return Err(StoreError::Conflict("write-off proposal"));
        }
        next.phase6
            .write_off_keys
            .insert(proposal.idempotency_key.clone(), proposal.id);
        next.phase6.write_offs.insert(proposal.id, proposal.clone());
    }
    for proposal in &pending.write_off_saves {
        let row = next
            .phase6
            .write_offs
            .get_mut(&proposal.id)
            .ok_or(StoreError::Invariant("save for unknown write-off"))?;
        *row = proposal.clone();
    }
    for proposal in &pending.remedial_inserts {
        if next.phase6.remedials.contains_key(&proposal.id)
            || next
                .phase6
                .remedial_keys
                .contains_key(&proposal.idempotency_key)
        {
            return Err(StoreError::Conflict("remedial proposal"));
        }
        next.phase6
            .remedial_keys
            .insert(proposal.idempotency_key.clone(), proposal.id);
        next.phase6.remedials.insert(proposal.id, proposal.clone());
    }
    for proposal in &pending.remedial_saves {
        let row = next
            .phase6
            .remedials
            .get_mut(&proposal.id)
            .ok_or(StoreError::Invariant("save for unknown remedial"))?;
        *row = proposal.clone();
    }
    for command in &pending.job_inserts {
        if next.phase6.job_commands.contains_key(&command.id)
            || next
                .phase6
                .job_command_keys
                .contains_key(&command.idempotency_key)
        {
            return Err(StoreError::Conflict("ops job command"));
        }
        next.phase6
            .job_command_keys
            .insert(command.idempotency_key.clone(), command.id);
        next.phase6.job_commands.insert(command.id, command.clone());
    }
    for command in &pending.job_saves {
        let row = next
            .phase6
            .job_commands
            .get_mut(&command.id)
            .ok_or(StoreError::Invariant("save for unknown job command"))?;
        *row = command.clone();
    }
    for command in &pending.publication_inserts {
        if next.phase6.publication_commands.contains_key(&command.id)
            || next
                .phase6
                .publication_command_keys
                .contains_key(&command.idempotency_key)
        {
            return Err(StoreError::Conflict("publication command key"));
        }
        if next.phase6.open_command_for_draft(command.draft).is_some() {
            return Err(StoreError::Conflict("publication command"));
        }
        next.phase6
            .publication_command_keys
            .insert(command.idempotency_key.clone(), command.id);
        next.phase6
            .publication_commands
            .insert(command.id, command.clone());
    }
    for command in &pending.publication_saves {
        let row = next
            .phase6
            .publication_commands
            .get_mut(&command.id)
            .ok_or(StoreError::Invariant(
                "save for unknown publication command",
            ))?;
        *row = command.clone();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// AuditWrite on the shared write transaction
// ---------------------------------------------------------------------------

/// The real D26 audit sink for the shared write transaction: buffered, then
/// applied in the same atomic commit as the mutation it describes.
pub(super) fn tx_audit_insert(tx: &mut InMemTx, action: AdminAction) -> Result<(), StoreError> {
    if action.actor_token_digest.is_empty() || action.action.is_empty() {
        return Err(StoreError::Invariant("audit row missing actor or action"));
    }
    tx.pending.phase6.audits.push(action);
    Ok(())
}

// ---------------------------------------------------------------------------
// ReceivableCollectionIo + OpsWriteIo for the shared write transaction
// ---------------------------------------------------------------------------

impl InMemTx {
    fn phase6_view<R>(&self, read: impl FnOnce(&Phase6Committed) -> R) -> R {
        let st = self.shared.state.lock();
        read(&st.phase6)
    }

    /// Outstanding for one receivable as this tx sees it (committed plus
    /// buffered movements).
    fn outstanding_view(&self, committed: &Phase6Committed, receivable: Uuid) -> i64 {
        let buffered = self
            .pending
            .phase6
            .movement_inserts
            .iter()
            .filter(|m| m.receivable == receivable)
            .fold(0_i64, |sum, m| match m.kind {
                ReceivableMovementKind::Opened => sum.saturating_add(m.amount_micro),
                ReceivableMovementKind::Collected | ReceivableMovementKind::WrittenOff => {
                    sum.saturating_sub(m.amount_micro)
                }
            });
        committed.outstanding(receivable).saturating_add(buffered)
    }

    fn receivable_view(&self, committed: &Phase6Committed, id: Uuid) -> Option<Receivable> {
        committed.receivables.get(&id).copied().or_else(|| {
            self.pending
                .phase6
                .receivable_inserts
                .iter()
                .find(|r| r.id == id)
                .copied()
        })
    }
}

#[async_trait]
impl ReceivableCollectionIo for InMemTx {
    async fn open_receivables_for_user(
        &mut self,
        user: UserId,
    ) -> Result<Vec<ReceivableOutstanding>, StoreError> {
        let st = self.shared.state.lock();
        let rows: Vec<ReceivableOutstanding> = st
            .phase6
            .receivable_order
            .iter()
            .filter_map(|id| st.phase6.receivables.get(id).copied())
            .chain(self.pending.phase6.receivable_inserts.iter().copied())
            .filter(|r| r.user == user)
            .map(|receivable| ReceivableOutstanding {
                receivable,
                outstanding_micro: self.outstanding_view(&st.phase6, receivable.id),
            })
            .filter(|row| row.outstanding_micro > 0)
            .collect();
        Ok(rows)
    }

    async fn insert_receivable_movement(
        &mut self,
        movement: ReceivableMovement,
    ) -> Result<(), StoreError> {
        if movement.amount_micro <= 0 {
            return Err(StoreError::Invariant("non-positive receivable movement"));
        }
        let key_taken = self.phase6_view(|p6| p6.movement_keys.contains(&movement.idempotency_key))
            || self
                .pending
                .phase6
                .movement_inserts
                .iter()
                .any(|m| m.idempotency_key == movement.idempotency_key);
        if key_taken {
            return Err(StoreError::Conflict("receivable movement key"));
        }
        self.pending.phase6.movement_inserts.push(movement);
        Ok(())
    }

    async fn account_balance(
        &mut self,
        account: domain::ledger::AccountId,
    ) -> Result<domain::money::MicroUsd, StoreError> {
        let committed = self
            .shared
            .state
            .lock()
            .balances
            .balance(account)
            .ok_or(StoreError::NotFound("account"))?;
        let buffered: i64 = self
            .pending
            .txns
            .iter()
            .flat_map(|t| t.txn.entries())
            .filter(|e| e.account == account)
            .map(|e| e.amount.0)
            .sum();
        Ok(domain::money::MicroUsd(
            committed.0.saturating_add(buffered),
        ))
    }
}

#[async_trait]
#[allow(clippy::too_many_lines)]
impl OpsWriteIo for InMemTx {
    async fn unwind_for_update(
        &mut self,
        market: MarketId,
    ) -> Result<Option<MarketUnwind>, StoreError> {
        // Serialize on the market row like other authority reads.
        if !self.market_guards.contains_key(&market.0) {
            let handle = self.shared.market_locks.handle(&market.0);
            let guard = handle.lock_owned().await;
            self.market_guards.insert(market.0, guard);
        }
        let pending = self
            .pending
            .phase6
            .unwind_saves
            .iter()
            .rev()
            .chain(self.pending.phase6.unwind_inserts.iter().rev())
            .find(|u| u.market == market)
            .cloned();
        if pending.is_some() {
            return Ok(pending);
        }
        Ok(self.phase6_view(|p6| p6.unwinds.get(&market.0).cloned()))
    }

    async fn insert_unwind(&mut self, unwind: MarketUnwind) -> Result<(), StoreError> {
        let exists = self.phase6_view(|p6| {
            p6.unwinds.contains_key(&unwind.market.0) || p6.unwind_keys.contains(&unwind.unwind_key)
        }) || self
            .pending
            .phase6
            .unwind_inserts
            .iter()
            .any(|u| u.market == unwind.market || u.unwind_key == unwind.unwind_key);
        if exists {
            return Err(StoreError::Conflict("market unwind"));
        }
        self.pending.phase6.unwind_inserts.push(unwind);
        Ok(())
    }

    async fn save_unwind(&mut self, unwind: &MarketUnwind) -> Result<(), StoreError> {
        self.pending.phase6.unwind_saves.push(unwind.clone());
        Ok(())
    }

    async fn record_reversal(
        &mut self,
        reversed_entry: Uuid,
        reversal_txn: Uuid,
        market: MarketId,
    ) -> Result<(), StoreError> {
        let taken = self.phase6_view(|p6| p6.reversals.contains_key(&reversed_entry))
            || self
                .pending
                .phase6
                .reversals
                .iter()
                .any(|(reversed, _, _)| *reversed == reversed_entry);
        if taken {
            return Err(StoreError::Conflict("reversal lineage"));
        }
        self.pending
            .phase6
            .reversals
            .push((reversed_entry, reversal_txn, market.0));
        Ok(())
    }

    async fn insert_receivable(&mut self, receivable: Receivable) -> Result<(), StoreError> {
        if receivable.opened_micro <= 0 {
            return Err(StoreError::Invariant("non-positive receivable"));
        }
        let duplicate = self.phase6_view(|p6| {
            p6.receivables
                .values()
                .any(|r| r.user == receivable.user && r.market == receivable.market)
        }) || self
            .pending
            .phase6
            .receivable_inserts
            .iter()
            .any(|r| r.user == receivable.user && r.market == receivable.market);
        if duplicate {
            return Err(StoreError::Conflict("receivable"));
        }
        self.pending.phase6.receivable_inserts.push(receivable);
        Ok(())
    }

    async fn open_receivables_total(&mut self) -> Result<i64, StoreError> {
        let st = self.shared.state.lock();
        let committed: i64 = st
            .phase6
            .receivables
            .keys()
            .map(|id| self.outstanding_view(&st.phase6, *id))
            .filter(|outstanding| *outstanding > 0)
            .sum();
        let buffered_opens: i64 = self
            .pending
            .phase6
            .receivable_inserts
            .iter()
            .map(|r| self.outstanding_view(&st.phase6, r.id).max(0))
            .sum();
        Ok(committed.saturating_add(buffered_opens))
    }

    async fn market_ledger_txns(
        &mut self,
        market: MarketId,
    ) -> Result<Vec<LedgerTxnFacts>, StoreError> {
        use domain::ledger::Currency;
        let st = self.shared.state.lock();
        let escrow = st
            .accounts
            .get(&(crate::model::OwnerRef::MarketEscrow(market), Currency::Usdc))
            .copied();
        let pool = st
            .accounts
            .get(&(crate::model::OwnerRef::MarketPool(market), Currency::Usdc))
            .copied();
        let owner_of: std::collections::HashMap<_, _> = st
            .accounts
            .iter()
            .map(|((owner, _), id)| (*id, *owner))
            .collect();
        let mut rows: Vec<(OffsetDateTime, LedgerTxnFacts)> = st
            .ledger_txns
            .values()
            .filter(|t| {
                t.txn
                    .entries()
                    .iter()
                    .any(|e| Some(e.account) == escrow || Some(e.account) == pool)
            })
            .map(|t| {
                (
                    t.created_at,
                    LedgerTxnFacts {
                        txn: t.id,
                        kind: t.txn.kind(),
                        entries: t
                            .txn
                            .entries()
                            .iter()
                            .map(|e| LedgerEntryFacts {
                                account: e.account,
                                owner: owner_of
                                    .get(&e.account)
                                    .copied()
                                    .unwrap_or(crate::model::OwnerRef::External),
                                amount_micro: e.amount.0,
                            })
                            .collect(),
                    },
                )
            })
            .collect();
        rows.sort_by_key(|(created_at, facts)| (*created_at, facts.txn));
        Ok(rows.into_iter().map(|(_, facts)| facts).collect())
    }

    async fn positions_for_market(
        &mut self,
        market: MarketId,
    ) -> Result<Vec<PositionRow>, StoreError> {
        let st = self.shared.state.lock();
        let row = st
            .markets
            .get(&market.0)
            .ok_or(StoreError::NotFound("market"))?;
        let outcomes = [row.yes_outcome.0, row.no_outcome.0];
        let mut positions: Vec<PositionRow> = st
            .positions
            .values()
            .filter(|p| outcomes.contains(&p.outcome.0))
            .copied()
            .collect();
        for pending in &self.pending.positions {
            if outcomes.contains(&pending.outcome.0) {
                if let Some(existing) = positions
                    .iter_mut()
                    .find(|p| p.user == pending.user && p.outcome == pending.outcome)
                {
                    *existing = *pending;
                } else {
                    positions.push(*pending);
                }
            }
        }
        positions.sort_by_key(|p| (p.user.0, p.outcome.0));
        Ok(positions)
    }

    async fn receivable_by_id(
        &mut self,
        id: Uuid,
    ) -> Result<Option<ReceivableOutstanding>, StoreError> {
        let st = self.shared.state.lock();
        Ok(self
            .receivable_view(&st.phase6, id)
            .map(|receivable| ReceivableOutstanding {
                receivable,
                outstanding_micro: self.outstanding_view(&st.phase6, id),
            }))
    }

    async fn write_off_for_update(
        &mut self,
        id: Uuid,
    ) -> Result<Option<WriteOffProposal>, StoreError> {
        let pending = self
            .pending
            .phase6
            .write_off_saves
            .iter()
            .rev()
            .chain(self.pending.phase6.write_off_inserts.iter().rev())
            .find(|p| p.id == id)
            .cloned();
        if pending.is_some() {
            return Ok(pending);
        }
        Ok(self.phase6_view(|p6| p6.write_offs.get(&id).cloned()))
    }

    async fn write_off_by_key(
        &mut self,
        key: &str,
    ) -> Result<Option<WriteOffProposal>, StoreError> {
        if let Some(pending) = self
            .pending
            .phase6
            .write_off_inserts
            .iter()
            .find(|p| p.idempotency_key == key)
        {
            return Ok(Some(pending.clone()));
        }
        Ok(self.phase6_view(|p6| {
            p6.write_off_keys
                .get(key)
                .and_then(|id| p6.write_offs.get(id))
                .cloned()
        }))
    }

    async fn insert_write_off(&mut self, proposal: WriteOffProposal) -> Result<(), StoreError> {
        let taken = self.phase6_view(|p6| {
            p6.write_offs.contains_key(&proposal.id)
                || p6.write_off_keys.contains_key(&proposal.idempotency_key)
        }) || self
            .pending
            .phase6
            .write_off_inserts
            .iter()
            .any(|p| p.id == proposal.id || p.idempotency_key == proposal.idempotency_key);
        if taken {
            return Err(StoreError::Conflict("write-off proposal"));
        }
        self.pending.phase6.write_off_inserts.push(proposal);
        Ok(())
    }

    async fn save_write_off(&mut self, proposal: &WriteOffProposal) -> Result<(), StoreError> {
        self.pending.phase6.write_off_saves.push(proposal.clone());
        Ok(())
    }

    async fn written_off_since(&mut self, since: OffsetDateTime) -> Result<i64, StoreError> {
        // The fake stamps movement application time at commit; buffered
        // write-offs in THIS tx count toward the cap probe.
        let _ = since; // fake window covers the whole lifetime (conservative)
        let committed = self.phase6_view(|p6| {
            p6.movements
                .iter()
                .filter(|m| matches!(m.kind, ReceivableMovementKind::WrittenOff))
                .map(|m| m.amount_micro)
                .sum::<i64>()
        });
        let buffered: i64 = self
            .pending
            .phase6
            .movement_inserts
            .iter()
            .filter(|m| matches!(m.kind, ReceivableMovementKind::WrittenOff))
            .map(|m| m.amount_micro)
            .sum();
        Ok(committed.saturating_add(buffered))
    }

    async fn remedial_for_update(
        &mut self,
        id: Uuid,
    ) -> Result<Option<RemedialCreditProposal>, StoreError> {
        let pending = self
            .pending
            .phase6
            .remedial_saves
            .iter()
            .rev()
            .chain(self.pending.phase6.remedial_inserts.iter().rev())
            .find(|p| p.id == id)
            .cloned();
        if pending.is_some() {
            return Ok(pending);
        }
        Ok(self.phase6_view(|p6| p6.remedials.get(&id).cloned()))
    }

    async fn remedial_by_key(
        &mut self,
        key: &str,
    ) -> Result<Option<RemedialCreditProposal>, StoreError> {
        if let Some(pending) = self
            .pending
            .phase6
            .remedial_inserts
            .iter()
            .find(|p| p.idempotency_key == key)
        {
            return Ok(Some(pending.clone()));
        }
        Ok(self.phase6_view(|p6| {
            p6.remedial_keys
                .get(key)
                .and_then(|id| p6.remedials.get(id))
                .cloned()
        }))
    }

    async fn insert_remedial(
        &mut self,
        proposal: RemedialCreditProposal,
    ) -> Result<(), StoreError> {
        let taken = self.phase6_view(|p6| {
            p6.remedials.contains_key(&proposal.id)
                || p6.remedial_keys.contains_key(&proposal.idempotency_key)
        }) || self
            .pending
            .phase6
            .remedial_inserts
            .iter()
            .any(|p| p.id == proposal.id || p.idempotency_key == proposal.idempotency_key);
        if taken {
            return Err(StoreError::Conflict("remedial proposal"));
        }
        self.pending.phase6.remedial_inserts.push(proposal);
        Ok(())
    }

    async fn save_remedial(&mut self, proposal: &RemedialCreditProposal) -> Result<(), StoreError> {
        self.pending.phase6.remedial_saves.push(proposal.clone());
        Ok(())
    }

    async fn remedial_credited_for_market(&mut self, market: MarketId) -> Result<i64, StoreError> {
        let confirmed = |p: &RemedialCreditProposal| {
            p.market == market && p.status == crate::model::ProposalStatus::Confirmed
        };
        let committed = self.phase6_view(|p6| {
            p6.remedials
                .values()
                .filter(|p| confirmed(p))
                .map(|p| p.amount_micro)
                .sum::<i64>()
        });
        let buffered: i64 = self
            .pending
            .phase6
            .remedial_saves
            .iter()
            .filter(|p| confirmed(p))
            .map(|p| p.amount_micro)
            .sum();
        Ok(committed.saturating_add(buffered))
    }

    async fn remedial_credited_since(&mut self, _since: OffsetDateTime) -> Result<i64, StoreError> {
        // The fake has no confirmed-at column; the daily window covers the
        // whole fake lifetime, which over-counts (conservative for caps).
        let committed = self.phase6_view(|p6| {
            p6.remedials
                .values()
                .filter(|p| p.status == crate::model::ProposalStatus::Confirmed)
                .map(|p| p.amount_micro)
                .sum::<i64>()
        });
        let buffered: i64 = self
            .pending
            .phase6
            .remedial_saves
            .iter()
            .filter(|p| p.status == crate::model::ProposalStatus::Confirmed)
            .map(|p| p.amount_micro)
            .sum();
        Ok(committed.saturating_add(buffered))
    }

    async fn insert_job_command(&mut self, command: OpsJobCommand) -> Result<(), StoreError> {
        let taken = self.phase6_view(|p6| {
            p6.job_commands.contains_key(&command.id)
                || p6.job_command_keys.contains_key(&command.idempotency_key)
        }) || self
            .pending
            .phase6
            .job_inserts
            .iter()
            .any(|c| c.id == command.id || c.idempotency_key == command.idempotency_key);
        if taken {
            return Err(StoreError::Conflict("ops job command"));
        }
        self.pending.phase6.job_inserts.push(command);
        Ok(())
    }

    async fn job_command_by_key(&mut self, key: &str) -> Result<Option<OpsJobCommand>, StoreError> {
        if let Some(pending) = self
            .pending
            .phase6
            .job_inserts
            .iter()
            .find(|c| c.idempotency_key == key)
        {
            return Ok(Some(pending.clone()));
        }
        Ok(self.phase6_view(|p6| {
            p6.job_command_keys
                .get(key)
                .and_then(|id| p6.job_commands.get(id))
                .cloned()
        }))
    }

    async fn due_job_commands(
        &mut self,
        now: OffsetDateTime,
        lease_until: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<OpsJobCommand>, StoreError> {
        use crate::ports::OpsJobStatus as S;
        let mut due: Vec<OpsJobCommand> = self.phase6_view(|p6| {
            p6.job_commands
                .values()
                .filter(|c| match c.status {
                    S::Pending => true,
                    S::Executing => c.lease_expires_at.is_none_or(|lease| lease <= now),
                    S::Done | S::Failed => false,
                })
                .cloned()
                .collect()
        });
        due.sort_by_key(|c| c.id);
        due.truncate(limit as usize);
        for command in &mut due {
            command.status = S::Executing;
            command.lease_expires_at = Some(lease_until);
            command.attempts = command.attempts.saturating_add(1);
            self.pending.phase6.job_saves.push(command.clone());
        }
        Ok(due)
    }

    async fn save_job_command(&mut self, command: &OpsJobCommand) -> Result<(), StoreError> {
        self.pending.phase6.job_saves.push(command.clone());
        Ok(())
    }

    async fn set_user_created_at(
        &mut self,
        user: UserId,
        at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        let known = self.shared.state.lock().users.contains_key(&user.0)
            || self.pending.users.iter().any(|(id, _)| *id == user);
        if !known {
            return Err(StoreError::NotFound("user"));
        }
        self.pending.user_created_at.push((user, at));
        Ok(())
    }

    async fn seed_reputation(
        &mut self,
        user: UserId,
        rep_micro: i64,
        tier: u8,
    ) -> Result<(), StoreError> {
        let known = self.shared.state.lock().users.contains_key(&user.0)
            || self.pending.users.iter().any(|(id, _)| *id == user);
        if !known {
            return Err(StoreError::NotFound("user"));
        }
        self.pending.reputation.push(crate::model::ReputationRow {
            user,
            rep_micro,
            tier,
        });
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Invariant snapshot (D27): clone committed state under the single lock
// ---------------------------------------------------------------------------

/// The fake's repeatable-read snapshot: one deep clone taken under the state
/// lock, so it is wholly-before-or-after any commit.
pub(super) struct FakeInvariantTx {
    state: State,
    as_of: OffsetDateTime,
}

pub(super) fn open_invariants(store: &InMemoryStore) -> FakeInvariantTx {
    let state = store.shared.state.lock().clone();
    FakeInvariantTx {
        state,
        as_of: OffsetDateTime::now_utc(),
    }
}

#[async_trait]
impl InvariantReadTx for FakeInvariantTx {
    async fn as_of(&mut self) -> Result<OffsetDateTime, StoreError> {
        Ok(self.as_of)
    }

    async fn unbalanced_txns(&mut self) -> Result<Vec<TxnSumRow>, StoreError> {
        Ok(self
            .state
            .ledger_txns
            .values()
            .filter_map(|t| {
                let sum: i64 = t.txn.entries().iter().map(|e| e.amount.0).sum();
                (sum != 0).then_some(TxnSumRow {
                    txn: t.id,
                    sum_micro: sum,
                })
            })
            .collect())
    }

    async fn account_balances(&mut self) -> Result<Vec<AccountBalanceRow>, StoreError> {
        if let Some(rows) = &self.state.invariant_account_balances_override {
            return Ok(rows.clone());
        }
        Ok(self
            .state
            .accounts
            .iter()
            .map(|((owner, currency), id)| AccountBalanceRow {
                owner_type: owner_type(*owner),
                currency: *currency,
                balance_micro: self
                    .state
                    .balances
                    .balance(*id)
                    .map_or(0, |balance| i128::from(balance.0)),
            })
            .collect())
    }

    async fn unpaired_payment_facts(&mut self) -> Result<Vec<Uuid>, StoreError> {
        use domain::ledger::Currency;
        // Identity 4: every deposit fact pairs 1:1 with an external leg in
        // its ledger transaction.
        let external = self
            .state
            .accounts
            .get(&(crate::model::OwnerRef::External, Currency::Usdc))
            .copied();
        Ok(self
            .state
            .deposits
            .values()
            .filter(|d| {
                let Some(external) = external else {
                    return true;
                };
                self.state
                    .ledger_txns
                    .get(&d.new.ledger_txn)
                    .is_none_or(|t| {
                        t.txn
                            .entries()
                            .iter()
                            .filter(|e| e.account == external)
                            .count()
                            != 1
                    })
            })
            .map(|d| d.id.0)
            .collect())
    }

    async fn escrow_history(&mut self) -> Result<Vec<EscrowHistoryRow>, StoreError> {
        use domain::ledger::Currency;
        let mut rows = Vec::new();
        for market in self.state.markets.keys() {
            let market = MarketId(*market);
            let Some(escrow) = self
                .state
                .accounts
                .get(&(crate::model::OwnerRef::MarketEscrow(market), Currency::Usdc))
            else {
                continue;
            };
            rows.push(EscrowHistoryRow {
                market,
                residual_micro: self.state.balances.balance(*escrow).map_or(0, |b| b.0),
                collateral_at_close_micro: self
                    .state
                    .collateral_at_close
                    .get(&market.0)
                    .map(|c| c.0),
            });
        }
        rows.sort_by_key(|row| row.market.0);
        Ok(rows)
    }

    async fn duplicate_job_effects(&mut self) -> Result<Vec<String>, StoreError> {
        // Structural uniqueness in the fake: `txn_keys` is itself keyed by
        // the idempotency key, so duplicate terminal effects are
        // unrepresentable in committed state.
        Ok(Vec::new())
    }

    async fn suspense_liability_micro(&mut self) -> Result<i64, StoreError> {
        Ok(self
            .state
            .accounts
            .get(&(
                crate::model::OwnerRef::DepositSuspense,
                domain::ledger::Currency::Usdc,
            ))
            .and_then(|id| self.state.balances.balance(*id).map(|b| b.0))
            .unwrap_or(0))
    }

    async fn bonus_reserve_and_promise(&mut self) -> Result<(i64, i64), StoreError> {
        let reserve = self
            .state
            .accounts
            .get(&(
                crate::model::OwnerRef::BonusReserve,
                domain::ledger::Currency::Usdc,
            ))
            .and_then(|id| self.state.balances.balance(*id).map(|b| b.0))
            .unwrap_or(0);
        let promised = crate::money::credits::remaining_real_money_promise(
            &self.state.phase7.lots,
            &self.state.phase7.allocations,
        );
        Ok((reserve, promised))
    }

    async fn receivable_reconciliation(&mut self) -> Result<Vec<ReceivableReconRow>, StoreError> {
        let p6 = &self.state.phase6;
        let mut by_origin: std::collections::BTreeMap<Uuid, ReceivableReconRow> =
            std::collections::BTreeMap::new();
        for receivable in p6.receivables.values() {
            let row =
                by_origin
                    .entry(receivable.origin_reversal_txn)
                    .or_insert(ReceivableReconRow {
                        origin_reversal_txn: receivable.origin_reversal_txn,
                        opened_micro: 0,
                        house_shortfall_micro: 0,
                        collected_micro: 0,
                        written_off_micro: 0,
                    });
            row.opened_micro = row.opened_micro.saturating_add(receivable.opened_micro);
            for movement in p6
                .movements
                .iter()
                .filter(|m| m.receivable == receivable.id)
            {
                match movement.kind {
                    ReceivableMovementKind::Opened => {}
                    ReceivableMovementKind::Collected => {
                        row.collected_micro =
                            row.collected_micro.saturating_add(movement.amount_micro);
                    }
                    ReceivableMovementKind::WrittenOff => {
                        row.written_off_micro =
                            row.written_off_micro.saturating_add(movement.amount_micro);
                    }
                }
            }
        }
        for row in by_origin.values_mut() {
            // House shortfall legs: the negative House entries of the origin
            // reversal transaction (seed returns are positive; see D30).
            if let Some(stored) = self.state.ledger_txns.get(&row.origin_reversal_txn) {
                let house = self
                    .state
                    .accounts
                    .get(&(
                        crate::model::OwnerRef::House,
                        domain::ledger::Currency::Usdc,
                    ))
                    .copied();
                row.house_shortfall_micro = stored
                    .txn
                    .entries()
                    .iter()
                    .filter(|e| Some(e.account) == house && e.amount.0 < 0)
                    .map(|e| e.amount.0.saturating_neg())
                    .sum();
            }
        }
        Ok(by_origin.into_values().collect())
    }
}

// ---------------------------------------------------------------------------
// Store-level reads: withdrawal eligibility + admin-plane queries
// ---------------------------------------------------------------------------

pub(super) fn withdrawal_eligibility_view(
    store: &InMemoryStore,
    user: UserId,
) -> Result<WithdrawalEligibilityView, StoreError> {
    use domain::ledger::Currency;
    let st = store.shared.state.lock();
    if !st.users.contains_key(&user.0) {
        return Err(StoreError::NotFound("user"));
    }
    let cash = st
        .accounts
        .get(&(crate::model::OwnerRef::User(user), Currency::Usdc))
        .and_then(|id| st.balances.balance(*id))
        .map_or(0, |b| b.0);
    let open: i64 = st
        .phase6
        .receivables
        .values()
        .filter(|r| r.user == user)
        .map(|r| st.phase6.outstanding(r.id).max(0))
        .sum();
    Ok(WithdrawalEligibilityView {
        user,
        cash_micro: cash,
        open_receivables_micro: open,
        eligible: open == 0,
    })
}

#[async_trait]
impl OpsQueries for InMemoryStore {
    async fn audit_page(
        &self,
        before: Option<OffsetDateTime>,
        limit: u32,
    ) -> Result<Vec<AuditPageRow>, StoreError> {
        let st = self.shared.state.lock();
        let mut rows: Vec<&StoredAdminAction> = st
            .phase6
            .admin_actions
            .iter()
            .filter(|row| before.is_none_or(|before| row.at < before))
            .collect();
        rows.sort_by_key(|row| std::cmp::Reverse(row.seq));
        Ok(rows
            .into_iter()
            .take(limit as usize)
            .map(|row| AuditPageRow {
                id: row.id,
                action: row.action.clone(),
                at: row.at,
            })
            .collect())
    }

    async fn publication_command_for_draft(
        &self,
        draft: DraftId,
    ) -> Result<Option<PublicationCommand>, StoreError> {
        let st = self.shared.state.lock();
        let mut commands: Vec<&PublicationCommand> = st
            .phase6
            .publication_commands
            .values()
            .filter(|c| c.draft == draft)
            .collect();
        commands.sort_by_key(|c| c.id);
        Ok(commands.last().map(|c| (*c).clone()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::model::{AdminContext, AdminRole, ProposalStatus, UnwindStage};
    use crate::ports::{OpsJobStatus, PublicationCommandStatus, Store};

    fn ids(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn unwind() -> MarketUnwind {
        MarketUnwind {
            market: MarketId(ids(1)),
            unwind_key: "unwind-1".into(),
            stage: UnwindStage::Proposed,
            proposer_token_id: "p".into(),
            confirmer_token_id: None,
            reason: "r".into(),
            confirm_not_before: OffsetDateTime::UNIX_EPOCH,
            reversal_txn: None,
        }
    }

    fn receivable() -> Receivable {
        Receivable {
            id: ids(2),
            market: MarketId(ids(1)),
            user: UserId(ids(3)),
            origin_reversal_txn: ids(4),
            opened_micro: 10,
        }
    }

    fn movement(kind: ReceivableMovementKind, key: &str) -> ReceivableMovement {
        ReceivableMovement {
            id: Uuid::new_v4(),
            receivable: ids(2),
            kind,
            amount_micro: 2,
            actor: "a".into(),
            cash_txn: None,
            idempotency_key: key.into(),
        }
    }

    fn write_off() -> WriteOffProposal {
        WriteOffProposal {
            id: ids(5),
            receivable: ids(2),
            idempotency_key: "wo".into(),
            amount_micro: 2,
            proposer_token_id: "p".into(),
            confirmer_token_id: None,
            reason: "r".into(),
            status: ProposalStatus::Pending,
            confirm_not_before: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn remedial() -> RemedialCreditProposal {
        RemedialCreditProposal {
            id: ids(6),
            market: MarketId(ids(1)),
            user: UserId(ids(3)),
            idempotency_key: "rc".into(),
            amount_micro: 3,
            proposer_token_id: "p".into(),
            confirmer_token_id: None,
            reason: "r".into(),
            status: ProposalStatus::Pending,
            confirm_not_before: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn job() -> OpsJobCommand {
        OpsJobCommand {
            id: ids(7),
            kind: "replay_job".into(),
            subject: "market:x".into(),
            idempotency_key: "job".into(),
            requested_by: "p".into(),
            status: OpsJobStatus::Pending,
            attempts: 0,
            lease_expires_at: None,
            error: None,
        }
    }

    fn publication() -> PublicationCommand {
        PublicationCommand {
            id: ids(8),
            draft: DraftId(ids(9)),
            idempotency_key: "pub".into(),
            requested_by: "p".into(),
            status: PublicationCommandStatus::Pending,
            attempts: 0,
            lease_expires_at: None,
            result_market: None,
            error: None,
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn phase6_commit_buffers_enforce_all_uniqueness_and_lineage_invariants() {
        let audit = AdminAction {
            actor_role: AdminRole::Ops,
            actor_token_digest: "digest".into(),
            action: "test".into(),
            subject: "subject".into(),
            before: None,
            after: None,
            reason: None,
        };
        let mut state = State::default();
        let pending = Phase6Pending {
            audits: vec![audit],
            unwind_inserts: vec![unwind()],
            receivable_inserts: vec![receivable()],
            movement_inserts: vec![movement(ReceivableMovementKind::Opened, "open")],
            write_off_inserts: vec![write_off()],
            remedial_inserts: vec![remedial()],
            job_inserts: vec![job()],
            publication_inserts: vec![publication()],
            ..Phase6Pending::default()
        };
        apply_phase6(&mut state, &pending).unwrap();
        assert_eq!(state.phase6.outstanding(ids(2)), 2);
        assert!(state
            .phase6
            .open_command_for_draft(DraftId(ids(9)))
            .is_some());

        let mut saves = Phase6Pending::default();
        let mut u = unwind();
        u.stage = UnwindStage::Confirmed;
        saves.unwind_saves.push(u);
        saves.reversals.push((ids(10), ids(11), ids(1)));
        let mut wo = write_off();
        wo.status = ProposalStatus::Rejected;
        saves.write_off_saves.push(wo);
        let mut rc = remedial();
        rc.status = ProposalStatus::Confirmed;
        saves.remedial_saves.push(rc);
        let mut j = job();
        j.status = OpsJobStatus::Done;
        saves.job_saves.push(j);
        let mut p = publication();
        p.status = PublicationCommandStatus::Done;
        saves.publication_saves.push(p);
        apply_phase6(&mut state, &saves).unwrap();
        state
            .phase6
            .publication_commands
            .get_mut(&ids(8))
            .unwrap()
            .status = PublicationCommandStatus::Pending;

        let mut errors: Vec<Phase6Pending> = Vec::new();
        errors.push(Phase6Pending {
            unwind_inserts: vec![unwind()],
            ..Phase6Pending::default()
        });
        let mut same_key = unwind();
        same_key.market = MarketId(ids(20));
        errors.push(Phase6Pending {
            unwind_inserts: vec![same_key],
            ..Phase6Pending::default()
        });
        errors.push(Phase6Pending {
            unwind_saves: vec![MarketUnwind {
                market: MarketId(ids(21)),
                ..unwind()
            }],
            ..Phase6Pending::default()
        });
        errors.push(Phase6Pending {
            reversals: vec![(ids(10), ids(30), ids(1))],
            ..Phase6Pending::default()
        });
        errors.push(Phase6Pending {
            receivable_inserts: vec![receivable()],
            ..Phase6Pending::default()
        });
        errors.push(Phase6Pending {
            movement_inserts: vec![ReceivableMovement {
                amount_micro: 0,
                ..movement(ReceivableMovementKind::Collected, "zero")
            }],
            ..Phase6Pending::default()
        });
        errors.push(Phase6Pending {
            movement_inserts: vec![ReceivableMovement {
                receivable: ids(99),
                ..movement(ReceivableMovementKind::Collected, "missing")
            }],
            ..Phase6Pending::default()
        });
        errors.push(Phase6Pending {
            movement_inserts: vec![movement(ReceivableMovementKind::Collected, "open")],
            ..Phase6Pending::default()
        });
        errors.push(Phase6Pending {
            write_off_inserts: vec![write_off()],
            ..Phase6Pending::default()
        });
        errors.push(Phase6Pending {
            write_off_saves: vec![WriteOffProposal {
                id: ids(99),
                ..write_off()
            }],
            ..Phase6Pending::default()
        });
        errors.push(Phase6Pending {
            remedial_inserts: vec![remedial()],
            ..Phase6Pending::default()
        });
        errors.push(Phase6Pending {
            remedial_saves: vec![RemedialCreditProposal {
                id: ids(99),
                ..remedial()
            }],
            ..Phase6Pending::default()
        });
        errors.push(Phase6Pending {
            job_inserts: vec![job()],
            ..Phase6Pending::default()
        });
        errors.push(Phase6Pending {
            job_saves: vec![OpsJobCommand {
                id: ids(99),
                ..job()
            }],
            ..Phase6Pending::default()
        });
        errors.push(Phase6Pending {
            publication_inserts: vec![publication()],
            ..Phase6Pending::default()
        });
        errors.push(Phase6Pending {
            publication_inserts: vec![PublicationCommand {
                id: ids(98),
                idempotency_key: "other-publication-key".into(),
                ..publication()
            }],
            ..Phase6Pending::default()
        });
        errors.push(Phase6Pending {
            publication_saves: vec![PublicationCommand {
                id: ids(99),
                ..publication()
            }],
            ..Phase6Pending::default()
        });
        for bad in errors {
            assert!(apply_phase6(&mut state.clone(), &bad).is_err());
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn ops_fake_ports_expose_pending_rows_and_reject_duplicates() {
        let store = InMemoryStore::new();
        let user = store.add_user("u", OffsetDateTime::UNIX_EPOCH, 0);
        let market = store
            .add_market(
                "m",
                domain::market::MarketState::Voided,
                OffsetDateTime::UNIX_EPOCH,
                OffsetDateTime::UNIX_EPOCH,
                domain::money::MicroShares(1),
                domain::money::BasisPoints(0),
            )
            .unwrap()
            .id;
        let mut tx = store.unwind_tx().await.unwrap();
        let mut u = unwind();
        u.market = market;
        tx.insert_unwind(u.clone()).await.unwrap();
        assert_eq!(tx.unwind_for_update(market).await.unwrap(), Some(u.clone()));
        assert!(tx.insert_unwind(u).await.is_err());
        assert!(tx.record_reversal(ids(31), ids(32), market).await.is_ok());
        assert!(tx.record_reversal(ids(31), ids(33), market).await.is_err());

        let mut r = receivable();
        r.market = market;
        r.user = user;
        assert!(tx
            .insert_receivable(Receivable {
                opened_micro: 0,
                ..r
            })
            .await
            .is_err());
        tx.insert_receivable(r).await.unwrap();
        assert!(tx.insert_receivable(r).await.is_err());
        assert_eq!(tx.open_receivables_total().await.unwrap(), 0);
        tx.insert_receivable_movement(movement(ReceivableMovementKind::Opened, "pending-open"))
            .await
            .unwrap();
        assert!(tx
            .insert_receivable_movement(movement(ReceivableMovementKind::Collected, "pending-open"))
            .await
            .is_err());
        assert_eq!(
            tx.receivable_by_id(ids(2))
                .await
                .unwrap()
                .unwrap()
                .outstanding_micro,
            2
        );
        tx.insert_receivable_movement(movement(
            ReceivableMovementKind::WrittenOff,
            "pending-written-off",
        ))
        .await
        .unwrap();
        assert_eq!(
            tx.receivable_by_id(ids(2))
                .await
                .unwrap()
                .unwrap()
                .outstanding_micro,
            0
        );
        assert_eq!(
            tx.written_off_since(OffsetDateTime::UNIX_EPOCH)
                .await
                .unwrap(),
            2
        );

        let wo = write_off();
        tx.insert_write_off(wo.clone()).await.unwrap();
        assert_eq!(
            tx.write_off_for_update(wo.id).await.unwrap(),
            Some(wo.clone())
        );
        assert_eq!(tx.write_off_by_key("wo").await.unwrap(), Some(wo.clone()));
        assert!(tx.insert_write_off(wo).await.is_err());
        let rc = RemedialCreditProposal {
            market,
            user,
            ..remedial()
        };
        tx.insert_remedial(rc.clone()).await.unwrap();
        assert_eq!(
            tx.remedial_for_update(rc.id).await.unwrap(),
            Some(rc.clone())
        );
        assert_eq!(tx.remedial_by_key("rc").await.unwrap(), Some(rc.clone()));
        assert!(tx.insert_remedial(rc).await.is_err());
        let mut confirmed_rc = remedial();
        confirmed_rc.market = market;
        confirmed_rc.user = user;
        confirmed_rc.status = ProposalStatus::Confirmed;
        tx.save_remedial(&confirmed_rc).await.unwrap();
        assert_eq!(tx.remedial_credited_for_market(market).await.unwrap(), 3);
        assert_eq!(
            tx.remedial_credited_since(OffsetDateTime::UNIX_EPOCH)
                .await
                .unwrap(),
            3
        );
        let j = job();
        tx.insert_job_command(j.clone()).await.unwrap();
        assert_eq!(tx.job_command_by_key("job").await.unwrap(), Some(j.clone()));
        assert!(tx.insert_job_command(j).await.is_err());
        assert_eq!(
            tx.due_job_commands(OffsetDateTime::UNIX_EPOCH, OffsetDateTime::UNIX_EPOCH, 10)
                .await
                .unwrap()
                .len(),
            0
        );

        assert!(tx
            .set_user_created_at(UserId(ids(99)), OffsetDateTime::UNIX_EPOCH)
            .await
            .is_err());
        assert!(tx.seed_reputation(UserId(ids(99)), 1, 1).await.is_err());
        tx.set_user_created_at(user, OffsetDateTime::UNIX_EPOCH)
            .await
            .unwrap();
        tx.seed_reputation(user, 1, 1).await.unwrap();
        tx.commit().await.unwrap();

        let mut committed = store.unwind_tx().await.unwrap();
        assert_eq!(committed.open_receivables_total().await.unwrap(), 0);
        assert!(committed
            .write_off_for_update(ids(5))
            .await
            .unwrap()
            .is_some());
        assert!(committed
            .remedial_for_update(ids(6))
            .await
            .unwrap()
            .is_some());
        let claimed = committed
            .due_job_commands(
                OffsetDateTime::UNIX_EPOCH,
                OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(10),
                10,
            )
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
        committed.commit().await.unwrap();
        let mut leased = store.unwind_tx().await.unwrap();
        assert!(leased
            .due_job_commands(OffsetDateTime::UNIX_EPOCH, OffsetDateTime::UNIX_EPOCH, 10,)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            leased
                .due_job_commands(
                    OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(10),
                    OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(20),
                    10,
                )
                .await
                .unwrap()
                .len(),
            1
        );

        let bad_audit = AdminAction {
            actor_role: AdminRole::Ops,
            actor_token_digest: String::new(),
            action: String::new(),
            subject: "s".into(),
            before: None,
            after: None,
            reason: None,
        };
        let mut audit_tx = store.unwind_tx().await.unwrap();
        assert!(audit_tx.audit_insert(bad_audit).await.is_err());
        drop(audit_tx);
        assert!(withdrawal_eligibility_view(&store, UserId(ids(99))).is_err());
        let _ = AdminContext::Machine;
    }

    async fn commit_open_receivable(
        store: &InMemoryStore,
        row: Receivable,
        movement_id: Uuid,
        key: &str,
    ) {
        let mut open = store.unwind_tx().await.unwrap();
        open.insert_receivable(row).await.unwrap();
        open.insert_receivable_movement(ReceivableMovement {
            id: movement_id,
            receivable: row.id,
            kind: ReceivableMovementKind::Opened,
            amount_micro: row.opened_micro,
            actor: "test".into(),
            cash_txn: None,
            idempotency_key: key.into(),
        })
        .await
        .unwrap();
        open.commit().await.unwrap();
    }

    async fn fund_and_collect_three_dollars(store: &InMemoryStore, user: UserId) {
        let mut collect = store.unwind_tx().await.unwrap();
        let external = collect
            .account(
                crate::model::OwnerRef::External,
                domain::ledger::Currency::Usdc,
            )
            .await
            .unwrap();
        let user_account = collect
            .account(
                crate::model::OwnerRef::User(user),
                domain::ledger::Currency::Usdc,
            )
            .await
            .unwrap();
        collect
            .ledger_apply(
                domain::ledger::TxnKind::Seed,
                "oldest-receivable-cash",
                &[
                    domain::ledger::Entry {
                        account: external,
                        amount: domain::money::MicroUsd(-3_000_000),
                    },
                    domain::ledger::Entry {
                        account: user_account,
                        amount: domain::money::MicroUsd(3_000_000),
                    },
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            crate::ops::receivable_collection::auto_collect(
                collect.as_mut(),
                user,
                "oldest-first",
                "machine:test",
            )
            .await
            .unwrap(),
            3_000_000
        );
        collect.commit().await.unwrap();
    }

    #[tokio::test]
    async fn open_receivables_follow_insertion_age_not_uuid_order() {
        let store = InMemoryStore::new();
        let user = store.add_user("oldest-receivable", OffsetDateTime::UNIX_EPOCH, 0);
        let older_market = store
            .add_market(
                "older-receivable-market",
                domain::market::MarketState::Voided,
                OffsetDateTime::UNIX_EPOCH,
                OffsetDateTime::UNIX_EPOCH,
                domain::money::MicroShares(1),
                domain::money::BasisPoints(0),
            )
            .unwrap()
            .id;
        let newer_market = store
            .add_market(
                "newer-receivable-market",
                domain::market::MarketState::Voided,
                OffsetDateTime::UNIX_EPOCH,
                OffsetDateTime::UNIX_EPOCH,
                domain::money::MicroShares(1),
                domain::money::BasisPoints(0),
            )
            .unwrap()
            .id;
        let older = Receivable {
            id: ids(200),
            market: older_market,
            user,
            origin_reversal_txn: ids(201),
            opened_micro: 3_000_000,
        };
        let newer = Receivable {
            id: ids(100),
            market: newer_market,
            user,
            origin_reversal_txn: ids(101),
            opened_micro: 5_000_000,
        };

        commit_open_receivable(&store, older, ids(202), "open-older").await;
        commit_open_receivable(&store, newer, ids(102), "open-newer").await;
        fund_and_collect_three_dollars(&store, user).await;

        let mut inspect = store.unwind_tx().await.unwrap();
        assert_eq!(
            inspect
                .receivable_by_id(older.id)
                .await
                .unwrap()
                .unwrap()
                .outstanding_micro,
            0,
            "the oldest receivable must be exhausted before a newer lower UUID"
        );
        assert_eq!(
            inspect
                .receivable_by_id(newer.id)
                .await
                .unwrap()
                .unwrap()
                .outstanding_micro,
            5_000_000
        );
        inspect.commit().await.unwrap();
    }

    #[tokio::test]
    async fn invariant_snapshot_reports_a_broken_receivable_reconciliation() {
        let store = InMemoryStore::new();
        let row = receivable();
        store
            .shared
            .state
            .lock()
            .phase6
            .receivables
            .insert(row.id, row);
        let report = crate::integrity::invariant_sweep::run(&store)
            .await
            .unwrap();
        assert!(!report.pass);
        let broken = report
            .identities
            .iter()
            .find(|identity| identity.identity == "receivables_reconcile")
            .unwrap();
        assert!(broken.detail.as_deref().unwrap().contains("opened 10"));
    }

    #[tokio::test]
    async fn invariant_snapshot_rejects_opposite_per_currency_drifts() {
        let store = InMemoryStore::new();
        store
            .shared
            .state
            .lock()
            .invariant_account_balances_override = Some(vec![
            AccountBalanceRow {
                owner_type: domain::ledger::OwnerType::External,
                currency: domain::ledger::Currency::Usdc,
                balance_micro: -10,
            },
            AccountBalanceRow {
                owner_type: domain::ledger::OwnerType::User,
                currency: domain::ledger::Currency::Usdc,
                balance_micro: 9,
            },
            AccountBalanceRow {
                owner_type: domain::ledger::OwnerType::External,
                currency: domain::ledger::Currency::UsdcCredit,
                balance_micro: -20,
            },
            AccountBalanceRow {
                owner_type: domain::ledger::OwnerType::User,
                currency: domain::ledger::Currency::UsdcCredit,
                balance_micro: 21,
            },
        ]);

        let report = crate::integrity::invariant_sweep::run(&store)
            .await
            .unwrap();
        let mirror = report
            .identities
            .iter()
            .find(|identity| identity.identity == "external_mirrors_internal")
            .unwrap();
        assert!(!mirror.pass, "opposite per-currency drifts must not cancel");
    }

    #[tokio::test]
    async fn pending_position_overlays_replace_the_same_user_outcome() {
        let store = InMemoryStore::new();
        let row = store
            .add_market(
                "position-overlay",
                domain::market::MarketState::Voided,
                OffsetDateTime::UNIX_EPOCH,
                OffsetDateTime::UNIX_EPOCH,
                domain::money::MicroShares(1),
                domain::money::BasisPoints(0),
            )
            .unwrap();
        let user = store.add_user("position-user", OffsetDateTime::UNIX_EPOCH, 0);
        let mut tx = store.unwind_tx().await.unwrap();
        tx.save_position(PositionRow {
            user,
            outcome: row.yes_outcome,
            shares: domain::money::MicroShares(1),
            cost: domain::money::MicroUsd(1),
            realized_pnl: domain::money::MicroUsd(0),
        })
        .await
        .unwrap();
        tx.save_position(PositionRow {
            user,
            outcome: row.yes_outcome,
            shares: domain::money::MicroShares(2),
            cost: domain::money::MicroUsd(2),
            realized_pnl: domain::money::MicroUsd(0),
        })
        .await
        .unwrap();
        let positions = tx.positions_for_market(row.id).await.unwrap();
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].shares, domain::money::MicroShares(2));
    }

    #[tokio::test]
    async fn invariant_snapshot_flags_payment_facts_when_external_is_absent() {
        let store = InMemoryStore::new();
        let deposit = crate::model::DepositId(ids(70));
        store.shared.state.lock().deposits.insert(
            deposit.0,
            super::super::StoredDeposit {
                id: deposit,
                new: crate::model::NewDeposit {
                    user: UserId(ids(71)),
                    amount: domain::money::MicroUsd(1),
                    chain_sig: "sig".into(),
                    ledger_txn: ids(72),
                },
            },
        );
        let mut snapshot = open_invariants(&store);
        assert_eq!(
            snapshot.unpaired_payment_facts().await.unwrap(),
            vec![ids(70)]
        );
    }

    #[tokio::test]
    async fn audit_cursor_and_escrow_residual_branches_are_observable() {
        use domain::ledger::{Currency, Entry, TxnKind};

        let store = InMemoryStore::new();
        crate::ensure_genesis::EnsureGenesis { store: &store }
            .execute(crate::ensure_genesis::EnsureGenesisCmd {
                currency: Currency::Usdc,
                amount: domain::money::MicroUsd(100),
            })
            .await
            .unwrap();
        let market = store
            .add_market(
                "residual",
                domain::market::MarketState::Paid,
                OffsetDateTime::UNIX_EPOCH,
                OffsetDateTime::UNIX_EPOCH,
                domain::money::MicroShares(1),
                domain::money::BasisPoints(0),
            )
            .unwrap()
            .id;
        let mut tx = store.unwind_tx().await.unwrap();
        let house = tx
            .account(crate::model::OwnerRef::House, Currency::Usdc)
            .await
            .unwrap();
        let escrow = tx
            .account(crate::model::OwnerRef::MarketEscrow(market), Currency::Usdc)
            .await
            .unwrap();
        tx.ledger_apply(
            TxnKind::Seed,
            "residual-seed",
            &[
                Entry {
                    account: house,
                    amount: domain::money::MicroUsd(-1),
                },
                Entry {
                    account: escrow,
                    amount: domain::money::MicroUsd(1),
                },
            ],
        )
        .await
        .unwrap();
        tx.audit_insert(AdminAction {
            actor_role: AdminRole::Ops,
            actor_token_digest: "ops".into(),
            action: "residual-test".into(),
            subject: format!("market:{}", market.0),
            before: None,
            after: None,
            reason: None,
        })
        .await
        .unwrap();
        tx.commit().await.unwrap();
        store
            .shared
            .state
            .lock()
            .collateral_at_close
            .insert(market.0, domain::money::MicroUsd(1));

        assert!(store
            .audit_page(Some(OffsetDateTime::UNIX_EPOCH), 10)
            .await
            .unwrap()
            .is_empty());
        let report = crate::integrity::invariant_sweep::run(&store)
            .await
            .unwrap();
        let residual = report
            .identities
            .iter()
            .find(|identity| identity.identity == "escrow_history_zero")
            .unwrap();
        assert!(!residual.pass);
        assert!(residual.detail.as_deref().unwrap().contains("residual 1"));
    }
}

#[cfg(test)]
mod money_invariant_snapshot_tests {
    use super::*;
    use crate::model::{DepositId, NewDeposit, OwnerRef};
    use crate::money::{CreditLotRow, GrantClass};
    use crate::ports::Store;
    use domain::ledger::{Currency, Entry, OwnerType, TxnKind};
    use domain::money::MicroUsd;

    async fn capitalize_house(store: &InMemoryStore) {
        assert_eq!(
            crate::ensure_genesis::EnsureGenesis { store }
                .execute(crate::ensure_genesis::EnsureGenesisCmd {
                    currency: Currency::Usdc,
                    amount: MicroUsd(100),
                })
                .await
                .map(|receipt| receipt.replayed),
            Ok(false)
        );
    }

    #[tokio::test]
    async fn snapshot_reports_malformed_external_pair_and_suspense_liability() {
        let store = InMemoryStore::new();
        capitalize_house(&store).await;
        let mut tx = store
            .credit_convert_tx()
            .await
            .unwrap_or_else(|error| panic!("credit transaction failed: {error}"));
        let house = tx
            .account(OwnerRef::House, Currency::Usdc)
            .await
            .unwrap_or_else(|error| panic!("house account failed: {error}"));
        let suspense = tx
            .account(OwnerRef::DepositSuspense, Currency::Usdc)
            .await
            .unwrap_or_else(|error| panic!("suspense account failed: {error}"));
        let malformed = tx
            .ledger_apply(
                TxnKind::Deposit,
                "malformed-deposit-pair",
                &[
                    Entry {
                        account: house,
                        amount: MicroUsd(-3),
                    },
                    Entry {
                        account: suspense,
                        amount: MicroUsd(3),
                    },
                ],
            )
            .await
            .unwrap_or_else(|error| panic!("malformed deposit fixture failed: {error}"));
        assert_eq!(tx.commit().await, Ok(()));

        let deposit = DepositId(Uuid::from_u128(400));
        let user = UserId(Uuid::from_u128(401));
        store.shared.state.lock().deposits.insert(
            deposit.0,
            super::super::StoredDeposit {
                id: deposit,
                new: NewDeposit {
                    user,
                    amount: MicroUsd(3),
                    chain_sig: "malformed-pair".into(),
                    ledger_txn: malformed,
                },
            },
        );

        let mut snapshot = open_invariants(&store);
        assert_eq!(snapshot.unpaired_payment_facts().await, Ok(vec![deposit.0]));
        assert_eq!(snapshot.suspense_liability_micro().await, Ok(3));
    }

    #[tokio::test]
    async fn snapshot_exposes_bonus_reserve_promise_and_withheld_owner() {
        let store = InMemoryStore::new();
        capitalize_house(&store).await;
        let mut tx = store
            .credit_convert_tx()
            .await
            .unwrap_or_else(|error| panic!("credit transaction failed: {error}"));
        let house = tx
            .account(OwnerRef::House, Currency::Usdc)
            .await
            .unwrap_or_else(|error| panic!("house account failed: {error}"));
        let reserve = tx
            .account(OwnerRef::BonusReserve, Currency::Usdc)
            .await
            .unwrap_or_else(|error| panic!("reserve account failed: {error}"));
        let _withheld = tx
            .account(OwnerRef::Withheld, Currency::Usdc)
            .await
            .unwrap_or_else(|error| panic!("withheld account failed: {error}"));
        assert_eq!(
            tx.ledger_apply(
                TxnKind::Seed,
                "reserve-funding",
                &[
                    Entry {
                        account: house,
                        amount: MicroUsd(-10),
                    },
                    Entry {
                        account: reserve,
                        amount: MicroUsd(10),
                    },
                ],
            )
            .await
            .map(|_| ()),
            Ok(())
        );
        assert_eq!(tx.commit().await, Ok(()));

        store.shared.state.lock().phase7.lots.push(CreditLotRow {
            id: Uuid::from_u128(402),
            user: UserId(Uuid::from_u128(401)),
            source: "snapshot-grant".into(),
            amount_micro: 4,
            granted_at: OffsetDateTime::UNIX_EPOCH,
            grant_class: GrantClass::RealMoney,
            policy_version: "v1".into(),
            converted_at: None,
        });
        let mut snapshot = open_invariants(&store);
        assert_eq!(snapshot.bonus_reserve_and_promise().await, Ok((10, 4)));
        let balances = snapshot
            .account_balances()
            .await
            .unwrap_or_else(|error| panic!("account balance snapshot failed: {error}"));
        assert!(balances
            .iter()
            .any(|row| row.owner_type == OwnerType::Withheld && row.balance_micro == 0));
    }
}
