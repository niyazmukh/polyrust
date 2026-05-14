//! Pure BUY-intent model.
//!
//! Absence of a BUY is `None`, not a runtime event. Logging/replay can sample
//! counters elsewhere; the hot path needs only a buy intent or no work.

use std::collections::VecDeque;

use crate::logline::{self, Field, Level};
use crate::state::{MarketContext, Quote};
use crate::types::{OutcomeSide, PriceTick, TokenId, TsUs};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BinanceSample {
    pub ts_us: TsUs,
    pub update_id: i64,
    pub bid: f64,
    pub ask: f64,
    pub bid_qty: f64,
    pub ask_qty: f64,
    pub microprice: f64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuyIntent {
    pub side: OutcomeSide,
    pub token: TokenId,
    pub limit: PriceTick,
    pub edge_price: PriceTick,
    pub edge_ticks: i32,
}

#[derive(Clone, Copy, Debug)]
pub struct SignalConfig {
    pub max_lag_us: i64,
    pub min_window_us: i64,
    pub max_window_us: i64,
    pub max_spread_usd: f64,
    pub min_move_usd: f64,
    pub min_abs_ofi: f64,
    pub min_imbalance: f64,
    pub min_total_qty: f64,
    pub min_edge_ticks: i32,
    pub entry_slippage_ticks: i32,
    pub max_quote_age_us: i64,
    pub min_tte_us: i64,
    pub min_buy_limit: PriceTick,
    pub max_buy_limit: PriceTick,
    pub prob_sigma_floor_usd: f64,
    pub prob_sigma_scale: f64,
    pub prob_floor: f64,
    pub prob_ceil: f64,
    pub max_samples: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct SignalPoint {
    ts_us: TsUs,
    microprice: f64,
    ofi: f64,
    imbalance: f64,
}

pub struct SignalEngine {
    cfg: SignalConfig,
    strike: f64,
    samples: VecDeque<SignalPoint>,
    last_update_id: i64,
    last_bid: f64,
    last_ask: f64,
    last_bid_qty: f64,
    last_ask_qty: f64,
}

impl SignalEngine {
    pub fn new(cfg: SignalConfig) -> Self {
        let max_samples = cfg.max_samples.max(8);
        Self {
            cfg: SignalConfig { max_samples, ..cfg },
            strike: 0.0,
            samples: VecDeque::with_capacity(max_samples),
            last_update_id: 0,
            last_bid: 0.0,
            last_ask: 0.0,
            last_bid_qty: 0.0,
            last_ask_qty: 0.0,
        }
    }

    pub fn set_strike(&mut self, strike: f64, reset_window: bool) {
        self.strike = strike;
        if reset_window {
            self.samples.clear();
            self.last_update_id = 0;
            self.last_bid = 0.0;
            self.last_ask = 0.0;
            self.last_bid_qty = 0.0;
            self.last_ask_qty = 0.0;
        }
    }

    pub fn push(&mut self, sample: BinanceSample) -> bool {
        if !sample.microprice.is_finite()
            || sample.microprice <= 0.0
            || sample.update_id <= self.last_update_id
            || sample.bid <= 0.0
            || sample.ask <= 0.0
            || sample.ask < sample.bid
            || sample.bid_qty <= 0.0
            || sample.ask_qty <= 0.0
        {
            return false;
        }
        let total_qty = sample.bid_qty + sample.ask_qty;
        if !total_qty.is_finite() || total_qty < self.cfg.min_total_qty {
            return false;
        }
        let spread = sample.ask - sample.bid;
        if self.cfg.max_spread_usd > 0.0 && spread > self.cfg.max_spread_usd {
            return false;
        }
        if let Some(last) = self.samples.back()
            && sample.ts_us <= last.ts_us
        {
            return false;
        }
        let imbalance = (sample.bid_qty - sample.ask_qty) / total_qty;
        let ofi = self.compute_ofi(sample);
        while self.samples.len() >= self.cfg.max_samples {
            self.samples.pop_front();
        }
        self.samples.push_back(SignalPoint {
            ts_us: sample.ts_us,
            microprice: sample.microprice,
            ofi,
            imbalance,
        });
        self.last_update_id = sample.update_id;
        self.last_bid = sample.bid;
        self.last_ask = sample.ask;
        self.last_bid_qty = sample.bid_qty;
        self.last_ask_qty = sample.ask_qty;
        true
    }

    pub fn on_sample(
        &mut self,
        sample: BinanceSample,
        market: &MarketContext,
        yes_quote: Quote,
        no_quote: Quote,
        now: TsUs,
        tte_us: i64,
    ) -> Option<BuyIntent> {
        let lag_us = now.micros() - sample.ts_us.micros();
        if self.cfg.max_lag_us > 0 && lag_us > self.cfg.max_lag_us {
            return None;
        }
        if !self.push(sample) {
            return None;
        }
        self.decide_latest(market, yes_quote, no_quote, now, tte_us)
    }

    pub fn decide_latest(
        &self,
        market: &MarketContext,
        yes_quote: Quote,
        no_quote: Quote,
        now: TsUs,
        tte_us: i64,
    ) -> Option<BuyIntent> {
        if self.strike <= 0.0 || tte_us < self.cfg.min_tte_us {
            return None;
        }
        let (latest, window_base) = self.latest_window()?;

        let move_usd = latest.microprice - window_base.microprice;
        if move_usd.abs() < self.cfg.min_move_usd {
            return None;
        }
        let side = if move_usd > 0.0 {
            OutcomeSide::Yes
        } else {
            OutcomeSide::No
        };
        let ofi_sum = self.ofi_sum_since(latest.ts_us.micros() - self.cfg.max_window_us);
        match side {
            OutcomeSide::Yes => {
                if ofi_sum < self.cfg.min_abs_ofi || latest.imbalance < self.cfg.min_imbalance {
                    return None;
                }
            }
            OutcomeSide::No => {
                if ofi_sum > -self.cfg.min_abs_ofi || latest.imbalance > -self.cfg.min_imbalance {
                    return None;
                }
            }
        }
        let quote = match side {
            OutcomeSide::Yes => yes_quote,
            OutcomeSide::No => no_quote,
        };
        let token = match side {
            OutcomeSide::Yes => market.yes_token.clone(),
            OutcomeSide::No => market.no_token.clone(),
        };

        let ask = match quote.ask {
            Some(a) => a,
            None => {
                Self::debug_reject("no_ask", move_usd, ofi_sum, latest.imbalance, 0, 0);
                return None;
            }
        };
        let quote_age = now.micros() - quote.ts_us.micros();
        if quote_age < 0 || quote_age > self.cfg.max_quote_age_us {
            Self::debug_reject(
                "quote_stale",
                move_usd,
                ofi_sum,
                latest.imbalance,
                quote_age,
                0,
            );
            return None;
        }

        let limit =
            PriceTick::checked(ask.ticks().checked_add(self.cfg.entry_slippage_ticks)?).ok()?;
        if limit < self.cfg.min_buy_limit || limit > self.cfg.max_buy_limit {
            Self::debug_reject(
                "limit_band",
                move_usd,
                ofi_sum,
                latest.imbalance,
                quote_age,
                limit.ticks(),
            );
            return None;
        }
        let edge_slippage_ticks = (self.cfg.entry_slippage_ticks + 1) / 2;
        let edge_price = PriceTick::checked(ask.ticks().checked_add(edge_slippage_ticks)?).ok()?;

        let p_yes = self.prob_yes(latest.microprice, latest.ts_us, tte_us)?;
        let side_prob_ticks = match side {
            OutcomeSide::Yes => p_yes * 100.0,
            OutcomeSide::No => (1.0 - p_yes) * 100.0,
        };
        let edge_ticks_f = side_prob_ticks - f64::from(edge_price.ticks());
        let edge_ticks = edge_ticks_f.floor() as i32;
        let live_spread_ticks = quote
            .bid
            .map(|bid| edge_price.ticks().saturating_sub(bid.ticks()))
            .unwrap_or(0)
            .max(0);
        let min_edge_ticks = self.cfg.min_edge_ticks.max(live_spread_ticks);
        if edge_ticks < min_edge_ticks {
            Self::debug_reject(
                "edge_low",
                move_usd,
                ofi_sum,
                latest.imbalance,
                edge_ticks as i64,
                min_edge_ticks,
            );
            return None;
        }

        Some(BuyIntent {
            side,
            token,
            limit,
            edge_price,
            edge_ticks,
        })
    }

    pub fn fair_ticks_for_side(&self, side: OutcomeSide, now: TsUs, tte_us: i64) -> Option<i32> {
        let (latest, _) = self.latest_window()?;
        let lag_us = now.micros() - latest.ts_us.micros();
        if self.cfg.max_lag_us > 0 && lag_us > self.cfg.max_lag_us {
            return None;
        }
        let p_yes = self.prob_yes(latest.microprice, latest.ts_us, tte_us)?;
        let side_prob_ticks = match side {
            OutcomeSide::Yes => p_yes * 100.0,
            OutcomeSide::No => (1.0 - p_yes) * 100.0,
        };
        Some(side_prob_ticks.floor() as i32)
    }

    fn debug_reject(gate: &str, move_usd: f64, ofi: f64, imbalance: f64, a: i64, b: i32) {
        logline::log_event(
            Level::Debug,
            "signal_reject",
            &[
                Field {
                    key: "gate",
                    value: &gate,
                },
                Field {
                    key: "move_usd",
                    value: &move_usd,
                },
                Field {
                    key: "ofi",
                    value: &ofi,
                },
                Field {
                    key: "imbalance",
                    value: &imbalance,
                },
                Field {
                    key: "a",
                    value: &a,
                },
                Field {
                    key: "b",
                    value: &(b as i64),
                },
            ],
        );
    }

    fn compute_ofi(&self, sample: BinanceSample) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let bid_flow = (if sample.bid >= self.last_bid {
            sample.bid_qty
        } else {
            0.0
        }) - if sample.bid <= self.last_bid {
            self.last_bid_qty
        } else {
            0.0
        };
        let ask_flow = (if sample.ask >= self.last_ask {
            self.last_ask_qty
        } else {
            0.0
        }) - if sample.ask <= self.last_ask {
            sample.ask_qty
        } else {
            0.0
        };
        bid_flow + ask_flow
    }

    fn oldest_since(&self, cutoff_us: i64) -> Option<SignalPoint> {
        self.samples
            .iter()
            .copied()
            .find(|sample| sample.ts_us.micros() >= cutoff_us)
    }

    fn latest_window(&self) -> Option<(SignalPoint, SignalPoint)> {
        let latest = *self.samples.back()?;
        let window_base = self.oldest_since(latest.ts_us.micros() - self.cfg.max_window_us)?;
        let window_us = latest.ts_us.micros() - window_base.ts_us.micros();
        if window_base.ts_us == latest.ts_us || window_us < self.cfg.min_window_us {
            return None;
        }
        Some((latest, window_base))
    }

    fn ofi_sum_since(&self, cutoff_us: i64) -> f64 {
        self.samples
            .iter()
            .filter(|sample| sample.ts_us.micros() >= cutoff_us)
            .map(|sample| sample.ofi)
            .sum()
    }

    fn prob_yes(&self, microprice: f64, latest_ts: TsUs, tte_us: i64) -> Option<f64> {
        if !microprice.is_finite() || microprice <= 0.0 || self.strike <= 0.0 || tte_us <= 0 {
            return None;
        }
        let sigma_px = self
            .realized_sigma_since(latest_ts.micros() - self.cfg.max_window_us)
            .max(self.cfg.prob_sigma_floor_usd);
        let sigma_eff =
            self.cfg.prob_sigma_scale * sigma_px * ((tte_us as f64) / 1_000_000.0).sqrt();
        if !sigma_eff.is_finite() || sigma_eff <= 1e-9 {
            return None;
        }
        let z = (microprice - self.strike) / sigma_eff;
        if !z.is_finite() {
            return None;
        }
        let floor = self.cfg.prob_floor.clamp(0.0, 1.0);
        let ceil = self.cfg.prob_ceil.clamp(floor, 1.0);
        Some(phi(z).clamp(floor, ceil))
    }

    fn realized_sigma_since(&self, cutoff_us: i64) -> f64 {
        if self.samples.len() < 2 {
            return 0.0;
        }
        let mut prev: Option<SignalPoint> = None;
        let mut sum_sq = 0.0;
        let mut total_dt_s = 0.0;
        for sample in self
            .samples
            .iter()
            .copied()
            .filter(|sample| sample.ts_us.micros() >= cutoff_us)
        {
            if let Some(p) = prev {
                let dt_s =
                    ((sample.ts_us.micros() - p.ts_us.micros()).max(1_000) as f64) / 1_000_000.0;
                let dp = sample.microprice - p.microprice;
                sum_sq += dp * dp;
                total_dt_s += dt_s;
            }
            prev = Some(sample);
        }
        if total_dt_s > 0.0 {
            (sum_sq / total_dt_s).sqrt()
        } else {
            0.0
        }
    }
}

fn phi(z: f64) -> f64 {
    if z >= 0.0 {
        1.0 - phi_tail(z)
    } else {
        phi_tail(-z)
    }
}

fn phi_tail(z: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.231_641_9 * z);
    let poly = (((((1.330_274_429 * t - 1.821_255_978) * t) + 1.781_477_937) * t - 0.356_563_782)
        * t
        + 0.319_381_530)
        * t;
    let pdf = (-0.5 * z * z).exp() * 0.398_942_280_401_432_7;
    pdf * poly
}
