//! Narrow Binance market-data parsing — SBE `@bestBidAsk` and JSON
//! `@bookTicker`. Auto-detects format from the first byte.
//!
//! SBE BestBidAskStreamEvent (template 10001) provides exchange-origin
//! `eventTime` in microseconds. JSON @bookTicker has no event time field
//! (per Binance docs — `E` is absent from this stream).

use crate::signal::BinanceSample;
use crate::types::TsUs;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BinanceBookTicker {
    pub event_time: TsUs,
    pub update_id: i64,
    pub bid: f64,
    pub ask: f64,
    pub bid_qty: f64,
    pub ask_qty: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinanceParseError {
    InvalidJson(String),
    MissingField(&'static str),
    InvalidNumber(&'static str),
    SbeTooShort,
    SbeBadTemplate(u16),
}

impl std::fmt::Display for BinanceParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BinanceParseError::InvalidJson(e) => write!(f, "invalid_json {e}"),
            BinanceParseError::MissingField(k) => write!(f, "missing_field {k}"),
            BinanceParseError::InvalidNumber(k) => write!(f, "invalid_number {k}"),
            BinanceParseError::SbeTooShort => write!(f, "sbe_too_short"),
            BinanceParseError::SbeBadTemplate(id) => write!(f, "sbe_bad_template id={id}"),
        }
    }
}

impl std::error::Error for BinanceParseError {}

/// Parse a single Binance market-data frame. Auto-detects SBE binary
/// (BestBidAskStreamEvent, template 10001) vs JSON (@bookTicker).
/// SBE frames start with blockLength=50 (0x32,0x00) and templateId=10001
/// (0x11,0x27 in little-endian). JSON frames start with `{` (0x7B).
pub fn parse_book_ticker(raw: &[u8]) -> Result<Option<BinanceBookTicker>, BinanceParseError> {
    if raw.len() >= 4 && raw[0] == 0x32 && raw[1] == 0x00 && raw[2] == 0x11 && raw[3] == 0x27 {
        return parse_sbe_best_bid_ask(raw);
    }
    // Fall back to JSON parser.
    parse_book_ticker_json(raw)
}

/// Parse the SBE BestBidAskStreamEvent (template 10001).
///
/// Layout (little-endian, per SBE spec):
///   Header (8 bytes):
///     [0..2]  blockLength  u16 = 50
///     [2..4]  templateId   u16 = 10001
///     [4..6]  schemaId     u16 = 1
///     [6..8]  version      u16 = 0
///   Root (50 bytes):
///     [0..8]   eventTime       i64  (microseconds UTC)
///     [8..16]  bookUpdateId    i64
///     [16]     priceExponent   i8
///     [17]     qtyExponent     i8
///     [18..26] bidPrice        i64  (mantissa × 10^priceExponent)
///     [26..34] bidQty          i64  (mantissa × 10^qtyExponent)
///     [34..42] askPrice        i64  (mantissa × 10^priceExponent)
///     [42..50] askQty          i64  (mantissa × 10^qtyExponent)
///   Variable:
///     [50]     symbol.length   u8
///     [51..]   symbol.varData  UTF-8
fn parse_sbe_best_bid_ask(raw: &[u8]) -> Result<Option<BinanceBookTicker>, BinanceParseError> {
    if raw.len() < 59 {
        return Err(BinanceParseError::SbeTooShort);
    }
    let block_length = u16::from_le_bytes([raw[0], raw[1]]);
    if block_length != 50 {
        return Ok(None);
    }
    let template_id = u16::from_le_bytes([raw[2], raw[3]]);
    if template_id != 10001 {
        return Err(BinanceParseError::SbeBadTemplate(template_id));
    }

    let base = 8usize; // past SBE header
    let event_time = i64::from_le_bytes(raw[base..base + 8].try_into().unwrap());
    let update_id = i64::from_le_bytes(raw[base + 8..base + 16].try_into().unwrap());
    let price_exp = raw[base + 16] as i8;
    let qty_exp = raw[base + 17] as i8;
    let bid_mantissa = i64::from_le_bytes(raw[base + 18..base + 26].try_into().unwrap());
    let bid_qty_mantissa = i64::from_le_bytes(raw[base + 26..base + 34].try_into().unwrap());
    let ask_mantissa = i64::from_le_bytes(raw[base + 34..base + 42].try_into().unwrap());
    let ask_qty_mantissa = i64::from_le_bytes(raw[base + 42..base + 50].try_into().unwrap());

    if event_time <= 0 || update_id <= 0 {
        return Err(BinanceParseError::InvalidNumber("eventTime"));
    }

    let bid = mantissa_to_f64(bid_mantissa, price_exp);
    let ask = mantissa_to_f64(ask_mantissa, price_exp);
    let bid_qty = mantissa_to_f64(bid_qty_mantissa, qty_exp);
    let ask_qty = mantissa_to_f64(ask_qty_mantissa, qty_exp);

    if bid <= 0.0 || ask <= 0.0 || bid_qty <= 0.0 || ask_qty <= 0.0 || bid > ask {
        return Err(BinanceParseError::InvalidNumber("bid/ask"));
    }

    Ok(Some(BinanceBookTicker {
        event_time: TsUs(event_time),
        update_id,
        bid,
        ask,
        bid_qty,
        ask_qty,
    }))
}

fn mantissa_to_f64(mantissa: i64, exponent: i8) -> f64 {
    (mantissa as f64) * 10.0_f64.powi(exponent as i32)
}

/// JSON @bookTicker parser — kept for compatibility with public streams
/// and tests. The Binance @bookTicker JSON stream has no `E` field, so
/// event_time is set to 0 and the caller substitutes system time.
fn parse_book_ticker_json(raw: &[u8]) -> Result<Option<BinanceBookTicker>, BinanceParseError> {
    let value: serde_json::Value =
        serde_json::from_slice(raw).map_err(|e| BinanceParseError::InvalidJson(e.to_string()))?;
    let value = value.get("data").unwrap_or(&value);
    let Some(obj) = value.as_object() else {
        return Ok(None);
    };

    if let Some(event_type) = obj.get("e").and_then(serde_json::Value::as_str)
        && !event_type.eq_ignore_ascii_case("bookTicker")
    {
        return Ok(None);
    }

    let has_ticker_shape = ["u", "b", "B", "a", "A"]
        .iter()
        .any(|k| obj.contains_key(*k));
    if !has_ticker_shape {
        return Ok(None);
    }

    // `E` is absent from @bookTicker per Binance docs. Use 0 — caller
    // substitutes system time.
    let event_time = match parse_positive_i64_json(value, "E") {
        Ok(et) => normalize_epoch_to_us(et)?,
        Err(BinanceParseError::MissingField(_)) => 0,
        Err(e) => return Err(e),
    };
    let update_id = parse_positive_i64_json(value, "u")?;
    let bid = parse_positive_f64_json(value, "b")?;
    let bid_qty = parse_positive_f64_json(value, "B")?;
    let ask = parse_positive_f64_json(value, "a")?;
    let ask_qty = parse_positive_f64_json(value, "A")?;
    if bid > ask {
        return Err(BinanceParseError::InvalidNumber("b"));
    }

    Ok(Some(BinanceBookTicker {
        event_time: TsUs(event_time),
        update_id,
        bid,
        ask,
        bid_qty,
        ask_qty,
    }))
}

impl BinanceBookTicker {
    pub fn microprice(&self) -> Option<f64> {
        let denom = self.bid_qty + self.ask_qty;
        if !denom.is_finite() || denom <= 0.0 {
            return None;
        }
        let px = (self.bid * self.ask_qty + self.ask * self.bid_qty) / denom;
        if px.is_finite() && px > 0.0 {
            Some(px)
        } else {
            None
        }
    }

    pub fn sample(&self) -> Option<BinanceSample> {
        Some(BinanceSample {
            ts_us: self.event_time,
            update_id: self.update_id,
            bid: self.bid,
            ask: self.ask,
            bid_qty: self.bid_qty,
            ask_qty: self.ask_qty,
            microprice: self.microprice()?,
        })
    }
}

fn parse_positive_i64_json(
    value: &serde_json::Value,
    key: &'static str,
) -> Result<i64, BinanceParseError> {
    let raw = value.get(key).ok_or(BinanceParseError::MissingField(key))?;
    let n = match raw {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => s.parse::<i64>().ok(),
        _ => None,
    }
    .ok_or(BinanceParseError::InvalidNumber(key))?;
    if n > 0 {
        Ok(n)
    } else {
        Err(BinanceParseError::InvalidNumber(key))
    }
}

fn parse_positive_f64_json(
    value: &serde_json::Value,
    key: &'static str,
) -> Result<f64, BinanceParseError> {
    let raw = value.get(key).ok_or(BinanceParseError::MissingField(key))?;
    let n = match raw {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
    .ok_or(BinanceParseError::InvalidNumber(key))?;
    if n.is_finite() && n > 0.0 {
        Ok(n)
    } else {
        Err(BinanceParseError::InvalidNumber(key))
    }
}

fn normalize_epoch_to_us(ts: i64) -> Result<i64, BinanceParseError> {
    if ts <= 0 {
        return Err(BinanceParseError::InvalidNumber("E"));
    }
    if ts < 100_000_000_000_000 {
        ts.checked_mul(1_000)
            .ok_or(BinanceParseError::InvalidNumber("E"))
    } else {
        Ok(ts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sbe_best_bid_ask_parses() {
        // Construct a valid SBE BestBidAskStreamEvent frame.
        let mut buf = Vec::new();
        // Header
        buf.extend_from_slice(&50u16.to_le_bytes()); // blockLength
        buf.extend_from_slice(&10001u16.to_le_bytes()); // templateId
        buf.extend_from_slice(&1u16.to_le_bytes()); // schemaId
        buf.extend_from_slice(&0u16.to_le_bytes()); // version
        // Root — all i64 LE
        buf.extend_from_slice(&1777000000123456i64.to_le_bytes()); // eventTime
        buf.extend_from_slice(&400900217i64.to_le_bytes()); // bookUpdateId
        buf.push((-8i8) as u8); // priceExponent
        buf.push((-8i8) as u8); // qtyExponent
        buf.extend_from_slice(&50012300000i64.to_le_bytes()); // bidPrice
        buf.extend_from_slice(&3121000000i64.to_le_bytes()); // bidQty
        buf.extend_from_slice(&50036520000i64.to_le_bytes()); // askPrice
        buf.extend_from_slice(&4066000000i64.to_le_bytes()); // askQty
        // Variable: symbol
        buf.push(7); // symbol length
        buf.extend_from_slice(b"BTCUSDT");

        let tick = parse_book_ticker(&buf).unwrap().unwrap();
        assert_eq!(tick.event_time, TsUs(1_777_000_000_123_456));
        assert_eq!(tick.update_id, 400900217);
        assert!((tick.bid - 500.123).abs() < 0.001);
        assert!((tick.ask - 500.3652).abs() < 0.001);
        assert!((tick.bid_qty - 31.21).abs() < 0.001);
        assert!((tick.ask_qty - 40.66).abs() < 0.001);
        assert!(tick.microprice().is_some());
    }
}
