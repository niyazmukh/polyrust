//! Startup config with fail-closed validators.
//!
//! Phase 1 of the plan only requires the *typed* config and validation
//! gates — feed-specific keys (Binance URL, signing keys) are added when
//! their consumers land in Phases 3–6. Resist the urge to define every
//! future field today; doing so creates dead state we'll have to remove.

use std::env;
use std::fmt;

/// Fully validated, frozen configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Required env var; if false, refuse to start a live runtime.
    pub allow_live_orders: bool,
    /// Optional dry-run mode: connect to feeds but do not submit.
    pub dry_run_orders: bool,
    /// Minimum acceptable USDC notional per BUY, in cents. Venue floor is 100¢.
    pub usdc_per_trade_cents: i64,
    /// Hard cap on absolute notional overrun above the target, in cents.
    pub max_notional_overrun_cents: i64,
    /// Optional cap on relative notional overrun, in basis points.
    pub max_notional_overrun_bps: i64,
    /// Minimum ask price (in cents) for entry consideration.
    pub min_buy_limit_cents: i32,
    /// Maximum ask price (in cents) for entry consideration.
    pub max_buy_limit_cents: i32,
    /// No-entry window before market expiry, in microseconds.
    pub min_decision_tte_us: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    Missing { name: &'static str },
    Invalid { name: &'static str, reason: String },
    LiveDisallowed,
    BandIncoherent { min: i32, max: i32 },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Missing { name } => write!(f, "missing_required_env name={name}"),
            ConfigError::Invalid { name, reason } => {
                write!(f, "invalid_env name={name} reason={reason}")
            }
            ConfigError::LiveDisallowed => write!(
                f,
                "refusing_live_runtime: set POLY_ALLOW_LIVE_ORDERS=true or MINIMAL_DRY_RUN_ORDERS=true"
            ),
            ConfigError::BandIncoherent { min, max } => {
                write!(f, "buy_band_incoherent min_cents={min} max_cents={max}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// Read the live-runtime configuration from process env vars.
    ///
    /// Failure mode: any required field missing or invalid → `ConfigError`.
    /// We never fall back silently; the live runtime must observe the same
    /// invariants the Python bot enforces.
    pub fn from_env() -> Result<Self, ConfigError> {
        let allow_live_orders = env_bool("POLY_ALLOW_LIVE_ORDERS").unwrap_or(false);
        let dry_run_orders = env_bool("MINIMAL_DRY_RUN_ORDERS").unwrap_or(false);
        if !allow_live_orders && !dry_run_orders {
            return Err(ConfigError::LiveDisallowed);
        }

        let usdc_per_trade_cents = env_dec_cents("MINIMAL_USDC_PER_TRADE")
            .ok_or(ConfigError::Missing {
                name: "MINIMAL_USDC_PER_TRADE",
            })?;
        if usdc_per_trade_cents < 100 {
            // Verified 2026-05-07 live probe: venue rejects sub-$1 marketable BUYs.
            return Err(ConfigError::Invalid {
                name: "MINIMAL_USDC_PER_TRADE",
                reason: format!("below_venue_floor cents={usdc_per_trade_cents} min=100"),
            });
        }

        let max_notional_overrun_cents = env_dec_cents("MINIMAL_MAX_NOTIONAL_OVERRUN").unwrap_or(1);
        let max_notional_overrun_bps = env_i64("MINIMAL_MAX_NOTIONAL_OVERRUN_BPS").unwrap_or(0);

        let min_buy_limit_cents = env_dec_cents("MINIMAL_MIN_BUY_LIMIT")
            .ok_or(ConfigError::Missing {
                name: "MINIMAL_MIN_BUY_LIMIT",
            })?
            .try_into()
            .map_err(|_| ConfigError::Invalid {
                name: "MINIMAL_MIN_BUY_LIMIT",
                reason: "out_of_range".into(),
            })?;

        let max_buy_limit_cents = env_dec_cents("MINIMAL_MAX_BUY_LIMIT")
            .unwrap_or(60)
            .try_into()
            .map_err(|_| ConfigError::Invalid {
                name: "MINIMAL_MAX_BUY_LIMIT",
                reason: "out_of_range".into(),
            })?;

        if max_buy_limit_cents <= min_buy_limit_cents {
            return Err(ConfigError::BandIncoherent {
                min: min_buy_limit_cents,
                max: max_buy_limit_cents,
            });
        }

        let min_decision_tte_us = env_i64("MINIMAL_DECISION_MIN_TTE_US").ok_or(
            ConfigError::Missing {
                name: "MINIMAL_DECISION_MIN_TTE_US",
            },
        )?;
        if min_decision_tte_us <= 0 {
            return Err(ConfigError::Invalid {
                name: "MINIMAL_DECISION_MIN_TTE_US",
                reason: format!("non_positive value={min_decision_tte_us}"),
            });
        }

        Ok(Self {
            allow_live_orders,
            dry_run_orders,
            usdc_per_trade_cents,
            max_notional_overrun_cents,
            max_notional_overrun_bps,
            min_buy_limit_cents,
            max_buy_limit_cents,
            min_decision_tte_us,
        })
    }
}

// ---- env helpers ----------------------------------------------------------

fn env_bool(name: &str) -> Option<bool> {
    let raw = env::var(name).ok()?.trim().to_ascii_lowercase();
    Some(matches!(
        raw.as_str(),
        "1" | "true" | "yes" | "on"
    ))
}

fn env_i64(name: &str) -> Option<i64> {
    env::var(name).ok()?.trim().parse::<i64>().ok()
}

/// Parse a decimal env var like "1.01" or "0.55" into integer cents (i64).
/// Returns None if missing or unparseable. Empty string treated as missing.
fn env_dec_cents(name: &str) -> Option<i64> {
    let raw = env::var(name).ok()?;
    parse_dec_cents(raw.trim())
}

/// Parse "1.01" -> 101 cents, "0.5" -> 50 cents, "10" -> 1000 cents.
/// Internal helper, exposed for tests.
pub(crate) fn parse_dec_cents(raw: &str) -> Option<i64> {
    if raw.is_empty() {
        return None;
    }
    let (sign, body) = match raw.as_bytes()[0] {
        b'-' => (-1i64, &raw[1..]),
        b'+' => (1i64, &raw[1..]),
        _ => (1i64, raw),
    };
    let (whole_str, frac_str) = match body.find('.') {
        Some(i) => (&body[..i], &body[i + 1..]),
        None => (body, ""),
    };
    let whole: i64 = if whole_str.is_empty() {
        0
    } else {
        whole_str.parse().ok()?
    };
    let frac: i64 = match frac_str.len() {
        0 => 0,
        1 => frac_str.parse::<i64>().ok()? * 10,
        2 => frac_str.parse::<i64>().ok()?,
        _ => {
            // 3+ digits: only accept if the trailing digits are zero, else
            // fail closed (no silent rounding of money values).
            if frac_str[2..].chars().all(|c| c == '0') {
                frac_str[..2].parse::<i64>().ok()?
            } else {
                return None;
            }
        }
    };
    Some(sign * (whole * 100 + frac))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dec_cents_basics() {
        assert_eq!(parse_dec_cents("1.01"), Some(101));
        assert_eq!(parse_dec_cents("0.55"), Some(55));
        assert_eq!(parse_dec_cents("10"), Some(1000));
        assert_eq!(parse_dec_cents("10.00"), Some(1000));
        assert_eq!(parse_dec_cents("0.5"), Some(50));
        assert_eq!(parse_dec_cents("0"), Some(0));
        assert_eq!(parse_dec_cents("-1.50"), Some(-150));
    }

    #[test]
    fn parse_dec_cents_rejects_silent_rounding() {
        // Three or more non-zero fractional digits would silently round.
        // We refuse; the operator must specify a 2-dp value explicitly.
        assert_eq!(parse_dec_cents("1.005"), None);
        assert_eq!(parse_dec_cents("1.001"), None);
        // Trailing zeros are fine.
        assert_eq!(parse_dec_cents("1.500"), Some(150));
    }

    #[test]
    fn parse_dec_cents_handles_garbage() {
        assert_eq!(parse_dec_cents(""), None);
        assert_eq!(parse_dec_cents("abc"), None);
        assert_eq!(parse_dec_cents("1.x"), None);
    }
}
