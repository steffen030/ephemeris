//! Palm rejection: suppress finger touch events while the pen is in range.
//!
//! Per ADR ephemeris-4ea ("handbalm"), resting the writing hand on the panel
//! must not generate spurious ink strokes.  [`PalmRejector`] sits in the
//! [`InputEvent`] pipeline between the backend and the ink engine:
//!
//! ```text
//!   Backend ──► PalmRejector::filter() ──► ActionRouter / InkEngine
//! ```
//!
//! ## Suppression logic
//!
//! Any pen-related event (`Hover`, `PenDown`, `PenMove`, `PenUp`, `PenButton`)
//! arms the suppressor and records the event timestamp.  Subsequent
//! `TouchBegin`, `TouchMove`, and `TouchEnd` events are **dropped** as long as
//! the elapsed time since the last pen event is ≤ `suppress_ms`.
//!
//! Once the timeout expires (pen out of range and no new pen events for
//! `suppress_ms` milliseconds), touch events flow through normally again.
//!
//! ## Usage
//!
//! ```rust
//! use ephemeris_pal::input::palm::{PalmRejector, PalmRejectorConfig};
//! use ephemeris_pal::input::{InputEvent, TouchSample};
//!
//! let mut rejector = PalmRejector::new(PalmRejectorConfig::default());
//! let touch = InputEvent::TouchBegin(TouchSample { id: 0, x: 10.0, y: 20.0 });
//! // No pen event yet → touch is allowed.
//! assert!(rejector.filter(touch, 0).is_some());
//! ```

use super::InputEvent;

/// Configures the palm rejection window.
#[derive(Debug, Clone)]
pub struct PalmRejectorConfig {
    /// How long (milliseconds) to suppress touch events after the last pen
    /// event.  This covers the gap between the pen leaving hover range and the
    /// hardware reporting it.  Default: 100 ms.
    pub suppress_ms: u32,
}

impl Default for PalmRejectorConfig {
    fn default() -> Self {
        Self { suppress_ms: 100 }
    }
}

/// Stateful palm-rejection filter.
///
/// Call [`PalmRejector::filter`] for each incoming [`InputEvent`], passing a
/// monotonically increasing timestamp `t_ms` (milliseconds).  The method
/// returns `Some(event)` to forward the event or `None` to drop it.
///
/// All pen events are always forwarded.  Touch events are dropped while the
/// pen is in range or within [`PalmRejectorConfig::suppress_ms`] of the last
/// pen event.
pub struct PalmRejector {
    cfg: PalmRejectorConfig,
    /// Timestamp of the most recent pen-related event.
    pen_last_ms: Option<u32>,
}

impl PalmRejector {
    pub fn new(cfg: PalmRejectorConfig) -> Self {
        Self {
            cfg,
            pen_last_ms: None,
        }
    }

    /// Filter one input event at timestamp `t_ms`.
    ///
    /// Returns `Some(event)` to forward it downstream, or `None` to drop it.
    /// Pen events always pass through; touch events are suppressed while the
    /// rejection window is active.
    pub fn filter(&mut self, event: InputEvent, t_ms: u32) -> Option<InputEvent> {
        match &event {
            InputEvent::Hover { .. }
            | InputEvent::PenDown(_)
            | InputEvent::PenMove(_)
            | InputEvent::PenUp(_)
            | InputEvent::PenButton { .. } => {
                self.pen_last_ms = Some(t_ms);
                Some(event)
            }
            InputEvent::TouchBegin(_) | InputEvent::TouchMove(_) | InputEvent::TouchEnd(_) => {
                if self.suppressing(t_ms) {
                    None
                } else {
                    Some(event)
                }
            }
        }
    }

    /// `true` if touch events are currently being suppressed at `t_ms`.
    pub fn is_suppressing(&self, t_ms: u32) -> bool {
        self.suppressing(t_ms)
    }

    #[inline]
    fn suppressing(&self, t_ms: u32) -> bool {
        match self.pen_last_ms {
            Some(last) => t_ms.saturating_sub(last) <= self.cfg.suppress_ms,
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{PenSample, TouchSample};

    fn pen(x: f32, y: f32) -> PenSample {
        PenSample {
            x,
            y,
            pressure: 0.5,
            tilt: 0.0,
            in_range: true,
        }
    }

    fn touch(id: u32) -> TouchSample {
        TouchSample {
            id,
            x: 50.0,
            y: 50.0,
        }
    }

    fn rejector() -> PalmRejector {
        PalmRejector::new(PalmRejectorConfig { suppress_ms: 100 })
    }

    #[test]
    fn touch_allowed_before_any_pen_event() {
        let mut r = rejector();
        assert!(r.filter(InputEvent::TouchBegin(touch(0)), 0).is_some());
        assert!(r.filter(InputEvent::TouchMove(touch(0)), 5).is_some());
        assert!(r.filter(InputEvent::TouchEnd(touch(0)), 10).is_some());
    }

    #[test]
    fn pen_hover_suppresses_immediate_touch() {
        let mut r = rejector();
        r.filter(InputEvent::Hover { x: 10.0, y: 10.0 }, 100);
        assert!(r.filter(InputEvent::TouchBegin(touch(0)), 100).is_none());
    }

    #[test]
    fn pen_down_suppresses_touch() {
        let mut r = rejector();
        r.filter(InputEvent::PenDown(pen(10.0, 10.0)), 200);
        assert!(r.filter(InputEvent::TouchBegin(touch(0)), 200).is_none());
        assert!(r.filter(InputEvent::TouchMove(touch(0)), 210).is_none());
    }

    #[test]
    fn touch_suppressed_within_window_after_pen_up() {
        let mut r = rejector();
        r.filter(InputEvent::PenUp(pen(10.0, 10.0)), 500);
        // Still within the 100 ms window.
        assert!(r.filter(InputEvent::TouchBegin(touch(0)), 550).is_none());
        assert!(r.filter(InputEvent::TouchBegin(touch(0)), 599).is_none());
        assert!(r.filter(InputEvent::TouchBegin(touch(0)), 600).is_none()); // boundary: 600 - 500 = 100 ≤ 100
    }

    #[test]
    fn touch_allowed_after_window_expires() {
        let mut r = rejector();
        r.filter(InputEvent::PenUp(pen(10.0, 10.0)), 500);
        // 601 - 500 = 101 > 100: window expired.
        assert!(r.filter(InputEvent::TouchBegin(touch(0)), 601).is_some());
    }

    #[test]
    fn pen_button_extends_suppression_window() {
        let mut r = rejector();
        r.filter(InputEvent::PenButton { pressed: true }, 1000);
        assert!(r.filter(InputEvent::TouchBegin(touch(0)), 1050).is_none());
        assert!(r.filter(InputEvent::TouchBegin(touch(0)), 1101).is_some());
    }

    #[test]
    fn pen_events_always_pass_through() {
        let mut r = rejector();
        // Even after a pen event the pen events themselves must not be dropped.
        assert!(r.filter(InputEvent::PenDown(pen(1.0, 1.0)), 0).is_some());
        assert!(r.filter(InputEvent::PenMove(pen(2.0, 2.0)), 8).is_some());
        assert!(r.filter(InputEvent::PenUp(pen(3.0, 3.0)), 16).is_some());
        assert!(r.filter(InputEvent::Hover { x: 4.0, y: 4.0 }, 24).is_some());
        assert!(r
            .filter(InputEvent::PenButton { pressed: false }, 32)
            .is_some());
    }

    #[test]
    fn interleaved_pen_and_touch_stream() {
        let mut r = rejector();
        // t=0: pen contacts surface
        r.filter(InputEvent::PenDown(pen(100.0, 100.0)), 0);
        // t=10: palm rests on screen — must be suppressed
        assert!(r.filter(InputEvent::TouchBegin(touch(1)), 10).is_none());
        assert!(r.filter(InputEvent::TouchMove(touch(1)), 20).is_none());
        // t=50: pen lifts
        r.filter(InputEvent::PenUp(pen(120.0, 100.0)), 50);
        // t=100: palm still within window (100 - 50 = 50 ≤ 100)
        assert!(r.filter(InputEvent::TouchMove(touch(1)), 100).is_none());
        // t=151: window expired (151 - 50 = 101 > 100)
        assert!(r.filter(InputEvent::TouchEnd(touch(1)), 151).is_some());
    }

    #[test]
    fn each_pen_event_refreshes_window() {
        let mut r = rejector();
        r.filter(InputEvent::PenMove(pen(0.0, 0.0)), 0);
        // Advance within first window, then another pen event extends it.
        r.filter(InputEvent::PenMove(pen(1.0, 0.0)), 90);
        // Touch at t=170: 170 - 90 = 80 ≤ 100 → still suppressed.
        assert!(r.filter(InputEvent::TouchBegin(touch(0)), 170).is_none());
        // Touch at t=192: 192 - 90 = 102 > 100 → allowed.
        assert!(r.filter(InputEvent::TouchBegin(touch(0)), 192).is_some());
    }

    #[test]
    fn is_suppressing_reflects_state() {
        let mut r = rejector();
        assert!(!r.is_suppressing(0));
        r.filter(InputEvent::PenDown(pen(0.0, 0.0)), 500);
        assert!(r.is_suppressing(550));
        assert!(!r.is_suppressing(601));
    }

    #[test]
    fn zero_suppress_ms_allows_touch_immediately_after_pen() {
        let mut r = PalmRejector::new(PalmRejectorConfig { suppress_ms: 0 });
        r.filter(InputEvent::PenUp(pen(0.0, 0.0)), 100);
        // 101 - 100 = 1 > 0: allowed immediately after the event timestamp.
        assert!(r.filter(InputEvent::TouchBegin(touch(0)), 101).is_some());
    }
}
