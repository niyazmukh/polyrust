//! Coverage for the execution-aware entry gates added on top of the Binance
//! fair-value signal:
//!
//! * RTT EWMA ceiling (Pillar A)
//! * Polymarket drift-up / drift-down blocks (Pillar C)
//! * Drift-buffer edge sufficiency (Pillar B)
//! * `refresh_sell_plan` repricing of FAK SELL at the latest bid (Pillar E.3)
//!
//! These tests build an active `RuntimeCore`, seed the Binance signal window
//! so a BUY intent would otherwise fire, then assert that each gate suppresses
//! the intent when its threshold is crossed and lets it through otherwise.

use minirust::binance::parse_book_ticker;
use minirust::config::Config;
use minirust::inventory::{TradeStatus, UserTrade};
use minirust::runtime::RuntimeCore;
use minirust::signal::BinanceSample;
use minirust::state::MarketContext;
use minirust::types::{
    ConditionId, OrderId, OrderSide, OutcomeSide, PriceTick, SharesAtoms, TokenId, TradeId, TsUs,
};

fn base_cfg() -> Config {
    Config {
        allow_live_orders: true,
        usdc_per_trade_cents: 101,
        max_notional_overrun_cents: 1,
        max_notional_overrun_bps: 0,
        min_buy_limit_cents: 35,
        max_buy_limit_cents: 65,
        min_decision_tte_us: 2_000_000,
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
        sell_slippage_cents: 0,
        exit_drop_ticks: 2,
        exit_arm_ticks: 2,
        exit_stop_ticks: 3,
        exit_edge_ticks: 0,
        exit_hold_us: 15_000_000,
        prob_sigma_scale: 1.0,
        prob_sigma_floor_usd: 2.0,
        prob_floor: 0.02,
        prob_ceil: 0.98,
        signal_ring_size: 128,
        signal_quote_age_us: 250_000,
        rtt_ceiling_us: 0,
        poly_drift_window_us: 0,
        poly_drift_block_up_ticks: 0,
        poly_drift_block_down_ticks: 0,
        poly_drift_safety_bps: 0,
        poly_drift_min_clean_edge_ticks: 0,
        use_implied_sigma: false,
    }
}

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

fn quote_yes_no(core: &mut RuntimeCore, yes_ask: i32, no_ask: i32, ts: TsUs) {
    core.state_mut().update_quote(
        TokenId::new("yes"),
        Some(PriceTick::checked(45).unwrap()),
        Some(PriceTick::checked(yes_ask).unwrap()),
        PriceTick::checked(1).unwrap(),
        ts,
    );
    core.state_mut().update_quote(
        TokenId::new("no"),
        Some(PriceTick::checked(45).unwrap()),
        Some(PriceTick::checked(no_ask).unwrap()),
        PriceTick::checked(1).unwrap(),
        ts,
    );
}

fn seed_signal(core: &mut RuntimeCore) {
    core.signal_mut().set_strike(100.0, true);
    core.signal_mut().push(BinanceSample {
        ts_us: TsUs(1_777_000_028_000_000),
        update_id: 1,
        bid: 99.0,
        ask: 101.0,
        bid_qty: 1.0,
        ask_qty: 1.0,
        microprice: 100.0,
    });
    core.signal_mut().push(BinanceSample {
        ts_us: TsUs(1_777_000_029_000_000),
        update_id: 2,
        bid: 101.0,
        ask: 103.0,
        bid_qty: 3.0,
        ask_qty: 1.0,
        microprice: 102.5,
    });
}

fn signal_ready_core(config: Config) -> RuntimeCore {
    let now = TsUs(1_777_000_030_010_000);
    let mut core = RuntimeCore::new(&config).unwrap();
    core.inventory_mut().set_user_wss_trusted(true);
    core.state_mut().set_market(market());
    quote_yes_no(&mut core, 50, 50, now);
    seed_signal(&mut core);
    core
}

fn ticker_sample() -> BinanceSample {
    parse_book_ticker(
        br#"{"E":1777000030000000,"u":3,"s":"BTCUSDT","b":"104.00","B":"3.0","a":"106.00","A":"1.0"}"#,
    )
    .unwrap()
    .unwrap()
    .sample()
    .unwrap()
}

#[test]
fn rtt_ewma_starts_at_first_sample_then_blends() {
    let mut core = RuntimeCore::new(&base_cfg()).unwrap();
    assert_eq!(core.current_ewma_rtt_us(), 0);

    core.record_rtt(1_000_000);
    assert_eq!(core.current_ewma_rtt_us(), 1_000_000);

    // Each new sample is folded in at weight 1/8: ewma += (sample − ewma)/8.
    // 1_000_000 + (200_000 − 1_000_000)/8 = 1_000_000 + (−100_000) = 900_000.
    core.record_rtt(200_000);
    assert_eq!(core.current_ewma_rtt_us(), 900_000);

    // Non-positive samples are ignored.
    core.record_rtt(0);
    core.record_rtt(-1);
    assert_eq!(core.current_ewma_rtt_us(), 900_000);
}

#[test]
fn entry_gate_blocks_when_rtt_ewma_exceeds_ceiling() {
    let mut config = base_cfg();
    config.rtt_ceiling_us = 800_000;
    let mut core = signal_ready_core(config);
    let now = TsUs(1_777_000_030_010_000);

    // Pin the EWMA above the ceiling.
    core.record_rtt(2_000_000);
    assert!(core.current_ewma_rtt_us() > 800_000);

    let intent = core
        .on_binance_sample(ticker_sample(), now, 60_000_000)
        .unwrap();
    assert_eq!(intent, None, "RTT EWMA above ceiling must suppress entry");
}

#[test]
fn entry_gate_passes_when_rtt_ewma_below_ceiling() {
    let mut config = base_cfg();
    config.rtt_ceiling_us = 800_000;
    let mut core = signal_ready_core(config);
    let now = TsUs(1_777_000_030_010_000);

    core.record_rtt(200_000);
    assert!(core.current_ewma_rtt_us() < 800_000);

    let intent = core
        .on_binance_sample(ticker_sample(), now, 60_000_000)
        .unwrap()
        .unwrap();
    assert_eq!(intent.side, OutcomeSide::Yes);
}

#[test]
fn entry_gate_blocks_when_poly_ask_drifted_up_inside_window() {
    let mut config = base_cfg();
    config.poly_drift_window_us = 1_000_000;
    config.poly_drift_block_up_ticks = 4;
    let now = TsUs(1_777_000_030_010_000);
    let earlier = TsUs(now.micros() - 800_000);

    let mut core = RuntimeCore::new(&config).unwrap();
    core.inventory_mut().set_user_wss_trusted(true);
    core.state_mut().set_market(market());
    // Seed history at `earlier` with YES ask 46.
    quote_yes_no(&mut core, 46, 50, earlier);
    // Current snapshot at `now` shows YES ask 50 → drift = +4 ticks.
    quote_yes_no(&mut core, 50, 50, now);
    seed_signal(&mut core);

    let intent = core
        .on_binance_sample(ticker_sample(), now, 60_000_000)
        .unwrap();
    assert_eq!(intent, None, "poly drift-up of 4 ticks must trip the gate");
}

#[test]
fn entry_gate_blocks_when_poly_ask_drifted_down_inside_window() {
    let mut config = base_cfg();
    config.poly_drift_window_us = 1_000_000;
    config.poly_drift_block_down_ticks = 4;
    let now = TsUs(1_777_000_030_010_000);
    let earlier = TsUs(now.micros() - 800_000);

    let mut core = RuntimeCore::new(&config).unwrap();
    core.inventory_mut().set_user_wss_trusted(true);
    core.state_mut().set_market(market());
    quote_yes_no(&mut core, 54, 50, earlier);
    quote_yes_no(&mut core, 50, 50, now);
    seed_signal(&mut core);

    let intent = core
        .on_binance_sample(ticker_sample(), now, 60_000_000)
        .unwrap();
    assert_eq!(
        intent, None,
        "poly drift-down of 4 ticks must trip the gate"
    );
}

#[test]
fn entry_gate_passes_when_poly_book_quiescent() {
    let mut config = base_cfg();
    config.poly_drift_window_us = 1_000_000;
    config.poly_drift_block_up_ticks = 4;
    config.poly_drift_block_down_ticks = 4;
    let now = TsUs(1_777_000_030_010_000);
    let earlier = TsUs(now.micros() - 800_000);

    let mut core = RuntimeCore::new(&config).unwrap();
    core.inventory_mut().set_user_wss_trusted(true);
    core.state_mut().set_market(market());
    // No drift between earlier and now.
    quote_yes_no(&mut core, 50, 50, earlier);
    quote_yes_no(&mut core, 50, 50, now);
    seed_signal(&mut core);

    let intent = core
        .on_binance_sample(ticker_sample(), now, 60_000_000)
        .unwrap()
        .unwrap();
    assert_eq!(intent.side, OutcomeSide::Yes);
}

#[test]
fn entry_gate_blocks_when_edge_below_drift_buffer_floor() {
    // 1-tick drift over 100 ms, EWMA RTT = 800 ms, safety = 12000 bps ⇒
    // required_drift_during_rtt ≈ ceil(1 · 800_000 · 12_000 / (100_000 · 10_000)) = 10.
    // Add min_clean_edge = 3 → required_edge = 13. The Binance signal here
    // produces edge_ticks ≈ 12 (yes_ask = 50, slippage 1, prob_yes ~ 0.98),
    // so the gate must trip.
    let mut config = base_cfg();
    config.poly_drift_window_us = 1_000_000;
    config.poly_drift_safety_bps = 12_000;
    config.poly_drift_min_clean_edge_ticks = 3;
    // Block thresholds disabled so we are testing pillar B only.
    config.poly_drift_block_up_ticks = 0;
    config.poly_drift_block_down_ticks = 0;

    let now = TsUs(1_777_000_030_010_000);
    let earlier = TsUs(now.micros() - 100_000);

    let mut core = RuntimeCore::new(&config).unwrap();
    core.inventory_mut().set_user_wss_trusted(true);
    core.state_mut().set_market(market());
    quote_yes_no(&mut core, 49, 50, earlier);
    quote_yes_no(&mut core, 50, 50, now);
    seed_signal(&mut core);

    // RTT must be high enough for the drift buffer to dominate.
    core.record_rtt(800_000);

    let intent = core
        .on_binance_sample(ticker_sample(), now, 60_000_000)
        .unwrap();
    assert_eq!(intent, None, "edge below drift buffer must suppress entry");
}

#[test]
fn refresh_sell_plan_uses_current_bid_not_planning_time_bid() {
    let mut core = RuntimeCore::new(&base_cfg()).unwrap();
    core.inventory_mut().set_user_wss_trusted(true);
    core.state_mut().set_market(market());

    // Arm sellable inventory.
    core.inventory_mut().apply_user_trade(UserTrade {
        trade_id: TradeId::new("buy-1"),
        token: TokenId::new("yes"),
        taker_order_id: Some(OrderId::new("0xbuy")),
        side: OrderSide::Buy,
        size_atoms: SharesAtoms(2_000_000),
        price: PriceTick::checked(50).unwrap(),
        status: TradeStatus::Matched,
        ts_us: 100,
    });

    // Initial bid = 49 ⇒ SELL limit = 49 (sell_slippage_cents = 0).
    quote_yes_no(&mut core, 50, 50, TsUs(1_000));
    core.state_mut().update_quote(
        TokenId::new("yes"),
        Some(PriceTick::checked(49).unwrap()),
        Some(PriceTick::checked(50).unwrap()),
        PriceTick::checked(1).unwrap(),
        TsUs(1_000),
    );
    let first = core.refresh_sell_plan(&TokenId::new("yes")).unwrap();
    assert_eq!(first.price, PriceTick::checked(49).unwrap());

    // Bid drops to 25 ⇒ a refreshed plan must follow it down.
    core.state_mut().update_quote(
        TokenId::new("yes"),
        Some(PriceTick::checked(25).unwrap()),
        Some(PriceTick::checked(28).unwrap()),
        PriceTick::checked(1).unwrap(),
        TsUs(2_000),
    );
    let refreshed = core.refresh_sell_plan(&TokenId::new("yes")).unwrap();
    assert_eq!(refreshed.price, PriceTick::checked(25).unwrap());
}
