//! Deposit observation and admission machine (D32).
//!
//! A finalized chain observation is durable before admission is attempted:
//! `External → DepositSuspense`, then a separate transaction advances
//! `observed_finalized → admission_pending → admitted | compliance_hold`.
//! The split makes a crash between observation and admission healable and
//! guarantees a compliance refusal never loses the finalized inflow.

use domain::ledger::{Currency, Entry, TxnKind};
use domain::money::MicroUsd;
use serde_json::json;

use crate::error::{AppError, StoreError};
use crate::model::{AdminContext, DepositId, DepositMachineStatus, Event, OwnerRef, UserId};
use crate::money::{ComplianceDecision, DepositMachineRow, DepositRefundPayment, ObservedDeposit};
use crate::ops::audit::audit_for;
use crate::ports::Store;

#[derive(Debug, Clone)]
pub struct CreditDepositCmd {
    pub user: UserId,
    pub amount: MicroUsd,
    /// The chain transaction signature — the dedupe key across retries and
    /// listener restarts.
    pub chain_sig: String,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DepositReceipt {
    pub deposit_id: DepositId,
    /// `None` on a replay observed through a different idempotency key (the
    /// signature dedupe cannot recover the original ledger transaction).
    pub ledger_txn: Option<uuid::Uuid>,
    pub replayed: bool,
    /// Micro-USD auto-collected against open receivables in the same
    /// transaction (0 when the user owes nothing).
    pub collected_micro: i64,
}

/// Result of the observation half of the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservationReceipt {
    pub deposit_id: DepositId,
    pub suspense_tx: uuid::Uuid,
    pub replayed: bool,
}

/// The complete persisted deposit-machine edge set. Callers may retry an
/// edge through CAS, but no adapter may invent a transition outside this
/// table.
#[must_use]
pub const fn legal_deposit_transition(
    from: DepositMachineStatus,
    to: DepositMachineStatus,
) -> bool {
    matches!(
        (from, to),
        (
            DepositMachineStatus::ObservedFinalized,
            DepositMachineStatus::AdmissionPending
        ) | (
            DepositMachineStatus::AdmissionPending,
            DepositMachineStatus::Admitted | DepositMachineStatus::ComplianceHold
        ) | (
            DepositMachineStatus::ComplianceHold,
            DepositMachineStatus::AdmissionPending
                | DepositMachineStatus::Admitted
                | DepositMachineStatus::RefundApproved
        ) | (
            DepositMachineStatus::RefundApproved,
            DepositMachineStatus::RefundSending
        ) | (
            DepositMachineStatus::RefundSending,
            DepositMachineStatus::Refunded
        )
    )
}

pub struct CreditDeposit<'a, S: Store> {
    pub store: &'a S,
}

impl<S: Store> CreditDeposit<'_, S> {
    /// # Errors
    /// Store failures; a duplicate `chain_sig` is a replay, not an error.
    pub async fn execute(&self, cmd: CreditDepositCmd) -> Result<DepositReceipt, AppError> {
        self.execute_as(cmd, &AdminContext::Machine).await
    }

    /// D26 actor threading for the staging faucet: an admin actor's audit
    /// fact commits in the SAME transaction as the credit. Machine paths
    /// (the chain listener) ride [`Self::execute`].
    ///
    /// # Errors
    /// As [`Self::execute`].
    pub async fn execute_as(
        &self,
        cmd: CreditDepositCmd,
        actor: &AdminContext,
    ) -> Result<DepositReceipt, AppError> {
        // Compatibility shim for the staging faucet. The inbound watcher
        // calls `observe_finalized_on_rail` with the startup-pinned rail.
        let observed = ObservedDeposit {
            user: Some(cmd.user),
            amount: cmd.amount,
            chain_sig: cmd.chain_sig.clone(),
            source_address: format!("staging-faucet:{}", cmd.user.0),
            dest_address: "staging-faucet-treasury".to_string(),
            mint: "staging-usdc".to_string(),
            slot: 0,
        };
        let observation =
            observe_finalized_on_rail(self.store, &observed, "staging-faucet").await?;
        let mut admission = admit_observed(
            self.store,
            &cmd.chain_sig,
            time::OffsetDateTime::now_utc(),
            actor,
            false,
        )
        .await?;
        admission.replayed &= observation.replayed;
        Ok(admission)
    }
}

/// Persist one finalized observation in suspense. Replaying the exact bound
/// tuple is a no-op; reusing a signature for a different tuple is a conflict.
///
/// # Errors
/// Invalid observations, binding conflicts, or store failures.
pub async fn observe_finalized<S: Store>(
    store: &S,
    observed: &ObservedDeposit,
) -> Result<ObservationReceipt, AppError> {
    let compatibility_fingerprint = format!("legacy-observation:{}", observed.mint);
    observe_finalized_on_rail(store, observed, &compatibility_fingerprint).await
}

/// Persist one finalized observation with the startup-pinned inbound rail
/// identity. The fingerprint becomes part of the immutable replay binding and
/// is later reused by the source-locked refund path.
///
/// # Errors
/// Invalid observations or rail identity, binding conflicts, or store failures.
pub async fn observe_finalized_on_rail<S: Store>(
    store: &S,
    observed: &ObservedDeposit,
    rail_fingerprint: &str,
) -> Result<ObservationReceipt, AppError> {
    validate_observation(observed)?;
    if rail_fingerprint.is_empty() {
        return Err(StoreError::Invariant("deposit rail fingerprint missing").into());
    }
    let guard_key = format!("deposit-observe:{}", observed.chain_sig);
    let mut tx = store.deposit_admission_tx().await?;
    tx.serialize_key(&guard_key).await?;
    if let Some(existing) = tx.deposit_machine_by_sig(&observed.chain_sig).await? {
        if !same_observation(&existing, observed, rail_fingerprint) {
            return Err(StoreError::Conflict("deposit observation binding").into());
        }
        return Ok(ObservationReceipt {
            deposit_id: existing.id,
            suspense_tx: existing.suspense_tx_id.ok_or(StoreError::Invariant(
                "observed deposit has no suspense transaction",
            ))?,
            replayed: true,
        });
    }

    let external = tx.account(OwnerRef::External, Currency::Usdc).await?;
    let suspense = tx
        .account(OwnerRef::DepositSuspense, Currency::Usdc)
        .await?;
    let suspense_tx = tx
        .ledger_apply(
            TxnKind::Deposit,
            &guard_key,
            &[
                Entry {
                    account: external,
                    amount: MicroUsd(-observed.amount.0),
                },
                Entry {
                    account: suspense,
                    amount: observed.amount,
                },
            ],
        )
        .await?;
    let deposit_id = tx
        .observe_deposit(observed, suspense_tx, rail_fingerprint)
        .await?;
    tx.append(Event {
        event_type: "DepositObserved",
        aggregate_type: "deposit",
        aggregate_id: deposit_id.0,
        payload: json!({
            "user_id": observed.user.map(|user| user.0.to_string()),
            "amount_micro": observed.amount.0,
            "chain_sig": observed.chain_sig,
            "source_address": observed.source_address,
            "dest_address": observed.dest_address,
            "mint": observed.mint,
            "slot": observed.slot,
            "rail_fingerprint": rail_fingerprint,
        }),
    })
    .await?;
    tx.commit().await?;
    Ok(ObservationReceipt {
        deposit_id,
        suspense_tx,
        replayed: false,
    })
}

/// Attempt admission of an observed deposit. A held row is a no-op unless
/// `reevaluate_hold` is true after fresh compliance facts or a policy change.
///
/// # Errors
/// Store failures or an impossible machine state.
#[allow(clippy::too_many_lines)]
pub async fn admit_observed<S: Store>(
    store: &S,
    chain_sig: &str,
    now: time::OffsetDateTime,
    actor: &AdminContext,
    reevaluate_hold: bool,
) -> Result<DepositReceipt, AppError> {
    if reevaluate_hold && !matches!(actor, AdminContext::Machine) {
        return Err(AppError::AdminForbidden(
            "held deposit reevaluation is machine-only; use dual control for a manual admission",
        ));
    }
    let guard_key = format!("deposit-machine:{chain_sig}");
    let mut tx = store.deposit_admission_tx().await?;
    tx.serialize_key(&guard_key).await?;
    let row = tx
        .deposit_machine_by_sig(chain_sig)
        .await?
        .ok_or(StoreError::NotFound("deposit"))?;

    match row.status {
        DepositMachineStatus::Admitted | DepositMachineStatus::AdmittedLegacy => {
            return Ok(existing_receipt(&row));
        }
        DepositMachineStatus::ComplianceHold if !reevaluate_hold => {
            return Ok(existing_receipt(&row));
        }
        DepositMachineStatus::RefundApproved
        | DepositMachineStatus::RefundSending
        | DepositMachineStatus::Refunded => {
            return Err(StoreError::Conflict("deposit refund already selected").into());
        }
        DepositMachineStatus::ObservedFinalized
        | DepositMachineStatus::AdmissionPending
        | DepositMachineStatus::ComplianceHold => {}
    }

    // Acquire the class-2 user lock before touching deposit money authority.
    // If a concurrent refund wins its CAS while we wait, every lazy
    // conversion/collection below rolls back with this transaction.
    let mut pre = crate::money::ConvertCollectReceipt::default();
    if let Some(user) = row.user {
        tx.lock_user(user).await?;
        pre = tx
            .convert_then_collect(user, now, &format!("deposit-lock:{}", row.id.0))
            .await?;
    }

    if row.status != DepositMachineStatus::AdmissionPending {
        let moved = tx
            .cas_deposit_status(row.id, row.status, DepositMachineStatus::AdmissionPending)
            .await?;
        if !moved {
            return Err(StoreError::Conflict("deposit status").into());
        }
    }

    let Some(user) = row.user else {
        return hold_admission(tx, &row, "unmatched", 0, actor, now).await;
    };

    let gate = if tx.user_status(user).await? == "banned" {
        Err(AppError::ComplianceHold { reason: "banned" })
    } else {
        crate::money::enforcement::enforce_money_mutation(
            tx.as_mut(),
            user,
            crate::money::enforcement::MoneyMutation::DepositAdmit,
            row.amount.0,
            now,
        )
        .await
    };
    if let Err(error) = gate {
        if let Some(reason) = admission_hold_reason(&error) {
            return hold_admission(tx, &row, reason, pre.collected_micro, actor, now).await;
        }
        return Err(error);
    }

    let suspense = tx
        .account(OwnerRef::DepositSuspense, Currency::Usdc)
        .await?;
    let user_account = tx.account(OwnerRef::User(user), Currency::Usdc).await?;
    let ledger_key = format!("deposit-admit:{}", row.id.0);
    let admit_tx = tx
        .ledger_apply(
            TxnKind::Deposit,
            &ledger_key,
            &[
                Entry {
                    account: suspense,
                    amount: MicroUsd(-row.amount.0),
                },
                Entry {
                    account: user_account,
                    amount: row.amount,
                },
            ],
        )
        .await?;
    tx.mark_admitted(row.id, admit_tx).await?;
    let moved = tx
        .cas_deposit_status(
            row.id,
            DepositMachineStatus::AdmissionPending,
            DepositMachineStatus::Admitted,
        )
        .await?;
    if !moved {
        return Err(StoreError::Conflict("deposit status").into());
    }
    tx.insert_compliance_decision(ComplianceDecision {
        id: uuid::Uuid::new_v4(),
        subject_type: "deposit".into(),
        subject_id: row.id.0,
        kind: "admitted".into(),
        actor: actor_name(actor),
        at: now,
        payload: json!({ "amount_micro": row.amount.0 }),
    })
    .await?;
    tx.append(Event {
        event_type: "DepositAdmitted",
        aggregate_type: "deposit",
        aggregate_id: row.id.0,
        payload: json!({
            "user_id": user.0.to_string(),
            "amount_micro": row.amount.0,
        }),
    })
    .await?;
    let post = tx.convert_then_collect(user, now, &ledger_key).await?;
    let collected_micro = pre
        .collected_micro
        .checked_add(post.collected_micro)
        .ok_or(AppError::Overflow)?;
    insert_deposit_audit(
        tx.as_mut(),
        actor,
        &row,
        "admitted",
        Some(admit_tx),
        collected_micro,
    )
    .await?;
    tx.commit().await?;
    Ok(DepositReceipt {
        deposit_id: row.id,
        ledger_txn: Some(admit_tx),
        replayed: false,
        collected_micro,
    })
}

/// Reevaluate a compliance-held deposit after a fresh fact or policy change.
///
/// # Errors
/// As [`admit_observed`].
pub async fn reevaluate_held<S: Store>(
    store: &S,
    chain_sig: &str,
    now: time::OffsetDateTime,
) -> Result<DepositReceipt, AppError> {
    admit_observed(store, chain_sig, now, &AdminContext::Machine, true).await
}

const MANUAL_DEPOSIT_ADMIT: &str = "manual_deposit_admit";
const DEPOSIT_REFUND: &str = "deposit_refund";

/// Finance proposes a manual admission or source-locked refund for one held
/// observation. The deposit id is the replay namespace mandated by the
/// command matrix.
///
/// # Errors
/// Role/reason/state failures or store errors.
pub async fn propose_deposit_command<S: Store>(
    store: &S,
    deposit: DepositId,
    refund: bool,
    reason: String,
    now: time::OffsetDateTime,
    actor: &AdminContext,
) -> Result<crate::money::MoneyProposal, AppError> {
    let (digest, role) = crate::money::admin::require_admin(actor)?;
    crate::money::admin::require_role(role, &[crate::model::AdminRole::Finance])?;
    if reason.trim().is_empty() {
        return Err(AppError::AdminForbidden("reason is required"));
    }
    let kind = if refund {
        DEPOSIT_REFUND
    } else {
        MANUAL_DEPOSIT_ADMIT
    };
    let replay_key = format!("{kind}:{}", deposit.0);
    let mut tx = store.deposit_admission_tx().await?;
    tx.serialize_key(&replay_key).await?;
    let row = tx
        .deposit_machine_by_id(deposit)
        .await?
        .ok_or(StoreError::NotFound("deposit"))?;
    if row.status != DepositMachineStatus::ComplianceHold {
        return Err(StoreError::Conflict("deposit is not held").into());
    }
    let payload = deposit_command_payload(kind, &row);
    let payload_hash = crate::money::admin::payload_hash(payload.as_bytes());
    if let Some(existing) = tx.get_proposal_by_replay(&replay_key).await? {
        if existing.kind != kind
            || existing.subject_id != deposit.0
            || existing.payload_hash != payload_hash
        {
            return Err(AppError::ProposalConflict("deposit proposal drift"));
        }
        return Ok(existing);
    }
    let proposal = crate::money::admin::new_proposal(
        kind,
        deposit.0,
        payload_hash,
        digest.to_string(),
        reason.clone(),
        replay_key,
        time::Duration::ZERO,
        now,
    );
    tx.insert_proposal(proposal.clone()).await?;
    tx.audit_insert(crate::model::AdminAction {
        actor_role: role,
        actor_token_digest: digest.to_string(),
        action: format!("propose_{kind}"),
        subject: format!("deposit:{}", deposit.0),
        before: Some(json!({
            "status": crate::money::deposit_status_name(row.status)
        })),
        after: None,
        reason: Some(reason),
    })
    .await?;
    tx.commit().await?;
    Ok(proposal)
}

/// Distinct superadmin confirms a manual held-deposit admission and commits
/// the proposal CAS, suspense release, compliance decision, event, and audit
/// in one transaction.
///
/// # Errors
/// Authority/proposal/state failures or store/ledger errors.
#[allow(clippy::too_many_lines)]
pub async fn confirm_manual_deposit_admission<S: Store>(
    store: &S,
    proposal_id: uuid::Uuid,
    now: time::OffsetDateTime,
    actor: &AdminContext,
) -> Result<DepositReceipt, AppError> {
    let (digest, role) = crate::money::admin::require_admin(actor)?;
    crate::money::admin::require_role(role, &[crate::model::AdminRole::Superadmin])?;
    let mut tx = store.deposit_admission_tx().await?;
    tx.serialize_key(&format!("deposit-confirm:{proposal_id}"))
        .await?;
    let proposal = tx.get_proposal(proposal_id).await?;
    validate_deposit_proposal(&proposal, MANUAL_DEPOSIT_ADMIT, digest, now)?;
    let row = tx
        .deposit_machine_by_id(DepositId(proposal.subject_id))
        .await?
        .ok_or(StoreError::NotFound("deposit"))?;
    validate_deposit_proposal_payload(&proposal, &row)?;
    if row.status != DepositMachineStatus::ComplianceHold {
        return Err(StoreError::Conflict("deposit is not held").into());
    }
    let user = row.user.ok_or(StoreError::Invariant(
        "unmatched deposit cannot be admitted",
    ))?;
    tx.lock_user(user).await?;
    let pre = tx
        .convert_then_collect(
            user,
            now,
            &format!("manual-deposit-admit-lock:{}", row.id.0),
        )
        .await?;
    let suspense = tx
        .account(OwnerRef::DepositSuspense, Currency::Usdc)
        .await?;
    let user_account = tx.account(OwnerRef::User(user), Currency::Usdc).await?;
    let ledger_key = format!("deposit-admit:{}", row.id.0);
    let admit_tx = tx
        .ledger_apply(
            TxnKind::Deposit,
            &ledger_key,
            &[
                Entry {
                    account: suspense,
                    amount: MicroUsd(-row.amount.0),
                },
                Entry {
                    account: user_account,
                    amount: row.amount,
                },
            ],
        )
        .await?;
    tx.mark_admitted(row.id, admit_tx).await?;
    if !tx
        .cas_deposit_status(
            row.id,
            DepositMachineStatus::ComplianceHold,
            DepositMachineStatus::Admitted,
        )
        .await?
    {
        return Err(StoreError::Conflict("deposit status").into());
    }
    let post = tx.convert_then_collect(user, now, &ledger_key).await?;
    let collected_micro = pre
        .collected_micro
        .checked_add(post.collected_micro)
        .ok_or(AppError::Overflow)?;
    let confirmed = tx.confirm_proposal(proposal.id, digest, now).await?;
    validate_confirmed_deposit_proposal(&confirmed)?;
    tx.insert_compliance_decision(ComplianceDecision {
        id: uuid::Uuid::new_v4(),
        subject_type: "deposit".into(),
        subject_id: row.id.0,
        kind: "manual_admitted".into(),
        actor: digest.to_string(),
        at: now,
        payload: json!({
            "proposal_id": proposal.id.to_string(),
            "ledger_txn": admit_tx.to_string(),
            "collected_micro": collected_micro,
        }),
    })
    .await?;
    tx.append(Event {
        event_type: "DepositAdmitted",
        aggregate_type: "deposit",
        aggregate_id: row.id.0,
        payload: json!({
            "user_id": user.0.to_string(),
            "amount_micro": row.amount.0,
            "manual": true,
            "proposal_id": proposal.id.to_string(),
        }),
    })
    .await?;
    tx.audit_insert(crate::model::AdminAction {
        actor_role: role,
        actor_token_digest: digest.to_string(),
        action: "confirm_manual_deposit_admit".into(),
        subject: format!("deposit:{}", row.id.0),
        before: Some(json!({
            "status": crate::money::deposit_status_name(DepositMachineStatus::ComplianceHold)
        })),
        after: Some(json!({
            "status": crate::money::deposit_status_name(DepositMachineStatus::Admitted),
            "ledger_txn": admit_tx.to_string(),
        })),
        reason: Some(proposal.reason),
    })
    .await?;
    tx.commit().await?;
    Ok(DepositReceipt {
        deposit_id: row.id,
        ledger_txn: Some(admit_tx),
        replayed: false,
        collected_micro,
    })
}

/// Distinct superadmin confirms a source-locked deposit refund. The proposal
/// CAS and `outbound_payments(subject=deposit_refund)` row commit atomically.
///
/// # Errors
/// Authority/proposal/state/screening failures or store errors.
#[allow(clippy::too_many_lines)]
pub async fn confirm_deposit_refund<S: Store>(
    store: &S,
    proposal_id: uuid::Uuid,
    now: time::OffsetDateTime,
    actor: &AdminContext,
) -> Result<RefundReceipt, AppError> {
    let (digest, role) = crate::money::admin::require_admin(actor)?;
    crate::money::admin::require_role(role, &[crate::model::AdminRole::Superadmin])?;
    let mut tx = store.deposit_admission_tx().await?;
    tx.serialize_key(&format!("deposit-confirm:{proposal_id}"))
        .await?;
    let proposal = tx.get_proposal(proposal_id).await?;
    validate_deposit_proposal(&proposal, DEPOSIT_REFUND, digest, now)?;
    let row = tx
        .deposit_machine_by_id(DepositId(proposal.subject_id))
        .await?
        .ok_or(StoreError::NotFound("deposit"))?;
    validate_deposit_proposal_payload(&proposal, &row)?;
    if row.status != DepositMachineStatus::ComplianceHold {
        return Err(StoreError::Conflict("deposit is not refundable").into());
    }
    if let Some(user) = row.user {
        tx.lock_user(user).await?;
        tx.convert_then_collect(user, now, &format!("deposit-refund-lock:{}", row.id.0))
            .await?;
        if tx.user_status(user).await? == "banned"
            || !tx.fresh_clear(user, "geo", now).await?
            || !tx.fresh_clear(user, "sanctions", now).await?
        {
            return Err(AppError::ComplianceHold {
                reason: "refund_requires_clear",
            });
        }
    }
    if !tx
        .cas_deposit_status(
            row.id,
            DepositMachineStatus::ComplianceHold,
            DepositMachineStatus::RefundApproved,
        )
        .await?
    {
        return Err(StoreError::Conflict("deposit status").into());
    }
    let outbound = crate::ports::OutboundPaymentRow {
        id: uuid::Uuid::new_v4(),
        subject: crate::ports::OutboundSubject::DepositRefund,
        subject_id: row.id.0,
        dest: row.source_address.clone(),
        amount_micro: row.amount.0,
        rail_fingerprint: row.rail_fingerprint.clone(),
    };
    tx.insert_outbound_payment(&outbound).await?;
    let confirmed = tx.confirm_proposal(proposal.id, digest, now).await?;
    validate_confirmed_deposit_proposal(&confirmed)?;
    tx.insert_compliance_decision(ComplianceDecision {
        id: uuid::Uuid::new_v4(),
        subject_type: "deposit".into(),
        subject_id: row.id.0,
        kind: "refund_approved".into(),
        actor: digest.to_string(),
        at: now,
        payload: json!({
            "proposal_id": proposal.id.to_string(),
            "payment_id": outbound.id.to_string(),
            "dest": outbound.dest,
        }),
    })
    .await?;
    tx.append(Event {
        event_type: "DepositRefundApproved",
        aggregate_type: "deposit",
        aggregate_id: row.id.0,
        payload: json!({
            "proposal_id": proposal.id.to_string(),
            "payment_id": outbound.id.to_string(),
        }),
    })
    .await?;
    tx.audit_insert(crate::model::AdminAction {
        actor_role: role,
        actor_token_digest: digest.to_string(),
        action: "confirm_deposit_refund".into(),
        subject: format!("deposit:{}", row.id.0),
        before: Some(json!({
            "status": crate::money::deposit_status_name(DepositMachineStatus::ComplianceHold)
        })),
        after: Some(json!({
            "status": crate::money::deposit_status_name(DepositMachineStatus::RefundApproved),
            "payment_id": outbound.id.to_string(),
            "dest": row.source_address,
        })),
        reason: Some(proposal.reason),
    })
    .await?;
    tx.commit().await?;
    Ok(RefundReceipt {
        deposit_id: row.id,
        payment_id: outbound.id,
        dest: outbound.dest,
        replayed: false,
    })
}

fn deposit_command_payload(kind: &str, row: &DepositMachineRow) -> String {
    format!(
        "{kind}|{}|{}|{}|{}|{}",
        row.id.0, row.amount.0, row.source_address, row.chain_sig, row.rail_fingerprint
    )
}

fn validate_deposit_proposal(
    proposal: &crate::money::MoneyProposal,
    kind: &str,
    confirmer: &str,
    now: time::OffsetDateTime,
) -> Result<(), AppError> {
    crate::money::admin::require_distinct_tokens(&proposal.proposer_token_id, confirmer)?;
    if proposal.kind != kind {
        return Err(AppError::ProposalConflict("wrong deposit proposal kind"));
    }
    if proposal.status != crate::money::ProposalStatus::Pending {
        return Err(AppError::ProposalConflict("proposal is not pending"));
    }
    if now < proposal.confirm_not_before || now > proposal.expires_at {
        return Err(AppError::ProposalConflict("proposal not confirmable"));
    }
    Ok(())
}

fn validate_deposit_proposal_payload(
    proposal: &crate::money::MoneyProposal,
    row: &DepositMachineRow,
) -> Result<(), AppError> {
    let expected =
        crate::money::admin::payload_hash(deposit_command_payload(&proposal.kind, row).as_bytes());
    if proposal.subject_id != row.id.0 || proposal.payload_hash != expected {
        return Err(AppError::ProposalConflict("deposit proposal drift"));
    }
    Ok(())
}

fn validate_confirmed_deposit_proposal(
    proposal: &crate::money::MoneyProposal,
) -> Result<(), AppError> {
    if proposal.status != crate::money::ProposalStatus::Confirmed {
        return Err(AppError::ProposalConflict("proposal confirmation lost"));
    }
    Ok(())
}

/// Replay-stable source-locked refund approval / send receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefundReceipt {
    pub deposit_id: DepositId,
    pub payment_id: uuid::Uuid,
    pub dest: String,
    pub replayed: bool,
}

/// Terminal suspense-release receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefundSettlementReceipt {
    pub deposit_id: DepositId,
    pub payment_id: uuid::Uuid,
    pub ledger_txn: uuid::Uuid,
    pub replayed: bool,
}

/// Claim the approved refund, persist one signed attempt before broadcast,
/// and advance the attempt to `broadcast` only after the rail call succeeds.
///
/// # Errors
/// Invalid state, signing/rail failure, or store failure.
#[allow(clippy::too_many_lines)]
pub async fn begin_refund_send<S: Store>(
    store: &S,
    chain_sig: &str,
    now: time::OffsetDateTime,
    rails: &dyn crate::ports::OutboundRails,
    signer: &dyn crate::ports::withdraw_send::WithdrawSigner,
) -> Result<RefundReceipt, AppError> {
    const LEASE: time::Duration = time::Duration::seconds(30);
    let guard_key = format!("deposit-machine:{chain_sig}");
    let mut tx = store.deposit_admission_tx().await?;
    tx.serialize_key(&guard_key).await?;
    let row = tx
        .deposit_machine_by_sig(chain_sig)
        .await?
        .ok_or(StoreError::NotFound("deposit"))?;
    if !matches!(
        row.status,
        DepositMachineStatus::RefundApproved
            | DepositMachineStatus::RefundSending
            | DepositMachineStatus::Refunded
    ) {
        return Err(StoreError::Conflict("deposit refund is not approved").into());
    }
    let payment = checked_refund_payment(tx.as_mut(), &row).await?;
    let outbound = refund_outbound(&row, &payment);
    if row.status == DepositMachineStatus::Refunded {
        let attempts = tx.attempts_for(payment.id).await?;
        if attempts
            .iter()
            .filter(|attempt| attempt.landing_state == crate::ports::LandingState::Finalized)
            .count()
            != 1
        {
            return Err(
                StoreError::Invariant("refunded deposit has no unique finalized attempt").into(),
            );
        }
        return Ok(refund_receipt(&row, &payment, true));
    }

    if row.status == DepositMachineStatus::RefundSending {
        let mut attempt = tx
            .live_attempt(payment.id)
            .await?
            .ok_or(StoreError::Invariant(
                "refund_sending deposit has no live attempt",
            ))?;
        match attempt.landing_state {
            crate::ports::LandingState::Broadcast => {
                return Ok(refund_receipt(&row, &payment, true));
            }
            crate::ports::LandingState::Prepared => {
                let lease = attempt.lease_expires_at.ok_or(StoreError::Invariant(
                    "prepared refund attempt has no lease",
                ))?;
                if now < lease {
                    return Err(AppError::ProposalConflict("send lease still held"));
                }
                match signer
                    .lookup_signature(&attempt.signature)
                    .await
                    .unwrap_or(crate::ports::SignaturePresence::Unknown)
                {
                    crate::ports::SignaturePresence::Present => {
                        attempt.landing_state = crate::ports::LandingState::Broadcast;
                        tx.save_attempt(&attempt).await?;
                        tx.commit().await?;
                        return Ok(refund_receipt(&row, &payment, true));
                    }
                    crate::ports::SignaturePresence::Absent
                        if signer
                            .finalized_block_height()
                            .await
                            .is_ok_and(|height| height <= attempt.last_valid_block_height) =>
                    {
                        tx.commit().await?;
                        crate::ports::outbound::same_bytes_rebroadcast(rails, &outbound, &attempt)
                            .await?;
                        mark_refund_broadcast(store, chain_sig).await?;
                        return Ok(refund_receipt(&row, &payment, true));
                    }
                    crate::ports::SignaturePresence::Absent
                    | crate::ports::SignaturePresence::Unknown => {
                        attempt.landing_state = crate::ports::LandingState::Unknown;
                        tx.save_attempt(&attempt).await?;
                        tx.commit().await?;
                        return Ok(refund_receipt(&row, &payment, true));
                    }
                }
            }
            crate::ports::LandingState::Unknown => {
                if signer.finalized_block_height().await? > attempt.last_valid_block_height {
                    return Err(AppError::ProposalConflict(
                        "refund blockhash expired; archival proof is required",
                    ));
                }
                tx.commit().await?;
                crate::ports::outbound::same_bytes_rebroadcast(rails, &outbound, &attempt).await?;
                return Ok(refund_receipt(&row, &payment, true));
            }
            crate::ports::LandingState::Finalized
            | crate::ports::LandingState::DefinitiveFailed => {
                return Err(StoreError::Invariant(
                    "refund_sending deposit has a terminal live attempt",
                )
                .into());
            }
        }
    }

    debug_assert_eq!(row.status, DepositMachineStatus::RefundApproved);
    if !tx.attempts_for(payment.id).await?.is_empty() {
        return Err(StoreError::Invariant("approved refund already has an attempt").into());
    }
    let (signed_tx_bytes, signature, last_valid_block_height) =
        signer.sign(&payment.dest, payment.amount_micro).await?;
    let attempt = crate::ports::OutboundAttemptRow {
        id: uuid::Uuid::new_v4(),
        payment_id: payment.id,
        attempt_number: 1,
        replaces_attempt_id: None,
        signed_tx_bytes,
        signature,
        last_valid_block_height,
        landing_state: crate::ports::LandingState::Prepared,
        lease_expires_at: Some(now + LEASE),
        evidence: None,
    };
    tx.insert_attempt(&attempt).await?;
    rails
        .persist_signed(payment.id, &attempt.signed_tx_bytes, &attempt.signature)
        .await?;
    if !tx
        .cas_deposit_status(
            row.id,
            DepositMachineStatus::RefundApproved,
            DepositMachineStatus::RefundSending,
        )
        .await?
    {
        return Err(StoreError::Conflict("deposit status").into());
    }
    tx.append(Event {
        event_type: "DepositRefundSending",
        aggregate_type: "deposit",
        aggregate_id: row.id.0,
        payload: json!({ "payment_id": payment.id.to_string() }),
    })
    .await?;
    tx.commit().await?;
    rails.broadcast(payment.id).await?;
    mark_refund_broadcast(store, chain_sig).await?;
    Ok(refund_receipt(&row, &payment, false))
}

/// Replace an expired unknown refund attempt only after the startup-pinned
/// three-endpoint set proves non-landing. The old attempt becomes
/// `definitive_failed`; the replacement names it, increments the attempt
/// number, and persists its signed bytes before any broadcast.
///
/// # Errors
/// Invalid state/rail identity, non-definitive proof, signature reuse, or
/// signer/rail/store failure.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn replace_expired_refund<S: Store>(
    store: &S,
    chain_sig: &str,
    now: time::OffsetDateTime,
    observations: &[crate::ports::QuorumObservation],
    identity: &crate::ports::RailIdentity,
    rails: &dyn crate::ports::OutboundRails,
    signer: &dyn crate::ports::withdraw_send::WithdrawSigner,
) -> Result<RefundReceipt, AppError> {
    const LEASE: time::Duration = time::Duration::seconds(30);
    let mut tx = store.deposit_admission_tx().await?;
    tx.serialize_key(&format!("deposit-machine:{chain_sig}"))
        .await?;
    let row = tx
        .deposit_machine_by_sig(chain_sig)
        .await?
        .ok_or(StoreError::NotFound("deposit"))?;
    if row.status != DepositMachineStatus::RefundSending {
        return Err(StoreError::Conflict("deposit refund is not sending").into());
    }
    let payment = checked_refund_payment(tx.as_mut(), &row).await?;
    if payment.rail_fingerprint != identity.fingerprint() {
        return Err(StoreError::Invariant("refund rail identity drift").into());
    }
    let attempts = tx.attempts_for(payment.id).await?;
    let mut old = tx
        .live_attempt(payment.id)
        .await?
        .ok_or(StoreError::Invariant("refund has no live attempt"))?;
    if old.landing_state != crate::ports::LandingState::Unknown {
        return Err(StoreError::Invariant("refund replacement attempt is not unknown").into());
    }
    if signer.finalized_block_height().await? <= old.last_valid_block_height {
        return Err(AppError::ProposalConflict("blockhash has not expired"));
    }
    let verdict = refund_non_landing_verdict(
        old.last_valid_block_height,
        observations,
        &identity.rpc_endpoints,
    );
    old.evidence = Some(json!({
        "purpose": "replacement",
        "last_valid_block_height": old.last_valid_block_height,
        "observed_at": now,
        "observations": observations.iter().map(|item| json!({
            "endpoint": item.endpoint,
            "finalized_height": item.finalized_height,
            "signature_present": item.signature_present,
            "pruned": item.pruned,
        })).collect::<Vec<_>>(),
        "verdict": match verdict {
            crate::ports::NonLandingVerdict::DefinitiveFailed => "definitive_failed",
            crate::ports::NonLandingVerdict::Unknown => "unknown",
        },
    }));
    if verdict != crate::ports::NonLandingVerdict::DefinitiveFailed {
        tx.save_attempt(&old).await?;
        tx.commit().await?;
        return Err(AppError::ProposalConflict(
            "archival non-landing proof is not definitive",
        ));
    }
    old.landing_state = crate::ports::LandingState::DefinitiveFailed;
    tx.save_attempt(&old).await?;

    let (signed_tx_bytes, signature, last_valid_block_height) =
        signer.sign(&payment.dest, payment.amount_micro).await?;
    if attempts
        .iter()
        .any(|attempt| attempt.signature == signature)
    {
        return Err(StoreError::Invariant("refund replacement signature was reused").into());
    }
    let replacement = crate::ports::OutboundAttemptRow {
        id: uuid::Uuid::new_v4(),
        payment_id: payment.id,
        attempt_number: crate::ports::outbound::next_attempt_number(&attempts),
        replaces_attempt_id: Some(old.id),
        signed_tx_bytes,
        signature,
        last_valid_block_height,
        landing_state: crate::ports::LandingState::Prepared,
        lease_expires_at: Some(now + LEASE),
        evidence: None,
    };
    tx.insert_attempt(&replacement).await?;
    rails
        .persist_signed(
            payment.id,
            &replacement.signed_tx_bytes,
            &replacement.signature,
        )
        .await?;
    tx.append(Event {
        event_type: "DepositRefundAttemptReplaced",
        aggregate_type: "deposit",
        aggregate_id: row.id.0,
        payload: json!({
            "payment_id": payment.id.to_string(),
            "attempt_id": replacement.id.to_string(),
            "replaces_attempt_id": old.id.to_string(),
        }),
    })
    .await?;
    tx.commit().await?;
    rails.broadcast(payment.id).await?;
    mark_refund_broadcast(store, chain_sig).await?;
    Ok(refund_receipt(&row, &payment, true))
}

fn refund_non_landing_verdict(
    last_valid_block_height: i64,
    observations: &[crate::ports::QuorumObservation],
    expected_endpoints: &[String],
) -> crate::ports::NonLandingVerdict {
    let observed: std::collections::BTreeSet<_> = observations
        .iter()
        .map(|observation| observation.endpoint.as_str())
        .collect();
    let expected: std::collections::BTreeSet<_> =
        expected_endpoints.iter().map(String::as_str).collect();
    if observations.len() != 3
        || observed.len() != 3
        || observed != expected
        || observations
            .iter()
            .any(|observation| observation.signature_present == Some(true))
    {
        return crate::ports::NonLandingVerdict::Unknown;
    }
    crate::ports::non_landing_verdict(last_valid_block_height, observations)
}

async fn mark_refund_broadcast<S: Store>(store: &S, chain_sig: &str) -> Result<(), AppError> {
    let mut tx = store.deposit_admission_tx().await?;
    tx.serialize_key(&format!("deposit-machine:{chain_sig}"))
        .await?;
    let row = tx
        .deposit_machine_by_sig(chain_sig)
        .await?
        .ok_or(StoreError::NotFound("deposit"))?;
    if row.status != DepositMachineStatus::RefundSending {
        return Err(StoreError::Conflict("deposit refund is not sending").into());
    }
    let payment = checked_refund_payment(tx.as_mut(), &row).await?;
    let mut attempt = tx
        .live_attempt(payment.id)
        .await?
        .ok_or(StoreError::Invariant(
            "refund_sending deposit has no live attempt",
        ))?;
    if attempt.landing_state == crate::ports::LandingState::Broadcast {
        return Ok(());
    }
    if !matches!(
        attempt.landing_state,
        crate::ports::LandingState::Prepared | crate::ports::LandingState::Unknown
    ) {
        return Err(StoreError::Invariant("refund attempt cannot become broadcast").into());
    }
    attempt.landing_state = crate::ports::LandingState::Broadcast;
    tx.save_attempt(&attempt).await?;
    tx.commit().await?;
    Ok(())
}

/// Settle a source-locked refund only after verifying finalized chain receipt
/// proof against the startup-pinned rail identity and persisted attempt.
///
/// # Errors
/// Invalid state/proof, corrupt payment identity, or ledger/store failures.
#[allow(clippy::too_many_lines)]
pub async fn settle_refund<S: Store>(
    store: &S,
    chain_sig: &str,
    now: time::OffsetDateTime,
    identity: &crate::ports::RailIdentity,
    receipt: &crate::ports::ChainReceipt,
) -> Result<RefundSettlementReceipt, AppError> {
    let mut tx = store.deposit_admission_tx().await?;
    tx.serialize_key(&format!("deposit-machine:{chain_sig}"))
        .await?;
    let row = tx
        .deposit_machine_by_sig(chain_sig)
        .await?
        .ok_or(StoreError::NotFound("deposit"))?;
    let payment = checked_refund_payment(tx.as_mut(), &row).await?;
    let outbound = refund_outbound(&row, &payment);
    if payment.rail_fingerprint != identity.fingerprint() {
        return Err(StoreError::Invariant("refund rail identity drift").into());
    }
    let mut attempts = tx.attempts_for(payment.id).await?;
    let attempt = attempts
        .iter_mut()
        .find(|attempt| attempt.signature == receipt.signature)
        .ok_or(StoreError::Invariant(
            "finalized refund receipt has no persisted attempt",
        ))?;
    crate::ports::verify_chain_receipt(identity, &outbound, attempt, receipt)
        .map_err(StoreError::Invariant)?;
    if row.status == DepositMachineStatus::Refunded {
        if attempt.landing_state != crate::ports::LandingState::Finalized {
            return Err(StoreError::Invariant("refunded deposit attempt is not finalized").into());
        }
        return Ok(RefundSettlementReceipt {
            deposit_id: row.id,
            payment_id: payment.id,
            ledger_txn: row.refund_tx_id.ok_or(StoreError::Invariant(
                "refunded deposit has no ledger transaction",
            ))?,
            replayed: true,
        });
    }
    if row.status != DepositMachineStatus::RefundSending {
        return Err(StoreError::Conflict("deposit refund is not sending").into());
    }
    if !attempt.landing_state.is_live() {
        return Err(StoreError::Invariant("refund attempt is not live").into());
    }
    attempt.landing_state = crate::ports::LandingState::Finalized;
    attempt.evidence = Some(json!({
        "signature": receipt.signature,
        "mint": receipt.mint,
        "source": receipt.source,
        "dest_token_account": receipt.dest_token_account,
        "delta_micro": receipt.delta_micro,
        "commitment": receipt.commitment,
        "verified_at": now,
    }));
    tx.save_attempt(attempt).await?;
    let suspense = tx
        .account(OwnerRef::DepositSuspense, Currency::Usdc)
        .await?;
    let external = tx.account(OwnerRef::External, Currency::Usdc).await?;
    let ledger_txn = tx
        .ledger_apply(
            TxnKind::Withdrawal,
            &format!("deposit-refund:{}", row.id.0),
            &[
                Entry {
                    account: suspense,
                    amount: MicroUsd(-row.amount.0),
                },
                Entry {
                    account: external,
                    amount: row.amount,
                },
            ],
        )
        .await?;
    tx.mark_refunded(row.id, ledger_txn).await?;
    if !tx
        .cas_deposit_status(
            row.id,
            DepositMachineStatus::RefundSending,
            DepositMachineStatus::Refunded,
        )
        .await?
    {
        return Err(StoreError::Conflict("deposit status").into());
    }
    tx.insert_compliance_decision(ComplianceDecision {
        id: uuid::Uuid::new_v4(),
        subject_type: "deposit".into(),
        subject_id: row.id.0,
        kind: "refunded".into(),
        actor: "machine".into(),
        at: now,
        payload: json!({
            "payment_id": payment.id.to_string(),
            "ledger_txn": ledger_txn.to_string(),
        }),
    })
    .await?;
    tx.append(Event {
        event_type: "DepositRefunded",
        aggregate_type: "deposit",
        aggregate_id: row.id.0,
        payload: json!({
            "payment_id": payment.id.to_string(),
            "ledger_txn": ledger_txn.to_string(),
        }),
    })
    .await?;
    tx.commit().await?;
    Ok(RefundSettlementReceipt {
        deposit_id: row.id,
        payment_id: payment.id,
        ledger_txn,
        replayed: false,
    })
}

fn refund_outbound(
    row: &DepositMachineRow,
    payment: &DepositRefundPayment,
) -> crate::ports::OutboundPaymentRow {
    crate::ports::OutboundPaymentRow {
        id: payment.id,
        subject: crate::ports::OutboundSubject::DepositRefund,
        subject_id: row.id.0,
        dest: payment.dest.clone(),
        amount_micro: payment.amount_micro,
        rail_fingerprint: payment.rail_fingerprint.clone(),
    }
}

async fn checked_refund_payment(
    tx: &mut (dyn crate::ports::DepositAdmissionTx + '_),
    row: &DepositMachineRow,
) -> Result<DepositRefundPayment, StoreError> {
    let payment = tx
        .refund_payment_for_deposit(row.id)
        .await?
        .ok_or(StoreError::Invariant(
            "refund state has no outbound payment",
        ))?;
    if payment.deposit != row.id
        || payment.dest != row.source_address
        || payment.amount_micro != row.amount.0
        || payment.rail_fingerprint != row.rail_fingerprint
    {
        return Err(StoreError::Invariant(
            "deposit refund payment is not source-locked",
        ));
    }
    Ok(payment)
}

fn refund_receipt(
    row: &DepositMachineRow,
    payment: &DepositRefundPayment,
    replayed: bool,
) -> RefundReceipt {
    RefundReceipt {
        deposit_id: row.id,
        payment_id: payment.id,
        dest: payment.dest.clone(),
        replayed,
    }
}

async fn hold_admission(
    mut tx: Box<dyn crate::ports::DepositAdmissionTx + '_>,
    row: &DepositMachineRow,
    reason: &'static str,
    collected_micro: i64,
    actor: &AdminContext,
    now: time::OffsetDateTime,
) -> Result<DepositReceipt, AppError> {
    let moved = tx
        .cas_deposit_status(
            row.id,
            DepositMachineStatus::AdmissionPending,
            DepositMachineStatus::ComplianceHold,
        )
        .await?;
    if !moved {
        return Err(StoreError::Conflict("deposit status").into());
    }
    tx.insert_compliance_decision(ComplianceDecision {
        id: uuid::Uuid::new_v4(),
        subject_type: "deposit".into(),
        subject_id: row.id.0,
        kind: "compliance_hold".into(),
        actor: actor_name(actor),
        at: now,
        payload: json!({ "reason": reason }),
    })
    .await?;
    tx.append(Event {
        event_type: "DepositHeld",
        aggregate_type: "deposit",
        aggregate_id: row.id.0,
        payload: json!({ "reason": reason }),
    })
    .await?;
    insert_deposit_audit(
        tx.as_mut(),
        actor,
        row,
        reason,
        row.suspense_tx_id,
        collected_micro,
    )
    .await?;
    tx.commit().await?;
    Ok(DepositReceipt {
        deposit_id: row.id,
        ledger_txn: row.suspense_tx_id,
        replayed: false,
        collected_micro,
    })
}

async fn insert_deposit_audit(
    tx: &mut (dyn crate::ports::DepositAdmissionTx + '_),
    actor: &AdminContext,
    row: &DepositMachineRow,
    outcome: &str,
    ledger_txn: Option<uuid::Uuid>,
    collected_micro: i64,
) -> Result<(), StoreError> {
    if let Some(audit) = audit_for(
        actor,
        "faucet_deposit",
        format!("user:{}", row.user.map_or(uuid::Uuid::nil(), |user| user.0)),
        None,
        Some(json!({
            "deposit_id": row.id.0.to_string(),
            "amount_micro": row.amount.0,
            "outcome": outcome,
            "collected_micro": collected_micro,
            "ledger_txn": ledger_txn.map(|id| id.to_string()),
        })),
        None,
    ) {
        tx.audit_insert(audit).await?;
    }
    Ok(())
}

fn existing_receipt(row: &DepositMachineRow) -> DepositReceipt {
    DepositReceipt {
        deposit_id: row.id,
        ledger_txn: row.admit_tx_id.or(row.refund_tx_id).or(row.suspense_tx_id),
        replayed: true,
        collected_micro: 0,
    }
}

fn actor_name(actor: &AdminContext) -> String {
    match actor {
        AdminContext::Machine => "machine".to_string(),
        AdminContext::Admin { token_digest, .. } => token_digest.clone(),
    }
}

fn admission_hold_reason(error: &AppError) -> Option<&'static str> {
    match error {
        AppError::ComplianceHold { reason } | AppError::MoneyForbidden(reason) => Some(reason),
        AppError::DepositsPaused => Some("pause_deposits"),
        AppError::PositionCapExceeded { .. } => Some("shadow_deposit_cap"),
        _ => None,
    }
}

fn validate_observation(observed: &ObservedDeposit) -> Result<(), StoreError> {
    if observed.amount.0 <= 0 {
        return Err(StoreError::Invariant("deposit amount must be positive"));
    }
    if observed.chain_sig.is_empty()
        || observed.source_address.is_empty()
        || observed.dest_address.is_empty()
        || observed.mint.is_empty()
    {
        return Err(StoreError::Invariant(
            "deposit observation identity incomplete",
        ));
    }
    if observed.slot < 0 {
        return Err(StoreError::Invariant("deposit slot must be non-negative"));
    }
    Ok(())
}

fn same_observation(
    existing: &DepositMachineRow,
    observed: &ObservedDeposit,
    rail_fingerprint: &str,
) -> bool {
    existing.user == observed.user
        && existing.amount == observed.amount
        && existing.chain_sig == observed.chain_sig
        && existing.source_address == observed.source_address
        && existing.dest_address == observed.dest_address
        && existing.mint == observed.mint
        && existing.rail_fingerprint == rail_fingerprint
        && existing.slot == observed.slot
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::fakes::InMemoryStore;
    use time::OffsetDateTime;

    fn admin(role: crate::model::AdminRole, token: &str) -> AdminContext {
        AdminContext::Admin {
            token_digest: token.to_string(),
            role,
        }
    }

    fn cmd(user: UserId, sig: &str, key: &str) -> CreditDepositCmd {
        CreditDepositCmd {
            user,
            amount: MicroUsd(25_000_000),
            chain_sig: sig.to_string(),
            idempotency_key: key.to_string(),
        }
    }

    async fn dual_approve_refund(
        store: &InMemoryStore,
        user: UserId,
        chain_sig: &str,
    ) -> (ObservedDeposit, crate::ports::RailIdentity, RefundReceipt) {
        let identity = crate::ports::withdraw_fakes::test_rail();
        let observed = ObservedDeposit {
            user: Some(user),
            amount: MicroUsd(25_000_000),
            chain_sig: chain_sig.into(),
            source_address: crate::ports::withdraw_fakes::dest_a(),
            dest_address: identity.treasury_token_account.clone(),
            mint: identity.usdc_mint.clone(),
            slot: 17,
        };
        observe_finalized_on_rail(store, &observed, &identity.fingerprint())
            .await
            .unwrap();
        store.set_money_flag("pause_deposits", true);
        let held = admit_observed(
            store,
            chain_sig,
            OffsetDateTime::UNIX_EPOCH,
            &AdminContext::Machine,
            false,
        )
        .await
        .unwrap();
        let proposal = propose_deposit_command(
            store,
            held.deposit_id,
            true,
            "return source-locked funds".into(),
            OffsetDateTime::UNIX_EPOCH,
            &admin(crate::model::AdminRole::Finance, "finance-a"),
        )
        .await
        .unwrap();
        let approved = confirm_deposit_refund(
            store,
            proposal.id,
            OffsetDateTime::UNIX_EPOCH,
            &admin(crate::model::AdminRole::Superadmin, "super-b"),
        )
        .await
        .unwrap();
        (observed, identity, approved)
    }

    fn finalized_refund_receipt(
        identity: &crate::ports::RailIdentity,
        dest: &str,
        amount_micro: i64,
        signature: &str,
    ) -> crate::ports::ChainReceipt {
        crate::ports::ChainReceipt {
            signature: signature.into(),
            mint: identity.usdc_mint.clone(),
            source: identity.treasury_token_account.clone(),
            dest_token_account: dest.into(),
            delta_micro: amount_micro,
            commitment: "finalized".into(),
        }
    }

    fn prepared_attempt(
        payment_id: uuid::Uuid,
        attempt_number: i32,
        replaces_attempt_id: Option<uuid::Uuid>,
        signature: &str,
        now: OffsetDateTime,
    ) -> crate::ports::OutboundAttemptRow {
        crate::ports::OutboundAttemptRow {
            id: uuid::Uuid::new_v4(),
            payment_id,
            attempt_number,
            replaces_attempt_id,
            signed_tx_bytes: signature.as_bytes().to_vec(),
            signature: signature.into(),
            last_valid_block_height: 1_000,
            landing_state: crate::ports::LandingState::Prepared,
            lease_expires_at: Some(now + time::Duration::seconds(30)),
            evidence: None,
        }
    }

    async fn observe_test_deposit(
        store: &InMemoryStore,
        user: Option<UserId>,
        chain_sig: &str,
    ) -> (ObservedDeposit, crate::ports::RailIdentity) {
        let identity = crate::ports::withdraw_fakes::test_rail();
        let observed = ObservedDeposit {
            user,
            amount: MicroUsd(25_000_000),
            chain_sig: chain_sig.into(),
            source_address: crate::ports::withdraw_fakes::dest_a(),
            dest_address: identity.treasury_token_account.clone(),
            mint: identity.usdc_mint.clone(),
            slot: 19,
        };
        observe_finalized_on_rail(store, &observed, &identity.fingerprint())
            .await
            .unwrap();
        (observed, identity)
    }

    #[test]
    fn machine_edges_are_total_and_terminal_states_cannot_reopen() {
        assert!(legal_deposit_transition(
            DepositMachineStatus::ObservedFinalized,
            DepositMachineStatus::AdmissionPending
        ));
        assert!(legal_deposit_transition(
            DepositMachineStatus::ComplianceHold,
            DepositMachineStatus::RefundApproved
        ));
        assert!(!legal_deposit_transition(
            DepositMachineStatus::Admitted,
            DepositMachineStatus::ComplianceHold
        ));
        assert!(!legal_deposit_transition(
            DepositMachineStatus::AdmittedLegacy,
            DepositMachineStatus::AdmissionPending
        ));
        assert!(!legal_deposit_transition(
            DepositMachineStatus::Refunded,
            DepositMachineStatus::AdmissionPending
        ));
    }

    #[test]
    fn refund_archival_quorum_is_bound_to_all_three_startup_endpoints() {
        let identity = crate::ports::withdraw_fakes::test_rail();
        let mut observations: Vec<_> = identity
            .rpc_endpoints
            .iter()
            .map(|endpoint| crate::ports::QuorumObservation {
                endpoint: endpoint.clone(),
                finalized_height: Some(1_001),
                signature_present: Some(false),
                pruned: false,
            })
            .collect();
        assert_eq!(
            refund_non_landing_verdict(1_000, &observations, &identity.rpc_endpoints),
            crate::ports::NonLandingVerdict::DefinitiveFailed
        );
        observations[0].signature_present = Some(true);
        assert_eq!(
            refund_non_landing_verdict(1_000, &observations, &identity.rpc_endpoints),
            crate::ports::NonLandingVerdict::Unknown
        );
        observations[0].signature_present = Some(false);
        observations[0].endpoint = "unexpected".into();
        assert_eq!(
            refund_non_landing_verdict(1_000, &observations, &identity.rpc_endpoints),
            crate::ports::NonLandingVerdict::Unknown
        );
        assert_eq!(
            refund_non_landing_verdict(1_000, &observations[..2], &identity.rpc_endpoints),
            crate::ports::NonLandingVerdict::Unknown
        );
    }

    #[test]
    fn observation_validation_and_hold_reason_mapping_fail_closed() {
        let valid = ObservedDeposit {
            user: None,
            amount: MicroUsd(1),
            chain_sig: "sig".into(),
            source_address: "source".into(),
            dest_address: "dest".into(),
            mint: "mint".into(),
            slot: 0,
        };
        let mut invalid = valid.clone();
        invalid.amount = MicroUsd(0);
        assert_eq!(
            validate_observation(&invalid),
            Err(StoreError::Invariant("deposit amount must be positive"))
        );
        for clear_field in 0..4 {
            let mut invalid = valid.clone();
            match clear_field {
                0 => invalid.chain_sig.clear(),
                1 => invalid.source_address.clear(),
                2 => invalid.dest_address.clear(),
                _ => invalid.mint.clear(),
            }
            assert_eq!(
                validate_observation(&invalid),
                Err(StoreError::Invariant(
                    "deposit observation identity incomplete"
                ))
            );
        }
        let mut invalid = valid;
        invalid.slot = -1;
        assert_eq!(
            validate_observation(&invalid),
            Err(StoreError::Invariant("deposit slot must be non-negative"))
        );
        assert_eq!(
            admission_hold_reason(&AppError::ComplianceHold { reason: "held" }),
            Some("held")
        );
        assert_eq!(
            admission_hold_reason(&AppError::MoneyForbidden("forbidden")),
            Some("forbidden")
        );
        assert_eq!(
            admission_hold_reason(&AppError::DepositsPaused),
            Some("pause_deposits")
        );
        assert_eq!(
            admission_hold_reason(&AppError::PositionCapExceeded {
                cap_micro: 1,
                tier: 0,
            }),
            Some("shadow_deposit_cap")
        );
        assert_eq!(admission_hold_reason(&AppError::Overflow), None);
    }

    #[test]
    fn deposit_proposal_validation_rejects_kind_lifecycle_window_and_payload_drift() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let row = DepositMachineRow {
            id: DepositId(uuid::Uuid::new_v4()),
            user: Some(UserId(uuid::Uuid::new_v4())),
            amount: MicroUsd(7_000_000),
            chain_sig: "proposal-sig".into(),
            source_address: "immutable-source".into(),
            dest_address: "treasury".into(),
            mint: "usdc".into(),
            rail_fingerprint: "rail-v1".into(),
            slot: 17,
            status: DepositMachineStatus::ComplianceHold,
            suspense_tx_id: Some(uuid::Uuid::new_v4()),
            admit_tx_id: None,
            refund_tx_id: None,
        };
        let payload_hash = crate::money::admin::payload_hash(
            deposit_command_payload(MANUAL_DEPOSIT_ADMIT, &row).as_bytes(),
        );
        let proposal = crate::money::admin::new_proposal(
            MANUAL_DEPOSIT_ADMIT,
            row.id.0,
            payload_hash,
            "finance-a".into(),
            "reviewed".into(),
            format!("{MANUAL_DEPOSIT_ADMIT}:{}", row.id.0),
            time::Duration::ZERO,
            now,
        );

        assert!(validate_deposit_proposal(&proposal, MANUAL_DEPOSIT_ADMIT, "super-b", now).is_ok());
        assert_eq!(
            validate_deposit_proposal(&proposal, DEPOSIT_REFUND, "super-b", now),
            Err(AppError::ProposalConflict("wrong deposit proposal kind"))
        );

        let mut confirmed = proposal.clone();
        confirmed.status = crate::money::ProposalStatus::Confirmed;
        assert!(validate_confirmed_deposit_proposal(&confirmed).is_ok());
        assert_eq!(
            validate_confirmed_deposit_proposal(&proposal),
            Err(AppError::ProposalConflict("proposal confirmation lost"))
        );
        assert_eq!(
            validate_deposit_proposal(&confirmed, MANUAL_DEPOSIT_ADMIT, "super-b", now),
            Err(AppError::ProposalConflict("proposal is not pending"))
        );
        assert_eq!(
            validate_deposit_proposal(
                &proposal,
                MANUAL_DEPOSIT_ADMIT,
                "super-b",
                now - time::Duration::seconds(1),
            ),
            Err(AppError::ProposalConflict("proposal not confirmable"))
        );
        assert_eq!(
            validate_deposit_proposal(
                &proposal,
                MANUAL_DEPOSIT_ADMIT,
                "super-b",
                proposal.expires_at + time::Duration::seconds(1),
            ),
            Err(AppError::ProposalConflict("proposal not confirmable"))
        );
        assert!(validate_deposit_proposal_payload(&proposal, &row).is_ok());

        let mut drifted = row;
        drifted.source_address = "different-source".into();
        assert_eq!(
            validate_deposit_proposal_payload(&proposal, &drifted),
            Err(AppError::ProposalConflict("deposit proposal drift"))
        );
    }

    #[tokio::test]
    async fn deposit_credits_user_and_emits() {
        let store = InMemoryStore::new();
        let user = UserId(uuid::Uuid::new_v4());
        let uc = CreditDeposit { store: &store };
        let receipt = uc.execute(cmd(user, "sig-1", "dep-1")).await.unwrap();
        assert!(!receipt.replayed);
        assert!(receipt.ledger_txn.is_some());
        assert_eq!(
            store.balance_of(OwnerRef::User(user), Currency::Usdc),
            Some(MicroUsd(25_000_000))
        );
        assert_eq!(
            store.balance_of(OwnerRef::External, Currency::Usdc),
            Some(MicroUsd(-25_000_000))
        );
        let events = store.outbox();
        assert!(events.iter().any(|e| e.event_type == "DepositObserved"));
        assert!(events.iter().any(|e| e.event_type == "DepositAdmitted"));
    }

    #[tokio::test]
    async fn chain_sig_replay_returns_original_without_writing() {
        let store = InMemoryStore::new();
        let user = UserId(uuid::Uuid::new_v4());
        let uc = CreditDeposit { store: &store };
        let first = uc.execute(cmd(user, "sig-1", "dep-1")).await.unwrap();
        let snapshot = store.snapshot();
        // Same signature through a DIFFERENT idempotency key (listener
        // restart shape): still deduped.
        let second = uc.execute(cmd(user, "sig-1", "dep-2")).await.unwrap();
        assert!(second.replayed);
        assert_eq!(second.deposit_id, first.deposit_id);
        assert_eq!(snapshot, store.snapshot());
    }

    #[tokio::test]
    async fn concurrent_same_sig_credits_exactly_once() {
        let store = InMemoryStore::new();
        let user = UserId(uuid::Uuid::new_v4());
        let uc = CreditDeposit { store: &store };
        let (a, b) = tokio::join!(
            uc.execute(cmd(user, "sig-race", "dep-a")),
            uc.execute(cmd(user, "sig-race", "dep-b"))
        );
        let wrote = usize::from(a.as_ref().is_ok_and(|r| !r.replayed))
            + usize::from(b.as_ref().is_ok_and(|r| !r.replayed));
        assert_eq!(
            wrote, 1,
            "exactly one racing deposit may write: {a:?} / {b:?}"
        );
        assert_eq!(
            store.balance_of(OwnerRef::User(user), Currency::Usdc),
            Some(MicroUsd(25_000_000)),
            "no double credit"
        );
    }

    #[tokio::test]
    async fn paused_admission_keeps_the_finalized_inflow_in_suspense() {
        let store = InMemoryStore::new();
        let user = UserId(uuid::Uuid::new_v4());
        store.set_money_flag("pause_deposits", true);
        let receipt = CreditDeposit { store: &store }
            .execute(cmd(user, "sig-paused", "dep-paused"))
            .await
            .unwrap();

        assert!(!receipt.replayed);
        assert_eq!(receipt.collected_micro, 0);
        assert_eq!(
            store.balance_of(OwnerRef::DepositSuspense, Currency::Usdc),
            Some(MicroUsd(25_000_000))
        );
        assert_eq!(store.balance_of(OwnerRef::User(user), Currency::Usdc), None);
        assert!(store
            .outbox()
            .iter()
            .any(|event| event.event_type == "DepositHeld"));
    }

    #[tokio::test]
    async fn held_deposit_manual_admission_requires_distinct_dual_control() {
        let store = InMemoryStore::new();
        let user = UserId(uuid::Uuid::new_v4());
        store.set_money_flag("pause_deposits", true);
        let held = CreditDeposit { store: &store }
            .execute(cmd(user, "sig-manual-admit", "dep-manual-admit"))
            .await
            .unwrap();

        assert!(propose_deposit_command(
            &store,
            held.deposit_id,
            false,
            "reviewed observation".into(),
            OffsetDateTime::UNIX_EPOCH,
            &AdminContext::Machine,
        )
        .await
        .is_err());
        let finance = admin(crate::model::AdminRole::Finance, "finance-a");
        assert!(propose_deposit_command(
            &store,
            held.deposit_id,
            false,
            "   ".into(),
            OffsetDateTime::UNIX_EPOCH,
            &finance,
        )
        .await
        .is_err());
        let proposal = propose_deposit_command(
            &store,
            held.deposit_id,
            false,
            "reviewed observation".into(),
            OffsetDateTime::UNIX_EPOCH,
            &finance,
        )
        .await
        .unwrap();
        assert_eq!(
            propose_deposit_command(
                &store,
                held.deposit_id,
                false,
                "reviewed observation".into(),
                OffsetDateTime::UNIX_EPOCH,
                &finance,
            )
            .await
            .unwrap()
            .id,
            proposal.id,
        );
        assert!(confirm_manual_deposit_admission(
            &store,
            proposal.id,
            OffsetDateTime::UNIX_EPOCH,
            &admin(crate::model::AdminRole::Superadmin, "finance-a"),
        )
        .await
        .is_err());

        let admitted = confirm_manual_deposit_admission(
            &store,
            proposal.id,
            OffsetDateTime::UNIX_EPOCH,
            &admin(crate::model::AdminRole::Superadmin, "super-b"),
        )
        .await
        .unwrap();
        assert_eq!(admitted.deposit_id, held.deposit_id);
        assert_eq!(
            store.balance_of(OwnerRef::User(user), Currency::Usdc),
            Some(MicroUsd(25_000_000))
        );
        assert_eq!(
            store.balance_of(OwnerRef::DepositSuspense, Currency::Usdc),
            Some(MicroUsd(0))
        );
        let mut inspect = store.deposit_admission_tx().await.unwrap();
        assert_eq!(
            inspect
                .deposit_machine_by_id(held.deposit_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            DepositMachineStatus::Admitted
        );
        let confirmed = inspect.get_proposal(proposal.id).await.unwrap();
        assert_eq!(confirmed.status, crate::money::ProposalStatus::Confirmed);
        assert_eq!(confirmed.confirmer_token_id.as_deref(), Some("super-b"));
        drop(inspect);
        let audits = crate::ports::OpsQueries::audit_page(&store, None, 20)
            .await
            .unwrap();
        assert!(audits
            .iter()
            .any(|row| row.action.action == "propose_manual_deposit_admit"));
        assert!(audits
            .iter()
            .any(|row| row.action.action == "confirm_manual_deposit_admit"));
    }

    #[tokio::test]
    async fn expired_manual_deposit_proposal_has_no_money_effect() {
        let store = InMemoryStore::new();
        let user = UserId(uuid::Uuid::new_v4());
        store.set_money_flag("pause_deposits", true);
        let held = CreditDeposit { store: &store }
            .execute(cmd(user, "sig-manual-expired", "dep-manual-expired"))
            .await
            .unwrap();
        let proposal = propose_deposit_command(
            &store,
            held.deposit_id,
            false,
            "reviewed observation".into(),
            OffsetDateTime::UNIX_EPOCH,
            &admin(crate::model::AdminRole::Finance, "finance-a"),
        )
        .await
        .unwrap();

        assert!(confirm_manual_deposit_admission(
            &store,
            proposal.id,
            OffsetDateTime::UNIX_EPOCH + time::Duration::minutes(16),
            &admin(crate::model::AdminRole::Superadmin, "super-b"),
        )
        .await
        .is_err());
        assert_eq!(store.balance_of(OwnerRef::User(user), Currency::Usdc), None);
        assert_eq!(
            store.balance_of(OwnerRef::DepositSuspense, Currency::Usdc),
            Some(MicroUsd(25_000_000))
        );
    }

    #[tokio::test]
    async fn deposit_command_proposal_rejects_machine_state_and_replay_drift() {
        let admitted_store = InMemoryStore::new();
        let admitted_user = UserId(uuid::Uuid::new_v4());
        let admitted = CreditDeposit {
            store: &admitted_store,
        }
        .execute(cmd(admitted_user, "sig-not-held", "dep-not-held"))
        .await
        .unwrap();
        assert_eq!(
            propose_deposit_command(
                &admitted_store,
                admitted.deposit_id,
                false,
                "cannot reopen".into(),
                OffsetDateTime::UNIX_EPOCH,
                &admin(crate::model::AdminRole::Finance, "finance-a"),
            )
            .await
            .unwrap_err(),
            AppError::Store(StoreError::Conflict("deposit is not held"))
        );

        let drift_store = InMemoryStore::new();
        let drift_user = UserId(uuid::Uuid::new_v4());
        drift_store.set_money_flag("pause_deposits", true);
        let drifted = CreditDeposit {
            store: &drift_store,
        }
        .execute(cmd(drift_user, "sig-proposal-drift", "dep-proposal-drift"))
        .await
        .unwrap();
        let replay_key = format!("{MANUAL_DEPOSIT_ADMIT}:{}", drifted.deposit_id.0);
        let poisoned = crate::money::admin::new_proposal(
            MANUAL_DEPOSIT_ADMIT,
            drifted.deposit_id.0,
            "wrong-payload".into(),
            "finance-a".into(),
            "poison replay".into(),
            replay_key,
            time::Duration::ZERO,
            OffsetDateTime::UNIX_EPOCH,
        );
        let mut poison_tx = drift_store.deposit_admission_tx().await.unwrap();
        poison_tx.insert_proposal(poisoned).await.unwrap();
        poison_tx.commit().await.unwrap();
        assert_eq!(
            propose_deposit_command(
                &drift_store,
                drifted.deposit_id,
                false,
                "reviewed".into(),
                OffsetDateTime::UNIX_EPOCH,
                &admin(crate::model::AdminRole::Finance, "finance-a"),
            )
            .await
            .unwrap_err(),
            AppError::ProposalConflict("deposit proposal drift")
        );

        let state_store = InMemoryStore::new();
        let state_user = UserId(uuid::Uuid::new_v4());
        state_store.set_money_flag("pause_deposits", true);
        let held = CreditDeposit {
            store: &state_store,
        }
        .execute(cmd(state_user, "sig-confirm-state", "dep-confirm-state"))
        .await
        .unwrap();
        let proposal = propose_deposit_command(
            &state_store,
            held.deposit_id,
            false,
            "reviewed".into(),
            OffsetDateTime::UNIX_EPOCH,
            &admin(crate::model::AdminRole::Finance, "finance-a"),
        )
        .await
        .unwrap();
        state_store.set_money_flag("pause_deposits", false);
        reevaluate_held(
            &state_store,
            "sig-confirm-state",
            OffsetDateTime::UNIX_EPOCH,
        )
        .await
        .unwrap();
        assert_eq!(
            confirm_manual_deposit_admission(
                &state_store,
                proposal.id,
                OffsetDateTime::UNIX_EPOCH,
                &admin(crate::model::AdminRole::Superadmin, "super-b"),
            )
            .await
            .unwrap_err(),
            AppError::Store(StoreError::Conflict("deposit is not held"))
        );
    }

    #[tokio::test]
    async fn held_deposit_refund_proposal_is_distinct_and_source_locked() {
        let store = InMemoryStore::new();
        let user = UserId(uuid::Uuid::new_v4());
        let identity = crate::ports::withdraw_fakes::test_rail();
        let source = crate::ports::withdraw_fakes::dest_a();
        store.set_money_flag("pause_deposits", true);
        let observed = ObservedDeposit {
            user: Some(user),
            amount: MicroUsd(25_000_000),
            chain_sig: "sig-dual-refund".into(),
            source_address: source.clone(),
            dest_address: identity.treasury_token_account.clone(),
            mint: identity.usdc_mint.clone(),
            slot: 8,
        };
        let observation = observe_finalized_on_rail(&store, &observed, &identity.fingerprint())
            .await
            .unwrap();
        admit_observed(
            &store,
            &observed.chain_sig,
            OffsetDateTime::UNIX_EPOCH,
            &AdminContext::Machine,
            false,
        )
        .await
        .unwrap();
        let proposal = propose_deposit_command(
            &store,
            observation.deposit_id,
            true,
            "return source-locked funds".into(),
            OffsetDateTime::UNIX_EPOCH,
            &admin(crate::model::AdminRole::Finance, "finance-a"),
        )
        .await
        .unwrap();
        assert!(confirm_manual_deposit_admission(
            &store,
            proposal.id,
            OffsetDateTime::UNIX_EPOCH,
            &admin(crate::model::AdminRole::Superadmin, "super-wrong-kind"),
        )
        .await
        .is_err());
        assert!(confirm_deposit_refund(
            &store,
            proposal.id,
            OffsetDateTime::UNIX_EPOCH,
            &admin(crate::model::AdminRole::Superadmin, "finance-a"),
        )
        .await
        .is_err());

        let approved = confirm_deposit_refund(
            &store,
            proposal.id,
            OffsetDateTime::UNIX_EPOCH,
            &admin(crate::model::AdminRole::Superadmin, "super-b"),
        )
        .await
        .unwrap();
        assert!(confirm_deposit_refund(
            &store,
            proposal.id,
            OffsetDateTime::UNIX_EPOCH,
            &admin(crate::model::AdminRole::Superadmin, "super-c"),
        )
        .await
        .is_err());
        assert_eq!(approved.dest, source);
        let mut inspect = store.deposit_admission_tx().await.unwrap();
        let row = inspect
            .deposit_machine_by_id(observation.deposit_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, DepositMachineStatus::RefundApproved);
        let payment = inspect
            .refund_payment_for_deposit(row.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(payment.dest, source);
        assert_eq!(payment.rail_fingerprint, identity.fingerprint());
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn held_deposit_refund_is_source_locked_and_settles_suspense_only() {
        let store = InMemoryStore::new();
        let user = UserId(uuid::Uuid::new_v4());
        let identity = crate::ports::withdraw_fakes::test_rail();
        let source = crate::ports::withdraw_fakes::dest_a();
        store.set_money_flag("pause_deposits", true);
        let observed = ObservedDeposit {
            user: Some(user),
            amount: MicroUsd(25_000_000),
            chain_sig: "sig-refund".into(),
            source_address: source.clone(),
            dest_address: identity.treasury_token_account.clone(),
            mint: identity.usdc_mint.clone(),
            slot: 7,
        };
        let observation = observe_finalized_on_rail(&store, &observed, &identity.fingerprint())
            .await
            .unwrap();
        admit_observed(
            &store,
            &observed.chain_sig,
            OffsetDateTime::UNIX_EPOCH,
            &AdminContext::Machine,
            false,
        )
        .await
        .unwrap();
        store.set_money_flag("pause_deposits", false);

        let proposal = propose_deposit_command(
            &store,
            observation.deposit_id,
            true,
            "return source-locked funds".into(),
            OffsetDateTime::UNIX_EPOCH,
            &admin(crate::model::AdminRole::Finance, "finance-a"),
        )
        .await
        .unwrap();
        let approved = confirm_deposit_refund(
            &store,
            proposal.id,
            OffsetDateTime::UNIX_EPOCH,
            &admin(crate::model::AdminRole::Superadmin, "super-b"),
        )
        .await
        .unwrap();
        assert_eq!(approved.dest, source);
        let mut inspect = store.deposit_admission_tx().await.unwrap();
        let row = inspect
            .deposit_machine_by_sig("sig-refund")
            .await
            .unwrap()
            .unwrap();
        let payment = inspect
            .refund_payment_for_deposit(row.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.rail_fingerprint, identity.fingerprint());
        assert_eq!(payment.rail_fingerprint, row.rail_fingerprint);
        drop(inspect);
        let rails = crate::ports::withdraw_fakes::FakeRails::default();
        let signer = crate::ports::withdraw_send::FakeSigner::new("deposit-refund-signature");
        let receipt = crate::ports::ChainReceipt {
            signature: signer.signature.clone(),
            mint: identity.usdc_mint.clone(),
            source: identity.treasury_token_account.clone(),
            dest_token_account: approved.dest.clone(),
            delta_micro: observed.amount.0,
            commitment: "finalized".into(),
        };
        assert!(settle_refund(
            &store,
            "sig-refund",
            OffsetDateTime::UNIX_EPOCH,
            &identity,
            &receipt,
        )
        .await
        .is_err());
        assert_eq!(
            admit_observed(
                &store,
                "sig-refund",
                OffsetDateTime::UNIX_EPOCH,
                &AdminContext::Machine,
                true,
            )
            .await
            .unwrap_err(),
            AppError::Store(StoreError::Conflict("deposit refund already selected"))
        );
        let sending = begin_refund_send(
            &store,
            "sig-refund",
            OffsetDateTime::UNIX_EPOCH,
            &rails,
            &signer,
        )
        .await
        .unwrap();
        assert_eq!(sending.payment_id, approved.payment_id);
        assert!(
            begin_refund_send(
                &store,
                "sig-refund",
                OffsetDateTime::UNIX_EPOCH,
                &rails,
                &signer,
            )
            .await
            .unwrap()
            .replayed
        );
        assert_eq!(
            rails.persisted_bytes(approved.payment_id).as_deref(),
            Some(signer.bytes.as_slice())
        );
        let settled = settle_refund(
            &store,
            "sig-refund",
            OffsetDateTime::UNIX_EPOCH,
            &identity,
            &receipt,
        )
        .await
        .unwrap();
        assert!(!settled.replayed);
        assert_eq!(
            store.balance_of(OwnerRef::DepositSuspense, Currency::Usdc),
            Some(MicroUsd(0))
        );
        assert_eq!(
            store.balance_of(OwnerRef::External, Currency::Usdc),
            Some(MicroUsd(0))
        );
        assert_eq!(store.balance_of(OwnerRef::User(user), Currency::Usdc), None);
        assert!(
            settle_refund(
                &store,
                "sig-refund",
                OffsetDateTime::UNIX_EPOCH,
                &identity,
                &receipt,
            )
            .await
            .unwrap()
            .replayed
        );
    }

    #[tokio::test]
    async fn refund_persist_failure_rolls_back_attempt_and_send_claim() {
        let store = InMemoryStore::new();
        let user = UserId(uuid::Uuid::new_v4());
        let (_, _, approved) = dual_approve_refund(&store, user, "sig-refund-persist-fail").await;
        let rails = crate::ports::withdraw_fakes::FakeRails::default();
        *rails.fail_persist.lock() = true;
        let signer = crate::ports::withdraw_send::FakeSigner::new("refund-persist-fail");

        assert!(begin_refund_send(
            &store,
            "sig-refund-persist-fail",
            OffsetDateTime::UNIX_EPOCH,
            &rails,
            &signer,
        )
        .await
        .is_err());
        let mut inspect = store.deposit_admission_tx().await.unwrap();
        let row = inspect
            .deposit_machine_by_sig("sig-refund-persist-fail")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, DepositMachineStatus::RefundApproved);
        assert!(inspect
            .attempts_for(approved.payment_id)
            .await
            .unwrap()
            .is_empty());
        assert!(rails.broadcast.lock().is_empty());
    }

    #[tokio::test]
    async fn refund_broadcast_failure_recovers_with_same_persisted_bytes_after_lease() {
        let store = InMemoryStore::new();
        let user = UserId(uuid::Uuid::new_v4());
        let (_, _, approved) = dual_approve_refund(&store, user, "sig-refund-broadcast-fail").await;
        let rails = crate::ports::withdraw_fakes::FakeRails::default();
        *rails.fail_broadcast.lock() = true;
        let signer = crate::ports::withdraw_send::FakeSigner::new("refund-broadcast-fail");

        assert!(begin_refund_send(
            &store,
            "sig-refund-broadcast-fail",
            OffsetDateTime::UNIX_EPOCH,
            &rails,
            &signer,
        )
        .await
        .is_err());
        assert_eq!(
            rails.persisted_bytes(approved.payment_id).as_deref(),
            Some(signer.bytes.as_slice())
        );
        let mut inspect = store.deposit_admission_tx().await.unwrap();
        let row = inspect
            .deposit_machine_by_sig("sig-refund-broadcast-fail")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, DepositMachineStatus::RefundSending);
        let attempts = inspect.attempts_for(approved.payment_id).await.unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(
            attempts[0].landing_state,
            crate::ports::LandingState::Prepared
        );
        drop(inspect);

        assert!(begin_refund_send(
            &store,
            "sig-refund-broadcast-fail",
            OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(29),
            &rails,
            &signer,
        )
        .await
        .is_err());
        *rails.fail_broadcast.lock() = false;
        assert!(
            begin_refund_send(
                &store,
                "sig-refund-broadcast-fail",
                OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(31),
                &rails,
                &signer,
            )
            .await
            .unwrap()
            .replayed
        );
        let mut inspect = store.deposit_admission_tx().await.unwrap();
        let attempts = inspect.attempts_for(approved.payment_id).await.unwrap();
        assert_eq!(attempts.len(), 1, "recovery must never re-sign");
        assert_eq!(attempts[0].signed_tx_bytes, signer.bytes);
        assert_eq!(
            attempts[0].landing_state,
            crate::ports::LandingState::Broadcast
        );
    }

    #[tokio::test]
    async fn bad_refund_receipt_cannot_release_suspense_or_finalize_attempt() {
        let store = InMemoryStore::new();
        let user = UserId(uuid::Uuid::new_v4());
        let (observed, identity, approved) =
            dual_approve_refund(&store, user, "sig-refund-bad-receipt").await;
        let rails = crate::ports::withdraw_fakes::FakeRails::default();
        let signer = crate::ports::withdraw_send::FakeSigner::new("refund-bad-receipt");
        begin_refund_send(
            &store,
            &observed.chain_sig,
            OffsetDateTime::UNIX_EPOCH,
            &rails,
            &signer,
        )
        .await
        .unwrap();
        let receipt = crate::ports::ChainReceipt {
            signature: signer.signature.clone(),
            mint: identity.usdc_mint.clone(),
            source: identity.treasury_token_account.clone(),
            dest_token_account: crate::ports::withdraw_fakes::dest_b(),
            delta_micro: observed.amount.0,
            commitment: "finalized".into(),
        };

        assert!(settle_refund(
            &store,
            &observed.chain_sig,
            OffsetDateTime::UNIX_EPOCH,
            &identity,
            &receipt,
        )
        .await
        .is_err());
        assert_eq!(
            store.balance_of(OwnerRef::DepositSuspense, Currency::Usdc),
            Some(observed.amount)
        );
        let mut inspect = store.deposit_admission_tx().await.unwrap();
        let row = inspect
            .deposit_machine_by_sig(&observed.chain_sig)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, DepositMachineStatus::RefundSending);
        assert!(inspect
            .attempts_for(approved.payment_id)
            .await
            .unwrap()
            .iter()
            .all(|attempt| attempt.landing_state != crate::ports::LandingState::Finalized));
    }

    #[tokio::test]
    async fn refunded_and_present_send_recovery_are_replay_stable() {
        let settled_store = InMemoryStore::new();
        let settled_user = UserId(uuid::Uuid::new_v4());
        let (observed, identity, approved) =
            dual_approve_refund(&settled_store, settled_user, "sig-refunded-replay").await;
        let rails = crate::ports::withdraw_fakes::FakeRails::default();
        let signer = crate::ports::withdraw_send::FakeSigner::new("refund-replay-finalized");
        begin_refund_send(
            &settled_store,
            &observed.chain_sig,
            OffsetDateTime::UNIX_EPOCH,
            &rails,
            &signer,
        )
        .await
        .unwrap();
        mark_refund_broadcast(&settled_store, &observed.chain_sig)
            .await
            .unwrap();
        let receipt = finalized_refund_receipt(
            &identity,
            &approved.dest,
            observed.amount.0,
            &signer.signature,
        );
        settle_refund(
            &settled_store,
            &observed.chain_sig,
            OffsetDateTime::UNIX_EPOCH,
            &identity,
            &receipt,
        )
        .await
        .unwrap();
        assert!(
            begin_refund_send(
                &settled_store,
                &observed.chain_sig,
                OffsetDateTime::UNIX_EPOCH,
                &rails,
                &signer,
            )
            .await
            .unwrap()
            .replayed
        );

        let present_store = InMemoryStore::new();
        let present_user = UserId(uuid::Uuid::new_v4());
        let (present_observed, present_identity, _) =
            dual_approve_refund(&present_store, present_user, "sig-refund-present").await;
        assert_eq!(
            mark_refund_broadcast(&present_store, &present_observed.chain_sig)
                .await
                .unwrap_err(),
            AppError::Store(StoreError::Conflict("deposit refund is not sending"))
        );
        let present_rails = crate::ports::withdraw_fakes::FakeRails::default();
        *present_rails.fail_broadcast.lock() = true;
        let present_signer = crate::ports::withdraw_send::FakeSigner::new("refund-present");
        assert!(begin_refund_send(
            &present_store,
            &present_observed.chain_sig,
            OffsetDateTime::UNIX_EPOCH,
            &present_rails,
            &present_signer,
        )
        .await
        .is_err());
        *present_rails.fail_broadcast.lock() = false;
        present_signer.set_presence(crate::ports::SignaturePresence::Present);
        assert!(
            begin_refund_send(
                &present_store,
                &present_observed.chain_sig,
                OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(31),
                &present_rails,
                &present_signer,
            )
            .await
            .unwrap()
            .replayed
        );
        assert_eq!(
            replace_expired_refund(
                &present_store,
                &present_observed.chain_sig,
                OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(32),
                &[],
                &present_identity,
                &present_rails,
                &present_signer,
            )
            .await
            .unwrap_err(),
            AppError::Store(StoreError::Invariant(
                "refund replacement attempt is not unknown"
            ))
        );
    }

    #[tokio::test]
    async fn unknown_refund_requires_proof_after_expiry_and_rebroadcasts_before_it() {
        let store = InMemoryStore::new();
        let user = UserId(uuid::Uuid::new_v4());
        let (observed, _, approved) =
            dual_approve_refund(&store, user, "sig-refund-unknown-retry").await;
        let rails = crate::ports::withdraw_fakes::FakeRails::default();
        *rails.fail_broadcast.lock() = true;
        let signer = crate::ports::withdraw_send::FakeSigner::new("refund-unknown-retry");
        assert!(begin_refund_send(
            &store,
            &observed.chain_sig,
            OffsetDateTime::UNIX_EPOCH,
            &rails,
            &signer,
        )
        .await
        .is_err());
        *rails.fail_broadcast.lock() = false;
        signer.set_presence(crate::ports::SignaturePresence::Unknown);
        begin_refund_send(
            &store,
            &observed.chain_sig,
            OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(31),
            &rails,
            &signer,
        )
        .await
        .unwrap();

        signer.set_finalized_height(1_001);
        assert_eq!(
            begin_refund_send(
                &store,
                &observed.chain_sig,
                OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(32),
                &rails,
                &signer,
            )
            .await
            .unwrap_err(),
            AppError::ProposalConflict("refund blockhash expired; archival proof is required")
        );
        signer.set_finalized_height(999);
        assert!(
            begin_refund_send(
                &store,
                &observed.chain_sig,
                OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(32),
                &rails,
                &signer,
            )
            .await
            .unwrap()
            .replayed
        );
        assert_eq!(
            rails.persisted_bytes(approved.payment_id).as_deref(),
            Some(signer.bytes.as_slice())
        );
        let mut inspect = store.deposit_admission_tx().await.unwrap();
        assert_eq!(
            inspect
                .live_attempt(approved.payment_id)
                .await
                .unwrap()
                .unwrap()
                .landing_state,
            crate::ports::LandingState::Unknown,
            "same-bytes rebroadcast cannot manufacture a positive landing signal"
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn corrupt_refund_payment_and_attempt_lineage_fail_closed() {
        let store = InMemoryStore::new();
        let user = UserId(uuid::Uuid::new_v4());
        let (observed, identity, approved) =
            dual_approve_refund(&store, user, "sig-refund-corrupt-lineage").await;
        let mut corrupt_tx = store.deposit_admission_tx().await.unwrap();
        let row = corrupt_tx
            .deposit_machine_by_sig(&observed.chain_sig)
            .await
            .unwrap()
            .unwrap();
        let payment = corrupt_tx
            .refund_payment_for_deposit(row.id)
            .await
            .unwrap()
            .unwrap();
        let mut source_drift = row.clone();
        source_drift.source_address = "attacker-dest".into();
        assert_eq!(
            checked_refund_payment(corrupt_tx.as_mut(), &source_drift)
                .await
                .unwrap_err(),
            StoreError::Invariant("deposit refund payment is not source-locked")
        );
        let attempt = prepared_attempt(
            payment.id,
            1,
            None,
            "corrupt-approved-attempt",
            OffsetDateTime::UNIX_EPOCH,
        );
        corrupt_tx.insert_attempt(&attempt).await.unwrap();
        corrupt_tx.commit().await.unwrap();

        let rails = crate::ports::withdraw_fakes::FakeRails::default();
        let signer = crate::ports::withdraw_send::FakeSigner::new("unused-new-signature");
        assert_eq!(
            begin_refund_send(
                &store,
                &observed.chain_sig,
                OffsetDateTime::UNIX_EPOCH,
                &rails,
                &signer,
            )
            .await
            .unwrap_err(),
            AppError::Store(StoreError::Invariant(
                "approved refund already has an attempt"
            ))
        );
        let receipt = finalized_refund_receipt(
            &identity,
            &approved.dest,
            observed.amount.0,
            &attempt.signature,
        );
        let mut drifted_identity = identity.clone();
        drifted_identity.treasury_owner = "different-owner".into();
        assert_eq!(
            settle_refund(
                &store,
                &observed.chain_sig,
                OffsetDateTime::UNIX_EPOCH,
                &drifted_identity,
                &receipt,
            )
            .await
            .unwrap_err(),
            AppError::Store(StoreError::Invariant("refund rail identity drift"))
        );
        assert_eq!(
            settle_refund(
                &store,
                &observed.chain_sig,
                OffsetDateTime::UNIX_EPOCH,
                &identity,
                &receipt,
            )
            .await
            .unwrap_err(),
            AppError::Store(StoreError::Conflict("deposit refund is not sending"))
        );

        let refunded_store = InMemoryStore::new();
        let refunded_user = UserId(uuid::Uuid::new_v4());
        let (refunded_observed, refunded_identity, refunded) = dual_approve_refund(
            &refunded_store,
            refunded_user,
            "sig-refund-terminal-corrupt",
        )
        .await;
        let refunded_rails = crate::ports::withdraw_fakes::FakeRails::default();
        let refunded_signer = crate::ports::withdraw_send::FakeSigner::new("refund-terminal-first");
        begin_refund_send(
            &refunded_store,
            &refunded_observed.chain_sig,
            OffsetDateTime::UNIX_EPOCH,
            &refunded_rails,
            &refunded_signer,
        )
        .await
        .unwrap();
        let finalized = finalized_refund_receipt(
            &refunded_identity,
            &refunded.dest,
            refunded_observed.amount.0,
            &refunded_signer.signature,
        );
        settle_refund(
            &refunded_store,
            &refunded_observed.chain_sig,
            OffsetDateTime::UNIX_EPOCH,
            &refunded_identity,
            &finalized,
        )
        .await
        .unwrap();
        let mut second_tx = refunded_store.deposit_admission_tx().await.unwrap();
        let first = second_tx
            .attempts_for(refunded.payment_id)
            .await
            .unwrap()
            .remove(0);
        let second = prepared_attempt(
            refunded.payment_id,
            2,
            Some(first.id),
            "refund-terminal-second",
            OffsetDateTime::UNIX_EPOCH,
        );
        second_tx.insert_attempt(&second).await.unwrap();
        second_tx.commit().await.unwrap();
        let second_receipt = finalized_refund_receipt(
            &refunded_identity,
            &refunded.dest,
            refunded_observed.amount.0,
            &second.signature,
        );
        assert_eq!(
            settle_refund(
                &refunded_store,
                &refunded_observed.chain_sig,
                OffsetDateTime::UNIX_EPOCH,
                &refunded_identity,
                &second_receipt,
            )
            .await
            .unwrap_err(),
            AppError::Store(StoreError::Invariant(
                "refunded deposit attempt is not finalized"
            ))
        );
    }

    #[tokio::test]
    async fn expired_unknown_refund_replaces_only_after_pinned_archival_quorum() {
        let store = InMemoryStore::new();
        let user = UserId(uuid::Uuid::new_v4());
        let (_, identity, approved) =
            dual_approve_refund(&store, user, "sig-refund-replacement").await;
        let rails = crate::ports::withdraw_fakes::FakeRails::default();
        *rails.fail_broadcast.lock() = true;
        let first_signer = crate::ports::withdraw_send::FakeSigner::new("refund-first-attempt");
        first_signer.set_presence(crate::ports::SignaturePresence::Unknown);
        assert!(begin_refund_send(
            &store,
            "sig-refund-replacement",
            OffsetDateTime::UNIX_EPOCH,
            &rails,
            &first_signer,
        )
        .await
        .is_err());
        *rails.fail_broadcast.lock() = false;
        begin_refund_send(
            &store,
            "sig-refund-replacement",
            OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(31),
            &rails,
            &first_signer,
        )
        .await
        .unwrap();

        let observations: Vec<_> = identity
            .rpc_endpoints
            .iter()
            .map(|endpoint| crate::ports::QuorumObservation {
                endpoint: endpoint.clone(),
                finalized_height: Some(1_001),
                signature_present: Some(false),
                pruned: false,
            })
            .collect();
        let replacement = crate::ports::withdraw_send::FakeSigner::new("refund-second-attempt");
        replacement.set_finalized_height(1_001);
        let mut unpinned = observations.clone();
        unpinned[0].endpoint = "unpinned-endpoint".into();
        assert!(replace_expired_refund(
            &store,
            "sig-refund-replacement",
            OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(32),
            &unpinned,
            &identity,
            &rails,
            &replacement,
        )
        .await
        .is_err());
        replace_expired_refund(
            &store,
            "sig-refund-replacement",
            OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(32),
            &observations,
            &identity,
            &rails,
            &replacement,
        )
        .await
        .unwrap();

        let mut inspect = store.deposit_admission_tx().await.unwrap();
        let attempts = inspect.attempts_for(approved.payment_id).await.unwrap();
        assert_eq!(attempts.len(), 2);
        assert_eq!(
            attempts[0].landing_state,
            crate::ports::LandingState::DefinitiveFailed
        );
        assert_eq!(attempts[1].attempt_number, 2);
        assert_eq!(attempts[1].replaces_attempt_id, Some(attempts[0].id));
        assert_eq!(
            attempts[1].landing_state,
            crate::ports::LandingState::Broadcast
        );
        assert_eq!(
            rails.persisted_bytes(approved.payment_id).as_deref(),
            Some(replacement.bytes.as_slice())
        );
        drop(inspect);
        let failed_attempt_receipt = finalized_refund_receipt(
            &identity,
            &approved.dest,
            25_000_000,
            &first_signer.signature,
        );
        assert_eq!(
            settle_refund(
                &store,
                "sig-refund-replacement",
                OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(33),
                &identity,
                &failed_attempt_receipt,
            )
            .await
            .unwrap_err(),
            AppError::Store(StoreError::Invariant("refund attempt is not live"))
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn refund_replacement_rejects_state_identity_expiry_and_signature_reuse() {
        let store = InMemoryStore::new();
        let user = UserId(uuid::Uuid::new_v4());
        let (_, identity, _) =
            dual_approve_refund(&store, user, "sig-refund-replacement-guards").await;
        let rails = crate::ports::withdraw_fakes::FakeRails::default();
        let first_signer = crate::ports::withdraw_send::FakeSigner::new("refund-guard-first");

        assert_eq!(
            replace_expired_refund(
                &store,
                "sig-refund-replacement-guards",
                OffsetDateTime::UNIX_EPOCH,
                &[],
                &identity,
                &rails,
                &first_signer,
            )
            .await
            .unwrap_err(),
            AppError::Store(StoreError::Conflict("deposit refund is not sending"))
        );

        *rails.fail_broadcast.lock() = true;
        assert!(begin_refund_send(
            &store,
            "sig-refund-replacement-guards",
            OffsetDateTime::UNIX_EPOCH,
            &rails,
            &first_signer,
        )
        .await
        .is_err());
        *rails.fail_broadcast.lock() = false;
        first_signer.set_presence(crate::ports::SignaturePresence::Unknown);
        begin_refund_send(
            &store,
            "sig-refund-replacement-guards",
            OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(31),
            &rails,
            &first_signer,
        )
        .await
        .unwrap();

        let observations: Vec<_> = identity
            .rpc_endpoints
            .iter()
            .map(|endpoint| crate::ports::QuorumObservation {
                endpoint: endpoint.clone(),
                finalized_height: Some(1_001),
                signature_present: Some(false),
                pruned: false,
            })
            .collect();
        let replacement = crate::ports::withdraw_send::FakeSigner::new("refund-guard-second");
        replacement.set_finalized_height(1_001);
        let mut drifted_identity = identity.clone();
        drifted_identity.treasury_owner = "different-treasury".into();
        assert_eq!(
            replace_expired_refund(
                &store,
                "sig-refund-replacement-guards",
                OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(32),
                &observations,
                &drifted_identity,
                &rails,
                &replacement,
            )
            .await
            .unwrap_err(),
            AppError::Store(StoreError::Invariant("refund rail identity drift"))
        );

        replacement.set_finalized_height(1_000);
        assert_eq!(
            replace_expired_refund(
                &store,
                "sig-refund-replacement-guards",
                OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(32),
                &observations,
                &identity,
                &rails,
                &replacement,
            )
            .await
            .unwrap_err(),
            AppError::ProposalConflict("blockhash has not expired")
        );

        first_signer.set_finalized_height(1_001);
        assert_eq!(
            replace_expired_refund(
                &store,
                "sig-refund-replacement-guards",
                OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(32),
                &observations,
                &identity,
                &rails,
                &first_signer,
            )
            .await
            .unwrap_err(),
            AppError::Store(StoreError::Invariant(
                "refund replacement signature was reused"
            ))
        );
    }

    #[tokio::test]
    async fn held_deposit_is_not_yet_approved_for_sending() {
        let store = InMemoryStore::new();
        let user = UserId(uuid::Uuid::new_v4());
        store.set_money_flag("pause_deposits", true);
        CreditDeposit { store: &store }
            .execute(cmd(user, "sig-held-send", "dep-held-send"))
            .await
            .unwrap();
        let rails = crate::ports::withdraw_fakes::FakeRails::default();
        let signer = crate::ports::withdraw_send::FakeSigner::new("held-refund-signature");
        assert_eq!(
            begin_refund_send(
                &store,
                "sig-held-send",
                OffsetDateTime::UNIX_EPOCH,
                &rails,
                &signer,
            )
            .await
            .unwrap_err(),
            AppError::Store(StoreError::Conflict("deposit refund is not approved"))
        );
    }

    #[tokio::test]
    async fn held_and_unmatched_observations_preserve_the_suspense_liability() {
        let store = InMemoryStore::new();
        let unmatched = ObservedDeposit {
            user: None,
            amount: MicroUsd(7_000_000),
            chain_sig: "sig-unmatched".into(),
            source_address: "unmatched-source".into(),
            dest_address: "treasury".into(),
            mint: "usdc".into(),
            slot: 7,
        };
        observe_finalized(&store, &unmatched).await.unwrap();
        let held = admit_observed(
            &store,
            &unmatched.chain_sig,
            OffsetDateTime::UNIX_EPOCH,
            &AdminContext::Machine,
            false,
        )
        .await
        .unwrap();
        assert!(!held.replayed);
        let replay = admit_observed(
            &store,
            &unmatched.chain_sig,
            OffsetDateTime::UNIX_EPOCH,
            &AdminContext::Machine,
            false,
        )
        .await
        .unwrap();
        assert!(replay.replayed);
        let held_row = {
            let mut inspect = store.deposit_admission_tx().await.unwrap();
            inspect
                .deposit_machine_by_sig(&unmatched.chain_sig)
                .await
                .unwrap()
                .unwrap()
        };
        let admit_proposal = propose_deposit_command(
            &store,
            held_row.id,
            false,
            "review unmatched admission".into(),
            OffsetDateTime::UNIX_EPOCH,
            &admin(crate::model::AdminRole::Finance, "finance-a"),
        )
        .await
        .unwrap();
        assert!(confirm_manual_deposit_admission(
            &store,
            admit_proposal.id,
            OffsetDateTime::UNIX_EPOCH,
            &admin(crate::model::AdminRole::Superadmin, "super-b"),
        )
        .await
        .is_err());
        assert!(propose_deposit_command(
            &store,
            held_row.id,
            true,
            "review unmatched source".into(),
            OffsetDateTime::UNIX_EPOCH,
            &AdminContext::Machine,
        )
        .await
        .is_err());
        let proposal = propose_deposit_command(
            &store,
            held_row.id,
            true,
            "review unmatched source".into(),
            OffsetDateTime::UNIX_EPOCH,
            &admin(crate::model::AdminRole::Finance, "finance-a"),
        )
        .await
        .unwrap();
        let approved = confirm_deposit_refund(
            &store,
            proposal.id,
            OffsetDateTime::UNIX_EPOCH,
            &admin(crate::model::AdminRole::Superadmin, "super-b"),
        )
        .await
        .unwrap();
        assert_eq!(approved.dest, unmatched.source_address);
        assert_eq!(
            store.balance_of(OwnerRef::DepositSuspense, Currency::Usdc),
            Some(MicroUsd(7_000_000))
        );
    }

    #[tokio::test]
    async fn banned_deposit_is_held_before_admission() {
        let store = InMemoryStore::new();
        let user = InMemoryStore::add_user(&store, "banned-deposit", OffsetDateTime::UNIX_EPOCH, 0);
        store.set_money_user(user, "banned", 2);
        let receipt = CreditDeposit { store: &store }
            .execute(cmd(user, "sig-banned", "dep-banned"))
            .await
            .unwrap();
        assert!(!receipt.replayed);
        assert_eq!(store.balance_of(OwnerRef::User(user), Currency::Usdc), None);
        assert_eq!(
            store.balance_of(OwnerRef::DepositSuspense, Currency::Usdc),
            Some(MicroUsd(25_000_000))
        );
    }

    #[tokio::test]
    async fn source_refund_requires_fresh_geo_and_sanctions_clear() {
        let store = InMemoryStore::new();
        let user = UserId(uuid::Uuid::new_v4());
        store.set_money_flag("pause_deposits", true);
        let held = CreditDeposit { store: &store }
            .execute(cmd(user, "sig-refund-hit", "dep-refund-hit"))
            .await
            .unwrap();
        store.set_money_flag("pause_deposits", false);
        store.set_screen(user, "sanctions", crate::ports::ScreenVerdict::Hit);

        let proposal = propose_deposit_command(
            &store,
            held.deposit_id,
            true,
            "review rejected deposit".into(),
            OffsetDateTime::UNIX_EPOCH,
            &admin(crate::model::AdminRole::Finance, "finance-a"),
        )
        .await
        .unwrap();

        assert_eq!(
            confirm_deposit_refund(
                &store,
                proposal.id,
                OffsetDateTime::UNIX_EPOCH,
                &admin(crate::model::AdminRole::Superadmin, "super-b"),
            )
            .await
            .unwrap_err(),
            AppError::ComplianceHold {
                reason: "refund_requires_clear"
            }
        );
        assert_eq!(
            store.balance_of(OwnerRef::DepositSuspense, Currency::Usdc),
            Some(MicroUsd(25_000_000))
        );
    }

    #[tokio::test]
    async fn observation_signature_is_bound_to_the_complete_chain_tuple() {
        let store = InMemoryStore::new();
        let user = UserId(uuid::Uuid::new_v4());
        let original = ObservedDeposit {
            user: Some(user),
            amount: MicroUsd(7_000_000),
            chain_sig: "tuple-sig".into(),
            source_address: "source-token-account".into(),
            dest_address: "treasury-token-account".into(),
            mint: "usdc-mint".into(),
            slot: 77,
        };
        assert!(!observe_finalized(&store, &original).await.unwrap().replayed);
        assert!(observe_finalized(&store, &original).await.unwrap().replayed);
        let mut changed = original;
        changed.slot += 1;
        assert_eq!(
            observe_finalized(&store, &changed).await.unwrap_err(),
            AppError::Store(StoreError::Conflict("deposit observation binding"))
        );
    }

    #[tokio::test]
    async fn observation_signature_is_bound_to_the_startup_rail_identity() {
        let store = InMemoryStore::new();
        let observed = ObservedDeposit {
            user: None,
            amount: MicroUsd(7_000_000),
            chain_sig: "rail-bound-sig".into(),
            source_address: "source-token-account".into(),
            dest_address: "treasury-token-account".into(),
            mint: "usdc-mint".into(),
            slot: 77,
        };

        assert_eq!(
            observe_finalized_on_rail(&store, &observed, "")
                .await
                .unwrap_err(),
            AppError::Store(StoreError::Invariant("deposit rail fingerprint missing"))
        );
        assert!(
            !observe_finalized_on_rail(&store, &observed, "startup-rail-v1")
                .await
                .unwrap()
                .replayed
        );
        assert!(
            observe_finalized_on_rail(&store, &observed, "startup-rail-v1")
                .await
                .unwrap()
                .replayed
        );
        assert_eq!(
            observe_finalized_on_rail(&store, &observed, "startup-rail-v2")
                .await
                .unwrap_err(),
            AppError::Store(StoreError::Conflict("deposit observation binding"))
        );

        let mut inspect = store.deposit_admission_tx().await.unwrap();
        let row = inspect
            .deposit_machine_by_sig(&observed.chain_sig)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.rail_fingerprint, "startup-rail-v1");
    }

    #[tokio::test]
    async fn held_deposit_reevaluates_through_pending_after_fresh_policy() {
        let store = InMemoryStore::new();
        let user = UserId(uuid::Uuid::new_v4());
        store.set_money_flag("pause_deposits", true);
        CreditDeposit { store: &store }
            .execute(cmd(user, "sig-reevaluate", "dep-reevaluate"))
            .await
            .unwrap();
        store.set_money_flag("pause_deposits", false);
        assert!(matches!(
            admit_observed(
                &store,
                "sig-reevaluate",
                OffsetDateTime::UNIX_EPOCH,
                &admin(crate::model::AdminRole::Finance, "finance-a"),
                true,
            )
            .await,
            Err(AppError::AdminForbidden(_))
        ));
        let receipt = reevaluate_held(&store, "sig-reevaluate", OffsetDateTime::UNIX_EPOCH)
            .await
            .unwrap();
        assert!(!receipt.replayed);
        assert_eq!(
            store.balance_of(OwnerRef::User(user), Currency::Usdc),
            Some(MicroUsd(25_000_000))
        );
        assert_eq!(
            store.balance_of(OwnerRef::DepositSuspense, Currency::Usdc),
            Some(MicroUsd(0))
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn deposit_adapter_faults_fail_closed_at_every_money_cas_and_lineage_guard() {
        let gate_store = InMemoryStore::new();
        let gate_user = UserId(uuid::Uuid::new_v4());
        let (gate_observed, _) =
            observe_test_deposit(&gate_store, Some(gate_user), "sig-gate-backend-fail").await;
        gate_store.fail_next_fresh_clear();
        assert_eq!(
            admit_observed(
                &gate_store,
                &gate_observed.chain_sig,
                OffsetDateTime::UNIX_EPOCH,
                &AdminContext::Machine,
                false,
            )
            .await
            .unwrap_err(),
            AppError::Store(StoreError::Unavailable("fake:fresh-clear"))
        );

        let pending_store = InMemoryStore::new();
        let pending_user = UserId(uuid::Uuid::new_v4());
        let (pending_observed, _) =
            observe_test_deposit(&pending_store, Some(pending_user), "sig-pending-recovery").await;
        let mut pending_tx = pending_store.deposit_admission_tx().await.unwrap();
        let pending_row = pending_tx
            .deposit_machine_by_sig(&pending_observed.chain_sig)
            .await
            .unwrap()
            .unwrap();
        assert!(pending_tx
            .cas_deposit_status(
                pending_row.id,
                DepositMachineStatus::ObservedFinalized,
                DepositMachineStatus::AdmissionPending,
            )
            .await
            .unwrap());
        pending_tx.commit().await.unwrap();
        assert!(
            !admit_observed(
                &pending_store,
                &pending_observed.chain_sig,
                OffsetDateTime::UNIX_EPOCH,
                &AdminContext::Machine,
                false,
            )
            .await
            .unwrap()
            .replayed
        );

        let first_cas_store = InMemoryStore::new();
        let first_cas_user = UserId(uuid::Uuid::new_v4());
        let (first_cas_observed, _) =
            observe_test_deposit(&first_cas_store, Some(first_cas_user), "sig-first-cas-miss")
                .await;
        first_cas_store.fail_deposit_cas_after(0);
        assert_eq!(
            admit_observed(
                &first_cas_store,
                &first_cas_observed.chain_sig,
                OffsetDateTime::UNIX_EPOCH,
                &AdminContext::Machine,
                false,
            )
            .await
            .unwrap_err(),
            AppError::Store(StoreError::Conflict("deposit status"))
        );

        let admit_cas_store = InMemoryStore::new();
        let admit_cas_user = UserId(uuid::Uuid::new_v4());
        let (admit_cas_observed, _) =
            observe_test_deposit(&admit_cas_store, Some(admit_cas_user), "sig-admit-cas-miss")
                .await;
        admit_cas_store.fail_deposit_cas_after(1);
        assert_eq!(
            admit_observed(
                &admit_cas_store,
                &admit_cas_observed.chain_sig,
                OffsetDateTime::UNIX_EPOCH,
                &AdminContext::Machine,
                false,
            )
            .await
            .unwrap_err(),
            AppError::Store(StoreError::Conflict("deposit status"))
        );

        let hold_cas_store = InMemoryStore::new();
        let (hold_cas_observed, _) =
            observe_test_deposit(&hold_cas_store, None, "sig-hold-cas-miss").await;
        hold_cas_store.fail_deposit_cas_after(1);
        assert_eq!(
            admit_observed(
                &hold_cas_store,
                &hold_cas_observed.chain_sig,
                OffsetDateTime::UNIX_EPOCH,
                &AdminContext::Machine,
                false,
            )
            .await
            .unwrap_err(),
            AppError::Store(StoreError::Conflict("deposit status"))
        );

        let manual_store = InMemoryStore::new();
        let manual_user = UserId(uuid::Uuid::new_v4());
        manual_store.set_money_flag("pause_deposits", true);
        let manual_held = CreditDeposit {
            store: &manual_store,
        }
        .execute(cmd(
            manual_user,
            "sig-manual-cas-miss",
            "dep-manual-cas-miss",
        ))
        .await
        .unwrap();
        let manual_proposal = propose_deposit_command(
            &manual_store,
            manual_held.deposit_id,
            false,
            "reviewed".into(),
            OffsetDateTime::UNIX_EPOCH,
            &admin(crate::model::AdminRole::Finance, "finance-manual"),
        )
        .await
        .unwrap();
        manual_store.fail_deposit_cas_after(0);
        assert_eq!(
            confirm_manual_deposit_admission(
                &manual_store,
                manual_proposal.id,
                OffsetDateTime::UNIX_EPOCH,
                &admin(crate::model::AdminRole::Superadmin, "super-manual"),
            )
            .await
            .unwrap_err(),
            AppError::Store(StoreError::Conflict("deposit status"))
        );

        let refund_store = InMemoryStore::new();
        let refund_user = UserId(uuid::Uuid::new_v4());
        refund_store.set_money_flag("pause_deposits", true);
        let refund_held = CreditDeposit {
            store: &refund_store,
        }
        .execute(cmd(
            refund_user,
            "sig-refund-cas-miss",
            "dep-refund-cas-miss",
        ))
        .await
        .unwrap();
        let refund_proposal = propose_deposit_command(
            &refund_store,
            refund_held.deposit_id,
            true,
            "return to source".into(),
            OffsetDateTime::UNIX_EPOCH,
            &admin(crate::model::AdminRole::Finance, "finance-refund"),
        )
        .await
        .unwrap();
        refund_store.fail_deposit_cas_after(0);
        assert_eq!(
            confirm_deposit_refund(
                &refund_store,
                refund_proposal.id,
                OffsetDateTime::UNIX_EPOCH,
                &admin(crate::model::AdminRole::Superadmin, "super-refund"),
            )
            .await
            .unwrap_err(),
            AppError::Store(StoreError::Conflict("deposit status"))
        );

        let send_store = InMemoryStore::new();
        let send_user = UserId(uuid::Uuid::new_v4());
        let (send_observed, send_identity, send_approved) =
            dual_approve_refund(&send_store, send_user, "sig-send-cas-miss").await;
        let send_rails = crate::ports::withdraw_fakes::FakeRails::default();
        let send_signer = crate::ports::withdraw_send::FakeSigner::new("send-cas-miss");
        send_store.fail_deposit_cas_after(0);
        assert_eq!(
            begin_refund_send(
                &send_store,
                &send_observed.chain_sig,
                OffsetDateTime::UNIX_EPOCH,
                &send_rails,
                &send_signer,
            )
            .await
            .unwrap_err(),
            AppError::Store(StoreError::Conflict("deposit status"))
        );
        begin_refund_send(
            &send_store,
            &send_observed.chain_sig,
            OffsetDateTime::UNIX_EPOCH,
            &send_rails,
            &send_signer,
        )
        .await
        .unwrap();

        send_store.force_next_live_attempt_state(crate::ports::LandingState::Finalized);
        assert_eq!(
            begin_refund_send(
                &send_store,
                &send_observed.chain_sig,
                OffsetDateTime::UNIX_EPOCH,
                &send_rails,
                &send_signer,
            )
            .await
            .unwrap_err(),
            AppError::Store(StoreError::Invariant(
                "refund_sending deposit has a terminal live attempt"
            ))
        );
        send_store.force_next_live_attempt_state(crate::ports::LandingState::DefinitiveFailed);
        assert_eq!(
            mark_refund_broadcast(&send_store, &send_observed.chain_sig)
                .await
                .unwrap_err(),
            AppError::Store(StoreError::Invariant(
                "refund attempt cannot become broadcast"
            ))
        );

        let settlement_receipt = finalized_refund_receipt(
            &send_identity,
            &send_approved.dest,
            send_observed.amount.0,
            &send_signer.signature,
        );
        send_store.fail_deposit_cas_after(0);
        assert_eq!(
            settle_refund(
                &send_store,
                &send_observed.chain_sig,
                OffsetDateTime::UNIX_EPOCH,
                &send_identity,
                &settlement_receipt,
            )
            .await
            .unwrap_err(),
            AppError::Store(StoreError::Conflict("deposit status"))
        );

        let corrupt_store = InMemoryStore::new();
        let corrupt_user = UserId(uuid::Uuid::new_v4());
        let (corrupt_observed, _, _) =
            dual_approve_refund(&corrupt_store, corrupt_user, "sig-refunded-no-finalized").await;
        let corrupt_rails = crate::ports::withdraw_fakes::FakeRails::default();
        let corrupt_signer = crate::ports::withdraw_send::FakeSigner::new("refunded-no-finalized");
        begin_refund_send(
            &corrupt_store,
            &corrupt_observed.chain_sig,
            OffsetDateTime::UNIX_EPOCH,
            &corrupt_rails,
            &corrupt_signer,
        )
        .await
        .unwrap();
        let mut corrupt_tx = corrupt_store.deposit_admission_tx().await.unwrap();
        let corrupt_row = corrupt_tx
            .deposit_machine_by_sig(&corrupt_observed.chain_sig)
            .await
            .unwrap()
            .unwrap();
        corrupt_tx
            .mark_refunded(corrupt_row.id, uuid::Uuid::new_v4())
            .await
            .unwrap();
        assert!(corrupt_tx
            .cas_deposit_status(
                corrupt_row.id,
                DepositMachineStatus::RefundSending,
                DepositMachineStatus::Refunded,
            )
            .await
            .unwrap());
        corrupt_tx.commit().await.unwrap();
        assert_eq!(
            begin_refund_send(
                &corrupt_store,
                &corrupt_observed.chain_sig,
                OffsetDateTime::UNIX_EPOCH,
                &corrupt_rails,
                &corrupt_signer,
            )
            .await
            .unwrap_err(),
            AppError::Store(StoreError::Invariant(
                "refunded deposit has no unique finalized attempt"
            ))
        );
    }

    #[tokio::test]
    async fn admission_and_refund_approval_race_has_one_winner() {
        let store = InMemoryStore::new();
        let user = UserId(uuid::Uuid::new_v4());
        store.set_money_flag("pause_deposits", true);
        let held = CreditDeposit { store: &store }
            .execute(cmd(user, "sig-admit-refund-race", "dep-race"))
            .await
            .unwrap();
        store.set_money_flag("pause_deposits", false);
        let proposal = propose_deposit_command(
            &store,
            held.deposit_id,
            true,
            "race source refund".into(),
            OffsetDateTime::UNIX_EPOCH,
            &admin(crate::model::AdminRole::Finance, "finance-a"),
        )
        .await
        .unwrap();
        let refund_confirmer = admin(crate::model::AdminRole::Superadmin, "super-b");

        let (admit, refund) = tokio::join!(
            reevaluate_held(&store, "sig-admit-refund-race", OffsetDateTime::UNIX_EPOCH),
            confirm_deposit_refund(
                &store,
                proposal.id,
                OffsetDateTime::UNIX_EPOCH,
                &refund_confirmer,
            )
        );
        assert_eq!(usize::from(admit.is_ok()) + usize::from(refund.is_ok()), 1);
        let user_balance = store
            .balance_of(OwnerRef::User(user), Currency::Usdc)
            .map_or(0, |amount| amount.0);
        let suspense = store
            .balance_of(OwnerRef::DepositSuspense, Currency::Usdc)
            .map_or(0, |amount| amount.0);
        assert_eq!(user_balance + suspense, 25_000_000);
    }
}
