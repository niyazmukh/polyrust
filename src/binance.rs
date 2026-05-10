//! Narrow Binance `@bookTicker` parsing.
//!
//! This module is deliberately only a parser/sample bridge. It does not own a
//! socket, reconnect policy, queue, counters, OFI, snapshots, or logging.

use serde_json::Value;

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
}

impl std::fmt::Display for BinanceParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BinanceParseError::InvalidJson(e) => write!(f, "invalid_json {e}"),
            BinanceParseError::MissingField(k) => write!(f, "missing_field {k}"),
            BinanceParseError::InvalidNumber(k) => write!(f, "invalid_number {k}"),
        }
    }
}

impl std::error::Error for BinanceParseError {}

pub fn parse_book_ticker(raw: &[u8]) -> Result<Option<BinanceBookTicker>, BinanceParseError> {
    let value: Value =
        serde_json::from_slice(raw).map_err(|e| BinanceParseError::InvalidJson(e.to_string()))?;
    let value = value.get("data").unwrap_or(&value);
    let Some(obj) = value.as_object() else {
        return Ok(None);
    };

    if let Some(event_type) = obj.get("e").and_then(Value::as_str)
        && !event_type.eq_ignore_ascii_case("bookTicker")
    {
        return Ok(None);
    }

    let has_ticker_shape = ["u", "b", "B", "a", "A"].iter().any(|k| obj.contains_key(*k));
    if !has_ticker_shape {
        return Ok(None);
    }

    let event_time = parse_positive_i64(value, "E")?;
    let update_id = parse_positive_i64(value, "u")?;
    let bid = parse_positive_f64(value, "b")?;
    let bid_qty = parse_positive_f64(value, "B")?;
    let ask = parse_positive_f64(value, "a")?;
    let ask_qty = parse_positive_f64(value, "A")?;
    if bid > ask {
        return Err(BinanceParseError::InvalidNumber("b"));
    }

    Ok(Some(BinanceBookTicker {
        event_time: TsUs(normalize_epoch_to_us(event_time)?),
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

fn parse_positive_i64(value: &Value, key: &'static str) -> Result<i64, BinanceParseError> {
    let raw = value.get(key).ok_or(BinanceParseError::MissingField(key))?;
    let n = match raw {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse::<i64>().ok(),
        _ => None,
    }
    .ok_or(BinanceParseError::InvalidNumber(key))?;
    if n > 0 {
        Ok(n)
    } else {
        Err(BinanceParseError::InvalidNumber(key))
    }
}

fn parse_positive_f64(value: &Value, key: &'static str) -> Result<f64, BinanceParseError> {
    let raw = value.get(key).ok_or(BinanceParseError::MissingField(key))?;
    let n = match raw {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse::<f64>().ok(),
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
