# Graph Report - src  (2026-05-11)

## Corpus Check
- Corpus is ~27,301 words - fits in a single context window. You may not need a graph.

## Summary
- 466 nodes · 857 edges · 26 communities (17 shown, 9 thin omitted)
- Extraction: 92% EXTRACTED · 8% INFERRED · 0% AMBIGUOUS · INFERRED: 65 edges (avg confidence: 0.8)
- Token cost: 0 input · 0 output

## Community Hubs (Navigation)
- [[_COMMUNITY_Inventory & Position Tracking|Inventory & Position Tracking]]
- [[_COMMUNITY_EIP-712 Order Signing|EIP-712 Order Signing]]
- [[_COMMUNITY_Runtime Orchestration & Sizing|Runtime Orchestration & Sizing]]
- [[_COMMUNITY_Entry Point & Feed IO|Entry Point & Feed IO]]
- [[_COMMUNITY_Configuration & Validation|Configuration & Validation]]
- [[_COMMUNITY_HTTP Submit & Classification|HTTP Submit & Classification]]
- [[_COMMUNITY_L1L2 Auth & Credentials|L1/L2 Auth & Credentials]]
- [[_COMMUNITY_Gamma Market Discovery|Gamma Market Discovery]]
- [[_COMMUNITY_Anchor Strike Resolution|Anchor Strike Resolution]]
- [[_COMMUNITY_Market Event Parsing|Market Event Parsing]]
- [[_COMMUNITY_Runtime State & Quotes|Runtime State & Quotes]]
- [[_COMMUNITY_User WSS Message Parsing|User WSS Message Parsing]]
- [[_COMMUNITY_Signal Engine & BUY Intent|Signal Engine & BUY Intent]]
- [[_COMMUNITY_Binance SBEJSON Parser|Binance SBE/JSON Parser]]
- [[_COMMUNITY_Fixed-Point Math Utilities|Fixed-Point Math Utilities]]
- [[_COMMUNITY_PriceTick Type|PriceTick Type]]
- [[_COMMUNITY_USD CentAtom Types|USD Cent/Atom Types]]
- [[_COMMUNITY_Shares4 Type|Shares4 Type]]
- [[_COMMUNITY_Shares2 Type|Shares2 Type]]
- [[_COMMUNITY_Community 19|Community 19]]
- [[_COMMUNITY_Community 20|Community 20]]
- [[_COMMUNITY_Community 21|Community 21]]
- [[_COMMUNITY_Community 22|Community 22]]
- [[_COMMUNITY_Community 23|Community 23]]
- [[_COMMUNITY_Community 24|Community 24]]
- [[_COMMUNITY_Community 25|Community 25]]

## God Nodes (most connected - your core abstractions)
1. `Inventory` - 20 edges
2. `main()` - 16 edges
3. `canonical_buy_target_for_notional()` - 15 edges
4. `RuntimeCore` - 15 edges
5. `classify()` - 15 edges
6. `SharesAtoms` - 14 edges
7. `RuntimeState` - 13 edges
8. `signature_recovers_to_signer_address_for_buy()` - 12 edges
9. `signature_recovers_for_many_salts_across_both_sides()` - 12 edges
10. `b()` - 12 edges

## Surprising Connections (you probably didn't know these)
- `parse_one()` --calls--> `Quote`  [INFERRED]
  market.rs → state.rs
- `canonical_buy_target_for_notional()` --calls--> `floor_to_multiple()`  [INFERRED]
  orders.rs → types.rs
- `canonical_buy_target_for_notional()` --calls--> `ceil_to_multiple()`  [INFERRED]
  orders.rs → types.rs
- `main()` --calls--> `derive_api_credentials()`  [INFERRED]
  main.rs → auth.rs
- `main()` --calls--> `parse_book_ticker()`  [INFERRED]
  main.rs → binance.rs

## Communities (26 total, 9 thin omitted)

### Community 0 - "Inventory & Position Tracking"
Cohesion: 0.09
Nodes (22): apply_inventory_delta(), confirmed_without_matched_still_recovers_inventory_once(), expire_pending_removes_only_old_pending_entries(), expire_pending_unblocks_same_token_buy(), Inventory, matched_applies_inventory_once_and_confirmed_only_finalizes(), order(), PendingSubmit (+14 more)

### Community 1 - "EIP-712 Order Signing"
Cohesion: 0.11
Nodes (33): address_derives_from_test_vector(), address_lower_hex(), address_to_uint256_be(), bytes32_hex(), compute_domain_separator(), derive_address(), domain_separator_for_polygon_v2_normal_is_stable(), encode_signature() (+25 more)

### Community 2 - "Runtime Orchestration & Sizing"
Cohesion: 0.07
Nodes (27): BuyCanonicalError, BuyCanonicalInput, BuyCanonicalPolicy, BuyCanonicalTarget, canonical_buy_target_for_notional(), canonical_sell_params(), input(), p050_target_1000_lands_at_2000_cents() (+19 more)

### Community 3 - "Entry Point & Feed IO"
Cohesion: 0.08
Nodes (28): binance_feed_loop(), binance_feed_receives_frames(), market_feed_loop(), user_feed_loop(), user_feed_sends_auth_frame_on_connect(), f64, Field, FieldValue (+20 more)

### Community 4 - "Configuration & Validation"
Cohesion: 0.1
Nodes (19): cfg_from_pairs(), Config, config_accepts_dry_run_only(), config_accepts_live_only(), config_builds_signal_and_buy_submit_policy_from_runtime_env_shape(), config_rejects_both_dry_run_and_live_false(), config_rejects_both_dry_run_and_live_true(), ConfigError (+11 more)

### Community 5 - "HTTP Submit & Classification"
Cohesion: 0.12
Nodes (19): b(), classify(), extract_error_field(), http_200_success_false_is_rejected(), http_200_with_order_id_is_accepted(), http_200_without_order_id_is_unknown(), http_400_is_rejected_regardless_of_body(), http_400_with_min_size_error_is_rejected() (+11 more)

### Community 6 - "L1/L2 Auth & Credentials"
Cohesion: 0.15
Nodes (20): address_lower_hex(), address_to_uint256_be(), clob_auth_struct_hash(), compute_clob_auth_domain(), decode_secret_b64_padded(), derive_address(), derive_api_credentials(), DerivedCredentials (+12 more)

### Community 7 - "Gamma Market Discovery"
Cohesion: 0.19
Nodes (13): ClobToken, GammaClient, parse_clob_tokens(), parse_gamma_iso8601(), parse_iso8601_golden_midnight_utc(), parse_iso8601_golden_noon_utc(), parse_iso8601_with_fractional(), parse_iso8601_with_offset_timezone() (+5 more)

### Community 8 - "Anchor Strike Resolution"
Cohesion: 0.28
Nodes (12): AnchorBuffer, AnchorResult, failed_when_window_closed_with_fewer_than_3(), late_discovery_uses_all_samples_after_window_start(), median(), no_pending_returns_pending(), normal_resolution_with_4_samples_uses_mean_of_middle_two(), normal_resolution_with_exactly_3_samples() (+4 more)

### Community 9 - "Market Event Parsing"
Cohesion: 0.19
Nodes (15): apply_market_events(), best_book_price(), MarketEvent, MarketParseError, optional_price(), optional_str(), parse_level(), parse_market_events() (+7 more)

### Community 10 - "Runtime State & Quotes"
Cohesion: 0.13
Nodes (4): RuntimeError, MarketContext, Quote, RuntimeState

### Community 11 - "User WSS Message Parsing"
Cohesion: 0.19
Nodes (15): AuthError, auth_unknown_status_returns_other(), is_trade_event(), optional_i64(), optional_str(), parse_side(), parse_trade_value(), parse_user_message() (+7 more)

### Community 12 - "Signal Engine & BUY Intent"
Cohesion: 0.17
Nodes (7): BinanceSample, BuyIntent, phi(), phi_tail(), SignalConfig, SignalEngine, SignalPoint

### Community 13 - "Binance SBE/JSON Parser"
Cohesion: 0.2
Nodes (11): BinanceBookTicker, BinanceParseError, mantissa_to_f64(), normalize_epoch_to_us(), parse_book_ticker(), parse_book_ticker_json(), parse_positive_f64_json(), parse_positive_i64_json() (+3 more)

### Community 14 - "Fixed-Point Math Utilities"
Cohesion: 0.14
Nodes (5): buy_size_multiple_taker_units(), ceil_to_multiple(), floor_to_multiple(), gcd(), OrderSide

### Community 15 - "PriceTick Type"
Cohesion: 0.22
Nodes (3): decimal_scale_digits(), parse_decimal_scaled(), PriceTick

### Community 16 - "USD Cent/Atom Types"
Cohesion: 0.22
Nodes (3): maker_cents_for(), UsdcAtoms, UsdcCents

## Knowledge Gaps
- **32 isolated node(s):** `AnchorResult`, `DerivedCredentials`, `ClobToken`, `SubmitIntent`, `PendingSubmit` (+27 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **9 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `main()` connect `Entry Point & Feed IO` to `Inventory & Position Tracking`, `Runtime Orchestration & Sizing`, `Configuration & Validation`, `L1/L2 Auth & Credentials`, `Binance SBE/JSON Parser`?**
  _High betweenness centrality (0.296) - this node is a cross-community bridge._
- **Why does `SharesAtoms` connect `Inventory & Position Tracking` to `Entry Point & Feed IO`, `Fixed-Point Math Utilities`, `PriceTick Type`, `Shares4 Type`, `Shares2 Type`?**
  _High betweenness centrality (0.142) - this node is a cross-community bridge._
- **Why does `RuntimeCore` connect `Runtime Orchestration & Sizing` to `Market Event Parsing`?**
  _High betweenness centrality (0.114) - this node is a cross-community bridge._
- **Are the 13 inferred relationships involving `main()` (e.g. with `init_background_logger()` and `load_env_file()`) actually correct?**
  _`main()` has 13 INFERRED edges - model-reasoned connections that need verification._
- **Are the 9 inferred relationships involving `canonical_buy_target_for_notional()` (e.g. with `buy_size_multiple_taker_units()` and `floor_to_multiple()`) actually correct?**
  _`canonical_buy_target_for_notional()` has 9 INFERRED edges - model-reasoned connections that need verification._
- **What connects `AnchorResult`, `DerivedCredentials`, `ClobToken` to the rest of the system?**
  _32 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Inventory & Position Tracking` be split into smaller, more focused modules?**
  _Cohesion score 0.09 - nodes in this community are weakly interconnected._