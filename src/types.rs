//! Fixed-point integer newtypes for venue-facing math.
//!
//! Venue precision:
//! * `PRICE_TICK`        = 0.01    (cents per share-price)
//! * `SIZE_STEP`         = 0.01    (share quantum for SELL)
//! * `MAKER_AMOUNT_STEP` = 0.01    (USDC quantum in maker amount)
//! * `TAKER_AMOUNT_STEP` = 0.0001  (share quantum inside the signed body)
//! * `TOKEN_AMOUNT_SCALE`= 1_000_000  (atoms per dollar / share in the body)
//!
//! We carry every venue-facing value as an integer in its native step.
//! `f64` is allowed only inside probability/volatility math (`signal.rs`,
//! later); it must never cross into a signed body without an explicit
//! checked constructor.
//!
//! Shape conventions:
//! * `PriceTick(i32)`   — number of $0.01 ticks. Range gate \[1, 99\].
//! * `Shares4(i64)`     — number of 0.0001-share units (signed-body taker step).
//! * `Shares2(i64)`     — number of 0.01-share units (SELL maker step).
//! * `UsdcCents(i64)`   — number of 1¢ units. BUY maker step.
//! * `SharesAtoms(i64)` — number of 1e-6 share units (signed-body amount).

use std::fmt;

/// Number of $0.01 ticks per dollar.
pub const PRICE_TICKS_PER_DOLLAR: i32 = 100;
/// Atoms-per-dollar (and atoms-per-share) used in signed body amounts.
pub const ATOMS_PER_DOLLAR: i64 = 1_000_000;

/// Lowest tradeable price in ticks ($0.01).
pub const MIN_PRICE_TICK: i32 = 1;
/// Highest tradeable price in ticks ($0.99). Polymarket binary tokens cap at $0.99.
pub const MAX_PRICE_TICK: i32 = 99;

// ------------------------------------------------------------------
// PriceTick
// ------------------------------------------------------------------

/// Integer ticks of $0.01. Range \[1, 99\].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PriceTick(pub i32);

impl PriceTick {
    /// Construct without range check. Use only when caller has just validated.
    pub const fn new_unchecked(ticks: i32) -> Self {
        Self(ticks)
    }

    /// Construct with range check.
    pub fn checked(ticks: i32) -> Result<Self, TypeError> {
        if (MIN_PRICE_TICK..=MAX_PRICE_TICK).contains(&ticks) {
            Ok(Self(ticks))
        } else {
            Err(TypeError::PriceOutOfRange { ticks })
        }
    }

    pub const fn ticks(self) -> i32 {
        self.0
    }

    /// Parse a venue decimal like "0.59" into a 1-cent tick.
    /// Fails closed on sub-cent precision instead of rounding.
    pub fn parse_decimal(raw: &str) -> Result<Self, TypeError> {
        let ticks = parse_decimal_scaled(raw, PRICE_TICKS_PER_DOLLAR as i64)
            .ok_or(TypeError::InvalidDecimal)?;
        let ticks = i32::try_from(ticks).map_err(|_| TypeError::PriceOutOfRange { ticks: -1 })?;
        Self::checked(ticks)
    }
}

impl fmt::Display for PriceTick {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Render as e.g. 0.50 — never as a float to avoid round-trip noise.
        let cents = self.0;
        write!(f, "{}.{:02}", cents / 100, cents % 100)
    }
}

// ------------------------------------------------------------------
// Shares4 — taker-step share count (0.0001-share units)
// ------------------------------------------------------------------

/// Share count in 0.0001-share units, the precision required by the V2
/// signed-body taker amount.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Shares4(pub i64);

impl Shares4 {
    pub const fn new_unchecked(units: i64) -> Self {
        Self(units)
    }

    pub const fn units(self) -> i64 {
        self.0
    }
}

// ------------------------------------------------------------------
// Shares2 — SELL maker-step share count (0.01-share units)
// ------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Shares2(pub i64);

impl Shares2 {
    pub const fn new_unchecked(units: i64) -> Self {
        Self(units)
    }

    /// Floor a 4-dp share count to the 2-dp SELL quantum.
    pub fn floor_from_shares4(shares4: Shares4) -> Self {
        Self(shares4.0.div_euclid(100))
    }

    pub const fn units(self) -> i64 {
        self.0
    }
}

// ------------------------------------------------------------------
// UsdcCents — BUY maker-step amount (1¢ units)
// ------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UsdcCents(pub i64);

impl UsdcCents {
    pub const fn cents(self) -> i64 {
        self.0
    }
}

impl fmt::Display for UsdcCents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let neg = self.0 < 0;
        let v = self.0.unsigned_abs() as i64;
        let dollars = v / 100;
        let cents = v % 100;
        if neg {
            write!(f, "-{}.{:02}", dollars, cents)
        } else {
            write!(f, "{}.{:02}", dollars, cents)
        }
    }
}

// ------------------------------------------------------------------
// Body-atom amounts (1e-6 unit). These cross the signed-body boundary.
// ------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SharesAtoms(pub i64);

impl SharesAtoms {
    pub const fn atoms(self) -> i64 {
        self.0
    }

    /// Parse a venue/user-WSS share decimal into 1e-6 share atoms.
    /// Fails closed on sub-atom precision instead of rounding.
    pub fn parse_decimal(raw: &str) -> Result<Self, TypeError> {
        let atoms = parse_decimal_scaled(raw, ATOMS_PER_DOLLAR).ok_or(TypeError::InvalidDecimal)?;
        if atoms < 0 {
            return Err(TypeError::NegativeAmount);
        }
        Ok(Self(atoms))
    }
}

impl fmt::Display for SharesAtoms {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ------------------------------------------------------------------
// Identifiers
// ------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TokenId(String);

impl TokenId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ConditionId(String);

impl ConditionId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OrderId(String);

impl OrderId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TradeId(String);

impl TradeId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// ------------------------------------------------------------------
// Sides
// ------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OrderSide {
    Buy,
    Sell,
}

impl OrderSide {
    pub fn as_str(self) -> &'static str {
        match self {
            OrderSide::Buy => "BUY",
            OrderSide::Sell => "SELL",
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            OrderSide::Buy => 0,
            OrderSide::Sell => 1,
        }
    }
}

/// "YES" / "NO" leg of a binary outcome market.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OutcomeSide {
    Yes,
    No,
}

impl OutcomeSide {
    pub fn as_str(self) -> &'static str {
        match self {
            OutcomeSide::Yes => "YES",
            OutcomeSide::No => "NO",
        }
    }
}

// ------------------------------------------------------------------
// Time
// ------------------------------------------------------------------

/// Microsecond timestamp, monotonic where possible.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TsUs(pub i64);

impl TsUs {
    pub const fn micros(self) -> i64 {
        self.0
    }
}

// ------------------------------------------------------------------
// Errors
// ------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeError {
    PriceOutOfRange { ticks: i32 },
    InvalidDecimal,
    NegativeAmount,
}

impl fmt::Display for TypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeError::PriceOutOfRange { ticks } => {
                write!(f, "price_out_of_range ticks={ticks}")
            }
            TypeError::InvalidDecimal => write!(f, "invalid_decimal"),
            TypeError::NegativeAmount => write!(f, "negative_amount"),
        }
    }
}

impl std::error::Error for TypeError {}

// ------------------------------------------------------------------
// Pure math helpers used by `orders.rs`.
// ------------------------------------------------------------------

/// Greatest common divisor for non-negative i64.
pub const fn gcd(mut a: i64, mut b: i64) -> i64 {
    if a < 0 {
        a = -a;
    }
    if b < 0 {
        b = -b;
    }
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

/// Smallest taker-units multiple for which `price_ticks * units / 10_000`
/// produces an integer cent count (i.e., `price * size` is aligned to the
/// 0.01 USDC maker step).
///
/// With `price_step=0.01`, `taker_step=0.0001`, `maker_step=0.01`,
/// the denominator is fixed at 10_000.
pub const fn buy_size_multiple_taker_units(price_ticks: i32) -> i64 {
    let denom: i64 = 10_000;
    let g = gcd(price_ticks as i64, denom);
    denom / g
}

/// Floor `raw_taker_units` to the nearest multiple of `mult_taker_units`.
pub const fn floor_to_multiple(raw_taker_units: i64, mult_taker_units: i64) -> i64 {
    if mult_taker_units <= 0 {
        return raw_taker_units;
    }
    (raw_taker_units / mult_taker_units) * mult_taker_units
}

/// Ceiling `raw_taker_units` to the nearest multiple of `mult_taker_units`.
pub const fn ceil_to_multiple(raw_taker_units: i64, mult_taker_units: i64) -> i64 {
    if mult_taker_units <= 0 {
        return raw_taker_units;
    }
    let r = raw_taker_units % mult_taker_units;
    if r == 0 {
        raw_taker_units
    } else {
        raw_taker_units + (mult_taker_units - r)
    }
}

/// Compute the maker amount in 1¢ units for a given price (ticks) and size
/// (taker units). Returns `None` if `price_ticks * size_taker` is not aligned
/// to the maker step (i.e., not divisible by 10_000) — that condition means
/// the body would fail venue precision validation.
pub fn maker_cents_for(price_ticks: i32, size_taker_units: i64) -> Option<UsdcCents> {
    let product = (price_ticks as i64).checked_mul(size_taker_units)?;
    if product % 10_000 != 0 {
        return None;
    }
    Some(UsdcCents(product / 10_000))
}

fn parse_decimal_scaled(raw: &str, scale: i64) -> Option<i64> {
    let raw = raw.trim();
    if raw.is_empty() || scale <= 0 {
        return None;
    }
    let (sign, body) = match raw.as_bytes()[0] {
        b'-' => (-1i64, &raw[1..]),
        b'+' => (1i64, &raw[1..]),
        _ => (1i64, raw),
    };
    let (whole_str, frac_str) = match body.find('.') {
        Some(i) => (&body[..i], &body[i + 1..]),
        None => (body, ""),
    };
    let whole = if whole_str.is_empty() {
        0
    } else {
        whole_str.parse::<i64>().ok()?
    };
    let scale_digits = decimal_scale_digits(scale)?;
    if frac_str.len() > scale_digits && !frac_str[scale_digits..].chars().all(|c| c == '0') {
        return None;
    }
    let mut frac_padded = String::with_capacity(scale_digits);
    let keep = frac_str.len().min(scale_digits);
    frac_padded.push_str(&frac_str[..keep]);
    while frac_padded.len() < scale_digits {
        frac_padded.push('0');
    }
    let frac = if frac_padded.is_empty() {
        0
    } else {
        frac_padded.parse::<i64>().ok()?
    };
    Some(sign * (whole.checked_mul(scale)? + frac))
}

fn decimal_scale_digits(scale: i64) -> Option<usize> {
    let mut n = scale;
    let mut digits = 0usize;
    while n > 1 {
        if n % 10 != 0 {
            return None;
        }
        n /= 10;
        digits += 1;
    }
    Some(digits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gcd_basics() {
        assert_eq!(gcd(0, 0), 0);
        assert_eq!(gcd(50, 10_000), 50);
        assert_eq!(gcd(67, 10_000), 1);
        assert_eq!(gcd(40, 10_000), 40);
    }

    #[test]
    fn multiples_match_venue_precision_lattice() {
        // Venue precision lattice with default steps.
        // p=0.50  -> multiple=200 taker units (=0.02 shares)
        // p=0.51  -> multiple=10000 taker units (=1.00 shares)
        // p=0.67  -> multiple=10000 taker units (=1.00 shares)
        // p=0.40  -> multiple=250 taker units (=0.025 shares)
        assert_eq!(buy_size_multiple_taker_units(50), 200);
        assert_eq!(buy_size_multiple_taker_units(51), 10_000);
        assert_eq!(buy_size_multiple_taker_units(67), 10_000);
        assert_eq!(buy_size_multiple_taker_units(40), 250);
    }

    #[test]
    fn maker_cents_alignment() {
        // p=0.50, size=2.02 shares = 20_200 taker units. 50*20200 = 1_010_000.
        // / 10_000 = 101 cents = $1.01. Aligned.
        assert_eq!(maker_cents_for(50, 20_200), Some(UsdcCents(101)));
        // p=0.50, size=2.025 shares = 20_250 taker units. 50*20250 = 1_012_500.
        // not divisible by 10_000 -> not aligned.
        assert_eq!(maker_cents_for(50, 20_250), None);
        // p=0.67, size=1.0 shares = 10_000 taker units. 67*10000 = 670_000.
        // / 10_000 = 67 cents = $0.67.
        assert_eq!(maker_cents_for(67, 10_000), Some(UsdcCents(67)));
    }

    #[test]
    fn pricetick_display() {
        assert_eq!(format!("{}", PriceTick(50)), "0.50");
        assert_eq!(format!("{}", PriceTick(7)), "0.07");
        assert_eq!(format!("{}", PriceTick(99)), "0.99");
    }

    #[test]
    fn pricetick_range() {
        assert!(PriceTick::checked(0).is_err());
        assert!(PriceTick::checked(100).is_err());
        assert!(PriceTick::checked(1).is_ok());
        assert!(PriceTick::checked(99).is_ok());
    }

    #[test]
    fn decimal_parsers_fail_closed() {
        assert_eq!(PriceTick::parse_decimal("0.59"), Ok(PriceTick(59)));
        assert!(PriceTick::parse_decimal("0.591").is_err());
        assert_eq!(
            SharesAtoms::parse_decimal("1.416664"),
            Ok(SharesAtoms(1_416_664))
        );
        assert!(SharesAtoms::parse_decimal("1.4166641").is_err());
    }

    #[test]
    fn floor_ceil_to_multiple_basics() {
        // multiple=10_000 (p=0.51): 15_100 floors to 10_000, ceils to 20_000.
        assert_eq!(floor_to_multiple(15_100, 10_000), 10_000);
        assert_eq!(ceil_to_multiple(15_100, 10_000), 20_000);
        // exact: 20_000 floors and ceils to 20_000.
        assert_eq!(floor_to_multiple(20_000, 10_000), 20_000);
        assert_eq!(ceil_to_multiple(20_000, 10_000), 20_000);
    }
}
