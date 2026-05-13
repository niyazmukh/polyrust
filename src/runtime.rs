//! Minimal runtime integration edges.
//!
//! This is not an orchestrator. It contains only the narrow glue that has
//! proven call sites: Binance book-ticker frame -> signal sample -> optional
//! BUY intent after current state and inventory risk checks.

use crate::config::{Config, ConfigError};
use crate::inventory::{Inventory, SubmitId, TradeState};
use crate::market::{MarketParseError, apply_market_events, parse_market_events};
use crate::orders::{
    BuyCanonicalError, BuyCanonicalInput, canonical_buy_target_for_notional, canonical_sell_params,
};
use crate::signal::{BuyIntent, SignalEngine};
use crate::signing::{OrderSigner, SignInputs, SignedFakOrderBody, SigningError};
use crate::state::RuntimeState;
use crate::submit::SubmitOutcome;
use crate::types::{OrderId, OutcomeSide, PriceTick, Shares2, TokenId, TsUs};
use crate::user::{UserMessage, UserParseError, parse_user_message};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    Market(MarketParseError),
    User(UserParseError),
    BuyCanonical(BuyCanonicalError),
    Signing(SigningError),
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuntimeError::Market(e) => write!(f, "market: {e}"),
            RuntimeError::User(e) => write!(f, "user: {e}"),
            RuntimeError::BuyCanonical(e) => write!(f, "buy_canonical: {e}"),
            RuntimeError::Signing(e) => write!(f, "signing: {e}"),
        }
    }
}

impl std::error::Error for RuntimeError {}

impl From<MarketParseError> for RuntimeError {
    fn from(value: MarketParseError) -> Self {
        RuntimeError::Market(value)
    }
}

impl From<UserParseError> for RuntimeError {
    fn from(value: UserParseError) -> Self {
        RuntimeError::User(value)
    }
}

impl From<BuyCanonicalError> for RuntimeError {
    fn from(value: BuyCanonicalError) -> Self {
        RuntimeError::BuyCanonical(value)
    }
}

impl From<SigningError> for RuntimeError {
    fn from(value: SigningError) -> Self {
        RuntimeError::Signing(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuySubmitPolicy {
    pub target_maker_cents: i64,
    pub min_size_taker_units: i64,
    pub min_maker_cents: i64,
    pub max_overrun_cents: i64,
    pub max_overrun_bps: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedBuySubmit {
    pub submit_id: SubmitId,
    pub body: SignedFakOrderBody,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedSellSubmit {
    pub price: PriceTick,
    pub size: Shares2,
    pub body: SignedFakOrderBody,
}

/// Snapshot of the inputs needed to sign a FAK SELL, computable purely
/// from WSS-owned inventory and state under the core lock, with no
/// signing or JSON serialization required. Callers on the hot path are
/// expected to build a `SellPlan` under the lock, drop the lock, then
/// hand the plan to `sign_sell_plan` outside the lock. Keeping signing
/// off the mutex prevents the SELL exit loop from serializing against
/// the Binance BUY decision critical section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SellPlan {
    pub token: TokenId,
    pub price: PriceTick,
    pub size: Shares2,
}

impl SellPlan {
    /// Sign the plan into a submit-ready body. This does EIP-712
    /// keccak256 + secp256k1 ECDSA + JSON serialization; it is
    /// intentionally not called inside any shared mutex.
    pub fn sign(
        &self,
        signer: &OrderSigner,
        sign_inputs: SignInputs,
    ) -> Result<PreparedSellSubmit, RuntimeError> {
        let body = signer.sign_fak_sell(&self.token, self.price, self.size, sign_inputs)?;
        Ok(PreparedSellSubmit {
            price: self.price,
            size: self.size,
            body,
        })
    }
}

pub struct RuntimeCore {
    state: RuntimeState,
    inventory: Inventory,
    signal: SignalEngine,
    buy_policy: BuySubmitPolicy,
    sell_slippage_ticks: i32,
}

impl RuntimeCore {
    pub fn new(config: &Config) -> Result<Self, ConfigError> {
        Ok(Self {
            state: RuntimeState::new(),
            inventory: Inventory::new(),
            signal: SignalEngine::new(config.signal_config()?),
            buy_policy: config.buy_submit_policy(),
            sell_slippage_ticks: config.sell_slippage_cents,
        })
    }

    pub fn state_mut(&mut self) -> &mut RuntimeState {
        &mut self.state
    }

    pub fn inventory(&self) -> &Inventory {
        &self.inventory
    }

    pub fn inventory_mut(&mut self) -> &mut Inventory {
        &mut self.inventory
    }

    pub fn signal_mut(&mut self) -> &mut SignalEngine {
        &mut self.signal
    }

    pub fn buy_submit_policy(&self) -> BuySubmitPolicy {
        self.buy_policy
    }

    pub fn on_binance_sample(
        &mut self,
        sample: crate::signal::BinanceSample,
        now: TsUs,
        tte_us: i64,
    ) -> Result<Option<BuyIntent>, RuntimeError> {
        on_binance_sample(
            sample,
            &mut self.signal,
            &self.state,
            &self.inventory,
            now,
            tte_us,
        )
    }

    pub fn apply_market_raw(&mut self, raw: &[u8], ts_us: TsUs) -> Result<usize, RuntimeError> {
        let events = parse_market_events(raw)?;
        Ok(apply_market_events(&events, &mut self.state, ts_us))
    }

    pub fn apply_user_raw(&mut self, raw: &[u8], ts_us: i64) -> Result<UserMessage, RuntimeError> {
        self.apply_user_raw_with_states(raw, ts_us)
            .map(|(msg, _states)| msg)
    }

    pub fn apply_user_raw_with_states(
        &mut self,
        raw: &[u8],
        ts_us: i64,
    ) -> Result<(UserMessage, Vec<TradeState>), RuntimeError> {
        let msg = parse_user_message(raw, ts_us)?;
        let mut states = Vec::new();
        if let UserMessage::Trades(ref trades) = msg {
            for trade in trades {
                states.push(self.inventory.apply_user_trade(trade.clone()));
            }
        }
        Ok((msg, states))
    }

    // Sell convenience methods. `&self` benefits from field-level borrow splitting.

    /// Build a `SellPlan` for a FAK SELL at the current executable bid.
    /// Returns `None` if no bid quote exists or sellable inventory is
    /// zero. This does no signing — callers must hand the plan to
    /// `SellPlan::sign` outside the core lock.
    /// Compute a `SellPlan` for a FAK SELL at the current executable bid,
    /// without signing. The returned plan can be handed to `SellPlan::sign`
    /// outside any shared mutex. Returns `None` if no bid exists, the
    /// sellable inventory is zero, or the size rounds to zero sellable
    /// units after venue-quantum snap.
    pub fn plan_sell_at_bid(&self, token: &TokenId) -> Option<SellPlan> {
        plan_sell_at_bid(
            token,
            &self.state,
            &self.inventory,
            self.sell_slippage_ticks,
        )
    }

    pub fn plan_sells_at_bid_excluding(
        &self,
        tokens: impl IntoIterator<Item = TokenId>,
        in_flight: &HashSet<TokenId>,
    ) -> Vec<SellPlan> {
        plan_sells_at_bid_excluding(
            tokens,
            &self.state,
            &self.inventory,
            self.sell_slippage_ticks,
            in_flight,
        )
    }
}

pub fn on_binance_sample(
    sample: crate::signal::BinanceSample,
    signal: &mut SignalEngine,
    state: &RuntimeState,
    inventory: &Inventory,
    now: TsUs,
    tte_us: i64,
) -> Result<Option<BuyIntent>, RuntimeError> {
    if !state.trading_active() {
        signal.push(sample);
        return Ok(None);
    }
    let Some(market) = state.market() else {
        signal.push(sample);
        return Ok(None);
    };
    let Some(yes_quote) = state.quote_for_side(OutcomeSide::Yes).copied() else {
        signal.push(sample);
        return Ok(None);
    };
    let Some(no_quote) = state.quote_for_side(OutcomeSide::No).copied() else {
        signal.push(sample);
        return Ok(None);
    };

    let Some(intent) = signal.on_sample(sample, market, yes_quote, no_quote, now, tte_us) else {
        return Ok(None);
    };
    if inventory.has_entry_exposure_or_pending(&intent.token) {
        return Ok(None);
    }
    Ok(Some(intent))
}

/// Prepare a BUY submit using a pre-claimed `SubmitId`.
///
/// The claim must have been registered via `inventory.claim_entry()`
/// **before** the async spawn, under the same lock that produced the
/// `BuyIntent`. This closes the duplicate race where a second intent
/// could pass `has_entry_exposure_or_pending` between the first
/// intent's lock release and `claim_entry` inside the spawn.
pub fn prepare_buy_submit(
    intent: &BuyIntent,
    policy: BuySubmitPolicy,
    signer: &OrderSigner,
    sign_inputs: SignInputs,
    claim_id: SubmitId,
) -> Result<PreparedBuySubmit, RuntimeError> {
    let target = canonical_buy_target_for_notional(BuyCanonicalInput {
        price: intent.limit,
        target_maker_cents: policy.target_maker_cents,
        min_size_taker_units: policy.min_size_taker_units,
        min_maker_cents: policy.min_maker_cents,
        max_overrun_cents: policy.max_overrun_cents,
        max_overrun_bps: policy.max_overrun_bps,
    })?;
    let body = signer.sign_fak_buy(&intent.token, &target, sign_inputs)?;
    Ok(PreparedBuySubmit {
        submit_id: claim_id,
        body,
    })
}

pub fn record_buy_submit_outcome(
    inventory: &mut Inventory,
    submit_id: &SubmitId,
    outcome: &SubmitOutcome,
    now_ts_us: i64,
) {
    match outcome {
        SubmitOutcome::Accepted { order_id, .. } => {
            inventory.mark_submit_accepted(submit_id, OrderId::new(order_id.clone()), now_ts_us);
        }
        SubmitOutcome::Unknown { .. } => {
            inventory.mark_submit_unknown(submit_id, now_ts_us);
        }
        SubmitOutcome::Rejected { .. } => {
            // FAK rejection is definitive no-order. Remove the claim
            // so it does not linger in pending scans.
            inventory.release_claim(submit_id);
        }
    }
}

/// Compute a `SellPlan` for a FAK SELL at the current executable bid,
/// without signing. The returned plan can be handed to `SellPlan::sign`
/// outside any shared mutex. Returns `None` if no bid exists, the
/// sellable inventory is zero, or the size rounds to zero sellable
/// units after venue-quantum snap.
pub fn plan_sell_at_bid(
    token: &TokenId,
    state: &RuntimeState,
    inventory: &Inventory,
    slippage_ticks: i32,
) -> Option<SellPlan> {
    let bid = state.quote_for_token(token).and_then(|q| q.bid)?;
    let position = inventory.position(token)?;
    if position.sellable.units() <= 0 {
        return None;
    }
    let limit_ticks = (bid.ticks() - slippage_ticks).max(1);
    let limit = PriceTick::checked(limit_ticks).ok()?;
    let (price, size) = canonical_sell_params(limit, position.sellable.units() * 100).ok()?;
    if size.units() <= 0 {
        return None;
    }
    Some(SellPlan {
        token: token.clone(),
        price,
        size,
    })
}

pub fn plan_sells_at_bid_excluding(
    tokens: impl IntoIterator<Item = TokenId>,
    state: &RuntimeState,
    inventory: &Inventory,
    slippage_ticks: i32,
    in_flight: &HashSet<TokenId>,
) -> Vec<SellPlan> {
    tokens
        .into_iter()
        .filter(|token| !in_flight.contains(token))
        .filter_map(|token| plan_sell_at_bid(&token, state, inventory, slippage_ticks))
        .collect()
}
