//! Integer CPMM over complete sets. All quantities are integer micro-units;
//! every product and discriminant is computed in checked `u128` (values are
//! non-negative by construction under the closed input caps); rounding always
//! favors the pool.

use crate::money::{apply_fee, BasisPoints, MicroShares, MicroUsd, MoneyError};
use thiserror::Error;

/// Closed cap on pool reserves, in micro-shares (= $10^9 of complete sets).
/// Quotes reject not only larger inputs but any RESULTING reserve above this,
/// so the cap is an invariant of reachable pool states.
pub const MAX_RESERVE: i64 = 1_000_000_000_000_000;
/// Closed cap on trade inputs (collateral in micro-USD, shares in micro-shares).
pub const MAX_AMOUNT: i64 = 1_000_000_000_000_000;

/// Which outcome side a trade touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Yes,
    No,
}

/// CPMM reserves plus the trading fee. Construct via [`Pool::new`]; the
/// constructor is what enforces positive, in-cap reserves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pool {
    pub yes: MicroShares,
    pub no: MicroShares,
    pub fee: BasisPoints,
}

/// Result of a buy quote. `avg_price_micro_per_share` is the all-in gross
/// price per whole share in micro-USD, floor-rounded — display only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuyQuote {
    pub shares_out: MicroShares,
    pub fee: MicroUsd,
    pub pool_after: Pool,
    pub avg_price_micro_per_share: i64,
}

/// Result of a sell quote. `collateral_out` is post-fee.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SellQuote {
    pub collateral_out: MicroUsd,
    pub fee: MicroUsd,
    pub pool_after: Pool,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AmmError {
    #[error("pool reserves must be positive")]
    EmptyPool,
    #[error("amount is too small to move the pool")]
    AmountTooSmall,
    #[error("sell would drain a pool reserve to zero")]
    DrainsPool,
    #[error("input or resulting reserve exceeds the closed cap")]
    InputTooLarge,
    #[error("arithmetic overflow")]
    Overflow,
}

/// Fee-math failures surface through the AMM vocabulary: a negative amount is
/// an amount too small to trade, an over-100% fee is an oversized input, and
/// overflow stays overflow.
impl From<MoneyError> for AmmError {
    fn from(e: MoneyError) -> Self {
        match e {
            MoneyError::NegativeAmount => AmmError::AmountTooSmall,
            MoneyError::Overflow => AmmError::Overflow,
            MoneyError::FeeTooHigh => AmmError::InputTooLarge,
        }
    }
}

/// `i64 → u128` for values already validated non-negative.
fn to_u128(v: i64) -> u128 {
    u128::from(v.unsigned_abs())
}

fn max_reserve_u128() -> u128 {
    to_u128(MAX_RESERVE)
}

/// Floor square root by Newton's method, seeded from `1 << ((bits+1)/2)`
/// (always ≥ √x, so the descent terminates at the floor root).
fn isqrt_u128(x: u128) -> u128 {
    if x < 2 {
        return x;
    }
    let bits = 128 - x.leading_zeros();
    let mut r = 1u128 << bits.div_ceil(2);
    loop {
        let next = u128::midpoint(r, x / r);
        if next >= r {
            return r;
        }
        r = next;
    }
}

impl Pool {
    /// # Errors
    ///
    /// Returns [`AmmError::EmptyPool`] for non-positive reserves and
    /// [`AmmError::InputTooLarge`] for reserves above [`MAX_RESERVE`] or a
    /// fee above 10 000 bps.
    pub fn new(yes: MicroShares, no: MicroShares, fee: BasisPoints) -> Result<Self, AmmError> {
        if yes.0 <= 0 || no.0 <= 0 {
            return Err(AmmError::EmptyPool);
        }
        if yes.0 > MAX_RESERVE || no.0 > MAX_RESERVE || fee.0 > 10_000 {
            return Err(AmmError::InputTooLarge);
        }
        Ok(Self { yes, no, fee })
    }
}

/// Marginal price of `side` in micro-USD per whole share (floor). Display
/// only — never used in settlement.
#[must_use]
pub fn price_micro(pool: &Pool, side: Side) -> i64 {
    let (yes, no) = (to_u128(pool.yes.0), to_u128(pool.no.0));
    let opposite = match side {
        Side::Yes => no,
        Side::No => yes,
    };
    let p = opposite * 1_000_000 / (yes + no);
    i64::try_from(p).unwrap_or(i64::MAX) // p ≤ 1_000_000, conversion cannot fail
}

/// Buy `side` with gross `collateral`: split out the fee, mint `net` complete
/// sets, hand the opposite leg to the pool, and retain
/// `ceil(buy_reserve · other_reserve / other_after)` on the buy side (ceil
/// keeps rounding dust in the pool).
///
/// # Errors
///
/// [`AmmError::InputTooLarge`] if `collateral` exceeds [`MAX_AMOUNT`] or the
/// resulting opposite reserve would exceed [`MAX_RESERVE`];
/// [`AmmError::AmountTooSmall`] if `collateral` is negative or buys zero
/// shares; [`AmmError::Overflow`] if any checked step cannot fit.
pub fn quote_buy(
    pool: &Pool,
    side: Side,
    collateral: MicroUsd,
    fee: BasisPoints,
) -> Result<BuyQuote, AmmError> {
    if collateral.0 > MAX_AMOUNT {
        return Err(AmmError::InputTooLarge);
    }
    let split = apply_fee(collateral, fee)?;
    let (buy_reserve, other_reserve) = match side {
        Side::Yes => (pool.yes.0, pool.no.0),
        Side::No => (pool.no.0, pool.yes.0),
    };
    let a = to_u128(buy_reserve);
    let b = to_u128(other_reserve);
    let net = to_u128(split.net.0);

    let b_after = b.checked_add(net).ok_or(AmmError::Overflow)?;
    if b_after > max_reserve_u128() {
        return Err(AmmError::InputTooLarge);
    }
    let k = a.checked_mul(b).ok_or(AmmError::Overflow)?;
    let a_pool = k.checked_add(b_after - 1).ok_or(AmmError::Overflow)? / b_after; // ceil
    let out = a
        .checked_add(net)
        .ok_or(AmmError::Overflow)?
        .checked_sub(a_pool)
        .ok_or(AmmError::Overflow)?;
    if out == 0 {
        return Err(AmmError::AmountTooSmall);
    }
    let avg = to_u128(collateral.0)
        .checked_mul(1_000_000)
        .ok_or(AmmError::Overflow)?
        / out;

    let (yes_after, no_after) = match side {
        Side::Yes => (a_pool, b_after),
        Side::No => (b_after, a_pool),
    };
    Ok(BuyQuote {
        shares_out: MicroShares(i64::try_from(out).map_err(|_| AmmError::Overflow)?),
        fee: split.fee,
        pool_after: Pool {
            yes: MicroShares(i64::try_from(yes_after).map_err(|_| AmmError::Overflow)?),
            no: MicroShares(i64::try_from(no_after).map_err(|_| AmmError::Overflow)?),
            fee: pool.fee,
        },
        avg_price_micro_per_share: i64::try_from(avg).map_err(|_| AmmError::Overflow)?,
    })
}

/// Sell `shares` of `side`: return them to the pool, then burn `c` complete
/// sets such that `(sell + s − c)(other − c) = sell·other`. With
/// `b = sell + s + other`, the valid (smaller) root is
/// `c = (b − isqrt(b² − 4·s·other)) / 2`, floored, then decremented while the
/// invariant would shrink (≤ 2 iterations in practice). The fee comes out of
/// the sell proceeds `c`.
///
/// # Errors
///
/// [`AmmError::AmountTooSmall`] if `shares` is non-positive or the proceeds
/// round to zero; [`AmmError::InputTooLarge`] if `shares` exceeds
/// [`MAX_AMOUNT`] or the resulting sell-side reserve would exceed
/// [`MAX_RESERVE`]; [`AmmError::DrainsPool`] if the burn would empty the
/// opposite reserve; [`AmmError::Overflow`] if any checked step cannot fit.
pub fn quote_sell(
    pool: &Pool,
    side: Side,
    shares: MicroShares,
    fee: BasisPoints,
) -> Result<SellQuote, AmmError> {
    if shares.0 <= 0 {
        return Err(AmmError::AmountTooSmall);
    }
    if shares.0 > MAX_AMOUNT {
        return Err(AmmError::InputTooLarge);
    }
    let (sell_reserve, other_reserve) = match side {
        Side::Yes => (pool.yes.0, pool.no.0),
        Side::No => (pool.no.0, pool.yes.0),
    };
    let sell_side = to_u128(sell_reserve);
    let other_side = to_u128(other_reserve);
    let s = to_u128(shares.0);

    let k = sell_side
        .checked_mul(other_side)
        .ok_or(AmmError::Overflow)?;
    let bsum = sell_side
        .checked_add(s)
        .ok_or(AmmError::Overflow)?
        .checked_add(other_side)
        .ok_or(AmmError::Overflow)?;
    let four_sb = s
        .checked_mul(other_side)
        .ok_or(AmmError::Overflow)?
        .checked_mul(4)
        .ok_or(AmmError::Overflow)?;
    let disc = bsum
        .checked_mul(bsum)
        .ok_or(AmmError::Overflow)?
        .checked_sub(four_sb)
        .ok_or(AmmError::Overflow)?;
    // Smaller quadratic root, floored. The exact root satisfies c < min(s, b)+1,
    // so `a + s - c` and the guard products below cannot underflow.
    let mut c = (bsum - isqrt_u128(disc)) / 2;
    if c >= other_side {
        return Err(AmmError::DrainsPool);
    }
    loop {
        if c == 0 {
            return Err(AmmError::AmountTooSmall);
        }
        let after_product = (sell_side + s - c)
            .checked_mul(other_side - c)
            .ok_or(AmmError::Overflow)?;
        if after_product >= k {
            break; // invariant holds; rounding dust stays in the pool
        }
        c -= 1;
    }
    let sell_after = sell_side + s - c;
    if sell_after > max_reserve_u128() {
        return Err(AmmError::InputTooLarge);
    }
    let other_after = other_side - c;

    let gross = MicroUsd(i64::try_from(c).map_err(|_| AmmError::Overflow)?);
    let split = apply_fee(gross, fee)?;
    let (yes_after, no_after) = match side {
        Side::Yes => (sell_after, other_after),
        Side::No => (other_after, sell_after),
    };
    Ok(SellQuote {
        collateral_out: split.net,
        fee: split.fee,
        pool_after: Pool {
            yes: MicroShares(i64::try_from(yes_after).map_err(|_| AmmError::Overflow)?),
            no: MicroShares(i64::try_from(no_after).map_err(|_| AmmError::Overflow)?),
            fee: pool.fee,
        },
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::money::{BasisPoints, MicroShares, MicroUsd};
    use proptest::prelude::*;

    fn pool(y: i64, n: i64) -> Pool {
        Pool::new(MicroShares(y), MicroShares(n), BasisPoints(100)).unwrap()
    }

    #[test]
    fn buy_at_even_pool_returns_close_to_double() {
        // symmetric $1000 pool, buy $10 gross: ~19.6 YES out at ~51¢ avg
        let q = quote_buy(
            &pool(1_000_000_000, 1_000_000_000),
            Side::Yes,
            MicroUsd(10_000_000),
            BasisPoints(100),
        )
        .unwrap();
        assert!(q.shares_out.0 > 19_000_000 && q.shares_out.0 < 20_000_000);
        assert_eq!(q.fee, MicroUsd(100_000)); // 1% of $10
    }

    #[test]
    fn sell_cannot_drain_pool() {
        // absurd share amount vs tiny pool must error, not panic or go negative
        // (i64::MAX/4 > MAX_AMOUNT, so the input cap legitimately fires first — R3/codex B2)
        let r = quote_sell(
            &pool(1_000, 1_000),
            Side::Yes,
            MicroShares(i64::MAX / 4),
            BasisPoints(100),
        );
        assert!(matches!(
            r,
            Err(AmmError::DrainsPool | AmmError::InputTooLarge | AmmError::Overflow)
        ));
        // and an IN-CAP drain attempt must hit the drain guard specifically:
        let r2 = quote_sell(
            &pool(1_000, 1_000),
            Side::Yes,
            MicroShares(1_000_000_000),
            BasisPoints(100),
        );
        assert!(matches!(r2, Err(AmmError::DrainsPool)));
    }

    #[test]
    fn sell_exact_symmetric_case() {
        // yes=no=1000, sell 1000 YES: b=3000, disc=5e6, isqrt=2236, c=382 floors
        // then the guard steps to 381 ((1619)(619)=1_002_161 ≥ 1e6).
        let q = quote_sell(
            &pool(1_000, 1_000),
            Side::Yes,
            MicroShares(1_000),
            BasisPoints(100),
        )
        .unwrap();
        assert_eq!(q.fee, MicroUsd(4)); // ceil(381 * 100 / 10_000)
        assert_eq!(q.collateral_out, MicroUsd(377));
        assert_eq!(q.pool_after.yes, MicroShares(1_619));
        assert_eq!(q.pool_after.no, MicroShares(619));
    }

    #[test]
    fn sell_no_side_is_symmetric() {
        let no = quote_sell(
            &pool(2_000, 3_000),
            Side::No,
            MicroShares(1_000),
            BasisPoints(100),
        )
        .unwrap();
        let yes = quote_sell(
            &pool(3_000, 2_000),
            Side::Yes,
            MicroShares(1_000),
            BasisPoints(100),
        )
        .unwrap();

        assert_eq!(no.collateral_out, yes.collateral_out);
        assert_eq!(no.fee, yes.fee);
        assert_eq!(no.pool_after.yes, yes.pool_after.no);
        assert_eq!(no.pool_after.no, yes.pool_after.yes);
    }

    #[test]
    fn pool_construction_bounds() {
        assert!(matches!(
            Pool::new(MicroShares(0), MicroShares(5), BasisPoints(0)),
            Err(AmmError::EmptyPool)
        ));
        assert!(matches!(
            Pool::new(MicroShares(5), MicroShares(-1), BasisPoints(0)),
            Err(AmmError::EmptyPool)
        ));
        assert!(Pool::new(
            MicroShares(MAX_RESERVE),
            MicroShares(MAX_RESERVE - 1),
            BasisPoints(0)
        )
        .is_ok());
        assert!(matches!(
            Pool::new(MicroShares(MAX_RESERVE + 1), MicroShares(5), BasisPoints(0)),
            Err(AmmError::InputTooLarge)
        ));
        assert!(matches!(
            Pool::new(MicroShares(5), MicroShares(5), BasisPoints(10_001)),
            Err(AmmError::InputTooLarge)
        ));
    }

    #[test]
    fn quote_input_and_result_caps_are_closed() {
        // input above the cap
        assert!(matches!(
            quote_buy(
                &pool(1_000, 1_000),
                Side::Yes,
                MicroUsd(MAX_AMOUNT + 1),
                BasisPoints(100)
            ),
            Err(AmmError::InputTooLarge)
        ));
        assert!(matches!(
            quote_sell(
                &pool(1_000, 1_000),
                Side::Yes,
                MicroShares(MAX_AMOUNT + 1),
                BasisPoints(100)
            ),
            Err(AmmError::InputTooLarge)
        ));
        // in-cap input whose RESULTING reserve would breach the cap
        let full = Pool::new(
            MicroShares(MAX_RESERVE),
            MicroShares(MAX_RESERVE),
            BasisPoints(0),
        )
        .unwrap();
        assert!(matches!(
            quote_buy(&full, Side::Yes, MicroUsd(1_000_000), BasisPoints(0)),
            Err(AmmError::InputTooLarge)
        ));
        assert!(matches!(
            quote_sell(&full, Side::Yes, MicroShares(1_000_000), BasisPoints(0)),
            Err(AmmError::InputTooLarge)
        ));
    }

    #[test]
    fn too_small_amounts_rejected() {
        assert!(matches!(
            quote_buy(
                &pool(1_000_000, 1_000_000),
                Side::Yes,
                MicroUsd(0),
                BasisPoints(100)
            ),
            Err(AmmError::AmountTooSmall)
        ));
        assert!(matches!(
            quote_buy(
                &pool(1_000_000, 1_000_000),
                Side::Yes,
                MicroUsd(-5),
                BasisPoints(100)
            ),
            Err(AmmError::AmountTooSmall)
        ));
        assert!(matches!(
            quote_sell(
                &pool(1_000_000, 1_000_000),
                Side::Yes,
                MicroShares(0),
                BasisPoints(100)
            ),
            Err(AmmError::AmountTooSmall)
        ));
        // a 1-micro-share sell against a deep pool rounds to zero proceeds
        assert!(matches!(
            quote_sell(
                &pool(1_000_000_000_000, 1_000_000_000_000),
                Side::Yes,
                MicroShares(1),
                BasisPoints(100),
            ),
            Err(AmmError::AmountTooSmall)
        ));
    }

    #[test]
    fn money_error_mapping_is_total() {
        assert_eq!(
            AmmError::from(MoneyError::NegativeAmount),
            AmmError::AmountTooSmall
        );
        assert_eq!(AmmError::from(MoneyError::Overflow), AmmError::Overflow);
        assert_eq!(
            AmmError::from(MoneyError::FeeTooHigh),
            AmmError::InputTooLarge
        );
    }

    #[test]
    fn isqrt_unit_cases() {
        assert_eq!(isqrt_u128(0), 0);
        assert_eq!(isqrt_u128(1), 1);
        assert_eq!(isqrt_u128(2), 1);
        assert_eq!(isqrt_u128(3), 1);
        assert_eq!(isqrt_u128(4), 2);
        assert_eq!(isqrt_u128(u128::MAX), (1 << 64) - 1);
    }

    proptest! {
        /// Square-roundtrip: x in 1..=u64::MAX so x·x cannot overflow and x·x − 1
        /// cannot underflow.
        #[test]
        fn isqrt_square_roundtrip(x in 1u64..=u64::MAX) {
            let x = u128::from(x);
            prop_assert_eq!(isqrt_u128(x * x), x);
            prop_assert_eq!(isqrt_u128(x * x - 1), x - 1);
        }
    }

    proptest! {
        /// Invariant never decreases on buys (rounding favors pool).
        #[test]
        fn buy_never_decreases_k(y in 1_000_000i64..1_000_000_000_000, n in 1_000_000i64..1_000_000_000_000, c in 1i64..10_000_000_000) {
            let p = pool(y, n);
            if let Ok(q) = quote_buy(&p, Side::Yes, MicroUsd(c), p.fee) {
                prop_assert!(i128::from(q.pool_after.yes.0) * i128::from(q.pool_after.no.0)
                          >= i128::from(y) * i128::from(n));
                prop_assert!(q.shares_out.0 > 0);
            }
        }

        /// Immediate round-trip is never profitable, even at zero fee (pure rounding).
        #[test]
        fn round_trip_never_profits(y in 1_000_000i64..1_000_000_000_000, n in 1_000_000i64..1_000_000_000_000, c in 1i64..10_000_000_000) {
            let p = Pool::new(MicroShares(y), MicroShares(n), BasisPoints(0)).unwrap();
            if let Ok(b) = quote_buy(&p, Side::Yes, MicroUsd(c), p.fee) {
                if let Ok(s) = quote_sell(&b.pool_after, Side::Yes, b.shares_out, p.fee) {
                    prop_assert!(s.collateral_out.0 <= c);
                }
            }
        }

        /// Marginal prices are a coherent probability pair within 1 micro.
        #[test]
        fn prices_sum_to_one(y in 1_000i64..1_000_000_000_000, n in 1_000i64..1_000_000_000_000) {
            let p = pool(y, n);
            let s = price_micro(&p, Side::Yes) + price_micro(&p, Side::No);
            prop_assert!((999_998..=1_000_000).contains(&s));
        }

        /// NO-side buy is exactly the mirrored YES-side buy.
        #[test]
        fn no_side_is_symmetric(y in 1_000_000i64..1_000_000_000, n in 1_000_000i64..1_000_000_000, c in 1i64..1_000_000_000) {
            let a = quote_buy(&pool(y, n), Side::No, MicroUsd(c), BasisPoints(100));
            let b = quote_buy(&pool(n, y), Side::Yes, MicroUsd(c), BasisPoints(100));
            match (a, b) {
                (Ok(qa), Ok(qb)) => {
                    prop_assert_eq!(qa.shares_out, qb.shares_out);
                    prop_assert_eq!(qa.pool_after.yes, qb.pool_after.no);
                    prop_assert_eq!(qa.pool_after.no, qb.pool_after.yes);
                }
                (Err(_), Err(_)) => {}
                (a, b) => prop_assert!(false, "asymmetry: {a:?} vs {b:?}"),
            }
        }
    }
}
