//! Canonical BUY/SELL parameter selection and signed-body precision.
//!
//! Chooses venue-valid FAK BUY/SELL body parameters from fixed-point inputs.
//!
//! The golden tests lock known-live venue precision cases, not Python object
//! structure. Rust owns the runtime design; Python is only a temporary oracle
//! for body-shape invariants that already survived live probes.
//!
//! This module exposes only the `(price, size, maker_amount)` choices needed
//! by signing and submit code.

use crate::types::{
    MAX_PRICE_TICK, MIN_PRICE_TICK, PriceTick, Shares2, Shares4, UsdcCents,
    buy_size_multiple_taker_units, ceil_to_multiple, floor_to_multiple, maker_cents_for,
};

/// Which side of the lattice the chosen size came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuyCanonicalPolicy {
    /// `ceil`: smallest valid size at-or-above the target — slight overshoot.
    Ceil,
    /// `floor`: largest valid size below the target — slight undershoot.
    Floor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuyCanonicalTarget {
    pub price: PriceTick,
    pub size: Shares4,
    pub maker_amount: UsdcCents,
    pub raw_size_taker_units: i64,
    pub policy: BuyCanonicalPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuyCanonicalError {
    /// Price not in venue range \[$0.01, $0.99\].
    PriceOutOfRange { ticks: i32 },
    /// `target / price` is below the configured minimum BUY share count.
    RawSizeBelowMin {
        price_ticks: i32,
        raw_size_taker_units: i64,
        min_taker_units: i64,
    },
    /// Neither floor nor ceil size produces a maker amount in the band
    /// `[min_maker, target + max_overrun]`.
    NoValidSizeWithinNotionalBounds {
        price_ticks: i32,
        target_cents: i64,
        max_allowed_cents: i64,
        min_maker_cents: i64,
        floor: Option<(i64, i64)>, // (size_taker, maker_cents)
        ceil: Option<(i64, i64)>,  // (size_taker, maker_cents)
    },
}

impl std::fmt::Display for BuyCanonicalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuyCanonicalError::PriceOutOfRange { ticks } => {
                write!(f, "price_out_of_range ticks={ticks}")
            }
            BuyCanonicalError::RawSizeBelowMin {
                price_ticks,
                raw_size_taker_units,
                min_taker_units,
            } => write!(
                f,
                "buy_raw_size_below_min_size price_ticks={price_ticks} raw_taker={raw_size_taker_units} min_taker={min_taker_units}"
            ),
            BuyCanonicalError::NoValidSizeWithinNotionalBounds {
                price_ticks,
                target_cents,
                max_allowed_cents,
                min_maker_cents,
                floor,
                ceil,
            } => write!(
                f,
                "no_valid_buy_size_within_notional_bounds price_ticks={price_ticks} target_cents={target_cents} max_allowed_cents={max_allowed_cents} min_maker_cents={min_maker_cents} floor={floor:?} ceil={ceil:?}"
            ),
        }
    }
}

impl std::error::Error for BuyCanonicalError {}

/// Inputs for `canonical_buy_target_for_notional` — kept as a struct so the
/// call site is self-documenting and we don't accidentally swap two i64s
/// at a call boundary.
#[derive(Clone, Copy, Debug)]
pub struct BuyCanonicalInput {
    pub price: PriceTick,
    pub target_maker_cents: i64,
    pub min_size_taker_units: i64,
    pub min_maker_cents: i64,
    pub max_overrun_cents: i64,
    pub max_overrun_bps: i64,
}

/// Choose a venue-valid BUY (size, maker) for a target notional.
///
/// Choose a FAK BUY lattice point:
/// 1. Caller supplies a tick-aligned `PriceTick`.
/// 2. Compute the smallest 0.01-share raw size covering target notional.
/// 3. Snap to the maker-amount lattice so the signed body has exact cents.
/// 4. Prefer the smallest in-band overshoot; otherwise accept in-band floor.
/// 5. Reject locally if no venue-valid size satisfies the risk band.
///
/// `max_overrun_cents` and `max_overrun_bps` combine via `max(...)` to match
/// the Python policy: `max_allowed = target + max(absolute, target * bps/10_000)`.
pub fn canonical_buy_target_for_notional(
    inp: BuyCanonicalInput,
) -> Result<BuyCanonicalTarget, BuyCanonicalError> {
    let price_ticks = inp.price.ticks();
    if !(MIN_PRICE_TICK..=MAX_PRICE_TICK).contains(&price_ticks) {
        return Err(BuyCanonicalError::PriceOutOfRange { ticks: price_ticks });
    }

    // raw_size in 0.01-share precision, ceiling division.
    //   shares = target_dollars / price_dollars
    //          = target_cents / price_ticks      (cents and ticks each x100)
    //   in 0.01-share units = ceil(target_cents * 100 / price_ticks)
    //   in taker-units (0.0001-share) = that * 100.
    let target_cents = inp.target_maker_cents;
    let numer = target_cents
        .checked_mul(100)
        .expect("target_cents * 100 overflow");
    let denom = price_ticks as i64;
    let raw_in_0_01 = numer.div_euclid(denom) + if numer.rem_euclid(denom) > 0 { 1 } else { 0 };
    let raw_size_taker_units = raw_in_0_01.checked_mul(100).expect("raw size overflow");

    if raw_size_taker_units < inp.min_size_taker_units {
        return Err(BuyCanonicalError::RawSizeBelowMin {
            price_ticks,
            raw_size_taker_units,
            min_taker_units: inp.min_size_taker_units,
        });
    }

    let mult = buy_size_multiple_taker_units(price_ticks);
    let floor_size = floor_to_multiple(raw_size_taker_units, mult);
    let ceil_size = ceil_to_multiple(raw_size_taker_units, mult);

    let floor_maker = maker_cents_for(price_ticks, floor_size);
    let ceil_maker = maker_cents_for(price_ticks, ceil_size);

    // Python: `max_allowed_maker = target_usdc + max(max_notional_overrun, target * bps/10_000)`.
    let bps_overrun_cents = (target_cents.saturating_mul(inp.max_overrun_bps)) / 10_000;
    let abs_overrun_cents = inp.max_overrun_cents;
    let max_allowed_cents = target_cents + abs_overrun_cents.max(bps_overrun_cents);

    let in_band = |maker: UsdcCents| -> bool {
        maker.cents() >= inp.min_maker_cents && maker.cents() <= max_allowed_cents
    };

    // Prefer ceil (slight overshoot) if within bounds.
    if let Some(maker) = ceil_maker
        && in_band(maker)
        && ceil_size >= inp.min_size_taker_units
    {
        return Ok(BuyCanonicalTarget {
            price: inp.price,
            size: Shares4::new_unchecked(ceil_size),
            maker_amount: maker,
            raw_size_taker_units,
            policy: BuyCanonicalPolicy::Ceil,
        });
    }

    // Otherwise accept floor (slight undershoot).
    if let Some(maker) = floor_maker
        && in_band(maker)
        && floor_size >= inp.min_size_taker_units
    {
        return Ok(BuyCanonicalTarget {
            price: inp.price,
            size: Shares4::new_unchecked(floor_size),
            maker_amount: maker,
            raw_size_taker_units,
            policy: BuyCanonicalPolicy::Floor,
        });
    }

    Err(BuyCanonicalError::NoValidSizeWithinNotionalBounds {
        price_ticks,
        target_cents,
        max_allowed_cents,
        min_maker_cents: inp.min_maker_cents,
        floor: floor_maker.map(|m| (floor_size, m.cents())),
        ceil: ceil_maker.map(|m| (ceil_size, m.cents())),
    })
}

/// Canonicalize a SELL (price, size) pair for venue submission.
///
/// SELL mirrors Python:
/// * `q_price = floor_to_step(price, tick)` — caller passes ticks already aligned.
/// * `q_size  = floor_to_step(size, 0.01)`  — drop sub-cent dust.
///
/// Returns the (price, size) pair in canonical units. Caller is responsible
/// for verifying `q_size > 0` against actual sellable inventory.
pub fn canonical_sell_params(
    price: PriceTick,
    raw_size_taker_units: i64,
) -> Result<(PriceTick, Shares2), BuyCanonicalError> {
    let price_ticks = price.ticks();
    if !(MIN_PRICE_TICK..=MAX_PRICE_TICK).contains(&price_ticks) {
        return Err(BuyCanonicalError::PriceOutOfRange { ticks: price_ticks });
    }
    let shares2 = Shares2::floor_from_shares4(Shares4::new_unchecked(raw_size_taker_units));
    Ok((price, shares2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(target_cents: i64, price_ticks: i32, max_overrun_cents: i64) -> BuyCanonicalInput {
        BuyCanonicalInput {
            price: PriceTick::checked(price_ticks).unwrap(),
            target_maker_cents: target_cents,
            // Python default: MINIMAL_MIN_SIZE = 0.01 share = 100 taker units.
            min_size_taker_units: 100,
            // Venue floor exactly $1.00 = 100 cents (verified live probe 2026-05-07).
            min_maker_cents: 100,
            max_overrun_cents,
            max_overrun_bps: 0,
        }
    }

    #[test]
    fn p050_target_101_takes_ceil_to_102_within_overrun() {
        // p=0.50 with target=$1.01 lands at 2.02 shares and $1.01 maker
        // amount exactly.
        let r = canonical_buy_target_for_notional(input(101, 50, 1)).unwrap();
        assert_eq!(r.price.ticks(), 50);
        assert_eq!(r.size.units(), 20_200);
        assert_eq!(r.maker_amount.cents(), 101);
        assert_eq!(r.policy, BuyCanonicalPolicy::Ceil);
    }

    #[test]
    fn p051_target_101_lands_at_2_shares_dollar02() {
        // p=0.51 → multiple = 1.0 share = 10_000 taker units.
        // raw = ceil(1.01/0.51) at 0.01 step = 1.99 shares = 19_900 taker
        //   units. (Python: 1.01/0.51 = 1.9803..., ceil to 0.01 = 1.99.)
        // ceil_size = 20_000 taker units = 2.00 shares; maker = 51*20000/10000
        //   = 102 cents = $1.02. Within [100,102] band ✅.
        let r = canonical_buy_target_for_notional(input(101, 51, 1)).unwrap();
        assert_eq!(r.size.units(), 20_000);
        assert_eq!(r.maker_amount.cents(), 102);
        assert_eq!(r.policy, BuyCanonicalPolicy::Ceil);
    }

    #[test]
    fn p067_target_101_rejects_no_band_fit() {
        // Documented in docs/README.md (now removed) line ~103 of the prior
        // version. floor=$0.67 < $1.00 floor; ceil=$1.34 > $1.02 cap.
        let err = canonical_buy_target_for_notional(input(101, 67, 1)).unwrap_err();
        match err {
            BuyCanonicalError::NoValidSizeWithinNotionalBounds {
                floor: Some((fs, fm)),
                ceil: Some((cs, cm)),
                ..
            } => {
                assert_eq!((fs, fm), (10_000, 67));
                assert_eq!((cs, cm), (20_000, 134));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn p050_target_1000_lands_at_2000_cents() {
        // target=$10.00 → raw = 20.00 shares; multiple = 0.02 share. Ceil and
        // floor both 20.00 exactly. maker = $10.00 = 1000 cents.
        let r = canonical_buy_target_for_notional(input(1000, 50, 1)).unwrap();
        assert_eq!(r.size.units(), 200_000);
        assert_eq!(r.maker_amount.cents(), 1000);
    }

    #[test]
    fn raw_size_below_min_rejected() {
        // target=$0.10, p=0.50 → raw=0.20 shares = 2000 taker units. With
        // min=0.01 share = 100 taker units, this passes the min check —
        // but the band check ($0.10 maker < $1.00 min) rejects.
        let err = canonical_buy_target_for_notional(input(10, 50, 1)).unwrap_err();
        match err {
            BuyCanonicalError::NoValidSizeWithinNotionalBounds { .. } => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn sell_floors_to_2dp() {
        // 1.235 shares = 12_350 taker units → 1.23 shares.
        let (p, s) = canonical_sell_params(PriceTick::checked(50).unwrap(), 12_350).unwrap();
        assert_eq!(p.ticks(), 50);
        assert_eq!(s.units(), 123);
    }
}
