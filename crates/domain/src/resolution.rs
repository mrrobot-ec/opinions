use crate::amm::Side;
use crate::ledger::AccountId;
use crate::money::{MicroShares, MicroUsd};
use thiserror::Error;

const MICRO_PER_WHOLE: i128 = 1_000_000;

/// Complete settlement output, with one floor-rounded payout per holding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Settlement {
    pub payouts: Vec<(AccountId, MicroUsd)>,
    pub dust: MicroUsd,
}

/// A rejected redemption or settlement computation.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ResolutionError {
    #[error("arithmetic overflow")]
    Overflow,
    #[error("settlement did not conserve escrow: expected {expected:?}, got {got:?}")]
    ConservationViolated { expected: MicroUsd, got: MicroUsd },
    #[error("actual YES result must be at most 10,000 basis points")]
    ActualOutOfRange,
    #[error("holdings are incomplete: minted {minted:?}, presented {presented:?}")]
    HoldingsIncomplete {
        minted: MicroShares,
        presented: MicroShares,
    },
}

/// Returns the YES and NO redemption values per whole share.
///
/// # Errors
///
/// Returns [`ResolutionError::ActualOutOfRange`] when `actual_yes_bps` is
/// greater than 10,000.
pub fn redemptions(actual_yes_bps: u16) -> Result<(MicroUsd, MicroUsd), ResolutionError> {
    if actual_yes_bps > 10_000 {
        return Err(ResolutionError::ActualOutOfRange);
    }

    let yes = i64::from(actual_yes_bps) * 100;
    Ok((MicroUsd(yes), MicroUsd(1_000_000 - yes)))
}

/// Computes one holding's payout, floor-rounded in favor of the house.
///
/// # Errors
///
/// Returns [`ResolutionError::Overflow`] for negative inputs or when the
/// resulting payout cannot fit an `i64`.
pub fn position_payout(
    shares: MicroShares,
    redemption_per_share: MicroUsd,
) -> Result<MicroUsd, ResolutionError> {
    if shares.0 < 0 || redemption_per_share.0 < 0 {
        return Err(ResolutionError::Overflow);
    }

    let payout = i128::from(shares.0) * i128::from(redemption_per_share.0) / MICRO_PER_WHOLE;
    i64::try_from(payout)
        .map(MicroUsd)
        .map_err(|_| ResolutionError::Overflow)
}

/// Settles every outstanding YES and NO balance against fully funded escrow.
///
/// Numerically, one micro-share of each side is one micro-USD of complete-set
/// escrow, so `escrow.0` is also the number of minted micro-share sets. Both
/// side totals must equal that number; this makes omitted pool/house inventory
/// a hard error instead of silently treating it as dust.
///
/// # Errors
///
/// Returns [`ResolutionError::ActualOutOfRange`] for an invalid actual result,
/// [`ResolutionError::HoldingsIncomplete`] unless both side totals equal the
/// minted sets, [`ResolutionError::Overflow`] for negative or unrepresentable
/// values, or [`ResolutionError::ConservationViolated`] unless payouts plus
/// dust equal escrow with `0 <= dust < holdings.len()`.
pub fn settle_market(
    holdings: &[(AccountId, Side, MicroShares)],
    actual_yes_bps: u16,
    escrow: MicroUsd,
) -> Result<Settlement, ResolutionError> {
    let (yes_redemption, no_redemption) = redemptions(actual_yes_bps)?;
    if escrow.0 < 0 {
        return Err(ResolutionError::Overflow);
    }

    let minted = MicroShares(escrow.0);
    let mut yes_presented = 0_i128;
    let mut no_presented = 0_i128;
    for &(_, side, shares) in holdings {
        if shares.0 < 0 {
            return Err(ResolutionError::Overflow);
        }
        let side_total = match side {
            Side::Yes => &mut yes_presented,
            Side::No => &mut no_presented,
        };
        *side_total = side_total
            .checked_add(i128::from(shares.0))
            .ok_or(ResolutionError::Overflow)?;
    }

    let minted_i128 = i128::from(minted.0);
    if yes_presented != minted_i128 {
        let presented = i64::try_from(yes_presented).map_err(|_| ResolutionError::Overflow)?;
        return Err(ResolutionError::HoldingsIncomplete {
            minted,
            presented: MicroShares(presented),
        });
    }
    if no_presented != minted_i128 {
        let presented = i64::try_from(no_presented).map_err(|_| ResolutionError::Overflow)?;
        return Err(ResolutionError::HoldingsIncomplete {
            minted,
            presented: MicroShares(presented),
        });
    }

    let mut payouts = Vec::with_capacity(holdings.len());
    let mut payout_sum = 0_i128;
    for &(account, side, shares) in holdings {
        let redemption = match side {
            Side::Yes => yes_redemption,
            Side::No => no_redemption,
        };
        let payout = position_payout(shares, redemption)?;
        payout_sum = payout_sum
            .checked_add(i128::from(payout.0))
            .ok_or(ResolutionError::Overflow)?;
        payouts.push((account, payout));
    }

    let escrow_i128 = i128::from(escrow.0);
    // Complete, non-negative YES/NO holdings and complementary redemptions
    // prove payout_sum <= escrow; dust is the accumulated per-holding floor.
    let dust_i128 = escrow_i128 - payout_sum;
    let dust_i64 = i64::try_from(dust_i128).map_err(|_| ResolutionError::Overflow)?;
    let dust = MicroUsd(dust_i64);
    let recombined = payout_sum
        .checked_add(dust_i128)
        .ok_or(ResolutionError::Overflow)?;
    let holding_count = i128::try_from(holdings.len()).map_err(|_| ResolutionError::Overflow)?;
    if recombined != escrow_i128 || dust_i128 >= holding_count {
        let got = i64::try_from(recombined).map_err(|_| ResolutionError::Overflow)?;
        return Err(ResolutionError::ConservationViolated {
            expected: escrow,
            got: MicroUsd(got),
        });
    }

    Ok(Settlement { payouts, dust })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use proptest::prelude::*;
    use uuid::Uuid;

    fn account() -> AccountId {
        AccountId(Uuid::new_v4())
    }

    #[test]
    fn redemption_extremes_and_midpoint_sum_to_one_dollar() {
        for (actual, expected) in [
            (0, (MicroUsd(0), MicroUsd(1_000_000))),
            (5_000, (MicroUsd(500_000), MicroUsd(500_000))),
            (10_000, (MicroUsd(1_000_000), MicroUsd(0))),
        ] {
            let redemption = redemptions(actual).unwrap();
            assert_eq!(redemption, expected);
            assert_eq!(redemption.0 .0 + redemption.1 .0, 1_000_000);
        }
    }

    #[test]
    fn redemption_rejects_out_of_range_actual() {
        assert_eq!(redemptions(10_001), Err(ResolutionError::ActualOutOfRange));
    }

    #[test]
    fn one_whole_yes_share_at_seventy_three_point_five_percent() {
        assert_eq!(
            position_payout(MicroShares(1_000_000), MicroUsd(735_000)).unwrap(),
            MicroUsd(735_000)
        );
    }

    #[test]
    fn payout_overflow_is_rejected() {
        assert_eq!(
            position_payout(MicroShares(i64::MAX), MicroUsd(i64::MAX)),
            Err(ResolutionError::Overflow)
        );
        assert_eq!(
            position_payout(MicroShares(-1), MicroUsd(500_000)),
            Err(ResolutionError::Overflow)
        );
        assert_eq!(
            position_payout(MicroShares(1), MicroUsd(-1)),
            Err(ResolutionError::Overflow)
        );
    }

    #[test]
    fn settlement_includes_pool_inventory_and_conserves_escrow() {
        let user = account();
        let pool = account();
        let minted = 1_000_003;
        let holdings = [
            (user, Side::Yes, MicroShares(600_001)),
            (pool, Side::Yes, MicroShares(400_002)),
            (user, Side::No, MicroShares(100_001)),
            (pool, Side::No, MicroShares(900_002)),
        ];

        let settlement = settle_market(&holdings, 7_350, MicroUsd(minted)).unwrap();
        let payout_sum: i64 = settlement.payouts.iter().map(|(_, payout)| payout.0).sum();

        assert_eq!(settlement.payouts.len(), holdings.len());
        assert_eq!(payout_sum + settlement.dust.0, minted);
        assert!(settlement.dust.0 >= 0);
        assert!(settlement.dust.0 < i64::try_from(holdings.len()).unwrap());
        assert_eq!(
            settlement
                .payouts
                .iter()
                .filter(|(account, _)| *account == pool)
                .count(),
            2
        );
    }

    #[test]
    fn missing_pool_inventory_is_a_hard_error() {
        let user = account();
        let pool = account();
        let minted = 1_000_003;
        let incomplete = [
            (user, Side::Yes, MicroShares(600_001)),
            (user, Side::No, MicroShares(100_001)),
            (pool, Side::No, MicroShares(900_002)),
        ];

        assert_eq!(
            settle_market(&incomplete, 7_350, MicroUsd(minted)),
            Err(ResolutionError::HoldingsIncomplete {
                minted: MicroShares(minted),
                presented: MicroShares(600_001),
            })
        );
    }

    #[test]
    fn one_holding_cannot_represent_a_positive_complete_set() {
        let minted = 1_000_000;
        assert_eq!(
            settle_market(
                &[(account(), Side::Yes, MicroShares(minted))],
                5_000,
                MicroUsd(minted),
            ),
            Err(ResolutionError::HoldingsIncomplete {
                minted: MicroShares(minted),
                presented: MicroShares(0),
            })
        );
    }

    #[test]
    fn invalid_settlement_quantities_are_rejected() {
        assert_eq!(
            settle_market(&[], 10_001, MicroUsd(0)),
            Err(ResolutionError::ActualOutOfRange)
        );
        assert_eq!(
            settle_market(&[], 5_000, MicroUsd(-1)),
            Err(ResolutionError::Overflow)
        );
        assert_eq!(
            settle_market(
                &[(account(), Side::Yes, MicroShares(-1))],
                5_000,
                MicroUsd(0),
            ),
            Err(ResolutionError::Overflow)
        );
    }

    #[test]
    fn empty_zero_market_violates_the_strict_dust_bound() {
        assert_eq!(
            settle_market(&[], 5_000, MicroUsd(0)),
            Err(ResolutionError::ConservationViolated {
                expected: MicroUsd(0),
                got: MicroUsd(0),
            })
        );
    }

    #[test]
    fn maximal_fractional_residue_stays_below_holding_count() {
        let minted = 9_999;
        let settlement = settle_market(
            &[
                (account(), Side::Yes, MicroShares(minted)),
                (account(), Side::No, MicroShares(minted)),
            ],
            1,
            MicroUsd(minted),
        )
        .unwrap();

        assert_eq!(settlement.dust, MicroUsd(1));
        assert!(settlement.dust.0 < 2);
    }

    proptest! {
        #[test]
        fn arbitrary_complete_holdings_conserve_with_bounded_dust(
            minted in 2i64..1_000_000_000,
            yes_seed in 0i64..1_000_000_000,
            no_seed in 0i64..1_000_000_000,
            actual in 0u16..=10_000,
        ) {
            let user = account();
            let pool = account();
            let yes_user = 1 + yes_seed % (minted - 1);
            let no_user = 1 + no_seed % (minted - 1);
            let holdings = [
                (user, Side::Yes, MicroShares(yes_user)),
                (pool, Side::Yes, MicroShares(minted - yes_user)),
                (user, Side::No, MicroShares(no_user)),
                (pool, Side::No, MicroShares(minted - no_user)),
            ];

            let settlement = settle_market(&holdings, actual, MicroUsd(minted)).unwrap();
            let payout_sum: i64 = settlement.payouts.iter().map(|(_, payout)| payout.0).sum();

            prop_assert_eq!(payout_sum + settlement.dust.0, minted);
            prop_assert!(settlement.dust.0 >= 0);
            prop_assert!(settlement.dust.0 < i64::try_from(holdings.len()).unwrap());
        }
    }
}
