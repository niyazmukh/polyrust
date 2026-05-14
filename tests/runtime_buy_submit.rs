use minirust::inventory::{Inventory, SubmitStatus, TradeStatus, UserTrade};
use minirust::runtime::{
    BuySubmitPolicy, expire_stale_entry_claims, prepare_buy_submit, record_buy_submit_outcome,
};
use minirust::signal::BuyIntent;
use minirust::signing::{
    EXCHANGE_V2_NORMAL, OrderSigner, POLYGON_CHAIN_ID, SignInputs, SignatureKind,
};
use minirust::submit::SubmitOutcome;
use minirust::types::{OrderId, OrderSide, OutcomeSide, PriceTick, SharesAtoms, TokenId, TradeId};

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

fn intent() -> BuyIntent {
    BuyIntent {
        side: OutcomeSide::Yes,
        token: TokenId::new("1234567890123456789012345678901234567890123456789012345678901234"),
        limit: PriceTick::checked(50).unwrap(),
        edge_price: PriceTick::checked(50).unwrap(),
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

fn claim(inv: &mut Inventory) -> minirust::inventory::SubmitId {
    inv.set_user_wss_trusted(true);
    inv.claim_entry(
        intent().token,
        OrderSide::Buy,
        SharesAtoms(1),
        1_777_000_000_000_000,
    )
}

// Known result for p=50, target=101¢, overrun=1¢: size=20200 taker units, maker=101¢.
// size atoms = 20200 * 100 = 2_020_000.
const EXPECTED_SIZE_ATOMS: SharesAtoms = SharesAtoms(2_020_000);

#[test]
fn prepare_buy_submit_returns_signed_fak_body_from_claim() {
    let mut inventory = Inventory::new();
    let claim_id = claim(&mut inventory);

    let prepared = prepare_buy_submit(
        &intent(),
        policy(),
        &signer(),
        SignInputs {
            salt: 11,
            timestamp_ms: 1_777_000_000_000,
        },
        claim_id,
    )
    .unwrap();

    assert!(prepared.body.as_bytes().windows(5).any(|w| w == b"order"));
    assert!(inventory.has_entry_exposure_or_pending(&intent().token));
}

#[test]
fn accepted_buy_submit_records_order_id_and_keeps_exposure_block() {
    let mut inventory = Inventory::new();
    let claim_id = claim(&mut inventory);
    let prepared = prepare_buy_submit(
        &intent(),
        policy(),
        &signer(),
        SignInputs {
            salt: 12,
            timestamp_ms: 1_777_000_000_001,
        },
        claim_id,
    )
    .unwrap();

    record_buy_submit_outcome(
        &mut inventory,
        &prepared.submit_id,
        &SubmitOutcome::Accepted {
            order_id: "0xabc".to_string(),
            http_status: 200,
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
    let claim_id = claim(&mut inventory);
    let prepared = prepare_buy_submit(
        &intent(),
        policy(),
        &signer(),
        SignInputs {
            salt: 13,
            timestamp_ms: 1_777_000_000_002,
        },
        claim_id,
    )
    .unwrap();

    record_buy_submit_outcome(
        &mut inventory,
        &prepared.submit_id,
        &SubmitOutcome::Unknown {
            http_status: 0,
            error: Some("transport_error".to_string()),
        },
        200,
    );
    assert_eq!(
        inventory.pending(&prepared.submit_id).unwrap().status,
        SubmitStatus::Unknown
    );
    assert!(inventory.has_entry_exposure_or_pending(&intent().token));

    let state = inventory.apply_user_trade(UserTrade {
        trade_id: TradeId::new("late-wss"),
        token: intent().token,
        taker_order_id: None,
        side: OrderSide::Buy,
        size_atoms: EXPECTED_SIZE_ATOMS,
        price: PriceTick::checked(50).unwrap(),
        status: TradeStatus::Matched,
        ts_us: 300,
    });

    assert_eq!(state.matched_submit, Some(prepared.submit_id.clone()));
    // MATCHED: pending stays alive (blocks duplicate BUY), but inventory waits
    // for CONFIRMED so local sellable balance matches CLOB resale readiness.
    assert!(inventory.pending(&prepared.submit_id).is_some());
    assert_eq!(inventory.owned_atoms(&intent().token), SharesAtoms(0));

    // CONFIRMED: inventory applied, pending removed.
    let confirmed = inventory.apply_user_trade(UserTrade {
        trade_id: TradeId::new("late-wss"),
        token: intent().token,
        taker_order_id: None,
        side: OrderSide::Buy,
        size_atoms: EXPECTED_SIZE_ATOMS,
        price: PriceTick::checked(50).unwrap(),
        status: TradeStatus::Confirmed,
        ts_us: 400,
    });
    assert!(confirmed.applied);
    assert_eq!(
        inventory.owned_atoms(&intent().token),
        SharesAtoms(2_020_000)
    );
    assert_eq!(inventory.pending(&prepared.submit_id), None);
}

#[test]
fn rejected_buy_submit_releases_duplicate_exposure_block() {
    let mut inventory = Inventory::new();
    let claim_id = claim(&mut inventory);
    let prepared = prepare_buy_submit(
        &intent(),
        policy(),
        &signer(),
        SignInputs {
            salt: 14,
            timestamp_ms: 1_777_000_000_003,
        },
        claim_id,
    )
    .unwrap();

    record_buy_submit_outcome(
        &mut inventory,
        &prepared.submit_id,
        &SubmitOutcome::Rejected {
            http_status: 400,
            error: Some("no orders found to match with FAK order".to_string()),
        },
        200,
    );

    assert_eq!(inventory.pending(&prepared.submit_id), None); // removed by release_claim
    assert!(!inventory.has_entry_exposure_or_pending(&intent().token));
}

#[test]
fn stale_unknown_buy_submit_stops_blocking_but_remains_matchable() {
    let mut inventory = Inventory::new();
    let claim_id = claim(&mut inventory);
    let prepared = prepare_buy_submit(
        &intent(),
        policy(),
        &signer(),
        SignInputs {
            salt: 15,
            timestamp_ms: 1_777_000_000_004,
        },
        claim_id,
    )
    .unwrap();

    record_buy_submit_outcome(
        &mut inventory,
        &prepared.submit_id,
        &SubmitOutcome::Unknown {
            http_status: 0,
            error: Some("transport_timeout".to_string()),
        },
        10_000_000,
    );

    expire_stale_entry_claims(&mut inventory, 14_999_999);
    assert_eq!(
        inventory.pending(&prepared.submit_id).unwrap().status,
        SubmitStatus::Unknown
    );
    assert!(inventory.has_entry_exposure_or_pending(&intent().token));

    expire_stale_entry_claims(&mut inventory, 15_000_000);
    assert_eq!(
        inventory.pending(&prepared.submit_id).unwrap().status,
        SubmitStatus::ExpiredUnknown
    );
    assert!(!inventory.has_entry_exposure_or_pending(&intent().token));

    let state = inventory.apply_user_trade(UserTrade {
        trade_id: TradeId::new("late-expired-wss"),
        token: intent().token,
        taker_order_id: None,
        side: OrderSide::Buy,
        size_atoms: EXPECTED_SIZE_ATOMS,
        price: PriceTick::checked(50).unwrap(),
        status: TradeStatus::Matched,
        ts_us: 15_100_000,
    });

    assert_eq!(state.matched_submit, Some(prepared.submit_id.clone()));
    assert_eq!(inventory.owned_atoms(&intent().token), SharesAtoms(0));

    let confirmed = inventory.apply_user_trade(UserTrade {
        trade_id: TradeId::new("late-expired-wss"),
        token: intent().token,
        taker_order_id: None,
        side: OrderSide::Buy,
        size_atoms: EXPECTED_SIZE_ATOMS,
        price: PriceTick::checked(50).unwrap(),
        status: TradeStatus::Confirmed,
        ts_us: 15_200_000,
    });
    assert!(confirmed.applied);
    assert_eq!(inventory.owned_atoms(&intent().token), EXPECTED_SIZE_ATOMS);
}
