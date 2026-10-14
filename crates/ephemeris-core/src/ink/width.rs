//! Pressure → stroke-width mapping.
//!
//! Pen pressure is normalised to `[0.0, 1.0]`.  We map it to a rendered width
//! in logical pixels around a nominal `base_width`.  The mapping is:
//!
//! ```text
//! width(pressure) = base_width * (min_ratio + (max_ratio - min_ratio) * f(pressure))
//! ```
//!
//! where `f` applies a `gamma` response curve so that light touches stay thin
//! and firm presses fatten up in a way that feels natural.  The result is
//! clamped to `[min_ratio, max_ratio] * base_width` and never goes below a
//! small floor so a zero-pressure sample still renders a visible dot.
//!
//! ## w9013 / PineNote defaults (ephemeris-ktg.6)
//!
//! `evdev_pen` normalises `ABS_PRESSURE` with a device-reported max (fallback
//! 4096).  The only checked-in capture (`synthetic_scribble.jsonl`) has contact
//! pressure in ≈ `[0.15, 0.85]` with median ≈ `0.58` — no light/firm device
//! histograms yet.  Defaults place that median near ratio `1.0` (mid-curve)
//! so normal writing is not clamped at `max_ratio`.

/// Configuration for the pressure→width curve.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WidthConfig {
    /// Fraction of `base_width` produced at pressure 0 (e.g. 0.40).
    pub min_ratio: f32,
    /// Fraction of `base_width` produced at pressure 1 (e.g. 1.50).
    pub max_ratio: f32,
    /// Response curve exponent (>0). `1.0` = linear; `>1` = softer low end.
    pub gamma: f32,
    /// Absolute minimum rendered width in logical pixels.
    pub min_px: f32,
}

impl Default for WidthConfig {
    fn default() -> Self {
        // See module docs — fitted so p≈0.58 (fixture median) ≈ base_width.
        Self {
            min_ratio: 0.40,
            max_ratio: 1.50,
            gamma: 1.15,
            min_px: 0.5,
        }
    }
}

impl WidthConfig {
    /// Map a `pressure` in `[0,1]` to a rendered width for a stroke whose
    /// nominal width is `base_width` logical pixels.
    pub fn width_for(&self, base_width: f32, pressure: f32) -> f32 {
        let p = pressure.clamp(0.0, 1.0);
        let shaped = p.powf(self.gamma.max(1e-3));
        let ratio = self.min_ratio + (self.max_ratio - self.min_ratio) * shaped;
        (base_width * ratio).max(self.min_px)
    }
}

/// Convenience: default-config width mapping.
pub fn width_for(base_width: f32, pressure: f32) -> f32 {
    WidthConfig::default().width_for(base_width, pressure)
}

/// Burger-menu pen presets (logical px). Medium matches [`InkConfig::base_width`](super::InkConfig).
pub const PEN_WIDTH_THIN: f32 = 2.0;
/// Default writing width chosen with the One-Euro / pressure sweep (ephemeris-ktg.6).
pub const PEN_WIDTH_MEDIUM: f32 = 3.0;
pub const PEN_WIDTH_THICK: f32 = 4.5;

/// Parse a burger-menu width label (`"thin"|"medium"|"thick"`). Unknown → medium.
pub fn pen_width_for_label(label: &str) -> f32 {
    match label {
        "thin" => PEN_WIDTH_THIN,
        "thick" => PEN_WIDTH_THICK,
        _ => PEN_WIDTH_MEDIUM,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_pressure_is_thin_but_visible() {
        let cfg = WidthConfig::default();
        let w = cfg.width_for(4.0, 0.0);
        assert!(w >= cfg.min_px);
        assert!(
            w < cfg.width_for(4.0, 1.0),
            "min pressure should be thinner than max"
        );
    }

    #[test]
    fn full_pressure_is_widest() {
        let cfg = WidthConfig::default();
        let low = cfg.width_for(4.0, 0.1);
        let high = cfg.width_for(4.0, 1.0);
        assert!(high > low);
        // At pressure 1, width == base * max_ratio.
        assert!((high - 4.0 * cfg.max_ratio).abs() < 1e-4);
    }

    #[test]
    fn normal_writing_lands_near_mid_curve() {
        // Fixture median contact pressure ≈ 0.58; mid-curve ⇒ ≈ base_width.
        let cfg = WidthConfig::default();
        let w = cfg.width_for(3.0, 0.58);
        assert!(
            (w - 3.0).abs() < 0.15,
            "normal writing width {w} should sit near base 3.0"
        );
    }

    #[test]
    fn width_is_monotonic_in_pressure() {
        let cfg = WidthConfig::default();
        let mut prev = 0.0;
        for i in 0..=100 {
            let p = i as f32 / 100.0;
            let w = cfg.width_for(3.0, p);
            assert!(
                w >= prev - 1e-6,
                "width must not decrease as pressure rises"
            );
            prev = w;
        }
    }

    #[test]
    fn clamps_out_of_range_pressure() {
        let cfg = WidthConfig::default();
        assert_eq!(cfg.width_for(3.0, -5.0), cfg.width_for(3.0, 0.0));
        assert_eq!(cfg.width_for(3.0, 5.0), cfg.width_for(3.0, 1.0));
    }

    #[test]
    fn respects_absolute_floor() {
        let cfg = WidthConfig {
            min_px: 2.0,
            ..Default::default()
        };
        // Tiny base width, zero pressure would go below the floor.
        assert_eq!(cfg.width_for(0.1, 0.0), 2.0);
    }
}
