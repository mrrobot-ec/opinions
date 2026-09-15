use crate::amm::Side;
use thiserror::Error;

/// Integer-basis-point components of one published vote score.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VoteScore {
    pub accuracy_bp: u16,
    pub majority_bp: u16,
    pub score_bp: u16,
}

/// Invalid input to the vote-scoring formula.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ScoringError {
    #[error("crowd guess must be at most 100 percent")]
    GuessOutOfRange,
    #[error("actual result must be at most 10,000 basis points")]
    ActualOutOfRange,
}

/// Scores a vote using the published 75% accuracy / 25% majority formula.
///
/// At exactly 5,000 actual YES basis points, both sides receive the majority
/// component.
///
/// # Errors
///
/// Returns [`ScoringError::GuessOutOfRange`] when `crowd_guess_pct > 100`, or
/// [`ScoringError::ActualOutOfRange`] when `actual_yes_bps > 10_000`.
pub fn score_vote(
    side: Side,
    crowd_guess_pct: u8,
    actual_yes_bps: u16,
) -> Result<VoteScore, ScoringError> {
    if crowd_guess_pct > 100 {
        return Err(ScoringError::GuessOutOfRange);
    }
    if actual_yes_bps > 10_000 {
        return Err(ScoringError::ActualOutOfRange);
    }

    let guessed_yes_bps = u16::from(crowd_guess_pct) * 100;
    let diff = guessed_yes_bps.abs_diff(actual_yes_bps);
    let accuracy_bp = 10_000_u16.saturating_sub(diff * 4);
    let wins_majority = actual_yes_bps == 5_000
        || matches!(side, Side::Yes) && actual_yes_bps > 5_000
        || matches!(side, Side::No) && actual_yes_bps < 5_000;
    let majority_bp = if wins_majority { 10_000_u16 } else { 0 };
    // Algebraically identical to (75*a + 25*m) / 100, while keeping the
    // bounded intermediate (at most 40,000) in u16.
    let score_bp = (3 * accuracy_bp + majority_bp) / 4;

    Ok(VoteScore {
        accuracy_bp,
        majority_bp,
        score_bp,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use proptest::prelude::*;

    #[test]
    fn accuracy_kernel_has_exact_landmarks() {
        assert_eq!(
            score_vote(Side::Yes, 73, 7_300).unwrap().accuracy_bp,
            10_000
        );
        assert_eq!(score_vote(Side::Yes, 75, 5_000).unwrap().accuracy_bp, 0);
        assert_eq!(score_vote(Side::Yes, 50, 6_250).unwrap().accuracy_bp, 5_000);
    }

    #[test]
    fn yes_majority_straddles_half_and_tie_awards_both_sides() {
        assert_eq!(
            score_vote(Side::Yes, 50, 5_001).unwrap().majority_bp,
            10_000
        );
        assert_eq!(score_vote(Side::Yes, 50, 4_999).unwrap().majority_bp, 0);
        assert_eq!(
            score_vote(Side::Yes, 50, 5_000).unwrap().majority_bp,
            10_000
        );
        assert_eq!(score_vote(Side::No, 50, 5_000).unwrap().majority_bp, 10_000);
    }

    #[test]
    fn no_majority_wins_below_half_only() {
        assert_eq!(score_vote(Side::No, 50, 4_999).unwrap().majority_bp, 10_000);
        assert_eq!(score_vote(Side::No, 50, 5_001).unwrap().majority_bp, 0);
    }

    #[test]
    fn perfect_guess_on_winning_side_scores_ten_thousand() {
        assert_eq!(score_vote(Side::Yes, 70, 7_000).unwrap().score_bp, 10_000);
    }

    #[test]
    fn out_of_range_inputs_are_rejected() {
        assert_eq!(
            score_vote(Side::Yes, 101, 5_000),
            Err(ScoringError::GuessOutOfRange)
        );
        assert_eq!(
            score_vote(Side::Yes, 50, 10_001),
            Err(ScoringError::ActualOutOfRange)
        );
    }

    proptest! {
        #[test]
        fn closer_guess_never_has_lower_accuracy(
            actual in 0u16..=10_000,
            yes_side in any::<bool>(),
            first in 0u8..=100,
            second in 0u8..=100,
        ) {
            let side = if yes_side { Side::Yes } else { Side::No };
            let first_distance = (i32::from(first) * 100 - i32::from(actual)).unsigned_abs();
            let second_distance = (i32::from(second) * 100 - i32::from(actual)).unsigned_abs();
            let (closer, farther) = if first_distance <= second_distance {
                (first, second)
            } else {
                (second, first)
            };

            let close_score = score_vote(side, closer, actual).unwrap();
            let far_score = score_vote(side, farther, actual).unwrap();

            prop_assert!(close_score.accuracy_bp >= far_score.accuracy_bp);
            for score in [close_score, far_score] {
                prop_assert!(score.accuracy_bp <= 10_000);
                prop_assert!(score.majority_bp <= 10_000);
                prop_assert!(score.score_bp <= 10_000);
            }
        }
    }
}
