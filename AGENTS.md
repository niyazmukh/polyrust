# AGENTS.md

Rust-first low-latency FAK trading bot for Polymarket 5-minute binary options.

---

## Guiding Principles

1. **Evidence first.** Source code, tests, Graphify, CI output, and live logs are authoritative. AI reports, commit messages, and comments are hypotheses until verified.

2. **Occam's razor.** Complexity must protect against a real live failure or it gets deleted. One source of truth. Direct functions. Bounded state. Explicit failure.

3. **No monkey job.** No fake safety gates. No defensive code for impossible states. No broad rewrites hiding clutter. No "robustness" layers without a proven failure mode. If it doesn't protect live correctness, reduce latency, enforce precision, or make docs truthful — don't add it.

4. **Less code is good code.** Every pub function must have a live caller. Every struct field must be read. Every enum variant must be constructed. Dead code is a bug.

5. **Hot path discipline.** Binance tick → BUY submit must cross the fewest useful functions, locks, and allocations. No blocking I/O, no pretty-printing, no subprocess wrappers, no unnecessary branches.

6. **Fixed-point precision.** No f64 crosses the signed body boundary. Venue-facing values are integer ticks/cents/atoms. Silent rounding is forbidden.

7. **WSS is inventory truth.** User WSS CONFIRMED trades own the balance. HTTP responses classify outcomes but don't own inventory. Trust starts false, granted only on venue AuthSuccess, revoked on disconnect/error.

8. **FAK rejection is cheap.** Don't over-protect BUY no-match, SELL no-match, or SELL balance rejection. Rejected BUY deletes the claim. SELL creates zero local state.

9. **Official docs rule.** When in doubt, consult Polymarket/Binance official docs. Priority: live evidence > official docs > source code > tests > Graphify > comments > AI summaries.

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

- Read sellable inventory → read bid → sign FAK SELL → submit → log.
- No SELL reservations, locks, cooldowns, balance locks, in-flight blockers, or pending state.
- Exit task fires every 50ms. That's the only sell mechanism needed.

### Market Rotation

- Unconditional. When Gamma discovers a new market, rotate immediately.
- Old inventory/pending/state is forgotten. Old markets resolve on-chain at expiry.
- Signal ring cleared on rotation (prevents stale momentum).
- Strike disabled until anchor resolves for the new market.

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
- force-exit tasks (exit task already sells everything)
- max-position caps (same-token duplicate protection is sufficient in 2-token markets)
- max-TTE gates (the 5-min market window IS the product boundary)
- SELL state of any kind
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
- user WSS trust gates BUY (AuthSuccess required, revoked on disconnect)
- BUY claim atomic with intent, deleted on rejection, removed on CONFIRMED
- UNKNOWN stays matchable, Accepted doesn't expire blindly
- SELL has zero local state
- inventory applies on MATCHED; CONFIRMED is idempotent; FAILED reverses
- decimal validation is fixed-point
- signature kind/funder fails closed
- rotation is unconditional, forgets old state, clears signal ring
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

## Safety

- Do not run live trading, submit orders, or deploy unless explicitly authorized.
- Never print/commit `.env`, private keys, API secrets, auth headers, or wallet keys.
