//! AML at-request evaluation: velocity + structuring band (D34).

use serde_json::{json, Value};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::error::StoreError;
use crate::model::UserId;

use super::ComplianceStore;

/// Seed catalog values (0011 / D24).
pub const SEED_STRUCTURING_FLOOR_MICRO: i64 = 100_000_000; // $100
pub const SEED_STRUCTURING_THRESHOLD_MICRO: i64 = 500_000_000; // $500
pub const SEED_STRUCTURING_N: i64 = 4;
pub const SEED_STRUCTURING_WINDOW_HOURS: i64 = 24;
pub const SEED_DEPOSIT_VELOCITY_MICRO_24H: i64 = 5_000_000_000;
pub const SEED_WITHDRAW_VELOCITY_MICRO_24H: i64 = 5_000_000_000;

/// Pinned e2e amounts.
pub const PINNED_DEPOSIT_DUST_MICRO: i64 = 25_000_000; // $25 — below floor
pub const PINNED_WITHDRAW_BAND_MICRO: i64 = 499_000_000; // $499 — in band

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmlKind {
    Structuring,
    DepositVelocity,
    WithdrawVelocity,
    SanctionsHit,
}

impl AmlKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Structuring => "structuring",
            Self::DepositVelocity => "deposit_velocity",
            Self::WithdrawVelocity => "withdraw_velocity",
            Self::SanctionsHit => "sanctions_hit",
        }
    }

    /// # Errors
    /// Unknown rule label.
    pub fn parse(raw: &str) -> Result<Self, StoreError> {
        match raw {
            "structuring" => Ok(Self::Structuring),
            "deposit_velocity" => Ok(Self::DepositVelocity),
            "withdraw_velocity" => Ok(Self::WithdrawVelocity),
            "sanctions_hit" => Ok(Self::SanctionsHit),
            _ => Err(StoreError::Invariant("unknown aml rule")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmlDirection {
    Deposit,
    Withdrawal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmlLeg {
    pub id: Uuid,
    pub user: UserId,
    pub dest: String,
    pub amount_micro: i64,
    pub at: OffsetDateTime,
    pub direction: AmlDirection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmlFlag {
    pub id: Uuid,
    pub user: UserId,
    pub rule: AmlKind,
    pub window_label: String,
    pub evidence: Value,
    pub open: bool,
    pub at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AmlPolicy {
    pub floor_micro: i64,
    pub threshold_micro: i64,
    pub n: i64,
    pub window: Duration,
    pub deposit_velocity_micro_24h: i64,
    pub withdraw_velocity_micro_24h: i64,
}

impl AmlPolicy {
    #[must_use]
    pub fn seed() -> Self {
        Self {
            floor_micro: SEED_STRUCTURING_FLOOR_MICRO,
            threshold_micro: SEED_STRUCTURING_THRESHOLD_MICRO,
            n: SEED_STRUCTURING_N,
            window: Duration::hours(SEED_STRUCTURING_WINDOW_HOURS),
            deposit_velocity_micro_24h: SEED_DEPOSIT_VELOCITY_MICRO_24H,
            withdraw_velocity_micro_24h: SEED_WITHDRAW_VELOCITY_MICRO_24H,
        }
    }
}

/// Structuring counts only legs in `[floor, threshold)`.
#[must_use]
pub fn in_structuring_band(amount_micro: i64, policy: &AmlPolicy) -> bool {
    amount_micro >= policy.floor_micro && amount_micro < policy.threshold_micro
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmlEvaluation {
    pub structuring_user: i64,
    pub structuring_dest: i64,
    pub deposit_velocity: i64,
    pub withdraw_velocity: i64,
    pub flags: Vec<AmlKind>,
}

/// Evaluate one request against already-collected legs **plus** the
/// candidate leg (at-request).
#[must_use]
pub fn evaluate_aml(existing: &[AmlLeg], candidate: &AmlLeg, policy: &AmlPolicy) -> AmlEvaluation {
    let since = candidate.at - policy.window;
    let mut legs: Vec<&AmlLeg> = existing
        .iter()
        .filter(|leg| leg.at >= since && leg.at <= candidate.at)
        .collect();
    legs.push(candidate);

    let structuring_user = legs
        .iter()
        .filter(|leg| leg.user == candidate.user && in_structuring_band(leg.amount_micro, policy))
        .count();
    let structuring_dest = legs
        .iter()
        .filter(|leg| leg.dest == candidate.dest && in_structuring_band(leg.amount_micro, policy))
        .count();
    let deposit_velocity: i64 = legs
        .iter()
        .filter(|leg| leg.user == candidate.user && matches!(leg.direction, AmlDirection::Deposit))
        .map(|leg| leg.amount_micro)
        .fold(0_i64, i64::saturating_add);
    let withdraw_velocity: i64 = legs
        .iter()
        .filter(|leg| {
            leg.user == candidate.user && matches!(leg.direction, AmlDirection::Withdrawal)
        })
        .map(|leg| leg.amount_micro)
        .fold(0_i64, i64::saturating_add);

    let mut flags = Vec::new();
    if i64::try_from(structuring_user).unwrap_or(i64::MAX) >= policy.n
        || i64::try_from(structuring_dest).unwrap_or(i64::MAX) >= policy.n
    {
        flags.push(AmlKind::Structuring);
    }
    if deposit_velocity >= policy.deposit_velocity_micro_24h {
        flags.push(AmlKind::DepositVelocity);
    }
    if withdraw_velocity >= policy.withdraw_velocity_micro_24h {
        flags.push(AmlKind::WithdrawVelocity);
    }

    AmlEvaluation {
        structuring_user: i64::try_from(structuring_user).unwrap_or(i64::MAX),
        structuring_dest: i64::try_from(structuring_dest).unwrap_or(i64::MAX),
        deposit_velocity,
        withdraw_velocity,
        flags,
    }
}

/// At-request: record the candidate leg, evaluate, persist any new flags.
///
/// # Errors
/// Store failures.
pub async fn evaluate_at_request(
    store: &impl ComplianceStore,
    candidate: AmlLeg,
    policy: AmlPolicy,
) -> Result<AmlEvaluation, StoreError> {
    let mut tx = store.compliance_tx().await?;
    tx.lock_user(candidate.user).await?;
    let since = candidate.at - policy.window;
    let mut legs = tx.list_aml_legs(candidate.user, since).await?;
    let dest_legs = tx.list_dest_aml_legs(&candidate.dest, since).await?;
    for dest_leg in dest_legs {
        if !legs.iter().any(|leg| leg.id == dest_leg.id) {
            legs.push(dest_leg);
        }
    }
    let evaluation = evaluate_aml(&legs, &candidate, &policy);
    tx.record_aml_leg(candidate.clone()).await?;
    for kind in &evaluation.flags {
        let existing = tx.open_aml_flags(candidate.user).await?;
        if existing.iter().any(|flag| flag.rule == *kind && flag.open) {
            continue;
        }
        tx.insert_aml_flag(AmlFlag {
            id: Uuid::new_v4(),
            user: candidate.user,
            rule: *kind,
            window_label: format!("{}h", policy.window.whole_hours()),
            evidence: json!({
                "structuring_user": evaluation.structuring_user,
                "structuring_dest": evaluation.structuring_dest,
                "deposit_velocity": evaluation.deposit_velocity,
                "withdraw_velocity": evaluation.withdraw_velocity,
                "leg_id": candidate.id,
            }),
            open: true,
            at: candidate.at,
        })
        .await?;
    }
    tx.commit().await?;
    Ok(evaluation)
}

/// Whether an open flag blocks auto-approve / send.
#[must_use]
pub fn open_flag_blocks_send(flags: &[AmlFlag]) -> bool {
    flags.iter().any(|flag| flag.open)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::fakes::FakeComplianceStore;

    fn t0() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(50_000)
    }

    fn withdraw(user: UserId, dest: &str, amount: i64, n: u8) -> AmlLeg {
        AmlLeg {
            id: Uuid::from_u128(u128::from(n)),
            user,
            dest: dest.into(),
            amount_micro: amount,
            at: t0() + Duration::minutes(i64::from(n)),
            direction: AmlDirection::Withdrawal,
        }
    }

    fn deposit(user: UserId, dest: &str, amount: i64, n: u8) -> AmlLeg {
        AmlLeg {
            id: Uuid::from_u128(100 + u128::from(n)),
            user,
            dest: dest.into(),
            amount_micro: amount,
            at: t0() + Duration::minutes(i64::from(n)),
            direction: AmlDirection::Deposit,
        }
    }

    #[test]
    fn band_excludes_floor_dust_and_includes_threshold_open() {
        let p = AmlPolicy::seed();
        assert!(!in_structuring_band(PINNED_DEPOSIT_DUST_MICRO, &p));
        assert!(in_structuring_band(PINNED_WITHDRAW_BAND_MICRO, &p));
        assert!(in_structuring_band(p.floor_micro, &p));
        assert!(!in_structuring_band(p.threshold_micro, &p));
        assert!(!in_structuring_band(p.floor_micro - 1, &p));
        assert_eq!(AmlKind::Structuring.as_str(), "structuring");
        assert_eq!(AmlKind::DepositVelocity.as_str(), "deposit_velocity");
        assert_eq!(AmlKind::WithdrawVelocity.as_str(), "withdraw_velocity");
        assert_eq!(AmlKind::SanctionsHit.as_str(), "sanctions_hit");
        assert_eq!(AmlKind::parse("structuring").unwrap(), AmlKind::Structuring);
        assert_eq!(
            AmlKind::parse("deposit_velocity").unwrap(),
            AmlKind::DepositVelocity
        );
        assert_eq!(
            AmlKind::parse("withdraw_velocity").unwrap(),
            AmlKind::WithdrawVelocity
        );
        assert_eq!(
            AmlKind::parse("sanctions_hit").unwrap(),
            AmlKind::SanctionsHit
        );
        assert!(AmlKind::parse("nope").is_err());
    }

    #[test]
    fn pinned_structuring_series() {
        let user = UserId(Uuid::from_u128(7));
        let p = AmlPolicy::seed();

        // Four $25 deposits do not flag.
        let d1 = deposit(user, "src", PINNED_DEPOSIT_DUST_MICRO, 1);
        let d2 = deposit(user, "src", PINNED_DEPOSIT_DUST_MICRO, 2);
        let d3 = deposit(user, "src", PINNED_DEPOSIT_DUST_MICRO, 3);
        let d4 = deposit(user, "src", PINNED_DEPOSIT_DUST_MICRO, 4);
        let dust = evaluate_aml(&[d1.clone(), d2.clone(), d3.clone()], &d4, &p);
        assert_eq!(dust.structuring_user, 0);
        assert!(dust.flags.is_empty());

        // Three $499 withdrawals do not flag.
        let w1 = withdraw(user, "dest-a", PINNED_WITHDRAW_BAND_MICRO, 1);
        let w2 = withdraw(user, "dest-a", PINNED_WITHDRAW_BAND_MICRO, 2);
        let w3 = withdraw(user, "dest-a", PINNED_WITHDRAW_BAND_MICRO, 3);
        let three = evaluate_aml(&[w1.clone(), w2.clone()], &w3, &p);
        assert_eq!(three.structuring_user, 3);
        assert!(!three.flags.contains(&AmlKind::Structuring));

        // Four $499 withdrawals flag.
        let w4 = withdraw(user, "dest-a", PINNED_WITHDRAW_BAND_MICRO, 4);
        let four = evaluate_aml(&[w1, w2, w3], &w4, &p);
        assert_eq!(four.structuring_user, 4);
        assert!(four.flags.contains(&AmlKind::Structuring));
    }

    #[test]
    fn per_dest_and_velocity_flags() {
        let a = UserId(Uuid::from_u128(1));
        let b = UserId(Uuid::from_u128(2));
        let p = AmlPolicy::seed();
        let legs = [
            withdraw(a, "mule", PINNED_WITHDRAW_BAND_MICRO, 1),
            withdraw(b, "mule", PINNED_WITHDRAW_BAND_MICRO, 2),
            withdraw(a, "mule", PINNED_WITHDRAW_BAND_MICRO, 3),
        ];
        let fourth = withdraw(b, "mule", PINNED_WITHDRAW_BAND_MICRO, 4);
        let eval = evaluate_aml(&legs, &fourth, &p);
        assert_eq!(eval.structuring_dest, 4);
        assert!(eval.flags.contains(&AmlKind::Structuring));

        let mut hot = p;
        hot.withdraw_velocity_micro_24h = PINNED_WITHDRAW_BAND_MICRO;
        let one = withdraw(a, "solo", PINNED_WITHDRAW_BAND_MICRO, 9);
        let vel = evaluate_aml(&[], &one, &hot);
        assert!(vel.flags.contains(&AmlKind::WithdrawVelocity));

        let mut dep = p;
        dep.deposit_velocity_micro_24h = PINNED_DEPOSIT_DUST_MICRO;
        let d = deposit(a, "s", PINNED_DEPOSIT_DUST_MICRO, 9);
        let dv = evaluate_aml(&[], &d, &dep);
        assert!(dv.flags.contains(&AmlKind::DepositVelocity));
    }

    #[test]
    fn velocity_totals_saturate_and_flag_when_history_exceeds_i64() {
        let user = UserId(Uuid::from_u128(3));
        let policy = AmlPolicy::seed();

        let prior_deposit = deposit(user, "source", i64::MAX, 1);
        let next_deposit = deposit(user, "source", 1, 2);
        let deposit_eval = evaluate_aml(&[prior_deposit], &next_deposit, &policy);
        assert_eq!(deposit_eval.deposit_velocity, i64::MAX);
        assert!(deposit_eval.flags.contains(&AmlKind::DepositVelocity));

        let prior_withdrawal = withdraw(user, "dest", i64::MAX, 3);
        let next_withdrawal = withdraw(user, "dest", 1, 4);
        let withdrawal_eval = evaluate_aml(&[prior_withdrawal], &next_withdrawal, &policy);
        assert_eq!(withdrawal_eval.withdraw_velocity, i64::MAX);
        assert!(withdrawal_eval.flags.contains(&AmlKind::WithdrawVelocity));
    }

    #[tokio::test]
    async fn at_request_persists_flag_once_and_blocks_send() {
        let store = FakeComplianceStore::new();
        let user = store.add_user("aml-e2e");
        let mule = store.add_user("aml-mule");
        let p = AmlPolicy::seed();
        // A leg another user already sent to the same dest is unioned in by
        // the per-dest read, so per-dest structuring sees both users.
        evaluate_at_request(
            &store,
            withdraw(mule, "dest-a", PINNED_WITHDRAW_BAND_MICRO, 0),
            p,
        )
        .await
        .unwrap();
        let mut last = None;
        for n in 1..=4 {
            let eval = evaluate_at_request(
                &store,
                withdraw(user, "dest-a", PINNED_WITHDRAW_BAND_MICRO, n),
                p,
            )
            .await
            .unwrap();
            last = Some(eval);
        }
        let last = last.unwrap();
        assert!(last.flags.contains(&AmlKind::Structuring));
        // Replay of a fifth evaluation should not insert a second open flag.
        evaluate_at_request(
            &store,
            withdraw(user, "dest-a", PINNED_WITHDRAW_BAND_MICRO, 5),
            p,
        )
        .await
        .unwrap();
        let mut tx = store.compliance_tx().await.unwrap();
        let flags = tx.open_aml_flags(user).await.unwrap();
        assert_eq!(flags.len(), 1);
        assert!(open_flag_blocks_send(&flags));
        assert!(!open_flag_blocks_send(&[]));
        tx.commit().await.unwrap();
    }
}
