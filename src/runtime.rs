//! Minimal runtime integration edges.
//!
//! This is not an orchestrator. It contains only the narrow glue that has
//! proven call sites: Binance book-ticker frame -> signal sample -> optional
//! BUY intent after current state and inventory risk checks.

use crate::config::{Config, ConfigError};
use crate::inventory::{Inventory, SubmitId, TradeState, TradeStatus, UserTrade};
use crate::logline::{self, Field, Level};
use crate::market::{MarketParseError, apply_market_events, parse_market_events};
use crate::orders::{
    BuyCanonicalError, BuyCanonicalInput, canonical_buy_target_for_notional, canonical_sell_params,
};
use crate::signal::{BuyIntent, SignalEngine, implied_p_yes_for};
use crate::signing::{OrderSigner, SignInputs, SignedFakOrderBody, SigningError};
use crate::state::RuntimeState;
use crate::submit::SubmitOutcome;
use crate::types::{OrderId, OutcomeSide, PriceTick, Shares2, TokenId, TsUs};
use crate::user::{UserMessage, UserParseError, parse_user_message};
use std::collections::{HashMap, HashSet};

/// Smoothing factor (1/2^RTT_EWMA_SHIFT) for the BUY submit→outcome RTT
/// exponential moving average. `shift=3` weights each new sample at 1/8 so
/// the EWMA stabilises over ~8 samples while reacting to a sustained latency
/// regression within a couple of minutes.
const RTT_EWMA_SHIFT: u32 = 3;

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
    pub body: SignedFakOrderBody,
}

pub const UNKNOWN_SUBMIT_EXPIRE_US: i64 = 5_000_000;
pub const PENDING_SUBMIT_EXPIRE_US: i64 = 60_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitReason {
    Value,
    Drop,
    Stop,
    Hold,
}

impl ExitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ExitReason::Value => "value",
            ExitReason::Drop => "drop",
            ExitReason::Stop => "stop",
            ExitReason::Hold => "hold",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitDecision {
    pub plan: SellPlan,
    pub reason: ExitReason,
    pub entry_price: PriceTick,
    pub entry_bid: PriceTick,
    pub peak_bid: PriceTick,
    pub bid: PriceTick,
    pub fair_ticks: Option<i32>,
    pub fair_minus_bid_ticks: Option<i32>,
    pub opposes: Option<bool>,
    pub hold_us: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExitTracker {
    entry_price: PriceTick,
    entry_bid: PriceTick,
    peak_bid: PriceTick,
    fill_ts_us: i64,
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
        Ok(PreparedSellSubmit { body })
    }
}

/// Adverse-selection gates that run after the Binance signal returns a
/// `BuyIntent`. All gates are configurable; setting a threshold to `0`
/// disables the check. Internal to the runtime — callers configure these via
/// `Config`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EntryGateConfig {
    /// Hard ceiling on the BUY submit→outcome RTT EWMA (µs). When exceeded,
    /// new entries are suppressed because the venue book has time to be lifted
    /// by faster Polymarket-native HFTs before our POST arrives.
    rtt_ceiling_us: i64,
    /// Pre-trade Polymarket drift window (µs). Drift is measured against the
    /// oldest history sample at or after `now − window`.
    poly_drift_window_us: i64,
    poly_drift_block_up_ticks: i32,
    poly_drift_block_down_ticks: i32,
    poly_drift_safety_bps: i32,
    poly_drift_min_clean_edge_ticks: i32,
}

pub struct RuntimeCore {
    state: RuntimeState,
    inventory: Inventory,
    signal: SignalEngine,
    buy_policy: BuySubmitPolicy,
    sell_slippage_ticks: i32,
    exit_drop_ticks: i32,
    exit_arm_ticks: i32,
    exit_stop_ticks: i32,
    exit_edge_ticks: i32,
    exit_hold_us: i64,
    exit_trackers: HashMap<TokenId, ExitTracker>,
    entry_gates: EntryGateConfig,
    /// Exponential moving average of recent BUY submit→outcome RTTs, in µs.
    /// Updated by `record_rtt` after each accepted/rejected/unknown outcome.
    /// Zero means "no samples yet" — every gate that consumes the EWMA must
    /// treat zero as "skip".
    rtt_ewma_us: i64,
}

impl RuntimeCore {
    pub fn new(config: &Config) -> Result<Self, ConfigError> {
        Ok(Self {
            state: RuntimeState::new(),
            inventory: Inventory::new(),
            signal: SignalEngine::new(config.signal_config()?),
            buy_policy: config.buy_submit_policy(),
            sell_slippage_ticks: config.sell_slippage_cents,
            exit_drop_ticks: config.exit_drop_ticks,
            exit_arm_ticks: config.exit_arm_ticks,
            exit_stop_ticks: config.exit_stop_ticks,
            exit_edge_ticks: config.exit_edge_ticks,
            exit_hold_us: config.exit_hold_us,
            exit_trackers: HashMap::new(),
            entry_gates: EntryGateConfig {
                rtt_ceiling_us: config.rtt_ceiling_us,
                poly_drift_window_us: config.poly_drift_window_us,
                poly_drift_block_up_ticks: config.poly_drift_block_up_ticks,
                poly_drift_block_down_ticks: config.poly_drift_block_down_ticks,
                poly_drift_safety_bps: config.poly_drift_safety_bps,
                poly_drift_min_clean_edge_ticks: config.poly_drift_min_clean_edge_ticks,
            },
            rtt_ewma_us: 0,
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
        let Some(intent) = on_binance_sample(
            sample,
            &mut self.signal,
            &self.state,
            &self.inventory,
            now,
            tte_us,
        )?
        else {
            return Ok(None);
        };
        if let Some(reason) = self.entry_gate_block(&intent, now) {
            log_signal_blocked(&intent, reason, self.rtt_ewma_us);
            return Ok(None);
        }
        Ok(Some(intent))
    }

    /// Fold a fresh BUY-submit RTT sample into the EWMA. Called off the hot
    /// path (inside the spawned submit task) once the HTTP outcome lands.
    pub fn record_rtt(&mut self, rtt_us: i64) {
        if rtt_us <= 0 {
            return;
        }
        if self.rtt_ewma_us == 0 {
            self.rtt_ewma_us = rtt_us;
            return;
        }
        let delta = (rtt_us - self.rtt_ewma_us) >> RTT_EWMA_SHIFT;
        // `>>` on negatives floors toward −∞, which is what we want for the
        // EWMA recurrence; saturating add keeps us safe against i64 overflow
        // on a pathological venue stall.
        self.rtt_ewma_us = self.rtt_ewma_us.saturating_add(delta);
    }

    pub fn current_ewma_rtt_us(&self) -> i64 {
        self.rtt_ewma_us
    }

    /// Recompute the SELL plan for a token at the *current* WSS bid, so the
    /// signed FAK limit reflects whatever the book moved to between the
    /// previous exit decision and the moment we are about to sign. Returns
    /// `None` if sellable inventory has cleared or there is no executable
    /// bid — the caller skips that exit cycle.
    pub fn refresh_sell_plan(&self, token: &TokenId) -> Option<SellPlan> {
        plan_sell_at_bid(
            token,
            &self.state,
            &self.inventory,
            self.sell_slippage_ticks,
        )
    }

    /// Apply the four execution-aware gates on top of the Binance fair-value
    /// model: RTT ceiling, Polymarket pre-trade drift (block-up + block-down),
    /// and the drift-buffer edge sufficiency check.
    fn entry_gate_block(&self, intent: &BuyIntent, now: TsUs) -> Option<&'static str> {
        let gates = &self.entry_gates;
        // Pillar A: EWMA RTT ceiling.
        if gates.rtt_ceiling_us > 0
            && self.rtt_ewma_us > 0
            && self.rtt_ewma_us > gates.rtt_ceiling_us
        {
            return Some("rtt_ewma_high");
        }
        if gates.poly_drift_window_us <= 0 {
            return None;
        }
        let cutoff_us = now.micros() - gates.poly_drift_window_us;
        let past = self
            .state
            .quote_history_oldest_since(&intent.token, cutoff_us)?;
        let past_ask_ticks = past.ask_ticks?;
        let current_ask = self
            .state
            .quote_for_token(&intent.token)
            .and_then(|q| q.ask)?;
        let drift_ticks = current_ask.ticks() - past_ask_ticks;
        // Pillar C: directional drift blocks.
        if gates.poly_drift_block_up_ticks > 0 && drift_ticks >= gates.poly_drift_block_up_ticks {
            return Some("poly_drift_up");
        }
        if gates.poly_drift_block_down_ticks > 0
            && drift_ticks <= -gates.poly_drift_block_down_ticks
        {
            return Some("poly_drift_down");
        }
        // Pillar B: empirical drift-buffer edge sufficiency.
        if gates.poly_drift_safety_bps > 0 && self.rtt_ewma_us > 0 {
            let dt_us = now.micros() - past.ts_us.micros();
            if dt_us > 0 {
                let drift_abs = i64::from(drift_ticks.abs());
                // required_drift_during_rtt_ticks = ceil(
                //     |drift_ticks| · rtt_ewma_us · safety_bps
                //     / (dt_us · 10_000)
                // )
                let numer = drift_abs
                    .saturating_mul(self.rtt_ewma_us)
                    .saturating_mul(i64::from(gates.poly_drift_safety_bps));
                let denom = dt_us.saturating_mul(10_000);
                if denom > 0 {
                    let required_drift = ((numer + denom - 1) / denom).min(i32::MAX as i64) as i32;
                    let required_edge =
                        required_drift.saturating_add(gates.poly_drift_min_clean_edge_ticks);
                    if intent.edge_ticks < required_edge {
                        return Some("edge_insufficient_for_drift");
                    }
                }
            }
        }
        None
    }

    pub fn apply_market_raw(&mut self, raw: &[u8], ts_us: TsUs) -> Result<usize, RuntimeError> {
        let events = parse_market_events(raw)?;
        Ok(apply_market_events(&events, &mut self.state, ts_us))
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
                let state = self.inventory.apply_user_trade(trade.clone());
                self.track_user_trade_for_exit(trade, &state);
                states.push(state);
            }
        }
        Ok((msg, states))
    }

    fn track_user_trade_for_exit(&mut self, trade: &UserTrade, state: &TradeState) {
        if !state.inventory_changed {
            return;
        }
        match state.side {
            crate::types::OrderSide::Buy => {
                if matches!(state.status, TradeStatus::Matched | TradeStatus::Confirmed)
                    && self.inventory.sellable(&state.token).units() > 0
                {
                    let current_bid = self
                        .state
                        .quote_for_token(&state.token)
                        .and_then(|quote| quote.bid)
                        .unwrap_or(trade.price);
                    self.exit_trackers.insert(
                        state.token.clone(),
                        ExitTracker {
                            entry_price: trade.price,
                            entry_bid: current_bid,
                            peak_bid: current_bid.max(trade.price),
                            fill_ts_us: trade.ts_us,
                        },
                    );
                }
            }
            crate::types::OrderSide::Sell => {
                if self.inventory.sellable(&state.token).units() <= 0 {
                    self.exit_trackers.remove(&state.token);
                }
            }
        }
    }

    /// Build FAK SELL plans for active-market inventory when fair-value,
    /// adverse-stop, or stale-model time-boundary logic fires. WSS SELL
    /// MATCHED clears inventory; until then, each exit loop may submit
    /// another FAK SELL.
    pub fn plan_exits(&mut self, now_ts_us: i64) -> Vec<ExitDecision> {
        let mut out = Vec::new();
        let Some(market) = self.state.market().cloned() else {
            return out;
        };
        let tokens = [
            (market.yes_token.clone(), OutcomeSide::Yes),
            (market.no_token.clone(), OutcomeSide::No),
        ];
        let yes_quote = self.state.quote_for_side(OutcomeSide::Yes).copied();
        let no_quote = self.state.quote_for_side(OutcomeSide::No).copied();
        let implied_hint = match (yes_quote, no_quote) {
            (Some(y), Some(n)) => implied_p_yes_for(y, n),
            _ => None,
        };
        for (token, side) in tokens {
            let Some(position) = self.inventory.position(&token) else {
                self.exit_trackers.remove(&token);
                continue;
            };
            if position.sellable.units() <= 0 {
                self.exit_trackers.remove(&token);
                continue;
            }
            let Some(bid) = self.state.quote_for_token(&token).and_then(|q| q.bid) else {
                continue;
            };
            let tte_us = market
                .end_ts
                .saturating_mul(1_000_000)
                .saturating_sub(now_ts_us);
            let fair_ticks =
                self.signal
                    .fair_ticks_for_side(side, TsUs(now_ts_us), tte_us, implied_hint);
            let opposes = self.signal.opposes_side(side, TsUs(now_ts_us));
            let Some(tracker) = self.exit_trackers.get_mut(&token) else {
                continue;
            };
            tracker.peak_bid = tracker.peak_bid.max(bid);
            let hold_us = now_ts_us.saturating_sub(tracker.fill_ts_us);
            let bid_drop_ticks = tracker.entry_bid.ticks() - bid.ticks();
            let profit_ticks = tracker.peak_bid.ticks() - tracker.entry_price.ticks();
            let peak_drop_ticks = tracker.peak_bid.ticks() - bid.ticks();
            let fair_minus_bid_ticks = fair_ticks.map(|fair| fair - bid.ticks());
            let fair_supports_hold = fair_ticks
                .map(|fair| fair > bid.ticks() + self.exit_edge_ticks && opposes != Some(true))
                .unwrap_or(false);
            let fair_value_exit = fair_ticks
                .map(|fair| fair <= bid.ticks() + self.exit_edge_ticks && opposes == Some(true))
                .unwrap_or(false);
            let fair_boundary_exit = fair_ticks
                .map(|fair| fair <= bid.ticks() + self.exit_edge_ticks)
                .unwrap_or(true);
            let reason = if bid_drop_ticks >= self.exit_stop_ticks && !fair_supports_hold {
                Some(ExitReason::Stop)
            } else if fair_value_exit {
                if profit_ticks >= self.exit_arm_ticks && peak_drop_ticks >= self.exit_drop_ticks {
                    Some(ExitReason::Drop)
                } else {
                    Some(ExitReason::Value)
                }
            } else if hold_us >= self.exit_hold_us && fair_boundary_exit {
                Some(ExitReason::Hold)
            } else {
                None
            };
            if let Some(reason) = reason
                && let Some(plan) = plan_sell_at_bid(
                    &token,
                    &self.state,
                    &self.inventory,
                    self.sell_slippage_ticks,
                )
            {
                out.push(ExitDecision {
                    plan,
                    reason,
                    entry_price: tracker.entry_price,
                    entry_bid: tracker.entry_bid,
                    peak_bid: tracker.peak_bid,
                    bid,
                    fair_ticks,
                    fair_minus_bid_ticks,
                    opposes,
                    hold_us,
                });
            }
        }
        out
    }

    pub fn release_market_scope<'a>(
        &mut self,
        active_tokens: impl IntoIterator<Item = &'a TokenId>,
    ) {
        let active: HashSet<TokenId> = active_tokens.into_iter().cloned().collect();
        self.inventory.release_market_scope(active.iter());
        self.exit_trackers.retain(|token, _| active.contains(token));
    }
}

fn log_signal_blocked(intent: &BuyIntent, reason: &'static str, ewma_rtt_us: i64) {
    logline::log_event(
        Level::Info,
        "signal_blocked",
        &[
            Field {
                key: "reason",
                value: &reason,
            },
            Field {
                key: "side",
                value: &intent.side.as_str(),
            },
            Field {
                key: "token_id",
                value: &intent.token.as_str(),
            },
            Field {
                key: "limit_ticks",
                value: &intent.limit.ticks(),
            },
            Field {
                key: "edge_ticks",
                value: &intent.edge_ticks,
            },
            Field {
                key: "ewma_rtt_us",
                value: &ewma_rtt_us,
            },
        ],
    );
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

pub fn expire_stale_entry_claims(inventory: &mut Inventory, now_ts_us: i64) {
    inventory.expire_unknowns(now_ts_us.saturating_sub(UNKNOWN_SUBMIT_EXPIRE_US));
    inventory.expire_pending(now_ts_us.saturating_sub(PENDING_SUBMIT_EXPIRE_US));
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
    let (price, size) = canonical_sell_params(limit, position.sellable.units() * 100);
    if size.units() <= 0 {
        return None;
    }
    Some(SellPlan {
        token: token.clone(),
        price,
        size,
    })
}
