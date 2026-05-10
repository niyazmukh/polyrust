// Delivered by DeepSeek.
//! Anchor strike resolution from Binance microprice samples.
//!
//! Maintains a rolling buffer of (timestamp_us, microprice) tuples and
//! computes the median strike on market rotation. Handles both normal
//! (window-based) and late-discovery (earliest-available) paths.
//!
//! Traces to:
//!   bot_orchestrator.py:592-658  (_anchor_strike_on_rotation, _try_resolve_pending_anchor)
//!   bot_orchestrator.py:660      (_append_anchor_sample)
//!   shadow_signal_probe.py:450-497 (late-discovery fallback)

use std::collections::VecDeque;

/// Window past `slug_ts` for collecting microprice samples.
/// Traces to: bot_orchestrator.py:21 (ANCHOR_WINDOW_END_US = 300_000).
const ANCHOR_WINDOW_END_US: i64 = 300_000;

/// Maximum age for retained samples before trimming.
/// Traces to: bot_orchestrator.py:27 (ANCHOR_BUFFER_HORIZON_US = 10_000_000).
const ANCHOR_BUFFER_HORIZON_US: i64 = 10_000_000;

/// Minimum samples required before strike can be computed.
/// Traces to: bot_orchestrator.py:32 (MIN_ANCHOR_SAMPLES = 3).
const MIN_ANCHOR_SAMPLES: usize = 3;

/// Threshold in microseconds past the window end beyond which we
/// use the late-discovery fallback.
/// Traces to: shadow_signal_probe.py:462 (late_discovery check).
const LATE_DISCOVERY_THRESHOLD_US: i64 = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AnchorResult {
    /// Not enough samples yet; retry on the next Binance tick.
    Pending,
    /// Window closed without enough samples.
    Failed,
    /// Strike resolved to this dollar value.
    Resolved(f64),
}

pub struct AnchorBuffer {
    /// (timestamp_us, microprice) pairs, kept in chronological order.
    samples: VecDeque<(i64, f64)>,
    /// The slug_ts we are currently trying to resolve.
    pending_slug_ts: i64,
    /// Highest timestamp seen (used for trimming).
    max_ts_seen: i64,
}

impl AnchorBuffer {
    pub fn new() -> Self {
        Self {
            samples: VecDeque::new(),
            pending_slug_ts: 0,
            max_ts_seen: 0,
        }
    }

    /// Push a Binance tick into the buffer. Computes microprice internally.
    /// Trims entries older than `max_ts_seen - ANCHOR_BUFFER_HORIZON_US`.
    ///
    /// Traces to: bot_orchestrator.py:660 (`_append_anchor_sample`).
    pub fn push(&mut self, ts_us: i64, bid: f64, ask: f64, bid_qty: f64, ask_qty: f64) {
        let denom = bid_qty + ask_qty;
        if denom <= 0.0 || !denom.is_finite() {
            return;
        }
        let microprice = (bid * ask_qty + ask * bid_qty) / denom;
        if !microprice.is_finite() || microprice <= 0.0 {
            return;
        }
        if ts_us <= self.max_ts_seen {
            return; // out-of-order or duplicate
        }
        self.max_ts_seen = ts_us;
        self.samples.push_back((ts_us, microprice));
        self.trim();
    }

    /// Set the pending anchor target. Called when a new market context arrives.
    ///
    /// Traces to: bot_orchestrator.py:620.
    pub fn set_pending(&mut self, slug_ts: i64) {
        self.pending_slug_ts = slug_ts;
    }

    /// Try to resolve the strike. Call on every Binance tick and on market
    /// rotation.
    ///
    /// Normal path: collects `MIN_ANCHOR_SAMPLES` or more samples in the
    /// window `[slug_ts * 1e6, slug_ts * 1e6 + 300ms]`. If the window is
    /// still open and we have < 3 samples → Pending. If expired → Failed.
    ///
    /// Late-discovery fallback: if `now_us` is more than 1s past the window
    /// end, use any samples with `ts >= slug_ts * 1e6` (no upper bound).
    /// This handles the common probe case where Gamma discovery fires
    /// seconds after the epoch start.
    ///
    /// Traces to: bot_orchestrator.py:623-658,
    ///   shadow_signal_probe.py:462-470 (late_discovery).
    pub fn try_resolve(&mut self, now_us: i64) -> AnchorResult {
        if self.pending_slug_ts <= 0 {
            return AnchorResult::Pending;
        }

        let window_start_us = self.pending_slug_ts.saturating_mul(1_000_000);
        let window_end_us = window_start_us.saturating_add(ANCHOR_WINDOW_END_US);
        let late_discovery = now_us > window_end_us.saturating_add(LATE_DISCOVERY_THRESHOLD_US);

        let samples: Vec<f64> = if late_discovery {
            // Late-discovery: use all samples at or after window start.
            self.samples
                .iter()
                .filter(|(ts, _)| *ts >= window_start_us)
                .map(|(_, micro)| *micro)
                .collect()
        } else {
            // Normal path: strict window.
            self.samples
                .iter()
                .filter(|(ts, _)| *ts >= window_start_us && *ts <= window_end_us)
                .map(|(_, micro)| *micro)
                .collect()
        };

        if samples.len() < MIN_ANCHOR_SAMPLES {
            // Window closed if we're past the end OR in late-discovery mode.
            // Uses `now_us` (not `max_ts_seen`) because ticks may stop arriving;
            // `now_us` always advances so we eventually fail instead of
            // pending forever.
            if late_discovery || now_us > window_end_us {
                return AnchorResult::Failed;
            }
            return AnchorResult::Pending;
        }

        // Compute median: sort, pick middle (odd) or mean of two middle (even).
        let strike = median(&samples);
        self.pending_slug_ts = 0;
        AnchorResult::Resolved(strike)
    }

    /// Trim entries older than `max_ts_seen - ANCHOR_BUFFER_HORIZON_US`.
    fn trim(&mut self) {
        let cutoff = self.max_ts_seen.saturating_sub(ANCHOR_BUFFER_HORIZON_US);
        while let Some(&(ts, _)) = self.samples.front() {
            if ts < cutoff {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }
}

impl Default for AnchorBuffer {
    fn default() -> Self {
        Self::new()
    }
}

fn median(vals: &[f64]) -> f64 {
    let mut sorted: Vec<f64> = vals.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn push_samples(buf: &mut AnchorBuffer, base_ts_us: i64, count: usize, step_us: i64) {
        for i in 0..count {
            let ts = base_ts_us + (i as i64) * step_us;
            // Simulate a drifting microprice to get distinct samples.
            let px = 100.0 + (i as f64) * 0.1;
            buf.push(ts, px - 1.0, px + 1.0, 1.0, 1.0); // microprice ≈ px
        }
    }

    #[test]
    fn normal_resolution_with_exactly_3_samples() {
        let mut buf = AnchorBuffer::new();
        let slug_ts = 1_777_000_000;
        let window_start_us = slug_ts * 1_000_000;

        push_samples(&mut buf, window_start_us + 50_000, 3, 100_000);

        buf.set_pending(slug_ts);
        let result = buf.try_resolve(window_start_us + 350_000);
        match result {
            AnchorResult::Resolved(strike) => {
                // Samples at 100.0, 100.1, 100.2 → median = 100.1
                assert!((strike - 100.1).abs() < 0.001, "strike={strike}");
            }
            other => panic!("expected Resolved, got {other:?}"),
        }
    }

    #[test]
    fn normal_resolution_with_4_samples_uses_mean_of_middle_two() {
        let mut buf = AnchorBuffer::new();
        let slug_ts = 1_777_000_000;
        let window_start_us = slug_ts * 1_000_000;

        // 4 samples within the 300ms window (step 50ms → 200ms total).
        push_samples(&mut buf, window_start_us + 50_000, 4, 50_000);

        buf.set_pending(slug_ts);
        let result = buf.try_resolve(window_start_us + 500_000);
        match result {
            AnchorResult::Resolved(strike) => {
                // 100.0, 100.1, 100.2, 100.3 → median = (100.1 + 100.2)/2 = 100.15
                assert!((strike - 100.15).abs() < 0.001, "strike={strike}");
            }
            other => panic!("expected Resolved, got {other:?}"),
        }
    }

    #[test]
    fn pending_when_window_open_and_fewer_than_3() {
        let mut buf = AnchorBuffer::new();
        let slug_ts = 1_777_000_000;
        let window_start_us = slug_ts * 1_000_000;

        push_samples(&mut buf, window_start_us + 50_000, 2, 100_000);

        buf.set_pending(slug_ts);
        // Only 200ms elapsed, window still open (300ms window).
        let result = buf.try_resolve(window_start_us + 200_000);
        assert_eq!(result, AnchorResult::Pending);
    }

    #[test]
    fn failed_when_window_closed_with_fewer_than_3() {
        let mut buf = AnchorBuffer::new();
        let slug_ts = 1_777_000_000;
        let window_start_us = slug_ts * 1_000_000;

        push_samples(&mut buf, window_start_us + 50_000, 2, 100_000);

        buf.set_pending(slug_ts);
        // Window closed (400ms > 300ms window).
        let result = buf.try_resolve(window_start_us + 400_000);
        assert_eq!(result, AnchorResult::Failed);
    }

    #[test]
    fn late_discovery_uses_all_samples_after_window_start() {
        let mut buf = AnchorBuffer::new();
        let slug_ts = 1_777_000_000;
        let window_start_us = slug_ts * 1_000_000;

        // Samples spread across 2 seconds (well past 300ms window).
        push_samples(&mut buf, window_start_us + 100_000, 5, 500_000);

        buf.set_pending(slug_ts);
        // Now is 3s past window end → late discovery threshold triggered.
        let result = buf.try_resolve(window_start_us + ANCHOR_WINDOW_END_US + 3_000_000);
        match result {
            AnchorResult::Resolved(strike) => {
                // 5 samples, median is 3rd: 100.2
                assert!((strike - 100.2).abs() < 0.001, "strike={strike}");
            }
            other => panic!("expected Resolved via late-discovery, got {other:?}"),
        }
    }

    #[test]
    fn trims_old_samples_past_horizon() {
        let mut buf = AnchorBuffer::new();
        // Push a sample at t=0.
        buf.push(1_000_000, 99.0, 101.0, 1.0, 1.0);
        assert_eq!(buf.samples.len(), 1);

        // Push a sample 11 seconds later (past 10s horizon).
        buf.push(12_000_000, 99.0, 101.0, 1.0, 1.0);
        assert_eq!(buf.samples.len(), 1); // old one trimmed
    }

    #[test]
    fn rejects_out_of_order_ticks() {
        let mut buf = AnchorBuffer::new();
        buf.push(2_000_000, 99.0, 101.0, 1.0, 1.0);
        buf.push(1_000_000, 99.0, 101.0, 1.0, 1.0); // older — rejected
        assert_eq!(buf.samples.len(), 1);
    }

    #[test]
    fn no_pending_returns_pending() {
        let mut buf = AnchorBuffer::new();
        // Never called set_pending.
        assert_eq!(buf.try_resolve(1_000_000), AnchorResult::Pending);
    }
}
