# minirust

Rust port of the minimal Polymarket/Binance bot. **Phase 1 + Phase 2** of
`docs/RUST_SOTA_ARCHITECTURE_REFACTOR_PLAN.md` only.

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
│   └── logline.rs        ← compact key=value log writer (parses like Python `log_event`)
└── tests/
    └── golden_canonical.rs ← BUY canonicalization golden table
```

Pure stdlib — no dependencies. EIP-712 signing, REST submit, WSS feeds,
runtime, and shadow mode are intentionally **not implemented yet** (Phases
3–9 of the plan).

## Why this is the first concrete deliverable

The plan's "First Concrete Implementation Task" is `types.rs` + `orders.rs`
+ golden body tests. Reason (verbatim from the plan):

> The highest real risk is invalid signed venue bodies. It is isolated from
> feed plumbing. It proves Rust can replace the Python/SDK signing boundary
> without monkey patches. It prevents building a fast bot that submits bad
> orders faster.

Phase 3 (EIP-712 signing) bolts directly onto the canonical (price, size,
maker_amount) triples produced by `canonical_buy_target_for_notional`.
Locking those triples against Python first removes the most expensive
class of bug.

## Build / test

```powershell
cd minirust
cargo test
cargo clippy -- -D warnings
```

A Rust toolchain (`stable-2024-04` or newer) is required. None was
installed on the development machine where this scaffold was authored;
the code is written to compile under stable Rust 1.78+ but has not been
exercised through `cargo test` yet. **Before merging:** run the test suite
and address any gaps.

## What changes vs the Python reference

Behavioural identity is the design goal at this layer:

* `types::buy_size_multiple_taker_units(price_ticks)` matches Python
  `_buy_size_multiple_for_amount_precision(price)` — verified by ratios
  (50→200, 51→10000, 67→10000, 40→250).
* `orders::canonical_buy_target_for_notional` returns the same
  `(price, size, maker_amount)` tuple Python returns, including the
  `Ceil`/`Floor` policy.
* Maker amounts are computed as integer cents
  (`price_ticks * size_taker_units / 10_000`) with explicit `None` when
  not aligned, instead of Python's implicit Decimal rounding.

## What is intentionally NOT here

Per `docs/RUST_SOTA_ARCHITECTURE_REFACTOR_PLAN.md` "Non-Goals" and Phase
gating:

* No Tokio, no WSS, no HTTP yet.
* No EIP-712 signing yet (Phase 3).
* No runtime orchestrator (Phase 7).
* No analyzer (off-runtime by doctrine).
* No GTC/GTD support — FAK only by strategy invariant.

## Phase progression

| Phase | Status | Adds |
|---|---|---|
| 1 — types/config/logline skeleton | ✅ | `types.rs`, `config.rs`, `logline.rs` |
| 2 — order body canonicalization | ✅ | `orders.rs` + golden tests |
| 3a — L2 auth headers | ✅ | `auth.rs` (HMAC-SHA256, golden vs Python) |
| 3b — EIP-712 order signing | ✅ via `polymarket_client_sdk_v2` | `signing.rs` |
| 4 — direct HTTP submitter | ⬜ | classifier, pooled client, body validator |
| 5 — WSS parsers + inventory | ⬜ | `inventory.rs`, market/user feeds |
| 6 — Binance feed + signal | ⬜ | `binance.rs`, `signal.rs` |
| 7 — runtime hot path | ⬜ | `runtime.rs`, `main.rs` |
| 8 — shadow mode on EC2 | ⬜ | live feeds, no submits |
| 9 — controlled live run | ⬜ | runtime-only deploy |

### Phase 3b note: SDK over re-implementation

We use the official `polymarket_client_sdk_v2` crate (crates.io) for
EIP-712 typed-data signing rather than reimplementing the schema. The
Order struct, domain separator, and signature recovery path live on the
on-chain `CTFExchange` contract; the SDK is the canonical Rust binding
to that schema. Reimplementing it would be a monkey job and would
silently drift if Polymarket revs the contract.

### Running live signing tests

The `signing::tests::signs_fak_buy_against_live_clob` test is `#[ignore]`
because the SDK's order builder calls `tick_size`, `fee_rate_bps`, and
`resolve_version` against the CLOB at sign time. To run:

```powershell
$env:POLYMARKET_PRIVATE_KEY = "0x..."
$env:POLY_API_KEY           = "uuid"
$env:POLY_API_SECRET        = "url-safe-b64"
$env:POLY_API_PASSPHRASE    = "..."
$env:POLY_TEST_TOKEN_ID     = "decimal-u256-token"
cargo test signing -- --ignored
```

Each phase commits independently. No phase ships without the validators
listed in the plan.
