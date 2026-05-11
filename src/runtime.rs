//! Minimal runtime integration edges.
//!
//! This is not an orchestrator. It contains only the narrow glue that has
//! proven call sites: Binance book-ticker frame -> signal sample -> optional
//! BUY intent after current state and inventory risk checks.

use crate::binance::BinanceParseError;
use crate::config::{Config, ConfigError};
use crate::inventory::{Inventory, SubmitId};
use crate::market::{apply_market_events, parse_market_events, MarketParseError};
use crate::orders::{
    canonical_buy_target_for_notional, canonical_sell_params, BuyCanonicalError, BuyCanonicalInput,
    BuyCanonicalTarget,
};
use crate::signal::{BuyIntent, SignalEngine};
use crate::signing::{OrderSigner, SignInputs, SignedFakOrderBody, SigningError};
use crate::state::RuntimeState;
use crate::submit::SubmitOutcome;
use crate::types::{OrderId, OutcomeSide, PriceTick, Shares2, SharesAtoms, TokenId, TsUs};
use crate::user::{parse_user_message, UserMessage, UserParseError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    Binance(BinanceParseError),
    Market(MarketParseError),
    User(UserParseError),
    BuyCanonical(BuyCanonicalError),
    Signing(SigningError),
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuntimeError::Binance(e) => write!(f, "binance: {e}"),
            RuntimeError::Market(e) => write!(f, "market: {e}"),
            RuntimeError::User(e) => write!(f, "user: {e}"),
            RuntimeError::BuyCanonical(e) => write!(f, "buy_canonical: {e}"),
            RuntimeError::Signing(e) => write!(f, "signing: {e}"),
        }
    }
}

impl std::error::Error for RuntimeError {}

impl From<BinanceParseError> for RuntimeError {
    fn from(value: BinanceParseError) -> Self {
        RuntimeError::Binance(value)
    }
}

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
    pub target: BuyCanonicalTarget,
    pub body: SignedFakOrderBody,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedSellSubmit {
    pub price: PriceTick,
    pub size: Shares2,
    pub body: SignedFakOrderBody,
}

pub struct RuntimeCore {
    state: RuntimeState,
    inventory: Inventory,
    signal: SignalEngine,
    buy_policy: BuySubmitPolicy,
    max_open_positions: usize,
}

impl RuntimeCore {
    pub fn new(config: &Config) -> Result<Self, ConfigError> {
        Ok(Self {
            state: RuntimeState::new(),
            inventory: Inventory::new(),
            signal: SignalEngine::new(config.signal_config()?),
            buy_policy: config.buy_submit_policy(),
            max_open_positions: config.max_concurrent_positions,
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

    pub fn max_open_positions(&self) -> usize {
        self.max_open_positions
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
            self.max_open_positions,
        )
    }

    pub fn apply_market_raw(&mut self, raw: &[u8], ts_us: TsUs) -> Result<usize, RuntimeError> {
        let events = parse_market_events(raw)?;
        Ok(apply_market_events(&events, &mut self.state, ts_us))
    }

    pub fn apply_user_raw(&mut self, raw: &[u8], ts_us: i64) -> Result<UserMessage, RuntimeError> {
        let msg = parse_user_message(raw, ts_us)?;
        if let UserMessage::Trades(ref trades) = msg {
            for trade in trades {
                self.inventory.apply_user_trade(trade.clone());
            }
        }
        Ok(msg)
    }

    // Delivered by DeepSeek — sell convenience methods.
    // Methods on `&self` benefit from field-level borrow splitting.

    /// Prepare a FAK SELL at the current executable bid. Returns `None` if
    /// no bid quote exists or sellable inventory is zero.
    pub fn prepare_sell_at_bid(
        &self,
        token: &TokenId,
        signer: &OrderSigner,
        sign_inputs: SignInputs,
    ) -> Result<Option<PreparedSellSubmit>, RuntimeError> {
        prepare_sell_submit_at_bid(token, &self.state, signer, sign_inputs, &self.inventory)
    }

    /// Prepare a FAK SELL for a specific size (in atoms) at the current bid.
    pub fn prepare_sell_for_size_at_bid(
        &self,
        token: &TokenId,
        size_atoms: SharesAtoms,
        signer: &OrderSigner,
        sign_inputs: SignInputs,
    ) -> Result<Option<PreparedSellSubmit>, RuntimeError> {
        prepare_sell_submit_for_size_at_bid(
            token, size_atoms, &self.state, signer, sign_inputs, &self.inventory,
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
    max_open_positions: usize,
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
    let scope = [&market.yes_token, &market.no_token];
    if max_open_positions == 0 || inventory.open_position_count(scope) >= max_open_positions {
        return Ok(None);
    }
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
/// intent's lock release and `register_submit` inside the spawn.
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
        target,
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
            inventory.mark_submit_rejected(submit_id, now_ts_us);
        }
    }
}

pub fn prepare_sell_submit(
    token: &TokenId,
    limit: PriceTick,
    signer: &OrderSigner,
    sign_inputs: SignInputs,
    inventory: &Inventory,
) -> Result<Option<PreparedSellSubmit>, RuntimeError> {
    let Some(position) = inventory.position(token) else {
        return Ok(None);
    };
    if position.sellable.units() <= 0 {
        return Ok(None);
    }
    let (price, size) = canonical_sell_params(limit, position.sellable.units() * 100)?;
    if size.units() <= 0 {
        return Ok(None);
    }
    let body = signer.sign_fak_sell(token, price, size, sign_inputs)?;
    Ok(Some(PreparedSellSubmit { price, size, body }))
}

pub fn prepare_sell_submit_at_bid(
    token: &TokenId,
    state: &RuntimeState,
    signer: &OrderSigner,
    sign_inputs: SignInputs,
    inventory: &Inventory,
) -> Result<Option<PreparedSellSubmit>, RuntimeError> {
    let Some(bid) = state.quote_for_token(token).and_then(|q| q.bid) else {
        return Ok(None);
    };
    prepare_sell_submit(token, bid, signer, sign_inputs, inventory)
}

pub fn prepare_sell_submit_for_size_at_bid(
    token: &TokenId,
    size_atoms: SharesAtoms,
    state: &RuntimeState,
    signer: &OrderSigner,
    sign_inputs: SignInputs,
    inventory: &Inventory,
) -> Result<Option<PreparedSellSubmit>, RuntimeError> {
    let Some(bid) = state.quote_for_token(token).and_then(|q| q.bid) else {
        return Ok(None);
    };
    prepare_sell_submit_for_size(token, bid, size_atoms, signer, sign_inputs, inventory)
}

fn prepare_sell_submit_for_size(
    token: &TokenId,
    limit: PriceTick,
    size_atoms: SharesAtoms,
    signer: &OrderSigner,
    sign_inputs: SignInputs,
    _inventory: &Inventory,
) -> Result<Option<PreparedSellSubmit>, RuntimeError> {
    let raw_size_taker_units = size_atoms.atoms().div_euclid(100);
    let (price, size) = canonical_sell_params(limit, raw_size_taker_units)?;
    if size.units() <= 0 {
        return Ok(None);
    }
    let body = signer.sign_fak_sell(token, price, size, sign_inputs)?;
    Ok(Some(PreparedSellSubmit { price, size, body }))
}

