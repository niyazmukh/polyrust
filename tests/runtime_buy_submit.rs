use bytes::Bytes;
use minirust::inventory::{Inventory, SubmitIntent, SubmitStatus, TradeStatus, UserTrade};
use minirust::orders::BuyCanonicalPolicy;
use minirust::runtime::{prepare_buy_submit, record_buy_submit_outcome, BuySubmitPolicy};
use minirust::signal::BuyIntent;
use minirust::signing::{
    OrderSigner, SignInputs, SignatureKind, EXCHANGE_V2_NORMAL, POLYGON_CHAIN_ID,
};
use minirust::submit::SubmitOutcome;
use minirust::types::{
    OrderId, OrderSide, OutcomeSide, PriceTick, SharesAtoms, TokenId, TradeId,
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

fn intent() -> BuyIntent {
    BuyIntent {
        side: OutcomeSide::Yes,
        token: TokenId::new("1234567890123456789012345678901234567890123456789012345678901234"),
        limit: PriceTick::checked(50).unwrap(),
        edge_ticks: 7,
    }
}

fn policy() -> BuySubmitPolicy {
    BuySubmitPolicy {
        target_maker_cents: 101,
        min_size_taker_units: 100,
        min_maker_cents: 100,
        max_overrun_cents: 1,
        max_overrun_bps: 0,
    }
}

#[test]
fn prepare_buy_submit_registers_pending_and_returns_signed_fak_body() {
    let mut inventory = Inventory::new();

    let prepared = prepare_buy_submit(
        &intent(),
        policy(),
        &signer(),
        SignInputs {
            salt: 11,
            timestamp_ms: 1_777_000_000_000,
        },
        &mut inventory,
        1_777_000_000_000_000,
    )
    .unwrap();

    assert_eq!(prepared.target.price, PriceTick::checked(50).unwrap());
    assert_eq!(prepared.target.size.units(), 20_200);
    assert_eq!(prepared.target.maker_amount.cents(), 101);
    assert_eq!(prepared.target.policy, BuyCanonicalPolicy::Ceil);
    assert!(prepared.body.as_bytes().windows(5).any(|w| w == b"order"));

    let pending = inventory.pending(&prepared.submit_id).unwrap();
    assert_eq!(pending.intent, SubmitIntent::Entry);
    assert_eq!(pending.token, intent().token);
    assert_eq!(pending.side, OrderSide::Buy);
    assert_eq!(pending.size_atoms, SharesAtoms(2_020_000));
    assert_eq!(pending.status, SubmitStatus::Pending);
    assert!(inventory.has_entry_exposure_or_pending(&intent().token));
}

#[test]
fn accepted_buy_submit_records_order_id_and_keeps_exposure_block() {
    let mut inventory = Inventory::new();
    let prepared = prepare_buy_submit(
        &intent(),
        policy(),
        &signer(),
        SignInputs {
            salt: 12,
            timestamp_ms: 1_777_000_000_001,
        },
        &mut inventory,
        100,
    )
    .unwrap();

    record_buy_submit_outcome(
        &mut inventory,
        &prepared.submit_id,
        &SubmitOutcome::Accepted {
            order_id: "0xabc".to_string(),
            http_status: 200,
            raw_body: Bytes::new(),
        },
        200,
    );

    let pending = inventory.pending(&prepared.submit_id).unwrap();
    assert_eq!(pending.status, SubmitStatus::Accepted);
    assert_eq!(pending.order_id, Some(OrderId::new("0xabc")));
    assert!(inventory.has_entry_exposure_or_pending(&intent().token));
}

#[test]
fn unknown_buy_submit_blocks_then_late_wss_trade_binds_and_applies_inventory() {
    let mut inventory = Inventory::new();
    let prepared = prepare_buy_submit(
        &intent(),
        policy(),
        &signer(),
        SignInputs {
            salt: 13,
            timestamp_ms: 1_777_000_000_002,
        },
        &mut inventory,
        100,
    )
    .unwrap();

    record_buy_submit_outcome(
        &mut inventory,
        &prepared.submit_id,
        &SubmitOutcome::Unknown {
            http_status: 0,
            error: Some("transport_error".to_string()),
            raw_body: Bytes::new(),
        },
        200,
    );
    assert_eq!(inventory.pending(&prepared.submit_id).unwrap().status, SubmitStatus::Unknown);
    assert!(inventory.has_entry_exposure_or_pending(&intent().token));

    let state = inventory.apply_user_trade(UserTrade {
        trade_id: TradeId::new("late-wss"),
        token: intent().token,
        taker_order_id: None,
        side: OrderSide::Buy,
        size_atoms: prepared.target.size.to_atoms(),
        price: prepared.target.price,
        status: TradeStatus::Matched,
        ts_us: 300,
    });

    assert_eq!(state.matched_submit, Some(prepared.submit_id.clone()));
    assert_eq!(inventory.owned_atoms(&intent().token), SharesAtoms(2_020_000));
    assert_eq!(inventory.pending(&prepared.submit_id).unwrap().status, SubmitStatus::Accepted);
}

#[test]
fn rejected_buy_submit_releases_duplicate_exposure_block() {
    let mut inventory = Inventory::new();
    let prepared = prepare_buy_submit(
        &intent(),
        policy(),
        &signer(),
        SignInputs {
            salt: 14,
            timestamp_ms: 1_777_000_000_003,
        },
        &mut inventory,
        100,
    )
    .unwrap();

    record_buy_submit_outcome(
        &mut inventory,
        &prepared.submit_id,
        &SubmitOutcome::Rejected {
            http_status: 400,
            error: Some("no orders found to match with FAK order".to_string()),
            raw_body: Bytes::new(),
        },
        200,
    );

    assert_eq!(inventory.pending(&prepared.submit_id).unwrap().status, SubmitStatus::Rejected);
    assert!(!inventory.has_entry_exposure_or_pending(&intent().token));
}
