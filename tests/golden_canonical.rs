//! Golden BUY-canonicalization fixtures.
//!
//! These are venue-body precision fixtures from live-proven Python behavior.
//! They are temporary oracles for the signed-body lattice only, not a mandate
//! to recreate the Python runtime graph.

use minirust::orders::{BuyCanonicalError, BuyCanonicalInput, canonical_buy_target_for_notional};
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
    Ok { size_taker: i64, maker_cents: i64 },
    Err,
}

fn golden_table() -> Vec<(&'static str, BuyCanonicalInput, Expected)> {
    vec![
        // Tight-band $1.01 setting (current production).
        (
            "p=0.50 target=$1.01 -> ceil 2.02 sh = $1.01",
            input(101, 50, 1),
            Expected::Ok {
                size_taker: 20_200,
                maker_cents: 101,
            },
        ),
        (
            "p=0.51 target=$1.01 -> ceil 2.00 sh = $1.02",
            input(101, 51, 1),
            Expected::Ok {
                size_taker: 20_000,
                maker_cents: 102,
            },
        ),
        (
            "p=0.67 target=$1.01 -> lattice gap, accept ceil $1.34",
            input(101, 67, 1),
            Expected::Ok {
                size_taker: 20_000,
                maker_cents: 134,
            },
        ),
        (
            "p=0.55 target=$1.01 -> lattice gap, accept ceil $1.10",
            input(101, 55, 1),
            Expected::Ok {
                size_taker: 20_000,
                maker_cents: 110,
            },
        ),
        (
            "p=0.60 target=$1.01 -> ceil 1.70 sh = $1.02",
            input(101, 60, 1),
            Expected::Ok {
                size_taker: 17_000,
                maker_cents: 102,
            },
        ),
        // Larger budget — lattice rejection disappears.
        (
            "p=0.50 target=$10.00 -> 20.0 sh = $10.00",
            input(1000, 50, 1),
            Expected::Ok {
                size_taker: 200_000,
                maker_cents: 1000,
            },
        ),
        (
            "p=0.67 target=$10.00 -> ceil 15.0 sh = $10.05",
            input(1000, 67, 5),
            Expected::Ok {
                size_taker: 150_000,
                maker_cents: 1005,
            },
        ),
        // Exact floor — venue accepts $1.00 boundary inclusive.
        (
            "p=0.50 target=$1.00 -> 2.00 sh = $1.00",
            input(100, 50, 1),
            Expected::Ok {
                size_taker: 20_000,
                maker_cents: 100,
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
                },
            ) => {
                assert_eq!(t.size.units(), *size_taker, "{label}: size_taker mismatch");
                assert_eq!(
                    t.maker_amount.cents(),
                    *maker_cents,
                    "{label}: maker_cents mismatch"
                );
            }
            (Err(BuyCanonicalError::NoValidSizeWithinNotionalBounds { .. }), Expected::Err) => {}
            (Err(other), Expected::Err) => panic!("{label}: unexpected err variant {other:?}"),
            (Ok(t), Expected::Err) => panic!("{label}: expected reject, got {t:?}"),
            (Err(e), Expected::Ok { .. }) => panic!("{label}: expected ok, got {e:?}"),
        }
    }
}
