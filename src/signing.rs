//! EIP-712 BUY/SELL order signing via the official Polymarket Rust SDK.
//!
//! Per `docs/RUST_SOTA_ARCHITECTURE_REFACTOR_PLAN.md` we do **not**
//! re-implement the EIP-712 schema or the signature recovery path. The
//! schema lives in the on-chain `CTFExchange` contract and is canonically
//! exposed by the official `polymarket_client_sdk_v2` crate. Replicating
//! it would be a monkey job and would silently drift if Polymarket ever
//! revs the contract.
//!
//! What this module owns:
//!
//! 1. Conversion from our fixed-point types (`PriceTick`, `Shares4`,
//!    `Shares2`) into the SDK's `Decimal` price/size at the call boundary.
//! 2. Construction of FAK BUY / FAK SELL orders for our policy.
//! 3. Signing via a `LocalSigner` we create once at startup.
//! 4. Serialisation of the `SignedOrder` to JSON bytes for direct HTTP
//!    submit. We hand the bytes to our own pooled HTTP client + L2 auth
//!    layer (Phase 4) instead of letting the SDK's `post_order` own the
//!    request.
//!
//! ### Network dependency
//!
//! The SDK's order builder calls `tick_size(token_id)`, `fee_rate_bps`,
//! and `resolve_version()` against the live CLOB before producing a
//! `SignableOrder`. Our `OrderSigner` therefore needs a reachable CLOB
//! at signing time. Tests that exercise the actual signing path are
//! marked `#[ignore]` and run manually via:
//!
//! ```bash
//! POLYMARKET_PRIVATE_KEY=0x... cargo test signing -- --ignored
//! ```
//!
//! Phase 4 (HTTP submit) and Phase 8 (shadow mode) will exercise this
//! path against a real endpoint as part of normal CI.

use std::str::FromStr;

use alloy::signers::k256::ecdsa::SigningKey;
use polymarket_client_sdk_v2::auth::state::Authenticated;
use polymarket_client_sdk_v2::auth::Credentials;
use polymarket_client_sdk_v2::auth::LocalSigner;
use polymarket_client_sdk_v2::auth::Normal;
use polymarket_client_sdk_v2::auth::Signer;
use polymarket_client_sdk_v2::clob::types::{Side, SignatureType};
use polymarket_client_sdk_v2::clob::{Client, Config};
use polymarket_client_sdk_v2::types::{Address, Decimal, U256};
use polymarket_client_sdk_v2::POLYGON;

use crate::orders::BuyCanonicalTarget;
use crate::types::{OrderSide, PriceTick, Shares2, TokenId};

/// Configured signer. Construct once at startup; clones share the underlying
/// alloy signer (which holds an `Arc<SigningKey>` internally).
pub struct OrderSigner {
    inner: LocalSigner<SigningKey>,
    /// Authenticated SDK client. Required because the SDK's
    /// `OrderBuilder::build` is gated on `Client<Authenticated<Normal>>`
    /// and uses the client's caches for tick size / fee rate / protocol
    /// version when constructing the order payload.
    client: Client<Authenticated<Normal>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SigningError {
    InvalidPrivateKey,
    InvalidTokenId,
    InvalidPriceOrSize,
    SdkError(String),
    Serialize(String),
}

impl std::fmt::Display for SigningError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SigningError::InvalidPrivateKey => write!(f, "invalid_private_key"),
            SigningError::InvalidTokenId => write!(f, "invalid_token_id"),
            SigningError::InvalidPriceOrSize => write!(f, "invalid_price_or_size"),
            SigningError::SdkError(e) => write!(f, "sdk_error: {e}"),
            SigningError::Serialize(e) => write!(f, "serialize: {e}"),
        }
    }
}

impl std::error::Error for SigningError {}

impl OrderSigner {
    /// Construct from a hex private key, an optional funder address (for
    /// proxy-wallet signature types), and an authenticated CLOB client.
    ///
    /// If `funder` is `None` and `signature_type` is a proxy variant, the
    /// SDK derives the proxy wallet via CREATE2 from the signer address.
    /// For EOA signing, `funder` must be `None`.
    pub async fn new(
        private_key_hex: &str,
        clob_host: &str,
        credentials: Credentials,
        funder: Option<Address>,
        signature_type: SignatureType,
    ) -> Result<Self, SigningError> {
        let signer = LocalSigner::from_str(private_key_hex)
            .map_err(|_| SigningError::InvalidPrivateKey)?
            .with_chain_id(Some(POLYGON));

        let unauth = Client::new(clob_host, Config::default())
            .map_err(|e| SigningError::SdkError(format!("client_new: {e:?}")))?;
        let builder_no_funder = unauth
            .authentication_builder(&signer)
            .credentials(credentials)
            .signature_type(signature_type);
        let builder = match funder {
            Some(addr) => builder_no_funder.funder(addr),
            None => builder_no_funder,
        };

        let client = builder
            .authenticate()
            .await
            .map_err(|e| SigningError::SdkError(format!("authenticate: {e:?}")))?;

        Ok(Self {
            inner: signer,
            client,
        })
    }

    /// Maker address (the on-chain identity we sign as).
    pub fn address(&self) -> Address {
        self.inner.address()
    }

    /// Build, sign, and JSON-serialize a FAK BUY for the given canonical
    /// target. Returns the serialized body bytes; caller hands the bytes
    /// to L2 auth + POST /order.
    pub async fn sign_fak_buy(
        &self,
        token: &TokenId,
        target: &BuyCanonicalTarget,
    ) -> Result<Vec<u8>, SigningError> {
        let token_u256 =
            U256::from_str(token.as_str()).map_err(|_| SigningError::InvalidTokenId)?;
        let price_dec = price_to_decimal(target.price.ticks());
        // Shares4 -> Decimal (4 dp). The SDK builder enforces size scale
        // <= LOT_SIZE_SCALE (=2). Our canonical sizes are always multiples
        // of 0.01 share so the trailing four-dp precision is dead. Reduce
        // before handing off.
        let size_dec = shares4_to_2dp_decimal(target.size.units())?;

        let order = self
            .client
            .limit_order()
            .token_id(token_u256)
            .side(Side::Buy)
            .price(price_dec)
            .size(size_dec)
            .order_type(polymarket_client_sdk_v2::clob::types::OrderType::FAK)
            .build()
            .await
            .map_err(|e| SigningError::SdkError(format!("limit_order_build: {e:?}")))?;

        let signed = self
            .client
            .sign(&self.inner, order)
            .await
            .map_err(|e| SigningError::SdkError(format!("sign: {e:?}")))?;

        serde_json::to_vec(&signed).map_err(|e| SigningError::Serialize(format!("{e}")))
    }

    /// Build, sign, and JSON-serialize a FAK SELL.
    pub async fn sign_fak_sell(
        &self,
        token: &TokenId,
        price: PriceTick,
        size: Shares2,
    ) -> Result<Vec<u8>, SigningError> {
        let token_u256 =
            U256::from_str(token.as_str()).map_err(|_| SigningError::InvalidTokenId)?;
        let price_dec = price_to_decimal(price.ticks());
        let size_dec =
            Decimal::try_from_i128_with_scale(size.units() as i128, 2)
                .map_err(|_| SigningError::InvalidPriceOrSize)?;

        let order = self
            .client
            .limit_order()
            .token_id(token_u256)
            .side(Side::Sell)
            .price(price_dec)
            .size(size_dec)
            .order_type(polymarket_client_sdk_v2::clob::types::OrderType::FAK)
            .build()
            .await
            .map_err(|e| SigningError::SdkError(format!("limit_order_build: {e:?}")))?;

        let signed = self
            .client
            .sign(&self.inner, order)
            .await
            .map_err(|e| SigningError::SdkError(format!("sign: {e:?}")))?;

        serde_json::to_vec(&signed).map_err(|e| SigningError::Serialize(format!("{e}")))
    }
}

impl From<OrderSide> for Side {
    fn from(value: OrderSide) -> Self {
        match value {
            OrderSide::Buy => Side::Buy,
            OrderSide::Sell => Side::Sell,
        }
    }
}

/// $0.01 tick count -> `Decimal` with 2-dp scale (matches the venue's
/// minimum tick).
fn price_to_decimal(ticks: i32) -> Decimal {
    Decimal::new(ticks as i64, 2)
}

/// Shares4 (0.0001-share units) -> Decimal at 2-dp scale. Errors if the
/// trailing 4-dp precision is non-zero (which would indicate a logic bug
/// in the canonicalizer; canonical BUY/SELL sizes are always 2-dp values
/// expressed in 4-dp units, so the bottom two digits are always `00`).
fn shares4_to_2dp_decimal(units_4dp: i64) -> Result<Decimal, SigningError> {
    if units_4dp.rem_euclid(100) != 0 {
        return Err(SigningError::InvalidPriceOrSize);
    }
    Decimal::try_from_i128_with_scale(units_4dp as i128 / 100, 2)
        .map_err(|_| SigningError::InvalidPriceOrSize)
}

#[cfg(test)]
mod tests {
    //! Phase 3b tests
    //!
    //! The pure-Rust tests below cover the conversion boundary
    //! (PriceTick / Shares* -> Decimal). Live signing tests are marked
    //! `#[ignore]` and require a reachable CLOB plus credentials —
    //! invoke them by exporting POLYMARKET_PRIVATE_KEY plus auth env
    //! vars and running:
    //!     cargo test signing::tests -- --ignored
    use super::*;
    use crate::types::PriceTick;
    use polymarket_client_sdk_v2::auth::Uuid;

    #[test]
    fn price_to_decimal_renders_two_dp() {
        let d = price_to_decimal(50);
        assert_eq!(d.scale(), 2);
        assert_eq!(d.to_string(), "0.50");
    }

    #[test]
    fn shares4_to_2dp_drops_trailing_zeros() {
        // 20_200 (Shares4 units) = 2.02 shares
        let d = shares4_to_2dp_decimal(20_200).unwrap();
        assert_eq!(d.scale(), 2);
        assert_eq!(d.to_string(), "2.02");
    }

    #[test]
    fn shares4_to_2dp_rejects_subcent_residue() {
        // 20_201 -> 2.0201 shares; trailing two digits non-zero.
        // Canonicalizer should never produce this, so we fail closed.
        let err = shares4_to_2dp_decimal(20_201).unwrap_err();
        assert_eq!(err, SigningError::InvalidPriceOrSize);
    }

    #[test]
    fn order_side_to_sdk_side() {
        let s: Side = OrderSide::Buy.into();
        assert_eq!(s, Side::Buy);
        let s: Side = OrderSide::Sell.into();
        assert_eq!(s, Side::Sell);
    }

    /// Live signing test. Skipped by default; needs:
    ///   POLYMARKET_PRIVATE_KEY: hex private key
    ///   POLY_API_KEY:           UUID
    ///   POLY_API_SECRET:        URL-safe base64 string
    ///   POLY_API_PASSPHRASE:    string
    /// And a reachable CLOB at https://clob-v2.polymarket.com
    #[tokio::test]
    #[ignore = "live: requires CLOB network access and credentials"]
    async fn signs_fak_buy_against_live_clob() {
        let pk = std::env::var("POLYMARKET_PRIVATE_KEY")
            .expect("POLYMARKET_PRIVATE_KEY required for live signing test");
        let creds = Credentials::new(
            std::env::var("POLY_API_KEY")
                .ok()
                .and_then(|k| Uuid::parse_str(&k).ok())
                .expect("POLY_API_KEY must be a valid UUID"),
            std::env::var("POLY_API_SECRET").expect("POLY_API_SECRET"),
            std::env::var("POLY_API_PASSPHRASE").expect("POLY_API_PASSPHRASE"),
        );
        // Adjust signature type to match the user's wallet kind.
        let signer = OrderSigner::new(
            &pk,
            "https://clob-v2.polymarket.com",
            creds,
            None,
            SignatureType::Eoa,
        )
        .await
        .expect("OrderSigner::new");

        let target = crate::orders::canonical_buy_target_for_notional(
            crate::orders::BuyCanonicalInput {
                price: PriceTick::checked(50).unwrap(),
                target_maker_cents: 101,
                min_size_taker_units: 100,
                min_maker_cents: 100,
                max_overrun_cents: 1,
                max_overrun_bps: 0,
            },
        )
        .unwrap();
        // Caller must supply a real, currently-listed token_id.
        let token = TokenId::new(
            std::env::var("POLY_TEST_TOKEN_ID").expect("POLY_TEST_TOKEN_ID"),
        );
        let body = signer.sign_fak_buy(&token, &target).await.unwrap();
        assert!(!body.is_empty());
        // Body must parse as JSON and carry a signature field somewhere.
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let body_str = serde_json::to_string(&v).unwrap();
        assert!(
            body_str.contains("signature"),
            "signed body missing signature: {body_str}"
        );
    }
}
