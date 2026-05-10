use minirust::inventory::{Inventory, TradeStatus, UserTrade};
use minirust::runtime::on_binance_book_ticker;
use minirust::signal::{BinanceSample, SignalConfig, SignalEngine};
use minirust::state::{MarketContext, RuntimeState};
use minirust::types::{
    ConditionId, OrderId, OrderSide, OutcomeSide, PriceTick, SharesAtoms, TokenId, TradeId, TsUs,
};

fn market() -> MarketContext {
    MarketContext {
        slug: "btc-up-down-1m".to_string(),
        condition_id: ConditionId::new("cond"),
        yes_token: TokenId::new("yes"),
        no_token: TokenId::new("no"),
        yes_label: "Up".to_string(),
        no_label: "Down".to_string(),
        start_ts: 1_000,
        end_ts: 1_060,
        slug_ts: 1_000,
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
        max_tte_us: 600_000_000,
        min_buy_limit: PriceTick::checked(35).unwrap(),
        max_buy_limit: PriceTick::checked(65).unwrap(),
        prob_sigma_floor_usd: 2.0,
        prob_sigma_scale: 1.0,
        prob_floor: 0.02,
        prob_ceil: 0.98,
        max_samples: 128,
    }
}

fn active_state(now: TsUs) -> RuntimeState {
    let m = market();
    let mut state = RuntimeState::new();
    state.set_market(m);
    state.update_quote(
        TokenId::new("yes"),
        Some(PriceTick::checked(45).unwrap()),
        Some(PriceTick::checked(50).unwrap()),
        PriceTick::checked(1).unwrap(),
        now,
    );
    state.update_quote(
        TokenId::new("no"),
        Some(PriceTick::checked(45).unwrap()),
        Some(PriceTick::checked(50).unwrap()),
        PriceTick::checked(1).unwrap(),
        now,
    );
    state
}

fn seeded_signal() -> SignalEngine {
    let mut signal = SignalEngine::new(cfg());
    signal.set_strike(100.0, true);
    signal.push(BinanceSample {
        ts_us: TsUs(1_777_000_028_000_000),
        update_id: 1,
        bid: 99.0,
        ask: 101.0,
        bid_qty: 1.0,
        ask_qty: 1.0,
        microprice: 100.0,
    });
    signal.push(BinanceSample {
        ts_us: TsUs(1_777_000_029_000_000),
        update_id: 2,
        bid: 101.0,
        ask: 103.0,
        bid_qty: 3.0,
        ask_qty: 1.0,
        microprice: 102.5,
    });
    signal
}

#[test]
fn book_ticker_with_active_quotes_and_no_exposure_returns_buy_intent() {
    let now = TsUs(1_777_000_030_010_000);
    let state = active_state(now);
    let inventory = Inventory::new();
    let mut signal = seeded_signal();

    let intent = on_binance_book_ticker(
        br#"{"E":1777000030000000,"u":3,"s":"BTCUSDT","b":"104.00","B":"3.0","a":"106.00","A":"1.0"}"#,
        &mut signal,
        &state,
        &inventory,
        now,
        60_000_000,
        3,
    )
    .unwrap()
    .unwrap();

    assert_eq!(intent.side, OutcomeSide::Yes);
    assert_eq!(intent.token, TokenId::new("yes"));
    assert_eq!(intent.limit, PriceTick::checked(51).unwrap());
}

#[test]
fn same_token_wss_inventory_suppresses_duplicate_buy_intent() {
    let now = TsUs(1_777_000_030_010_000);
    let state = active_state(now);
    let mut inventory = Inventory::new();
    inventory.apply_user_trade(UserTrade {
        trade_id: TradeId::new("trade-1"),
        token: TokenId::new("yes"),
        taker_order_id: Some(OrderId::new("0xorder")),
        side: OrderSide::Buy,
        size_atoms: SharesAtoms(1_000_000),
        price: PriceTick::checked(50).unwrap(),
        status: TradeStatus::Matched,
        ts_us: now.micros(),
    });
    let mut signal = seeded_signal();

    let intent = on_binance_book_ticker(
        br#"{"E":1777000030000000,"u":3,"s":"BTCUSDT","b":"104.00","B":"3.0","a":"106.00","A":"1.0"}"#,
        &mut signal,
        &state,
        &inventory,
        now,
        60_000_000,
        3,
    )
    .unwrap();

    assert_eq!(intent, None);
}

#[test]
fn inactive_market_returns_none_without_error() {
    let now = TsUs(1_777_000_030_010_000);
    let state = RuntimeState::new();
    let inventory = Inventory::new();
    let mut signal = seeded_signal();

    let intent = on_binance_book_ticker(
        br#"{"E":1777000030000000,"u":3,"s":"BTCUSDT","b":"104.00","B":"3.0","a":"106.00","A":"1.0"}"#,
        &mut signal,
        &state,
        &inventory,
        now,
        60_000_000,
        3,
    )
    .unwrap();

    assert_eq!(intent, None);
}

#[test]
fn non_book_ticker_returns_none_without_error() {
    let now = TsUs(1_777_000_030_010_000);
    let state = active_state(now);
    let inventory = Inventory::new();
    let mut signal = seeded_signal();

    let intent = on_binance_book_ticker(
        br#"{"e":"trade","E":1777000030000000,"p":"105.00","q":"1.0"}"#,
        &mut signal,
        &state,
        &inventory,
        now,
        60_000_000,
        3,
    )
    .unwrap();

    assert_eq!(intent, None);
}
