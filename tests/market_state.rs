use minirust::market::{apply_market_events, parse_market_events};
use minirust::state::{MarketContext, RuntimeState};
use minirust::types::{ConditionId, OutcomeSide, PriceTick, TokenId, TsUs};

fn context() -> MarketContext {
    MarketContext {
        slug: "btc-up-down-1m".to_string(),
        condition_id: ConditionId::new("cond-1"),
        yes_token: TokenId::new("yes-token"),
        no_token: TokenId::new("no-token"),
        end_ts: 1_060,
        slug_ts: 1_000,
    }
}

#[test]
fn book_quote_updates_active_market_and_resolved_clears_it() {
    let mut state = RuntimeState::new();
    state.set_market(context());

    let raw = br#"{
        "event_type":"book",
        "asset_id":"yes-token",
        "bids":[["0.57","0"],["0.58","12.50"],["0.56","1.0"]],
        "asks":[["0.61","1.0"],["0.60","0"],["0.62","2.0"]],
        "tick_size":"0.01"
    }"#;
    let events = parse_market_events(raw).unwrap();
    assert_eq!(apply_market_events(&events, &mut state, TsUs(10_000)), 1);

    let quote = state.quote_for_side(OutcomeSide::Yes).unwrap();
    assert_eq!(quote.bid, Some(PriceTick::checked(58).unwrap()));
    assert_eq!(quote.ask, Some(PriceTick::checked(61).unwrap()));
    assert_eq!(
        quote.buy_cutoff_for_cents(101),
        Some(PriceTick::checked(62).unwrap())
    );

    let resolved =
        parse_market_events(br#"{"event_type":"market_resolved","condition_id":"cond-1"}"#)
            .unwrap();
    assert_eq!(apply_market_events(&resolved, &mut state, TsUs(11_000)), 0);
    assert!(!state.trading_active());
    assert!(state.quote_for_side(OutcomeSide::Yes).is_none());
}

#[test]
fn price_change_requires_executable_bid_or_ask_and_active_token() {
    let mut state = RuntimeState::new();
    state.set_market(context());

    let raw = br#"{
        "event_type":"price_change",
        "price_changes":[
            {"asset_id":"yes-token","price":"0.63"},
            {"asset_id":"old-token","best_bid":"0.10","best_ask":"0.11"},
            {"asset_id":"no-token","best_bid":"0.42","best_ask":"0.44"}
        ]
    }"#;
    let events = parse_market_events(raw).unwrap();
    assert_eq!(apply_market_events(&events, &mut state, TsUs(20_000)), 1);

    assert!(state.quote_for_side(OutcomeSide::Yes).is_none());
    let quote = state.quote_for_side(OutcomeSide::No).unwrap();
    assert_eq!(quote.bid, Some(PriceTick::checked(42).unwrap()));
    assert_eq!(quote.ask, Some(PriceTick::checked(44).unwrap()));
}
