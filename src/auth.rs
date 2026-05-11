//! L2 authentication header generation for Polymarket REST.
//!
//! The signature is `HMAC-SHA256(secret_bytes, ts || method || path || body)`,
//! base64-url encoded with padding. Tests compare against the current live
//! Python implementation only to lock the wire format.
//!
//! Per-request timestamping means we cannot precompute the signature, but
//! the secret-decoding step (base64-url with padding) is one-shot and
//! amortizes across requests via `L2AuthSigner`.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE as BASE64_URL_SAFE;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Decoded credentials for L2 signing. Lifetime is the process; the secret
/// bytes never leave this struct.
#[derive(Clone)]
pub struct L2AuthSigner {
    api_key: String,
    passphrase: String,
    address: String,
    secret: Vec<u8>,
}

impl L2AuthSigner {
    /// Construct from the standard Polymarket API credential triple. The
    /// secret is decoded as URL-safe base64 with optional missing padding,
    /// matching the credential format used by Polymarket API keys.
    pub fn new(
        api_key: impl Into<String>,
        passphrase: impl Into<String>,
        api_secret_b64: &str,
        address: impl Into<String>,
    ) -> Result<Self, AuthError> {
        Ok(Self {
            api_key: api_key.into(),
            passphrase: passphrase.into(),
            address: address.into(),
            secret: decode_secret_b64_padded(api_secret_b64)?,
        })
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }
    pub fn passphrase(&self) -> &str {
        &self.passphrase
    }
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Build the six L2 headers for a request. `ts_secs` is the Unix
    /// timestamp in seconds; the caller supplies it so tests can lock the
    /// timestamp and live code uses `SystemTime::now()`.
    pub fn headers(&self, method: &str, path: &str, body: &[u8], ts_secs: i64) -> L2Headers {
        let method = method.to_ascii_uppercase();
        let ts = ts_secs.to_string();
        let mut mac =
            HmacSha256::new_from_slice(&self.secret).expect("HMAC-SHA256 accepts any key length");
        mac.update(ts.as_bytes());
        mac.update(method.as_bytes());
        mac.update(path.as_bytes());
        mac.update(body);
        let sig_bytes = mac.finalize().into_bytes();
        let signature = BASE64_URL_SAFE.encode(sig_bytes);

        L2Headers {
            poly_api_key: self.api_key.clone(),
            poly_passphrase: self.passphrase.clone(),
            poly_signature: signature,
            poly_timestamp: ts,
            poly_address: self.address.clone(),
            content_type: "application/json".into(),
        }
    }
}

/// Pre-rendered header pairs ready for HTTP attachment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct L2Headers {
    pub poly_api_key: String,
    pub poly_passphrase: String,
    pub poly_signature: String,
    pub poly_timestamp: String,
    pub poly_address: String,
    pub content_type: String,
}

impl L2Headers {
    /// Iterate as `(name, value)` pairs in the order Polymarket expects to
    /// see them. Order is not strictly required by HTTP/1.1 but matches
    /// Python's dict ordering for deterministic test snapshots.
    pub fn as_pairs(&self) -> [(&'static str, &str); 6] {
        [
            ("POLY_API_KEY", &self.poly_api_key),
            ("POLY_PASSPHRASE", &self.poly_passphrase),
            ("POLY_SIGNATURE", &self.poly_signature),
            ("POLY_TIMESTAMP", &self.poly_timestamp),
            ("POLY_ADDRESS", &self.poly_address),
            ("Content-Type", &self.content_type),
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    InvalidSecretBase64,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::InvalidSecretBase64 => write!(f, "invalid_secret_b64"),
        }
    }
}

impl std::error::Error for AuthError {}

/// Decode URL-safe base64, padding to a multiple of 4 if shorter. Mirrors
/// Python's `_b64_urlsafe_decode_padded`.
fn decode_secret_b64_padded(s: &str) -> Result<Vec<u8>, AuthError> {
    let pad = (4 - s.len() % 4) % 4;
    let mut buf = String::with_capacity(s.len() + pad);
    buf.push_str(s);
    for _ in 0..pad {
        buf.push('=');
    }
    BASE64_URL_SAFE
        .decode(buf.as_bytes())
        .map_err(|_| AuthError::InvalidSecretBase64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic credentials shared across L2 fixtures.
    /// secret_b64 decodes to ASCII "ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890".
    const API_KEY: &str = "00000000-0000-0000-0000-000000000001";
    const SECRET_B64: &str = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVoxMjM0NTY3ODkw";
    const PASSPHRASE: &str = "test-passphrase";
    const ADDRESS: &str = "0x000000000000000000000000000000000000dEaD";
    /// 2024-01-01 00:00:00 UTC.
    const TS_FIXED: i64 = 1_704_067_200;

    #[test]
    fn secret_decode_matches_python() {
        // Python: base64.urlsafe_b64decode("QUJ..." + "=" * (-len % 4))
        // -> bytes "ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890"
        let secret = decode_secret_b64_padded(SECRET_B64).unwrap();
        assert_eq!(secret, b"ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890");
    }

    #[test]
    fn signature_matches_python_get_data_orders() {
        // Captured from Python auth.L2Auth.headers at TS_FIXED:
        //   GET /data/orders body=b"" -> J5KxwOwqKqeWsE9yxJ4U3zPNZWHBGYtQYTFRBL_O8pg=
        let signer = L2AuthSigner::new(API_KEY, PASSPHRASE, SECRET_B64, ADDRESS).unwrap();
        let h = signer.headers("GET", "/data/orders", b"", TS_FIXED);
        assert_eq!(
            h.poly_signature,
            "J5KxwOwqKqeWsE9yxJ4U3zPNZWHBGYtQYTFRBL_O8pg="
        );
        assert_eq!(h.poly_timestamp, "1704067200");
        assert_eq!(h.poly_api_key, API_KEY);
        assert_eq!(h.poly_passphrase, PASSPHRASE);
        assert_eq!(h.poly_address, ADDRESS);
        assert_eq!(h.content_type, "application/json");
    }

    #[test]
    fn signature_matches_python_post_order_full_body() {
        // Captured from Python: POST /order with the JSON body below
        //   -> x6f0Hg9yxXs6uBnBDwuIaVDpfxtK43rBYvSGqTfho7g=
        let body =
            br#"{"order":{"id":1},"owner":"k","orderType":"FAK","postOnly":false,"deferExec":false}"#;
        let signer = L2AuthSigner::new(API_KEY, PASSPHRASE, SECRET_B64, ADDRESS).unwrap();
        let h = signer.headers("POST", "/order", body, TS_FIXED);
        assert_eq!(
            h.poly_signature,
            "x6f0Hg9yxXs6uBnBDwuIaVDpfxtK43rBYvSGqTfho7g="
        );
    }

    #[test]
    fn signature_matches_python_post_order_short_body() {
        // POST /order body=b'{"order":{"a":"b"}}' -> GC-M2ceYVmNc4J1L6H4UyY9bKIH_s7aS64bOqHjwdBg=
        let body = br#"{"order":{"a":"b"}}"#;
        let signer = L2AuthSigner::new(API_KEY, PASSPHRASE, SECRET_B64, ADDRESS).unwrap();
        let h = signer.headers("POST", "/order", body, TS_FIXED);
        assert_eq!(
            h.poly_signature,
            "GC-M2ceYVmNc4J1L6H4UyY9bKIH_s7aS64bOqHjwdBg="
        );
    }

    #[test]
    fn method_is_uppercased() {
        // Python uppercases internally; ensure we match for both 'post' and 'POST'.
        let signer = L2AuthSigner::new(API_KEY, PASSPHRASE, SECRET_B64, ADDRESS).unwrap();
        let h_lower = signer.headers("post", "/order", b"x", TS_FIXED);
        let h_upper = signer.headers("POST", "/order", b"x", TS_FIXED);
        assert_eq!(h_lower.poly_signature, h_upper.poly_signature);
    }

    #[test]
    fn pairs_render_order() {
        let signer = L2AuthSigner::new(API_KEY, PASSPHRASE, SECRET_B64, ADDRESS).unwrap();
        let h = signer.headers("GET", "/data/orders", b"", TS_FIXED);
        let names: Vec<&'static str> = h.as_pairs().iter().map(|(k, _)| *k).collect();
        assert_eq!(
            names,
            vec![
                "POLY_API_KEY",
                "POLY_PASSPHRASE",
                "POLY_SIGNATURE",
                "POLY_TIMESTAMP",
                "POLY_ADDRESS",
                "Content-Type",
            ]
        );
    }
}
