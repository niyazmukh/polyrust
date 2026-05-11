# Phase 2 Production Refactor Report

## Executive Summary
This report details the comprehensive Phase 2 refactoring executed to harden the `minirust` trading bot against critical production blockers. The focus was on strict safety invariants, authoritative WSS inventory gating, flat-start safety checks, and secure credential parsing. All changes have been directly synced to the `minimal_rust` repository.

## 1. User WSS Authority (P0)
**Objective**: Enforce the `User WSS` connection as the absolute authority for executing BUY intents, preventing trades while disconnected or unauthenticated.

* **`src/user.rs`**: 
  * Introduced the `UserMessage` enum to represent `AuthSuccess`, `AuthError`, and `Trades`.
  * Refactored `parse_user_trades` into `parse_user_message` to correctly extract and classify WSS auth frames (`{"event_type":"auth", "status":"SUCCESS"}`).
* **`src/inventory.rs`**:
  * Added `user_wss_trusted: bool` state to the `Inventory` struct (defaults to `false`).
  * Updated `has_entry_exposure_or_pending` to immediately short-circuit and return `true` (blocking execution) if `user_wss_trusted` is `false`.
* **`src/runtime.rs`**:
  * Updated the signature of `apply_user_raw` to return `Result<UserMessage, RuntimeError>` instead of the raw trades list.
* **`src/main.rs`**:
  * Rewired the `user_feed_loop` to match against `UserMessage::AuthSuccess` / `AuthError`.
  * Triggered `inventory.set_user_wss_trusted(true)` upon successful authentication, and immediately revoked trust (`false`) on drops, parsing errors, or reconnection attempts.

## 2. Accepted BUY Expiry Race Condition (P0)
**Objective**: Prevent `SubmitStatus::Accepted` orders from silently expiring due to missing WSS confirmation events, which caused duplicate execution entries.

* **`src/inventory.rs`**:
  * Modified the `expire_unknowns` function to strictly target `SubmitStatus::Unknown`. 
  * `SubmitStatus::Accepted` orders are now explicitly excluded from the `20` second timeout and remain permanently in the pending queue until properly reconciled by terminal WSS match/cancel events or manual intervention.

## 3. Flat-Start Open Orders & Credentials (P0/P1)
**Objective**: Guarantee that the bot never starts running with preexisting resting limit orders, and enforce robust credential validation.

* **`src/submit.rs`**:
  * Expanded `verify_flat_start` to execute a `GET /orders?status=OPEN` query alongside the existing `/positions` check. 
  * Fail-closed mechanism actively prevents `live` mode execution if the response JSON array has a length greater than 0.
* **`src/main.rs`**:
  * Hardened the live-mode pre-flight checks. `POLY_PASSPHRASE` and `POLY_ADDRESS` are now strictly validated as non-empty variables; partial configuration immediately triggers a `std::process::exit(2)` with a descriptive fatal error.

## 4. Explicit Signature Kind and Funder Config (P1)
**Objective**: Transition from hardcoded EOA assumptions to support configurable proxy signatures.

* **`src/config.rs`**:
  * Enhanced `LaunchConfig` with two new fields: `poly_signature_kind: minirust::signing::SignatureKind` and `poly_funder: Option<String>`.
  * Parsed the `POLY_SIGNATURE_KIND` env var into the `SignatureKind` enum (`PolyGnosisSafe`, `PolyProxy`, `Eoa`).
* **`src/main.rs`**:
  * Fed the newly loaded `launch.poly_signature_kind` and `launch.poly_funder` straight into `OrderSigner::new` instead of hardcoding `SignatureKind::Eoa` and `None`.

## 5. Pre-signed Templates & Stale Docs (P1/P2)
**Objective**: Clean up documentation and stale assertions regarding cached signing bodies and static market limits.

* **`README.md`**:
  * Removed deprecated claims about "cached signed bodies". Updated to document the synchronous, on-demand FAK signing methodology used by the `OrderSigner` component.
  * Corrected references pointing to static shadow mode configs; added references to the `Gamma` dynamic discovery API.
* **`src/config.rs` & `src/gamma.rs`**:
  * Renamed and updated `launch_config_requires_static_market` to `launch_config_loads_properly` and utilized `MINIRUST_MARKET_SLUG_FMT`.
  * Cleaned up nested `if` statements and `clippy::collapsible_if` warnings within `parse_clob_tokens` via Rust 1.65+ `let` chains.

## Code Quality & Verification
* Full test suite passed across `81` unit and integration tests.
* Zero `clippy` warnings (strictly enforced by `-D warnings`).
* Test compilation errors regarding `H160` parsing and `SignatureKind` enum variant mismatches were proactively resolved. 
* Total synchronization achieved; codebase natively runs on `minimal_rust`.
