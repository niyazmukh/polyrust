//! `minirust` — minimal Rust Polymarket/Binance HFT runtime.
//!
//! Current runtime primitives:
//!
//! * `types`   — fixed-point integer newtypes; no `f64` in venue-facing math.
//! * `orders`  — canonical BUY/SELL parameter selection for venue-valid FAK
//!   bodies.
//! * `auth`    — L2 auth headers (HMAC-SHA256), golden-locked vs Python.
//! * `signing` — synchronous offline EIP-712 V2 order signing.
//! * `config`  — typed startup config with fail-closed validators.
//! * `logline` — structured key=value line logger for the hot path.
//! * `submit`  — direct `/order` POST with L2 headers and typed signed body.
//! * `user`    — user-channel trade parser feeding WSS-authoritative inventory.
//! * `market`  — market-channel quote/resolution parser.
//! * `state`   — active market context and latest quotes only.
//! * `signal`  — pure Binance move to BUY-intent model; non-buy is `None`.
//! * `binance` — narrow Binance book-ticker parser into signal samples.
//! * `runtime` — thin integration edges; no god orchestrator.

pub mod anchor;
pub mod auth;
pub mod binance;
pub mod config;
pub mod feed;
pub mod gamma;
pub mod inventory;
pub mod logline;
pub mod market;
pub mod orders;
pub mod runtime;
pub mod signal;
pub mod signing;
pub mod state;
pub mod submit;
pub mod types;
pub mod user;
pub mod ws;
