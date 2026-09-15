//! One pure authority for tiered trade fees.

use crate::money::BasisPoints;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeeContext {
    pub base_bps: u16,
    pub tier: u8,
    pub discount_bp_by_tier: [u16; 5],
    pub min_fee_bps: u16,
    pub is_flip_within_window: bool,
}

/// Returns the authoritative fee for a trade. A recent-buy sell pays the
/// base rate; every other trade receives its tier discount down to the floor.
#[must_use]
pub fn effective_fee(ctx: &FeeContext) -> BasisPoints {
    if ctx.is_flip_within_window {
        return BasisPoints(ctx.base_bps);
    }
    let index = usize::from(ctx.tier.min(4));
    BasisPoints(
        ctx.base_bps
            .saturating_sub(ctx.discount_bp_by_tier[index])
            .max(ctx.min_fee_bps),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> FeeContext {
        FeeContext {
            base_bps: 100,
            tier: 2,
            discount_bp_by_tier: [0, 5, 20, 50, 100],
            min_fee_bps: 10,
            is_flip_within_window: false,
        }
    }

    #[test]
    fn discount_and_floor_have_one_authority() {
        assert_eq!(effective_fee(&context()), BasisPoints(80));
        let high_tier = FeeContext {
            tier: 4,
            ..context()
        };
        assert_eq!(effective_fee(&high_tier), BasisPoints(10));
    }

    #[test]
    fn flip_uses_base_and_defensive_tier_saturates() {
        let flip = FeeContext {
            is_flip_within_window: true,
            ..context()
        };
        assert_eq!(effective_fee(&flip), BasisPoints(100));
        let corrupt_tier = FeeContext {
            tier: u8::MAX,
            ..context()
        };
        assert_eq!(effective_fee(&corrupt_tier), BasisPoints(10));
    }
}
