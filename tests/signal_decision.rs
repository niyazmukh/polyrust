use minirust::signal::{BinanceSample, SignalConfig, SignalEngine, Thesis};
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

fn cfg_with_fixed_prob(entry_slippage_ticks: i32, prob_ticks: i32) -> SignalConfig {
    SignalConfig {
        entry_slippage_ticks,
        prob_floor: f64::from(prob_ticks) / 100.0,
        prob_ceil: f64::from(prob_ticks) / 100.0,
        ..cfg()
    }
}

fn seed_window(engine: &mut SignalEngine) {
    engine.push(sample(28_000_000, 1, 99.0, 101.0, 1.0, 1.0));
    engine.push(sample(29_000_000, 2, 101.0, 103.0, 3.0, 1.0));
}

fn sample(
    ts_us: i64,
    update_id: i64,
    bid: f64,
    ask: f64,
    bid_qty: f64,
    ask_qty: f64,
) -> BinanceSample {
    let microprice = ((bid * ask_qty) + (ask * bid_qty)) / (bid_qty + ask_qty);
    BinanceSample {
        ts_us: TsUs(ts_us),
        update_id,
        bid,
        ask,
        bid_qty,
        ask_qty,
        microprice,
    }
}

#[test]
fn thesis_for_side_uses_flow_not_absolute_strike_location() {
    let mut engine = SignalEngine::new(cfg());
    engine.set_strike(100.0, true);
    engine.push(sample(28_000_000, 1, 104.0, 106.0, 4.0, 1.0));
    engine.push(sample(29_000_000, 2, 103.0, 105.0, 1.0, 4.0));

    assert_eq!(
        engine.thesis_for_side(OutcomeSide::Yes, TsUs(29_010_000)),
        Some(Thesis::Opposes)
    );
    assert_eq!(
        engine.thesis_for_side(OutcomeSide::No, TsUs(29_010_000)),
        Some(Thesis::Supports)
    );
}

#[test]
fn thesis_for_side_requires_fresh_valid_signal_window() {
    let mut engine = SignalEngine::new(cfg());
    engine.set_strike(100.0, true);
    engine.push(sample(28_900_000, 1, 99.0, 101.0, 1.0, 1.0));
    engine.push(sample(29_000_000, 2, 101.0, 103.0, 3.0, 1.0));

    assert_eq!(
        engine.thesis_for_side(OutcomeSide::Yes, TsUs(29_010_000)),
        None
    );

    let mut stale = SignalEngine::new(cfg());
    stale.set_strike(100.0, true);
    seed_window(&mut stale);
    assert_eq!(
        stale.thesis_for_side(OutcomeSide::Yes, TsUs(29_300_001)),
        None
    );
}

#[test]
fn thesis_for_side_classifies_mixed_flow_as_weakens() {
    let mut engine = SignalEngine::new(cfg());
    engine.set_strike(100.0, true);
    engine.push(sample(28_000_000, 1, 99.0, 101.0, 4.0, 1.0));
    engine.push(sample(29_000_000, 2, 101.0, 103.0, 1.0, 4.0));

    assert_eq!(
        engine.thesis_for_side(OutcomeSide::Yes, TsUs(29_010_000)),
        Some(Thesis::Weakens)
    );
}

#[test]
fn old_model_up_momentum_with_ofi_imbalance_and_edge_returns_buy_intent() {
    let mut engine = SignalEngine::new(cfg());
    engine.set_strike(100.0, true);
    engine.push(sample(28_000_000, 1, 99.0, 101.0, 1.0, 1.0));
    engine.push(sample(29_000_000, 2, 101.0, 103.0, 3.0, 1.0));

    let intent = engine.on_sample(
        sample(30_000_000, 3, 104.0, 106.0, 3.0, 1.0),
        &market(),
        quote(45, 50, 30_000_000),
        quote(45, 50, 30_000_000),
        TsUs(30_010_000),
        60_000_000,
    );

    let intent = intent.unwrap();
    assert_eq!(intent.side, OutcomeSide::Yes);
    assert_eq!(intent.token, TokenId::new("yes"));
    assert_eq!(intent.limit, PriceTick::checked(51).unwrap());
    assert!(intent.edge_ticks >= 5);
}

#[test]
fn old_model_down_pressure_blocks_up_signal_even_when_price_is_high() {
    let mut engine = SignalEngine::new(cfg());
    engine.set_strike(100.0, true);
    engine.push(sample(28_000_000, 1, 100.0, 102.0, 4.0, 1.0));
    engine.push(sample(29_000_000, 2, 105.0, 107.0, 1.0, 4.0));

    let intent = engine.on_sample(
        sample(30_000_000, 3, 104.0, 106.0, 1.0, 4.0),
        &market(),
        quote(45, 50, 30_000_000),
        quote(45, 50, 30_000_000),
        TsUs(30_010_000),
        60_000_000,
    );

    assert_eq!(intent, None);
}

#[test]
fn spread_eaten_edge_returns_none() {
    let mut engine = SignalEngine::new(cfg());
    engine.set_strike(100.0, true);
    engine.push(sample(28_000_000, 1, 99.0, 101.0, 1.0, 1.0));
    engine.push(sample(29_000_000, 2, 101.0, 103.0, 3.0, 1.0));

    let intent = engine.on_sample(
        sample(30_000_000, 3, 104.0, 106.0, 3.0, 1.0),
        &market(),
        quote(30, 50, 30_000_000),
        quote(30, 50, 30_000_000),
        TsUs(30_010_000),
        60_000_000,
    );

    assert_eq!(intent, None);
}

#[test]
fn entry_slippage_is_execution_cap_not_full_edge_debit() {
    let mut engine = SignalEngine::new(cfg_with_fixed_prob(5, 60));
    engine.set_strike(100.0, true);
    engine.push(sample(28_000_000, 1, 99.0, 101.0, 1.0, 1.0));
    engine.push(sample(29_000_000, 2, 101.0, 103.0, 3.0, 1.0));

    let intent = engine.on_sample(
        sample(30_000_000, 3, 104.0, 106.0, 3.0, 1.0),
        &market(),
        quote(50, 52, 30_000_000),
        quote(45, 50, 30_000_000),
        TsUs(30_010_000),
        60_000_000,
    );

    let intent = intent.unwrap();
    assert_eq!(intent.limit, PriceTick::checked(57).unwrap());
    assert_eq!(intent.edge_price, PriceTick::checked(55).unwrap());
    assert_eq!(intent.edge_ticks, 5);
}

#[test]
fn ofi_and_imbalance_must_confirm_up_move() {
    let mut engine = SignalEngine::new(cfg());
    engine.set_strike(100.0, true);
    engine.push(sample(28_000_000, 1, 99.0, 101.0, 1.0, 1.0));
    engine.push(sample(29_000_000, 2, 101.0, 103.0, 0.5, 6.0));

    let intent = engine.on_sample(
        sample(30_000_000, 3, 104.0, 106.0, 0.5, 6.0),
        &market(),
        quote(45, 50, 30_000_000),
        quote(45, 50, 30_000_000),
        TsUs(30_010_000),
        60_000_000,
    );

    assert_eq!(intent, None);
}

#[test]
fn stale_binance_sample_returns_none() {
    let mut engine = SignalEngine::new(cfg());
    engine.set_strike(100.0, true);
    engine.push(sample(28_000_000, 1, 99.0, 101.0, 1.0, 1.0));
    engine.push(sample(29_000_000, 2, 101.0, 103.0, 3.0, 1.0));

    let intent = engine.on_sample(
        sample(30_000_000, 3, 104.0, 106.0, 3.0, 1.0),
        &market(),
        quote(45, 50, 30_000_000),
        quote(45, 50, 30_000_000),
        TsUs(30_300_001),
        60_000_000,
    );

    assert_eq!(intent, None);
}
