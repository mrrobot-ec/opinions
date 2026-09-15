//! W2 enforcement hooks consumed at W3 call sites (D33/D34).
//!
//! Screening *writers* are W2-owned. These hooks only read persisted facts
//! and fail closed: missing policy, missing Clear, banned, or self-excluded
//! blocks the money mutation.
//!
//! The hook is split in two on purpose. [`read_gate_snapshot`] is the only
//! part that touches W3's transaction — straight-line reads, no policy — and
//! [`decide_money_mutation`] is the policy itself, a pure function over the
//! snapshot. W3's call sites keep calling [`enforce_money_mutation`], which
//! is just the composition of the two.

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::error::{AppError, StoreError};
use crate::model::UserId;
use crate::money::CreditIo;

/// Kind of money mutation being gated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoneyMutation {
    PlaceTrade,
    CastVote,
    DepositAdmit,
    CreditGrant,
}

/// User-facing money status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserMoneyStatus {
    Active,
    ShadowLimited,
    Banned,
}

impl UserMoneyStatus {
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "banned" => Self::Banned,
            "shadow_limited" => Self::ShadowLimited,
            _ => Self::Active,
        }
    }
}

/// The exact facts the fail-closed gate consults, read once under the
/// caller's `lock_user`. The booleans are independent persisted facts, not a
/// state enum in disguise — collapsing them would hide which one refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct MoneyGateSnapshot {
    pub status: UserMoneyStatus,
    pub self_excluded: bool,
    pub kyc_tier: i64,
    pub required_kyc_tier: i64,
    pub geo_clear: bool,
    pub sanctions_clear: bool,
    pub deposit_limit_micro: Option<i64>,
    pub deposits_paused: bool,
    pub shadow_trade_cap_micro: i64,
    pub shadow_deposit_cap_micro: i64,
}

/// The compliance reads the gate needs. `CreditIo` already provides every
/// one of them; naming them here keeps the hook's surface explicit.
#[async_trait]
pub trait MoneyGateReads: Send {
    async fn gate_snapshot(
        &mut self,
        user: UserId,
        now: OffsetDateTime,
    ) -> Result<MoneyGateSnapshot, StoreError>;
}

#[async_trait]
impl MoneyGateReads for dyn CreditIo + '_ {
    async fn gate_snapshot(
        &mut self,
        user: UserId,
        now: OffsetDateTime,
    ) -> Result<MoneyGateSnapshot, StoreError> {
        // Straight line by design: every read runs for every mutation kind so
        // this function carries no policy branch of its own. The reads are all
        // indexed point lookups inside the caller's open transaction.
        let status = UserMoneyStatus::parse(&self.user_status(user).await?);
        let self_excluded = self.self_excluded(user, now).await?;
        let kyc_tier = i64::from(self.user_kyc_tier(user).await?);
        let required_kyc_tier =
            self.config_i64("deposit_kyc_tier")
                .await?
                .ok_or(StoreError::Invariant(
                    "required money config missing or malformed",
                ))?;
        let geo_clear = self.fresh_clear(user, "geo", now).await?;
        let sanctions_clear = self.fresh_clear(user, "sanctions", now).await?;
        let deposit_limit_micro = self.deposit_limit_micro(user).await?;
        let deposits_paused = self.required_config_flag("pause_deposits").await?;
        let shadow_trade_cap_micro = self
            .config_i64("shadow_trade_cap_micro")
            .await?
            .unwrap_or(crate::money::PUBLISHED_TIER0_CAP_MICRO);
        let shadow_deposit_cap_micro = self
            .config_i64("shadow_deposit_cap_micro")
            .await?
            .unwrap_or(crate::money::PUBLISHED_TIER0_CAP_MICRO);
        Ok(MoneyGateSnapshot {
            status,
            self_excluded,
            kyc_tier,
            required_kyc_tier,
            geo_clear,
            sanctions_clear,
            deposit_limit_micro,
            deposits_paused,
            shadow_trade_cap_micro,
            shadow_deposit_cap_micro,
        })
    }
}

/// Read every gate fact for `user` in one pass.
///
/// # Errors
/// Store failures.
pub async fn read_gate_snapshot(
    tx: &mut (dyn CreditIo + '_),
    user: UserId,
    now: OffsetDateTime,
) -> Result<MoneyGateSnapshot, StoreError> {
    tx.gate_snapshot(user, now).await
}

/// The fail-closed policy itself — pure, so every refusal is unit-provable.
///
/// # Errors
/// [`AppError::MoneyForbidden`] / [`AppError::DepositsPaused`] /
/// [`AppError::ComplianceHold`] / [`AppError::PositionCapExceeded`].
pub fn decide_money_mutation(
    snap: &MoneyGateSnapshot,
    kind: MoneyMutation,
    amount_micro: i64,
) -> Result<(), AppError> {
    // A banned user's inbound deposit still books to suspense; only the
    // egress side is frozen (D34 egress split), so admission is evaluated.
    if snap.status == UserMoneyStatus::Banned && kind != MoneyMutation::DepositAdmit {
        return Err(AppError::MoneyForbidden("user banned"));
    }
    if snap.self_excluded {
        return Err(AppError::MoneyForbidden("self-excluded"));
    }
    match kind {
        MoneyMutation::DepositAdmit => {
            if snap.deposits_paused {
                return Err(AppError::DepositsPaused);
            }
            if snap.kyc_tier < snap.required_kyc_tier {
                return Err(AppError::ComplianceHold { reason: "kyc" });
            }
            if !snap.geo_clear {
                return Err(AppError::ComplianceHold { reason: "geo" });
            }
            if !snap.sanctions_clear {
                return Err(AppError::ComplianceHold {
                    reason: "sanctions",
                });
            }
            if snap
                .deposit_limit_micro
                .is_some_and(|limit| amount_micro > limit)
            {
                return Err(AppError::ComplianceHold {
                    reason: "deposit_limit",
                });
            }
            if snap.status == UserMoneyStatus::ShadowLimited
                && amount_micro > snap.shadow_deposit_cap_micro
            {
                return Err(shadow_refusal(snap.shadow_deposit_cap_micro));
            }
        }
        MoneyMutation::PlaceTrade => {
            if snap.kyc_tier < snap.required_kyc_tier {
                return Err(AppError::MoneyForbidden("kyc"));
            }
            if !snap.geo_clear {
                return Err(AppError::MoneyForbidden("geo"));
            }
            if !snap.sanctions_clear {
                return Err(AppError::MoneyForbidden("sanctions"));
            }
            if snap.status == UserMoneyStatus::ShadowLimited
                && amount_micro > snap.shadow_trade_cap_micro
            {
                return Err(shadow_refusal(snap.shadow_trade_cap_micro));
            }
        }
        MoneyMutation::CastVote => {
            if !snap.geo_clear {
                return Err(AppError::MoneyForbidden("geo"));
            }
        }
        MoneyMutation::CreditGrant => {
            if snap.status != UserMoneyStatus::Active {
                return Err(AppError::MoneyForbidden("status"));
            }
        }
    }
    Ok(())
}

/// Shadow refusals are indistinguishable from the published tier-0 cap
/// refusal: same code, same shape, no `ShadowLimited` vocabulary (D34).
fn shadow_refusal(cap_micro: i64) -> AppError {
    let (cap_micro, tier) = crate::money::homogeneous_cap_exceeded(cap_micro);
    AppError::PositionCapExceeded { cap_micro, tier }
}

/// Fail-closed checks shared by `PlaceTrade` / `CastVote` / deposit admission.
///
/// # Errors
/// [`AppError::MoneyForbidden`] / [`AppError::DepositsPaused`] / a hold.
pub async fn enforce_money_mutation(
    tx: &mut (dyn CreditIo + '_),
    user: UserId,
    kind: MoneyMutation,
    amount_micro: i64,
    now: OffsetDateTime,
) -> Result<(), AppError> {
    let snap = read_gate_snapshot(tx, user, now).await?;
    decide_money_mutation(&snap, kind, amount_micro)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::fakes::InMemoryStore;
    use crate::money::PUBLISHED_TIER0_CAP_MICRO;
    use crate::ports::Store;

    fn clean() -> MoneyGateSnapshot {
        MoneyGateSnapshot {
            status: UserMoneyStatus::Active,
            self_excluded: false,
            kyc_tier: 2,
            required_kyc_tier: 1,
            geo_clear: true,
            sanctions_clear: true,
            deposit_limit_micro: None,
            deposits_paused: false,
            shadow_trade_cap_micro: PUBLISHED_TIER0_CAP_MICRO,
            shadow_deposit_cap_micro: PUBLISHED_TIER0_CAP_MICRO,
        }
    }

    #[tokio::test]
    async fn gate_snapshot_rejects_a_missing_required_deposit_kyc_tier() {
        let store = InMemoryStore::new();
        store.remove_money_i64("deposit_kyc_tier");
        let user = UserId(uuid::Uuid::new_v4());
        let mut tx = store.deposit_admission_tx().await.unwrap();
        tx.lock_user(user).await.unwrap();

        assert_eq!(
            read_gate_snapshot(tx.as_mut(), user, OffsetDateTime::UNIX_EPOCH).await,
            Err(StoreError::Invariant(
                "required money config missing or malformed"
            ))
        );
    }

    #[tokio::test]
    async fn gate_snapshot_rejects_a_missing_required_deposit_pause_flag() {
        let store = InMemoryStore::new();
        store.remove_money_flag("pause_deposits");
        let user = UserId(uuid::Uuid::new_v4());
        let mut tx = store.deposit_admission_tx().await.unwrap();
        tx.lock_user(user).await.unwrap();

        assert_eq!(
            read_gate_snapshot(tx.as_mut(), user, OffsetDateTime::UNIX_EPOCH).await,
            Err(StoreError::Invariant(
                "required money config missing or malformed"
            ))
        );
    }

    const KINDS: [MoneyMutation; 4] = [
        MoneyMutation::PlaceTrade,
        MoneyMutation::CastVote,
        MoneyMutation::DepositAdmit,
        MoneyMutation::CreditGrant,
    ];

    #[test]
    fn status_parse_is_fail_closed_to_active_only_for_unknown() {
        assert_eq!(UserMoneyStatus::parse("banned"), UserMoneyStatus::Banned);
        assert_eq!(
            UserMoneyStatus::parse("shadow_limited"),
            UserMoneyStatus::ShadowLimited
        );
        assert_eq!(UserMoneyStatus::parse("active"), UserMoneyStatus::Active);
        assert_eq!(UserMoneyStatus::parse("mystery"), UserMoneyStatus::Active);
    }

    #[test]
    fn a_clean_snapshot_passes_every_mutation_kind() {
        for kind in KINDS {
            assert!(decide_money_mutation(&clean(), kind, 1_000_000).is_ok());
        }
    }

    #[test]
    fn ban_blocks_everything_except_the_inbound_deposit_leg() {
        let snap = MoneyGateSnapshot {
            status: UserMoneyStatus::Banned,
            ..clean()
        };
        for kind in [
            MoneyMutation::PlaceTrade,
            MoneyMutation::CastVote,
            MoneyMutation::CreditGrant,
        ] {
            assert!(matches!(
                decide_money_mutation(&snap, kind, 1),
                Err(AppError::MoneyForbidden("user banned"))
            ));
        }
        assert!(decide_money_mutation(&snap, MoneyMutation::DepositAdmit, 1).is_ok());
    }

    #[test]
    fn self_exclusion_blocks_every_kind_including_admission() {
        let snap = MoneyGateSnapshot {
            self_excluded: true,
            ..clean()
        };
        for kind in KINDS {
            assert!(matches!(
                decide_money_mutation(&snap, kind, 1),
                Err(AppError::MoneyForbidden("self-excluded"))
            ));
        }
    }

    #[test]
    fn deposit_admission_holds_on_every_missing_fact() {
        assert!(matches!(
            decide_money_mutation(
                &MoneyGateSnapshot {
                    deposits_paused: true,
                    ..clean()
                },
                MoneyMutation::DepositAdmit,
                1
            ),
            Err(AppError::DepositsPaused)
        ));
        assert!(matches!(
            decide_money_mutation(
                &MoneyGateSnapshot {
                    kyc_tier: 0,
                    ..clean()
                },
                MoneyMutation::DepositAdmit,
                1
            ),
            Err(AppError::ComplianceHold { reason: "kyc" })
        ));
        assert!(matches!(
            decide_money_mutation(
                &MoneyGateSnapshot {
                    geo_clear: false,
                    ..clean()
                },
                MoneyMutation::DepositAdmit,
                1
            ),
            Err(AppError::ComplianceHold { reason: "geo" })
        ));
        assert!(matches!(
            decide_money_mutation(
                &MoneyGateSnapshot {
                    sanctions_clear: false,
                    ..clean()
                },
                MoneyMutation::DepositAdmit,
                1
            ),
            Err(AppError::ComplianceHold {
                reason: "sanctions"
            })
        ));
        let limited = MoneyGateSnapshot {
            deposit_limit_micro: Some(10_000_000),
            ..clean()
        };
        assert!(decide_money_mutation(&limited, MoneyMutation::DepositAdmit, 10_000_000).is_ok());
        assert!(matches!(
            decide_money_mutation(&limited, MoneyMutation::DepositAdmit, 10_000_001),
            Err(AppError::ComplianceHold {
                reason: "deposit_limit"
            })
        ));
    }

    #[test]
    fn place_trade_refuses_on_every_missing_fact() {
        for (snap, expected) in [
            (
                MoneyGateSnapshot {
                    kyc_tier: 0,
                    ..clean()
                },
                "kyc",
            ),
            (
                MoneyGateSnapshot {
                    geo_clear: false,
                    ..clean()
                },
                "geo",
            ),
            (
                MoneyGateSnapshot {
                    sanctions_clear: false,
                    ..clean()
                },
                "sanctions",
            ),
        ] {
            let refusal = decide_money_mutation(&snap, MoneyMutation::PlaceTrade, 1).unwrap_err();
            assert!(
                matches!(refusal, AppError::MoneyForbidden(reason) if reason == expected),
                "expected {expected}"
            );
        }
    }

    #[test]
    fn shadow_caps_are_indistinguishable_from_the_published_tier_zero_refusal() {
        let snap = MoneyGateSnapshot {
            status: UserMoneyStatus::ShadowLimited,
            shadow_trade_cap_micro: PUBLISHED_TIER0_CAP_MICRO,
            shadow_deposit_cap_micro: PUBLISHED_TIER0_CAP_MICRO,
            ..clean()
        };
        assert!(
            decide_money_mutation(&snap, MoneyMutation::PlaceTrade, PUBLISHED_TIER0_CAP_MICRO)
                .is_ok()
        );
        for kind in [MoneyMutation::PlaceTrade, MoneyMutation::DepositAdmit] {
            let refusal =
                decide_money_mutation(&snap, kind, PUBLISHED_TIER0_CAP_MICRO + 1).unwrap_err();
            assert!(matches!(
                refusal,
                AppError::PositionCapExceeded {
                    cap_micro: PUBLISHED_TIER0_CAP_MICRO,
                    tier: 0
                }
            ));
        }
        // A shadow-limited user still votes and still passes CreditGrant's
        // status gate only when active.
        assert!(decide_money_mutation(&snap, MoneyMutation::CastVote, 0).is_ok());
        assert!(matches!(
            decide_money_mutation(&snap, MoneyMutation::CreditGrant, 0),
            Err(AppError::MoneyForbidden("status"))
        ));
    }

    #[test]
    fn cast_vote_only_needs_a_fresh_geo_clear() {
        let snap = MoneyGateSnapshot {
            kyc_tier: 0,
            sanctions_clear: false,
            ..clean()
        };
        assert!(decide_money_mutation(&snap, MoneyMutation::CastVote, 0).is_ok());
        assert!(matches!(
            decide_money_mutation(
                &MoneyGateSnapshot {
                    geo_clear: false,
                    ..clean()
                },
                MoneyMutation::CastVote,
                0
            ),
            Err(AppError::MoneyForbidden("geo"))
        ));
    }
}
