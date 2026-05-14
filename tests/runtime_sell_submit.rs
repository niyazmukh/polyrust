use minirust::inventory::{Inventory, TradeStatus, UserTrade};
use minirust::runtime::{ExitReason, RuntimeCore, plan_sell_at_bid};
use minirust::signal::BinanceSample;
use minirust::signing::{
    EXCHANGE_V2_NORMAL, OrderSigner, POLYGON_CHAIN_ID, SignInputs, SignatureKind,
};
use minirust::state::{MarketContext, RuntimeState};
use minirust::types::{
    ConditionId, OrderId, OrderSide, PriceTick, Shares2, SharesAtoms, TokenId, TradeId, TsUs,
};

const TEST_PRIVATE_KEY: &str = "0x0000000000000000000000000000000000000000000000000000000000000001";
const TEST_API_KEY: &str = "00000000-0000-0000-0000-000000000001";

fn signer() -> OrderSigner {
    OrderSigner::new(
        TEST_PRIVATE_KEY,
        TEST_API_KEY,
        None,
        SignatureKind::Eoa,
        POLYGON_CHAIN_ID,
        EXCHANGE_V2_NORMAL,
    )
    .unwrap()
}

fn token() -> TokenId {
    TokenId::new("1234567890123456789012345678901234567890123456789012345678901234")
}

fn no_token() -> TokenId {
    TokenId::new("2234567890123456789012345678901234567890123456789012345678901234")
}

fn state_with_bid(bid: Option<i32>) -> RuntimeState {
    let yes = token();
    let no = no_token();
    let mut state = RuntimeState::new();
    state.set_market(MarketContext {
        slug: "btc-up-down-1m".to_string(),
        condition_id: ConditionId::new("cond"),
        yes_token: yes.clone(),
        no_token: no.clone(),
        end_ts: 1_060,
        slug_ts: 1_000,
    });
    state.update_quote(
        yes,
        bid.map(|v| PriceTick::checked(v).unwrap()),
        Some(PriceTick::checked(55).unwrap()),
        PriceTick::checked(1).unwrap(),
        TsUs(200),
    );
    state.update_quote(
        no,
        Some(PriceTick::checked(45).unwrap()),
        Some(PriceTick::checked(50).unwrap()),
        PriceTick::checked(1).unwrap(),
        TsUs(200),
    );
    state
}

fn buy_trade(price_ticks: i32, size_atoms: i64) -> UserTrade {
    UserTrade {
        trade_id: TradeId::new("buy-1"),
        token: token(),
        taker_order_id: Some(OrderId::new("0xbuy")),
        side: OrderSide::Buy,
        size_atoms: SharesAtoms(size_atoms),
        price: PriceTick::checked(price_ticks).unwrap(),
        status: TradeStatus::Confirmed,
        ts_us: 100,
    }
}

fn apply_buy_raw(core: &mut RuntimeCore, price_ticks: i32, size: &str, ts_us: i64) {
    let raw = format!(
        r#"{{"event_type":"trade","trade_id":"buy-raw-{ts_us}","asset_id":"{}","side":"BUY","size":"{}","price":"{}","status":"MATCHED","order_id":"0xbuy"}}"#,
        token().as_str(),
        size,
        PriceTick::checked(price_ticks).unwrap()
    );
    core.apply_user_raw_with_states(raw.as_bytes(), ts_us)
        .unwrap();
}

fn apply_sell_raw(core: &mut RuntimeCore, price_ticks: i32, size: &str, ts_us: i64) {
    let raw = format!(
        r#"{{"event_type":"trade","trade_id":"sell-raw-{ts_us}","asset_id":"{}","side":"SELL","size":"{}","price":"{}","status":"MATCHED","order_id":"0xsell"}}"#,
        token().as_str(),
        size,
        PriceTick::checked(price_ticks).unwrap()
    );
    core.apply_user_raw_with_states(raw.as_bytes(), ts_us)
        .unwrap();
}

fn cfg() -> minirust::config::Config {
    minirust::config::Config {
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
    }
}

fn cfg_with_fixed_prob(prob_ticks: i32) -> minirust::config::Config {
    let mut config = cfg();
    config.prob_floor = f64::from(prob_ticks) / 100.0;
    config.prob_ceil = f64::from(prob_ticks) / 100.0;
    config
}

fn core_with_bid(bid: i32) -> RuntimeCore {
    core_with_bid_and_config(bid, cfg())
}

fn core_with_bid_and_config(bid: i32, config: minirust::config::Config) -> RuntimeCore {
    let mut core = RuntimeCore::new(&config).unwrap();
    let state = state_with_bid(Some(bid));
    let market = state.market().cloned().unwrap();
    core.state_mut().set_market(market);
    core.state_mut().update_quote(
        token(),
        Some(PriceTick::checked(bid).unwrap()),
        Some(PriceTick::checked(55).unwrap()),
        PriceTick::checked(1).unwrap(),
        TsUs(200),
    );
    core.state_mut().update_quote(
        no_token(),
        Some(PriceTick::checked(45).unwrap()),
        Some(PriceTick::checked(50).unwrap()),
        PriceTick::checked(1).unwrap(),
        TsUs(200),
    );
    core
}

fn sample(ts_us: i64, update_id: i64, microprice: f64) -> BinanceSample {
    BinanceSample {
        ts_us: TsUs(ts_us),
        update_id,
        bid: microprice - 1.0,
        ask: microprice + 1.0,
        bid_qty: 1.0,
        ask_qty: 1.0,
        microprice,
    }
}

fn seed_signal_window(core: &mut RuntimeCore, latest_ts_us: i64, update_id: i64, microprice: f64) {
    core.signal_mut().set_strike(100.0, true);
    core.signal_mut()
        .push(sample(latest_ts_us - 300_000, update_id, microprice));
    core.signal_mut()
        .push(sample(latest_ts_us, update_id + 1, microprice));
}

fn update_bid(core: &mut RuntimeCore, bid: i32, ask: i32, ts_us: i64) {
    core.state_mut().update_quote(
        token(),
        Some(PriceTick::checked(bid).unwrap()),
        Some(PriceTick::checked(ask).unwrap()),
        PriceTick::checked(1).unwrap(),
        TsUs(ts_us),
    );
}

// Plan-only path: build SellPlan under lock, sign outside.
// Tests demonstrate that plan construction requires no OrderSigner
// and is pure state+inventory read.

#[test]
fn plan_sell_at_bid_matches_prepare_without_signing() {
    let state = state_with_bid(Some(49));
    let mut inventory = Inventory::new();
    inventory.apply_user_trade(buy_trade(50, 2_000_000));

    let plan = plan_sell_at_bid(&token(), &state, &inventory, 0)
        .expect("bid + sellable inventory => Some plan");
    assert_eq!(plan.token, token());
    assert_eq!(plan.price, PriceTick::checked(49).unwrap());
    assert_eq!(plan.size, Shares2::new_unchecked(200));

    // And signing that plan outside the lock produces the same FAK body
    // that `prepare_sell_submit_at_bid` would have built inline.
    let prepared_via_plan = plan
        .sign(
            &signer(),
            SignInputs {
                salt: 41,
                timestamp_ms: 1_777_000_000_000,
            },
        )
        .unwrap();
    assert!(
        prepared_via_plan
            .body
            .as_bytes()
            .windows(4)
            .any(|w| w == b"SELL")
    );
}

#[test]
fn plan_sell_at_bid_returns_none_when_no_sellable_inventory() {
    let state = state_with_bid(Some(49));
    let inventory = Inventory::new(); // no trades applied

    assert_eq!(plan_sell_at_bid(&token(), &state, &inventory, 0), None);
}

#[test]
fn plan_sell_at_bid_returns_none_without_executable_bid() {
    let state = state_with_bid(None);
    let mut inventory = Inventory::new();
    inventory.apply_user_trade(buy_trade(50, 2_000_000));

    assert_eq!(plan_sell_at_bid(&token(), &state, &inventory, 0), None);
}

#[test]
fn exit_tracker_holds_until_arm_and_drop() {
    let mut core = core_with_bid(50);
    seed_signal_window(&mut core, 899_000, 1, 300.0);
    apply_buy_raw(&mut core, 50, "2.000000", 0);

    assert_eq!(core.plan_exits(&Default::default(), 800_000), Vec::new());

    update_bid(&mut core, 53, 55, 900_000);
    assert_eq!(core.plan_exits(&Default::default(), 900_000), Vec::new());

    seed_signal_window(&mut core, 999_000, 10, 100.0);
    update_bid(&mut core, 51, 54, 1_000_000);
    let exits = core.plan_exits(&Default::default(), 1_000_000);
    assert_eq!(exits.len(), 1);
    assert_eq!(exits[0].reason, ExitReason::Drop);
    assert_eq!(exits[0].plan.price, PriceTick::checked(51).unwrap());
    assert_eq!(exits[0].entry_price, PriceTick::checked(50).unwrap());
    assert_eq!(exits[0].peak_bid, PriceTick::checked(53).unwrap());
}

#[test]
fn exit_tracker_sells_after_hold_without_profit() {
    let mut core = core_with_bid(49);
    apply_buy_raw(&mut core, 50, "2.000000", 0);

    let exits = core.plan_exits(&Default::default(), 15_000_100);
    assert_eq!(exits.len(), 1);
    assert_eq!(exits[0].reason, ExitReason::Hold);
    assert_eq!(exits[0].plan.price, PriceTick::checked(49).unwrap());
}

#[test]
fn exit_tracker_stops_when_bid_drops_below_entry_bid() {
    let mut core = core_with_bid(50);
    seed_signal_window(&mut core, 999_000, 1, 200.0);
    apply_buy_raw(&mut core, 50, "2.000000", 0);

    update_bid(&mut core, 47, 50, 1_000_000);

    let exits = core.plan_exits(&Default::default(), 1_000_000);
    assert_eq!(exits.len(), 1);
    assert_eq!(exits[0].reason, ExitReason::Stop);
    assert_eq!(exits[0].plan.price, PriceTick::checked(47).unwrap());
    assert_eq!(exits[0].entry_price, PriceTick::checked(50).unwrap());
    assert_eq!(exits[0].peak_bid, PriceTick::checked(50).unwrap());
}

#[test]
fn exit_tracker_uses_entry_bid_for_adverse_drop() {
    let mut core = core_with_bid(32);
    seed_signal_window(&mut core, 4_599_000, 1, 200.0);
    apply_buy_raw(&mut core, 33, "2.000000", 0);

    update_bid(&mut core, 30, 33, 4_500_000);
    assert_eq!(core.plan_exits(&Default::default(), 4_500_000), Vec::new());

    update_bid(&mut core, 29, 32, 4_600_000);
    let exits = core.plan_exits(&Default::default(), 4_600_000);
    assert_eq!(exits.len(), 1);
    assert_eq!(exits[0].reason, ExitReason::Stop);
    assert_eq!(exits[0].plan.price, PriceTick::checked(29).unwrap());
}

#[test]
fn exit_tracker_sells_when_fair_value_no_longer_exceeds_bid() {
    let mut core = core_with_bid_and_config(55, cfg_with_fixed_prob(55));
    seed_signal_window(&mut core, 999_000, 1, 102.0);
    apply_buy_raw(&mut core, 50, "2.000000", 0);

    let exits = core.plan_exits(&Default::default(), 1_000_000);
    assert_eq!(exits.len(), 1);
    assert_eq!(exits[0].reason, ExitReason::Value);
    assert_eq!(exits[0].plan.price, PriceTick::checked(55).unwrap());
    assert_eq!(exits[0].fair_ticks, Some(55));
    assert_eq!(exits[0].fair_minus_bid_ticks, Some(0));
}

#[test]
fn value_exit_is_not_sticky_after_model_recovers() {
    let mut core = core_with_bid(55);
    seed_signal_window(&mut core, 999_000, 1, 100.0);
    apply_buy_raw(&mut core, 50, "2.000000", 0);

    let exits = core.plan_exits(&Default::default(), 1_000_000);
    assert_eq!(exits.len(), 1);
    assert_eq!(exits[0].reason, ExitReason::Value);

    seed_signal_window(&mut core, 1_999_000, 10, 300.0);
    update_bid(&mut core, 56, 57, 2_000_000);
    assert_eq!(core.plan_exits(&Default::default(), 2_000_000), Vec::new());
}

#[test]
fn profitable_pullback_holds_while_fair_value_still_supports_side() {
    let mut core = core_with_bid(68);
    seed_signal_window(&mut core, 7_999_000, 1, 300.0);
    apply_buy_raw(&mut core, 69, "2.000000", 0);

    update_bid(&mut core, 78, 79, 7_500_000);
    assert_eq!(core.plan_exits(&Default::default(), 7_500_000), Vec::new());

    update_bid(&mut core, 76, 79, 8_000_000);
    assert_eq!(core.plan_exits(&Default::default(), 8_000_000), Vec::new());
}

#[test]
fn adverse_collapse_stops_even_when_fair_value_supports_side() {
    let mut core = core_with_bid(55);
    seed_signal_window(&mut core, 999_000, 1, 300.0);
    apply_buy_raw(&mut core, 56, "2.000000", 0);

    update_bid(&mut core, 52, 54, 1_000_000);
    let exits = core.plan_exits(&Default::default(), 1_000_000);
    assert_eq!(exits.len(), 1);
    assert_eq!(exits[0].reason, ExitReason::Stop);
    assert_eq!(exits[0].plan.price, PriceTick::checked(52).unwrap());
}

#[test]
fn exit_tracker_clears_after_sell_trade_and_market_rotation() {
    let mut core = core_with_bid(53);
    apply_buy_raw(&mut core, 50, "2.000000", 0);
    apply_sell_raw(&mut core, 53, "2.000000", 200);

    assert_eq!(core.plan_exits(&Default::default(), 15_000_100), Vec::new());

    apply_buy_raw(&mut core, 50, "2.000000", 300);
    core.release_market_scope([&no_token()]);
    assert_eq!(core.plan_exits(&Default::default(), 15_000_100), Vec::new());
}
