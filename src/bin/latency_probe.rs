//! Standalone regional latency probe.
//!
//! No runtime modules, no order signing, no order submission. The output is
//! CSV so identical binaries can run in different AWS regions and be compared
//! directly.
//!
//! Example:
//! `cargo run --release --bin latency_probe -- --samples 60 --region-label eu-west-1 --yes-token <YES> --no-token <NO>`

use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use minirust::auth::L2AuthSigner;
use minirust::orders::{BuyCanonicalInput, canonical_buy_target_for_notional};
use minirust::signing::{
    EXCHANGE_V2_NORMAL, OrderSigner, POLYGON_CHAIN_ID, SignInputs, SignatureKind,
};
use minirust::submit::{ORDER_PATH, classify};
use minirust::types::{PriceTick, TokenId};
use primitive_types::H160;
use reqwest::header::{HeaderMap, HeaderValue};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

const DEFAULT_SYMBOL: &str = "BTCUSDT";
const DEFAULT_ORDER_PROBE_CENTS: i64 = 10_000;
const DEFAULT_ORDER_PROBE_LIMIT_TICKS: i32 = 85;
const HARDCODED_BINANCE_SBE_API_KEY: &str =
    "XidZIvWQf4ejGGKMnrBuvIJpWQwDMh8aXJsNaz3qdIVAw0kXGv4crzB6gBPJZNZV";
const HARDCODED_POLY_PK: &str =
    "0x07034cb9caa94a82e6a245eef132404f20b84cf8a62ac4b1d1eca0a8259068a2";
const HARDCODED_POLY_ADDRESS: &str = "0x17725ad16443fDe9499f7105934E9bd4816B86d1";
const HARDCODED_POLY_FUNDER: &str = "0x17725ad16443fDe9499f7105934E9bd4816B86d1";
const HARDCODED_POLY_SIGNATURE_KIND: &str = "POLY_PROXY";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Args {
    samples: usize,
    interval_ms: u64,
    symbol: String,
    binance_ws_url: String,
    clob_url: String,
    gamma_url: String,
    poly_market_ws_url: String,
    poly_user_ws_url: String,
    yes_token: Option<String>,
    no_token: Option<String>,
    condition_id: Option<String>,
    allow_order_probe: bool,
    order_probe_token: Option<String>,
    order_probe_limit_ticks: Option<i32>,
    order_probe_cents: i64,
    region_label: String,
    connect_timeout_ms: u64,
    request_timeout_ms: u64,
}

impl Default for Args {
    fn default() -> Self {
        let symbol =
            std::env::var("MINIRUST_BINANCE_SYMBOL").unwrap_or_else(|_| DEFAULT_SYMBOL.to_owned());
        let lower = symbol.to_ascii_lowercase();
        Self {
            samples: env_usize("PROBE_SAMPLES").unwrap_or(30),
            interval_ms: env_u64("PROBE_INTERVAL_MS").unwrap_or(1_000),
            symbol,
            binance_ws_url: std::env::var("PROBE_BINANCE_WS_URL").unwrap_or_else(|_| {
                format!("wss://stream-sbe.binance.com:9443/ws/{lower}@bestBidAsk")
            }),
            clob_url: std::env::var("PROBE_CLOB_URL")
                .unwrap_or_else(|_| "https://clob.polymarket.com".to_owned()),
            gamma_url: std::env::var("PROBE_GAMMA_URL")
                .unwrap_or_else(|_| "https://gamma-api.polymarket.com".to_owned()),
            poly_market_ws_url: std::env::var("PROBE_POLY_MARKET_WS_URL").unwrap_or_else(|_| {
                "wss://ws-subscriptions-clob.polymarket.com/ws/market".to_owned()
            }),
            poly_user_ws_url: std::env::var("PROBE_POLY_USER_WS_URL").unwrap_or_else(|_| {
                "wss://ws-subscriptions-clob.polymarket.com/ws/user".to_owned()
            }),
            yes_token: std::env::var("PROBE_YES_TOKEN")
                .ok()
                .filter(|s| !s.is_empty()),
            no_token: std::env::var("PROBE_NO_TOKEN")
                .ok()
                .filter(|s| !s.is_empty()),
            condition_id: std::env::var("PROBE_CONDITION_ID")
                .ok()
                .filter(|s| !s.is_empty()),
            allow_order_probe: env_bool("PROBE_ALLOW_ORDER").unwrap_or(false),
            order_probe_token: std::env::var("PROBE_ORDER_TOKEN")
                .ok()
                .filter(|s| !s.is_empty()),
            order_probe_limit_ticks: env_i32("PROBE_ORDER_LIMIT_TICKS")
                .or(Some(DEFAULT_ORDER_PROBE_LIMIT_TICKS)),
            order_probe_cents: env_i64("PROBE_ORDER_CENTS").unwrap_or(DEFAULT_ORDER_PROBE_CENTS),
            region_label: std::env::var("PROBE_REGION_LABEL")
                .or_else(|_| std::env::var("AWS_REGION"))
                .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
                .unwrap_or_else(|_| "-".to_owned()),
            connect_timeout_ms: env_u64("PROBE_CONNECT_TIMEOUT_MS").unwrap_or(500),
            request_timeout_ms: env_u64("PROBE_REQUEST_TIMEOUT_MS").unwrap_or(10_000),
        }
    }
}

impl Args {
    fn parse(mut it: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut args = Args::default();
        let _program = it.next();
        while let Some(flag) = it.next() {
            match flag.as_str() {
                "--samples" => args.samples = parse_next(&mut it, &flag)?,
                "--interval-ms" => args.interval_ms = parse_next(&mut it, &flag)?,
                "--symbol" => {
                    args.symbol = next_value(&mut it, &flag)?;
                    args.binance_ws_url = format!(
                        "wss://stream-sbe.binance.com:9443/ws/{}@bestBidAsk",
                        args.symbol.to_ascii_lowercase()
                    );
                }
                "--binance-ws-url" => args.binance_ws_url = next_value(&mut it, &flag)?,
                "--clob-url" => args.clob_url = trim_trailing_slash(next_value(&mut it, &flag)?),
                "--gamma-url" => args.gamma_url = trim_trailing_slash(next_value(&mut it, &flag)?),
                "--poly-market-ws-url" => args.poly_market_ws_url = next_value(&mut it, &flag)?,
                "--poly-user-ws-url" => args.poly_user_ws_url = next_value(&mut it, &flag)?,
                "--yes-token" => args.yes_token = Some(next_value(&mut it, &flag)?),
                "--no-token" => args.no_token = Some(next_value(&mut it, &flag)?),
                "--condition-id" => args.condition_id = Some(next_value(&mut it, &flag)?),
                "--allow-order-probe" => args.allow_order_probe = true,
                "--order-probe-token" => args.order_probe_token = Some(next_value(&mut it, &flag)?),
                "--order-probe-limit-ticks" => {
                    args.order_probe_limit_ticks = Some(parse_next(&mut it, &flag)?);
                }
                "--order-probe-cents" => args.order_probe_cents = parse_next(&mut it, &flag)?,
                "--region-label" => args.region_label = next_value(&mut it, &flag)?,
                "--connect-timeout-ms" => args.connect_timeout_ms = parse_next(&mut it, &flag)?,
                "--request-timeout-ms" => args.request_timeout_ms = parse_next(&mut it, &flag)?,
                "--help" | "-h" => return Err(usage()),
                other => return Err(format!("unknown argument: {other}\n{}", usage())),
            }
        }
        Ok(args)
    }
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.trim().parse().ok()
}

fn env_usize(name: &str) -> Option<usize> {
    std::env::var(name).ok()?.trim().parse().ok()
}

fn env_i32(name: &str) -> Option<i32> {
    std::env::var(name).ok()?.trim().parse().ok()
}

fn env_i64(name: &str) -> Option<i64> {
    std::env::var(name).ok()?.trim().parse().ok()
}

fn env_bool(name: &str) -> Option<bool> {
    let raw = std::env::var(name).ok()?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "y" => Some(true),
        "0" | "false" | "no" | "n" => Some(false),
        _ => None,
    }
}

fn next_value(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    it.next().ok_or_else(|| format!("missing value for {flag}"))
}

fn parse_next<T: std::str::FromStr>(
    it: &mut impl Iterator<Item = String>,
    flag: &str,
) -> Result<T, String> {
    next_value(it, flag)?
        .parse()
        .map_err(|_| format!("invalid value for {flag}"))
}

fn trim_trailing_slash(mut s: String) -> String {
    while s.ends_with('/') {
        s.pop();
    }
    s
}

fn usage() -> String {
    "usage: latency_probe [--samples N] [--interval-ms MS] [--region-label NAME] [--symbol BTCUSDT] [--yes-token TOKEN] [--no-token TOKEN] [--condition-id ID] [--allow-order-probe --order-probe-token TOKEN --order-probe-limit-ticks TICKS --order-probe-cents CENTS] [--binance-ws-url URL] [--clob-url URL] [--gamma-url URL] [--poly-market-ws-url URL] [--poly-user-ws-url URL]".to_owned()
}

fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[derive(Debug)]
struct Row<'a> {
    region: &'a str,
    probe: &'a str,
    seq: usize,
    ok: bool,
    status: i64,
    headers_us: i64,
    total_us: i64,
    first_msg_us: i64,
    event_lag_us: i64,
    bytes: i64,
    cf_ray: &'a str,
    error: &'a str,
}

impl Row<'_> {
    fn print(&self) {
        println!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{}",
            now_us(),
            csv(self.region),
            csv(self.probe),
            self.seq,
            self.ok,
            self.status,
            self.headers_us,
            self.total_us,
            self.first_msg_us,
            self.event_lag_us,
            self.bytes,
            csv(self.cf_ray),
            csv(self.error),
        );
    }
}

fn csv(s: &str) -> String {
    if s.bytes().any(|b| matches!(b, b',' | b'"' | b'\n' | b'\r')) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}

fn official_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("User-Agent", HeaderValue::from_static("rs_clob_client"));
    headers.insert("Accept", HeaderValue::from_static("*/*"));
    headers.insert("Connection", HeaderValue::from_static("keep-alive"));
    headers
}

fn client(args: &Args) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .default_headers(official_headers())
        .connect_timeout(Duration::from_millis(args.connect_timeout_ms))
        .timeout(Duration::from_millis(args.request_timeout_ms))
        .pool_idle_timeout(Duration::from_secs(75))
        .pool_max_idle_per_host(8)
        .tcp_keepalive(Duration::from_secs(20))
        .no_proxy()
        .build()
}

async fn probe_http(
    client: &reqwest::Client,
    region: &str,
    probe: &'static str,
    seq: usize,
    url: String,
) {
    let start = Instant::now();
    match client.get(&url).send().await {
        Ok(resp) => {
            let headers_us = start.elapsed().as_micros() as i64;
            let status = resp.status().as_u16() as i64;
            let cf_ray = resp
                .headers()
                .get("cf-ray")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("-")
                .to_owned();
            match resp.bytes().await {
                Ok(body) => Row {
                    region,
                    probe,
                    seq,
                    ok: (200..300).contains(&status),
                    status,
                    headers_us,
                    total_us: start.elapsed().as_micros() as i64,
                    first_msg_us: -1,
                    event_lag_us: -1,
                    bytes: body.len() as i64,
                    cf_ray: &cf_ray,
                    error: "-",
                }
                .print(),
                Err(e) => Row {
                    region,
                    probe,
                    seq,
                    ok: false,
                    status,
                    headers_us,
                    total_us: start.elapsed().as_micros() as i64,
                    first_msg_us: -1,
                    event_lag_us: -1,
                    bytes: -1,
                    cf_ray: &cf_ray,
                    error: &format!("body:{e}"),
                }
                .print(),
            }
        }
        Err(e) => Row {
            region,
            probe,
            seq,
            ok: false,
            status: 0,
            headers_us: -1,
            total_us: start.elapsed().as_micros() as i64,
            first_msg_us: -1,
            event_lag_us: -1,
            bytes: -1,
            cf_ray: "-",
            error: &format!("request:{e}"),
        }
        .print(),
    }
}

async fn probe_binance_ws(args: &Args) {
    let mut request = match args.binance_ws_url.clone().into_client_request() {
        Ok(r) => r,
        Err(e) => {
            Row {
                region: &args.region_label,
                probe: "binance_ws",
                seq: 0,
                ok: false,
                status: 0,
                headers_us: -1,
                total_us: -1,
                first_msg_us: -1,
                event_lag_us: -1,
                bytes: -1,
                cf_ray: "-",
                error: &format!("invalid_url:{e}"),
            }
            .print();
            return;
        }
    };
    if let Ok(key) = std::env::var("BINANCE_SBE_API_KEY")
        .or_else(|_| std::env::var("BINANCE_API_KEY"))
        .or_else(|_| std::env::var("PROBE_BINANCE_API_KEY"))
        .or_else(|_| Ok::<String, std::env::VarError>(HARDCODED_BINANCE_SBE_API_KEY.to_owned()))
        && let Ok(value) = key.parse()
    {
        request.headers_mut().insert("X-MBX-APIKEY", value);
    }

    let connect_start = Instant::now();
    let (mut ws, _resp) = match tokio_tungstenite::connect_async(request).await {
        Ok(pair) => pair,
        Err(e) => {
            Row {
                region: &args.region_label,
                probe: "binance_ws",
                seq: 0,
                ok: false,
                status: 0,
                headers_us: -1,
                total_us: connect_start.elapsed().as_micros() as i64,
                first_msg_us: -1,
                event_lag_us: -1,
                bytes: -1,
                cf_ray: "-",
                error: &format!("connect:{e}"),
            }
            .print();
            return;
        }
    };
    let connect_us = connect_start.elapsed().as_micros() as i64;
    for seq in 0..args.samples {
        let msg_start = Instant::now();
        match tokio::time::timeout(Duration::from_millis(args.request_timeout_ms), ws.next()).await
        {
            Ok(Some(Ok(msg))) => {
                if let Some((bytes, event_ts_us)) = binance_payload(&msg) {
                    let lag = event_ts_us
                        .filter(|ts| *ts > 0)
                        .map_or(-1, |ts| now_us().saturating_sub(ts));
                    Row {
                        region: &args.region_label,
                        probe: "binance_ws",
                        seq,
                        ok: true,
                        status: 101,
                        headers_us: if seq == 0 { connect_us } else { -1 },
                        total_us: if seq == 0 {
                            connect_us + msg_start.elapsed().as_micros() as i64
                        } else {
                            msg_start.elapsed().as_micros() as i64
                        },
                        first_msg_us: msg_start.elapsed().as_micros() as i64,
                        event_lag_us: lag,
                        bytes: bytes as i64,
                        cf_ray: "-",
                        error: "-",
                    }
                    .print();
                }
            }
            Ok(Some(Err(e))) => {
                Row {
                    region: &args.region_label,
                    probe: "binance_ws",
                    seq,
                    ok: false,
                    status: 101,
                    headers_us: if seq == 0 { connect_us } else { -1 },
                    total_us: msg_start.elapsed().as_micros() as i64,
                    first_msg_us: -1,
                    event_lag_us: -1,
                    bytes: -1,
                    cf_ray: "-",
                    error: &format!("read:{e}"),
                }
                .print();
                break;
            }
            Ok(None) => break,
            Err(_) => {
                Row {
                    region: &args.region_label,
                    probe: "binance_ws",
                    seq,
                    ok: false,
                    status: 101,
                    headers_us: if seq == 0 { connect_us } else { -1 },
                    total_us: args.request_timeout_ms as i64 * 1_000,
                    first_msg_us: -1,
                    event_lag_us: -1,
                    bytes: -1,
                    cf_ray: "-",
                    error: "timeout",
                }
                .print();
                break;
            }
        }
    }
}

fn binance_payload(msg: &Message) -> Option<(usize, Option<i64>)> {
    match msg {
        Message::Binary(b) => Some((b.len(), parse_sbe_event_time_us(b))),
        Message::Text(t) => Some((t.len(), parse_json_event_time_us(t.as_bytes()))),
        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => None,
        Message::Close(_) => Some((0, None)),
    }
}

fn parse_sbe_event_time_us(raw: &[u8]) -> Option<i64> {
    if raw.len() >= 16 && raw[0] == 0x32 && raw[1] == 0x00 && raw[2] == 0x11 && raw[3] == 0x27 {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&raw[8..16]);
        Some(i64::from_le_bytes(buf))
    } else {
        None
    }
}

fn parse_json_event_time_us(raw: &[u8]) -> Option<i64> {
    let v: serde_json::Value = serde_json::from_slice(raw).ok()?;
    let v = v.get("data").unwrap_or(&v);
    let n = v.get("E")?.as_i64()?;
    Some(if n > 10_000_000_000_000 { n } else { n * 1_000 })
}

async fn probe_poly_market_ws(args: &Args, seq: usize) {
    let assets = match (&args.yes_token, &args.no_token) {
        (Some(y), Some(n)) => format!("[\"{y}\",\"{n}\"]"),
        (Some(y), None) => format!("[\"{y}\"]"),
        (None, Some(n)) => format!("[\"{n}\"]"),
        (None, None) => "[]".to_owned(),
    };
    let subscribe =
        format!("{{\"assets_ids\":{assets},\"type\":\"market\",\"custom_feature_enabled\":true}}");
    let start = Instant::now();
    let (mut ws, _resp) =
        match tokio_tungstenite::connect_async(args.poly_market_ws_url.as_str()).await {
            Ok(pair) => pair,
            Err(e) => {
                Row {
                    region: &args.region_label,
                    probe: "poly_market_ws",
                    seq,
                    ok: false,
                    status: 0,
                    headers_us: -1,
                    total_us: start.elapsed().as_micros() as i64,
                    first_msg_us: -1,
                    event_lag_us: -1,
                    bytes: -1,
                    cf_ray: "-",
                    error: &format!("connect:{e}"),
                }
                .print();
                return;
            }
        };
    let connect_us = start.elapsed().as_micros() as i64;
    if assets != "[]"
        && let Err(e) = ws.send(Message::Text(subscribe.into())).await
    {
        Row {
            region: &args.region_label,
            probe: "poly_market_ws",
            seq,
            ok: false,
            status: 101,
            headers_us: connect_us,
            total_us: start.elapsed().as_micros() as i64,
            first_msg_us: -1,
            event_lag_us: -1,
            bytes: -1,
            cf_ray: "-",
            error: &format!("subscribe:{e}"),
        }
        .print();
        return;
    }
    let read_start = Instant::now();
    match tokio::time::timeout(Duration::from_millis(args.request_timeout_ms), ws.next()).await {
        Ok(Some(Ok(msg))) => Row {
            region: &args.region_label,
            probe: "poly_market_ws",
            seq,
            ok: true,
            status: 101,
            headers_us: connect_us,
            total_us: start.elapsed().as_micros() as i64,
            first_msg_us: read_start.elapsed().as_micros() as i64,
            event_lag_us: -1,
            bytes: message_len(&msg),
            cf_ray: "-",
            error: "-",
        }
        .print(),
        Ok(Some(Err(e))) => Row {
            region: &args.region_label,
            probe: "poly_market_ws",
            seq,
            ok: false,
            status: 101,
            headers_us: connect_us,
            total_us: start.elapsed().as_micros() as i64,
            first_msg_us: -1,
            event_lag_us: -1,
            bytes: -1,
            cf_ray: "-",
            error: &format!("read:{e}"),
        }
        .print(),
        Ok(None) => {}
        Err(_) => Row {
            region: &args.region_label,
            probe: "poly_market_ws",
            seq,
            ok: false,
            status: 101,
            headers_us: connect_us,
            total_us: start.elapsed().as_micros() as i64,
            first_msg_us: -1,
            event_lag_us: -1,
            bytes: -1,
            cf_ray: "-",
            error: "timeout",
        }
        .print(),
    }
}

struct UserWsAuth {
    api_key: String,
    api_secret: String,
    passphrase: String,
}

fn user_ws_auth_from_env() -> Option<UserWsAuth> {
    user_ws_auth_from_direct_env()
}

fn user_ws_auth_from_direct_env() -> Option<UserWsAuth> {
    let api_key = std::env::var("POLY_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())?;
    let api_secret = std::env::var("POLY_API_SECRET")
        .ok()
        .filter(|s| !s.is_empty())?;
    let passphrase = std::env::var("POLY_PASSPHRASE")
        .ok()
        .filter(|s| !s.is_empty())?;
    if api_key.is_empty() || api_secret.is_empty() || passphrase.is_empty() {
        return None;
    }
    Some(UserWsAuth {
        api_key,
        api_secret,
        passphrase,
    })
}

async fn user_ws_auth_from_hardcoded_pk(clob_url: &str) -> Option<UserWsAuth> {
    match minirust::auth::derive_api_credentials(HARDCODED_POLY_PK, POLYGON_CHAIN_ID, clob_url)
        .await
    {
        Ok((api_key, api_secret, passphrase, _address)) => Some(UserWsAuth {
            api_key,
            api_secret,
            passphrase,
        }),
        Err(_) => None,
    }
}

struct OrderProbeEnv {
    private_key: String,
    api_key: String,
    api_secret: String,
    passphrase: String,
    address: String,
    funder: Option<String>,
    signature_kind: String,
}

fn order_probe_env_from_env() -> Option<OrderProbeEnv> {
    let private_key = std::env::var("POLY_PK")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| HARDCODED_POLY_PK.to_owned());
    let api_key = std::env::var("POLY_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())?;
    let api_secret = std::env::var("POLY_API_SECRET")
        .ok()
        .filter(|s| !s.is_empty())?;
    let passphrase = std::env::var("POLY_PASSPHRASE")
        .ok()
        .filter(|s| !s.is_empty())?;
    let address = std::env::var("POLY_ADDRESS")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| HARDCODED_POLY_ADDRESS.to_owned());
    if private_key.is_empty()
        || api_key.is_empty()
        || api_secret.is_empty()
        || passphrase.is_empty()
        || address.is_empty()
    {
        return None;
    }
    Some(OrderProbeEnv {
        private_key,
        api_key,
        api_secret,
        passphrase,
        address,
        funder: std::env::var("POLY_FUNDER")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| Some(HARDCODED_POLY_FUNDER.to_owned())),
        signature_kind: std::env::var("POLY_SIGNATURE_KIND")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| HARDCODED_POLY_SIGNATURE_KIND.to_owned()),
    })
}

async fn order_probe_env_from_hardcoded_pk(clob_url: &str) -> Option<OrderProbeEnv> {
    let (api_key, api_secret, passphrase, address) =
        match minirust::auth::derive_api_credentials(HARDCODED_POLY_PK, POLYGON_CHAIN_ID, clob_url)
            .await
        {
            Ok(creds) => creds,
            Err(_) => return None,
        };
    Some(OrderProbeEnv {
        private_key: HARDCODED_POLY_PK.to_owned(),
        api_key,
        api_secret,
        passphrase,
        address,
        funder: Some(HARDCODED_POLY_FUNDER.to_owned()),
        signature_kind: HARDCODED_POLY_SIGNATURE_KIND.to_owned(),
    })
}

fn parse_signature_kind(raw: &str) -> Result<SignatureKind, String> {
    match raw.to_ascii_uppercase().as_str() {
        "EOA" => Ok(SignatureKind::Eoa),
        "POLY_PROXY" => Ok(SignatureKind::PolyProxy),
        "POLYGON_GNO_SAFE" | "POLY_GNOSIS_SAFE" => Ok(SignatureKind::PolyGnosisSafe),
        other => Err(format!("invalid_signature_kind:{other}")),
    }
}

fn build_order_probe_body(
    args: &Args,
    env: &OrderProbeEnv,
    salt: u64,
    timestamp_ms: u128,
) -> Result<minirust::signing::SignedFakOrderBody, String> {
    if !args.allow_order_probe {
        return Err("order_probe_not_allowed".to_owned());
    }
    let token = args
        .order_probe_token
        .as_ref()
        .or(args.yes_token.as_ref())
        .or(args.no_token.as_ref())
        .ok_or_else(|| "missing_order_probe_token".to_owned())?;
    let ticks = args
        .order_probe_limit_ticks
        .ok_or_else(|| "missing_order_probe_limit_ticks".to_owned())?;
    let price = PriceTick::checked(ticks).map_err(|e| format!("invalid_order_probe_price:{e}"))?;
    let target = canonical_buy_target_for_notional(BuyCanonicalInput {
        price,
        target_maker_cents: args.order_probe_cents,
        min_size_taker_units: 100,
        min_maker_cents: 100,
        max_overrun_cents: 0,
        max_overrun_bps: 0,
    })
    .map_err(|e| format!("canonical_buy:{e}"))?;
    let kind = parse_signature_kind(&env.signature_kind)?;
    let funder: Option<H160> = match &env.funder {
        Some(raw) => Some(raw.parse().map_err(|_| "invalid_poly_funder".to_owned())?),
        None => None,
    };
    let signer = OrderSigner::new(
        &env.private_key,
        &env.api_key,
        funder,
        kind,
        POLYGON_CHAIN_ID,
        EXCHANGE_V2_NORMAL,
    )
    .map_err(|e| format!("order_signer:{e}"))?;
    signer
        .sign_fak_buy(
            &TokenId::new(token.clone()),
            &target,
            SignInputs { salt, timestamp_ms },
        )
        .map_err(|e| format!("sign_buy:{e}"))
}

async fn probe_order_buy(client: &reqwest::Client, args: &Args, env: &OrderProbeEnv, seq: usize) {
    let body = match build_order_probe_body(args, env, now_us() as u64 ^ seq as u64, now_ms()) {
        Ok(body) => body,
        Err(e) => {
            Row {
                region: &args.region_label,
                probe: "poly_order_buy",
                seq,
                ok: false,
                status: 0,
                headers_us: -1,
                total_us: -1,
                first_msg_us: -1,
                event_lag_us: -1,
                bytes: -1,
                cf_ray: "-",
                error: &e,
            }
            .print();
            return;
        }
    };
    let auth = match L2AuthSigner::new(&env.api_key, &env.passphrase, &env.api_secret, &env.address)
    {
        Ok(auth) => auth,
        Err(e) => {
            Row {
                region: &args.region_label,
                probe: "poly_order_buy",
                seq,
                ok: false,
                status: 0,
                headers_us: -1,
                total_us: -1,
                first_msg_us: -1,
                event_lag_us: -1,
                bytes: -1,
                cf_ray: "-",
                error: &format!("l2_auth:{e}"),
            }
            .print();
            return;
        }
    };
    let url = format!("{}{}", args.clob_url, ORDER_PATH);
    let bytes = body.as_bytes();
    let headers = auth.headers("POST", ORDER_PATH, bytes, now_ms() as i64 / 1_000);
    let start = Instant::now();
    let mut req = client.post(url).body(bytes.to_vec());
    for (name, value) in headers.as_pairs() {
        req = req.header(name, value);
    }
    match req.send().await {
        Ok(resp) => {
            let headers_us = start.elapsed().as_micros() as i64;
            let status = resp.status().as_u16();
            let cf_ray = resp
                .headers()
                .get("cf-ray")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("-")
                .to_owned();
            match resp.bytes().await {
                Ok(raw) => {
                    let outcome = classify(status, raw);
                    let error = outcome.error_text().unwrap_or("-");
                    Row {
                        region: &args.region_label,
                        probe: "poly_order_buy",
                        seq,
                        ok: true,
                        status: outcome.http_status() as i64,
                        headers_us,
                        total_us: start.elapsed().as_micros() as i64,
                        first_msg_us: -1,
                        event_lag_us: -1,
                        bytes: bytes.len() as i64,
                        cf_ray: &cf_ray,
                        error,
                    }
                    .print();
                }
                Err(e) => Row {
                    region: &args.region_label,
                    probe: "poly_order_buy",
                    seq,
                    ok: false,
                    status: status as i64,
                    headers_us,
                    total_us: start.elapsed().as_micros() as i64,
                    first_msg_us: -1,
                    event_lag_us: -1,
                    bytes: bytes.len() as i64,
                    cf_ray: &cf_ray,
                    error: &format!("body:{e}"),
                }
                .print(),
            }
        }
        Err(e) => Row {
            region: &args.region_label,
            probe: "poly_order_buy",
            seq,
            ok: false,
            status: 0,
            headers_us: -1,
            total_us: start.elapsed().as_micros() as i64,
            first_msg_us: -1,
            event_lag_us: -1,
            bytes: bytes.len() as i64,
            cf_ray: "-",
            error: &format!("request:{e}"),
        }
        .print(),
    }
}

async fn probe_poly_user_ws(args: &Args, auth: &UserWsAuth, seq: usize) {
    let condition_id = match &args.condition_id {
        Some(s) => s,
        None => return,
    };
    let start = Instant::now();
    let (mut ws, _resp) =
        match tokio_tungstenite::connect_async(args.poly_user_ws_url.as_str()).await {
            Ok(pair) => pair,
            Err(e) => {
                Row {
                    region: &args.region_label,
                    probe: "poly_user_ws",
                    seq,
                    ok: false,
                    status: 0,
                    headers_us: -1,
                    total_us: start.elapsed().as_micros() as i64,
                    first_msg_us: -1,
                    event_lag_us: -1,
                    bytes: -1,
                    cf_ray: "-",
                    error: &format!("connect:{e}"),
                }
                .print();
                return;
            }
        };
    let connect_us = start.elapsed().as_micros() as i64;
    let auth_frame = serde_json::json!({
        "auth": {
            "apiKey": auth.api_key,
            "secret": auth.api_secret,
            "passphrase": auth.passphrase,
        },
        "markets": [condition_id],
        "type": "user",
    })
    .to_string();
    match ws.send(Message::Text(auth_frame.into())).await {
        Ok(()) => Row {
            region: &args.region_label,
            probe: "poly_user_ws",
            seq,
            ok: true,
            status: 101,
            headers_us: connect_us,
            total_us: start.elapsed().as_micros() as i64,
            first_msg_us: -1,
            event_lag_us: -1,
            bytes: 0,
            cf_ray: "-",
            error: "-",
        }
        .print(),
        Err(e) => Row {
            region: &args.region_label,
            probe: "poly_user_ws",
            seq,
            ok: false,
            status: 101,
            headers_us: connect_us,
            total_us: start.elapsed().as_micros() as i64,
            first_msg_us: -1,
            event_lag_us: -1,
            bytes: -1,
            cf_ray: "-",
            error: &format!("auth_send:{e}"),
        }
        .print(),
    }
}

fn message_len(msg: &Message) -> i64 {
    match msg {
        Message::Text(t) => t.len() as i64,
        Message::Binary(b) | Message::Ping(b) | Message::Pong(b) => b.len() as i64,
        Message::Close(_) => 0,
        Message::Frame(_) => -1,
    }
}

#[tokio::main]
async fn main() {
    let args = match Args::parse(std::env::args()) {
        Ok(args) => args,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(if msg.starts_with("usage:") { 0 } else { 2 });
        }
    };
    let client = match client(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("build_client:{e}");
            std::process::exit(2);
        }
    };
    let user_ws_auth = match user_ws_auth_from_env() {
        Some(auth) => Some(auth),
        None if args.condition_id.is_some() => user_ws_auth_from_hardcoded_pk(&args.clob_url).await,
        None => None,
    };
    let order_probe_env = if args.allow_order_probe {
        match order_probe_env_from_env() {
            Some(env) => Some(env),
            None => order_probe_env_from_hardcoded_pk(&args.clob_url).await,
        }
    } else {
        None
    };

    println!(
        "ts_us,region,probe,seq,ok,status,headers_us,total_us,first_msg_us,event_lag_us,bytes,cf_ray,error"
    );
    for seq in 0..args.samples {
        probe_http(
            &client,
            &args.region_label,
            "poly_clob_time",
            seq,
            format!("{}/time", args.clob_url),
        )
        .await;
        probe_http(
            &client,
            &args.region_label,
            "poly_clob_ok",
            seq,
            format!("{}/ok", args.clob_url),
        )
        .await;
        probe_http(
            &client,
            &args.region_label,
            "poly_gamma_events",
            seq,
            format!("{}/events?limit=1", args.gamma_url),
        )
        .await;
        if args.yes_token.is_some() || args.no_token.is_some() {
            probe_poly_market_ws(&args, seq).await;
        }
        if let Some(auth) = &user_ws_auth {
            probe_poly_user_ws(&args, auth, seq).await;
        }
        if args.allow_order_probe {
            match &order_probe_env {
                Some(env) => probe_order_buy(&client, &args, env, seq).await,
                None => Row {
                    region: &args.region_label,
                    probe: "poly_order_buy",
                    seq,
                    ok: false,
                    status: 0,
                    headers_us: -1,
                    total_us: -1,
                    first_msg_us: -1,
                    event_lag_us: -1,
                    bytes: -1,
                    cf_ray: "-",
                    error: "missing_order_probe_env",
                }
                .print(),
            }
        }
        tokio::time::sleep(Duration::from_millis(args.interval_ms)).await;
    }
    probe_binance_ws(&args).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cli_overrides() {
        let args = Args::parse(
            [
                "probe",
                "--samples",
                "7",
                "--interval-ms",
                "250",
                "--symbol",
                "ETHUSDT",
                "--yes-token",
                "yes",
                "--no-token",
                "no",
                "--condition-id",
                "0xcondition",
                "--clob-url",
                "https://clob.polymarket.com/",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(args.samples, 7);
        assert_eq!(args.interval_ms, 250);
        assert_eq!(
            args.binance_ws_url,
            "wss://stream-sbe.binance.com:9443/ws/ethusdt@bestBidAsk"
        );
        assert_eq!(args.yes_token.as_deref(), Some("yes"));
        assert_eq!(args.no_token.as_deref(), Some("no"));
        assert_eq!(args.condition_id.as_deref(), Some("0xcondition"));
        assert_eq!(args.clob_url, "https://clob.polymarket.com");
    }

    #[test]
    fn parses_sbe_event_time() {
        let mut raw = vec![0u8; 59];
        raw[0] = 0x32;
        raw[2] = 0x11;
        raw[3] = 0x27;
        raw[8..16].copy_from_slice(&1_779_000_000_123_456i64.to_le_bytes());
        assert_eq!(parse_sbe_event_time_us(&raw), Some(1_779_000_000_123_456));
    }

    #[test]
    fn csv_quotes_only_when_needed() {
        assert_eq!(csv("abc"), "abc");
        assert_eq!(csv("a,b"), "\"a,b\"");
        assert_eq!(csv("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn parses_order_probe_only_with_explicit_allow_flag() {
        let args = Args::parse(
            [
                "probe",
                "--allow-order-probe",
                "--order-probe-token",
                "12345678901234567890",
                "--order-probe-limit-ticks",
                "50",
                "--order-probe-cents",
                "10000",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .unwrap();
        assert!(args.allow_order_probe);
        assert_eq!(
            args.order_probe_token.as_deref(),
            Some("12345678901234567890")
        );
        assert_eq!(args.order_probe_limit_ticks, Some(50));
        assert_eq!(args.order_probe_cents, 10_000);
    }

    #[test]
    fn order_probe_body_is_signed_fak_buy_for_requested_notional() {
        let args = Args {
            allow_order_probe: true,
            yes_token: Some("12345678901234567890".to_owned()),
            order_probe_token: None,
            order_probe_limit_ticks: Some(50),
            order_probe_cents: 10_000,
            ..Args::default()
        };
        let env = OrderProbeEnv {
            private_key: "0x0000000000000000000000000000000000000000000000000000000000000001"
                .to_owned(),
            api_key: "00000000-0000-0000-0000-000000000001".to_owned(),
            api_secret: "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVoxMjM0NTY3ODkw".to_owned(),
            passphrase: "phrase".to_owned(),
            address: "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf".to_owned(),
            funder: None,
            signature_kind: "EOA".to_owned(),
        };
        let body = build_order_probe_body(&args, &env, 7, 1_777_000_000_000).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(body.as_bytes()).unwrap();
        assert_eq!(parsed["orderType"], "FAK");
        assert_eq!(parsed["order"]["side"], "BUY");
        assert_eq!(parsed["order"]["tokenId"], "12345678901234567890");
        assert_eq!(parsed["order"]["makerAmount"], "100000000");
        assert_eq!(parsed["order"]["takerAmount"], "200000000");
    }

    #[test]
    fn order_probe_defaults_to_100_usd_and_env_poly_limit() {
        let args = Args::parse(["probe"].into_iter().map(str::to_owned)).unwrap();
        assert_eq!(args.order_probe_cents, 10_000);
        assert_eq!(args.order_probe_limit_ticks, Some(85));
    }
}
