//! Startup config with fail-closed validators.
//!
//! Keep only keys with a current consumer. Future feed/signing keys are added
//! when their runtime owner exists; predeclared env state is dead state.

use std::{env, fs};
use std::fmt;

use crate::runtime::BuySubmitPolicy;
use crate::signal::SignalConfig;
use crate::types::PriceTick;

/// Fully validated, frozen configuration.
#[derive(Clone, Debug, PartialEq)]
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
    pub max_decision_tte_us: i64,
    pub max_concurrent_positions: usize,
    pub signal_max_lag_us: i64,
    pub signal_min_window_us: i64,
    pub signal_max_window_us: i64,
    pub signal_max_spread_usd: f64,
    pub signal_min_abs_move_usd: f64,
    pub signal_min_abs_ofi: f64,
    pub signal_min_imbalance: f64,
    pub signal_min_total_qty: f64,
    pub decision_min_edge_cents: i32,
    pub entry_slippage_cents: i32,
    pub prob_sigma_scale: f64,
    pub prob_sigma_floor_usd: f64,
    pub prob_floor: f64,
    pub prob_ceil: f64,
    pub signal_ring_size: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LaunchConfig {
    pub binance_ws_url: String,
    pub poly_market_ws_url: String,
    pub poly_user_ws_url: String,
    // Delivered by DeepSeek — dynamic market discovery fields.
    /// Slug template for Gamma REST API.
    /// Traces to: market_ws.py:124 (slug = cfg.market_slug_fmt.format(ts=ts)).
    pub market_slug_fmt: String,
    /// Market window in seconds (300 for 5 m markets).
    /// Traces to: market_ws.py:118 (window_s = cfg.market_window_s).
    pub market_window_s: i64,
    /// CLOB REST API base URL for `/time` endpoint.
    /// Traces to: http_client.py:45 (get_clob_time).
    pub clob_url: String,
    /// Gamma REST API base URL for market discovery.
    /// Traces to: http_client.py:55 (gamma_get_event_by_slug).
    pub gamma_url: String,
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
        Self::from_lookup(|name| env::var(name).ok())
    }

    fn from_lookup(mut lookup: impl FnMut(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let allow_live_orders = env_bool_lookup(&mut lookup, "POLY_ALLOW_LIVE_ORDERS").unwrap_or(false);
        let dry_run_orders = env_bool_lookup(&mut lookup, "MINIMAL_DRY_RUN_ORDERS").unwrap_or(false);
        if !allow_live_orders && !dry_run_orders {
            return Err(ConfigError::LiveDisallowed);
        }

        let usdc_per_trade_cents = env_dec_cents_lookup(&mut lookup, "MINIMAL_USDC_PER_TRADE")
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

        let max_notional_overrun_cents =
            env_dec_cents_lookup(&mut lookup, "MINIMAL_MAX_NOTIONAL_OVERRUN").unwrap_or(1);
        let max_notional_overrun_bps =
            env_i64_lookup(&mut lookup, "MINIMAL_MAX_NOTIONAL_OVERRUN_BPS").unwrap_or(0);

        let min_buy_limit_cents = env_dec_cents_lookup(&mut lookup, "MINIMAL_MIN_BUY_LIMIT")
            .ok_or(ConfigError::Missing {
                name: "MINIMAL_MIN_BUY_LIMIT",
            })?
            .try_into()
            .map_err(|_| ConfigError::Invalid {
                name: "MINIMAL_MIN_BUY_LIMIT",
                reason: "out_of_range".into(),
            })?;

        let max_buy_limit_cents = env_dec_cents_lookup(&mut lookup, "MINIMAL_MAX_BUY_LIMIT")
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

        let min_decision_tte_us = env_i64_lookup(&mut lookup, "MINIMAL_DECISION_MIN_TTE_US").ok_or(
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
        let max_decision_tte_us =
            env_i64_lookup(&mut lookup, "MINIMAL_DECISION_MAX_TTE_US").unwrap_or(600_000_000);
        if max_decision_tte_us <= min_decision_tte_us {
            return Err(ConfigError::Invalid {
                name: "MINIMAL_DECISION_MAX_TTE_US",
                reason: format!("lte_min value={max_decision_tte_us} min={min_decision_tte_us}"),
            });
        }

        let decision_min_edge_cents =
            env_dec_cents_lookup(&mut lookup, "MINIMAL_DECISION_MIN_EDGE")
                .unwrap_or(5)
                .try_into()
                .map_err(|_| ConfigError::Invalid {
                    name: "MINIMAL_DECISION_MIN_EDGE",
                    reason: "out_of_range".into(),
                })?;
        let entry_slippage_cents =
            env_dec_cents_lookup(&mut lookup, "MINIMAL_ENTRY_SLIPPAGE")
                .unwrap_or(0)
                .try_into()
                .map_err(|_| ConfigError::Invalid {
                    name: "MINIMAL_ENTRY_SLIPPAGE",
                    reason: "out_of_range".into(),
                })?;

        Ok(Self {
            allow_live_orders,
            dry_run_orders,
            usdc_per_trade_cents,
            max_notional_overrun_cents,
            max_notional_overrun_bps,
            min_buy_limit_cents,
            max_buy_limit_cents,
            min_decision_tte_us,
            max_decision_tte_us,
            max_concurrent_positions: env_i64_lookup(&mut lookup, "MINIMAL_MAX_CONCURRENT_POSITIONS")
                .unwrap_or(3)
                .max(0) as usize,
            signal_max_lag_us: env_i64_lookup(&mut lookup, "MINIMAL_SIGNAL_MAX_LAG_US")
                .unwrap_or(250_000),
            signal_min_window_us: env_i64_lookup(&mut lookup, "MINIMAL_SIGNAL_MIN_WINDOW_US")
                .unwrap_or(250_000),
            signal_max_window_us: env_i64_lookup(&mut lookup, "MINIMAL_SIGNAL_MAX_WINDOW_US")
                .unwrap_or(2_000_000),
            signal_max_spread_usd: env_f64_lookup(&mut lookup, "MINIMAL_SIGNAL_MAX_SPREAD")
                .unwrap_or(2.0),
            signal_min_abs_move_usd: env_f64_lookup(&mut lookup, "MINIMAL_SIGNAL_MIN_ABS_MOVE")
                .unwrap_or(0.50),
            signal_min_abs_ofi: env_f64_lookup(&mut lookup, "MINIMAL_SIGNAL_MIN_ABS_OFI")
                .unwrap_or(1.0),
            signal_min_imbalance: env_f64_lookup(&mut lookup, "MINIMAL_SIGNAL_MIN_IMBALANCE")
                .unwrap_or(0.12),
            signal_min_total_qty: env_f64_lookup(&mut lookup, "MINIMAL_SIGNAL_MIN_TOTAL_QTY")
                .unwrap_or(0.000001),
            decision_min_edge_cents,
            entry_slippage_cents,
            prob_sigma_scale: env_f64_lookup(&mut lookup, "MINIMAL_PROB_SIGMA_SCALE")
                .unwrap_or(1.5),
            prob_sigma_floor_usd: env_f64_lookup(&mut lookup, "MINIMAL_PROB_SIGMA_FLOOR_USD")
                .unwrap_or(2.0),
            prob_floor: env_f64_lookup(&mut lookup, "MINIMAL_PROB_FLOOR").unwrap_or(0.02),
            prob_ceil: env_f64_lookup(&mut lookup, "MINIMAL_PROB_CEIL").unwrap_or(0.98),
            signal_ring_size: env_i64_lookup(&mut lookup, "MINIMAL_SIGNAL_RING_SIZE")
                .unwrap_or(128)
                .max(8) as usize,
        })
    }

    pub fn signal_config(&self) -> Result<SignalConfig, ConfigError> {
        Ok(SignalConfig {
            max_lag_us: self.signal_max_lag_us,
            min_window_us: self.signal_min_window_us,
            max_window_us: self.signal_max_window_us,
            max_spread_usd: self.signal_max_spread_usd,
            min_move_usd: self.signal_min_abs_move_usd,
            min_abs_ofi: self.signal_min_abs_ofi,
            min_imbalance: self.signal_min_imbalance,
            min_total_qty: self.signal_min_total_qty,
            min_edge_ticks: self.decision_min_edge_cents,
            entry_slippage_ticks: self.entry_slippage_cents,
            max_quote_age_us: 250_000,
            min_tte_us: self.min_decision_tte_us,
            max_tte_us: self.max_decision_tte_us,
            min_buy_limit: PriceTick::checked(self.min_buy_limit_cents).map_err(|e| {
                ConfigError::Invalid {
                    name: "MINIMAL_MIN_BUY_LIMIT",
                    reason: e.to_string(),
                }
            })?,
            max_buy_limit: PriceTick::checked(self.max_buy_limit_cents).map_err(|e| {
                ConfigError::Invalid {
                    name: "MINIMAL_MAX_BUY_LIMIT",
                    reason: e.to_string(),
                }
            })?,
            prob_sigma_floor_usd: self.prob_sigma_floor_usd,
            prob_sigma_scale: self.prob_sigma_scale,
            prob_floor: self.prob_floor,
            prob_ceil: self.prob_ceil,
            max_samples: self.signal_ring_size,
        })
    }

    pub fn buy_submit_policy(&self) -> BuySubmitPolicy {
        BuySubmitPolicy {
            target_maker_cents: self.usdc_per_trade_cents,
            min_size_taker_units: 100,
            min_maker_cents: 100,
            max_overrun_cents: self.max_notional_overrun_cents,
            max_overrun_bps: self.max_notional_overrun_bps,
        }
    }
}

impl LaunchConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| env::var(name).ok())
    }

    fn from_lookup(mut lookup: impl FnMut(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let symbol = required_string(&mut lookup, "MINIRUST_BINANCE_SYMBOL")?.to_ascii_lowercase();
        let binance_ws_url = lookup("MINIRUST_BINANCE_WS_URL")
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                format!(
                    "wss://stream.binance.com:9443/ws/{symbol}@bookTicker?timeUnit=MICROSECOND"
                )
            });
        let poly_market_ws_url = lookup("MINIRUST_POLY_MARKET_WS_URL")
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "wss://ws-subscriptions-clob.polymarket.com/ws/market".to_owned());
        let poly_user_ws_url = lookup("MINIRUST_POLY_USER_WS_URL")
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "wss://ws-subscriptions-clob.polymarket.com/ws/user".to_owned());

        // Delivered by DeepSeek — dynamic discovery fields.
        // Traces to: market_ws.py (cfg.market_slug_fmt, cfg.market_window_s),
        //   http_client.py (CLOB / Gamma base URLs).
        let market_slug_fmt = lookup("MINIRUST_MARKET_SLUG_FMT")
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "btc-updown-5m-{ts}".to_owned());
        let market_window_s = env_i64_lookup(&mut lookup, "MINIRUST_MARKET_WINDOW_S").unwrap_or(300);
        if market_window_s <= 0 {
            return Err(ConfigError::Invalid {
                name: "MINIRUST_MARKET_WINDOW_S",
                reason: format!("non_positive value={market_window_s}"),
            });
        }
        let clob_url = lookup("MINIRUST_CLOB_URL")
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "https://clob.polymarket.com".to_owned());
        let gamma_url = lookup("MINIRUST_GAMMA_URL")
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "https://gamma-api.polymarket.com".to_owned());

        Ok(Self {
            binance_ws_url,
            poly_market_ws_url,
            poly_user_ws_url,
            market_slug_fmt,
            market_window_s,
            clob_url,
            gamma_url,
        })
    }
}

pub fn load_env_file(path: &str) -> Result<usize, ConfigError> {
    let text = fs::read_to_string(path).map_err(|e| ConfigError::Invalid {
        name: "ENV_FILE",
        reason: format!("read_failed path={path} error={e}"),
    })?;
    let mut loaded = 0usize;
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || env::var_os(key).is_some() {
            continue;
        }
        let value = value.trim().trim_matches('"').trim_matches('\'');
        // Startup-only compatibility with .env.poly. Hot path never reads env.
        unsafe {
            env::set_var(key, value);
        }
        loaded += 1;
    }
    Ok(loaded)
}

// ---- env helpers ----------------------------------------------------------

fn env_bool_lookup(lookup: &mut impl FnMut(&str) -> Option<String>, name: &str) -> Option<bool> {
    let raw = lookup(name)?.trim().to_ascii_lowercase();
    Some(matches!(
        raw.as_str(),
        "1" | "true" | "yes" | "on"
    ))
}

fn env_i64_lookup(lookup: &mut impl FnMut(&str) -> Option<String>, name: &str) -> Option<i64> {
    lookup(name)?.trim().parse::<i64>().ok()
}

fn env_f64_lookup(lookup: &mut impl FnMut(&str) -> Option<String>, name: &str) -> Option<f64> {
    let n = lookup(name)?.trim().parse::<f64>().ok()?;
    if n.is_finite() {
        Some(n)
    } else {
        None
    }
}

fn required_string(
    lookup: &mut impl FnMut(&str) -> Option<String>,
    name: &'static str,
) -> Result<String, ConfigError> {
    lookup(name)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .ok_or(ConfigError::Missing { name })
}

/// Parse a decimal env var like "1.01" or "0.55" into integer cents (i64).
/// Returns None if missing or unparseable. Empty string treated as missing.
fn env_dec_cents_lookup(
    lookup: &mut impl FnMut(&str) -> Option<String>,
    name: &str,
) -> Option<i64> {
    let raw = lookup(name)?;
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
    use std::collections::HashMap;

    fn cfg_from_pairs(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let map = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect::<HashMap<_, _>>();
        Config::from_lookup(|name| map.get(name).cloned())
    }

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

    #[test]
    fn config_builds_signal_and_buy_submit_policy_from_runtime_env_shape() {
        let cfg = cfg_from_pairs(&[
            ("POLY_ALLOW_LIVE_ORDERS", "true"),
            ("MINIMAL_USDC_PER_TRADE", "1.01"),
            ("MINIMAL_MAX_NOTIONAL_OVERRUN", "0.01"),
            ("MINIMAL_MAX_NOTIONAL_OVERRUN_BPS", "0"),
            ("MINIMAL_MIN_BUY_LIMIT", "0.35"),
            ("MINIMAL_MAX_BUY_LIMIT", "0.65"),
            ("MINIMAL_DECISION_MIN_TTE_US", "45000000"),
            ("MINIMAL_MAX_CONCURRENT_POSITIONS", "3"),
            ("MINIMAL_SIGNAL_MAX_LAG_US", "250000"),
            ("MINIMAL_SIGNAL_MIN_WINDOW_US", "250000"),
            ("MINIMAL_SIGNAL_MAX_WINDOW_US", "2000000"),
            ("MINIMAL_SIGNAL_MAX_SPREAD", "2.0"),
            ("MINIMAL_SIGNAL_MIN_ABS_MOVE", "0.50"),
            ("MINIMAL_SIGNAL_MIN_ABS_OFI", "1.0"),
            ("MINIMAL_SIGNAL_MIN_IMBALANCE", "0.12"),
            ("MINIMAL_SIGNAL_MIN_TOTAL_QTY", "0.000001"),
            ("MINIMAL_DECISION_MIN_EDGE", "0.05"),
            ("MINIMAL_ENTRY_SLIPPAGE", "0.03"),
            ("MINIMAL_PROB_SIGMA_SCALE", "1.5"),
            ("MINIMAL_PROB_SIGMA_FLOOR_USD", "2.0"),
            ("MINIMAL_PROB_FLOOR", "0.02"),
            ("MINIMAL_PROB_CEIL", "0.98"),
        ])
        .unwrap();

        let signal = cfg.signal_config().unwrap();
        assert_eq!(signal.max_lag_us, 250_000);
        assert_eq!(signal.min_window_us, 250_000);
        assert_eq!(signal.max_window_us, 2_000_000);
        assert_eq!(signal.min_edge_ticks, 5);
        assert_eq!(signal.entry_slippage_ticks, 3);
        assert_eq!(signal.min_buy_limit.ticks(), 35);
        assert_eq!(signal.max_buy_limit.ticks(), 65);
        assert_eq!(signal.min_tte_us, 45_000_000);

        let buy = cfg.buy_submit_policy();
        assert_eq!(buy.target_maker_cents, 101);
        assert_eq!(buy.min_maker_cents, 100);
        assert_eq!(buy.max_overrun_cents, 1);
    }

    #[test]
    fn launch_config_requires_static_market_and_builds_official_stream_urls() {
        let launch = LaunchConfig::from_lookup(|name| {
            HashMap::from([
                ("MINIRUST_MARKET_SLUG", "btc-up-down-1m"),
                ("MINIRUST_CONDITION_ID", "0xcond"),
                ("MINIRUST_YES_TOKEN_ID", "yes"),
                ("MINIRUST_NO_TOKEN_ID", "no"),
                ("MINIRUST_MARKET_START_TS", "1777000000"),
                ("MINIRUST_MARKET_END_TS", "1777000060"),
                ("MINIRUST_STRIKE_USD", "100000"),
                ("MINIRUST_BINANCE_SYMBOL", "BTCUSDT"),
            ])
            .get(name)
            .map(|s| (*s).to_owned())
        })
        .unwrap();

        assert_eq!(
            launch.binance_ws_url,
            "wss://stream.binance.com:9443/ws/btcusdt@bookTicker?timeUnit=MICROSECOND"
        );
        assert_eq!(
            launch.poly_market_ws_url,
            "wss://ws-subscriptions-clob.polymarket.com/ws/market"
        );
    }

    #[test]
    fn load_env_file_sets_missing_keys_only() {
        let path = std::env::temp_dir().join(format!(
            "minirust-env-{}.poly",
            std::process::id()
        ));
        std::fs::write(
            &path,
            "MINIRUST_ENV_TEST_A=one\nMINIRUST_ENV_TEST_B=\"two\"\n# ignored\n",
        )
        .unwrap();
        unsafe {
            std::env::remove_var("MINIRUST_ENV_TEST_A");
            std::env::set_var("MINIRUST_ENV_TEST_B", "preexisting");
        }

        let loaded = load_env_file(path.to_str().unwrap()).unwrap();

        assert_eq!(loaded, 1);
        assert_eq!(std::env::var("MINIRUST_ENV_TEST_A").unwrap(), "one");
        assert_eq!(std::env::var("MINIRUST_ENV_TEST_B").unwrap(), "preexisting");
        let _ = std::fs::remove_file(path);
        unsafe {
            std::env::remove_var("MINIRUST_ENV_TEST_A");
            std::env::remove_var("MINIRUST_ENV_TEST_B");
        }
    }
}
