# AGENTS.md

## Purpose

This repository contains a Rust-first low-latency trading bot for Polymarket-style FAK execution.

Every agent working here must optimize for:

- live-risk correctness,
- minimal runtime state,
- hot-path speed,
- fixed-point precision,
- WSS-owned inventory truth,
- simple Rust-native invariants,
- evidence over summaries.

This is not a Python port. Do not preserve Python-era architecture unless it protects against a real live failure.

---

## Non-Negotiable Principles

### 1. Evidence First

Source code, tests, Graphify, CI output, logs, and actual runtime behavior are authoritative.

Do not trust:

- AI reports,
- commit messages,
- README claims,
- comments,
- old architecture plans,

unless verified against current source/runtime.

Every serious claim must cite exact file/function/path evidence.

---

### 2. Occam’s Razor

Complexity must pay rent.

Delete complexity unless it protects against a real live failure.

Prefer:

- one source of truth,
- direct functions,
- small Rust-native types,
- bounded state,
- explicit failure.

Avoid:

- reconciliation layers,
- manager/orchestrator sprawl,
- dead compatibility paths,
- state kept “just in case.”

---

### 3. No Monkey Job

Do not add code merely to satisfy a reviewer or previous AI report.

Forbidden:

- fake safety gates,
- broad rewrites hiding clutter,
- hidden SDK global mutation,
- local state that does not protect exposure,
- “robustness” layers without a proven failure mode,
- defensive code for impossible internal states.

If a change does not protect live correctness, reduce latency, enforce precision, or make docs/tests truthful, do not add it.

---

## Runtime Doctrine

### WSS Authority

User WSS trades are inventory truth.

HTTP submit responses are useful for immediate classification only. They are not final inventory truth.

Required behavior:

- user WSS trust starts false,
- BUY cannot pass while user WSS is untrusted,
- auth success makes user WSS trusted,
- auth error, parse error, disconnect, or feed failure revokes trust,
- WSS trade applies inventory deltas,
- WSS-confirmed BUY removes local pending claim.

---

### FAK Rejection Is Cheap

FAK no-match rejection is cheap.

Do not over-protect:

- BUY no-match,
- SELL no-match,
- SELL balance rejection.

Rejected BUY is definitive no-order state and must release local claim.

SELL rejection must not create reservations, cooldowns, locks, or persistent local state.

---

### BUY Duplicate Protection

BUY protection exists only where it prevents real duplicate exposure.

Allowed BUY protection:

- atomic entry claim,
- same-token pending exposure block,
- WSS-owned inventory block,
- max concurrent exposure cap,
- UNKNOWN submit matchability.

Required behavior:

- BUY claim is created in the same critical section as BUY intent creation,
- dry-run does not create fake claims,
- rejected BUY deletes claim,
- WSS-confirmed BUY deletes claim,
- UNKNOWN remains WSS-matchable while useful,
- Accepted must not blindly expire into a non-blocking state.

Do not add broad BUY cooldowns unless live evidence proves they are necessary.

---

### SELL Must Stay Under-Gated

SELL must be simple.

Required behavior:

- read WSS-owned sellable inventory,
- read current bid,
- sign FAK SELL,
- submit,
- log outcome.

Forbidden:

- SELL reservations,
- SELL locks,
- SELL balance locks,
- SELL cooldowns,
- SELL in-flight blockers,
- persistent SELL pending state.

SELL state should not exist unless hard live evidence proves it prevents a real failure.

---

### Market Rotation

Market rotation is a hard boundary.

At market rotation:

- previous market assets become unsellable,
- previous market orders are no longer actionable,
- previous inventory/assets/orders should be forgotten,
- new market context becomes the only active trading scope.

Given the 45-second no-entry-before-expiry gate, no FAK entry order should remain realistically pending across rotation. Do not add reconciliation logic for old pending FAK orders.

Required behavior:

- no BUY on stale strike,
- strike reset or BUY disabled on rotation until current anchor resolves,
- active market context switches cleanly to the newly discovered market,
- previous market inventory/assets/orders are released/dropped as dead state.

Forbidden monkey work:

- preserving old market inventory for post-rotation SELL,
- reconciling old pending FAK orders after rotation,
- blocking rotation because old pending FAK submits exist,
- building multi-market inventory drain logic,
- retaining dead market state “just in case.”

Only revisit this rule if official docs or live evidence prove that old assets remain sellable/actionable after rotation.

---

### Decimal Precision

Decimal precision is non-negotiable.

Venue-facing signed bodies must use fixed-point validation.

Forbidden:

- silent rounding,
- float-derived order body fields,
- unchecked decimal strings,
- accepting invalid venue-facing precision.

Required behavior:

- signed body fields validate locally before submit,
- prices and sizes use fixed-point domain types,
- malformed decimal env/config/body values fail closed.

---

### Signing and Submit

No full SDK order builder on signal.

Forbidden on the signal path:

- `create_and_post_order()`,
- SDK network/order-builder mutation,
- hidden global SDK state.

Allowed:

- local signing,
- direct FAK body submit,
- fresh L2 headers,
- explicit signature kind/funder config.

Signature kind and funder configuration must fail closed on invalid values.

---

## Hot Path Discipline

The Binance tick → BUY submit path must be short and obvious.

Audit for:

- duplicate parsing,
- unnecessary locks,
- unnecessary branches,
- blocking logs,
- JSON pretty printing,
- subprocess wrappers,
- raw event dumps,
- local state scans that do not protect exposure.

Forbidden on hot path:

- subprocess wrappers,
- JSON file writes,
- raw event pretty-printing,
- broad debug dumps,
- unnecessary allocation-heavy transformations.

Compact non-blocking logging is acceptable.

---

## Graphify Requirement

Graphify is evidence, not decoration.

Before major review/refactor:

1. Locate Graphify output.
2. Trace runtime flow through graph.
3. Compare graph against source.
4. Report stale or missing graph nodes.
5. Update/rebuild Graphify after runtime changes if tooling is available.

Source wins over graph, but graph/source mismatch is a validation issue.

Trace at least:

- startup,
- config/env loading,
- live/dry-run gate,
- flat-start check,
- Gamma discovery,
- market rotation,
- market WSS,
- Binance WSS,
- signal decision,
- BUY claim,
- BUY signing,
- HTTP submit,
- submit outcome handling,
- user WSS auth,
- user WSS disconnect trust revocation,
- user WSS trade parsing,
- inventory mutation,
- SELL path,
- force-exit,
- maintenance loop.

---

## Required Review Workflow

Before editing:

1. Read this file.
2. Read `README.md`.
3. Read local reports only as hypotheses.
4. Read Graphify output.
5. Read runtime source:
   - `src/main.rs`
   - `src/runtime.rs`
   - `src/inventory.rs`
   - `src/submit.rs`
   - `src/signing.rs`
   - `src/auth.rs`
   - `src/user.rs`
   - `src/feed.rs`
   - `src/ws.rs`
   - `src/gamma.rs`
   - `src/config.rs`
   - `src/signal.rs`
   - `src/anchor.rs`
   - `src/orders.rs`
   - `src/types.rs`
6. Run:
   ```bash
   git status --short
   git log --oneline -8
````

Produce an evidence map before editing.

---

## Runtime Flows to Trace

Every comprehensive review must trace:

### Startup / Dry-Run

`main` → config → credential checks → signer/auth construction → flat-start behavior → feed spawn → dry-run BUY behavior.

### Startup / Live

`main` → live gate → L2 auth → signer → submitter → flat-start → WSS prerequisites → feed startup.

### Gamma / Rotation

maintenance loop → Gamma client → market selection → anchor resolution → state update → strike reset → old inventory precondition.

### Binance Hot Path

raw frame → parse/sample → signal buffer → signal decision → WSS trust gate → exposure gate → BUY claim → dry-run/live branch → submit task.

### BUY Submit

claim id → prepare BUY → sign → decimal validation → submitter → classify outcome → record outcome → release/retain pending logic.

### User WSS

connect → auth frame → auth success/error parsing → trust mutation → trade parsing → pending match → inventory mutation.

### SELL

WSS-owned inventory → bid availability → prepare sell → sign → submit → no local SELL state.

### Failure / Reconnect

market WSS disconnect, user WSS disconnect, Binance disconnect, HTTP timeout, HTTP 4xx rejection, HTTP 5xx/transport unknown.

---

## Stale-Symbol Grep

Run after meaningful refactors:

```bash
rg -n "create_and_post_order|SubmitStatus::Rejected|SubmitIntent::Exit|record_sell_submit_outcome|mark_submit_rejected|Delivered by DeepSeek|MINIRUST_CONDITION_ID|MINIRUST_MARKET_SLUG|cached signed body|std::process::Command|serde_json::to_writer|pretty|raw event" .
```

Interpret results carefully.

Hits in ignored local reports may be irrelevant.

Hits in active runtime/docs/tests are likely stale-symbol bugs.

---

## Validation Commands

After any runtime change:

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

Also run stale-symbol grep and update/read Graphify.

Do not suppress warnings with broad `allow` attributes unless narrowly justified.

---

## Official Docs Rule

When in doubt, consult official docs before changing runtime behavior.

This applies especially to:

- Polymarket CLOB order semantics,
- FAK / FOK / GTC behavior,
- `/order`, `/orders`, `/positions`, `/data/orders` endpoint paths and schemas,
- L2 authentication headers,
- EIP-712 signing fields,
- signature kind / funder / proxy-wallet behavior,
- user WSS auth frames,
- user WSS trade event schemas,
- market WSS book/update schemas,
- Gamma market discovery fields,
- Binance stream payloads and timestamps.

Priority order for disputed facts:

1. actual live/runtime evidence,
2. official venue/API docs,
3. source code,
4. focused tests,
5. Graphify,
6. README/comments/reports,
7. AI summaries.

If official docs are unavailable or inconclusive, say so explicitly and do not invent behavior. Mark the point as requiring live/shadow proof.

---

## Documentation Rule

Docs must describe actual runtime.

README, env examples, tests, comments, and Graphify must not describe:

* old static-market architecture,
* placeholder binaries,
* removed submit states,
* removed env vars,
* intended future behavior as if it is current behavior.

If docs and runtime disagree, fix docs or source. Do not leave both.

---

## Safety Rules

Do not run live trading unless explicitly authorized.

Do not submit orders unless explicitly authorized.

Do not deploy to EC2 unless explicitly authorized.

Do not print or commit:

* `.env`,
* `.env.poly`,
* private keys,
* API secrets,
* auth headers,
* wallet keys,
* SSH material.

Never expose secrets in logs, reports, tests, or screenshots.

---

## Severity Model

### P0

Can cause unintended exposure, duplicate BUY exposure, trading without inventory truth, startup with unknown position/order state, wrong signing/account mode, or invalid venue-facing body/signature.

### P1

Can block trading indefinitely, hide liveness failure, pollute hot-path state, or make shadow/live validation misleading.

### P2

Docs/tests/CI/Graphify mismatch, stale symbols, missing evidence, non-hot-path cleanup.

### P3

Style/readability only.

---

## Refactor Policy

Allowed:

* delete dead state,
* collapse duplicate flow,
* move gates closer to the failure they protect,
* remove stale docs/comments/tests,
* replace reconciliation with one source of truth,
* simplify hot path,
* add focused tests,
* update Graphify/docs after source changes.

Forbidden:

* broad rewrites,
* new managers/orchestrators without clear rent,
* SELL locks/reservations/cooldowns,
* old-pending rotation reconciliation without hard evidence,
* global SDK mutation,
* generic robustness layers,
* defensive code for impossible internal states,
* keeping stale state for comfort.

---

## Production Gate

Do not call the bot production-ready unless all are true:

* live/shadow startup fails fast on required credentials,
* user WSS trust gates BUY,
* user WSS trust revokes on disconnect/error/parse failure,
* BUY claim is atomic with intent creation,
* rejected BUY deletes claim,
* WSS-confirmed BUY deletes claim and inventory owns truth,
* UNKNOWN remains matchable while useful,
* Accepted does not blindly expire into non-blocking state,
* SELL has no local pending/reservation/lock/cooldown,
* flat-start checks positions and open orders fail-closed,
* decimal validation is fixed-point,
* signature kind/funder config fails closed,
- market rotation cannot trade on stale strike,
- market rotation cleanly forgets previous dead market assets/orders/inventory,
- no old-market pending/inventory reconciliation exists unless official docs or live evidence prove it protects a real failure,
* no full SDK order builder on signal,
* no subprocess/raw-event/JSON-hot-path junk,
* docs/tests/Graphify match runtime,
* `cargo fmt`, `cargo test`, and `cargo clippy -D warnings` pass,
* stale-symbol grep is clean or every hit is justified.

Production readiness also requires shadow/live evidence for:

* user WSS auth success,
* Gamma discovery,
* market WSS subscription,
* Binance signal path,
* BUY claim,
* BUY submit outcome classification,
* user WSS inventory update,
* SELL fire path.

```
```
