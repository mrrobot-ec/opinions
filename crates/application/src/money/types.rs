//! W1 withdrawal vocabulary (D31). Lives here because `model.rs` is frozen.

use std::net::IpAddr;

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::StoreError;
use crate::model::{
    Event, NewNotification, UserId, UserStatus, WithdrawalReviewState, WithdrawalSendState,
    WithdrawalStatus,
};
use crate::ports::ScreenVerdict;

/// Stable withdrawal identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WithdrawalId(pub Uuid);

/// Persisted three-dimension combination (ops.md W1–W15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Combo {
    pub status: WithdrawalStatus,
    pub review: WithdrawalReviewState,
    pub send: WithdrawalSendState,
}

impl Combo {
    pub const W1: Self = Self {
        status: WithdrawalStatus::Queued,
        review: WithdrawalReviewState::Screening,
        send: WithdrawalSendState::Unsent,
    };
    pub const W2: Self = Self {
        status: WithdrawalStatus::Queued,
        review: WithdrawalReviewState::Approved,
        send: WithdrawalSendState::Unsent,
    };
    pub const W3: Self = Self {
        status: WithdrawalStatus::Queued,
        review: WithdrawalReviewState::Approved,
        send: WithdrawalSendState::Sending,
    };
    pub const W4: Self = Self {
        status: WithdrawalStatus::RiskHold,
        review: WithdrawalReviewState::ReviewRequired,
        send: WithdrawalSendState::Unsent,
    };
    pub const W5: Self = Self {
        status: WithdrawalStatus::RiskHold,
        review: WithdrawalReviewState::ApprovalProposed,
        send: WithdrawalSendState::Unsent,
    };
    pub const W6: Self = Self {
        status: WithdrawalStatus::Sent,
        review: WithdrawalReviewState::Approved,
        send: WithdrawalSendState::Broadcast,
    };
    pub const W7: Self = Self {
        status: WithdrawalStatus::Sent,
        review: WithdrawalReviewState::Approved,
        send: WithdrawalSendState::Unknown,
    };
    pub const W8: Self = Self {
        status: WithdrawalStatus::Sent,
        review: WithdrawalReviewState::Approved,
        send: WithdrawalSendState::Sending,
    };
    pub const W9: Self = Self {
        status: WithdrawalStatus::Sent,
        review: WithdrawalReviewState::Approved,
        send: WithdrawalSendState::Finalized,
    };
    pub const W10: Self = Self {
        status: WithdrawalStatus::Settled,
        review: WithdrawalReviewState::Approved,
        send: WithdrawalSendState::Finalized,
    };
    pub const W11: Self = Self {
        status: WithdrawalStatus::Denied,
        review: WithdrawalReviewState::Screening,
        send: WithdrawalSendState::Unsent,
    };
    pub const W12: Self = Self {
        status: WithdrawalStatus::Denied,
        review: WithdrawalReviewState::ReviewRequired,
        send: WithdrawalSendState::Unsent,
    };
    pub const W13: Self = Self {
        status: WithdrawalStatus::Denied,
        review: WithdrawalReviewState::ApprovalProposed,
        send: WithdrawalSendState::Unsent,
    };
    pub const W14: Self = Self {
        status: WithdrawalStatus::Denied,
        review: WithdrawalReviewState::Approved,
        send: WithdrawalSendState::Unsent,
    };
    pub const W15: Self = Self {
        status: WithdrawalStatus::Failed,
        review: WithdrawalReviewState::Approved,
        send: WithdrawalSendState::DefinitiveFailed,
    };

    #[must_use]
    pub const fn of(
        status: WithdrawalStatus,
        review: WithdrawalReviewState,
        send: WithdrawalSendState,
    ) -> Self {
        Self {
            status,
            review,
            send,
        }
    }

    /// Published table label, or `None` if the triple is illegal.
    #[must_use]
    pub fn label(self) -> Option<&'static str> {
        use WithdrawalReviewState::{ApprovalProposed, Approved, ReviewRequired, Screening};
        use WithdrawalSendState::{
            Broadcast, DefinitiveFailed, Finalized, Sending, Unknown, Unsent,
        };
        use WithdrawalStatus::{Denied, Failed, Queued, RiskHold, Sent, Settled};
        match (self.status, self.review, self.send) {
            (Queued, Screening, Unsent) => Some("W1"),
            (Queued, Approved, Unsent) => Some("W2"),
            (Queued, Approved, Sending) => Some("W3"),
            (RiskHold, ReviewRequired, Unsent) => Some("W4"),
            (RiskHold, ApprovalProposed, Unsent) => Some("W5"),
            (Sent, Approved, Broadcast) => Some("W6"),
            (Sent, Approved, Unknown) => Some("W7"),
            (Sent, Approved, Sending) => Some("W8"),
            (Sent, Approved, Finalized) => Some("W9"),
            (Settled, Approved, Finalized) => Some("W10"),
            (Denied, Screening, Unsent) => Some("W11"),
            (Denied, ReviewRequired, Unsent) => Some("W12"),
            (Denied, ApprovalProposed, Unsent) => Some("W13"),
            (Denied, Approved, Unsent) => Some("W14"),
            (Failed, Approved, DefinitiveFailed) => Some("W15"),
            _ => None,
        }
    }

    #[must_use]
    pub fn is_legal(self) -> bool {
        self.label().is_some()
    }

    /// Deny CAS target for an unsent source, or `None` if deny is illegal.
    #[must_use]
    pub fn deny_target(self) -> Option<Self> {
        match (self.status, self.review, self.send) {
            (
                WithdrawalStatus::Queued,
                WithdrawalReviewState::Screening,
                WithdrawalSendState::Unsent,
            ) => Some(Self::W11),
            (
                WithdrawalStatus::Queued,
                WithdrawalReviewState::Approved,
                WithdrawalSendState::Unsent,
            ) => Some(Self::W14),
            (
                WithdrawalStatus::RiskHold,
                WithdrawalReviewState::ReviewRequired,
                WithdrawalSendState::Unsent,
            ) => Some(Self::W12),
            (
                WithdrawalStatus::RiskHold,
                WithdrawalReviewState::ApprovalProposed,
                WithdrawalSendState::Unsent,
            ) => Some(Self::W13),
            _ => None,
        }
    }

    #[must_use]
    pub fn has_active_hold(self) -> bool {
        !matches!(
            self.status,
            WithdrawalStatus::Denied | WithdrawalStatus::Failed | WithdrawalStatus::Settled
        )
    }
}

/// One persisted withdrawal row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WithdrawalRow {
    pub id: WithdrawalId,
    pub user: UserId,
    pub dest: String,
    pub amount_micro: i64,
    pub combo: Combo,
    pub hold_tx_id: Uuid,
    pub release_tx_id: Option<Uuid>,
    pub settle_tx_id: Option<Uuid>,
    pub request_fingerprint: String,
    pub risk_reasons: Vec<String>,
    pub requested_at: OffsetDateTime,
    pub decided_at: Option<OffsetDateTime>,
    pub sent_at: Option<OffsetDateTime>,
    pub settled_at: Option<OffsetDateTime>,
}

impl WithdrawalRow {
    #[must_use]
    pub const fn combo(&self) -> Combo {
        self.combo
    }
}

/// Replay-stable receipt, including refusals that never took a hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WithdrawalReceipt {
    pub id: Option<WithdrawalId>,
    pub user: UserId,
    pub dest: String,
    pub amount_micro: i64,
    pub combo: Option<Combo>,
    pub hold_tx_id: Option<Uuid>,
    pub replayed: bool,
    pub refused: bool,
    pub refuse_code: Option<String>,
    pub refuse_message: Option<String>,
}

/// User fields revalidated under `lock_user`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserMoneyView {
    pub status: UserStatus,
    pub kyc_tier: i32,
    pub self_excluded_until: Option<OffsetDateTime>,
}

/// Catalog limits read under the user/cap lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WithdrawLimits {
    pub min_micro: i64,
    pub max_micro: i64,
    pub daily_micro: i64,
    pub auto_approve_micro: i64,
    pub dual_control_micro: i64,
    pub dest_warm_floor_micro: i64,
    pub dest_warm_age_hours: i64,
    pub dest_daily_micro: i64,
    pub hot_daily_micro: i64,
    pub withdraw_kyc_tier: i64,
    pub pause_withdrawals: bool,
    pub approve_daily_cap_micro: i64,
}

impl WithdrawLimits {
    /// D24 seed snapshot.
    #[must_use]
    pub const fn seed() -> Self {
        Self {
            min_micro: 5_000_000,
            max_micro: 1_000_000_000,
            daily_micro: 2_000_000_000,
            auto_approve_micro: 50_000_000,
            dual_control_micro: 500_000_000,
            dest_warm_floor_micro: 100_000_000,
            dest_warm_age_hours: 72,
            dest_daily_micro: 1_000_000_000,
            hot_daily_micro: 10_000_000_000,
            withdraw_kyc_tier: 2,
            pause_withdrawals: false,
            approve_daily_cap_micro: 5_000_000_000,
        }
    }
}

/// Settled-dest warmth facts (D31 dest-warmth).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DestWarmth {
    pub settled_micro: i64,
    pub first_settled_at: Option<OffsetDateTime>,
    pub distinct_users: u32,
    pub is_refund_dest: bool,
}

impl DestWarmth {
    #[must_use]
    pub fn is_warm(self, limits: WithdrawLimits, now: OffsetDateTime) -> bool {
        if self.is_refund_dest || self.distinct_users >= 2 {
            return false;
        }
        if self.settled_micro < limits.dest_warm_floor_micro {
            return false;
        }
        let Some(first) = self.first_settled_at else {
            return false;
        };
        now - first >= time::Duration::hours(limits.dest_warm_age_hours)
    }
}

/// Generalized outbound subject (withdrawal or deposit refund).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundSubject {
    Withdrawal,
    DepositRefund,
}

impl OutboundSubject {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Withdrawal => "withdrawal",
            Self::DepositRefund => "deposit_refund",
        }
    }
}

/// One `outbound_payments` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundPaymentRow {
    pub id: Uuid,
    pub subject: OutboundSubject,
    pub subject_id: Uuid,
    pub dest: String,
    pub amount_micro: i64,
    pub rail_fingerprint: String,
}

/// Attempt landing state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LandingState {
    Prepared,
    Broadcast,
    Unknown,
    Finalized,
    DefinitiveFailed,
}

impl LandingState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Broadcast => "broadcast",
            Self::Unknown => "unknown",
            Self::Finalized => "finalized",
            Self::DefinitiveFailed => "definitive_failed",
        }
    }

    #[must_use]
    pub const fn is_live(self) -> bool {
        matches!(self, Self::Prepared | Self::Broadcast | Self::Unknown)
    }
}

/// One `outbound_send_attempts` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundAttemptRow {
    pub id: Uuid,
    pub payment_id: Uuid,
    pub attempt_number: i32,
    pub replaces_attempt_id: Option<Uuid>,
    pub signed_tx_bytes: Vec<u8>,
    pub signature: String,
    pub last_valid_block_height: i64,
    pub landing_state: LandingState,
    pub lease_expires_at: Option<OffsetDateTime>,
    pub evidence: Option<serde_json::Value>,
}

/// Dual-control money-command proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoneyProposal {
    pub id: Uuid,
    pub kind: String,
    pub subject_id: Uuid,
    pub payload_hash: String,
    pub proposer_token_id: String,
    pub confirmer_token_id: Option<String>,
    pub reason: String,
    pub status: crate::model::ProposalStatus,
    pub confirm_not_before: OffsetDateTime,
    pub expires_at: OffsetDateTime,
    pub replay_key: String,
}

/// On-chain receipt used at observe-finalized / settle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainReceipt {
    pub signature: String,
    pub mint: String,
    pub source: String,
    pub dest_token_account: String,
    pub delta_micro: i64,
    pub commitment: String,
}

/// One archival-RPC observation for the 2-of-3 predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuorumObservation {
    pub endpoint: String,
    pub finalized_height: Option<i64>,
    pub signature_present: Option<bool>,
    pub pruned: bool,
}

/// Outcome of the non-landing predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonLandingVerdict {
    DefinitiveFailed,
    Unknown,
}

/// Open receivable used by request/send-time lien checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenReceivable {
    pub id: Uuid,
    pub outstanding_micro: i64,
}

/// Client request.
#[derive(Debug, Clone)]
pub struct RequestWithdrawCmd {
    pub user: UserId,
    pub amount_micro: i64,
    pub dest: String,
    pub client_ip: Option<IpAddr>,
    pub idempotency_key: Option<String>,
}

/// Lock-free factory plus fingerprint lookup (D31 step 1).
#[async_trait]
pub trait WithdrawStore: Send + Sync {
    async fn withdraw_tx(&self) -> Result<Box<dyn super::WithdrawTx + '_>, StoreError>;
    /// Lock-free owner lookup used to acquire the global user lock before a
    /// withdrawal row lock. Callers must revalidate the owner after locking.
    async fn withdrawal_user(&self, id: WithdrawalId) -> Result<UserId, StoreError>;
    async fn lookup_fingerprint(
        &self,
        fingerprint: &str,
    ) -> Result<Option<WithdrawalReceipt>, StoreError>;
}

/// Subject-agnostic outbound payment and attempt lineage.
///
/// Both withdrawals and deposit refunds use this exact persistence seam so
/// signed bytes, replacement lineage, and finalization cannot fork by subject.
#[async_trait]
pub trait OutboundIo: Send {
    async fn insert_outbound_payment(
        &mut self,
        payment: &OutboundPaymentRow,
    ) -> Result<(), StoreError>;
    async fn outbound_by_subject(
        &mut self,
        subject: OutboundSubject,
        subject_id: Uuid,
    ) -> Result<Option<OutboundPaymentRow>, StoreError>;
    async fn insert_attempt(&mut self, attempt: &OutboundAttemptRow) -> Result<(), StoreError>;
    async fn live_attempt(
        &mut self,
        payment_id: Uuid,
    ) -> Result<Option<OutboundAttemptRow>, StoreError>;
    async fn save_attempt(&mut self, attempt: &OutboundAttemptRow) -> Result<(), StoreError>;
    async fn attempts_for(
        &mut self,
        payment_id: Uuid,
    ) -> Result<Vec<OutboundAttemptRow>, StoreError>;
}

/// Withdrawal-specific IO. Ledger legs are encapsulated so W1 does not
/// need frozen `OwnerRef::Withheld`.
#[async_trait]
pub trait WithdrawIo: Send + OutboundIo {
    async fn withheld_balance(&mut self) -> Result<i64, StoreError>;
    async fn user_cash(&mut self, user: UserId) -> Result<i64, StoreError>;
    async fn user_money_view(&mut self, user: UserId) -> Result<UserMoneyView, StoreError>;
    async fn limits(&mut self) -> Result<WithdrawLimits, StoreError>;

    async fn persist_screening(
        &mut self,
        user: UserId,
        context: &str,
        verdict: &ScreenVerdict,
    ) -> Result<(), StoreError>;
    async fn latest_screening(
        &mut self,
        user: UserId,
        context: &str,
    ) -> Result<Option<ScreenVerdict>, StoreError>;
    async fn open_aml_flag_count(&mut self, user: UserId) -> Result<u32, StoreError>;
    async fn evaluate_withdraw_aml_candidate(
        &mut self,
        id: WithdrawalId,
        user: UserId,
        dest: &str,
        amount_micro: i64,
        at: OffsetDateTime,
    ) -> Result<(), StoreError>;

    async fn persist_intent(&mut self, receipt: &WithdrawalReceipt) -> Result<(), StoreError>;
    async fn lookup_fingerprint_tx(
        &mut self,
        fingerprint: &str,
    ) -> Result<Option<WithdrawalReceipt>, StoreError>;
    async fn lookup_idempotency(&mut self, key: &str) -> Result<Option<String>, StoreError>;
    async fn persist_idempotency(&mut self, key: &str, fingerprint: &str)
        -> Result<(), StoreError>;

    async fn insert_withdrawal(&mut self, row: &WithdrawalRow) -> Result<(), StoreError>;
    async fn withdrawal_for_update(
        &mut self,
        id: WithdrawalId,
    ) -> Result<WithdrawalRow, StoreError>;
    async fn cas_withdrawal(
        &mut self,
        id: WithdrawalId,
        expected: Combo,
        next: &WithdrawalRow,
    ) -> Result<bool, StoreError>;
    async fn append_withdrawal_event(
        &mut self,
        withdrawal: WithdrawalId,
        kind: &str,
        actor: &str,
        payload: serde_json::Value,
    ) -> Result<(), StoreError>;
    async fn list_withdrawals(&mut self) -> Result<Vec<WithdrawalRow>, StoreError>;

    async fn window_sum_user(
        &mut self,
        user: UserId,
        since: OffsetDateTime,
    ) -> Result<i64, StoreError>;
    async fn window_sum_dest(
        &mut self,
        dest: &str,
        since: OffsetDateTime,
    ) -> Result<i64, StoreError>;
    async fn window_sum_hot(&mut self, since: OffsetDateTime) -> Result<i64, StoreError>;
    async fn dest_warmth(&mut self, dest: &str) -> Result<DestWarmth, StoreError>;
    async fn dest_was_settled_for_user(
        &mut self,
        user: UserId,
        dest: &str,
    ) -> Result<bool, StoreError>;
    async fn dest_is_observation_source_for_user(
        &mut self,
        user: UserId,
        dest: &str,
    ) -> Result<bool, StoreError>;

    async fn apply_hold(
        &mut self,
        user: UserId,
        amount_micro: i64,
        key: &str,
    ) -> Result<Uuid, StoreError>;
    async fn apply_release(
        &mut self,
        user: UserId,
        amount_micro: i64,
        key: &str,
    ) -> Result<Uuid, StoreError>;
    async fn apply_settle(&mut self, amount_micro: i64, key: &str) -> Result<Uuid, StoreError>;
    async fn open_receivables(&mut self, user: UserId) -> Result<Vec<OpenReceivable>, StoreError>;

    async fn insert_proposal(&mut self, proposal: &MoneyProposal) -> Result<(), StoreError>;
    async fn proposal_by_replay(&mut self, key: &str) -> Result<Option<MoneyProposal>, StoreError>;
    async fn open_proposal_for(
        &mut self,
        subject: Uuid,
        kind: &str,
    ) -> Result<Option<MoneyProposal>, StoreError>;
    async fn save_proposal(&mut self, proposal: &MoneyProposal) -> Result<(), StoreError>;
    async fn finance_approve_sum_since(&mut self, since: OffsetDateTime)
        -> Result<i64, StoreError>;
    async fn record_finance_approve(&mut self, amount_micro: i64) -> Result<(), StoreError>;

    async fn record_event(&mut self, event: Event) -> Result<i64, StoreError>;
    async fn insert_notification(
        &mut self,
        notification: NewNotification,
    ) -> Result<(), StoreError>;
    async fn lock_cap(&mut self, name: &str) -> Result<(), StoreError>;
}

/// Immutable client-intent fingerprint (user, amount, canonical dest).
#[must_use]
pub fn intent_fingerprint(user: UserId, amount_micro: i64, dest: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"withdraw-v1|");
    hasher.update(user.0.as_bytes());
    hasher.update(b"|");
    hasher.update(amount_micro.to_string().as_bytes());
    hasher.update(b"|");
    hasher.update(dest.as_bytes());
    hex_encode(&hasher.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

/// Solana dest: base58 encoding of exactly 32 bytes.
///
/// # Errors
/// Returns `None` when the string is not a 32-byte base58 pubkey.
#[must_use]
pub fn canonicalize_dest(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    (decode_base58(trimmed)?.len() == 32).then(|| trimmed.to_string())
}

fn decode_base58(input: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    if input.is_empty() {
        return None;
    }
    let mut acc = vec![0_u8];
    for byte in input.bytes() {
        let value = ALPHABET.iter().position(|&item| item == byte)?;
        let mut carry = value;
        for slot in acc.iter_mut().rev() {
            carry += usize::from(*slot) * 58;
            *slot = u8::try_from(carry % 256).ok()?;
            carry /= 256;
        }
        while carry > 0 {
            acc.insert(0, u8::try_from(carry % 256).ok()?);
            carry /= 256;
        }
    }
    let zeros = input.bytes().take_while(|&byte| byte == b'1').count();
    let start = acc.iter().position(|&byte| byte != 0).unwrap_or(acc.len());
    let mut out = vec![0_u8; zeros];
    out.extend_from_slice(&acc[start..]);
    Some(out)
}

/// Union toward more review (tighten-only).
#[must_use]
pub fn union_reasons(snapshot: &[String], current: &[String]) -> Vec<String> {
    let mut out = snapshot.to_vec();
    for reason in current {
        if !out.iter().any(|existing| existing == reason) {
            out.push(reason.clone());
        }
    }
    out
}

/// Auto-approve iff the tightened reason set is empty.
#[must_use]
pub fn blocks_auto_approve(reasons: &[String]) -> bool {
    !reasons.is_empty()
}

/// Daily-window inclusion: every state except denied and definitive-failed.
#[must_use]
pub fn counts_in_daily_window(combo: Combo) -> bool {
    combo.status != WithdrawalStatus::Denied && combo != Combo::W15
}

/// 2-of-3 archival non-landing predicate (D31).
#[must_use]
pub fn non_landing_verdict(
    last_valid: i64,
    observations: &[QuorumObservation],
) -> NonLandingVerdict {
    let mut fail_votes = 0_u8;
    for observation in observations {
        if observation.pruned
            || observation.finalized_height.is_none()
            || observation.signature_present.is_none()
        {
            return NonLandingVerdict::Unknown;
        }
        let height = observation.finalized_height.unwrap_or(0);
        let present = observation.signature_present.unwrap_or(true);
        if height > last_valid && !present {
            fail_votes = fail_votes.saturating_add(1);
        }
    }
    if fail_votes >= 2 {
        NonLandingVerdict::DefinitiveFailed
    } else {
        NonLandingVerdict::Unknown
    }
}

/// Receipt verification: signature, mint, source, dest, exact delta, finalized.
///
/// # Errors
/// Returns a static reason when any field disagrees with the rail identity
/// or the outbound payment.
pub fn verify_chain_receipt(
    identity: &crate::ports::RailIdentity,
    payment: &OutboundPaymentRow,
    attempt: &OutboundAttemptRow,
    receipt: &ChainReceipt,
) -> Result<(), &'static str> {
    if receipt.commitment != "finalized" || identity.commitment != "finalized" {
        return Err("commitment must be finalized");
    }
    if receipt.signature != attempt.signature {
        return Err("signature mismatch");
    }
    if receipt.mint != identity.usdc_mint {
        return Err("mint mismatch");
    }
    if receipt.source != identity.treasury_token_account {
        return Err("source mismatch");
    }
    if receipt.dest_token_account != payment.dest {
        return Err("dest mismatch");
    }
    if receipt.delta_micro != payment.amount_micro {
        return Err("delta mismatch");
    }
    Ok(())
}

/// Identity (a): withheld cash equals the sum of active holds.
#[must_use]
pub fn identity_a_holds(withheld_balance: i64, rows: &[WithdrawalRow]) -> bool {
    let sum: i64 = rows
        .iter()
        .filter(|row| row.combo.has_active_hold())
        .map(|row| row.amount_micro)
        .sum();
    withheld_balance == sum
}

/// Identity (b): every accepted (non-refusal) row has a hold txn.
#[must_use]
pub fn identity_b_holds(rows: &[WithdrawalRow]) -> bool {
    rows.iter().all(|row| row.hold_tx_id != Uuid::nil())
}

/// Identity (c): every terminal row has exactly one reversal XOR settle.
#[must_use]
pub fn identity_c_holds(rows: &[WithdrawalRow]) -> bool {
    rows.iter().all(|row| {
        let released = row.release_tx_id.is_some();
        let settled = row.settle_tx_id.is_some();
        match row.combo.status {
            WithdrawalStatus::Denied | WithdrawalStatus::Failed => released && !settled,
            WithdrawalStatus::Settled => settled && !released,
            _ => !released && !settled,
        }
    })
}

/// Identity (e): a second settle/deny does not create a second terminal leg.
#[must_use]
pub fn identity_e_replay_ok(terminal_legs: usize) -> bool {
    terminal_legs == 1
}

/// Identity (d): attempts 1:N, ≤1 finalized, settled payment pairs 1:1.
#[must_use]
pub fn identity_d_holds(attempts: &[OutboundAttemptRow], settled: bool) -> bool {
    let finalized = attempts
        .iter()
        .filter(|attempt| attempt.landing_state == LandingState::Finalized)
        .count();
    if finalized > 1 {
        return false;
    }
    if settled && finalized != 1 {
        return false;
    }
    let mut seen = std::collections::BTreeSet::new();
    attempts
        .iter()
        .all(|attempt| seen.insert(attempt.attempt_number))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn combination_table_accepts_exactly_the_fifteen_legal_triples() {
        const LABELS: [&str; 15] = [
            "W1", "W2", "W3", "W4", "W5", "W6", "W7", "W8", "W9", "W10", "W11", "W12", "W13",
            "W14", "W15",
        ];
        let legal = [
            Combo::W1,
            Combo::W2,
            Combo::W3,
            Combo::W4,
            Combo::W5,
            Combo::W6,
            Combo::W7,
            Combo::W8,
            Combo::W9,
            Combo::W10,
            Combo::W11,
            Combo::W12,
            Combo::W13,
            Combo::W14,
            Combo::W15,
        ];
        for (combo, label) in legal.iter().zip(LABELS) {
            assert_eq!(combo.label(), Some(label));
            assert!(combo.is_legal());
        }
        let illegal = Combo::of(
            WithdrawalStatus::Queued,
            WithdrawalReviewState::ReviewRequired,
            WithdrawalSendState::Unsent,
        );
        assert!(!illegal.is_legal());
        assert_eq!(Combo::W1.deny_target(), Some(Combo::W11));
        assert_eq!(Combo::W2.deny_target(), Some(Combo::W14));
        assert_eq!(Combo::W4.deny_target(), Some(Combo::W12));
        assert_eq!(Combo::W5.deny_target(), Some(Combo::W13));
        assert_eq!(Combo::W6.deny_target(), None);
        assert!(Combo::W1.has_active_hold());
        assert!(!Combo::W10.has_active_hold());
        assert!(!Combo::W15.has_active_hold());
        assert!(counts_in_daily_window(Combo::W10));
        assert!(!counts_in_daily_window(Combo::W11));
        assert!(!counts_in_daily_window(Combo::W15));
    }

    #[test]
    fn dest_canonicalization_and_fingerprint_are_intent_only() {
        let dest = canonicalize_dest("11111111111111111111111111111111").unwrap();
        assert!(canonicalize_dest("not-a-key").is_none());
        assert!(canonicalize_dest("").is_none());
        let user = UserId(Uuid::nil());
        let a = intent_fingerprint(user, 5_000_000, &dest);
        let b = intent_fingerprint(user, 5_000_000, &dest);
        let c = intent_fingerprint(user, 6_000_000, &dest);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn dest_warmth_requires_settled_floor_age_and_single_user() {
        let limits = WithdrawLimits::seed();
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let first = now - time::Duration::hours(80);
        let warm = DestWarmth {
            settled_micro: limits.dest_warm_floor_micro,
            first_settled_at: Some(first),
            distinct_users: 1,
            is_refund_dest: false,
        };
        assert!(warm.is_warm(limits, now));
        let dust = DestWarmth {
            settled_micro: limits.min_micro,
            first_settled_at: Some(first),
            distinct_users: 1,
            is_refund_dest: false,
        };
        assert!(!dust.is_warm(limits, now));
        let shared = DestWarmth {
            distinct_users: 2,
            ..warm
        };
        assert!(!shared.is_warm(limits, now));
        let refund = DestWarmth {
            is_refund_dest: true,
            ..warm
        };
        assert!(!refund.is_warm(limits, now));
        let young = DestWarmth {
            first_settled_at: Some(now - time::Duration::hours(1)),
            ..warm
        };
        assert!(!young.is_warm(limits, now));
        let never_settled = DestWarmth {
            first_settled_at: None,
            ..warm
        };
        assert!(!never_settled.is_warm(limits, now));
    }

    #[test]
    fn tighten_only_unions_toward_more_review() {
        let snapshot = vec!["dest_not_warm".into()];
        let current = vec!["amount_ge_auto".into()];
        let union = union_reasons(&snapshot, &current);
        assert!(blocks_auto_approve(&union));
        assert_eq!(union.len(), 2);
        assert!(!blocks_auto_approve(&[]));
    }

    #[test]
    fn two_of_three_non_landing_is_fail_closed() {
        let fail = QuorumObservation {
            endpoint: "a".into(),
            finalized_height: Some(10),
            signature_present: Some(false),
            pruned: false,
        };
        let timeout = QuorumObservation {
            endpoint: "b".into(),
            finalized_height: None,
            signature_present: None,
            pruned: false,
        };
        let pruned = QuorumObservation {
            endpoint: "c".into(),
            finalized_height: Some(10),
            signature_present: Some(false),
            pruned: true,
        };
        assert_eq!(
            non_landing_verdict(5, &[fail.clone(), fail.clone(), fail.clone()]),
            NonLandingVerdict::DefinitiveFailed
        );
        assert_eq!(
            non_landing_verdict(5, &[fail.clone(), fail.clone(), timeout]),
            NonLandingVerdict::Unknown
        );
        assert_eq!(
            non_landing_verdict(5, &[fail.clone(), fail, pruned]),
            NonLandingVerdict::Unknown
        );
        let too_early = QuorumObservation {
            endpoint: "d".into(),
            finalized_height: Some(5),
            signature_present: Some(false),
            pruned: false,
        };
        assert_eq!(
            non_landing_verdict(5, &[too_early.clone(), too_early]),
            NonLandingVerdict::Unknown
        );
    }

    #[test]
    fn identities_a_through_d_match_the_stated_predicates() {
        let hold = Uuid::from_u128(1);
        let row = WithdrawalRow {
            id: WithdrawalId(Uuid::from_u128(2)),
            user: UserId(Uuid::from_u128(3)),
            dest: "d".into(),
            amount_micro: 10,
            combo: Combo::W2,
            hold_tx_id: hold,
            release_tx_id: None,
            settle_tx_id: None,
            request_fingerprint: "f".into(),
            risk_reasons: vec![],
            requested_at: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            decided_at: None,
            sent_at: None,
            settled_at: None,
        };
        assert!(identity_a_holds(10, std::slice::from_ref(&row)));
        assert!(!identity_a_holds(0, std::slice::from_ref(&row)));
        assert_eq!(row.combo(), Combo::W2);
        assert!(identity_b_holds(std::slice::from_ref(&row)));
        assert!(identity_c_holds(std::slice::from_ref(&row)));
        let settled = WithdrawalRow {
            combo: Combo::W10,
            settle_tx_id: Some(Uuid::from_u128(9)),
            ..row.clone()
        };
        assert!(identity_c_holds(std::slice::from_ref(&settled)));
        let both = WithdrawalRow {
            release_tx_id: Some(Uuid::from_u128(8)),
            ..settled
        };
        assert!(!identity_c_holds(&[both]));
        for combo in [Combo::W11, Combo::W12, Combo::W13, Combo::W14, Combo::W15] {
            let terminal = WithdrawalRow {
                combo,
                release_tx_id: Some(Uuid::from_u128(8)),
                settle_tx_id: None,
                ..row.clone()
            };
            assert!(identity_c_holds(&[terminal]));
        }
        assert!(identity_e_replay_ok(1));
        assert!(!identity_e_replay_ok(0));
        let attempts = [OutboundAttemptRow {
            id: Uuid::from_u128(4),
            payment_id: Uuid::from_u128(5),
            attempt_number: 1,
            replaces_attempt_id: None,
            signed_tx_bytes: vec![1],
            signature: "s".into(),
            last_valid_block_height: 1,
            landing_state: LandingState::Finalized,
            lease_expires_at: None,
            evidence: None,
        }];
        assert!(identity_d_holds(&attempts, true));
        assert!(identity_d_holds(&attempts, false));
        assert!(!identity_d_holds(&[], true));
        assert!(!identity_d_holds(
            &[
                attempts[0].clone(),
                OutboundAttemptRow {
                    id: Uuid::from_u128(6),
                    attempt_number: 2,
                    landing_state: LandingState::Finalized,
                    ..attempts[0].clone()
                }
            ],
            true
        ));
    }

    #[test]
    fn subject_and_landing_names_match_the_0011_checks() {
        assert_eq!(OutboundSubject::Withdrawal.as_str(), "withdrawal");
        assert_eq!(OutboundSubject::DepositRefund.as_str(), "deposit_refund");
        assert_eq!(LandingState::Prepared.as_str(), "prepared");
        assert_eq!(LandingState::Broadcast.as_str(), "broadcast");
        assert_eq!(LandingState::Unknown.as_str(), "unknown");
        assert_eq!(LandingState::Finalized.as_str(), "finalized");
        assert_eq!(LandingState::DefinitiveFailed.as_str(), "definitive_failed");
        assert!(LandingState::Unknown.is_live());
        assert!(!LandingState::Finalized.is_live());
        let seed = WithdrawLimits::seed();
        assert!(seed.min_micro < seed.auto_approve_micro);
        assert!(seed.auto_approve_micro < seed.dest_warm_floor_micro);
    }

    #[test]
    fn receipt_verification_rejects_each_bound_field() {
        let identity = crate::ports::RailIdentity {
            genesis_hash: "genesis".into(),
            rpc_endpoints: vec!["a".into(), "b".into(), "c".into()],
            usdc_mint: "mint".into(),
            decimals: 6,
            treasury_token_account: "source".into(),
            treasury_owner: "owner".into(),
            commitment: "finalized".into(),
        };
        let payment = OutboundPaymentRow {
            id: Uuid::from_u128(10),
            subject: OutboundSubject::Withdrawal,
            subject_id: Uuid::from_u128(11),
            dest: "dest".into(),
            amount_micro: 42,
            rail_fingerprint: identity.fingerprint(),
        };
        let attempt = OutboundAttemptRow {
            id: Uuid::from_u128(12),
            payment_id: payment.id,
            attempt_number: 1,
            replaces_attempt_id: None,
            signed_tx_bytes: vec![1],
            signature: "sig".into(),
            last_valid_block_height: 10,
            landing_state: LandingState::Finalized,
            lease_expires_at: None,
            evidence: None,
        };
        let receipt = ChainReceipt {
            signature: "sig".into(),
            mint: "mint".into(),
            source: "source".into(),
            dest_token_account: "dest".into(),
            delta_micro: 42,
            commitment: "finalized".into(),
        };
        assert_eq!(
            verify_chain_receipt(&identity, &payment, &attempt, &receipt),
            Ok(())
        );
        let mut bad = receipt.clone();
        bad.commitment = "confirmed".into();
        assert_eq!(
            verify_chain_receipt(&identity, &payment, &attempt, &bad),
            Err("commitment must be finalized")
        );
        bad = receipt.clone();
        bad.signature = "other".into();
        assert_eq!(
            verify_chain_receipt(&identity, &payment, &attempt, &bad),
            Err("signature mismatch")
        );
        bad = receipt.clone();
        bad.mint = "other".into();
        assert_eq!(
            verify_chain_receipt(&identity, &payment, &attempt, &bad),
            Err("mint mismatch")
        );
        bad = receipt.clone();
        bad.source = "other".into();
        assert_eq!(
            verify_chain_receipt(&identity, &payment, &attempt, &bad),
            Err("source mismatch")
        );
        bad = receipt.clone();
        bad.dest_token_account = "other".into();
        assert_eq!(
            verify_chain_receipt(&identity, &payment, &attempt, &bad),
            Err("dest mismatch")
        );
        bad = receipt;
        bad.delta_micro = 41;
        assert_eq!(
            verify_chain_receipt(&identity, &payment, &attempt, &bad),
            Err("delta mismatch")
        );
    }
}
