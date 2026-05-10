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
│   └── main.rs           ← placeholder binary; feed executable pending
└── tests/
    └── golden_canonical.rs ← BUY canonicalization golden table
```

`RuntimeCore` now accepts raw Polymarket market frames, raw user trade frames,
and Binance bookTicker frames. The live WebSocket executable is still pending;
unvalidated socket dependencies are not kept in the runtime tree.

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
  inventory, signal, BUY policy, and max-position cap. It also connects parser,
  state, signal, inventory checks, BUY submit lifecycle, and SELL submit
  lifecycle without sockets or a god orchestrator.
* `main.rs` is a placeholder binary. Feed IO lands only after the WebSocket
  crate can be fetched and validated.

## Build / test

```powershell
cd minirust
cargo test
cargo clippy -- -D warnings
```

Rust 2024 edition is required. This is a new low-latency implementation,
not a compatibility exercise for old compilers.

Shadow mode requires `.env.poly` plus explicit static market context:

```powershell
$env:MINIMAL_DRY_RUN_ORDERS="true"
$env:MINIRUST_MARKET_SLUG="btc-up-down-1m"
$env:MINIRUST_CONDITION_ID="0x..."
$env:MINIRUST_YES_TOKEN_ID="..."
$env:MINIRUST_NO_TOKEN_ID="..."
$env:MINIRUST_MARKET_START_TS="1777000000"
$env:MINIRUST_MARKET_END_TS="1777000060"
$env:MINIRUST_STRIKE_USD="100000"
$env:MINIRUST_BINANCE_SYMBOL="BTCUSDT"
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
* Shadow launch config is explicit and static: market slug, condition ID,
  YES/NO token IDs, expiry, strike, and Binance symbol. Rust market discovery
  is not implemented yet, so missing context fails startup instead of guessing.
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

* No user-channel socket runtime yet.
* No live-submit executable path yet.
* No analyzer (off-runtime by doctrine).
* No GTC/GTD support — FAK only by strategy invariant.

## Implementation Slices

| Slice | Status | Adds |
|---|---|---|
| Fixed-point venue math | ✅ | `types.rs`, `orders.rs` |
| Local signing/auth | ✅ | `auth.rs`, `signing.rs` |
| Direct submit classifier | ✅ partial | `submit.rs`; live submit wiring still pending |
| WSS parser/state authority | ✅ partial | `inventory.rs`, `user.rs`, `market.rs`, `state.rs` |
| BUY-intent model | ✅ partial | `signal.rs`, `binance.rs`; Binance socket IO still pending |
| Runtime hot path | ✅ partial | `runtime.rs`; HTTP submit call and user WSS still pending |
| Shadow mode on EC2 | ⬜ | WebSocket IO crate fetch/build blocked locally |
| Controlled live run | ⬜ | runtime-only deploy |

### Signing Inline, No SDK At Runtime

The official `polymarket_client_sdk_v2` crate exists but its order
builder calls `tick_size`, `fee_rate_bps`, and `resolve_version`
against the live CLOB *before* producing a signable body. That defeats
this bot's pre-signing architecture (Binance tick → cached signed body
→ submit, with the EIP-712 + secp256k1 cost paid off the hot path).

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
