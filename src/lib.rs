//! `minirust` — Rust port of the minimal Polymarket/Binance bot.
//!
//! Scope of the current build (Phase 1+2 of `docs/RUST_SOTA_ARCHITECTURE_REFACTOR_PLAN.md`):
//!
//! * `types`   — fixed-point integer newtypes; no `f64` in venue-facing math.
//! * `orders`  — canonical BUY/SELL parameter selection that matches the
//!               Python reference in `fast_order_submitter.py` byte-for-byte.
//! * `config`  — typed startup config with fail-closed validators.
//! * `logline` — structured key=value line logger for the hot path.
//!
//! Phases 3–9 (EIP-712 signing, REST submit, WSS feeds, runtime, shadow
//! mode, live deploy) intentionally do not exist yet. The plan starts here
//! because the highest harmful-event risk is invalid signed venue bodies,
//! and that risk is isolated from feed/runtime plumbing.

pub mod auth;
pub mod config;
pub mod logline;
pub mod orders;
pub mod types;

/// Crate-wide error type for Phase 1+2.
///
/// Submit/feed phases will add their own error variants when introduced; we
/// resist the urge to pre-design a unified error enum until concrete call
/// sites prove the abstraction would remove more complexity than it adds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Config(config::ConfigError),
    BuyCanonical(orders::BuyCanonicalError),
    Auth(auth::AuthError),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Config(e) => write!(f, "config: {e}"),
            Error::BuyCanonical(e) => write!(f, "buy_canonical: {e}"),
            Error::Auth(e) => write!(f, "auth: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<config::ConfigError> for Error {
    fn from(value: config::ConfigError) -> Self {
        Error::Config(value)
    }
}

impl From<orders::BuyCanonicalError> for Error {
    fn from(value: orders::BuyCanonicalError) -> Self {
        Error::BuyCanonical(value)
    }
}

impl From<auth::AuthError> for Error {
    fn from(value: auth::AuthError) -> Self {
        Error::Auth(value)
    }
}
