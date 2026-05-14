# AGENTS.md

Rust-first low-latency FAK trading bot for Polymarket 5-minute binary options.

---

## Guiding Principles

1. **Evidence first.** Source code, tests, Graphify, CI output, and live logs are authoritative. AI reports, commit messages, and comments are hypotheses until verified.

2. **Occam's razor.** Complexity must protect against a real live failure or it gets deleted. One source of truth. Direct functions. Bounded state. Explicit failure.

3. **No monkey job.** No fake safety gates. No defensive code for impossible states. No broad rewrites hiding clutter. No "robustness" layers without a proven failure mode. If it doesn't protect live correctness, reduce latency, enforce precision, or make docs truthful — don't add it.

4. **Hunt overwiring.** Treat overcomplication as a live-risk smell. Actively remove speculative abstractions, fallback ladders without a timed/typed invariant, duplicate sources of authority, state machines for one-bit facts, helper functions used only to satisfy test scaffolding, adapters around direct calls, "future-proof" knobs with no operator decision, cached/stale artifacts presented as truth, and any code path whose only defense is "maybe useful later." Classify these as P2 unless they can alter trading, inventory, signing, or rotation; then classify as P1/P0.

5. **Less code is good code.** Every pub function must have a live caller. Every struct field must be read. Every enum variant must be constructed. Dead code is a bug.

6. **Hot path discipline.** Binance tick → BUY submit must cross the fewest useful functions, locks, and allocations. No blocking I/O, no pretty-printing, no subprocess wrappers, no unnecessary branches.

7. **Fixed-point precision.** No f64 crosses the signed body boundary. Venue-facing values are integer ticks/cents/atoms. Silent rounding is forbidden.

8. **WSS is inventory truth.** User WSS trade events own inventory. MATCHED applies inventory immediately for SELL; CONFIRMED is idempotent finality; FAILED after MATCHED reverses. HTTP responses classify outcomes but don't own inventory. User WSS must subscribe to the active condition ID and receive rotation subscription updates. Trust starts false, granted on successful auth frame send (venue has no explicit auth ACK per official SDK — invalid creds cause server disconnect), revoked on disconnect/error.

9. **FAK rejection is cheap, submit storms are not.** Don't over-protect BUY no-match, SELL no-match, or definitive SELL balance rejection. Rejected BUY deletes the claim. SELL does not own inventory, but SELL submission is single-flight per token until the HTTP outcome returns; this prevents repeated full-size FAKs from colliding with venue-side matched/open reservations during transport uncertainty.

10. **Official docs rule.** When in doubt, consult Polymarket/Binance official docs. Priority: live evidence > official docs > source code > tests > Graphify > comments > AI summaries.

---

## Runtime Rules

### Inventory

- Inventory applies on **MATCHED** (not CONFIRMED — MATCHED is the first on-chain signal; SELL fires immediately). CONFIRMED is idempotent. FAILED after MATCHED reverses the delta.
- Pending claim stays alive until terminal status (CONFIRMED/FAILED) to block duplicate BUY.
- WSS-confirmed trade removes pending claim. Inventory is then the sole authority.

### BUY Lifecycle

- Claim created atomically with intent (same `core.lock()` scope).
- Dry-run does not create claims.
- Rejected → claim deleted (no tombstone).
- UNKNOWN → stays WSS-matchable, blocks same-token BUY.
- Accepted → does not blindly expire.

### SELL Lifecycle

- BUY MATCHED starts a bid tracker from WSS fill price.
- Exit wakes every 50ms: update peak bid, sell when bid drops `EXIT_DROP_TICKS` from protected bid after profit arm (`EXIT_ARM_TICKS`) or below entry, or on hold timeout (`EXIT_HOLD_US`).
- When exit fires: read sellable inventory, read bid, sign FAK SELL, submit, log.

- Read sellable inventory → read bid → sign FAK SELL → submit → log.
- Inventory remains WSS-owned; HTTP SELL responses do not mutate balance.
- At most one SELL submit may be in flight per token. The next retry is allowed only after the prior HTTP outcome returns.
- No cooldown knobs, balance locks, REST reconciliation, force-exit tasks, or pending-inventory state.
- Exit task wakes every 50ms, but token-level single-flight bounds submit concurrency.

### Market Rotation

- Discovery and rotation have one source of truth: Gamma by slug timestamp.
- Ordinary discovery is initial/current slug only. It must not promote the next window early.
- Rotation is scheduled exactly 5 seconds before current market expiry (`end_ts - 5s`) and discovers only the exact next slug (`slug_ts = current.end_ts`).
- Old inventory/pending/state is forgotten. Old markets resolve on-chain at expiry.
- Signal ring cleared on rotation (prevents stale momentum).
- Strike disabled until anchor resolves for the new market.

### WebSockets

- Market WSS subscribes by token/asset IDs: `assets_ids = [yes_token, no_token]`.
- User WSS subscribes by market condition IDs: `markets = [condition_id]`.
- On Gamma rotation, the same `MarketContext` must update both channels: market WSS gets YES/NO token IDs; user WSS gets the condition ID.
- Missing user-channel market subscription is a P0/P1 inventory-truth failure, because matched/confirmed trades may not reach the runtime.

### Signing

- Local EIP-712 V2 signing. No SDK order builder on the signal path.
- Signature kind/funder config fails closed on invalid values.
- CLOB host (`clob.polymarket.com`), domain version "2", pUSD collateral.
- `clob-v2.polymarket.com` is a 301 redirect — POST must go to `clob.polymarket.com`.
- EOA address used for L2 auth headers when credentials derived from PK.

---

## Forbidden Patterns

- flat-start position checks (WSS authority handles restart-with-position)
- rotation blockers (old markets resolve automatically)
- force-exit tasks (50ms exit task owns bid-trailing SELL)
- max-position caps (same-token duplicate protection is sufficient in 2-token markets)
- max-TTE gates (the 5-min market window IS the product boundary)
- SELL inventory state of any kind
- overlapping SELL submits for the same token
- periodic rediscovery that can promote a future market before `end_ts - 5s`
- old-market pending/inventory reconciliation
- SDK network calls on the signal path
- subprocess wrappers, JSON file writes, raw event dumps on hot path
- broad `#[allow]` attributes
- dead pub symbols (functions, types, constants with zero live callers)

---

## Validation

After any change:

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

Stale-symbol grep:
```bash
rg -n "create_and_post_order|SubmitStatus::Rejected|record_sell_submit_outcome|mark_submit_rejected|MINIRUST_CONDITION_ID|cached signed body|std::process::Command|serde_json::to_writer|pretty|raw event|max_decision_tte|EXCHANGE_V2_NEG_RISK|SHARES4_PER_SHARE" src/
```

Update Graphify after structural changes.

---

## Review Workflow

Before editing:

1. Read this file + README.md.
2. Read Graphify output (`graphify-out/GRAPH_REPORT.md`).
3. Read the source files you're changing + their callers.
4. Run `git status --short` and `git log --oneline -8`.
5. Produce an evidence map before editing.

---

## Severity

- **P0**: Unintended exposure, duplicate BUY, trading without inventory truth, wrong signing/account mode, invalid venue-facing body.
- **P1**: Blocks trading indefinitely, hides liveness failure, pollutes hot-path state.
- **P2**: Docs/tests/Graphify mismatch, stale symbols, non-hot-path cleanup.
- **P3**: Style only.

---

## Production Gate

All must be true:

- startup fails fast on missing credentials
- user WSS trust gates BUY (trust on auth frame sent, revoked on disconnect)
- user WSS subscription includes active condition ID and updates on rotation
- BUY claim atomic with intent, deleted on rejection, removed on CONFIRMED
- UNKNOWN stays matchable, Accepted doesn't expire blindly
- BUY MATCHED arms bid-trailing exit; exit fires on `drop` or `hold`
- SELL submit concurrency is single-flight per token; SELL does not own inventory
- inventory applies on MATCHED; CONFIRMED is idempotent; FAILED reverses
- decimal validation is fixed-point
- signature kind/funder fails closed
- rotation occurs only at `end_ts - 5s`, forgets old state, clears signal ring
- no periodic next-window promotion before the scheduled rotation deadline
- no SDK order builder on signal path
- docs/tests/Graphify match runtime
- `cargo fmt` + `cargo test` + `cargo clippy -D warnings` pass
- stale-symbol grep clean

Shadow/live evidence still needed before deployment:

- user WSS auth success observed in logs
- Gamma discovery + rotation observed
- Binance signal → BUY submit → outcome classification observed
- WSS trade → inventory update → SELL fire observed

---

## Official Docs Used

- Polymarket POST order: https://docs.polymarket.com/api-reference/trade/post-a-new-order
- Polymarket create order: https://docs.polymarket.com/trading/orders/create
- Polymarket user WSS API: https://docs.polymarket.com/api-reference/wss/user
- Polymarket market WSS API: https://docs.polymarket.com/api-reference/wss/market
- Polymarket user channel guide: https://docs.polymarket.com/market-data/websocket/user-channel
- Polymarket market channel guide: https://docs.polymarket.com/market-data/websocket/market-channel

Runtime conclusions from those docs:

- Market channel subscriptions are asset/token scoped.
- User channel subscriptions are condition/market scoped.
- User channel trade statuses include MATCHED, CONFIRMED, and FAILED lifecycle events.
- SELL failures with insufficient balance/allowance are venue-side rejections; they must not create local SELL state.

---

## Safety

- Do not run live trading, submit orders, or deploy unless explicitly authorized.
- Never print/commit `.env`, private keys, API secrets, auth headers, or wallet keys.
