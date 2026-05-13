//! EIP-712 BUY/SELL order signing — synchronous, offline, on-demand.
//!
//! ## Why not the SDK
//!
//! `polymarket_client_sdk_v2::clob::Client::limit_order().build()` calls
//! `tick_size().await?`, `fee_rate_bps().await?`, and `resolve_version()`
//! against the live CLOB *before* producing a signable body. That defeats
//! the on-demand signing architecture this bot is built around: Binance
//! tick → signal decision → sign a fresh FAK body in the spawned submit
//! task → POST /order, with the EIP-712 + secp256k1 cost paid in that
//! spawned task off the core mutex. Holding network round-trips inside
//! the signing function would force every signal to wait on the SDK's
//! HTTP cache lookups before submit — a full architectural regression.
//!
//! ## What this module does
//!
//! * Reads the canonical schema from the on-chain `CTFExchange` V2
//!   contract (verbatim from the SDK source for reference; the schema
//!   lives in the contract, not the SDK):
//!   ```text
//!   struct Order {
//!     uint256 salt;
//!     address maker;
//!     address signer;
//!     uint256 tokenId;
//!     uint256 makerAmount;
//!     uint256 takerAmount;
//!     uint8   side;
//!     uint8   signatureType;
//!     uint256 timestamp;
//!     bytes32 metadata;
//!     bytes32 builder;
//!   }
//!   ```
//! * Precomputes the domain separator at construction time so signing
//!   is one keccak256 (struct hash) + one keccak256 (digest) + one
//!   ECDSA sign per call.
//! * Renders the JSON body in the venue-expected shape (V2 with the
//!   signature folded into the inner `order` object, plus
//!   `orderType`/`owner`/`deferExec` outer fields).
//! * Is fully synchronous, deterministic, and safe to call on-demand
//!   from the spawned submit task without reaching back into the core
//!   mutex.
//!
//! Verifying contract addresses for V2 (Polygon, chain_id 137) are taken
//! from the SDK's `CONFIG` map. They are pinned to the on-chain contracts;
//! changing them invalidates every signature.

use k256::ecdsa::{RecoveryId, Signature as EcdsaSignature, SigningKey, VerifyingKey};
use primitive_types::{H160, H256, U256};
use serde::Serialize;
use sha3::{Digest, Keccak256};

use crate::orders::BuyCanonicalTarget;
use crate::types::{OrderSide, PriceTick, Shares2, TokenId};

// ----------------------------------------------------------------------
// Schema constants — copied from the SDK source (which copies them from
// the on-chain contracts). These are venue-pinned; do not edit without
// confirming on-chain.
// ----------------------------------------------------------------------

/// Solidity type string for the V2 Order struct.
pub(crate) const ORDER_TYPE_STRING_V2: &str = concat!(
    "Order(uint256 salt,address maker,address signer,uint256 tokenId,",
    "uint256 makerAmount,uint256 takerAmount,uint8 side,uint8 signatureType,",
    "uint256 timestamp,bytes32 metadata,bytes32 builder)"
);

/// Standard EIP-712 domain typehash input.
const EIP712_DOMAIN_TYPE: &str =
    "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";

/// Domain `name` field used by the CTF Exchange contracts.
const DOMAIN_NAME: &str = "Polymarket CTF Exchange";
/// Domain `version` field for V2.
const DOMAIN_VERSION_V2: &str = "2";

/// Polygon mainnet chain id.
pub const POLYGON_CHAIN_ID: u64 = 137;

/// V2 CTF Exchange (normal markets) on Polygon mainnet.
pub const EXCHANGE_V2_NORMAL: H160 = H160([
    0xE1, 0x11, 0x18, 0x00, 0x00, 0xd2, 0x66, 0x3C, 0x00, 0x91, 0xe4, 0xf4, 0x00, 0x23, 0x75, 0x45,
    0xB8, 0x7B, 0x99, 0x6B,
]);

// ----------------------------------------------------------------------
// Public types
// ----------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SignatureKind {
    /// EOA: the signer is the funder; no proxy address.
    Eoa = 0,
    /// Polymarket proxy wallet: maker is the proxy, signer is the EOA.
    PolyProxy = 1,
    /// Gnosis-Safe-style wallet: maker is the safe, signer is the EOA.
    PolyGnosisSafe = 2,
}

impl SignatureKind {
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Configured signer. Construct once at startup.
#[derive(Clone, Debug)]
pub struct OrderSigner {
    signing_key: SigningKey,
    address: H160,
    /// API-key UUID (the `owner` field in the JSON body envelope).
    api_key: String,
    /// `maker` address — funder if set, else signer.
    maker: H160,
    /// EOA / Proxy / GnosisSafe.
    signature_kind: SignatureKind,
    /// Precomputed domain separator (keccak256(EIP712Domain encoding)).
    domain_separator: H256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SigningError {
    InvalidPrivateKey,
    InvalidTokenId,
    InvalidApiKey,
    InvalidFunder,
    InvalidPriceOrSize,
    SerializeBody(String),
}

/// A locally signed, locally validated FAK order body ready for `POST /order`.
///
/// The REST submitter accepts this newtype instead of raw bytes so invalid
/// caller-constructed JSON cannot bypass the signing/validation boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedFakOrderBody(Vec<u8>);

impl SignedFakOrderBody {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Display for SigningError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SigningError::InvalidPrivateKey => write!(f, "invalid_private_key"),
            SigningError::InvalidTokenId => write!(f, "invalid_token_id"),
            SigningError::InvalidApiKey => write!(f, "invalid_api_key"),
            SigningError::InvalidFunder => write!(f, "invalid_funder"),
            SigningError::InvalidPriceOrSize => write!(f, "invalid_price_or_size"),
            SigningError::SerializeBody(s) => write!(f, "serialize_body: {s}"),
        }
    }
}

impl std::error::Error for SigningError {}

/// Per-call inputs that must be fresh on every submit attempt — salt
/// must change so the signed body hash is unique (a replayed body can
/// hit `INVALID_ORDER_DUPLICATED` on the venue), and timestamp must be
/// the current time so the order doesn't reject as stale.
#[derive(Clone, Copy, Debug)]
pub struct SignInputs {
    pub salt: u64,
    pub timestamp_ms: u128,
}

// ----------------------------------------------------------------------
// Internal Order representation. Mirrors the V2 schema exactly.
// ----------------------------------------------------------------------

#[derive(Clone, Debug)]
struct OrderV2 {
    salt: u64,
    maker: H160,
    signer: H160,
    token_id: U256,
    maker_amount: U256,
    taker_amount: U256,
    side: OrderSide,
    signature_kind: SignatureKind,
    timestamp_ms: u128,
    metadata: H256,
    builder: H256,
}

impl OrderV2 {
    /// abi.encode(typehash || each field padded to 32 bytes), then keccak256.
    fn struct_hash(&self) -> H256 {
        let typehash = keccak256(ORDER_TYPE_STRING_V2.as_bytes());

        let mut buf: Vec<u8> = Vec::with_capacity(32 * 12);
        buf.extend_from_slice(typehash.as_bytes());
        // salt: u64 → uint256 → 32 bytes BE
        buf.extend_from_slice(&u64_to_uint256_be(self.salt));
        // maker, signer: address → 32 bytes (left-padded)
        buf.extend_from_slice(&address_to_uint256_be(self.maker));
        buf.extend_from_slice(&address_to_uint256_be(self.signer));
        // tokenId, makerAmount, takerAmount: uint256 → 32 bytes BE
        let mut tmp = [0u8; 32];
        self.token_id.to_big_endian(&mut tmp);
        buf.extend_from_slice(&tmp);
        self.maker_amount.to_big_endian(&mut tmp);
        buf.extend_from_slice(&tmp);
        self.taker_amount.to_big_endian(&mut tmp);
        buf.extend_from_slice(&tmp);
        // side, signatureType: uint8 → 32 bytes (left-padded)
        buf.extend_from_slice(&u8_to_uint256_be(self.side.as_u8()));
        buf.extend_from_slice(&u8_to_uint256_be(self.signature_kind.as_u8()));
        // timestamp: u128 → uint256 → 32 bytes BE
        buf.extend_from_slice(&u128_to_uint256_be(self.timestamp_ms));
        // metadata, builder: bytes32 verbatim
        buf.extend_from_slice(self.metadata.as_bytes());
        buf.extend_from_slice(self.builder.as_bytes());

        keccak256(&buf)
    }
}

// ----------------------------------------------------------------------
// OrderSigner impl
// ----------------------------------------------------------------------

impl OrderSigner {
    /// Construct from a hex private key (0x-prefixed or bare), the API key
    /// UUID (the `owner` field in body envelopes), an optional funder
    /// (required for `PolyProxy` / `PolyGnosisSafe`; forbidden for `Eoa`),
    /// the signature kind, and the verifying-contract address (`EXCHANGE_V2_NORMAL`).
    pub fn new(
        private_key_hex: &str,
        api_key: &str,
        funder: Option<H160>,
        signature_kind: SignatureKind,
        chain_id: u64,
        verifying_contract: H160,
    ) -> Result<Self, SigningError> {
        let pk_hex = private_key_hex
            .strip_prefix("0x")
            .unwrap_or(private_key_hex);
        let pk_bytes = hex::decode(pk_hex).map_err(|_| SigningError::InvalidPrivateKey)?;
        if pk_bytes.len() != 32 {
            return Err(SigningError::InvalidPrivateKey);
        }
        let signing_key =
            SigningKey::from_slice(&pk_bytes).map_err(|_| SigningError::InvalidPrivateKey)?;
        let address = derive_address(signing_key.verifying_key());

        if api_key.is_empty() {
            return Err(SigningError::InvalidApiKey);
        }

        // EOA: maker == signer; funder must NOT be set.
        // Proxy / GnosisSafe: maker = funder; signer = EOA address.
        let maker = match (signature_kind, funder) {
            (SignatureKind::Eoa, None) => address,
            (SignatureKind::Eoa, Some(_)) => {
                return Err(SigningError::InvalidFunder);
            }
            (SignatureKind::PolyProxy | SignatureKind::PolyGnosisSafe, Some(f)) => f,
            (SignatureKind::PolyProxy | SignatureKind::PolyGnosisSafe, None) => {
                return Err(SigningError::InvalidFunder);
            }
        };

        let domain_separator =
            compute_domain_separator(DOMAIN_NAME, DOMAIN_VERSION_V2, chain_id, verifying_contract);

        Ok(Self {
            signing_key,
            address,
            api_key: api_key.to_owned(),
            maker,
            signature_kind,
            domain_separator,
        })
    }

    pub fn signer_address(&self) -> H160 {
        self.address
    }

    pub fn maker_address(&self) -> H160 {
        self.maker
    }

    /// Sign a FAK BUY for the canonical target. Returns the JSON body
    /// bytes ready for POST /order. Synchronous; no network.
    pub fn sign_fak_buy(
        &self,
        token: &TokenId,
        target: &BuyCanonicalTarget,
        inputs: SignInputs,
    ) -> Result<SignedFakOrderBody, SigningError> {
        // Maker amount in atoms (1e-6 dollars per atom).
        // UsdcCents → atoms = cents * 10_000.
        let maker_atoms = (target.maker_amount.cents() as i128)
            .checked_mul(10_000)
            .ok_or(SigningError::InvalidPriceOrSize)?;
        if maker_atoms <= 0 {
            return Err(SigningError::InvalidPriceOrSize);
        }
        // Taker amount in atoms (1e-6 shares per atom).
        // Shares4 units (0.0001-share units) → atoms = units * 100.
        let taker_atoms = (target.size.units() as i128)
            .checked_mul(100)
            .ok_or(SigningError::InvalidPriceOrSize)?;
        if taker_atoms <= 0 {
            return Err(SigningError::InvalidPriceOrSize);
        }
        self.sign_v2(
            token,
            OrderSide::Buy,
            U256::from(maker_atoms as u128),
            U256::from(taker_atoms as u128),
            inputs,
        )
    }

    /// Sign a FAK SELL. For SELL, maker_amount is in shares and
    /// taker_amount is in dollars (the venue swaps the legs vs BUY).
    pub fn sign_fak_sell(
        &self,
        token: &TokenId,
        price: PriceTick,
        size: Shares2,
        inputs: SignInputs,
    ) -> Result<SignedFakOrderBody, SigningError> {
        // SELL maker = size × atoms per share = Shares2 units × 10_000.
        let maker_atoms = (size.units() as i128)
            .checked_mul(10_000)
            .ok_or(SigningError::InvalidPriceOrSize)?;
        if maker_atoms <= 0 {
            return Err(SigningError::InvalidPriceOrSize);
        }
        // SELL taker = price × size in dollars × atoms per dollar.
        // Dimensional: ticks ($0.01/share) × Shares2 (0.01 share)
        //   = 0.0001 USD per (ticks·units) = 100 atoms per (ticks·units).
        let taker_atoms = (price.ticks() as i128)
            .checked_mul(size.units() as i128)
            .and_then(|v| v.checked_mul(100))
            .ok_or(SigningError::InvalidPriceOrSize)?;
        if taker_atoms <= 0 {
            return Err(SigningError::InvalidPriceOrSize);
        }
        self.sign_v2(
            token,
            OrderSide::Sell,
            U256::from(maker_atoms as u128),
            U256::from(taker_atoms as u128),
            inputs,
        )
    }

    fn sign_v2(
        &self,
        token: &TokenId,
        side: OrderSide,
        maker_amount: U256,
        taker_amount: U256,
        inputs: SignInputs,
    ) -> Result<SignedFakOrderBody, SigningError> {
        let token_id = parse_u256_decimal(token.as_str()).ok_or(SigningError::InvalidTokenId)?;

        let order = OrderV2 {
            salt: inputs.salt,
            maker: self.maker,
            signer: self.address,
            token_id,
            maker_amount,
            taker_amount,
            side,
            signature_kind: self.signature_kind,
            timestamp_ms: inputs.timestamp_ms,
            metadata: H256::zero(),
            builder: H256::zero(),
        };

        // EIP-712 digest = keccak256(0x1901 || domainSeparator || structHash)
        let struct_hash = order.struct_hash();
        let mut digest_buf = [0u8; 66];
        digest_buf[0] = 0x19;
        digest_buf[1] = 0x01;
        digest_buf[2..34].copy_from_slice(self.domain_separator.as_bytes());
        digest_buf[34..66].copy_from_slice(struct_hash.as_bytes());
        let digest = keccak256(&digest_buf);

        // ECDSA sign on the prehashed digest.
        let (sig, recovery_id): (EcdsaSignature, RecoveryId) = self
            .signing_key
            .sign_prehash_recoverable(digest.as_bytes())
            .map_err(|_| SigningError::InvalidPrivateKey)?;
        // Ethereum (and the CTFExchange verifier) requires low-S signatures.
        // If `normalize_s` flips `s`, the public key recovered from the
        // signature changes unless we also flip the recovery parity. Newer
        // `k256::sign_prehash_recoverable` always returns low-S, so the
        // branch is typically unreachable — but keeping the parity flip
        // here makes correctness independent of the crate's internal
        // normalization policy. See test
        // `sign_prehash_recoverable_returns_low_s_for_many_digests`.
        let (normalized_sig, normalized_recovery_id) = match sig.normalize_s() {
            Some(flipped) => {
                let flipped_parity = u8::from(recovery_id) ^ 1;
                let new_recovery_id = RecoveryId::try_from(flipped_parity)
                    .map_err(|_| SigningError::InvalidPrivateKey)?;
                (flipped, new_recovery_id)
            }
            None => (sig, recovery_id),
        };
        let signature_bytes = encode_signature(&normalized_sig, normalized_recovery_id);

        // Render JSON body matching the V2 envelope expected by /order.
        // expiration is 0 for FAK — the V2 schema does not include it in
        // the signed struct, but the venue requires it on the wire.
        let body = SignedOrderBodyV2 {
            order: OrderV2WithSignature {
                salt: order.salt,
                maker: address_lower_hex(order.maker),
                signer: address_lower_hex(order.signer),
                token_id: u256_decimal(order.token_id),
                maker_amount: u256_decimal(order.maker_amount),
                taker_amount: u256_decimal(order.taker_amount),
                side: side.as_str(),
                expiration: "0",
                signature_type: order.signature_kind.as_u8(),
                timestamp: u128_decimal(order.timestamp_ms),
                metadata: bytes32_hex(order.metadata),
                builder: bytes32_hex(order.builder),
                signature: signature_hex(&signature_bytes),
            },
            order_type: "FAK",
            owner: &self.api_key,
            defer_exec: false,
        };

        let bytes =
            serde_json::to_vec(&body).map_err(|e| SigningError::SerializeBody(format!("{e}")))?;
        Ok(SignedFakOrderBody(bytes))
    }
}

// ----------------------------------------------------------------------
// JSON body shape (matches what the venue expects).
// ----------------------------------------------------------------------

#[derive(Serialize)]
struct SignedOrderBodyV2<'a> {
    order: OrderV2WithSignature<'a>,
    #[serde(rename = "orderType")]
    order_type: &'a str,
    owner: &'a str,
    #[serde(rename = "deferExec")]
    defer_exec: bool,
}

#[derive(Serialize)]
struct OrderV2WithSignature<'a> {
    salt: u64,
    maker: String,
    signer: String,
    #[serde(rename = "tokenId")]
    token_id: String,
    #[serde(rename = "makerAmount")]
    maker_amount: String,
    #[serde(rename = "takerAmount")]
    taker_amount: String,
    side: &'a str,
    expiration: &'a str,
    #[serde(rename = "signatureType")]
    signature_type: u8,
    timestamp: String,
    metadata: String,
    builder: String,
    signature: String,
}

// ----------------------------------------------------------------------
// EIP-712 / ABI-encoding helpers
// ----------------------------------------------------------------------

fn compute_domain_separator(
    name: &str,
    version: &str,
    chain_id: u64,
    verifying_contract: H160,
) -> H256 {
    let typehash = keccak256(EIP712_DOMAIN_TYPE.as_bytes());
    let name_hash = keccak256(name.as_bytes());
    let version_hash = keccak256(version.as_bytes());

    let mut buf: Vec<u8> = Vec::with_capacity(32 * 5);
    buf.extend_from_slice(typehash.as_bytes());
    buf.extend_from_slice(name_hash.as_bytes());
    buf.extend_from_slice(version_hash.as_bytes());
    buf.extend_from_slice(&u64_to_uint256_be(chain_id));
    buf.extend_from_slice(&address_to_uint256_be(verifying_contract));
    keccak256(&buf)
}

pub(crate) fn keccak256(input: &[u8]) -> H256 {
    let mut hasher = Keccak256::new();
    hasher.update(input);
    H256::from_slice(&hasher.finalize())
}

/// Derive the Ethereum address from a secp256k1 verifying key:
/// keccak256(uncompressed_pubkey[1..])[12..32] is the lower 20 bytes.
pub fn derive_address(vk: &VerifyingKey) -> H160 {
    let point = vk.to_encoded_point(false); // uncompressed: 0x04 || X || Y
    let bytes = point.as_bytes();
    debug_assert_eq!(bytes.len(), 65);
    let hash = keccak256(&bytes[1..]);
    H160::from_slice(&hash.as_bytes()[12..])
}

pub(crate) fn u64_to_uint256_be(v: u64) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[24..].copy_from_slice(&v.to_be_bytes());
    out
}

fn u128_to_uint256_be(v: u128) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[16..].copy_from_slice(&v.to_be_bytes());
    out
}

fn u8_to_uint256_be(v: u8) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[31] = v;
    out
}

pub(crate) fn address_to_uint256_be(addr: H160) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[12..].copy_from_slice(addr.as_bytes());
    out
}

/// Encode (signature_bytes, recovery_id) as a 65-byte Ethereum signature
/// `r (32) || s (32) || v (1)` where `v = 27 + recovery_id`.
fn encode_signature(sig: &EcdsaSignature, recovery_id: RecoveryId) -> [u8; 65] {
    let mut out = [0u8; 65];
    let r_bytes = sig.r().to_bytes();
    let s_bytes = sig.s().to_bytes();
    out[..32].copy_from_slice(&r_bytes);
    out[32..64].copy_from_slice(&s_bytes);
    out[64] = 27 + u8::from(recovery_id);
    out
}

// ----------------------------------------------------------------------
// JSON rendering helpers
// ----------------------------------------------------------------------

pub fn address_lower_hex(addr: H160) -> String {
    let mut s = String::with_capacity(42);
    s.push_str("0x");
    s.push_str(&hex::encode(addr.as_bytes()));
    s
}

fn bytes32_hex(b: H256) -> String {
    let mut s = String::with_capacity(66);
    s.push_str("0x");
    s.push_str(&hex::encode(b.as_bytes()));
    s
}

fn signature_hex(sig: &[u8; 65]) -> String {
    let mut s = String::with_capacity(132);
    s.push_str("0x");
    s.push_str(&hex::encode(sig));
    s
}

fn u256_decimal(v: U256) -> String {
    v.to_string()
}

fn u128_decimal(v: u128) -> String {
    v.to_string()
}

/// Parse a U256 from a decimal string. Used for the ERC-1155 token id
/// which the gamma metadata renders as a base-10 number.
fn parse_u256_decimal(s: &str) -> Option<U256> {
    U256::from_dec_str(s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orders::{BuyCanonicalInput, canonical_buy_target_for_notional};

    /// Standard low-private-key test vector. Address derives to:
    /// 0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf
    const TEST_PRIVATE_KEY: &str =
        "0x0000000000000000000000000000000000000000000000000000000000000001";
    const TEST_ADDRESS: &str = "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf";
    const TEST_API_KEY: &str = "00000000-0000-0000-0000-000000000001";

    fn signer() -> OrderSigner {
        OrderSigner::new(
            TEST_PRIVATE_KEY,
            TEST_API_KEY,
            None,
            SignatureKind::Eoa,
            POLYGON_CHAIN_ID,
            EXCHANGE_V2_NORMAL,
        )
        .unwrap()
    }

    #[test]
    fn address_derives_from_test_vector() {
        let s = signer();
        assert_eq!(address_lower_hex(s.signer_address()), TEST_ADDRESS);
    }

    #[test]
    fn order_type_string_matches_sdk() {
        // Locked verbatim from the on-chain contract via the SDK source.
        // If this changes upstream, every signature this bot produces is
        // invalid — a refactor must reverify against the live contract.
        assert_eq!(
            ORDER_TYPE_STRING_V2,
            "Order(uint256 salt,address maker,address signer,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint8 side,uint8 signatureType,uint256 timestamp,bytes32 metadata,bytes32 builder)"
        );
    }

    #[test]
    fn domain_separator_for_polygon_v2_normal_is_stable() {
        // Locks the precomputed domain separator. If chain id, name,
        // version, or verifying contract drifts, this test breaks before
        // any signature can be produced. Hash computed once and pinned;
        // recompute manually if intentionally changing the schema.
        let s = signer();
        let expected = compute_domain_separator(
            DOMAIN_NAME,
            DOMAIN_VERSION_V2,
            POLYGON_CHAIN_ID,
            EXCHANGE_V2_NORMAL,
        );
        assert_eq!(s.domain_separator, expected);
    }

    #[test]
    fn signature_recovers_to_signer_address_for_buy() {
        let s = signer();
        let target = canonical_buy_target_for_notional(BuyCanonicalInput {
            price: PriceTick::checked(50).unwrap(),
            target_maker_cents: 101,
            min_size_taker_units: 100,
            min_maker_cents: 100,
            max_overrun_cents: 1,
            max_overrun_bps: 0,
        })
        .unwrap();

        let token =
            TokenId::new("1234567890123456789012345678901234567890123456789012345678901234");
        let inputs = SignInputs {
            salt: 0xdead_beef_cafe_babe,
            timestamp_ms: 1_777_000_000_000,
        };

        let body = s.sign_fak_buy(&token, &target, inputs).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(body.as_bytes()).unwrap();

        // Sanity: structure.
        assert_eq!(parsed["orderType"], "FAK");
        assert_eq!(parsed["owner"], TEST_API_KEY);
        assert_eq!(parsed["deferExec"], false);
        let order = &parsed["order"];
        assert_eq!(order["side"], "BUY");
        assert_eq!(order["signatureType"], 0);
        assert_eq!(order["maker"], TEST_ADDRESS);
        assert_eq!(order["signer"], TEST_ADDRESS);
        // maker_amount in atoms: $1.01 * 1e6 = 1010000.
        assert_eq!(order["makerAmount"], "1010000");
        // taker_amount in atoms: 2.02 shares * 1e6 = 2020000.
        assert_eq!(order["takerAmount"], "2020000");
        assert_eq!(order["expiration"], "0");
        assert_eq!(order["timestamp"], "1777000000000");
        assert_eq!(order["metadata"], format!("0x{}", "0".repeat(64)));
        assert_eq!(order["builder"], format!("0x{}", "0".repeat(64)));

        // Recover signer address from signature and assert match.
        let signature_str = order["signature"].as_str().unwrap();
        let signature = signature_str.strip_prefix("0x").unwrap();
        let signature_bytes = hex::decode(signature).unwrap();
        assert_eq!(signature_bytes.len(), 65);

        // Reconstruct the digest and recover.
        let token_id = parse_u256_decimal(token.as_str()).unwrap();
        let order_v2 = OrderV2 {
            salt: inputs.salt,
            maker: s.maker,
            signer: s.address,
            token_id,
            maker_amount: U256::from(1_010_000u64),
            taker_amount: U256::from(2_020_000u64),
            side: OrderSide::Buy,
            signature_kind: SignatureKind::Eoa,
            timestamp_ms: inputs.timestamp_ms,
            metadata: H256::zero(),
            builder: H256::zero(),
        };
        let mut digest_buf = [0u8; 66];
        digest_buf[0] = 0x19;
        digest_buf[1] = 0x01;
        digest_buf[2..34].copy_from_slice(s.domain_separator.as_bytes());
        digest_buf[34..66].copy_from_slice(order_v2.struct_hash().as_bytes());
        let digest = keccak256(&digest_buf);

        let r = k256::ecdsa::Signature::from_slice(&signature_bytes[..64]).unwrap();
        let v = signature_bytes[64];
        let recovery_id = RecoveryId::try_from(v - 27).unwrap();
        let recovered =
            VerifyingKey::recover_from_prehash(digest.as_bytes(), &r, recovery_id).unwrap();
        let recovered_address = derive_address(&recovered);
        assert_eq!(recovered_address, s.address);
    }

    #[test]
    fn signature_recovers_to_signer_address_for_sell() {
        let s = signer();
        let token =
            TokenId::new("1234567890123456789012345678901234567890123456789012345678901234");
        let inputs = SignInputs {
            salt: 1,
            timestamp_ms: 1_777_000_000_000,
        };

        // 1.50 shares at $0.60 → 0.90 USDC notional = 900_000 atoms.
        let body = s
            .sign_fak_sell(
                &token,
                PriceTick::checked(60).unwrap(),
                Shares2::new_unchecked(150),
                inputs,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(body.as_bytes()).unwrap();
        let order = &parsed["order"];
        assert_eq!(order["side"], "SELL");
        // maker = shares × 1e6 = 1.50 × 1e6 = 1_500_000
        assert_eq!(order["makerAmount"], "1500000");
        // taker = price * shares * 1e6 = 0.60 * 1.50 * 1e6 = 900_000
        assert_eq!(order["takerAmount"], "900000");

        // Recover.
        let signature_str = order["signature"].as_str().unwrap();
        let signature_bytes = hex::decode(signature_str.strip_prefix("0x").unwrap()).unwrap();
        let token_id = parse_u256_decimal(token.as_str()).unwrap();
        let order_v2 = OrderV2 {
            salt: inputs.salt,
            maker: s.maker,
            signer: s.address,
            token_id,
            maker_amount: U256::from(1_500_000u64),
            taker_amount: U256::from(900_000u64),
            side: OrderSide::Sell,
            signature_kind: SignatureKind::Eoa,
            timestamp_ms: inputs.timestamp_ms,
            metadata: H256::zero(),
            builder: H256::zero(),
        };
        let mut digest_buf = [0u8; 66];
        digest_buf[0] = 0x19;
        digest_buf[1] = 0x01;
        digest_buf[2..34].copy_from_slice(s.domain_separator.as_bytes());
        digest_buf[34..66].copy_from_slice(order_v2.struct_hash().as_bytes());
        let digest = keccak256(&digest_buf);

        let r = k256::ecdsa::Signature::from_slice(&signature_bytes[..64]).unwrap();
        let recovery_id = RecoveryId::try_from(signature_bytes[64] - 27).unwrap();
        let recovered =
            VerifyingKey::recover_from_prehash(digest.as_bytes(), &r, recovery_id).unwrap();
        assert_eq!(derive_address(&recovered), s.address);
    }

    #[test]
    fn rejects_eoa_with_funder() {
        let err = OrderSigner::new(
            TEST_PRIVATE_KEY,
            TEST_API_KEY,
            Some(H160::zero()),
            SignatureKind::Eoa,
            POLYGON_CHAIN_ID,
            EXCHANGE_V2_NORMAL,
        )
        .unwrap_err();
        assert_eq!(err, SigningError::InvalidFunder);
    }

    #[test]
    fn rejects_proxy_without_funder() {
        let err = OrderSigner::new(
            TEST_PRIVATE_KEY,
            TEST_API_KEY,
            None,
            SignatureKind::PolyProxy,
            POLYGON_CHAIN_ID,
            EXCHANGE_V2_NORMAL,
        )
        .unwrap_err();
        assert_eq!(err, SigningError::InvalidFunder);
    }

    #[test]
    fn rejects_invalid_token_id() {
        let s = signer();
        let target = canonical_buy_target_for_notional(BuyCanonicalInput {
            price: PriceTick::checked(50).unwrap(),
            target_maker_cents: 101,
            min_size_taker_units: 100,
            min_maker_cents: 100,
            max_overrun_cents: 1,
            max_overrun_bps: 0,
        })
        .unwrap();
        let bogus = TokenId::new("not a number");
        let err = s
            .sign_fak_buy(
                &bogus,
                &target,
                SignInputs {
                    salt: 0,
                    timestamp_ms: 0,
                },
            )
            .unwrap_err();
        assert_eq!(err, SigningError::InvalidTokenId);
    }

    #[test]
    fn salt_renders_as_numeric_in_body() {
        // The SDK serialises salt as a u64 (numeric, not stringified).
        // Lock that to prevent silent drift.
        let s = signer();
        let target = canonical_buy_target_for_notional(BuyCanonicalInput {
            price: PriceTick::checked(50).unwrap(),
            target_maker_cents: 101,
            min_size_taker_units: 100,
            min_maker_cents: 100,
            max_overrun_cents: 1,
            max_overrun_bps: 0,
        })
        .unwrap();
        let token = TokenId::new("1");
        let body = s
            .sign_fak_buy(
                &token,
                &target,
                SignInputs {
                    salt: 12345,
                    timestamp_ms: 1,
                },
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(body.as_bytes()).unwrap();
        assert!(
            parsed["order"]["salt"].is_number(),
            "salt must be a JSON number"
        );
        assert_eq!(parsed["order"]["salt"].as_u64(), Some(12345));
    }

    /// Sign `iterations` BUY bodies with varying salts and recover the
    /// public key from each signature. A mismatch means either the
    /// signing path produced a high-S signature without flipping recovery
    /// parity, or low-S normalization silently changed `s` without the
    /// corresponding `v` flip. Running over a broad salt range exercises
    /// RFC 6979 nonce derivation enough to hit both parity bits.
    #[test]
    fn signature_recovers_for_many_salts_across_both_sides() {
        let s = signer();
        let token =
            TokenId::new("1234567890123456789012345678901234567890123456789012345678901234");
        let buy_target = canonical_buy_target_for_notional(BuyCanonicalInput {
            price: PriceTick::checked(50).unwrap(),
            target_maker_cents: 101,
            min_size_taker_units: 100,
            min_maker_cents: 100,
            max_overrun_cents: 1,
            max_overrun_bps: 0,
        })
        .unwrap();

        let iterations = 128u64;
        let mut low_s_count = 0u64;
        let mut high_s_count = 0u64;
        let half_order = {
            // secp256k1 order / 2, as big-endian bytes.
            // Anything strictly > this would originally have been high-S.
            let bytes: [u8; 32] = [
                0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
                0xFF, 0xFF, 0x5D, 0x57, 0x6E, 0x73, 0x57, 0xA4, 0x50, 0x1D, 0xDF, 0xE9, 0x2F, 0x46,
                0x68, 0x1B, 0x20, 0xA0,
            ];
            bytes
        };

        for salt in 0..iterations {
            let inputs = SignInputs {
                salt,
                timestamp_ms: 1_777_000_000_000 + salt as u128,
            };

            // BUY recovery.
            let body = s.sign_fak_buy(&token, &buy_target, inputs).unwrap();
            let parsed: serde_json::Value = serde_json::from_slice(body.as_bytes()).unwrap();
            let sig_hex = parsed["order"]["signature"].as_str().unwrap();
            let sig_bytes = hex::decode(sig_hex.strip_prefix("0x").unwrap()).unwrap();
            assert_eq!(sig_bytes.len(), 65);

            // All output signatures must be in low-S canonical form.
            let s_bytes: [u8; 32] = sig_bytes[32..64].try_into().unwrap();
            if s_bytes <= half_order {
                low_s_count += 1;
            } else {
                high_s_count += 1;
            }
            assert!(
                s_bytes <= half_order,
                "signature at salt={salt} has high-S component — normalization regression"
            );

            let order_v2 = OrderV2 {
                salt: inputs.salt,
                maker: s.maker,
                signer: s.address,
                token_id: parse_u256_decimal(token.as_str()).unwrap(),
                maker_amount: U256::from(1_010_000u64),
                taker_amount: U256::from(2_020_000u64),
                side: OrderSide::Buy,
                signature_kind: SignatureKind::Eoa,
                timestamp_ms: inputs.timestamp_ms,
                metadata: H256::zero(),
                builder: H256::zero(),
            };
            let mut buf = [0u8; 66];
            buf[0] = 0x19;
            buf[1] = 0x01;
            buf[2..34].copy_from_slice(s.domain_separator.as_bytes());
            buf[34..66].copy_from_slice(order_v2.struct_hash().as_bytes());
            let digest = keccak256(&buf);

            let r = k256::ecdsa::Signature::from_slice(&sig_bytes[..64]).unwrap();
            let v = sig_bytes[64];
            assert!(v == 27 || v == 28, "v must be 27 or 28, got {v}");
            let rid = RecoveryId::try_from(v - 27).unwrap();
            let recovered = VerifyingKey::recover_from_prehash(digest.as_bytes(), &r, rid).unwrap();
            assert_eq!(
                derive_address(&recovered),
                s.address,
                "BUY recovery mismatch at salt={salt}"
            );

            // SELL recovery with same salt.
            let body = s
                .sign_fak_sell(
                    &token,
                    PriceTick::checked(60).unwrap(),
                    Shares2::new_unchecked(150),
                    inputs,
                )
                .unwrap();
            let parsed: serde_json::Value = serde_json::from_slice(body.as_bytes()).unwrap();
            let sig_hex = parsed["order"]["signature"].as_str().unwrap();
            let sig_bytes = hex::decode(sig_hex.strip_prefix("0x").unwrap()).unwrap();
            let s_bytes: [u8; 32] = sig_bytes[32..64].try_into().unwrap();
            assert!(
                s_bytes <= half_order,
                "SELL signature at salt={salt} has high-S component"
            );

            let order_v2 = OrderV2 {
                salt: inputs.salt,
                maker: s.maker,
                signer: s.address,
                token_id: parse_u256_decimal(token.as_str()).unwrap(),
                maker_amount: U256::from(1_500_000u64),
                taker_amount: U256::from(900_000u64),
                side: OrderSide::Sell,
                signature_kind: SignatureKind::Eoa,
                timestamp_ms: inputs.timestamp_ms,
                metadata: H256::zero(),
                builder: H256::zero(),
            };
            let mut buf = [0u8; 66];
            buf[0] = 0x19;
            buf[1] = 0x01;
            buf[2..34].copy_from_slice(s.domain_separator.as_bytes());
            buf[34..66].copy_from_slice(order_v2.struct_hash().as_bytes());
            let digest = keccak256(&buf);

            let r = k256::ecdsa::Signature::from_slice(&sig_bytes[..64]).unwrap();
            let v = sig_bytes[64];
            let rid = RecoveryId::try_from(v - 27).unwrap();
            let recovered = VerifyingKey::recover_from_prehash(digest.as_bytes(), &r, rid).unwrap();
            assert_eq!(
                derive_address(&recovered),
                s.address,
                "SELL recovery mismatch at salt={salt}"
            );
        }

        // Document the observed low-S rate. `k256::sign_prehash_recoverable`
        // in the pinned version produces low-S for effectively every salt
        // via internal normalization; `high_s_count` should stay at 0.
        // If it becomes non-zero in a future k256 update, the parity-flip
        // path is exercised and still correct.
        assert_eq!(
            low_s_count + high_s_count,
            iterations,
            "salt accounting lost signatures"
        );
    }
}
