//! Stable content-draft vocabulary. Validation and slotting land in Task 5.1.

use crate::money::{BasisPoints, MicroUsd};

const SECONDS_PER_DAY: i128 = 86_400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DraftingError {
    #[error("daily slot count must be positive")]
    InvalidSlots,
    #[error("flash cadence must be positive")]
    InvalidCadence,
    #[error("slot timestamp overflow")]
    TimestampOverflow,
    #[error("question must not be empty")]
    EmptyQuestion,
    #[error("description must not be empty")]
    EmptyDescription,
    #[error("video script must not be empty")]
    EmptyVideoScript,
    #[error("slug must not be empty")]
    EmptySlug,
    #[error("seed must be positive")]
    InvalidSeed,
    #[error("fee must not exceed 10,000 basis points")]
    InvalidFee,
    #[error("minimum votes must be positive")]
    InvalidMinVotes,
    #[error("hidden window must be shorter than a positive open window")]
    InvalidWindow,
    #[error("seed is below the tier floor")]
    SeedBelowFloor,
    #[error("minimum votes are below the tier floor")]
    VotesBelowFloor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DraftSource {
    Template,
    Llm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DraftTier {
    Daily,
    Flash,
}

/// Complete engine output; ordered plain values make generation replayable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftSpec {
    pub question: String,
    pub description: String,
    pub video_script: String,
    pub slug: String,
    pub tier: DraftTier,
    pub seed: MicroUsd,
    pub fee: BasisPoints,
    pub min_votes_to_resolve: i32,
    pub open_secs: u64,
    pub hidden_window_secs: u64,
}

impl DraftSpec {
    /// Validates the generator-independent shape of a draft.
    ///
    /// # Errors
    /// Returns the first invalid required field or numeric invariant.
    pub fn validate(&self) -> Result<(), DraftingError> {
        if self.question.trim().is_empty() {
            return Err(DraftingError::EmptyQuestion);
        }
        if self.description.trim().is_empty() {
            return Err(DraftingError::EmptyDescription);
        }
        if self.video_script.trim().is_empty() {
            return Err(DraftingError::EmptyVideoScript);
        }
        if self.slug.trim().is_empty() {
            return Err(DraftingError::EmptySlug);
        }
        if self.seed.0 <= 0 {
            return Err(DraftingError::InvalidSeed);
        }
        if self.fee.0 > 10_000 {
            return Err(DraftingError::InvalidFee);
        }
        if self.min_votes_to_resolve <= 0 {
            return Err(DraftingError::InvalidMinVotes);
        }
        if self.open_secs == 0 || self.hidden_window_secs >= self.open_secs {
            return Err(DraftingError::InvalidWindow);
        }
        Ok(())
    }

    /// Revalidates the capital and participation floors selected for a tier.
    ///
    /// # Errors
    /// Returns a shape error or the first tier floor the draft violates.
    pub fn validate_floors(
        &self,
        seed_floor_micro: i64,
        min_votes_floor: i32,
    ) -> Result<(), DraftingError> {
        self.validate()?;
        if self.seed.0 < seed_floor_micro {
            return Err(DraftingError::SeedBelowFloor);
        }
        if self.min_votes_to_resolve < min_votes_floor {
            return Err(DraftingError::VotesBelowFloor);
        }
        Ok(())
    }
}

/// Returns the next daily-tier UTC boundary strictly after `unix_seconds`.
/// Each UTC day has `slots` starts: midnight plus `i * floor(86400/slots)`
/// for `i = 0..slots`; the remainder is absorbed by the final interval.
///
/// # Errors
/// Returns [`DraftingError::InvalidSlots`] for zero slots or
/// [`DraftingError::TimestampOverflow`] when the next boundary exceeds `i64`.
pub fn next_daily_slot(unix_seconds: i64, slots: u16) -> Result<i64, DraftingError> {
    if slots == 0 {
        return Err(DraftingError::InvalidSlots);
    }
    let now = i128::from(unix_seconds);
    let day = now.div_euclid(SECONDS_PER_DAY);
    let second = now.rem_euclid(SECONDS_PER_DAY);
    let interval = SECONDS_PER_DAY / i128::from(slots);
    let next_index = second / interval + 1;
    let next = if next_index < i128::from(slots) {
        day * SECONDS_PER_DAY + next_index * interval
    } else {
        (day + 1) * SECONDS_PER_DAY
    };
    i64::try_from(next).map_err(|_| DraftingError::TimestampOverflow)
}

/// Returns the next cadence boundary strictly after `unix_seconds`.
///
/// # Errors
/// Returns [`DraftingError::InvalidCadence`] for a zero cadence or
/// [`DraftingError::TimestampOverflow`] when the next boundary exceeds `i64`.
pub fn next_flash_slot(unix_seconds: i64, cadence_secs: u64) -> Result<i64, DraftingError> {
    if cadence_secs == 0 {
        return Err(DraftingError::InvalidCadence);
    }
    let now = i128::from(unix_seconds);
    let cadence = i128::from(cadence_secs);
    let next = (now.div_euclid(cadence) + 1)
        .checked_mul(cadence)
        .ok_or(DraftingError::TimestampOverflow)?;
    i64::try_from(next).map_err(|_| DraftingError::TimestampOverflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_spec(tier: DraftTier) -> DraftSpec {
        DraftSpec {
            question: "Will the subway extension open by 2030?".into(),
            description: "A locally sourced transit question.".into(),
            video_script: "Track the milestones and vote.".into(),
            slug: "subway-extension-2030".into(),
            tier,
            seed: MicroUsd(2_000_000),
            fee: BasisPoints(100),
            min_votes_to_resolve: 3,
            open_secs: 3_600,
            hidden_window_secs: 300,
        }
    }

    #[test]
    fn daily_slots_are_integer_pinned_and_strictly_after_boundaries() {
        // Seven intervals use floor(86_400 / 7) = 12_342 seconds. The
        // six-second remainder belongs only to the final interval.
        assert_eq!(next_daily_slot(0, 7), Ok(12_342));
        assert_eq!(next_daily_slot(12_341, 7), Ok(12_342));
        assert_eq!(next_daily_slot(12_342, 7), Ok(24_684));
        assert_eq!(next_daily_slot(74_052, 7), Ok(86_400));
        assert_eq!(next_daily_slot(86_400, 7), Ok(98_742));

        // Euclidean day arithmetic keeps pre-epoch timestamps pinned too.
        assert_eq!(next_daily_slot(-1, 2), Ok(0));
        assert_eq!(next_daily_slot(-43_200, 2), Ok(0));
    }

    #[test]
    fn flash_slots_are_strictly_after_the_cadence_boundary() {
        assert_eq!(next_flash_slot(0, 3_600), Ok(3_600));
        assert_eq!(next_flash_slot(3_599, 3_600), Ok(3_600));
        assert_eq!(next_flash_slot(3_600, 3_600), Ok(7_200));
        assert_eq!(next_flash_slot(-1, 3_600), Ok(0));
    }

    #[test]
    fn slotting_rejects_zero_and_overflow_instead_of_wrapping() {
        assert_eq!(next_daily_slot(0, 0), Err(DraftingError::InvalidSlots));
        assert_eq!(
            next_daily_slot(i64::MAX, 1),
            Err(DraftingError::TimestampOverflow)
        );
        assert_eq!(next_flash_slot(0, 0), Err(DraftingError::InvalidCadence));
        assert_eq!(
            next_flash_slot(i64::MAX, 1),
            Err(DraftingError::TimestampOverflow)
        );
    }

    #[test]
    fn draft_validation_and_tier_floors_are_explicit() {
        let spec = valid_spec(DraftTier::Flash);
        assert_eq!(spec.validate(), Ok(()));
        assert_eq!(spec.validate_floors(1_000_000, 3), Ok(()));

        let mut invalid = spec.clone();
        invalid.question = "   ".into();
        assert_eq!(invalid.validate(), Err(DraftingError::EmptyQuestion));
        invalid = spec.clone();
        invalid.description = String::new();
        assert_eq!(invalid.validate(), Err(DraftingError::EmptyDescription));
        invalid = spec.clone();
        invalid.video_script = String::new();
        assert_eq!(invalid.validate(), Err(DraftingError::EmptyVideoScript));
        invalid = spec.clone();
        invalid.slug = String::new();
        assert_eq!(invalid.validate(), Err(DraftingError::EmptySlug));
        invalid = spec.clone();
        invalid.seed = MicroUsd(0);
        assert_eq!(invalid.validate(), Err(DraftingError::InvalidSeed));
        invalid = spec.clone();
        invalid.fee = BasisPoints(10_001);
        assert_eq!(invalid.validate(), Err(DraftingError::InvalidFee));
        invalid = spec.clone();
        invalid.min_votes_to_resolve = 0;
        assert_eq!(invalid.validate(), Err(DraftingError::InvalidMinVotes));
        invalid = spec.clone();
        invalid.hidden_window_secs = invalid.open_secs;
        assert_eq!(invalid.validate(), Err(DraftingError::InvalidWindow));
        invalid = spec.clone();
        invalid.seed = MicroUsd(999_999);
        assert_eq!(
            invalid.validate_floors(1_000_000, 3),
            Err(DraftingError::SeedBelowFloor)
        );
        invalid = spec;
        invalid.min_votes_to_resolve = 2;
        assert_eq!(
            invalid.validate_floors(1_000_000, 3),
            Err(DraftingError::VotesBelowFloor)
        );
    }
}
