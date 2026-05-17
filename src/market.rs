//! Polymarket market-channel event parsing.
//!
//! This module produces quote updates only. It does not own inventory,
//! positions, orders, timers, or retry behavior.

use serde_json::Value;

use crate::logline::{self, Field, Level};
use crate::state::{BookDepth, BookLevel, RuntimeState};
use crate::types::{ConditionId, PriceTick, SharesAtoms, TokenId, TsUs};

const DEFAULT_TICK: PriceTick = PriceTick::new_unchecked(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuoteUpdate {
    pub token: TokenId,
    pub bid: Option<PriceTick>,
    pub ask: Option<PriceTick>,
    pub ask_depth: BookDepth,
    pub tick: PriceTick,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarketEvent {
    Quote(QuoteUpdate),
    Resolved { condition_id: Option<ConditionId> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketParseError(pub String);

impl std::fmt::Display for MarketParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid_json {}", self.0)
    }
}

impl std::error::Error for MarketParseError {}

pub fn parse_market_events(raw: &[u8]) -> Result<Vec<MarketEvent>, MarketParseError> {
    let value: Value = serde_json::from_slice(raw).map_err(|e| MarketParseError(e.to_string()))?;
    let mut out = Vec::new();
    match value {
        Value::Array(items) => {
            for item in &items {
                parse_one(item, &mut out);
            }
        }
        Value::Object(_) => parse_one(&value, &mut out),
        _ => {}
    }
    Ok(out)
}

pub fn apply_market_events(events: &[MarketEvent], state: &mut RuntimeState, ts_us: TsUs) -> usize {
    let mut applied = 0usize;
    for event in events {
        match event {
            MarketEvent::Quote(q) => {
                if state.update_quote_with_depth(
                    q.token.clone(),
                    q.bid,
                    q.ask,
                    q.ask_depth,
                    q.tick,
                    ts_us,
                ) {
                    applied += 1;
                    if logline::enabled(Level::Debug) {
                        let recv_us = ts_us.micros();
                        let side = state
                            .side_for_token(&q.token)
                            .map_or("-", |side| side.as_str());
                        let bid_ticks = q.bid.map_or(-1, |bid| bid.ticks());
                        let ask_ticks = q.ask.map_or(-1, |ask| ask.ticks());
                        logline::log_event(
                            Level::Debug,
                            "poly_quote",
                            &[
                                Field {
                                    key: "recv_us",
                                    value: &recv_us,
                                },
                                Field {
                                    key: "token_id",
                                    value: &q.token.as_str(),
                                },
                                Field {
                                    key: "side",
                                    value: &side,
                                },
                                Field {
                                    key: "bid_ticks",
                                    value: &bid_ticks,
                                },
                                Field {
                                    key: "ask_ticks",
                                    value: &ask_ticks,
                                },
                            ],
                        );
                    }
                }
            }
            MarketEvent::Resolved { condition_id } => {
                if resolution_matches_active_market(condition_id.as_ref(), state) {
                    state.mark_market_inactive();
                }
            }
        }
    }
    applied
}

fn parse_one(value: &Value, out: &mut Vec<MarketEvent>) {
    let Some(obj) = value.as_object() else {
        return;
    };
    let event_type = optional_str(value, &["event_type", "eventType", "type"]).unwrap_or_default();
    if event_type.eq_ignore_ascii_case("market_resolved") {
        out.push(MarketEvent::Resolved {
            condition_id: optional_str(value, &["market", "condition_id", "conditionId"])
                .map(ConditionId::new),
        });
        return;
    }

    if event_type.eq_ignore_ascii_case("book") {
        if let Some(update) = quote_from_book(value) {
            out.push(MarketEvent::Quote(update));
        }
        return;
    }

    if let Some(changes) = obj
        .get("price_changes")
        .or_else(|| obj.get("priceChanges"))
        .and_then(Value::as_array)
    {
        for change in changes {
            if let Some(update) = quote_from_dict(change) {
                out.push(MarketEvent::Quote(update));
            }
        }
        return;
    }

    if let Some(update) = quote_from_dict(value) {
        out.push(MarketEvent::Quote(update));
    }
}

fn quote_from_book(value: &Value) -> Option<QuoteUpdate> {
    let token = parse_token(value)?;
    let ask_depth = book_depth(value.get("asks"), false);
    let bid = best_book_price(value.get("bids"), true);
    let ask = ask_depth.best_price();
    if bid.is_none() && ask.is_none() {
        return None;
    }
    Some(QuoteUpdate {
        token,
        bid,
        ask,
        ask_depth,
        tick: parse_tick(value),
    })
}

fn quote_from_dict(value: &Value) -> Option<QuoteUpdate> {
    let token = parse_token(value)?;
    let bid = optional_price(value, &["best_bid", "bestBid", "bid"]);
    let ask = optional_price(value, &["best_ask", "bestAsk", "ask"]);
    if bid.is_none() && ask.is_none() {
        return None;
    }
    Some(QuoteUpdate {
        token,
        bid,
        ask,
        ask_depth: BookDepth::empty(),
        tick: parse_tick(value),
    })
}

fn resolution_matches_active_market(
    condition_id: Option<&ConditionId>,
    state: &RuntimeState,
) -> bool {
    let Some(condition_id) = condition_id else {
        // Can't verify which market resolved — don't kill the active one.
        return false;
    };
    state
        .market()
        .map(|m| &m.condition_id == condition_id)
        .unwrap_or(false)
}

fn parse_token(value: &Value) -> Option<TokenId> {
    optional_str(value, &["asset_id", "assetId", "token_id", "tokenId"]).map(TokenId::new)
}

fn parse_tick(value: &Value) -> PriceTick {
    optional_str(value, &["tick_size", "tickSize"])
        .and_then(|s| PriceTick::parse_decimal(&s).ok())
        .unwrap_or(DEFAULT_TICK)
}

fn optional_price(value: &Value, keys: &[&str]) -> Option<PriceTick> {
    for key in keys {
        if let Some(raw) = value.get(*key).and_then(value_to_string) {
            return PriceTick::parse_decimal(&raw).ok();
        }
    }
    None
}

fn book_depth(value: Option<&Value>, want_bid: bool) -> BookDepth {
    let Some(levels) = value.and_then(Value::as_array) else {
        return BookDepth::empty();
    };
    BookDepth::from_levels(
        levels.iter().filter_map(|level| {
            let (price, size) = parse_level(level)?;
            Some(BookLevel {
                price,
                size_atoms: size.atoms(),
            })
        }),
        want_bid,
    )
}

fn best_book_price(value: Option<&Value>, want_bid: bool) -> Option<PriceTick> {
    let levels = value?.as_array()?;
    let mut best = None;
    for level in levels {
        let Some((price, size)) = parse_level(level) else {
            continue;
        };
        if size.atoms() <= 0 {
            continue;
        }
        best = match (best, want_bid) {
            (None, _) => Some(price),
            (Some(prev), true) if price > prev => Some(price),
            (Some(prev), false) if price < prev => Some(price),
            (Some(prev), _) => Some(prev),
        };
    }
    best
}

fn parse_level(value: &Value) -> Option<(PriceTick, SharesAtoms)> {
    if let Some(arr) = value.as_array() {
        let price = arr.first().and_then(value_to_string)?;
        let size = arr.get(1).and_then(value_to_string)?;
        return Some((
            PriceTick::parse_decimal(&price).ok()?,
            SharesAtoms::parse_decimal(&size).ok()?,
        ));
    }
    let price = value.get("price").and_then(value_to_string)?;
    let size = value.get("size").and_then(value_to_string)?;
    Some((
        PriceTick::parse_decimal(&price).ok()?,
        SharesAtoms::parse_decimal(&size).ok()?,
    ))
}

fn optional_str(value: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(s) = value.get(*key).and_then(value_to_string)
            && !s.is_empty()
        {
            return Some(s);
        }
    }
    None
}

fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}
