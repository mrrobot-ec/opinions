//! D36 `fee_bps_override:{market}` write/revert via `money_command_proposals`
//! (finance + 2P, 5 min delay, Δ≤20bp/5min). The confirm writes a generation
//! bump and a `MarketFeeChanged` outbox/WS frame in the same transaction.

use async_trait::async_trait;
use serde_json::{json, Value};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::error::{AppError, StoreError};
use crate::model::{AdminContext, AdminRole, Event, MarketId};

pub const FEE_OVERRIDE_KIND: &str = "fee_bps_override";
pub const FEE_OVERRIDE_DELAY_SECS: i64 = 300;
pub const FEE_OVERRIDE_WINDOW_SECS: i64 = 900;
pub const FEE_OVERRIDE_MAX_DELTA_BPS: i64 = 20;
pub const MARKET_FEE_CHANGED: &str = "MarketFeeChanged";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeeOverrideValue {
    Inherit,
    Override { bps: u16 },
}

impl FeeOverrideValue {
    #[must_use]
    pub fn as_json(&self) -> Value {
        match self {
            Self::Inherit => json!("inherit"),
            Self::Override { bps } => json!({"override": bps}),
        }
    }

    /// # Errors
    /// Unknown shape.
    pub fn from_json(value: &Value) -> Result<Self, AppError> {
        if value.as_str() == Some("inherit") {
            return Ok(Self::Inherit);
        }
        if let Some(bps) = value.get("override").and_then(Value::as_i64) {
            let bps = u16::try_from(bps).map_err(|_| AppError::ConfigInvalid {
                key: "fee_bps_override".into(),
                reason: "override bps out of u16",
            })?;
            return Ok(Self::Override { bps });
        }
        if let Some(bps) = value.as_i64() {
            let bps = u16::try_from(bps).map_err(|_| AppError::ConfigInvalid {
                key: "fee_bps_override".into(),
                reason: "override bps out of u16",
            })?;
            return Ok(Self::Override { bps });
        }
        Err(AppError::ConfigInvalid {
            key: "fee_bps_override".into(),
            reason: "fee override must be inherit or override(bps)",
        })
    }

    #[must_use]
    pub fn bps_or_pool(&self, pool_stamp_bps: u16) -> u16 {
        match self {
            Self::Inherit => pool_stamp_bps,
            Self::Override { bps } => *bps,
        }
    }
}

#[must_use]
pub fn override_key(market: MarketId) -> String {
    format!("fee_bps_override:{}", market.0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeeOverrideProposal {
    pub id: Uuid,
    pub market: MarketId,
    pub value: FeeOverrideValue,
    pub proposer_token_id: String,
    pub confirmer_token_id: Option<String>,
    pub reason: String,
    pub replay_key: String,
    pub status: FeeOverrideStatus,
    pub confirm_not_before: OffsetDateTime,
    pub expires_at: OffsetDateTime,
    pub pool_stamp_bps: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeeOverrideStatus {
    Pending,
    Confirmed,
    Rejected,
    Expired,
}

#[async_trait]
pub trait FeeOverrideIo: Send {
    async fn load_pool_stamp(&mut self, market: MarketId) -> Result<u16, StoreError>;
    async fn load_current_override(
        &mut self,
        market: MarketId,
    ) -> Result<FeeOverrideValue, StoreError>;
    async fn insert_proposal(&mut self, proposal: &FeeOverrideProposal) -> Result<(), StoreError>;
    async fn proposal_for_update(&mut self, id: Uuid) -> Result<FeeOverrideProposal, StoreError>;
    async fn confirm_write(
        &mut self,
        proposal: &FeeOverrideProposal,
        generation: i64,
        event: Event,
    ) -> Result<i64, StoreError>;
    async fn next_generation(&mut self) -> Result<i64, StoreError>;
}

#[derive(Debug, Clone)]
pub struct ProposeFeeOverride {
    pub actor: AdminContext,
    pub market: MarketId,
    pub value: FeeOverrideValue,
    pub reason: String,
    pub replay_key: String,
}

pub struct ProposeFeeOverrideOp<'a, I> {
    pub io: &'a mut I,
    pub now: OffsetDateTime,
}

impl<I: FeeOverrideIo> ProposeFeeOverrideOp<'_, I> {
    /// # Errors
    /// Role, bounds, delta, or store failures.
    pub async fn execute(
        &mut self,
        cmd: ProposeFeeOverride,
    ) -> Result<FeeOverrideProposal, AppError> {
        let AdminContext::Admin { token_digest, role } = &cmd.actor else {
            return Err(AppError::AdminForbidden(
                "fee override requires an admin actor",
            ));
        };
        if !matches!(role, AdminRole::Finance | AdminRole::Superadmin) {
            return Err(AppError::AdminForbidden(
                "fee override proposer must be finance",
            ));
        }
        if cmd.reason.trim().is_empty() {
            return Err(AppError::ConfigInvalid {
                key: override_key(cmd.market),
                reason: "reason is mandatory",
            });
        }
        let pool = self.io.load_pool_stamp(cmd.market).await?;
        let current = self.io.load_current_override(cmd.market).await?;
        let baseline = current.bps_or_pool(pool);
        let next = cmd.value.bps_or_pool(pool);
        if !(10..=200).contains(&next) && !matches!(cmd.value, FeeOverrideValue::Inherit) {
            return Err(AppError::ConfigInvalid {
                key: override_key(cmd.market),
                reason: "override bps must be in [10, 200]",
            });
        }
        if matches!(cmd.value, FeeOverrideValue::Override { .. })
            && i64::from(next).abs_diff(i64::from(baseline)) > FEE_OVERRIDE_MAX_DELTA_BPS as u64
        {
            return Err(AppError::ConfigInvalid {
                key: override_key(cmd.market),
                reason: "delta exceeds 20bp vs pool stamp or last override",
            });
        }
        let confirm_not_before = self.now + Duration::seconds(FEE_OVERRIDE_DELAY_SECS);
        let proposal = FeeOverrideProposal {
            id: Uuid::new_v4(),
            market: cmd.market,
            value: cmd.value,
            proposer_token_id: token_digest.clone(),
            confirmer_token_id: None,
            reason: cmd.reason,
            replay_key: cmd.replay_key,
            status: FeeOverrideStatus::Pending,
            confirm_not_before,
            expires_at: confirm_not_before + Duration::seconds(FEE_OVERRIDE_WINDOW_SECS),
            pool_stamp_bps: pool,
        };
        self.io.insert_proposal(&proposal).await?;
        Ok(proposal)
    }
}

#[derive(Debug, Clone)]
pub struct ConfirmFeeOverride {
    pub actor: AdminContext,
    pub id: Uuid,
}

pub struct ConfirmFeeOverrideOp<'a, I> {
    pub io: &'a mut I,
    pub now: OffsetDateTime,
}

impl<I: FeeOverrideIo> ConfirmFeeOverrideOp<'_, I> {
    /// # Errors
    /// Role, same-token, window, or store failures.
    pub async fn execute(
        &mut self,
        cmd: ConfirmFeeOverride,
    ) -> Result<(FeeOverrideProposal, i64), AppError> {
        let AdminContext::Admin { token_digest, role } = &cmd.actor else {
            return Err(AppError::AdminForbidden(
                "fee override confirm requires an admin actor",
            ));
        };
        if !matches!(role, AdminRole::Finance | AdminRole::Superadmin) {
            return Err(AppError::AdminForbidden(
                "fee override confirmer must be finance or superadmin",
            ));
        }
        let mut proposal = self.io.proposal_for_update(cmd.id).await?;
        if proposal.status != FeeOverrideStatus::Pending {
            return Err(AppError::ProposalConflict("fee override is not pending"));
        }
        if proposal.proposer_token_id == *token_digest {
            return Err(AppError::AdminForbidden("confirmer token must be distinct"));
        }
        if self.now < proposal.confirm_not_before {
            return Err(AppError::ProposalConflict(
                "confirm_not_before has not elapsed",
            ));
        }
        if self.now >= proposal.expires_at {
            proposal.status = FeeOverrideStatus::Expired;
            return Err(AppError::ProposalConflict("fee override proposal expired"));
        }
        proposal.confirmer_token_id = Some(token_digest.clone());
        proposal.status = FeeOverrideStatus::Confirmed;
        let generation = self.io.next_generation().await?;
        let event = Event {
            event_type: MARKET_FEE_CHANGED,
            aggregate_type: "market",
            aggregate_id: proposal.market.0,
            payload: json!({
                "market_id": proposal.market.0,
                "override": proposal.value.as_json(),
                "generation": generation,
            }),
        };
        let applied = self.io.confirm_write(&proposal, generation, event).await?;
        Ok((proposal, applied))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::collections::BTreeMap;

    struct FakeIo {
        pool: u16,
        current: FeeOverrideValue,
        proposals: BTreeMap<Uuid, FeeOverrideProposal>,
        generation: i64,
        last_event: Option<Event>,
        last_written: Option<FeeOverrideValue>,
    }

    impl FakeIo {
        fn new() -> Self {
            Self {
                pool: 100,
                current: FeeOverrideValue::Inherit,
                proposals: BTreeMap::new(),
                generation: 1,
                last_event: None,
                last_written: None,
            }
        }
    }

    #[async_trait]
    impl FeeOverrideIo for FakeIo {
        async fn load_pool_stamp(&mut self, _market: MarketId) -> Result<u16, StoreError> {
            Ok(self.pool)
        }
        async fn load_current_override(
            &mut self,
            _market: MarketId,
        ) -> Result<FeeOverrideValue, StoreError> {
            Ok(self.current.clone())
        }
        async fn insert_proposal(
            &mut self,
            proposal: &FeeOverrideProposal,
        ) -> Result<(), StoreError> {
            self.proposals.insert(proposal.id, proposal.clone());
            Ok(())
        }
        async fn proposal_for_update(
            &mut self,
            id: Uuid,
        ) -> Result<FeeOverrideProposal, StoreError> {
            self.proposals
                .get(&id)
                .cloned()
                .ok_or(StoreError::NotFound("fee override proposal"))
        }
        async fn confirm_write(
            &mut self,
            proposal: &FeeOverrideProposal,
            generation: i64,
            event: Event,
        ) -> Result<i64, StoreError> {
            self.last_event = Some(event);
            self.last_written = Some(proposal.value.clone());
            self.current = proposal.value.clone();
            self.proposals.insert(proposal.id, proposal.clone());
            Ok(generation)
        }
        async fn next_generation(&mut self) -> Result<i64, StoreError> {
            self.generation += 1;
            Ok(self.generation)
        }
    }

    fn finance(id: &str) -> AdminContext {
        AdminContext::Admin {
            token_digest: id.into(),
            role: AdminRole::Finance,
        }
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    #[tokio::test]
    async fn propose_and_confirm_writes_generation_and_frame() {
        let mut io = FakeIo::new();
        let market = MarketId(Uuid::from_u128(1));
        let proposed = ProposeFeeOverrideOp {
            io: &mut io,
            now: now(),
        }
        .execute(ProposeFeeOverride {
            actor: finance("a"),
            market,
            value: FeeOverrideValue::Override { bps: 110 },
            reason: "reprice".into(),
            replay_key: "p1".into(),
        })
        .await
        .unwrap();
        assert_eq!(proposed.status, FeeOverrideStatus::Pending);
        assert_eq!(
            proposed.expires_at - proposed.confirm_not_before,
            Duration::seconds(FEE_OVERRIDE_WINDOW_SECS)
        );

        let too_soon = ConfirmFeeOverrideOp {
            io: &mut io,
            now: now(),
        }
        .execute(ConfirmFeeOverride {
            actor: finance("b"),
            id: proposed.id,
        })
        .await;
        assert!(matches!(too_soon, Err(AppError::ProposalConflict(_))));

        let same = ConfirmFeeOverrideOp {
            io: &mut io,
            now: now() + Duration::seconds(FEE_OVERRIDE_DELAY_SECS),
        }
        .execute(ConfirmFeeOverride {
            actor: finance("a"),
            id: proposed.id,
        })
        .await;
        assert!(matches!(same, Err(AppError::AdminForbidden(_))));

        let (confirmed, generation) = ConfirmFeeOverrideOp {
            io: &mut io,
            now: now() + Duration::seconds(FEE_OVERRIDE_DELAY_SECS),
        }
        .execute(ConfirmFeeOverride {
            actor: finance("b"),
            id: proposed.id,
        })
        .await
        .unwrap();
        assert_eq!(confirmed.status, FeeOverrideStatus::Confirmed);
        assert_eq!(generation, 2);
        assert_eq!(
            io.last_event.as_ref().unwrap().event_type,
            MARKET_FEE_CHANGED
        );
        assert_eq!(
            io.last_written,
            Some(FeeOverrideValue::Override { bps: 110 })
        );
    }

    #[tokio::test]
    async fn revert_is_inherit_and_delta_and_roles_are_enforced() {
        let mut io = FakeIo::new();
        let market = MarketId(Uuid::from_u128(2));
        let huge = ProposeFeeOverrideOp {
            io: &mut io,
            now: now(),
        }
        .execute(ProposeFeeOverride {
            actor: finance("a"),
            market,
            value: FeeOverrideValue::Override { bps: 200 },
            reason: "too far".into(),
            replay_key: "big".into(),
        })
        .await;
        assert!(matches!(huge, Err(AppError::ConfigInvalid { .. })));
        let oob = ProposeFeeOverrideOp {
            io: &mut io,
            now: now(),
        }
        .execute(ProposeFeeOverride {
            actor: finance("a"),
            market,
            value: FeeOverrideValue::Override { bps: 1 },
            reason: "below floor".into(),
            replay_key: "oob".into(),
        })
        .await;
        assert!(matches!(oob, Err(AppError::ConfigInvalid { .. })));

        let ops = AdminContext::Admin {
            token_digest: "ops".into(),
            role: AdminRole::Ops,
        };
        assert!(ProposeFeeOverrideOp {
            io: &mut io,
            now: now(),
        }
        .execute(ProposeFeeOverride {
            actor: ops,
            market,
            value: FeeOverrideValue::Inherit,
            reason: "nope".into(),
            replay_key: "ops".into(),
        })
        .await
        .is_err());
        assert!(ProposeFeeOverrideOp {
            io: &mut io,
            now: now(),
        }
        .execute(ProposeFeeOverride {
            actor: AdminContext::Machine,
            market,
            value: FeeOverrideValue::Inherit,
            reason: "nope".into(),
            replay_key: "m".into(),
        })
        .await
        .is_err());
        assert!(ProposeFeeOverrideOp {
            io: &mut io,
            now: now(),
        }
        .execute(ProposeFeeOverride {
            actor: finance("a"),
            market,
            value: FeeOverrideValue::Inherit,
            reason: "   ".into(),
            replay_key: "blank".into(),
        })
        .await
        .is_err());
    }

    #[tokio::test]
    async fn revert_writes_inherit_and_the_typed_enum_round_trips() {
        let mut io = FakeIo::new();
        let market = MarketId(Uuid::from_u128(2));
        let revert = ProposeFeeOverrideOp {
            io: &mut io,
            now: now(),
        }
        .execute(ProposeFeeOverride {
            actor: finance("a"),
            market,
            value: FeeOverrideValue::Inherit,
            reason: "revert".into(),
            replay_key: "rev".into(),
        })
        .await
        .unwrap();
        assert_eq!(revert.value, FeeOverrideValue::Inherit);
        assert_eq!(revert.value.as_json(), json!("inherit"));
        assert_eq!(
            FeeOverrideValue::from_json(&json!("inherit")).unwrap(),
            FeeOverrideValue::Inherit
        );
        assert_eq!(
            FeeOverrideValue::from_json(&json!({"override": 40})).unwrap(),
            FeeOverrideValue::Override { bps: 40 }
        );
        assert_eq!(
            FeeOverrideValue::from_json(&json!(40)).unwrap(),
            FeeOverrideValue::Override { bps: 40 }
        );
        assert!(FeeOverrideValue::from_json(&json!("nope")).is_err());
        assert!(FeeOverrideValue::from_json(&json!({"override": -1})).is_err());
        assert!(FeeOverrideValue::from_json(&json!(-1)).is_err());
        assert_eq!(FeeOverrideValue::Inherit.bps_or_pool(100), 100);
        assert_eq!(
            override_key(market),
            format!("fee_bps_override:{}", market.0)
        );
    }

    #[tokio::test]
    async fn confirm_rejects_settled_expired_and_wrong_role_principals() {
        let mut io = FakeIo::new();
        let market = MarketId(Uuid::from_u128(2));
        let revert = ProposeFeeOverrideOp {
            io: &mut io,
            now: now(),
        }
        .execute(ProposeFeeOverride {
            actor: finance("a"),
            market,
            value: FeeOverrideValue::Inherit,
            reason: "revert".into(),
            replay_key: "rev".into(),
        })
        .await
        .unwrap();

        io.proposals.get_mut(&revert.id).unwrap().status = FeeOverrideStatus::Confirmed;
        let settled = ConfirmFeeOverrideOp {
            io: &mut io,
            now: now() + Duration::seconds(FEE_OVERRIDE_DELAY_SECS),
        }
        .execute(ConfirmFeeOverride {
            actor: finance("b"),
            id: revert.id,
        })
        .await;
        assert!(matches!(settled, Err(AppError::ProposalConflict(_))));

        let pending = ProposeFeeOverrideOp {
            io: &mut io,
            now: now(),
        }
        .execute(ProposeFeeOverride {
            actor: finance("a"),
            market,
            value: FeeOverrideValue::Override { bps: 110 },
            reason: "late".into(),
            replay_key: "late".into(),
        })
        .await
        .unwrap();
        let expired = ConfirmFeeOverrideOp {
            io: &mut io,
            now: now() + Duration::seconds(FEE_OVERRIDE_DELAY_SECS + FEE_OVERRIDE_WINDOW_SECS + 1),
        }
        .execute(ConfirmFeeOverride {
            actor: finance("b"),
            id: pending.id,
        })
        .await;
        assert!(matches!(expired, Err(AppError::ProposalConflict(_))));

        assert!(ConfirmFeeOverrideOp {
            io: &mut io,
            now: now() + Duration::seconds(FEE_OVERRIDE_DELAY_SECS),
        }
        .execute(ConfirmFeeOverride {
            actor: AdminContext::Machine,
            id: pending.id,
        })
        .await
        .is_err());
        assert!(ConfirmFeeOverrideOp {
            io: &mut io,
            now: now() + Duration::seconds(FEE_OVERRIDE_DELAY_SECS),
        }
        .execute(ConfirmFeeOverride {
            actor: AdminContext::Admin {
                token_digest: "c".into(),
                role: AdminRole::Ops,
            },
            id: pending.id,
        })
        .await
        .is_err());
    }
}
