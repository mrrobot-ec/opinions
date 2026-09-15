//! Deterministic integer hot ranking shared with the SQL read projection.

use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum RankError {
    #[error("a future timestamp cannot be ranked")]
    FutureTimestamp,
}

/// Returns the pinned gravity-two hot score.
///
/// # Errors
/// Returns [`RankError::FutureTimestamp`] when `age_secs` is negative.
pub fn hot_score(score: i32, age_secs: i64) -> Result<i64, RankError> {
    if age_secs < 0 {
        return Err(RankError::FutureTimestamp);
    }
    let score_num = (i128::from(score.max(0)) + 1) * 1_000_000;
    let age_hours = i128::from(age_secs / 3_600);
    let denominator = (age_hours + 2) * (age_hours + 2);
    Ok(i64::try_from(score_num / denominator).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn boundaries_and_baseline_are_exact() {
        assert_eq!(hot_score(-1, 0), Ok(250_000));
        assert_eq!(hot_score(0, 3_599), Ok(250_000));
        assert_eq!(hot_score(0, 3_600), Ok(111_111));
        assert_eq!(hot_score(1, 7_199), Ok(222_222));
        assert_eq!(hot_score(i32::MAX, i64::MAX), Ok(0));
        assert_eq!(hot_score(0, -1), Err(RankError::FutureTimestamp));
    }

    #[test]
    fn score_clamps_and_age_decay_are_monotonic() {
        assert_eq!(hot_score(i32::MIN, 0), hot_score(0, 0));
        assert!(hot_score(10, 0).unwrap() > hot_score(1, 0).unwrap());
        assert!(hot_score(1, 0).unwrap() > hot_score(1, 3_600).unwrap());
    }
}
