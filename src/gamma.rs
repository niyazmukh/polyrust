//! Gamma REST API client for Polymarket market discovery.
//!
//! Traces to:
//!   market_ws.py:116-173  (_discover_current_market)
//!   http_client.py:45-62  (get_clob_time, gamma_get_event_by_slug)
//!   market_ws.py:90-113   (_select_yes_no)

use crate::state::MarketContext;
use crate::types::{ConditionId, TokenId};
use serde_json::Value;

/// A token parsed from Gamma's `clobTokenIds` array.
#[derive(Clone, Debug)]
pub struct ClobToken {
    pub token_id: TokenId,
    pub outcome: String,
}

/// Stateless Gamma REST client. `reqwest::Client` is cheap to clone
/// (Arc-shared pool internally).
#[derive(Clone)]
pub struct GammaClient {
    clob_url: String,
    gamma_url: String,
    slug_fmt: String,
    window_s: i64,
    client: reqwest::Client,
}

impl GammaClient {
    pub fn new(clob_url: &str, gamma_url: &str, slug_fmt: &str, window_s: i64) -> Self {
        Self {
            clob_url: clob_url.to_owned(),
            gamma_url: gamma_url.to_owned(),
            slug_fmt: slug_fmt.to_owned(),
            window_s,
            client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_millis(500))
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap_or_default(),
        }
    }

    // ------------------------------------------------------------------
    // CLOB server time
    // ------------------------------------------------------------------

    /// GET `{clob_url}/time` → unix seconds. Normalises ms→s if the value
    /// exceeds 10_000_000_000.
    ///
    /// Traces to: http_client.py:45-62 (`get_clob_time`).
    pub async fn get_clob_time(&self) -> Result<i64, String> {
        let url = format!("{}/time", self.clob_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("clob /time request failed: {e}"))?;
        let body = resp
            .text()
            .await
            .map_err(|e| format!("clob /time read failed: {e}"))?;
        let raw: i64 = body
            .trim()
            .parse()
            .map_err(|e| format!("clob /time parse failed: {e}"))?;
        // Normalise ms → s if the value looks like a millisecond timestamp.
        Ok(if raw > 10_000_000_000 {
            raw / 1000
        } else {
            raw
        })
    }

    // ------------------------------------------------------------------
    // Slug computation
    // ------------------------------------------------------------------

    /// Compute the slug timestamp: snap `server_ts` down to the nearest
    /// `window_s` boundary.
    ///
    /// Traces to: market_ws.py:118-119.
    pub fn slug_ts(&self, server_ts: i64) -> i64 {
        server_ts - server_ts.rem_euclid(self.window_s)
    }

    // ------------------------------------------------------------------
    // Market discovery
    // ------------------------------------------------------------------

    /// Discover the current active binary-option market.
    ///
    /// 1. Get CLOB server time.
    /// 2. Compute current window `slug_ts`, try it and the next window.
    /// 3. For each: GET `gamma-api.polymarket.com/events/slug/{slug}`.
    /// 4. Find the first active, accepting-orders, non-closed market.
    /// 5. Parse `clobTokenIds` and select YES/NO tokens by label.
    ///
    /// Traces to: market_ws.py:116-173 (`_discover_current_market`).
    pub async fn discover(&self) -> Option<MarketContext> {
        let server_ts = self.get_clob_time().await.ok()?;
        let base_slug_ts = self.slug_ts(server_ts);

        for candidate_ts in [base_slug_ts, base_slug_ts + self.window_s] {
            let slug = self.slug_fmt.replace("{ts}", &candidate_ts.to_string());
            if let Some(ctx) = self.try_discover_slug(&slug, candidate_ts).await {
                return Some(ctx);
            }
        }
        None
    }

    async fn try_discover_slug(&self, slug: &str, slug_ts: i64) -> Option<MarketContext> {
        let url = format!("{}/events/slug/{}", self.gamma_url, slug);
        let resp = self.client.get(&url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let event: Value = resp.json().await.ok()?;

        // Gate: must be active and not closed.
        if !event
            .get("active")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return None;
        }
        if event
            .get("closed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return None;
        }

        let markets = event.get("markets")?.as_array()?;
        for market in markets {
            if !market
                .get("active")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                continue;
            }
            if !market
                .get("acceptingOrders")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                continue;
            }
            if market
                .get("closed")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                continue;
            }

            let condition_id = market
                .get("conditionId")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())?;

            // Parse clobTokenIds and outcomes
            let tokens = parse_clob_tokens(market.get("clobTokenIds")?, market.get("outcomes"))?;
            let (yes_tok, no_tok) = select_yes_no(&tokens)?;

            // Parse start/end timestamps — Gamma uses ISO-8601 strings.
            // For our purposes, we use slug_ts as start_ts and derive end_ts.
            let _start_raw = market
                .get("startDate")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let end_raw = market.get("endDate").and_then(|v| v.as_str()).unwrap_or("");
            let end_ts = parse_gamma_iso8601(end_raw).unwrap_or(slug_ts + self.window_s);

            // Already expired? Skip.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            if end_ts <= now {
                continue;
            }

            return Some(MarketContext {
                slug: slug.to_owned(),
                condition_id: ConditionId::new(condition_id),
                yes_token: yes_tok.token_id,
                no_token: no_tok.token_id,
                end_ts,
                slug_ts,
            });
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Token parsing
// ---------------------------------------------------------------------------

/// Parse `clobTokenIds` and `outcomes` from Gamma's JSON representation.
fn parse_clob_tokens(value: &Value, outcomes: Option<&Value>) -> Option<Vec<ClobToken>> {
    // try object array first (case 4 logic)
    if let Some(arr) = value.as_array()
        && arr.first().and_then(|v| v.as_object()).is_some()
    {
        return Some(parse_token_objects(arr));
    }
    if let Some(raw) = value.as_str()
        && let Ok(parsed) = serde_json::from_str::<Vec<Value>>(raw)
        && parsed.first().and_then(|v| v.as_object()).is_some()
    {
        return Some(parse_token_objects(&parsed));
    }

    let parse_str_array = |v: &Value| -> Option<Vec<String>> {
        if let Some(arr) = v.as_array() {
            return Some(
                arr.iter()
                    .filter_map(|s| s.as_str().map(|s| s.to_owned()))
                    .collect(),
            );
        }
        if let Some(s) = v.as_str()
            && let Ok(p) = serde_json::from_str::<Vec<String>>(s)
        {
            return Some(p);
        }
        None
    };

    let ids = parse_str_array(value)?;
    let outs = outcomes.and_then(parse_str_array).unwrap_or_default();

    Some(
        ids.into_iter()
            .enumerate()
            .map(|(i, id)| ClobToken {
                token_id: TokenId::new(id),
                outcome: outs.get(i).cloned().unwrap_or_default(),
            })
            .collect(),
    )
}

fn parse_token_objects(arr: &[Value]) -> Vec<ClobToken> {
    arr.iter()
        .filter_map(|obj| {
            let id = obj
                .get("id")
                .or_else(|| obj.get("tokenId"))
                .or_else(|| obj.get("token_id"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())?;
            let outcome = obj
                .get("outcome")
                .or_else(|| obj.get("label"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            Some(ClobToken {
                token_id: TokenId::new(id),
                outcome,
            })
        })
        .collect()
}

/// Select YES/NO tokens from Gamma's CLOB token list by matching outcome
/// labels against known keywords.
///
/// YES matches: `yes`, `up`, `above`, `higher`
/// NO matches:  `no`, `down`, `below`, `lower`
///
/// Traces to: market_ws.py:90-113 (`_select_yes_no`).
fn select_yes_no(tokens: &[ClobToken]) -> Option<(ClobToken, ClobToken)> {
    let yes_keywords = ["yes", "up", "above", "higher"];
    let no_keywords = ["no", "down", "below", "lower"];

    let yes = tokens.iter().find(|t| {
        let lo = t.outcome.to_ascii_lowercase();
        yes_keywords.iter().any(|kw| lo.contains(kw))
    })?;

    let no = tokens.iter().find(|t| {
        let lo = t.outcome.to_ascii_lowercase();
        no_keywords.iter().any(|kw| lo.contains(kw)) && t.token_id != yes.token_id
    })?;

    Some((yes.clone(), no.clone()))
}

/// Parse a Gamma ISO-8601 timestamp to Unix seconds. Strips fractional
/// seconds and optional `Z` suffix. Handles both `2026-05-10T12:00:00Z`
/// and `2026-05-10T12:00:00.123Z` formats.
///
/// Traces to: utils.py:64 (`parse_gamma_iso8601_to_unix`).
fn parse_gamma_iso8601(raw: &str) -> Option<i64> {
    // Very narrow parser: we only need to extract the unix timestamp.
    // Formats seen in the wild:
    //   "2026-05-10T12:00:00Z"
    //   "2026-05-10T12:00:00.000Z"
    //   "2026-05-10T12:00:00+00:00"
    let body = raw.trim().trim_matches('"');
    // Strip trailing Z / timezone.
    let body = body
        .strip_suffix('Z')
        .or_else(|| body.strip_suffix("+00:00"))
        .unwrap_or(body);
    // Strip fractional seconds.
    let body = match body.find('.') {
        Some(dot) => &body[..dot],
        None => body,
    };
    // Use chrono-like manual parsing to avoid adding a dependency.
    // Format: YYYY-MM-DDTHH:MM:SS
    let parts: Vec<&str> = body.split(&['-', 'T', ':']).collect();
    if parts.len() < 6 {
        return None;
    }
    let year: i64 = parts[0].parse().ok()?;
    let month: i64 = parts[1].parse().ok()?;
    let day: i64 = parts[2].parse().ok()?;
    let hour: i64 = parts[3].parse().ok()?;
    let min: i64 = parts[4].parse().ok()?;
    let sec: i64 = parts[5].parse().ok()?;

    // Days since epoch using a simple algorithm.
    let a = (14 - month) / 12;
    let y = year + 4800 - a;
    let m = month + 12 * a - 3;
    let jdn = day + (153 * m + 2) / 5 + 365 * y + y / 4 - y / 100 + y / 400 - 32045;
    let epoch_days = jdn - 2_440_588; // days from 1970-01-01
    Some(epoch_days * 86_400 + hour * 3_600 + min * 60 + sec)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_ts_snaps_to_window() {
        let gc = GammaClient::new(
            "https://clob.polymarket.com",
            "https://gamma-api.polymarket.com",
            "btc-updown-5m-{ts}",
            300,
        );
        // Use values that are cleanly divisible: 1_777_000_200 / 300 = 5_923_334 exactly.
        assert_eq!(gc.slug_ts(1_777_000_230), 1_777_000_200); // 30s in → snapped down
        assert_eq!(gc.slug_ts(1_777_000_200), 1_777_000_200); // exact boundary
        assert_eq!(gc.slug_ts(1_777_000_499), 1_777_000_200); // 299s → still same window
    }

    #[test]
    fn select_yes_no_finds_keywords() {
        let tokens = vec![
            ClobToken {
                token_id: TokenId::new("tok_yes"),
                outcome: "Up".into(),
            },
            ClobToken {
                token_id: TokenId::new("tok_no"),
                outcome: "Down".into(),
            },
        ];
        let (y, n) = select_yes_no(&tokens).unwrap();
        assert_eq!(y.token_id.as_str(), "tok_yes");
        assert_eq!(n.token_id.as_str(), "tok_no");
    }

    #[test]
    fn select_yes_no_finds_yes_no_literally() {
        let tokens = vec![
            ClobToken {
                token_id: TokenId::new("a"),
                outcome: "Yes".into(),
            },
            ClobToken {
                token_id: TokenId::new("b"),
                outcome: "No".into(),
            },
        ];
        let (y, n) = select_yes_no(&tokens).unwrap();
        assert_eq!(y.token_id.as_str(), "a");
        assert_eq!(n.token_id.as_str(), "b");
    }

    #[test]
    fn select_yes_no_returns_none_when_only_yes_exists() {
        let tokens = vec![ClobToken {
            token_id: TokenId::new("only_yes"),
            outcome: "Yes".into(),
        }];
        assert!(select_yes_no(&tokens).is_none());
    }

    #[test]
    fn parse_iso8601_golden_noon_utc() {
        // Golden value: 2026-05-10T12:00:00Z = 1_778_414_400 Unix seconds.
        // Computed via: JDN 2_461_171 - 2_440_588 epoch days = 20_583 days
        //              * 86_400 + 12*3_600 = 1_778_414_400.
        let ts = parse_gamma_iso8601("2026-05-10T12:00:00Z").unwrap();
        assert_eq!(ts, 1_778_414_400);
    }

    #[test]
    fn parse_iso8601_golden_midnight_utc() {
        // 2026-05-10T00:00:00Z = 1_778_371_200 (noon - 12h).
        let ts = parse_gamma_iso8601("2026-05-10T00:00:00Z").unwrap();
        assert_eq!(ts, 1_778_371_200);
    }

    #[test]
    fn parse_iso8601_with_fractional() {
        let ts1 = parse_gamma_iso8601("2026-05-10T12:00:00Z").unwrap();
        let ts2 = parse_gamma_iso8601("2026-05-10T12:00:00.123Z").unwrap();
        assert_eq!(ts1, ts2); // fractional discarded but same second
    }

    #[test]
    fn parse_iso8601_with_offset_timezone() {
        // +00:00 timezone should match Z suffix exactly.
        let ts_z = parse_gamma_iso8601("2026-05-10T12:00:00Z").unwrap();
        let ts_offset = parse_gamma_iso8601("2026-05-10T12:00:00+00:00").unwrap();
        assert_eq!(ts_z, ts_offset);
    }
}
