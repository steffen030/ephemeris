//! End-to-end ink latency tracing (`EPHEMERIS_INK_TRACE=1`).
//!
//! Structured log lines (parsed by `scripts/ink-latency-report.py`):
//!
//! ```text
//! ink_trace stage=drained t_us=123 age_us=450
//! ink_trace stage=feed_done t_us=123 age_us=900
//! ink_trace stage=present t_us=123 sample_to_present_us=8500 blit_us=2000
//! ink_trace stats n=120 p50_us=8000 p95_us=15000 max_us=42000 window_ms=2000
//! ```
//!
//! `t_us` is the evdev kernel timestamp (unix micros). Ages / latencies are
//! wall-clock deltas from that stamp (or blit duration for `blit_us`).

use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// True when `EPHEMERIS_INK_TRACE` is set in the environment (cached).
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("EPHEMERIS_INK_TRACE").is_some())
}

fn now_unix_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// Collects per-sample timings and emits p50/p95/max every ~2 s while drawing.
#[derive(Debug)]
pub struct InkLatencyTrace {
    pending: Vec<u64>,
    samples_us: Vec<u64>,
    last_report: Instant,
    report_every: Duration,
}

impl Default for InkLatencyTrace {
    fn default() -> Self {
        Self::new()
    }
}

impl InkLatencyTrace {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            samples_us: Vec::new(),
            last_report: Instant::now(),
            report_every: Duration::from_secs(2),
        }
    }

    /// Kernel sample reached the UI thread drain.
    pub fn on_drained(&mut self, t_us: u64) {
        if !enabled() || t_us == 0 {
            return;
        }
        let age = now_unix_us().saturating_sub(t_us);
        log::info!("ink_trace stage=drained t_us={t_us} age_us={age}");
    }

    /// `feed_input` finished for this sample; queue for the next present.
    pub fn on_feed_done(&mut self, t_us: u64) {
        if !enabled() || t_us == 0 {
            return;
        }
        let age = now_unix_us().saturating_sub(t_us);
        log::info!("ink_trace stage=feed_done t_us={t_us} age_us={age}");
        self.pending.push(t_us);
    }

    /// Softbuffer / Wayland present returned. `blit_us` is the present path cost.
    pub fn on_present(&mut self, blit_us: u64) {
        if !enabled() {
            return;
        }
        let now = now_unix_us();
        for t_us in self.pending.drain(..) {
            let latency = now.saturating_sub(t_us);
            log::info!(
                "ink_trace stage=present t_us={t_us} sample_to_present_us={latency} blit_us={blit_us}"
            );
            self.samples_us.push(latency);
        }
        self.maybe_report();
    }

    fn maybe_report(&mut self) {
        if self.samples_us.is_empty() || self.last_report.elapsed() < self.report_every {
            return;
        }
        let window_ms = self.last_report.elapsed().as_millis();
        self.last_report = Instant::now();
        let mut sorted = std::mem::take(&mut self.samples_us);
        sorted.sort_unstable();
        let n = sorted.len();
        let p50 = percentile_us(&sorted, 50);
        let p95 = percentile_us(&sorted, 95);
        let max = *sorted.last().unwrap_or(&0);
        log::info!(
            "ink_trace stats n={n} p50_us={p50} p95_us={p95} max_us={max} window_ms={window_ms}"
        );
    }
}

/// Inclusive percentile over a non-empty sorted slice (`p` in 0..=100).
pub fn percentile_us(sorted: &[u64], p: u8) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let p = u32::from(p.min(100));
    let idx = ((p as usize) * (sorted.len() - 1)) / 100;
    sorted[idx]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_p50_p95_max() {
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile_us(&v, 50), 50);
        assert_eq!(percentile_us(&v, 95), 95);
        assert_eq!(percentile_us(&v, 100), 100);
        assert_eq!(percentile_us(&[], 50), 0);
    }
}
