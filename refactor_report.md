# Production Refactor Report

## Executive Summary

This report covers the multi-phase hardening of `minirust` from an initial Phase 1+2+3a prototype (types, config, logline, canonical body, L2 auth) through to a production-candidate runtime. Each phase addressed live-risk invariants discovered through systematic audit. The current state passes 117 tests with zero clippy warnings and has a CI workflow enforcing both on every push.

## Phase 2 — WSS Authority, Flat-Start, Credential Hardening (commits f91d1c1, 05802f1, 0515210)

### User WSS Authority Gate (P0)
- **`src/user.rs`**: Introduced `UserMessage` enum (`AuthSuccess`, `AuthError(String)`, `Trades(Vec<UserTrade>)`, `Other`). Refactored `parse_user_trades` into `parse_user_message` to correctly extract and classify WSS auth frames.
- **`src/inventory.rs`**: Added `user_wss_trusted: bool` (default `false`). `has_entry_exposure_or_pending()` short-circuits to `true` (blocking all BUY) when untrusted.
- **`src/runtime.rs`**: `apply_user_raw` returns `Result<UserMessage, RuntimeError>` instead of `Result<usize, RuntimeError>`.
- **`src/main.rs`**: User feed loop matches against `UserMessage::AuthSuccess`/`AuthError`, toggling `set_user_wss_trusted` accordingly.

### BUY Claim Atomicity (P0)
- **`src/inventory.rs`**: Added `claim_entry()` and `release_claim()` for synchronous BUY duplicate protection. The claim is registered under the same mutex lock that produced the `BuyIntent`, closing the race where a second tick could pass `has_entry_exposure_or_pending()` between intent production and `register_submit` inside an async spawn.
- **`src/runtime.rs`**: `prepare_buy_submit` now takes a pre-claimed `SubmitId` instead of `&mut Inventory` + timestamp.

### Flat-Start Integrity (P0)
- **`src/submit.rs`**: `verify_flat_start` runs `GET /orders?status=OPEN` alongside the existing `/positions` check. Fails-closed if any open orders exist. Position parsing made fail-closed: missing/malformed size returns `Err` instead of silently treating as flat.

### Accepted BUY Expiry Fix (P0)
- **`src/inventory.rs`**: `expire_unknowns` strictly targets `SubmitStatus::Unknown`. `Accepted` orders are excluded from the 30-second timeout and remain until reconciled by WSS terminal events.

### Configurable Signature Kind (P1)
- **`src/config.rs`**: `LaunchConfig` gained `poly_signature_kind: SignatureKind` and `poly_funder: Option<String>`. `POLY_SIGNATURE_KIND` unknown values are fatal (`ConfigError::Invalid`) — no silent fallback to EOA.
- **`src/main.rs`**: `OrderSigner::new` receives config-driven signature kind and funder instead of hardcoded `Eoa` / `None`.

### Dynamic Market Discovery (P0)
- **`src/gamma.rs`**: Gamma REST API client with `connect_timeout(500ms)` + `timeout(5s)` on reqwest client. Slug template substitution, CLOB token parsing with YES/NO label matching.
- **`src/config.rs`**: `LaunchConfig` gained `market_slug_fmt`, `market_window_s`, `clob_url`, `gamma_url`, and `binance_api_key` fields.

### Feed IO Layer (P0)
- **`src/ws.rs`**: WebSocket connect with optional API key header, exponential backoff (3 presets matching Python values), app-level ping loop.
- **`src/feed.rs`**: Three async feed loops (market, binance, user) each with independent reconnect logic, subscribe/auth frames, and synchronous callbacks (`Fn(Bytes)`).
- **`src/anchor.rs`**: Anchor strike resolution from Binance microprice samples with normal (300ms window) and late-discovery fallback paths. Median computation with minimum 3 samples.

## Phase 3 — Auth Parser, WSS Disconnect Trust, Atomic Claim, Flat-Start Fail-Closed (commit d5405f6)

### Auth Parser Fix (P0-1a)
- **`src/user.rs`**: `parse_user_message` now handles the real Polymarket auth frame shape `{"event_type":"auth","status":"SUCCESS"}`. Added `event_type == "auth"` branch before the generic error/success fallthrough. Added 3 tests: `parses_auth_success`, `parses_auth_error_with_message`, `auth_unknown_status_returns_other`.

### WSS Disconnect Trust Revocation (P0-1b)
- **`src/feed.rs`**: `user_feed_loop` gained an `on_disconnect` callback parameter. Called before backoff sleep on every reconnect cycle.
- **`src/main.rs`**: `on_disconnect` callback sets `user_wss_trusted` to `false`, preventing BUY signals from firing on stale trust after a connection drop.

### BUY Claim Moved Into Lock (P0-2)
- **`src/main.rs`**: `claim_entry()` moved into the same `core.lock()` scope as `on_binance_sample()`. A second Binance tick cannot pass `has_entry_exposure_or_pending` between intent production and claim registration.

### Flat-Start Fail-Closed (P0-3)
- **`src/submit.rs`**: Position parsing uses `SharesAtoms::parse_decimal` instead of `f64` tolerance check. Missing/malformed size returns `Err`.

### Signature Kind Validation (P1-1)
- **`src/config.rs`**: Unknown `POLY_SIGNATURE_KIND` values return `ConfigError::Invalid` instead of silently defaulting to EOA.

### Dry-Run Claim Regression (P1)
- **`src/main.rs`**: `claim_entry()` only called in live mode (`if dry_run { None } else { Some((policy, claim_id)) }`). Shadow mode logs `shadow_buy_signal` without creating fake pending exposure that blocks future same-token BUYs.

### Doc/Comment Cleanup (P2)
- **`README.md`**: Module status table updated, static market env vars replaced with dynamic Gamma discovery docs, user WSS credential requirement documented.
- **`Cargo.toml`**: Fixed stale architecture comment ("cached signed body" → "on-demand FAK signing").
- **`src/main.rs`**: Force-exit comment fixed ("no limit" → "at best bid").

## Phase 4 — Pending State Minimization, Credential Fail-Fast, CI (commits b5f46d8, 91daa31)

### Entry Pending Removed on WSS Confirmation (P1)
- **`src/inventory.rs`**: `apply_user_trade()` now deletes matched Entry pending submits immediately after WSS trade confirmation. The pending Entry served its purpose (blocking duplicate BUYs between claim and WSS); inventory is now the authoritative source of truth.

### SELL Bookkeeping Removed (P1)
- **`src/inventory.rs`**: `SubmitIntent::Exit` variant removed. Only `Entry` remains.
- **`src/runtime.rs`**: `prepare_sell_submit()` and `prepare_sell_submit_for_size()` no longer call `inventory.register_submit()`. `PreparedSellSubmit` no longer has a `submit_id` field. `record_sell_submit_outcome()` removed entirely. Sell functions take `&Inventory` instead of `&mut Inventory`.
- **`src/main.rs`**: All `record_sell_submit_outcome` call sites removed from user-task sell path, exit task, and force-exit task.

### Rejected BUY Uses release_claim (P1)
- **`src/runtime.rs`**: `record_buy_submit_outcome` calls `inventory.release_claim(submit_id)` for `SubmitOutcome::Rejected` instead of `mark_submit_rejected`. FAK rejection is definitive no-order — no pending state persists.
- **`src/inventory.rs`**: `SubmitStatus::Rejected` variant removed. `mark_submit_rejected()` method removed.

### Credential Fail-Fast (P2)
- **`src/main.rs`**: Startup fails fast if `POLY_API_KEY`, `POLY_API_SECRET`, or `POLY_PASSPHRASE` are missing — even in dry-run mode. Clear fatal message explains that BUY signals are gated on authenticated WSS inventory truth.
- **`README.md`**: Shadow mode instructions updated to require all three credentials. PowerShell example includes `POLY_PASSPHRASE`.

### CI Workflow (P2)
- **`.github/workflows/ci.yml`**: GitHub Actions workflow enforcing `cargo fmt --check`, `cargo test`, and `cargo clippy -- -D warnings` on every push and PR to `main`.

### Comment Cleanup
- Removed all 9 "Delivered by DeepSeek" attribution comments from `src/ws.rs`, `src/feed.rs`, `src/main.rs`, `src/anchor.rs`, `src/gamma.rs`, `src/config.rs` (2 occurrences), `src/runtime.rs`, and `src/lib.rs`.

## First-Principles Runtime State Model

After all phases, the local runtime state model is:

- **BUY pending** exists only in `Pending | Accepted | Unknown` statuses. It blocks same-token re-entry while it protects against real duplicate exposure. It is deleted on WSS trade confirmation or on definitive rejection (`release_claim`).
- **SELL pending** does not exist. SELL is fire-and-forget — WSS trade events own inventory truth. FAK rejection is cheap and does not need local bookkeeping.
- **WSS owns inventory**. `user_wss_trusted` is `false` until the first `AuthSuccess` frame, and reverts to `false` on any disconnect. When untrusted, `has_entry_exposure_or_pending` returns `true`, blocking all BUY signals.
- **Rejected is deletion**, not status. A rejected FAK BUY is definitive no-order. The claim is removed from pending — it does not linger as a non-blocking tombstone that adds scan overhead.

## Code Quality & Verification

- **117 tests** pass across unit, integration, and doc tests.
- **Zero clippy warnings** (strictly enforced via `-D warnings`).
- **CI workflow** enforces `fmt`, `test`, and `clippy` on every push and PR.
- Release profile: LTO thin, single codegen unit, panic abort, stripped symbols.

## Production Readiness Status

| Item | Status |
|---|---|
| Fixed-point venue math | Done |
| Offline EIP-712 signing | Done |
| L2 auth | Done |
| Direct submit classifier | Done |
| WSS-authoritative inventory | Done |
| BUY claim atomicity | Done |
| Flat-start integrity | Done |
| Auth parser (real frame shape) | Done |
| WSS disconnect trust revocation | Done |
| Dry-run claim regression | Done |
| Entry pending removed on WSS match | Done |
| SELL bookkeeping removed | Done |
| Rejected BUY release_claim | Done |
| Credential fail-fast (all 3) | Done |
| CI workflow | Done |
| Stale comments removed | Done |
| EC2 shadow/live probe | Not yet |
| `/orders?status=OPEN` endpoint verification | Not yet |

## Commit History

| Commit | Description |
|---|---|
| `91daa31` | release_claim for rejected BUY, README passphrase, CI workflow, cleanup stale comments |
| `b5f46d8` | Remove stale BUY pending gating and SELL bookkeeping, add WSS credential fail-fast |
| `d5405f6` | P0 runtime defects — auth parser, WSS trust disconnect, BUY claim atomicity, flat-start fail-closed, signature kind validation |
| `f91d1c1` | Phase 2 production refactor — WSS authority gate, flat-start orders check, configurable signature kind, credential hardening |
| `05802f1` | P0 production hardening — fail-closed live mode, race-safe buy claims, flat-start integrity, rotation preconditions, parsing dedup |
| `0515210` | P0 runtime hardening |
| `ffd3caf` | Dynamic discovery, background logging, safety checks |
| `62fdda8` | Phase 3b — rip SDK runtime dep, sign EIP-712 inline + offline |
| `b00ec37` | Phase 3b — order signing via polymarket_client_sdk_v2 |
| `320b4af` | Initial — Phase 1+2+3a types, config, logline, canonical body, L2 auth |
