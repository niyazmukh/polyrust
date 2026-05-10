use minirust::config::Config;
use minirust::inventory::{TradeStatus, UserTrade};
use minirust::runtime::RuntimeCore;
use minirust::state::MarketContext;
use minirust::types::{
    ConditionId, OrderId, OrderSide, OutcomeSide, PriceTick, Shares2, SharesAtoms, TokenId, TradeId,
    TsUs,
};

fn cfg() -> Config {
    Config {
        allow_live_orders: true,
        dry_run_orders: false,
        usdc_per_trade_cents: 101,
        max_notional_overrun_cents: 1,
        max_notional_overrun_bps: 0,
        min_buy_limit_cents: 35,
        max_buy_limit_cents: 65,
        min_decision_tte_us: 2_000_000,
        max_decision_tte_us: 600_000_000,
        max_concurrent_positions: 3,
        signal_max_lag_us: 250_000,
        signal_min_window_us: 250_000,
        signal_max_window_us: 2_000_000,
        signal_max_spread_usd: 2.0,
        signal_min_abs_move_usd: 0.50,
        signal_min_abs_ofi: 1.0,
        signal_min_imbalance: 0.12,
        signal_min_total_qty: 0.000001,
        decision_min_edge_cents: 5,
        entry_slippage_cents: 1,
        prob_sigma_scale: 1.0,
        prob_sigma_floor_usd: 2.0,
        prob_floor: 0.02,
        prob_ceil: 0.98,
        signal_ring_size: 128,
    }
}

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

fn seed_core(core: &mut RuntimeCore, now: TsUs) {
    core.state_mut().set_market(market());
    core.signal_mut().set_strike(100.0, true);
    core.state_mut().update_quote(
        TokenId::new("yes"),
        Some(PriceTick::checked(45).unwrap()),
        Some(PriceTick::checked(50).unwrap()),
        PriceTick::checked(1).unwrap(),
        now,
    );
    core.state_mut().update_quote(
        TokenId::new("no"),
        Some(PriceTick::checked(45).unwrap()),
        Some(PriceTick::checked(50).unwrap()),
        PriceTick::checked(1).unwrap(),
        now,
    );
    core.signal_mut().push(
        minirust::signal::BinanceSample {
            ts_us: TsUs(1_777_000_028_000_000),
            update_id: 1,
            bid: 99.0,
            ask: 101.0,
            bid_qty: 1.0,
            ask_qty: 1.0,
            microprice: 100.0,
        },
    );
    core.signal_mut().push(
        minirust::signal::BinanceSample {
            ts_us: TsUs(1_777_000_029_000_000),
            update_id: 2,
            bid: 101.0,
            ask: 103.0,
            bid_qty: 3.0,
            ask_qty: 1.0,
            microprice: 102.5,
        },
    );
}

#[test]
fn runtime_core_binds_config_state_inventory_signal_and_buy_policy() {
    let mut core = RuntimeCore::new(&cfg()).unwrap();
    assert_eq!(core.buy_submit_policy().target_maker_cents, 101);
    assert_eq!(core.max_open_positions(), 3);

    let now = TsUs(1_777_000_030_010_000);
    seed_core(&mut core, now);

    let intent = core
        .on_binance_book_ticker(
            br#"{"E":1777000030000000,"u":3,"s":"BTCUSDT","b":"104.00","B":"3.0","a":"106.00","A":"1.0"}"#,
            now,
            60_000_000,
        )
        .unwrap()
        .unwrap();

    assert_eq!(intent.side, OutcomeSide::Yes);
    assert_eq!(intent.token, TokenId::new("yes"));
}

#[test]
fn runtime_core_uses_wss_inventory_for_duplicate_entry_block() {
    let mut core = RuntimeCore::new(&cfg()).unwrap();
    let now = TsUs(1_777_000_030_010_000);
    seed_core(&mut core, now);
    core.inventory_mut().apply_user_trade(UserTrade {
        trade_id: TradeId::new("trade-1"),
        token: TokenId::new("yes"),
        taker_order_id: Some(OrderId::new("0xorder")),
        side: OrderSide::Buy,
        size_atoms: SharesAtoms(1_000_000),
        price: PriceTick::checked(50).unwrap(),
        status: TradeStatus::Matched,
        ts_us: now.micros(),
    });

    let intent = core
        .on_binance_book_ticker(
            br#"{"E":1777000030000000,"u":3,"s":"BTCUSDT","b":"104.00","B":"3.0","a":"106.00","A":"1.0"}"#,
            now,
            60_000_000,
        )
        .unwrap();

    assert_eq!(intent, None);
}

#[test]
fn runtime_core_applies_raw_market_frames_to_state() {
    let mut core = RuntimeCore::new(&cfg()).unwrap();
    core.state_mut().set_market(market());

    let applied = core
        .apply_market_raw(
            br#"{"event_type":"price_change","asset_id":"yes","best_bid":"0.47","best_ask":"0.48","tick_size":"0.01"}"#,
            TsUs(1_777_000_031_000_000),
        )
        .unwrap();

    assert_eq!(applied, 1);
    let quote = core
        .state_mut()
        .quote_for_side(OutcomeSide::Yes)
        .copied()
        .unwrap();
    assert_eq!(quote.bid, Some(PriceTick::checked(47).unwrap()));
    assert_eq!(quote.ask, Some(PriceTick::checked(48).unwrap()));
}

#[test]
fn runtime_core_applies_raw_user_trades_to_inventory() {
    let mut core = RuntimeCore::new(&cfg()).unwrap();

    let applied = core
        .apply_user_raw(
            br#"{"event_type":"trade","trade_id":"trade-raw","asset_id":"yes","side":"BUY","size":"1.416664","price":"0.59","status":"MATCHED","order_id":"0xabc"}"#,
            1_777_000_031_000_001,
        )
        .unwrap();

    assert_eq!(applied, 1);
    let position = core.inventory_mut().position(&TokenId::new("yes")).unwrap();
    assert_eq!(position.sellable, Shares2::new_unchecked(141));
}
