use thiserror::Error;

/// Integer micro-USDC. All money is `i64` micro; no floats anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MicroUsd(pub i64);
/// Integer micro-shares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MicroShares(pub i64);
/// Fee rate in basis points (1 bp = 0.01%).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BasisPoints(pub u16);

/// Result of splitting a gross amount into net + fee; `net + fee == gross` exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeSplit {
    pub net: MicroUsd,
    pub fee: MicroUsd,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MoneyError {
    #[error("amount must be non-negative")]
    NegativeAmount,
    #[error("arithmetic overflow")]
    Overflow,
    #[error("fee above 100%")]
    FeeTooHigh,
}

impl MicroUsd {
    #[must_use]
    pub fn checked_add(self, o: Self) -> Option<Self> {
        self.0.checked_add(o.0).map(Self)
    }
    #[must_use]
    pub fn checked_sub(self, o: Self) -> Option<Self> {
        self.0.checked_sub(o.0).map(Self)
    }
}

/// Fee rounds UP (ceil) so rounding always favors the house; `net + fee == gross` exactly.
///
/// # Errors
///
/// Returns [`MoneyError::NegativeAmount`] if `gross` is negative,
/// [`MoneyError::FeeTooHigh`] if `fee` exceeds 10 000 bps, and
/// [`MoneyError::Overflow`] if the fee amount cannot fit an `i64`.
pub fn apply_fee(gross: MicroUsd, fee: BasisPoints) -> Result<FeeSplit, MoneyError> {
    if gross.0 < 0 {
        return Err(MoneyError::NegativeAmount);
    }
    if fee.0 > 10_000 {
        return Err(MoneyError::FeeTooHigh);
    }
    let g = i128::from(gross.0);
    let f = (g * i128::from(fee.0) + 9_999) / 10_000; // ceil
    let fee_amt = i64::try_from(f).map_err(|_| MoneyError::Overflow)?;
    Ok(FeeSplit {
        net: MicroUsd(gross.0 - fee_amt),
        fee: MicroUsd(fee_amt),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn fee_rounds_up_against_user() {
        // 1% of 101 micro = 1.01 -> fee must be 2 (ceil), net 99
        let s = apply_fee(MicroUsd(101), BasisPoints(100)).unwrap();
        assert_eq!(s.fee, MicroUsd(2));
        assert_eq!(s.net, MicroUsd(99));
    }

    #[test]
    fn zero_fee_passes_through() {
        let s = apply_fee(MicroUsd(500), BasisPoints(0)).unwrap();
        assert_eq!((s.net, s.fee), (MicroUsd(500), MicroUsd(0)));
    }

    #[test]
    fn negative_and_overfee_rejected() {
        assert!(matches!(
            apply_fee(MicroUsd(-1), BasisPoints(100)),
            Err(MoneyError::NegativeAmount)
        ));
        assert!(matches!(
            apply_fee(MicroUsd(1), BasisPoints(10_001)),
            Err(MoneyError::FeeTooHigh)
        ));
    }

    #[test]
    fn checked_arithmetic_reports_bounds() {
        assert_eq!(MicroUsd(7).checked_add(MicroUsd(5)), Some(MicroUsd(12)));
        assert_eq!(MicroUsd(i64::MAX).checked_add(MicroUsd(1)), None);
        assert_eq!(MicroUsd(7).checked_sub(MicroUsd(5)), Some(MicroUsd(2)));
        assert_eq!(MicroUsd(i64::MIN).checked_sub(MicroUsd(1)), None);
    }

    proptest! {
        #[test]
        fn split_always_reassembles(gross in 0i64..=i64::MAX / 20_000, bps in 0u16..=10_000) {
            let s = apply_fee(MicroUsd(gross), BasisPoints(bps)).unwrap();
            prop_assert_eq!(s.net.0 + s.fee.0, gross);
            prop_assert!(s.fee.0 >= 0 && s.net.0 >= 0);
        }
    }
}
