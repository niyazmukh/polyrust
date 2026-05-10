use bytes::Bytes;
use minirust::inventory::{Inventory, SubmitIntent, SubmitStatus, TradeStatus, UserTrade};
use minirust::runtime::{
    prepare_sell_submit, prepare_sell_submit_at_bid, prepare_sell_submit_for_size_at_bid,
    record_sell_submit_outcome,
};
use minirust::signing::{
    OrderSigner, SignInputs, SignatureKind, EXCHANGE_V2_NORMAL, POLYGON_CHAIN_ID,
};
use minirust::state::{MarketContext, RuntimeState};
use minirust::submit::SubmitOutcome;
use minirust::types::{
    ConditionId, OrderId, OrderSide, PriceTick, Shares2, SharesAtoms, TokenId, TradeId, TsUs,
};

const TEST_PRIVATE_KEY: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000001";
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
        yes_label: "Up".to_string(),
        no_label: "Down".to_string(),
        start_ts: 1_000,
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
        status: TradeStatus::Matched,
        ts_us: 100,
    }
}

#[test]
fn prepare_sell_submit_uses_explicit_limit_and_sellable_floor() {
    let mut inventory = Inventory::new();
    inventory.apply_user_trade(buy_trade(50, 2_025_000));

    let prepared = prepare_sell_submit(
        &token(),
        PriceTick::checked(55).unwrap(),
        &signer(),
        SignInputs {
            salt: 21,
            timestamp_ms: 1_777_000_000_000,
        },
        &mut inventory,
        200,
    )
    .unwrap()
    .unwrap();

    assert_eq!(prepared.price, PriceTick::checked(55).unwrap());
    assert_eq!(prepared.size, Shares2::new_unchecked(202));
    assert!(prepared.body.as_bytes().windows(4).any(|w| w == b"SELL"));

    let pending = inventory.pending(&prepared.submit_id).unwrap();
    assert_eq!(pending.intent, SubmitIntent::Exit);
    assert_eq!(pending.token, token());
    assert_eq!(pending.side, OrderSide::Sell);
    assert_eq!(pending.size_atoms, SharesAtoms(2_020_000));
    assert_eq!(pending.status, SubmitStatus::Pending);
}

#[test]
fn prepare_sell_submit_returns_none_when_only_dust_is_owned() {
    let mut inventory = Inventory::new();
    inventory.apply_user_trade(buy_trade(50, 9_999));

    let prepared = prepare_sell_submit(
        &token(),
        PriceTick::checked(55).unwrap(),
        &signer(),
        SignInputs {
            salt: 22,
            timestamp_ms: 1_777_000_000_000,
        },
        &mut inventory,
        200,
    )
    .unwrap();

    assert_eq!(prepared, None);
}

#[test]
fn rejected_sell_submit_does_not_change_wss_inventory() {
    let mut inventory = Inventory::new();
    inventory.apply_user_trade(buy_trade(50, 2_000_000));
    let prepared = prepare_sell_submit(
        &token(),
        PriceTick::checked(55).unwrap(),
        &signer(),
        SignInputs {
            salt: 23,
            timestamp_ms: 1_777_000_000_000,
        },
        &mut inventory,
        200,
    )
    .unwrap()
    .unwrap();

    record_sell_submit_outcome(
        &mut inventory,
        &prepared.submit_id,
        &SubmitOutcome::Rejected {
            http_status: 400,
            error: Some("insufficient balance".to_string()),
            raw_body: Bytes::new(),
        },
        300,
    );

    assert_eq!(inventory.pending(&prepared.submit_id).unwrap().status, SubmitStatus::Rejected);
    assert_eq!(inventory.owned_atoms(&token()), SharesAtoms(2_000_000));
    assert_eq!(inventory.sellable(&token()), Shares2::new_unchecked(200));
}

#[test]
fn prepare_sell_submit_at_bid_has_no_profit_gate() {
    let state = state_with_bid(Some(49));
    let mut inventory = Inventory::new();
    inventory.apply_user_trade(buy_trade(50, 2_000_000));

    let prepared = prepare_sell_submit_at_bid(
        &token(),
        &state,
        &signer(),
        SignInputs {
            salt: 24,
            timestamp_ms: 1_777_000_000_000,
        },
        &mut inventory,
        200,
    )
    .unwrap()
    .unwrap();

    assert_eq!(prepared.price, PriceTick::checked(49).unwrap());
    assert_eq!(prepared.size, Shares2::new_unchecked(200));
}

#[test]
fn prepare_sell_submit_for_size_at_bid_does_not_wait_for_wss_inventory() {
    let state = state_with_bid(Some(55));
    let mut inventory = Inventory::new();

    let prepared = prepare_sell_submit_for_size_at_bid(
        &token(),
        SharesAtoms(2_025_000),
        &state,
        &signer(),
        SignInputs {
            salt: 25,
            timestamp_ms: 1_777_000_000_000,
        },
        &mut inventory,
        200,
    )
    .unwrap()
    .unwrap();

    assert_eq!(prepared.price, PriceTick::checked(55).unwrap());
    assert_eq!(prepared.size, Shares2::new_unchecked(202));
    assert_eq!(inventory.owned_atoms(&token()), SharesAtoms(0));
    assert_eq!(inventory.pending(&prepared.submit_id).unwrap().intent, SubmitIntent::Exit);
}

#[test]
fn prepare_sell_submit_at_bid_returns_none_without_executable_bid() {
    let state = state_with_bid(None);
    let mut inventory = Inventory::new();
    inventory.apply_user_trade(buy_trade(50, 2_000_000));

    let prepared = prepare_sell_submit_at_bid(
        &token(),
        &state,
        &signer(),
        SignInputs {
            salt: 26,
            timestamp_ms: 1_777_000_000_000,
        },
        &mut inventory,
        200,
    )
    .unwrap();

    assert_eq!(prepared, None);
}
