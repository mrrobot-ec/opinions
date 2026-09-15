//! Phase 7 money / compliance ports (plan 7.0a).
//!
//! Waves own the real implementations. Every skeleton path fails closed
//! with [`StoreError::Unavailable`]`("phase7:<area>")`.

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::error::StoreError;
use crate::model::{DepositId, UserId};

use super::Committable;

/// Immutable Solana / USDC rail identity, validated at process start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RailIdentity {
    pub genesis_hash: String,
    pub rpc_endpoints: Vec<String>,
    pub usdc_mint: String,
    pub decimals: u8,
    pub treasury_owner: String,
    pub treasury_token_account: String,
    pub commitment: String,
}

impl RailIdentity {
    /// # Errors
    /// Missing required fields, decimals ≠ 6, or fewer than 3 RPC endpoints
    /// (the 2-of-3 archival quorum).
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.genesis_hash.is_empty() {
            return Err("genesis hash is required");
        }
        if self.rpc_endpoints.len() != 3 {
            return Err("need exactly three RPC endpoints for the 2-of-3 quorum");
        }
        if self.rpc_endpoints.iter().any(String::is_empty) {
            return Err("rpc endpoint must be non-empty");
        }
        let mut endpoints: Vec<&String> = self.rpc_endpoints.iter().collect();
        endpoints.sort();
        endpoints.dedup();
        if endpoints.len() != 3 {
            return Err("rpc endpoints must be distinct for an independent quorum");
        }
        if self.usdc_mint.is_empty() {
            return Err("usdc mint is required");
        }
        if self.decimals != 6 {
            return Err("usdc decimals must be 6");
        }
        if self.treasury_owner.is_empty() || self.treasury_token_account.is_empty() {
            return Err("treasury owner and token account are required");
        }
        if self.commitment != "finalized" {
            return Err("commitment must be finalized");
        }
        Ok(())
    }

    #[must_use]
    pub fn fingerprint(&self) -> String {
        // Sorted endpoints so operand order cannot fork the identity; the
        // deposit-depth interpretation is the pinned D31/D32 constant (depth
        // measured against finalized roots), stamped so a future change is a
        // visible identity change, never silent.
        let mut endpoints = self.rpc_endpoints.clone();
        endpoints.sort();
        format!(
            "{}|{}|{}|{}|{}|{}|{}|depth=finalized-roots",
            self.genesis_hash,
            self.usdc_mint,
            self.decimals,
            self.treasury_owner,
            self.treasury_token_account,
            self.commitment,
            endpoints.join(",")
        )
    }
}

/// Screening result algebra (D33).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenVerdict {
    Clear {
        checked_at: OffsetDateTime,
        expires_at: OffsetDateTime,
        policy_version: String,
    },
    Hit,
    Indeterminate,
}

#[async_trait]
pub trait OutboundRails: Send + Sync {
    async fn persist_signed(
        &self,
        payment_id: uuid::Uuid,
        bytes: &[u8],
        signature: &str,
    ) -> Result<(), StoreError>;
    async fn broadcast(&self, payment_id: uuid::Uuid) -> Result<(), StoreError>;
}

#[async_trait]
pub trait KycProvider: Send + Sync {
    async fn start_verification(&self, user: UserId) -> Result<String, StoreError>;
}

#[async_trait]
pub trait SanctionsScreen: Send + Sync {
    async fn screen(&self, user: UserId, context: &str) -> Result<ScreenVerdict, StoreError>;
}

#[async_trait]
pub trait GeoResolver: Send + Sync {
    async fn resolve(&self, ip: std::net::IpAddr) -> Result<ScreenVerdict, StoreError>;
}

#[async_trait]
pub trait Alerter: Send + Sync {
    async fn page(&self, severity: &str, key: &str, body: &str) -> Result<(), StoreError>;
}

#[async_trait]
pub trait ChainBalance: Send + Sync {
    async fn wallet_balance_micro(&self) -> Result<i64, StoreError>;
}

#[async_trait]
pub trait Telemetry: Send + Sync {
    fn counter(&self, name: &str, value: u64);
}

#[async_trait]
pub trait PhoneVerification: Send + Sync {
    async fn start_challenge(&self, user: UserId, e164: &str) -> Result<(), StoreError>;
    async fn verify(&self, user: UserId, code: &str) -> Result<bool, StoreError>;
}

/// Withdrawal request / decide / send / settle unit of work (W1).
pub trait WithdrawTx:
    super::IdempotencyGuard
    + super::UserLockGuard
    + super::OutboxWriter
    + super::AuditWrite
    + WithdrawIo
    + Committable
{
}
impl<T> WithdrawTx for T where
    T: super::IdempotencyGuard
        + super::UserLockGuard
        + super::OutboxWriter
        + super::AuditWrite
        + WithdrawIo
        + Committable
{
}

#[path = "../money/types.rs"]
mod types;
pub use types::*;

#[path = "../money/outbound.rs"]
pub mod outbound;
#[path = "../money/withdraw_decide.rs"]
pub mod withdraw_decide;
#[path = "../money/withdraw_request.rs"]
pub mod withdraw_request;
#[path = "../money/withdraw_send.rs"]
pub mod withdraw_send;
pub use withdraw_send::SignaturePresence;
#[path = "../money/withdraw_reconcile.rs"]
pub mod withdraw_reconcile;
#[path = "../money/withdraw_settle.rs"]
pub mod withdraw_settle;

#[path = "../fakes/withdraw.rs"]
pub mod withdraw_fakes;

/// W3 credit/deposit lifecycle composed onto `lock_user` transactions.
pub trait CreditLifecycle:
    crate::money::CreditIo + super::LedgerWriter + super::ReceivableCollectionIo
{
}
impl<T> CreditLifecycle for T where
    T: crate::money::CreditIo + super::LedgerWriter + super::ReceivableCollectionIo
{
}

/// Deposit-side AML evaluation inside the admission unit of work.
///
/// Implementations must identify the candidate by `deposit`, persist its AML
/// leg at most once, use `source_address` as the counterparty, and return
/// whether any AML flag for the user is currently open. All reads and writes
/// occur in the caller's transaction after the user lock is held.
#[async_trait]
pub trait DepositAmlIo: Send {
    /// Evaluate and, on the first call, record one deposit candidate.
    ///
    /// # Errors
    /// Backend failures or a conflicting durable candidate binding.
    async fn evaluate_deposit_aml_candidate(
        &mut self,
        deposit: DepositId,
        user: UserId,
        source_address: &str,
        amount_micro: i64,
        at: OffsetDateTime,
    ) -> Result<bool, StoreError>;
}

/// Deposit observation / admission unit of work (W3).
pub trait DepositAdmissionTx:
    super::IdempotencyGuard
    + super::UserLockGuard
    + super::LedgerWriter
    + super::OutboxWriter
    + super::ReceivableCollectionIo
    + super::AuditWrite
    + super::DepositWriter
    + crate::money::CreditIo
    + DepositAmlIo
    + OutboundIo
    + MoneyProposalIo
    + Committable
{
}
impl<T> DepositAdmissionTx for T where
    T: super::IdempotencyGuard
        + super::UserLockGuard
        + super::LedgerWriter
        + super::OutboxWriter
        + super::ReceivableCollectionIo
        + super::AuditWrite
        + super::DepositWriter
        + crate::money::CreditIo
        + DepositAmlIo
        + OutboundIo
        + MoneyProposalIo
        + Committable
{
}

/// Grant / convert unit of work (W3).
pub trait CreditConvertTx:
    super::IdempotencyGuard
    + super::UserLockGuard
    + super::LedgerWriter
    + super::OutboxWriter
    + super::ReceivableCollectionIo
    + crate::money::CreditIo
    + Committable
{
}
impl<T> CreditConvertTx for T where
    T: super::IdempotencyGuard
        + super::UserLockGuard
        + super::LedgerWriter
        + super::OutboxWriter
        + super::ReceivableCollectionIo
        + crate::money::CreditIo
        + Committable
{
}

/// Fail-closed placeholders until the owning wave lands.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableMoney;

#[async_trait]
impl OutboundRails for UnavailableMoney {
    async fn persist_signed(
        &self,
        _payment_id: uuid::Uuid,
        _bytes: &[u8],
        _signature: &str,
    ) -> Result<(), StoreError> {
        Err(StoreError::Unavailable("phase7:rails"))
    }
    async fn broadcast(&self, _payment_id: uuid::Uuid) -> Result<(), StoreError> {
        Err(StoreError::Unavailable("phase7:rails"))
    }
}

#[async_trait]
impl KycProvider for UnavailableMoney {
    async fn start_verification(&self, _user: UserId) -> Result<String, StoreError> {
        Err(StoreError::Unavailable("phase7:kyc"))
    }
}

#[async_trait]
impl SanctionsScreen for UnavailableMoney {
    async fn screen(&self, _user: UserId, _context: &str) -> Result<ScreenVerdict, StoreError> {
        Err(StoreError::Unavailable("phase7:sanctions"))
    }
}

#[async_trait]
impl GeoResolver for UnavailableMoney {
    async fn resolve(&self, _ip: std::net::IpAddr) -> Result<ScreenVerdict, StoreError> {
        Err(StoreError::Unavailable("phase7:geo"))
    }
}

#[async_trait]
impl Alerter for UnavailableMoney {
    async fn page(&self, _severity: &str, _key: &str, _body: &str) -> Result<(), StoreError> {
        Err(StoreError::Unavailable("phase7:alerter"))
    }
}

#[async_trait]
impl ChainBalance for UnavailableMoney {
    async fn wallet_balance_micro(&self) -> Result<i64, StoreError> {
        Err(StoreError::Unavailable("phase7:chain-balance"))
    }
}

#[async_trait]
impl Telemetry for UnavailableMoney {
    fn counter(&self, _name: &str, _value: u64) {}
}

#[async_trait]
impl PhoneVerification for UnavailableMoney {
    async fn start_challenge(&self, _user: UserId, _e164: &str) -> Result<(), StoreError> {
        Err(StoreError::Unavailable("phase7:phone"))
    }
    async fn verify(&self, _user: UserId, _code: &str) -> Result<bool, StoreError> {
        Err(StoreError::Unavailable("phase7:phone"))
    }
}

#[async_trait]
impl FeeAllocationFinalize for UnavailableMoney {}
#[async_trait]
impl FeeAllocationReverse for UnavailableMoney {}
#[async_trait]
impl MoneyProposalIo for UnavailableMoney {}
#[async_trait]
impl ReferralPaidFinalize for UnavailableMoney {}

/// D32 referral-on-Paid role (W3): both referral legs fire when the referee's
/// first Paid market qualifies. Enumeration is lock-free and feeds resolve's
/// sorted pre-market advisory locks; the grant hook re-validates under those
/// locks and runs ONLY on Paid. Defaults are safe no-ops: an unimplemented
/// backend simply grants nothing (credits mint under caps — not a money-egress
/// surface), keeping every pre-Phase-7 resolve test green.
#[async_trait]
pub trait ReferralPaidFinalize: Send {
    /// Users whose advisory locks the resolve tx must take before the market
    /// lock: every potentially qualifying referee (bound or not) plus each
    /// currently bound referrer. Implementations must sort/dedup the result;
    /// resolution unions it with voters into one global class-2 lock run.
    async fn referral_relevant_users(
        &mut self,
        _market: crate::model::MarketId,
    ) -> Result<Vec<UserId>, StoreError> {
        Ok(Vec::new())
    }
    /// Grants qualifying referral pairs; returns the number of grants minted.
    ///
    /// # Errors
    /// Backend failures; mint-cap refusals.
    async fn grant_referrals_on_paid(
        &mut self,
        _market: crate::model::MarketId,
    ) -> Result<u32, StoreError> {
        Ok(0)
    }
}

/// Narrow dual-control proposal role (0011 `money_command_proposals`): the
/// slice of W2's admin surface that money-effect transactions compose so a
/// proposal CAS and its economic effect commit atomically (codex-p7r3 B4).
/// Defaults fail closed until the owning lane's implementation delegates to
/// the real persistence.
#[async_trait]
pub trait MoneyProposalIo: Send {
    /// # Errors
    /// [`StoreError::Unavailable`] until the real implementation lands.
    async fn insert_proposal(
        &mut self,
        _proposal: crate::money::MoneyProposal,
    ) -> Result<crate::money::MoneyProposal, StoreError> {
        Err(StoreError::Unavailable("phase7:money-proposal"))
    }
    /// # Errors
    /// [`StoreError::Unavailable`] until the real implementation lands.
    async fn get_proposal_by_replay(
        &mut self,
        _replay_key: &str,
    ) -> Result<Option<crate::money::MoneyProposal>, StoreError> {
        Err(StoreError::Unavailable("phase7:money-proposal"))
    }
    /// # Errors
    /// [`StoreError::Unavailable`] until the real implementation lands.
    async fn get_proposal(
        &mut self,
        _id: uuid::Uuid,
    ) -> Result<crate::money::MoneyProposal, StoreError> {
        Err(StoreError::Unavailable("phase7:money-proposal"))
    }
    /// # Errors
    /// [`StoreError::Unavailable`] until the real implementation lands.
    async fn confirm_proposal(
        &mut self,
        _id: uuid::Uuid,
        _confirmer: &str,
        _now: OffsetDateTime,
    ) -> Result<crate::money::MoneyProposal, StoreError> {
        Err(StoreError::Unavailable("phase7:money-proposal"))
    }
}

/// Set-based fee-allocation finalization for the resolve-to-Paid transaction
/// (D32): every provisional allocation whose trade belongs to the newly Paid
/// market moves `allocated → finalized` in the same DB tx. Defaults fail
/// closed until W3's real implementation replaces them.
#[async_trait]
pub trait FeeAllocationFinalize: Send {
    /// Returns the number of allocations finalized.
    ///
    /// # Errors
    /// [`StoreError::Unavailable`] until the real implementation lands.
    async fn finalize_market_fee_allocations(
        &mut self,
        _market: crate::model::MarketId,
    ) -> Result<u32, StoreError> {
        Err(StoreError::Unavailable("phase7:fee-allocation-finalize"))
    }
}

/// Set-based fee-allocation reversal for the Voided-unwind transaction (D32):
/// every live provisional allocation of the unwound market gains a `reversed`
/// terminal child in the same DB tx. Voided books never finalize.
#[async_trait]
pub trait FeeAllocationReverse: Send {
    /// Returns the number of allocations reversed.
    ///
    /// # Errors
    /// [`StoreError::Unavailable`] until the real implementation lands.
    async fn reverse_market_fee_allocations(
        &mut self,
        _market: crate::model::MarketId,
    ) -> Result<u32, StoreError> {
        Err(StoreError::Unavailable("phase7:fee-allocation-reverse"))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn valid_rail() -> RailIdentity {
        RailIdentity {
            genesis_hash: "gen".into(),
            rpc_endpoints: vec!["a".into(), "b".into(), "c".into()],
            usdc_mint: "mint".into(),
            decimals: 6,
            treasury_owner: "owner".into(),
            treasury_token_account: "ata".into(),
            commitment: "finalized".into(),
        }
    }

    #[test]
    fn rail_identity_accepts_a_complete_finalized_config() {
        let rail = valid_rail();
        assert_eq!(rail.validate(), Ok(()));
        assert!(rail.fingerprint().contains("mint"));
    }

    #[test]
    fn rail_identity_rejects_each_required_field() {
        let mut r = valid_rail();
        r.genesis_hash.clear();
        assert_eq!(r.validate(), Err("genesis hash is required"));
        r = valid_rail();
        r.rpc_endpoints.pop();
        assert_eq!(
            r.validate(),
            Err("need exactly three RPC endpoints for the 2-of-3 quorum")
        );
        r = valid_rail();
        r.rpc_endpoints[0].clear();
        assert_eq!(r.validate(), Err("rpc endpoint must be non-empty"));
        r = valid_rail();
        r.rpc_endpoints[2] = r.rpc_endpoints[0].clone();
        assert_eq!(
            r.validate(),
            Err("rpc endpoints must be distinct for an independent quorum")
        );
        r = valid_rail();
        r.usdc_mint.clear();
        assert_eq!(r.validate(), Err("usdc mint is required"));
        r = valid_rail();
        r.decimals = 9;
        assert_eq!(r.validate(), Err("usdc decimals must be 6"));
        r = valid_rail();
        r.treasury_owner.clear();
        assert_eq!(
            r.validate(),
            Err("treasury owner and token account are required")
        );
        r = valid_rail();
        r.commitment = "confirmed".into();
        assert_eq!(r.validate(), Err("commitment must be finalized"));
    }

    #[tokio::test]
    async fn unavailable_money_ports_fail_closed() {
        let u = UnavailableMoney;
        let user = UserId(uuid::Uuid::nil());
        assert!(matches!(
            u.persist_signed(uuid::Uuid::nil(), &[], "").await,
            Err(StoreError::Unavailable("phase7:rails"))
        ));
        assert!(matches!(
            u.broadcast(uuid::Uuid::nil()).await,
            Err(StoreError::Unavailable("phase7:rails"))
        ));
        assert!(matches!(
            u.start_verification(user).await,
            Err(StoreError::Unavailable("phase7:kyc"))
        ));
        assert!(matches!(
            SanctionsScreen::screen(&u, user, "withdraw").await,
            Err(StoreError::Unavailable("phase7:sanctions"))
        ));
        assert!(matches!(
            u.resolve(std::net::IpAddr::from([127, 0, 0, 1])).await,
            Err(StoreError::Unavailable("phase7:geo"))
        ));
        assert!(matches!(
            u.page("crit", "k", "b").await,
            Err(StoreError::Unavailable("phase7:alerter"))
        ));
        assert!(matches!(
            u.wallet_balance_micro().await,
            Err(StoreError::Unavailable("phase7:chain-balance"))
        ));
        u.counter("n", 1);
        assert!(matches!(
            u.start_challenge(user, "+1").await,
            Err(StoreError::Unavailable("phase7:phone"))
        ));
        assert!(matches!(
            u.verify(user, "000000").await,
            Err(StoreError::Unavailable("phase7:phone"))
        ));
        let mut u = UnavailableMoney;
        let market = crate::model::MarketId(uuid::Uuid::nil());
        assert!(matches!(
            u.finalize_market_fee_allocations(market).await,
            Err(StoreError::Unavailable("phase7:fee-allocation-finalize"))
        ));
        assert!(matches!(
            u.reverse_market_fee_allocations(market).await,
            Err(StoreError::Unavailable("phase7:fee-allocation-reverse"))
        ));
    }

    #[tokio::test]
    async fn unavailable_money_proposals_fail_closed_and_referrals_are_neutral() {
        let mut unavailable = UnavailableMoney;
        let market = crate::model::MarketId(uuid::Uuid::new_v4());
        assert_eq!(
            ReferralPaidFinalize::referral_relevant_users(&mut unavailable, market)
                .await
                .unwrap(),
            Vec::<UserId>::new()
        );
        assert_eq!(
            ReferralPaidFinalize::grant_referrals_on_paid(&mut unavailable, market)
                .await
                .unwrap(),
            0
        );

        let now = OffsetDateTime::UNIX_EPOCH;
        let proposal = crate::money::MoneyProposal {
            id: uuid::Uuid::new_v4(),
            kind: "approve_withdrawal".into(),
            subject_id: uuid::Uuid::new_v4(),
            payload_hash: "payload".into(),
            proposer_token_id: "finance-a".into(),
            confirmer_token_id: None,
            reason: "coverage contract".into(),
            status: crate::money::ProposalStatus::Pending,
            confirm_not_before: now,
            expires_at: now + time::Duration::minutes(15),
            replay_key: "proposal-replay".into(),
            created_at: now,
        };
        assert!(matches!(
            MoneyProposalIo::insert_proposal(&mut unavailable, proposal.clone()).await,
            Err(StoreError::Unavailable("phase7:money-proposal"))
        ));
        assert!(matches!(
            MoneyProposalIo::get_proposal_by_replay(&mut unavailable, &proposal.replay_key).await,
            Err(StoreError::Unavailable("phase7:money-proposal"))
        ));
        assert!(matches!(
            MoneyProposalIo::get_proposal(&mut unavailable, proposal.id).await,
            Err(StoreError::Unavailable("phase7:money-proposal"))
        ));
        assert!(matches!(
            MoneyProposalIo::confirm_proposal(&mut unavailable, proposal.id, "finance-b", now,)
                .await,
            Err(StoreError::Unavailable("phase7:money-proposal"))
        ));
    }
}
