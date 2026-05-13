# minirust

Minimal Rust Polymarket/Binance FAK trading runtime. Not a Python port.

Binance tick → signal decision → EIP-712 sign → POST /order → WSS inventory.
That's the entire hot path. Everything else is startup or periodic maintenance.

## Structure

```
src/
├── main.rs       ← orchestrator: 5 async tasks (market, binance, user, exit, maint)
├── runtime.rs    ← RuntimeCore: signal + inventory + state behind one Mutex
├── signal.rs     ← microprice momentum + OFI + imbalance → BuyIntent | None
├── inventory.rs  ← WSS-authoritative inventory, pending claim lifecycle
├── signing.rs    ← offline EIP-712 V2 order signing (no SDK, no network)
├── auth.rs       ← L2 HMAC headers + L1 credential derivation from PK
├── submit.rs     ← POST /order + response classifier (Accepted/Rejected/Unknown)
├── orders.rs     ← canonical BUY/SELL body parameter selection (lattice math)
├── types.rs      ← fixed-point newtypes (PriceTick, Shares4, UsdcCents, SharesAtoms)
├── config.rs     ← typed env config, fail-closed validation
├── binance.rs    ← SBE @bestBidAsk + JSON @bookTicker parser (auto-detect)
├── market.rs     ← market-channel quote/resolution parser
├── user.rs       ← user-channel trade + auth parser
├── state.rs      ← active MarketContext + latest quotes
├── anchor.rs     ← strike resolution from microprice samples
├── gamma.rs      ← Gamma REST market discovery (slug → condition_id → tokens)
├── feed.rs       ← three WS feed loops with exponential backoff
├── ws.rs         ← shared WS connect + Backoff
├── logline.rs    ← non-blocking structured key=value logger
└── lib.rs        ← crate root (module declarations only)
```

## Key Design Decisions

**WSS is inventory truth.** User-channel CONFIRMED trades own the balance.
HTTP submit responses classify outcomes but don't own inventory.

**Inventory applies on MATCHED.** MATCHED is the first on-chain signal; inventory
is applied immediately so SELL can fire without waiting for CONFIRMED. If FAILED
arrives after MATCHED the delta is reversed. CONFIRMED is idempotent (already applied).
SELL only fires once MATCHED balance exists.

**No flat-start check.** Old positions on expired markets resolve automatically.
The bot only trades the current 5-minute window discovered via Gamma.

**Unconditional market rotation.** When Gamma discovers a new market, the bot
rotates immediately. Old inventory is forgotten — it resolves on-chain at expiry.

**BUY duplicate protection via atomic claim.** `claim_entry()` runs inside the
same `core.lock()` as the signal decision. Pending stays alive until CONFIRMED
(blocks same-token re-entry). Rejected → claim deleted.

**SELL is fire-and-forget.** No SELL state, no locks, no cooldowns. Exit task
fires every 50ms at the executable bid. FAK rejection is cheap.

**Auth trust gated on venue response.** `user_wss_trusted` starts false, set
true only on `AuthSuccess` message from the venue, revoked on disconnect/error.
BUY blocked while untrusted.

**Signal ring cleared on rotation.** Prevents stale microprice samples from
producing spurious momentum against the new market's strike.

**CLOB host.** Default `https://clob.polymarket.com` (pUSD collateral,
EIP-712 domain version "2"). The `clob-v2` subdomain is a 301 redirect
alias — POST requests must go directly to `clob.polymarket.com`.

**EOA address for L2 auth.** When credentials are derived from PK, the
`POLY_ADDRESS` header uses the EOA (the address the API key is associated
with), not the proxy/funder.

## Build / Test

```powershell
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

## Shadow Mode

```powershell
$env:MINIMAL_DRY_RUN_ORDERS="true"
$env:MINIRUST_BINANCE_SYMBOL="BTCUSDT"
$env:MINIRUST_MARKET_SLUG_FMT="btc-updown-5m-{ts}"
$env:POLY_PK="0x..."  # API creds derived automatically
cargo run --release
```

Requires `POLY_PK` (private key). API credentials are derived at startup
via `/auth/derive-api-key`. The bot gates BUY signals on authenticated
user WSS even in dry-run — without WSS auth, no signals fire.

## Live Mode

```powershell
$env:POLY_ALLOW_LIVE_ORDERS="true"
$env:POLY_PK="0x..."
$env:POLY_FUNDER="0x..."           # proxy/safe wallet address
$env:POLY_SIGNATURE_KIND="POLY_PROXY"  # or EOA, POLYGON_GNO_SAFE
$env:MINIRUST_BINANCE_SYMBOL="BTCUSDT"
$env:MINIRUST_MARKET_SLUG_FMT="btc-updown-5m-{ts}"
cargo run --release
```

## What Is NOT Here

* No position cap — in a 2-token market, `has_entry_exposure_or_pending` is sufficient.
* No flat-start check — WSS authority handles restart-with-position.
* No rotation blocker — old markets resolve automatically at expiry.
* No force-exit task — exit task (50ms) already sells all sellable inventory.
* No SELL state/locks/cooldowns — FAK rejection is cheap.
* No full SDK order builder — signing is local, synchronous, on-demand.
* No analyzer — off-runtime by doctrine.
* No GTC/GTD — FAK only.
