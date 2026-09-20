//! Ink engine (ADR ephemeris-cm7, task ephemeris-6iy.1).
//!
//! Turns the pen [`InputEvent`](ephemeris_pal::input::InputEvent) stream into
//! rendered [`Stroke`](crate::model::Stroke)s:
//!
//! * [`one_euro`] — adaptive low-pass smoothing of raw samples.
//! * [`spline`] — centripetal Catmull-Rom resampling for a smooth curve.
//! * [`width`] — pressure → stroke-width mapping.
//! * [`engine`] — the [`InkEngine`] that ties it together and reports the
//!   incremental damage rect for eink Fast-refresh.
//!
//! See [`crate::store`] for persistence of completed strokes.

pub mod engine;
pub mod eraser;
pub mod one_euro;
pub mod spline;
pub mod width;

pub use engine::{InkConfig, InkEngine, InkUpdate};
pub use eraser::{EraserConfig, EraserEngine, EraserUpdate};
pub use one_euro::{OneEuroConfig, OneEuroFilter, OneEuroFilter2D};
pub use width::{width_for, WidthConfig};
