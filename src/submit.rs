//! Direct REST submitter for `POST /order`.
//!
//! No SDK indirection: we hand a locally validated pre-signed body to a pooled
//! `reqwest::Client` with L2 auth headers generated per request. This module
//! owns only the wire submit boundary and response classification.
//!
//! Response classification is the venue-correctness boundary. The plan
//! requires three buckets:
//!
//! * **Accepted** — venue confirmed acceptance with a usable `orderID`.
//! * **Rejected** — venue definitively did not accept the order. Examples:
//!   `"no orders found to match with FAK order"`, `"invalid amount for a
//!   marketable BUY order ($0.99), min size: $1"`, malformed body.
//! * **Unknown** — outcome is ambiguous: HTTP transport error, `0` status,
//!   `5xx` without an explicit `success: false` body, or a `2xx` body that
//!   confirms acceptance but lacks an `orderID`. Caller MUST keep the
//!   pending submit alive for WSS reconciliation; the order may have
//!   actually filled on the venue side.
//!
//! Live evidence (Python live logs, `bot_live_2026-05-07*.log`) showed
//! UNKNOWN-to-WSS-bind recovery happening for transport timeouts. This
//! implementation preserves that behaviour explicitly.

use std::time::Duration;

use bytes::Bytes;
use reqwest::Client;
use serde_json::Value;

use crate::auth::L2AuthSigner;
use crate::signing::SignedFakOrderBody;

/// Path component of the order endpoint, relative to the configured
/// base URL.
pub const ORDER_PATH: &str = "/order";

/// Per-request HTTP timeout. The Python value (`total=2.0s,
/// sock_connect=0.5s, sock_read=2.0s`) was chosen against eu-west-1 →
/// Polymarket-US RTT (~300-400ms p95). 2 s gives p99 headroom while
/// failing fast on FAK orders that would expire well before 5 s.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

/// Outcome of a submit attempt, after the classifier has run on the
/// response body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// 2xx with parseable body and a usable `orderID`.
    Accepted {
        order_id: String,
        http_status: u16,
        raw_body: Bytes,
    },
    /// 4xx, or 2xx with `success: false`, or 5xx with explicit
    /// `success: false` and a non-transport error message.
    Rejected {
        http_status: u16,
        error: Option<String>,
        raw_body: Bytes,
    },
    /// Transport-level failure, ambiguous 5xx, or 2xx that confirmed
    /// acceptance but lacked `orderID`. Caller must keep pending submit
    /// alive for WSS reconciliation.
    Unknown {
        http_status: u16,
        error: Option<String>,
        raw_body: Bytes,
    },
}

impl SubmitOutcome {
    pub fn is_accepted(&self) -> bool {
        matches!(self, SubmitOutcome::Accepted { .. })
    }
    pub fn is_rejected(&self) -> bool {
        matches!(self, SubmitOutcome::Rejected { .. })
    }
    pub fn is_unknown(&self) -> bool {
        matches!(self, SubmitOutcome::Unknown { .. })
    }

    pub fn http_status(&self) -> u16 {
        match self {
            SubmitOutcome::Accepted { http_status, .. }
            | SubmitOutcome::Rejected { http_status, .. }
            | SubmitOutcome::Unknown { http_status, .. } => *http_status,
        }
    }

    pub fn error_text(&self) -> Option<&str> {
        match self {
            SubmitOutcome::Accepted { .. } => None,
            SubmitOutcome::Rejected { error, .. } | SubmitOutcome::Unknown { error, .. } => {
                error.as_deref()
            }
        }
    }
}

/// HTTP submitter wrapping a pooled reqwest client and the L2 auth
/// signer. One per process; clone is cheap (reqwest's `Client` is
/// `Arc`-shared internally).
#[derive(Clone)]
pub struct HttpSubmitter {
    client: Client,
    base_url: String,
    auth: L2AuthSigner,
    now_secs: fn() -> i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitError {
    BuildClient(String),
    InvalidBaseUrl,
}

impl std::fmt::Display for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubmitError::BuildClient(e) => write!(f, "build_client: {e}"),
            SubmitError::InvalidBaseUrl => write!(f, "invalid_base_url"),
        }
    }
}

impl std::error::Error for SubmitError {}

impl HttpSubmitter {
    /// Construct a new submitter. `base_url` should be the bare CLOB host
    /// without a trailing slash (e.g. `"https://clob.polymarket.com"`).
    pub fn new(base_url: &str, auth: L2AuthSigner) -> Result<Self, SubmitError> {
        if base_url.is_empty() || base_url.ends_with('/') {
            return Err(SubmitError::InvalidBaseUrl);
        }
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .pool_idle_timeout(Duration::from_secs(75))
            .pool_max_idle_per_host(8)
            .https_only(false) // base_url scheme controls; allows mock servers in tests.
            .user_agent("minirust/0.0.1")
            .build()
            .map_err(|e| SubmitError::BuildClient(format!("{e}")))?;
        Ok(Self {
            client,
            base_url: base_url.to_owned(),
            auth,
            now_secs: default_now_secs,
        })
    }

    /// Fire a lightweight GET to warm the TLS connection pool.
    /// Called on market rotation so the first POST doesn't hit a cold connection.
    pub async fn warm_connection(&self) {
        let url = format!("{}/tick-size", self.base_url);
        let _ = self.client.get(&url).send().await;
    }

    /// Submit a locally signed, locally validated FAK order body.
    pub async fn submit_order(&self, body: &SignedFakOrderBody) -> SubmitOutcome {
        let url = format!("{}{}", self.base_url, ORDER_PATH);
        let ts = (self.now_secs)();
        let bytes = body.as_bytes();
        let headers = self.auth.headers("POST", ORDER_PATH, bytes, ts);

        let mut req = self
            .client
            .post(&url)
            .body(bytes.to_vec())
            .header("Content-Type", "application/json");
        for (name, value) in headers.as_pairs() {
            req = req.header(name, value);
        }

        match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let raw = match resp.bytes().await {
                    Ok(b) => b,
                    Err(e) => {
                        return SubmitOutcome::Unknown {
                            http_status: status,
                            error: Some(format!("read_body: {e}")),
                            raw_body: Bytes::new(),
                        };
                    }
                };
                classify(status, raw)
            }
            Err(e) => SubmitOutcome::Unknown {
                http_status: 0,
                error: Some(format!("transport_error: {e}")),
                raw_body: Bytes::new(),
            },
        }
    }
}

/// Default unix-seconds source. Unwraps to 0 on the unreasonable
/// pre-1970 system clock state, which the L2 signer will then reject
/// at the venue side.
fn default_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Classify an HTTP response. Public so unit tests can exercise the
/// branch tree without spinning up a mock server.
///
/// The invariant is live risk, not Python parity: definitive rejection stops
/// local pending state; ambiguous transport/server outcomes stay UNKNOWN for
/// user-WSS reconciliation.
pub fn classify(http_status: u16, raw_body: Bytes) -> SubmitOutcome {
    let body_value: Option<Value> = serde_json::from_slice(&raw_body).ok();
    let success = body_value
        .as_ref()
        .and_then(|v| v.get("success"))
        .and_then(|v| v.as_bool());
    let error = body_value
        .as_ref()
        .and_then(|v| extract_error_field(v))
        .map(|s| s.to_owned());

    // Transport error / status==0 → unknown.
    if http_status == 0 || error.as_deref() == Some("transport_error") {
        return SubmitOutcome::Unknown {
            http_status,
            error,
            raw_body,
        };
    }

    // 5xx: ambiguous unless body explicitly says "no order placed".
    if http_status >= 500 {
        let definitive_reject = success == Some(false)
            && error
                .as_deref()
                .is_some_and(|e| !e.to_ascii_lowercase().contains("transport"));
        return if definitive_reject {
            SubmitOutcome::Rejected {
                http_status,
                error,
                raw_body,
            }
        } else {
            SubmitOutcome::Unknown {
                http_status,
                error,
                raw_body,
            }
        };
    }

    // 4xx: definitive rejection.
    if http_status >= 400 {
        return SubmitOutcome::Rejected {
            http_status,
            error,
            raw_body,
        };
    }

    // 2xx: check for explicit failure flag.
    if success == Some(false) {
        return SubmitOutcome::Rejected {
            http_status,
            error,
            raw_body,
        };
    }

    // 2xx accepted: extract orderID. Missing orderID → unknown (the
    // venue says "ok" but we don't know which order it was).
    let order_id = body_value.as_ref().and_then(extract_order_id);
    match order_id {
        Some(id) if !id.is_empty() => SubmitOutcome::Accepted {
            order_id: id,
            http_status,
            raw_body,
        },
        _ => SubmitOutcome::Unknown {
            http_status,
            error: Some("accepted_missing_order_id".into()),
            raw_body,
        },
    }
}

/// Extract a non-empty `orderID` (or aliases) from the response, walking
/// the same key-name fallbacks as Python `extract_order_id`.
fn extract_order_id(value: &Value) -> Option<String> {
    fn shallow(value: &Value) -> Option<String> {
        for key in ["orderID", "orderId", "order_id"] {
            if let Some(s) = value.get(key).and_then(|v| v.as_str())
                && !s.is_empty()
            {
                return Some(s.to_owned());
            }
        }
        // Nested {"order": {"id": "..."}} envelope.
        if let Some(nested_id) = value
            .get("order")
            .and_then(|o| o.get("id"))
            .and_then(|v| v.as_str())
            && !nested_id.is_empty()
        {
            return Some(nested_id.to_owned());
        }
        None
    }
    if let Some(id) = shallow(value) {
        return Some(id);
    }
    // Recurse through common envelope wrappers.
    for key in ["data", "result", "response", "payload"] {
        if let Some(inner) = value.get(key)
            && let Some(id) = extract_order_id(inner)
        {
            return Some(id);
        }
    }
    None
}

/// Extract the canonical error text from a response, preferring `error`
/// over `errorMsg`.
fn extract_error_field(value: &Value) -> Option<&str> {
    if let Some(e) = value.get("error").and_then(|v| v.as_str())
        && !e.is_empty()
    {
        return Some(e);
    }
    if let Some(e) = value.get("errorMsg").and_then(|v| v.as_str())
        && !e.is_empty()
    {
        return Some(e);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(s: &str) -> Bytes {
        Bytes::copy_from_slice(s.as_bytes())
    }

    #[test]
    fn http_200_with_order_id_is_accepted() {
        let r = classify(200, b(r#"{"orderID":"0xabc"}"#));
        match r {
            SubmitOutcome::Accepted { order_id, .. } => assert_eq!(order_id, "0xabc"),
            other => panic!("expected accepted, got {other:?}"),
        }
    }

    #[test]
    fn http_200_without_order_id_is_unknown() {
        // Venue says ok but we have no id to bind WSS events to.
        let r = classify(200, b(r#"{"success":true}"#));
        assert!(r.is_unknown());
    }

    #[test]
    fn http_200_success_false_is_rejected() {
        let r = classify(
            200,
            b(r#"{"success":false,"error":"insufficient balance"}"#),
        );
        assert!(r.is_rejected());
    }

    #[test]
    fn http_400_is_rejected_regardless_of_body() {
        // FAK no-match comes back as 400. Body parses, error is set.
        let r = classify(
            400,
            b(
                r#"{"error":"no orders found to match with FAK order. FAK orders are partially filled or killed if no match is found."}"#,
            ),
        );
        assert!(r.is_rejected());
    }

    #[test]
    fn http_400_with_min_size_error_is_rejected() {
        // Below-floor BUY (probe scenario from 2026-05-07).
        let r = classify(
            400,
            b(r#"{"error":"invalid amount for a marketable BUY order ($0.99), min size: $1"}"#),
        );
        match r {
            SubmitOutcome::Rejected { error: Some(e), .. } => {
                assert!(e.contains("min size: $1"), "{e}");
            }
            other => panic!("expected rejected, got {other:?}"),
        }
    }

    #[test]
    fn http_500_without_explicit_failure_is_unknown() {
        // Bare 5xx with no body — order MAY have landed.
        let r = classify(503, Bytes::new());
        assert!(r.is_unknown());
    }

    #[test]
    fn http_500_with_explicit_failure_is_rejected() {
        // 5xx with body explicitly saying "did not place" → rejected.
        let r = classify(
            500,
            b(r#"{"success":false,"error":"validation: bad nonce"}"#),
        );
        assert!(r.is_rejected());
    }

    #[test]
    fn http_500_with_transport_error_text_remains_unknown() {
        // Even success=false, if error mentions "transport" we treat as
        // ambiguous — Python kept this carve-out because some venue
        // proxies surface transport hiccups inside 500 bodies.
        let r = classify(
            502,
            b(r#"{"success":false,"error":"upstream transport reset"}"#),
        );
        assert!(r.is_unknown());
    }

    #[test]
    fn transport_status_zero_is_unknown() {
        let r = classify(0, b(r#"{"error":"transport_error"}"#));
        assert!(r.is_unknown());
    }

    #[test]
    fn order_id_extracts_from_nested_envelope() {
        let r = classify(200, b(r#"{"data":{"orderID":"0xnested"}}"#));
        match r {
            SubmitOutcome::Accepted { order_id, .. } => {
                assert_eq!(order_id, "0xnested");
            }
            other => panic!("expected accepted, got {other:?}"),
        }
    }

    #[test]
    fn order_id_extracts_from_order_id_under_order_envelope() {
        // Some payloads wrap as { order: { id: ... } }.
        let r = classify(200, b(r#"{"order":{"id":"0xenveloped"}}"#));
        match r {
            SubmitOutcome::Accepted { order_id, .. } => {
                assert_eq!(order_id, "0xenveloped");
            }
            other => panic!("expected accepted, got {other:?}"),
        }
    }

    #[test]
    fn unparseable_body_at_2xx_is_unknown() {
        // 200 but body isn't JSON — treat as unknown (no orderID
        // extractable, no `success:false` to definitively reject).
        let r = classify(200, b("garbage not json"));
        assert!(r.is_unknown());
    }

    #[test]
    fn invalid_base_url_rejected() {
        let signer = L2AuthSigner::new(
            "00000000-0000-0000-0000-000000000001",
            "p",
            "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVoxMjM0NTY3ODkw",
            "0x0",
        )
        .unwrap();
        assert!(HttpSubmitter::new("", signer.clone()).is_err());
        // Trailing slash: also rejected to keep path-joining unambiguous.
        assert!(HttpSubmitter::new("https://x.test/", signer).is_err());
    }
}
