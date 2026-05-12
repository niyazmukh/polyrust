# Graph Report - minimal_rust  (2026-05-12)

## Corpus Check
- 20 files · ~26,346 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 442 nodes · 834 edges · 19 communities (18 shown, 1 thin omitted)
- Extraction: 91% EXTRACTED · 9% INFERRED · 0% AMBIGUOUS · INFERRED: 72 edges (avg confidence: 0.8)
- Token cost: 0 input · 0 output

## Graph Freshness
- Built from commit: `eb5f99c8`
- Run `git rev-parse HEAD` and compare to check if the graph is stale.
- Run `graphify update .` after code changes (no API cost).

## Community Hubs (Navigation)
- [[_COMMUNITY_Community 0|Community 0]]
- [[_COMMUNITY_Community 1|Community 1]]
- [[_COMMUNITY_Community 2|Community 2]]
- [[_COMMUNITY_Community 3|Community 3]]
- [[_COMMUNITY_Community 4|Community 4]]
- [[_COMMUNITY_Community 5|Community 5]]
- [[_COMMUNITY_Community 6|Community 6]]
- [[_COMMUNITY_Community 7|Community 7]]
- [[_COMMUNITY_Community 8|Community 8]]
- [[_COMMUNITY_Community 9|Community 9]]
- [[_COMMUNITY_Community 10|Community 10]]
- [[_COMMUNITY_Community 11|Community 11]]
- [[_COMMUNITY_Community 12|Community 12]]
- [[_COMMUNITY_Community 13|Community 13]]
- [[_COMMUNITY_Community 14|Community 14]]
- [[_COMMUNITY_Community 15|Community 15]]
- [[_COMMUNITY_Community 16|Community 16]]
- [[_COMMUNITY_Community 17|Community 17]]

## God Nodes (most connected - your core abstractions)
1. `Inventory` - 18 edges
2. `main()` - 16 edges
3. `canonical_buy_target_for_notional()` - 16 edges
4. `classify()` - 15 edges
5. `SharesAtoms` - 14 edges
6. `signature_recovers_to_signer_address_for_buy()` - 13 edges
7. `signature_recovers_for_many_salts_across_both_sides()` - 13 edges
8. `RuntimeCore` - 12 edges
9. `RuntimeState` - 12 edges
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

## Communities (19 total, 1 thin omitted)

### Community 0 - "Community 0"
Cohesion: 0.11
Nodes (33): address_derives_from_test_vector(), address_lower_hex(), address_to_uint256_be(), bytes32_hex(), compute_domain_separator(), derive_address(), domain_separator_for_polygon_v2_normal_is_stable(), encode_signature() (+25 more)

### Community 1 - "Community 1"
Cohesion: 0.1
Nodes (21): apply_inventory_delta(), confirmed_without_matched_still_recovers_inventory_once(), expire_pending_removes_only_old_pending_entries(), expire_pending_unblocks_same_token_buy(), Inventory, matched_does_not_apply_inventory_confirmed_does(), order(), PendingSubmit (+13 more)

### Community 2 - "Community 2"
Cohesion: 0.05
Nodes (17): buy_size_multiple_taker_units(), ceil_to_multiple(), ConditionId, decimal_scale_digits(), floor_to_multiple(), gcd(), maker_cents_for(), OrderId (+9 more)

### Community 3 - "Community 3"
Cohesion: 0.1
Nodes (22): binance_feed_loop(), binance_feed_receives_frames(), market_feed_loop(), user_feed_loop(), user_feed_sends_auth_frame_on_connect(), f64, Field, FieldValue (+14 more)

### Community 4 - "Community 4"
Cohesion: 0.13
Nodes (19): b(), classify(), extract_error_field(), http_200_success_false_is_rejected(), http_200_with_order_id_is_accepted(), http_200_without_order_id_is_unknown(), http_400_is_rejected_regardless_of_body(), http_400_with_min_size_error_is_rejected() (+11 more)

### Community 5 - "Community 5"
Cohesion: 0.12
Nodes (18): cfg_from_pairs(), Config, config_accepts_dry_run_only(), config_accepts_live_only(), config_builds_signal_and_buy_submit_policy_from_runtime_env_shape(), config_rejects_both_dry_run_and_live_false(), config_rejects_both_dry_run_and_live_true(), ConfigError (+10 more)

### Community 6 - "Community 6"
Cohesion: 0.17
Nodes (20): address_lower_hex(), address_to_uint256_be(), clob_auth_struct_hash(), compute_clob_auth_domain(), decode_secret_b64_padded(), derive_address(), derive_api_credentials(), DerivedCredentials (+12 more)

### Community 7 - "Community 7"
Cohesion: 0.1
Nodes (11): BuySubmitPolicy, on_binance_sample(), plan_sell_at_bid(), plan_sell_for_size_at_bid(), prepare_buy_submit(), PreparedBuySubmit, PreparedSellSubmit, record_buy_submit_outcome() (+3 more)

### Community 8 - "Community 8"
Cohesion: 0.16
Nodes (15): BuyCanonicalError, BuyCanonicalInput, BuyCanonicalPolicy, BuyCanonicalTarget, canonical_buy_target_for_notional(), canonical_sell_params(), input(), p036_target_101_lattice_gap_accepts_ceil() (+7 more)

### Community 9 - "Community 9"
Cohesion: 0.19
Nodes (13): ClobToken, GammaClient, parse_clob_tokens(), parse_gamma_iso8601(), parse_iso8601_golden_midnight_utc(), parse_iso8601_golden_noon_utc(), parse_iso8601_with_fractional(), parse_iso8601_with_offset_timezone() (+5 more)

### Community 10 - "Community 10"
Cohesion: 0.28
Nodes (12): AnchorBuffer, AnchorResult, failed_when_window_closed_with_fewer_than_3(), late_discovery_uses_all_samples_after_window_start(), median(), no_pending_returns_pending(), normal_resolution_with_4_samples_uses_mean_of_middle_two(), normal_resolution_with_exactly_3_samples() (+4 more)

### Community 11 - "Community 11"
Cohesion: 0.19
Nodes (15): apply_market_events(), best_book_price(), MarketEvent, MarketParseError, optional_price(), optional_str(), parse_level(), parse_market_events() (+7 more)

### Community 12 - "Community 12"
Cohesion: 0.19
Nodes (15): AuthError, auth_unknown_status_returns_other(), is_trade_event(), optional_i64(), optional_str(), parse_side(), parse_trade_value(), parse_user_message() (+7 more)

### Community 13 - "Community 13"
Cohesion: 0.17
Nodes (7): BinanceSample, BuyIntent, phi(), phi_tail(), SignalConfig, SignalEngine, SignalPoint

### Community 14 - "Community 14"
Cohesion: 0.2
Nodes (11): BinanceBookTicker, BinanceParseError, mantissa_to_f64(), normalize_epoch_to_us(), parse_book_ticker(), parse_book_ticker_json(), parse_positive_f64_json(), parse_positive_i64_json() (+3 more)

### Community 15 - "Community 15"
Cohesion: 0.17
Nodes (3): MarketContext, Quote, RuntimeState

### Community 16 - "Community 16"
Cohesion: 0.42
Nodes (6): Backoff, backoff_binance_preset(), backoff_converges_to_max(), backoff_market_preset(), backoff_reset_restores_initial(), backoff_user_preset()

## Knowledge Gaps
- **31 isolated node(s):** `AnchorResult`, `DerivedCredentials`, `ClobToken`, `PendingSubmit`, `UserTrade` (+26 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **1 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `main()` connect `Community 3` to `Community 1`, `Community 5`, `Community 6`, `Community 7`, `Community 14`?**
  _High betweenness centrality (0.244) - this node is a cross-community bridge._
- **Why does `SharesAtoms` connect `Community 1` to `Community 8`, `Community 2`, `Community 3`?**
  _High betweenness centrality (0.125) - this node is a cross-community bridge._
- **Why does `RuntimeCore` connect `Community 7` to `Community 11`?**
  _High betweenness centrality (0.092) - this node is a cross-community bridge._
- **Are the 13 inferred relationships involving `main()` (e.g. with `init_background_logger()` and `load_env_file()`) actually correct?**
  _`main()` has 13 INFERRED edges - model-reasoned connections that need verification._
- **Are the 9 inferred relationships involving `canonical_buy_target_for_notional()` (e.g. with `buy_size_multiple_taker_units()` and `floor_to_multiple()`) actually correct?**
  _`canonical_buy_target_for_notional()` has 9 INFERRED edges - model-reasoned connections that need verification._
- **Are the 8 inferred relationships involving `SharesAtoms` (e.g. with `.owned_atoms()` and `trade()`) actually correct?**
  _`SharesAtoms` has 8 INFERRED edges - model-reasoned connections that need verification._
- **What connects `AnchorResult`, `DerivedCredentials`, `ClobToken` to the rest of the system?**
  _31 weakly-connected nodes found - possible documentation gaps or missing edges._