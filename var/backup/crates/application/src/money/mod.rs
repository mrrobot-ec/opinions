//! Phase 7 money path. W1 owns withdraw_* via `ports::money` path includes;
//! W2 owns kyc/geo/sanctions/aml; W3 owns credits/referrals/deposit.

pub mod admin;
pub mod aml;
pub mod credits;
pub mod enforcement;
pub mod geo;
pub mod kyc;
pub mod phone_verification;
pub mod referrals;
pub mod sanctions;
pub mod self_exclusion;
pub mod statuses;

use async_trait::async_trait;
use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{AppError, StoreError};
use crate::model::{DepositId, DepositMachineStatus, MarketId, UserId};
use crate::ports::{Committable, ScreenVerdict};
use domain::money::MicroUsd;

pub use admin::{
    accept_inbox, enforce, hex_encode, inbox_algebra, payload_hash, ComplianceAdminStore,
    ComplianceAdminTx, EgressDisposition, FrozenFundsLicense, GateDecision, GateMutation,
    InboxOutcome, InboxRecord, MoneyProposal, ProposalStatus, UserComplianceSnapshot, BAN_DELAY,
    FROZEN_FUNDS_DELAY,
};
pub use aml::{
    evaluate_aml, evaluate_at_request, AmlDirection, AmlFlag, AmlKind, AmlLeg, AmlPolicy,
    PINNED_WITHDRAW_BAND_MICRO,
};
pub use kyc::{kyc_meets, KycEvent};
pub use sanctions::SanctionScreening;
pub use self_exclusion::{SelfExclusion, UserDepositLimit};
pub use statuses::{
    homogeneous_cap_exceeded, parse_user_status, ShadowCaps, PUBLISHED_TIER0_CAP_MICRO,
};

/// Typed `fee_bps_override:{market}` value (D36).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeeBpsOverride {
    Inherit,
    Override(u16),
}

impl FeeBpsOverride {
    /// Parses the closed D36 enum: `"inherit" | {"override": 10..=200}`.
    ///
    /// # Errors
    /// Malformed or out-of-range persisted config. Corrupt configuration is
    /// never silently interpreted as `inherit` on a money path.
    pub fn parse(value: Option<&Value>) -> Result<Self, StoreError> {
        let Some(value) = value else {
            return Ok(Self::Inherit);
        };
        if value.as_str() == Some("inherit") {
            return Ok(Self::Inherit);
        }
        if let Some(object) = value.as_object() {
            if object.len() == 1 {
                if let Some(bps) = object.get("override").and_then(Value::as_u64) {
                    if (10..=200).contains(&bps) {
                        #[allow(clippy::cast_possible_truncation)]
                        let bps = bps as u16;
                        return Ok(Self::Override(bps));
                    }
                }
            }
        }
        Err(StoreError::Invariant("invalid fee override"))
    }

    /// Override replaces the pool stamp; inherit keeps it.
    #[must_use]
    pub fn base_bps(self, pool_fee_bps: u16) -> u16 {
        match self {
            Self::Inherit => pool_fee_bps,
            Self::Override(bps) => bps,
        }
    }
}

/// Config key for a market's fee override.
#[must_use]
pub fn fee_override_key(market: MarketId) -> String {
    format!("fee_bps_override:{}", market.0)
}

/// Wire name for a deposit-machine status.
#[must_use]
pub fn deposit_status_name(status: DepositMachineStatus) -> &'static str {
    match status {
        DepositMachineStatus::ObservedFinalized => "observed_finalized",
        DepositMachineStatus::AdmissionPending => "admission_pending",
        DepositMachineStatus::Admitted => "admitted",
        DepositMachineStatus::AdmittedLegacy => "admitted_legacy",
        DepositMachineStatus::ComplianceHold => "compliance_hold",
        DepositMachineStatus::RefundApproved => "refund_approved",
        DepositMachineStatus::RefundSending => "refund_sending",
        DepositMachineStatus::Refunded => "refunded",
    }
}

/// # Errors
/// Unknown status string.
pub fn parse_deposit_status(value: &str) -> Result<DepositMachineStatus, StoreError> {
    match value {
        // Raw legacy `seen`/`confirmed` never reach the machine decoder:
        // 0011 rewrites them to `quarantined_legacy`, which is excluded
        // upstream and must never decode as an active observation.
        "observed_finalized" => Ok(DepositMachineStatus::ObservedFinalized),
        "admission_pending" => Ok(DepositMachineStatus::AdmissionPending),
        "admitted" => Ok(DepositMachineStatus::Admitted),
        "admitted_legacy" | "credited" => Ok(DepositMachineStatus::AdmittedLegacy),
        "compliance_hold" => Ok(DepositMachineStatus::ComplianceHold),
        "refund_approved" => Ok(DepositMachineStatus::RefundApproved),
        "refund_sending" => Ok(DepositMachineStatus::RefundSending),
        "refunded" => Ok(DepositMachineStatus::Refunded),
        _ => Err(StoreError::Invariant("unknown deposit machine status")),
    }
}

/// True when the status still sits on `DepositSuspense`.
#[must_use]
pub fn is_suspense_liability(status: DepositMachineStatus) -> bool {
    !matches!(
        status,
        DepositMachineStatus::Admitted
            | DepositMachineStatus::AdmittedLegacy
            | DepositMachineStatus::Refunded
    )
}

/// Fresh Clear is the only progressing verdict (D33).
#[must_use]
pub fn allows_progress(verdict: &ScreenVerdict, now: OffsetDateTime) -> bool {
    match verdict {
        ScreenVerdict::Clear { expires_at, .. } => *expires_at > now,
        ScreenVerdict::Hit | ScreenVerdict::Indeterminate => false,
    }
}

/// Aged Clear must be re-screened at the send boundary.
#[must_use]
pub fn needs_rescreen(verdict: &ScreenVerdict, now: OffsetDateTime) -> bool {
    match verdict {
        ScreenVerdict::Clear { expires_at, .. } => *expires_at <= now,
        ScreenVerdict::Indeterminate => true,
        ScreenVerdict::Hit => false,
    }
}

/// Locked user row the compliance writers need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserComplianceRow {
    pub user: UserId,
    pub kyc_tier: i32,
    pub status: String,
}

/// Append-only compliance-decision fact (machine path: never `admin_actions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComplianceDecision {
    pub id: Uuid,
    pub subject_type: String,
    pub subject_id: Uuid,
    pub kind: String,
    pub actor: String,
    pub at: OffsetDateTime,
    pub payload: Value,
}

/// W2 compliance unit of work.
#[async_trait]
pub trait ComplianceTx: Committable + Send {
    async fn lock_user(&mut self, user: UserId) -> Result<UserComplianceRow, StoreError>;
    async fn insert_kyc_event(&mut self, event: KycEvent) -> Result<(), StoreError>;
    async fn set_kyc_tier(&mut self, user: UserId, tier: i32) -> Result<(), StoreError>;
    async fn latest_kyc(&mut self, user: UserId) -> Result<Option<KycEvent>, StoreError>;
    async fn insert_screening(&mut self, screening: SanctionScreening) -> Result<(), StoreError>;
    async fn latest_screening(
        &mut self,
        user: UserId,
        context: &str,
    ) -> Result<Option<SanctionScreening>, StoreError>;
    async fn insert_aml_flag(&mut self, flag: AmlFlag) -> Result<(), StoreError>;
    async fn open_aml_flags(&mut self, user: UserId) -> Result<Vec<AmlFlag>, StoreError>;
    async fn list_aml_legs(
        &mut self,
        user: UserId,
        since: OffsetDateTime,
    ) -> Result<Vec<AmlLeg>, StoreError>;
    async fn list_dest_aml_legs(
        &mut self,
        dest: &str,
        since: OffsetDateTime,
    ) -> Result<Vec<AmlLeg>, StoreError>;
    async fn record_aml_leg(&mut self, leg: AmlLeg) -> Result<(), StoreError>;
    async fn insert_decision(&mut self, decision: ComplianceDecision) -> Result<(), StoreError>;
}

/// Factory for [`ComplianceTx`].
#[async_trait]
pub trait ComplianceStore: Send + Sync {
    async fn compliance_tx(&self) -> Result<Box<dyn ComplianceTx + '_>, StoreError>;
}

/// Inbound observation the chain watcher books into suspense.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedDeposit {
    pub user: Option<UserId>,
    pub amount: MicroUsd,
    pub chain_sig: String,
    pub source_address: String,
    pub dest_address: String,
    pub mint: String,
    pub slot: i64,
}

/// Persisted deposit-machine row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepositMachineRow {
    pub id: DepositId,
    pub user: Option<UserId>,
    pub amount: MicroUsd,
    pub chain_sig: String,
    pub source_address: String,
    pub dest_address: String,
    pub mint: String,
    /// Startup-pinned identity of the inbound rail that produced the
    /// observation. Refunds must reuse this exact identity.
    pub rail_fingerprint: String,
    pub slot: i64,
    pub status: DepositMachineStatus,
    pub suspense_tx_id: Option<Uuid>,
    pub admit_tx_id: Option<Uuid>,
    pub refund_tx_id: Option<Uuid>,
}

/// Source-locked outbound payment created for a held deposit refund.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepositRefundPayment {
    pub id: Uuid,
    pub deposit: DepositId,
    pub dest: String,
    pub amount_micro: i64,
    pub rail_fingerprint: String,
}

/// Result of convert-then-collect inside a `lock_user` transaction.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConvertCollectReceipt {
    pub converted_micro: i64,
    pub collected_micro: i64,
    pub lots_converted: u32,
    pub conversion_txns: Vec<CreditConversionTxn>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreditConversionTxn {
    pub lot_id: Uuid,
    pub retire_txn: Uuid,
    pub pay_txn: Uuid,
    pub replayed: bool,
}

/// Referee's earliest Paid market and aggregate trade notional on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReferralPaidMarket {
    pub market: MarketId,
    pub notional_micro: i64,
}

/// Credit grant class stamped on the lot (D32).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantClass {
    RealMoney,
    Sweeps,
}

impl GrantClass {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RealMoney => "real_money",
            Self::Sweeps => "sweeps",
        }
    }

    /// # Errors
    /// Unknown class.
    pub fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "real_money" => Ok(Self::RealMoney),
            "sweeps" => Ok(Self::Sweeps),
            _ => Err(StoreError::Invariant("unknown grant class")),
        }
    }
}

/// One immutable credit grant lot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditLotRow {
    pub id: Uuid,
    pub user: UserId,
    pub source: String,
    pub amount_micro: i64,
    pub granted_at: OffsetDateTime,
    pub grant_class: GrantClass,
    pub policy_version: String,
    pub converted_at: Option<OffsetDateTime>,
}

/// Allocation kind (D32 algebra).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocationKind {
    Allocated,
    Finalized,
    Reversed,
}

impl AllocationKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allocated => "allocated",
            Self::Finalized => "finalized",
            Self::Reversed => "reversed",
        }
    }

    /// # Errors
    /// Unknown kind.
    pub fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "allocated" => Ok(Self::Allocated),
            "finalized" => Ok(Self::Finalized),
            "reversed" => Ok(Self::Reversed),
            _ => Err(StoreError::Invariant("unknown allocation kind")),
        }
    }
}

/// One append-only fee-allocation fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocationFact {
    pub id: Uuid,
    pub trade_id: Uuid,
    pub lot_id: Uuid,
    pub split_seq: i32,
    pub amount_micro: i64,
    pub kind: AllocationKind,
    pub source_allocation_id: Option<Uuid>,
    pub idempotency_key: String,
}

/// W3 credit / deposit-machine IO, composed onto write transactions.
#[async_trait]
pub trait CreditIo: Send {
    async fn lock_credit_lots(&mut self, user: UserId) -> Result<(), StoreError>;
    async fn insert_credit_lot(&mut self, lot: &CreditLotRow) -> Result<Uuid, StoreError>;
    async fn lots_for_user(&mut self, user: UserId) -> Result<Vec<CreditLotRow>, StoreError>;
    async fn lot_by_idempotency(&mut self, key: &str) -> Result<Option<CreditLotRow>, StoreError>;
    async fn remember_lot_idempotency(&mut self, key: &str, lot: Uuid) -> Result<(), StoreError>;
    async fn mark_lot_converted(&mut self, lot: Uuid, at: OffsetDateTime)
        -> Result<(), StoreError>;
    async fn insert_allocation(&mut self, fact: &AllocationFact) -> Result<(), StoreError>;
    async fn allocations_for_lot(&mut self, lot: Uuid) -> Result<Vec<AllocationFact>, StoreError>;
    async fn live_allocations_for_trade(
        &mut self,
        trade: Uuid,
    ) -> Result<Vec<AllocationFact>, StoreError>;
    async fn bonus_reserve_balance(&mut self) -> Result<i64, StoreError>;
    async fn remaining_real_money_promise(&mut self) -> Result<i64, StoreError>;
    async fn bonus_minted_since(&mut self, since: OffsetDateTime) -> Result<i64, StoreError>;

    async fn observe_deposit(
        &mut self,
        obs: &ObservedDeposit,
        suspense_tx: Uuid,
        rail_fingerprint: &str,
    ) -> Result<DepositId, StoreError>;
    async fn deposit_machine_by_sig(
        &mut self,
        sig: &str,
    ) -> Result<Option<DepositMachineRow>, StoreError>;
    async fn deposit_machine_by_id(
        &mut self,
        id: DepositId,
    ) -> Result<Option<DepositMachineRow>, StoreError>;
    async fn cas_deposit_status(
        &mut self,
        id: DepositId,
        from: DepositMachineStatus,
        to: DepositMachineStatus,
    ) -> Result<bool, StoreError>;
    async fn mark_admitted(&mut self, id: DepositId, admit_tx: Uuid) -> Result<(), StoreError>;
    async fn mark_refunded(&mut self, id: DepositId, refund_tx: Uuid) -> Result<(), StoreError>;
    async fn insert_refund_payment(
        &mut self,
        deposit: DepositId,
        dest: &str,
        amount: i64,
        rail_fp: &str,
    ) -> Result<Uuid, StoreError>;
    async fn refund_payment_for_deposit(
        &mut self,
        deposit: DepositId,
    ) -> Result<Option<DepositRefundPayment>, StoreError>;

    async fn user_status(&mut self, user: UserId) -> Result<String, StoreError>;
    async fn user_kyc_tier(&mut self, user: UserId) -> Result<i32, StoreError>;
    async fn fresh_clear(
        &mut self,
        user: UserId,
        context: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError>;
    async fn self_excluded(
        &mut self,
        user: UserId,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError>;
    async fn deposit_limit_micro(&mut self, user: UserId) -> Result<Option<i64>, StoreError>;
    async fn config_flag(&mut self, key: &str) -> Result<bool, StoreError>;
    /// Read a mandatory boolean policy value without applying an optional
    /// feature-flag default.
    async fn required_config_flag(&mut self, key: &str) -> Result<bool, StoreError>;
    async fn config_i64(&mut self, key: &str) -> Result<Option<i64>, StoreError>;
    async fn config_text(&mut self, key: &str) -> Result<Option<String>, StoreError>;
    async fn insert_compliance_decision(
        &mut self,
        decision: ComplianceDecision,
    ) -> Result<(), StoreError>;

    async fn phone_verified(
        &mut self,
        user: UserId,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError>;
    async fn referral_code_for_user(&mut self, user: UserId) -> Result<Option<String>, StoreError>;
    async fn referral_code_owner(&mut self, code: &str) -> Result<Option<UserId>, StoreError>;
    async fn insert_referral_code(&mut self, user: UserId, code: &str) -> Result<(), StoreError>;
    async fn insert_referral_bind(
        &mut self,
        referrer: UserId,
        referee: UserId,
        bind_key: &str,
    ) -> Result<Uuid, StoreError>;
    async fn referral_bind_by_key(&mut self, bind_key: &str) -> Result<Option<Uuid>, StoreError>;
    async fn referral_bind_parties(
        &mut self,
        bind: Uuid,
    ) -> Result<Option<(UserId, UserId, String)>, StoreError>;
    async fn referral_bind_for_referee(
        &mut self,
        referee: UserId,
    ) -> Result<Option<Uuid>, StoreError>;
    async fn first_paid_market_for_user(
        &mut self,
        user: UserId,
    ) -> Result<Option<ReferralPaidMarket>, StoreError>;
    async fn referral_referees_for_paid_market(
        &mut self,
        market: MarketId,
    ) -> Result<Vec<UserId>, StoreError>;

    async fn convert_then_collect(
        &mut self,
        user: UserId,
        now: OffsetDateTime,
        key: &str,
    ) -> Result<ConvertCollectReceipt, AppError>;
}

/// Map a missing required preview version to the public 422.
#[must_use]
pub fn missing_config_version() -> AppError {
    AppError::ExpectedConfigVersionRequired
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    #[test]
    fn fee_override_parses_only_the_closed_typed_enum() {
        assert_eq!(
            FeeBpsOverride::parse(None).unwrap(),
            FeeBpsOverride::Inherit
        );
        assert_eq!(
            FeeBpsOverride::parse(Some(&json!("inherit"))).unwrap(),
            FeeBpsOverride::Inherit
        );
        assert_eq!(
            FeeBpsOverride::parse(Some(&json!({"override": 40}))).unwrap(),
            FeeBpsOverride::Override(40)
        );
        assert!(FeeBpsOverride::parse(Some(&json!(55))).is_err());
        assert!(FeeBpsOverride::parse(Some(&json!({"override": 9}))).is_err());
        assert!(FeeBpsOverride::parse(Some(&json!({"override": "40"}))).is_err());
        assert!(FeeBpsOverride::parse(Some(&json!({"override": 40, "extra": true}))).is_err());
        assert_eq!(FeeBpsOverride::Override(40).base_bps(100), 40);
        assert_eq!(FeeBpsOverride::Inherit.base_bps(100), 100);
    }

    #[test]
    fn deposit_status_round_trips_and_maps_legacy() {
        for status in [
            DepositMachineStatus::ObservedFinalized,
            DepositMachineStatus::AdmissionPending,
            DepositMachineStatus::Admitted,
            DepositMachineStatus::AdmittedLegacy,
            DepositMachineStatus::ComplianceHold,
            DepositMachineStatus::RefundApproved,
            DepositMachineStatus::RefundSending,
            DepositMachineStatus::Refunded,
        ] {
            assert_eq!(
                parse_deposit_status(deposit_status_name(status)).unwrap(),
                status
            );
        }
        assert_eq!(
            parse_deposit_status("credited").unwrap(),
            DepositMachineStatus::AdmittedLegacy
        );
        assert!(is_suspense_liability(
            DepositMachineStatus::ObservedFinalized
        ));
        assert!(!is_suspense_liability(DepositMachineStatus::Admitted));
        assert!(parse_deposit_status("mystery").is_err());
    }

    #[test]
    fn clear_progress_is_fresh_only() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let later = now + time::Duration::hours(1);
        let clear = ScreenVerdict::Clear {
            checked_at: now,
            expires_at: later,
            policy_version: "1".into(),
        };
        assert!(allows_progress(&clear, now));
        assert!(!allows_progress(&clear, later));
        assert!(needs_rescreen(&clear, later));
        assert!(!allows_progress(&ScreenVerdict::Hit, now));
        assert!(!allows_progress(&ScreenVerdict::Indeterminate, now));
        assert!(needs_rescreen(&ScreenVerdict::Indeterminate, now));
        assert!(!needs_rescreen(&ScreenVerdict::Hit, now));
    }

    #[test]
    fn credit_database_enums_have_total_wire_decoders() {
        for class in [GrantClass::RealMoney, GrantClass::Sweeps] {
            assert_eq!(GrantClass::parse(class.as_str()).unwrap(), class);
        }
        assert!(GrantClass::parse("unknown").is_err());
        for kind in [
            AllocationKind::Allocated,
            AllocationKind::Finalized,
            AllocationKind::Reversed,
        ] {
            assert_eq!(AllocationKind::parse(kind.as_str()).unwrap(), kind);
        }
        assert!(AllocationKind::parse("unknown").is_err());
    }
}
