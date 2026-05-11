# minirust

Minimal Rust Polymarket/Binance HFT runtime. This is not a Python port.
Current scope covers fixed-point venue math, canonical FAK order sizing, L2
auth, offline EIP-712 signing, direct REST submit classification,
user-WSS inventory state, market-WSS quote state, and the narrow parsers
needed to feed them. The signal model emits `Some(BuyIntent)` only; no BUY
means `None`, not a separate non-buy event.

## What's here

```
minirust/
├── Cargo.toml
├── README.md             ← this file
├── src/
│   ├── lib.rs            ← crate root and `Error` enum
│   ├── types.rs          ← fixed-point newtypes, no f64 in venue math
│   ├── orders.rs         ← canonical_buy_target_for_notional + canonical_sell_params
│   ├── config.rs         ← typed startup config with fail-closed validators
│   ├── auth.rs           ← L2 auth header signing
│   ├── signing.rs        ← offline EIP-712 FAK order signing
│   ├── submit.rs         ← direct /order submit + response classifier
│   ├── inventory.rs      ← WSS trade authority + UNKNOWN submit matching
│   ├── user.rs           ← user-channel TRADE parser
│   ├── market.rs         ← market-channel quote/resolution parser
│   ├── state.rs          ← active market context + latest quotes only
│   ├── signal.rs         ← pure Binance move + quote edge → optional BUY intent
│   ├── binance.rs        ← narrow Binance book-ticker parser into signal samples
│   ├── runtime.rs        ← thin parser/state/signal/inventory integration edges
│   ├── logline.rs        ← compact key=value log writer
│   └── main.rs           ← primary binary integrating WS feeds and trading loop
└── tests/
    └── golden_canonical.rs ← BUY canonicalization golden table
```

`RuntimeCore` accepts parsed Polymarket market frames, user trade frames,
and Binance bookTicker samples. It is driven by the fully integrated feed orchestrator
in `main.rs`.

## Why These Modules Exist

Each module is present because it protects a live invariant or removes hot-path
overhead:

* `config.rs` parses live env once and builds typed signal/order policies.
* `types.rs` and `orders.rs` prevent invalid venue bodies.
* `signing.rs` signs locally without SDK network calls.
* `submit.rs` submits directly and preserves UNKNOWN outcomes for WSS recovery.
* `inventory.rs` makes user-WSS trades the only inventory authority.
* `market.rs` and `state.rs` keep only active-market executable quotes.
* `signal.rs` emits only actionable BUY intent; non-buy is `None`; the front
  filter follows the old live Python model's paid-rent parts: 250ms-2s
  microprice momentum with OFI and imbalance confirmation.
* `binance.rs` parses only usable book-ticker fields into `BinanceSample`.
* `runtime.rs` owns `RuntimeCore`, the small in-process owner of state,
  inventory, signal, BUY policy, and max-position cap. It connects parser,
  state, signal, inventory checks, BUY submit lifecycle, and SELL submit
  lifecycle.
* `main.rs` is the primary executable orchestrating the 3 WebSocket feeds
  (Polymarket market, user, and Binance ticker), periodic maintenance,
  and executing submit tasks.

## Build / test

```powershell
cd minirust
cargo test
cargo clippy -- -D warnings
```

Rust 2024 edition is required. This is a new low-latency implementation,
not a compatibility exercise for old compilers.

Shadow mode requires `.env.poly` with Gamma dynamic discovery, Binance symbol,
and the standard signal/sizing env vars. Market context (slug, token IDs, strike)
is discovered dynamically from the Gamma API at startup and on rotation.

Shadow mode ALSO requires Polymarket user-channel credentials
(`POLY_API_KEY` / `POLY_API_SECRET`) — the bot gates BUY signals on
authenticated user WSS inventory truth, even in dry-run. Without them,
no BUY signals will fire.

```powershell
$env:MINIMAL_DRY_RUN_ORDERS="true"
$env:MINIRUST_MARKET_SLUG_FMT="btc-updown-5m-{ts}"
$env:MINIRUST_BINANCE_SYMBOL="BTCUSDT"
$env:POLY_API_KEY="..."
$env:POLY_API_SECRET="..."
cargo run --release
```

## Runtime Invariants

The design goal is not behavioural identity with the Python bot. The design
goal is the smaller live invariant set:

* Venue-facing values are fixed-point integers; no `f64` crosses the signed
  body boundary.
* BUY body sizing stays inside the configured notional band and venue minimum.
* Maker amounts are computed as integer cents with explicit rejection when
  not aligned.
* User-WSS trade parsing feeds `Inventory`; HTTP matched submit is not a
  second inventory ledger.
* Market-WSS parsing updates `RuntimeState` quotes only for the active YES/NO
  tokens; bare `price` changes do not become executable bid/ask quotes.
* Binance book-ticker parsing rejects missing/invalid timestamps, prices, sizes,
  and update IDs before samples enter the signal ring.
* Startup config owns all signal thresholds and order sizing policy. Tests use
  the same env-shaped parser instead of parallel hardcoded runtime defaults.
* Shadow launch config uses dynamic market discovery via Gamma to fetch market slug, condition ID,
  YES/NO token IDs, expiry, and strike on startup.
* Runtime BUY integration returns `Some(BuyIntent)` or `None`; inactive market,
  missing quotes, weak signal/OFI/imbalance, existing token exposure, and
  max-position cap do not become hot-path reason enums.
* BUY submit lifecycle registers pending exposure before HTTP, maps Accepted /
  Rejected / Unknown outcomes into inventory state, and keeps UNKNOWN WSS-bindable.
* SELL submit lifecycle can use WSS-owned inventory or a trusted HTTP matched
  size hint, floors to venue sellable quantum, signs a fresh FAK SELL at the
  executable bid, and never predicts local balance.
* Exit is not a profit gate. After a fill, runtime should fire fresh FAK SELLs
  at the current executable bid while sellable inventory exists. Expected edge
  belongs in the BUY decision before exposure is opened.
* Signal evaluation returns a `BuyIntent` or `None`; non-buys are absence of
  work, not hot-path log events.
* Python golden cases remain only as regression oracles for already-live
  venue-body precision until direct Rust live probes replace them.

## What is intentionally NOT here

Per `docs/RUST_SOTA_ARCHITECTURE_REFACTOR_PLAN.md` non-goals:

* No analyzer (off-runtime by doctrine).
* No GTC/GTD support — FAK only by strategy invariant.

## Implementation Slices

| Slice | Status | Adds |
|---|---|---|
| Fixed-point venue math | ✅ | `types.rs`, `orders.rs` |
| Local signing/auth | ✅ | `auth.rs`, `signing.rs` |
| Direct submit classifier | ✅ | `submit.rs`; full live submit wiring in `main.rs` |
| WSS parser/state authority | ✅ | `inventory.rs`, `user.rs`, `market.rs`, `state.rs` |
| BUY-intent model | ✅ | `signal.rs`, `binance.rs`; fully integrated on hot path |
| Runtime hot path | ✅ | `runtime.rs`, `main.rs` |
| Shadow mode on EC2 | ⬜ | WebSocket IO crate fetch/build blocked locally |
| Controlled live run | ⬜ | runtime-only deploy |

### Signing Inline, No SDK At Runtime

The official `polymarket_client_sdk_v2` crate exists but its order
builder calls `tick_size`, `fee_rate_bps`, and `resolve_version`
against the live CLOB *before* producing a signable body. That defeats
this bot's low-latency architecture (Binance tick → sign FAK order on-demand
→ submit, with the ~100μs EIP-712 + secp256k1 cost paid concurrently).

We instead read the V2 schema from the SDK source as a reference (the
schema is fixed by the on-chain `CTFExchange` V2 contract; copying it
from a published Rust crate is reading a datasheet, not deconstruction
of a wrapper) and compute the typehash, domain separator, struct hash,
and ECDSA signature locally and synchronously using only:

* `k256` — secp256k1 ECDSA, prehash signing, recovery.
* `sha3` — Keccak256.
* `primitive-types` — U256 / H160 / H256.

The signing path takes no `await` and makes no network call. Runtime
dep tree is intentionally kept small for the current slice.

`signing::tests::signature_recovers_to_signer_address_*` exercises the
full digest + sign + recover pipeline on canonical BUY and SELL bodies
with a deterministic test private key (`0x0...001`) — fully offline,
no `#[ignore]` gate.

No slice ships without `cargo test`, `cargo clippy -- -D warnings`, stale-term
grep, and a runtime-scoped Graphify update.
