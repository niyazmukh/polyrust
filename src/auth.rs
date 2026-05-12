//! L2 authentication header generation for Polymarket REST.
//!
//! The signature is `HMAC-SHA256(secret_bytes, ts || method || path || body)`,
//! base64-url encoded with padding. Tests compare against the current live
//! Python implementation only to lock the wire format.
//!
//! Per-request timestamping means we cannot precompute the signature, but
//! the secret-decoding step (base64-url with padding) is one-shot and
//! amortizes across requests via `L2AuthSigner`.
//!
//! L1 credential derivation (private key → API key/secret/passphrase) is
//! provided by `derive_api_credentials`. This calls the Polymarket
//! `/auth/derive-api-key` or `/auth/api-key` endpoint with an EIP-712
//! `ClobAuth` signature, matching the official SDK's L1 auth flow.
//! Derivation runs once at startup; it is never on the hot path.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE as BASE64_URL_SAFE;
use hmac::{Hmac, Mac};
use k256::ecdsa::{RecoveryId, Signature as EcdsaSignature, SigningKey, VerifyingKey};
use primitive_types::{H160, H256};
use serde::Deserialize;
use sha2::Sha256;
use sha3::{Digest, Keccak256};

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

// ---------------------------------------------------------------------------
// L1 credential derivation — private key → API credentials
// ---------------------------------------------------------------------------

/// Credentials returned by the Polymarket `/auth/derive-api-key` or
/// `/auth/api-key` endpoint.
#[derive(Debug, Clone, Deserialize)]
struct DerivedCredentials {
    #[serde(rename = "apiKey")]
    api_key: String,
    secret: String,
    passphrase: String,
}

/// EIP-712 type string for the `ClobAuth` struct — pinned to the
/// Polymarket SDK's definition. Changing this invalidates L1 signatures.
const CLOB_AUTH_TYPE_STRING: &str =
    "ClobAuth(address address,string timestamp,uint256 nonce,string message)";

/// Domain name for L1 ClobAuth EIP-712 signatures.
const CLOB_AUTH_DOMAIN_NAME: &str = "ClobAuthDomain";
const CLOB_AUTH_DOMAIN_VERSION: &str = "1";

/// The fixed attestation message signed by L1 credential derivation.
const CLOB_AUTH_MESSAGE: &str = "This message attests that I control the given wallet";

/// Derive Polymarket API credentials (key, secret, passphrase) from an
/// Ethereum private key. Calls the CLOB `/auth/derive-api-key` endpoint
/// (falling back to `POST /auth/api-key` on status errors), authenticated
/// with an EIP-712 `ClobAuth` signature.
///
/// Returns `(api_key, secret, passphrase, eoa_address_hex)`. The EOA
/// address is the address the API key is associated with — callers must
/// use it as `POLY_ADDRESS` in L2 headers.
///
/// This is a startup-only operation — never on the hot path.
pub async fn derive_api_credentials(
    private_key_hex: &str,
    chain_id: u64,
    clob_url: &str,
) -> Result<(String, String, String, String), AuthError> {
    let pk_hex = private_key_hex
        .strip_prefix("0x")
        .unwrap_or(private_key_hex);
    let pk_bytes = hex::decode(pk_hex).map_err(|_| AuthError::InvalidSecretBase64)?;
    if pk_bytes.len() != 32 {
        return Err(AuthError::InvalidSecretBase64);
    }
    let signing_key =
        SigningKey::from_slice(&pk_bytes).map_err(|_| AuthError::InvalidSecretBase64)?;
    let address = derive_address(signing_key.verifying_key());
    let address_hex = address_lower_hex(address);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_millis(500))
        .timeout(std::time::Duration::from_secs(5))
        .https_only(true)
        .build()
        .map_err(|_| AuthError::InvalidSecretBase64)?;

    // Try derive first, then create. Matches the SDK's fallback logic.
    for (method, path) in [("GET", "/auth/derive-api-key"), ("POST", "/auth/api-key")] {
        let url = format!("{clob_url}{path}");
        let ts = now.to_string();
        let nonce = 0u32;

        let sig = sign_clob_auth(&signing_key, address, &ts, nonce, chain_id)?;
        let sig_hex = format!("0x{}", hex::encode(sig));

        let req = client
            .request(
                if method == "GET" {
                    reqwest::Method::GET
                } else {
                    reqwest::Method::POST
                },
                &url,
            )
            .header("POLY_ADDRESS", &address_hex)
            .header("POLY_NONCE", nonce.to_string())
            .header("POLY_SIGNATURE", &sig_hex)
            .header("POLY_TIMESTAMP", &ts);

        match req.send().await {
            Ok(resp) => {
                let success = resp.status().is_success();
                let status = resp.status().as_u16();
                let body = resp.text().await.unwrap_or_default();
                if success {
                    let creds: DerivedCredentials =
                        serde_json::from_str(&body).map_err(|_| AuthError::InvalidSecretBase64)?;
                    if creds.api_key.is_empty()
                        || creds.secret.is_empty()
                        || creds.passphrase.is_empty()
                    {
                        return Err(AuthError::InvalidSecretBase64);
                    }
                    return Ok((
                        creds.api_key,
                        creds.secret,
                        creds.passphrase,
                        address_hex.clone(),
                    ));
                }
                if (400..500).contains(&status) {
                    continue; // 4xx on derive → try create (SDK fallback)
                }
                continue;
            }
            Err(_) => continue,
        }
    }

    Err(AuthError::InvalidSecretBase64)
}

/// Sign the ClobAuth EIP-712 typed data. Returns the 65-byte Ethereum
/// signature (r || s || v). Pinned to the SDK's L1 auth spec.
fn sign_clob_auth(
    signing_key: &SigningKey,
    address: H160,
    timestamp: &str,
    nonce: u32,
    chain_id: u64,
) -> Result<[u8; 65], AuthError> {
    let typehash = keccak256(CLOB_AUTH_TYPE_STRING.as_bytes());
    let domain_separator = compute_clob_auth_domain(chain_id);
    let struct_hash = clob_auth_struct_hash(typehash, address, timestamp, nonce);

    let mut digest_buf = [0u8; 66];
    digest_buf[0] = 0x19;
    digest_buf[1] = 0x01;
    digest_buf[2..34].copy_from_slice(domain_separator.as_bytes());
    digest_buf[34..66].copy_from_slice(struct_hash.as_bytes());
    let digest = keccak256(&digest_buf);

    let (sig, recovery_id): (EcdsaSignature, RecoveryId) = signing_key
        .sign_prehash_recoverable(digest.as_bytes())
        .map_err(|_| AuthError::InvalidSecretBase64)?;
    let sig = sig.normalize_s().unwrap_or(sig);

    let mut out = [0u8; 65];
    out[..32].copy_from_slice(&sig.r().to_bytes());
    out[32..64].copy_from_slice(&sig.s().to_bytes());
    out[64] = 27 + u8::from(recovery_id);
    Ok(out)
}

fn clob_auth_struct_hash(typehash: H256, address: H160, timestamp: &str, nonce: u32) -> H256 {
    let mut buf: Vec<u8> = Vec::with_capacity(32 * 5);
    buf.extend_from_slice(typehash.as_bytes());
    buf.extend_from_slice(&address_to_uint256_be(address));
    buf.extend_from_slice(keccak256(timestamp.as_bytes()).as_bytes());
    buf.extend_from_slice(&u32_to_uint256_be(nonce));
    buf.extend_from_slice(keccak256(CLOB_AUTH_MESSAGE.as_bytes()).as_bytes());
    keccak256(&buf)
}

fn compute_clob_auth_domain(chain_id: u64) -> H256 {
    let domain_type = "EIP712Domain(string name,string version,uint256 chainId)";
    let typehash = keccak256(domain_type.as_bytes());
    let name_hash = keccak256(CLOB_AUTH_DOMAIN_NAME.as_bytes());
    let version_hash = keccak256(CLOB_AUTH_DOMAIN_VERSION.as_bytes());

    let mut buf: Vec<u8> = Vec::with_capacity(32 * 4);
    buf.extend_from_slice(typehash.as_bytes());
    buf.extend_from_slice(name_hash.as_bytes());
    buf.extend_from_slice(version_hash.as_bytes());
    buf.extend_from_slice(&u64_to_uint256_be(chain_id));
    keccak256(&buf)
}

// ---------------------------------------------------------------------------
// EIP-712 / ABI helpers (shared with signing.rs — keep in sync)
// ---------------------------------------------------------------------------

fn keccak256(input: &[u8]) -> H256 {
    let mut hasher = Keccak256::new();
    hasher.update(input);
    H256::from_slice(&hasher.finalize())
}

fn derive_address(vk: &VerifyingKey) -> H160 {
    let point = vk.to_encoded_point(false);
    let bytes = point.as_bytes();
    debug_assert_eq!(bytes.len(), 65);
    let hash = keccak256(&bytes[1..]);
    H160::from_slice(&hash.as_bytes()[12..])
}

fn address_lower_hex(addr: H160) -> String {
    let mut s = String::with_capacity(42);
    s.push_str("0x");
    s.push_str(&hex::encode(addr.as_bytes()));
    s
}

fn u64_to_uint256_be(v: u64) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[24..].copy_from_slice(&v.to_be_bytes());
    out
}

fn u32_to_uint256_be(v: u32) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[28..].copy_from_slice(&v.to_be_bytes());
    out
}

fn address_to_uint256_be(addr: H160) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[12..].copy_from_slice(addr.as_bytes());
    out
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
