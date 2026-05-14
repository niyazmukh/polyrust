use minirust::binance::{BinanceParseError, parse_book_ticker};
use minirust::signal::{SignalConfig, SignalEngine};
use minirust::state::{MarketContext, Quote};
use minirust::types::{ConditionId, OutcomeSide, PriceTick, TokenId, TsUs};

fn market() -> MarketContext {
    MarketContext {
        slug: "btc-up-down-1m".to_string(),
        condition_id: ConditionId::new("cond"),
        yes_token: TokenId::new("yes"),
        no_token: TokenId::new("no"),
        end_ts: 1_060,
        slug_ts: 1_000,
    }
}

fn quote(bid: i32, ask: i32, ts_us: i64) -> Quote {
    Quote {
        bid: Some(PriceTick::checked(bid).unwrap()),
        ask: Some(PriceTick::checked(ask).unwrap()),
        tick: PriceTick::checked(1).unwrap(),
        ts_us: TsUs(ts_us),
    }
}

fn cfg() -> SignalConfig {
    SignalConfig {
        max_lag_us: 250_000,
        min_window_us: 250_000,
        max_window_us: 2_000_000,
        max_spread_usd: 2.0,
        min_move_usd: 0.50,
        min_abs_ofi: 1.0,
        min_imbalance: 0.12,
        min_total_qty: 0.000001,
        min_edge_ticks: 5,
        entry_slippage_ticks: 1,
        max_quote_age_us: 250_000,
        min_tte_us: 2_000_000,
        min_buy_limit: PriceTick::checked(35).unwrap(),
        max_buy_limit: PriceTick::checked(65).unwrap(),
        prob_sigma_floor_usd: 2.0,
        prob_sigma_scale: 1.0,
        prob_floor: 0.02,
        prob_ceil: 0.98,
        max_samples: 128,
    }
}

#[test]
fn valid_book_ticker_parses_and_computes_microprice() {
    let raw = br#"{
        "e":"bookTicker",
        "E":1777000000000000,
        "u":400900217,
        "s":"BTCUSDT",
        "b":"100.00",
        "B":"3.0",
        "a":"101.00",
        "A":"1.0"
    }"#;

    let tick = parse_book_ticker(raw).unwrap().unwrap();

    assert_eq!(tick.event_time, TsUs(1_777_000_000_000_000));
    assert_eq!(tick.update_id, 400900217);
    assert_eq!(tick.bid, 100.0);
    assert_eq!(tick.ask, 101.0);
    assert_eq!(tick.bid_qty, 3.0);
    assert_eq!(tick.ask_qty, 1.0);
    assert_eq!(tick.microprice(), Some(100.75));
    assert_eq!(tick.sample().unwrap().microprice, 100.75);
}

#[test]
fn combined_stream_wrapper_is_unwrapped() {
    let raw = br#"{
        "stream":"btcusdt@bookTicker",
        "data":{
            "E":1777000000000000,
            "u":7,
            "s":"BTCUSDT",
            "b":"100.00",
            "B":"1.0",
            "a":"101.00",
            "A":"1.0"
        }
    }"#;

    let tick = parse_book_ticker(raw).unwrap().unwrap();

    assert_eq!(tick.update_id, 7);
    assert_eq!(tick.microprice(), Some(100.5));
}

#[test]
fn unrelated_json_returns_none() {
    let raw = br#"{"e":"trade","E":1777000000000000,"p":"100.00","q":"1.0"}"#;

    assert_eq!(parse_book_ticker(raw).unwrap(), None);
}

#[test]
fn missing_event_time_returns_zero() {
    // Binance @bookTicker JSON omits the `E` field per official docs.
    // Parser returns event_time=0; caller substitutes system time.
    let raw = br#"{
        "u":400900217,
        "s":"BTCUSDT",
        "b":"100.00",
        "B":"3.0",
        "a":"101.00",
        "A":"1.0"
    }"#;

    let tick = parse_book_ticker(raw).unwrap().unwrap();
    assert_eq!(tick.event_time, TsUs(0));
    assert_eq!(tick.update_id, 400900217);
}

#[test]
fn invalid_price_or_quantity_rejects() {
    let raw = br#"{
        "E":1777000000000000,
        "u":400900217,
        "s":"BTCUSDT",
        "b":"100.00",
        "B":"0",
        "a":"101.00",
        "A":"1.0"
    }"#;

    assert_eq!(
        parse_book_ticker(raw).unwrap_err(),
        BinanceParseError::InvalidNumber("B")
    );
}

#[test]
fn parser_output_feeds_signal_engine_to_buy_intent() {
    let mut engine = SignalEngine::new(cfg());
    engine.set_strike(100.0, true);

    for raw in [
        br#"{"E":1777000028000000,"u":1,"s":"BTCUSDT","b":"99.00","B":"1.0","a":"101.00","A":"1.0"}"#.as_slice(),
        br#"{"E":1777000029000000,"u":2,"s":"BTCUSDT","b":"101.00","B":"3.0","a":"103.00","A":"1.0"}"#.as_slice(),
    ] {
        engine.push(parse_book_ticker(raw).unwrap().unwrap().sample().unwrap());
    }

    let latest = parse_book_ticker(
        br#"{"E":1777000030000000,"u":3,"s":"BTCUSDT","b":"104.00","B":"3.0","a":"106.00","A":"1.0"}"#,
    )
    .unwrap()
    .unwrap()
    .sample()
    .unwrap();

    let intent = engine.on_sample(
        latest,
        &market(),
        quote(45, 50, latest.ts_us.micros()),
        quote(45, 50, latest.ts_us.micros()),
        TsUs(latest.ts_us.micros() + 10_000),
        60_000_000,
    );

    let intent = intent.unwrap();
    assert_eq!(intent.side, OutcomeSide::Yes);
    assert_eq!(intent.token, TokenId::new("yes"));
}
