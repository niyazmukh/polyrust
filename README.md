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

**WSS is inventory truth.** User-channel trade events own inventory. HTTP
submit responses classify outcomes but don't own inventory.

**BUY inventory applies on CONFIRMED.** MATCHED binds the pending submit and
keeps duplicate BUY blocked, but local sellable inventory waits for CONFIRMED
because live CLOB rejected early resale attempts before confirmation. SELL
inventory still applies on MATCHED so a matched SELL clears local sellable
balance immediately and prevents duplicate resale attempts.

**No flat-start check.** Old positions on expired markets resolve automatically.
The bot only trades the current 5-minute window discovered via Gamma.

**Scheduled market rotation.** Gamma discovery is the only market source, but
ordinary discovery is current-slug only. The bot rotates exactly 5 seconds
before market expiry by querying the next slug (`slug_ts = current.end_ts`).
Old inventory is forgotten; old markets resolve on-chain at expiry.

**BUY duplicate protection via atomic claim.** `claim_entry()` runs inside the
same `core.lock()` as the signal decision. Pending stays alive until CONFIRMED
(blocks same-token re-entry). Rejected → claim deleted. Transport-timeout
UNKNOWN stays WSS-matchable, but stale UNKNOWN stops blocking same-token BUY
after the live timeout window.

**BUY slippage split.** `MINIMAL_ENTRY_SLIPPAGE` is the FAK execution cap. Edge
math charges half that cap rounded up as the expected fill debit, because a
marketable FAK BUY starts at the best ask and only walks the book if needed.

**Exit is fair-value gated.** BUY CONFIRMED starts a per-token bid tracker from
the WSS fill price and executable entry bid. The exit task wakes every 50ms,
updates peak bid, and sells on hard hold timeout (`EXIT_HOLD_US`), hard local
stop (`EXIT_STOP_TICKS` below entry bid), or when the same Binance probability
model used for entry no longer values the held side above current bid plus
`EXIT_EDGE_TICKS`. A profitable pullback logs as `drop` only when the fair-value
gate also says holding no longer pays. SELL remains FAK at current bid.

**SELL submit is single-flight per token.** Inventory remains WSS-owned, and
HTTP SELL responses never own balance. Once exit decides to sell, a token cannot
submit another FAK SELL until the prior HTTP outcome returns. This prevents
repeated full-size SELLs from colliding with venue-side reservations under
transport uncertainty.

**User WSS scoped to the active market.** `user_wss_trusted` starts false,
set true after the auth frame with the active condition ID is successfully
sent. Rotation sends a user-channel subscription update for the next condition
ID. Revoked on disconnect/error. BUY blocked while untrusted.

**WSS subscription split.** Market WSS subscribes by token IDs
(`assets_ids: [yes_token, no_token]`). User WSS subscribes by condition IDs
(`markets: [condition_id]`). The Gamma-discovered `MarketContext` feeds both
channels on startup and rotation.

**Signal ring cleared on rotation.** Prevents stale microprice samples from
producing spurious momentum against the new market's strike.

**CLOB host.** Default `https://clob.polymarket.com` (pUSD collateral,
EIP-712 domain version "2"). The `clob-v2` subdomain is a 301 redirect
alias — POST requests must go directly to `clob.polymarket.com`.

**EOA address for L2 auth.** When credentials are derived from PK, the
`POLY_ADDRESS` header uses the EOA (the address the API key is associated
with), not the proxy/funder.

## Observability

Structured key=value logs to stderr via non-blocking background thread.
Level filter: `MINIRUST_LOG_LEVEL` (DEBUG/INFO/WARNING/ERROR, default WARNING).

**Latency fields** on signal/submit logs enable pipeline breakdown:
```
src_ts_us → recv_us → decide_us → submit_us → outcome (rtt_us)
 [network]  [signal]   [spawn+sign]  [HTTP RTT]
```

BUY submit outcomes also log `limit_ticks`, `edge_price_ticks`, and
`edge_ticks`, so FAK no-match events can be separated into stale/slow execution
versus intentional price-band rejections.

**Post-signal price tracker** (INFO level): logs token bid and ask prices at
1s intervals for 15s after each signal fires. Zero hot-path overhead —
runs in a spawned task off the critical path.

**User trade application** (WARNING level): logs `user_trade_applied` after a
parsed user-channel trade updates inventory, including trade id, token, side,
status, size atoms, matched submit id, and sellable balance after the update.

**Exit decision** (WARNING level): logs `exit_triggered` with reason (`stop`,
`value`, `drop`, or `hold`), entry ticks, entry bid ticks, peak bid ticks,
current bid ticks, fair ticks, fair-minus-bid ticks, and hold time.

## Build / Test

```powershell
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

## Shadow Mode

```powershell
$env:MINIRUST_BINANCE_SYMBOL="BTCUSDT"
$env:MINIRUST_MARKET_SLUG_FMT="btc-updown-5m-{ts}"
$env:POLY_PK="0x..."  # API creds derived automatically
cargo run --release
```

Requires `POLY_PK` (private key). API credentials are derived at startup
via `/auth/derive-api-key`. Without `POLY_ALLOW_LIVE_ORDERS=true`, the bot
runs in dry-run mode (signals fire, no orders submitted).

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

## Official Polymarket Docs Used

- POST order: https://docs.polymarket.com/api-reference/trade/post-a-new-order
- Create order: https://docs.polymarket.com/trading/orders/create
- User WSS API: https://docs.polymarket.com/api-reference/wss/user
- Market WSS API: https://docs.polymarket.com/api-reference/wss/market
- User channel guide: https://docs.polymarket.com/market-data/websocket/user-channel
- Market channel guide: https://docs.polymarket.com/market-data/websocket/market-channel

Runtime mapping from docs:

- Market channel subscription uses token/asset IDs.
- User channel subscription uses condition/market IDs.
- User trade lifecycle includes MATCHED, CONFIRMED, and FAILED updates.
- Insufficient balance/allowance on SELL is a venue rejection, not a reason to
  add local SELL state.

## What Is NOT Here

* No position cap — in a 2-token market, `has_entry_exposure_or_pending` is sufficient.
* No flat-start check — WSS authority handles restart-with-position.
* No early next-window promotion — rotation is scheduled at `end_ts - 5s`.
* No rotation blocker — old markets resolve automatically at expiry.
* No force-exit task - the 50ms exit task owns fair-value-gated SELL decisions.
* No SELL inventory state/locks/cooldowns — submit concurrency is single-flight
  per token to avoid duplicate full-size FAKs while an HTTP outcome is pending.
* No full SDK order builder — signing is local, synchronous, on-demand.
* No analyzer — off-runtime by doctrine.
* No GTC/GTD — FAK only.
* No max-TTE gate — the 5-min market window IS the product boundary.
