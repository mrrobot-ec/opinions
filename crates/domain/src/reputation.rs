//! Fixed-point voter reputation and inclusive-lower tier mapping.

use thiserror::Error;

const UNIT: i64 = 1_000_000;

/// Supported reputation half-lives. Each coefficient is the precomputed
/// round-half-up value of `2^(-1/H) * 1_000_000`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HalfLife {
    H10,
    H20,
    H40,
}

impl HalfLife {
    #[must_use]
    pub const fn k_ppm(self) -> u32 {
        match self {
            Self::H10 => 933_033,
            Self::H20 => 965_936,
            Self::H40 => 982_821,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum RepError {
    #[error("reputation and score must be within 0..=1,000,000")]
    OperandOutOfRange,
}

/// Applies one fixed-point EWMA update with round-half-up division.
///
/// The coefficient is pinned by [`HalfLife`], so it can never equal the unit
/// and freeze reputation. Per-step rounding error relative to the exact
/// rational recurrence is at most one half micro-unit; the golden recurrence
/// tests pin the accumulated integer result rather than claiming an exact
/// half-distance after H steps.
///
/// # Errors
/// Returns [`RepError::OperandOutOfRange`] for either operand outside the
/// closed fixed-point unit interval.
pub fn update_rep(rep_micro: i64, score_micro: i64, h: HalfLife) -> Result<i64, RepError> {
    if !(0..=UNIT).contains(&rep_micro) || !(0..=UNIT).contains(&score_micro) {
        return Err(RepError::OperandOutOfRange);
    }
    let k = i128::from(h.k_ppm());
    let numerator = i128::from(rep_micro) * k + i128::from(score_micro) * (i128::from(UNIT) - k);
    let rounded = (numerator + i128::from(UNIT / 2)) / i128::from(UNIT);
    i64::try_from(rounded).map_err(|_| RepError::OperandOutOfRange)
}

/// Maps reputation into tier 0..=4. Thresholds are inclusive lower bounds
/// and are validated by the injected application configuration.
#[must_use]
pub fn tier_for(rep_micro: i64, thresholds: &[i64; 4]) -> u8 {
    thresholds.iter().fold(0_u8, |tier, threshold| {
        tier + u8::from(rep_micro >= *threshold)
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn coefficients_are_exactly_pinned() {
        assert_eq!(HalfLife::H10.k_ppm(), 933_033);
        assert_eq!(HalfLife::H20.k_ppm(), 965_936);
        assert_eq!(HalfLife::H40.k_ppm(), 982_821);
    }

    fn repeated(mut rep: i64, score: i64, h: HalfLife, count: usize) -> i64 {
        for _ in 0..count {
            rep = update_rep(rep, score, h).unwrap();
        }
        rep
    }

    #[test]
    fn integer_recurrence_golden_vectors_are_exact() {
        assert_eq!(repeated(0, UNIT, HalfLife::H10, 10), 500_000);
        assert_eq!(repeated(UNIT, 0, HalfLife::H10, 10), 500_000);
        assert_eq!(repeated(0, UNIT, HalfLife::H20, 20), 500_004);
        assert_eq!(repeated(UNIT, 0, HalfLife::H20, 20), 499_996);
        assert_eq!(repeated(0, UNIT, HalfLife::H40, 40), 499_993);
        assert_eq!(repeated(UNIT, 0, HalfLife::H40, 40), 500_007);
    }

    #[test]
    fn bounds_are_fixed_points_and_inputs_are_closed() {
        assert_eq!(update_rep(0, 0, HalfLife::H20).unwrap(), 0);
        assert_eq!(update_rep(UNIT, UNIT, HalfLife::H20).unwrap(), UNIT);
        for (rep, score) in [(-1, 0), (0, -1), (UNIT + 1, 0), (0, UNIT + 1)] {
            assert_eq!(
                update_rep(rep, score, HalfLife::H20),
                Err(RepError::OperandOutOfRange)
            );
        }
    }

    #[test]
    fn movement_is_monotonic_toward_the_score() {
        assert!(update_rep(100_000, 900_000, HalfLife::H20).unwrap() > 100_000);
        assert!(update_rep(900_000, 100_000, HalfLife::H20).unwrap() < 900_000);
    }

    #[test]
    fn tier_thresholds_are_inclusive_lower_bounds() {
        let thresholds = [100_000, 300_000, 600_000, 900_000];
        assert_eq!(tier_for(99_999, &thresholds), 0);
        assert_eq!(tier_for(100_000, &thresholds), 1);
        assert_eq!(tier_for(599_999, &thresholds), 2);
        assert_eq!(tier_for(600_000, &thresholds), 3);
        assert_eq!(tier_for(1_000_000, &thresholds), 4);
    }
}
