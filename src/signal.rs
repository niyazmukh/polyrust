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
    pub entry_notional_cents: i64,
    pub max_quote_age_us: i64,
    pub min_tte_us: i64,
    pub min_buy_limit: PriceTick,
    pub max_buy_limit: PriceTick,
    pub prob_sigma_floor_usd: f64,
    pub prob_sigma_scale: f64,
    pub prob_floor: f64,
    pub prob_ceil: f64,
    pub max_samples: usize,
    /// When `true`, fold the Polymarket-implied probability (book BBO mid,
    /// normalised across YES/NO) into the `prob_yes` σ estimate by taking the
    /// larger of realised-σ and Polymarket-implied-σ. Acts as a "trust the
    /// book" guard against overconfident Binance-only fair-value calls.
    pub use_implied_sigma: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct SignalPoint {
    ts_us: TsUs,
    recv_ts_us: TsUs,
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
        self.push_received(sample, sample.ts_us)
    }

    fn push_received(&mut self, sample: BinanceSample, recv_ts_us: TsUs) -> bool {
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
            recv_ts_us,
            microprice: sample.microprice,
            ofi,
            imbalance,
        });
        self.last_update_id = sample.update_id;
        self.last_bid = sample.bid;
        self.last_ask = sample.ask;
        self.last_bid_qty = sample.bid_qty;
        self.last_ask_qty = sample.ask_qty;
        if logline::enabled(Level::Debug) {
            let src_ts_us = sample.ts_us.micros();
            logline::log_event(
                Level::Debug,
                "binance_sample",
                &[
                    Field {
                        key: "src_ts_us",
                        value: &src_ts_us,
                    },
                    Field {
                        key: "update_id",
                        value: &sample.update_id,
                    },
                    Field {
                        key: "bid",
                        value: &sample.bid,
                    },
                    Field {
                        key: "ask",
                        value: &sample.ask,
                    },
                    Field {
                        key: "bid_qty",
                        value: &sample.bid_qty,
                    },
                    Field {
                        key: "ask_qty",
                        value: &sample.ask_qty,
                    },
                    Field {
                        key: "microprice",
                        value: &sample.microprice,
                    },
                    Field {
                        key: "ofi",
                        value: &ofi,
                    },
                    Field {
                        key: "imbalance",
                        value: &imbalance,
                    },
                ],
            );
        }
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
        if !self.push_received(sample, now) {
            return None;
        }
        if self.cfg.max_lag_us > 0 && lag_us > self.cfg.max_lag_us {
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

        let entry_ask = match quote.buy_cutoff_for_cents(self.cfg.entry_notional_cents) {
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

        let limit = PriceTick::checked(
            entry_ask
                .ticks()
                .checked_add(self.cfg.entry_slippage_ticks)?,
        )
        .ok()?;
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
        let edge_price =
            PriceTick::checked(entry_ask.ticks().checked_add(edge_slippage_ticks)?).ok()?;

        let implied_p_yes_hint = if self.cfg.use_implied_sigma {
            implied_p_yes_for(yes_quote, no_quote)
        } else {
            None
        };
        let p_yes = self.prob_yes(latest.microprice, latest.ts_us, tte_us, implied_p_yes_hint)?;
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

    pub fn fair_ticks_for_side(
        &self,
        side: OutcomeSide,
        now: TsUs,
        tte_us: i64,
        implied_p_yes_hint: Option<f64>,
    ) -> Option<i32> {
        let (latest, _) = self.latest_window()?;
        let lag_us = now.micros() - latest.recv_ts_us.micros();
        if lag_us < 0 || (self.cfg.max_lag_us > 0 && lag_us > self.cfg.max_lag_us) {
            return None;
        }
        let hint = if self.cfg.use_implied_sigma {
            implied_p_yes_hint
        } else {
            None
        };
        let p_yes = self.prob_yes(latest.microprice, latest.ts_us, tte_us, hint)?;
        let side_prob = match side {
            OutcomeSide::Yes => p_yes,
            OutcomeSide::No => 1.0 - p_yes,
        };
        Some((side_prob * 100.0).floor() as i32)
    }

    pub fn opposes_side(&self, side: OutcomeSide, now: TsUs) -> Option<bool> {
        let (latest, window_base) = self.latest_window()?;
        let lag_us = now.micros() - latest.recv_ts_us.micros();
        if lag_us < 0 || (self.cfg.max_lag_us > 0 && lag_us > self.cfg.max_lag_us) {
            return None;
        }
        let sign = match side {
            OutcomeSide::Yes => 1.0,
            OutcomeSide::No => -1.0,
        };
        let signed_move = sign * (latest.microprice - window_base.microprice);
        let signed_ofi = sign * self.ofi_sum_since(latest.ts_us.micros() - self.cfg.max_window_us);
        let signed_imbalance = sign * latest.imbalance;
        Some(
            signed_move <= -self.cfg.min_move_usd
                && signed_ofi <= -self.cfg.min_abs_ofi
                && signed_imbalance <= -self.cfg.min_imbalance,
        )
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

    fn prob_yes(
        &self,
        microprice: f64,
        latest_ts: TsUs,
        tte_us: i64,
        implied_p_yes_hint: Option<f64>,
    ) -> Option<f64> {
        if !microprice.is_finite() || microprice <= 0.0 || self.strike <= 0.0 || tte_us <= 0 {
            return None;
        }
        let realized_px = self
            .realized_sigma_since(latest_ts.micros() - self.cfg.max_window_us)
            .max(self.cfg.prob_sigma_floor_usd);
        let scale_to_eff = self.cfg.prob_sigma_scale * ((tte_us as f64) / 1_000_000.0).sqrt();
        let realized_eff = realized_px * scale_to_eff;
        let drift = microprice - self.strike;
        // Polymarket-implied σ: solve drift = z · σ for σ, where z = Φ⁻¹(p̂).
        // Only accept positive σ aligned with the sign of `drift`; an implied
        // probability on the other side of the strike would yield a non-sensical
        // negative σ (drift and z disagree) — fall back to realised in that case.
        let implied_eff = implied_p_yes_hint.and_then(|p| {
            let p = p.clamp(1e-6, 1.0 - 1e-6);
            let z = phi_inv(p);
            if !z.is_finite() || z.abs() < 1e-6 {
                return None;
            }
            let s = drift / z;
            if s.is_finite() && s > 0.0 {
                Some(s)
            } else {
                None
            }
        });
        let sigma_eff = match implied_eff {
            Some(implied) => realized_eff.max(implied),
            None => realized_eff,
        };
        if !sigma_eff.is_finite() || sigma_eff <= 1e-9 {
            return None;
        }
        let z = drift / sigma_eff;
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

/// Inverse standard-normal CDF via Acklam's rational approximation
/// (max relative error ~1.15 · 10⁻⁹). Used to derive Polymarket-implied σ.
fn phi_inv(p: f64) -> f64 {
    if !(0.0 < p && p < 1.0) {
        return f64::NAN;
    }
    const A: [f64; 6] = [
        -3.969_683_028_665_376e1,
        2.209_460_984_245_205e2,
        -2.759_285_104_469_687e2,
        1.383_577_518_672_69e2,
        -3.066_479_806_614_716e1,
        2.506_628_277_459_239,
    ];
    const B: [f64; 5] = [
        -5.447_609_879_822_406e1,
        1.615_858_368_580_409e2,
        -1.556_989_798_598_866e2,
        6.680_131_188_771_972e1,
        -1.328_068_155_288_572e1,
    ];
    const C: [f64; 6] = [
        -7.784_894_002_430_293e-3,
        -3.223_964_580_411_365e-1,
        -2.400_758_277_161_838,
        -2.549_732_539_343_734,
        4.374_664_141_464_968,
        2.938_163_982_698_783,
    ];
    const D: [f64; 4] = [
        7.784_695_709_041_462e-3,
        3.224_671_290_700_398e-1,
        2.445_134_137_142_996,
        3.754_408_661_907_416,
    ];
    let p_low = 0.024_25;
    let p_high = 1.0 - p_low;
    if p < p_low {
        let q = (-2.0 * p.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if p <= p_high {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    } else {
        let q = (-2.0 * (1.0 - p).ln()).sqrt();
        -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    }
}

/// Polymarket-implied P(YES) from the two-legged BBO. Normalises the YES and
/// NO mids so they sum to one (the spread carve-out the venue charges both
/// sides). Returns `None` if neither leg has both a bid and an ask.
///
/// Public so the runtime exit path can feed the same hint into
/// `fair_ticks_for_side` as the entry path passes to `prob_yes`.
pub fn implied_p_yes_for(yes_quote: Quote, no_quote: Quote) -> Option<f64> {
    fn mid(q: Quote) -> Option<f64> {
        let bid = q.bid?.ticks();
        let ask = q.ask?.ticks();
        if bid <= 0 || ask <= 0 || ask < bid {
            return None;
        }
        Some(f64::from(bid + ask) * 0.5 / 100.0)
    }
    match (mid(yes_quote), mid(no_quote)) {
        (Some(y), Some(n)) => {
            let total = y + n;
            if total > 1e-9 && total.is_finite() {
                Some((y / total).clamp(1e-6, 1.0 - 1e-6))
            } else {
                None
            }
        }
        (Some(y), None) => Some(y.clamp(1e-6, 1.0 - 1e-6)),
        (None, Some(n)) => Some((1.0 - n).clamp(1e-6, 1.0 - 1e-6)),
        (None, None) => None,
    }
}
