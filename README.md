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
| 3b — EIP-712 order signing | ✅ inline (k256 + sha3 + primitive-types) | `signing.rs` |
| 4 — direct HTTP submitter | ⬜ | classifier, pooled client, body validator |
| 5 — WSS parsers + inventory | ⬜ | `inventory.rs`, market/user feeds |
| 6 — Binance feed + signal | ⬜ | `binance.rs`, `signal.rs` |
| 7 — runtime hot path | ⬜ | `runtime.rs`, `main.rs` |
| 8 — shadow mode on EC2 | ⬜ | live feeds, no submits |
| 9 — controlled live run | ⬜ | runtime-only deploy |

### Phase 3b note: signing inline, no SDK at runtime

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
dep tree is 9 direct crates and ~99 transitive nodes.

`signing::tests::signature_recovers_to_signer_address_*` exercises the
full digest + sign + recover pipeline on canonical BUY and SELL bodies
with a deterministic test private key (`0x0...001`) — fully offline,
no `#[ignore]` gate.

Each phase commits independently. No phase ships without the validators
listed in the plan.
