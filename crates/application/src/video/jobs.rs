//! Leasing arithmetic pinned by Task 5.2 (codex B4 / P5R2 N3).

use time::{Duration, OffsetDateTime};

/// Claim batch per worker tick; small so leases stay honest under load.
pub const CLAIM_BATCH: u32 = 8;
/// Expired-lease reclaim batch per tick.
pub const RECLAIM_BATCH: u32 = 32;

/// Latest representable delay target: 9999-12-31T23:59:59Z. Backoffs clamp
/// here so saturation can never overflow the timestamp domain.
const MAX_UNIX_SECS: i64 = 253_402_300_799;

/// Saturating exponential backoff:
/// `available_at = now + backoff_base_secs * 2^min(attempts, 8)`.
#[must_use]
pub fn backoff_available_at(now: OffsetDateTime, base_secs: u64, attempts: u32) -> OffsetDateTime {
    let factor = 1u64 << attempts.min(8);
    let secs = i64::try_from(base_secs.saturating_mul(factor)).unwrap_or(i64::MAX);
    let headroom = (MAX_UNIX_SECS - now.unix_timestamp()).max(0);
    now + Duration::seconds(secs.min(headroom))
}

/// The `>=` attempts boundary shared by failure completion and expired-lease
/// reclaim: `max_attempts = 1` means exactly one attempt, ever.
#[must_use]
pub fn is_terminal(attempts: u32, max_attempts: u32) -> bool {
    attempts >= max_attempts
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn at(unix: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(unix).unwrap()
    }

    #[test]
    fn backoff_doubles_then_saturates_at_two_to_the_eighth() {
        let now = at(1_700_000_000);
        assert_eq!(backoff_available_at(now, 5, 0), now + Duration::seconds(5));
        assert_eq!(backoff_available_at(now, 5, 1), now + Duration::seconds(10));
        assert_eq!(backoff_available_at(now, 5, 3), now + Duration::seconds(40));
        assert_eq!(
            backoff_available_at(now, 5, 8),
            now + Duration::seconds(5 * 256)
        );
        // Saturating: the exponent pins at 8 for every larger attempt count.
        assert_eq!(
            backoff_available_at(now, 5, 9),
            backoff_available_at(now, 5, 8)
        );
        assert_eq!(
            backoff_available_at(now, 5, u32::MAX),
            backoff_available_at(now, 5, 8)
        );
    }

    #[test]
    fn backoff_saturates_on_multiplication_and_time_overflow() {
        let now = at(1_700_000_000);
        // u64 saturating multiplication, then i64 clamp, then time clamp: the
        // result is a valid future timestamp, never a panic or wraparound.
        let far = backoff_available_at(now, u64::MAX, 8);
        assert!(far > now);
        let clamped = backoff_available_at(now, u64::MAX / 2, u32::MAX);
        assert!(clamped > now);
    }

    #[test]
    fn terminal_boundary_is_inclusive() {
        assert!(is_terminal(1, 1)); // max = 1 → exactly one attempt
        assert!(!is_terminal(0, 1));
        assert!(is_terminal(3, 3));
        assert!(!is_terminal(2, 3));
        assert!(is_terminal(4, 3));
    }
}
