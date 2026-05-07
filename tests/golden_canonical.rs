//! Golden BUY-canonicalization fixtures.
//!
//! Inputs and outputs are taken from the Python reference
//! `fast_order_submitter.canonical_buy_target_for_notional`. If a future
//! Polymarket precision change requires a behavioural update, **also** update
//! the Python reference and the table below in lockstep — never silently
//! diverge. The Phase-3 signing layer will rely on these (price, size,
//! maker_amount) triples being identical to the Python output.

use minirust::orders::{
    canonical_buy_target_for_notional, BuyCanonicalError, BuyCanonicalInput, BuyCanonicalPolicy,
};
use minirust::types::PriceTick;

fn input(target_cents: i64, price_ticks: i32, max_overrun_cents: i64) -> BuyCanonicalInput {
    BuyCanonicalInput {
        price: PriceTick::checked(price_ticks).unwrap(),
        target_maker_cents: target_cents,
        min_size_taker_units: 100, // 0.01 share
        min_maker_cents: 100,      // venue floor
        max_overrun_cents,
        max_overrun_bps: 0,
    }
}

#[derive(Debug)]
enum Expected {
    Ok {
        size_taker: i64,
        maker_cents: i64,
        policy: BuyCanonicalPolicy,
    },
    Err,
}

fn golden_table() -> Vec<(&'static str, BuyCanonicalInput, Expected)> {
    use BuyCanonicalPolicy::*;
    vec![
        // Tight-band $1.01 setting (current production).
        (
            "p=0.50 target=$1.01 -> ceil 2.02 sh = $1.01",
            input(101, 50, 1),
            Expected::Ok {
                size_taker: 20_200,
                maker_cents: 101,
                policy: Ceil,
            },
        ),
        (
            "p=0.51 target=$1.01 -> ceil 2.00 sh = $1.02",
            input(101, 51, 1),
            Expected::Ok {
                size_taker: 20_000,
                maker_cents: 102,
                policy: Ceil,
            },
        ),
        (
            "p=0.67 target=$1.01 -> reject (no band fit)",
            input(101, 67, 1),
            Expected::Err,
        ),
        (
            "p=0.55 target=$1.01 -> reject (no band fit)",
            input(101, 55, 1),
            Expected::Err,
        ),
        (
            "p=0.60 target=$1.01 -> ceil 1.70 sh = $1.02",
            input(101, 60, 1),
            Expected::Ok {
                size_taker: 17_000,
                maker_cents: 102,
                policy: Ceil,
            },
        ),
        // Larger budget — lattice rejection disappears.
        (
            "p=0.50 target=$10.00 -> 20.0 sh = $10.00",
            input(1000, 50, 1),
            Expected::Ok {
                size_taker: 200_000,
                maker_cents: 1000,
                policy: Ceil,
            },
        ),
        (
            "p=0.67 target=$10.00 -> ceil 15.0 sh = $10.05",
            input(1000, 67, 5),
            Expected::Ok {
                size_taker: 150_000,
                maker_cents: 1005,
                policy: Ceil,
            },
        ),
        // Exact floor — venue accepts $1.00 boundary inclusive.
        (
            "p=0.50 target=$1.00 -> 2.00 sh = $1.00",
            input(100, 50, 1),
            Expected::Ok {
                size_taker: 20_000,
                maker_cents: 100,
                policy: Ceil,
            },
        ),
        // Below-floor: too small to meet $1.00 even at ceil.
        (
            "p=0.50 target=$0.10 -> reject (below venue floor)",
            input(10, 50, 1),
            Expected::Err,
        ),
    ]
}

#[test]
fn golden_buy_canonical_matches_python() {
    for (label, inp, expect) in golden_table() {
        let got = canonical_buy_target_for_notional(inp);
        match (got, &expect) {
            (
                Ok(t),
                Expected::Ok {
                    size_taker,
                    maker_cents,
                    policy,
                },
            ) => {
                assert_eq!(t.size.units(), *size_taker, "{label}: size_taker mismatch");
                assert_eq!(
                    t.maker_amount.cents(),
                    *maker_cents,
                    "{label}: maker_cents mismatch"
                );
                assert_eq!(t.policy, *policy, "{label}: policy mismatch");
            }
            (Err(BuyCanonicalError::NoValidSizeWithinNotionalBounds { .. }), Expected::Err) => {}
            (Err(other), Expected::Err) => panic!("{label}: unexpected err variant {other:?}"),
            (Ok(t), Expected::Err) => panic!("{label}: expected reject, got {t:?}"),
            (Err(e), Expected::Ok { .. }) => panic!("{label}: expected ok, got {e:?}"),
        }
    }
}
