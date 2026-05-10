//! Polymarket user-channel parsing.
//!
//! The parser is deliberately narrow: it extracts trade events into the
//! inventory model and ignores non-trade messages. It does not own inventory
//! and it does not create a second accounting path.

use serde_json::Value;

use crate::inventory::{TradeStatus, UserTrade};
use crate::types::{OrderId, OrderSide, PriceTick, SharesAtoms, TokenId, TradeId, TypeError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserParseError {
    InvalidJson(String),
    MissingField(&'static str),
    InvalidSide(String),
    InvalidPrice(TypeError),
    InvalidSize(TypeError),
}

impl std::fmt::Display for UserParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UserParseError::InvalidJson(e) => write!(f, "invalid_json: {e}"),
            UserParseError::MissingField(name) => write!(f, "missing_field: {name}"),
            UserParseError::InvalidSide(s) => write!(f, "invalid_side: {s}"),
            UserParseError::InvalidPrice(e) => write!(f, "invalid_price: {e}"),
            UserParseError::InvalidSize(e) => write!(f, "invalid_size: {e}"),
        }
    }
}

impl std::error::Error for UserParseError {}

pub fn parse_user_trades(raw: &[u8], ts_us: i64) -> Result<Vec<UserTrade>, UserParseError> {
    let value: Value =
        serde_json::from_slice(raw).map_err(|e| UserParseError::InvalidJson(format!("{e}")))?;
    let mut out = Vec::new();
    match value {
        Value::Array(items) => {
            for item in items {
                if let Some(trade) = parse_trade_value(&item, ts_us)? {
                    out.push(trade);
                }
            }
        }
        other => {
            if let Some(trade) = parse_trade_value(&other, ts_us)? {
                out.push(trade);
            }
        }
    }
    Ok(out)
}

fn parse_trade_value(value: &Value, fallback_ts_us: i64) -> Result<Option<UserTrade>, UserParseError> {
    if !is_trade_event(value) {
        return Ok(None);
    }
    let trade_id = required_str(value, &["trade_id", "tradeId", "id"], "trade_id")?;
    let token = required_str(value, &["asset_id", "assetId", "token_id", "tokenId"], "asset_id")?;
    let side = parse_side(required_str(value, &["side"], "side")?)?;
    let size_raw = required_str(value, &["size", "amount"], "size")?;
    let price_raw = required_str(value, &["price"], "price")?;
    let status_raw = optional_str(value, &["status"]).unwrap_or("MATCHED");
    let ts_us = optional_i64(value, &["ts_us", "timestamp_us", "timestampUs"]).unwrap_or(fallback_ts_us);

    Ok(Some(UserTrade {
        trade_id: TradeId::new(trade_id),
        token: TokenId::new(token),
        taker_order_id: optional_str(
            value,
            &["taker_order_id", "takerOrderId", "order_id", "orderId", "orderID"],
        )
        .map(OrderId::new),
        side,
        size_atoms: SharesAtoms::parse_decimal(size_raw).map_err(UserParseError::InvalidSize)?,
        price: PriceTick::parse_decimal(price_raw).map_err(UserParseError::InvalidPrice)?,
        status: TradeStatus::from_venue(status_raw),
        ts_us,
    }))
}

fn is_trade_event(value: &Value) -> bool {
    optional_str(value, &["event_type", "eventType", "type"])
        .is_some_and(|s| s.eq_ignore_ascii_case("trade"))
}

fn parse_side(raw: &str) -> Result<OrderSide, UserParseError> {
    match raw.trim().to_ascii_uppercase().as_str() {
        "BUY" => Ok(OrderSide::Buy),
        "SELL" => Ok(OrderSide::Sell),
        _ => Err(UserParseError::InvalidSide(raw.to_owned())),
    }
}

fn required_str<'a>(
    value: &'a Value,
    keys: &[&'static str],
    name: &'static str,
) -> Result<&'a str, UserParseError> {
    optional_str(value, keys).ok_or(UserParseError::MissingField(name))
}

fn optional_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())
}

fn optional_i64(value: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|key| {
        let v = value.get(*key)?;
        v.as_i64().or_else(|| v.as_str()?.parse::<i64>().ok())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_trade() {
        let raw = br#"{
            "event_type":"trade",
            "trade_id":"tr1",
            "taker_order_id":"0xabc",
            "asset_id":"123",
            "side":"BUY",
            "size":"1.416664",
            "price":"0.59",
            "status":"MATCHED",
            "ts_us":1778087750526774
        }"#;
        let trades = parse_user_trades(raw, 1).unwrap();
        assert_eq!(trades.len(), 1);
        let t = &trades[0];
        assert_eq!(t.trade_id.as_str(), "tr1");
        assert_eq!(t.token.as_str(), "123");
        assert_eq!(t.side, OrderSide::Buy);
        assert_eq!(t.size_atoms, SharesAtoms(1_416_664));
        assert_eq!(t.price, PriceTick::checked(59).unwrap());
        assert_eq!(t.status, TradeStatus::Matched);
        assert_eq!(t.ts_us, 1_778_087_750_526_774);
    }

    #[test]
    fn parses_list_and_ignores_non_trade() {
        let raw = br#"[
            {"event_type":"order","id":"o1"},
            {"event_type":"trade","id":"tr1","asset_id":"123","side":"SELL","size":"0.010000","price":"0.87","status":"CONFIRMED"}
        ]"#;
        let trades = parse_user_trades(raw, 42).unwrap();
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].side, OrderSide::Sell);
        assert_eq!(trades[0].size_atoms, SharesAtoms(10_000));
        assert_eq!(trades[0].price, PriceTick::checked(87).unwrap());
        assert_eq!(trades[0].status, TradeStatus::Confirmed);
        assert_eq!(trades[0].ts_us, 42);
    }

    #[test]
    fn rejects_sub_atom_size_and_sub_cent_price() {
        let raw = br#"{"event_type":"trade","id":"tr1","asset_id":"123","side":"BUY","size":"1.0000001","price":"0.59"}"#;
        assert!(matches!(
            parse_user_trades(raw, 1),
            Err(UserParseError::InvalidSize(_))
        ));

        let raw = br#"{"event_type":"trade","id":"tr1","asset_id":"123","side":"BUY","size":"1.000000","price":"0.591"}"#;
        assert!(matches!(
            parse_user_trades(raw, 1),
            Err(UserParseError::InvalidPrice(_))
        ));
    }
}
