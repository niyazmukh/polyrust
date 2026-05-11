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

/// Crate-wide error type.
///
/// Submit/feed phases will add their own error variants when introduced; we
/// resist the urge to pre-design a unified error enum until concrete call
/// sites prove the abstraction would remove more complexity than it adds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Config(config::ConfigError),
    BuyCanonical(orders::BuyCanonicalError),
    Auth(auth::AuthError),
    Signing(signing::SigningError),
    Submit(submit::SubmitError),
    Runtime(runtime::RuntimeError),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Config(e) => write!(f, "config: {e}"),
            Error::BuyCanonical(e) => write!(f, "buy_canonical: {e}"),
            Error::Auth(e) => write!(f, "auth: {e}"),
            Error::Signing(e) => write!(f, "signing: {e}"),
            Error::Submit(e) => write!(f, "submit: {e}"),
            Error::Runtime(e) => write!(f, "runtime: {e}"),
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

impl From<signing::SigningError> for Error {
    fn from(value: signing::SigningError) -> Self {
        Error::Signing(value)
    }
}

impl From<submit::SubmitError> for Error {
    fn from(value: submit::SubmitError) -> Self {
        Error::Submit(value)
    }
}

impl From<runtime::RuntimeError> for Error {
    fn from(value: runtime::RuntimeError) -> Self {
        Error::Runtime(value)
    }
}
