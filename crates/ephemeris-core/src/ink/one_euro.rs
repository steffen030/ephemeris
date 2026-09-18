//! One-Euro filter for low-latency input smoothing.
//!
//! The One-Euro filter (Casiez, Roussel, Vogel, 2012) is an adaptive low-pass
//! filter: it smooths jitter aggressively when the pen moves slowly (where
//! jitter is most visible) and lets fast motion through with little lag (where
//! responsiveness matters).  This is exactly the trade-off wanted for pen ink
//! on an eink device.
//!
//! We run two independent [`OneEuroFilter`]s — one per axis — inside
//! [`OneEuroFilter2D`], driven by per-sample timestamps.

/// A first-order low-pass (exponential moving average) whose smoothing factor
/// is recomputed each step from the cutoff frequency and dt.
#[derive(Debug, Clone)]
struct LowPass {
    initialised: bool,
    prev: f32,
}

impl LowPass {
    fn new() -> Self {
        Self {
            initialised: false,
            prev: 0.0,
        }
    }

    fn filter(&mut self, x: f32, alpha: f32) -> f32 {
        let out = if self.initialised {
            alpha * x + (1.0 - alpha) * self.prev
        } else {
            self.initialised = true;
            x
        };
        self.prev = out;
        out
    }

    fn last(&self) -> f32 {
        self.prev
    }
}

/// Configuration for a One-Euro filter.
#[derive(Debug, Clone, Copy)]
pub struct OneEuroConfig {
    /// Minimum cutoff frequency (Hz).  Lower = smoother but laggier at rest.
    pub min_cutoff: f32,
    /// Speed coefficient.  Higher = less lag when moving fast.
    pub beta: f32,
    /// Cutoff for the derivative (speed) estimate (Hz).
    pub d_cutoff: f32,
}

impl Default for OneEuroConfig {
    fn default() -> Self {
        // Tuned for pen input sampled at ~100–200 Hz in logical pixels.
        Self {
            min_cutoff: 1.0,
            beta: 0.007,
            d_cutoff: 1.0,
        }
    }
}

/// Compute the EMA smoothing factor for cutoff `fc` (Hz) over interval `dt` (s).
fn alpha(fc: f32, dt: f32) -> f32 {
    let tau = 1.0 / (2.0 * std::f32::consts::PI * fc);
    1.0 / (1.0 + tau / dt)
}

/// One-Euro filter for a single scalar signal.
#[derive(Debug, Clone)]
pub struct OneEuroFilter {
    cfg: OneEuroConfig,
    x_lp: LowPass,
    dx_lp: LowPass,
    last_time_s: Option<f32>,
    has_prev_x: bool,
    prev_x: f32,
}

impl OneEuroFilter {
    pub fn new(cfg: OneEuroConfig) -> Self {
        Self {
            cfg,
            x_lp: LowPass::new(),
            dx_lp: LowPass::new(),
            last_time_s: None,
            has_prev_x: false,
            prev_x: 0.0,
        }
    }

    /// Filter sample `x` observed at absolute time `t_s` (seconds).
    ///
    /// Timestamps must be non-decreasing.  The first sample is returned
    /// unchanged (there is nothing to smooth against yet).
    pub fn filter(&mut self, x: f32, t_s: f32) -> f32 {
        // Derive dt from timestamps; fall back to a nominal 60 Hz if two
        // samples share a timestamp (avoids division by zero).
        let dt = match self.last_time_s {
            Some(prev) if t_s > prev => t_s - prev,
            _ => 1.0 / 60.0,
        };
        self.last_time_s = Some(t_s);

        // Estimate derivative (speed) and low-pass it.
        let dx = if self.has_prev_x {
            (x - self.prev_x) / dt
        } else {
            0.0
        };
        self.prev_x = x;
        self.has_prev_x = true;

        let edx = self.dx_lp.filter(dx, alpha(self.cfg.d_cutoff, dt));

        // Adapt the cutoff to the (smoothed) speed.
        let cutoff = self.cfg.min_cutoff + self.cfg.beta * edx.abs();
        self.x_lp.filter(x, alpha(cutoff, dt))
    }

    /// The most recently produced (filtered) value.
    pub fn last(&self) -> f32 {
        self.x_lp.last()
    }
}

/// A pair of One-Euro filters for 2D positions, sharing a config.
#[derive(Debug, Clone)]
pub struct OneEuroFilter2D {
    x: OneEuroFilter,
    y: OneEuroFilter,
}

impl OneEuroFilter2D {
    pub fn new(cfg: OneEuroConfig) -> Self {
        Self {
            x: OneEuroFilter::new(cfg),
            y: OneEuroFilter::new(cfg),
        }
    }

    /// Filter a `(x, y)` position observed at time `t_s` (seconds).
    pub fn filter(&mut self, x: f32, y: f32, t_s: f32) -> (f32, f32) {
        (self.x.filter(x, t_s), self.y.filter(y, t_s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sample_is_passthrough() {
        let mut f = OneEuroFilter::new(OneEuroConfig::default());
        let out = f.filter(42.0, 0.0);
        assert!((out - 42.0).abs() < 1e-6);
    }

    #[test]
    fn reduces_jitter_variance() {
        // Feed a constant signal plus alternating jitter; the filtered signal
        // should have far smaller variance than the raw input.
        let mut f = OneEuroFilter::new(OneEuroConfig::default());
        let base = 100.0_f32;
        let dt = 1.0 / 120.0;

        let mut raw = Vec::new();
        let mut filt = Vec::new();
        for i in 0..200 {
            let jitter = if i % 2 == 0 { 1.0 } else { -1.0 };
            let x = base + jitter;
            raw.push(x);
            filt.push(f.filter(x, i as f32 * dt));
        }

        let var = |v: &[f32]| {
            let n = v.len() as f32;
            let mean = v.iter().sum::<f32>() / n;
            v.iter().map(|x| (x - mean).powi(2)).sum::<f32>() / n
        };

        // Ignore the warm-up transient.
        let raw_var = var(&raw[20..]);
        let filt_var = var(&filt[20..]);
        assert!(
            filt_var < raw_var * 0.5,
            "filtered variance {filt_var} should be well below raw {raw_var}"
        );
    }

    #[test]
    fn tracks_a_ramp_without_large_steady_state_lag() {
        // For a constant-velocity ramp, after warm-up the filter should track
        // closely (One-Euro raises its cutoff with speed).
        let mut f = OneEuroFilter::new(OneEuroConfig::default());
        let dt = 1.0 / 120.0;
        let v = 500.0; // px/s
        let mut last_err = f32::INFINITY;
        for i in 0..300 {
            let t = i as f32 * dt;
            let x = v * t;
            let out = f.filter(x, t);
            last_err = (x - out).abs();
        }
        // Lag should be a small fraction of a second's worth of travel.
        assert!(last_err < v * 0.05, "steady-state lag too high: {last_err}");
    }
}
