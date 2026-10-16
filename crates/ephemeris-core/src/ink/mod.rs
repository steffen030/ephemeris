//! Ink engine (ADR ephemeris-cm7, task ephemeris-6iy.1).
//!
//! Turns the pen [`InputEvent`](ephemeris_pal::input::InputEvent) stream into
//! rendered [`Stroke`](crate::model::Stroke)s:
//!
//! * [`one_euro`] — adaptive low-pass smoothing of raw samples.
//! * [`spline`] — centripetal Catmull-Rom resampling for a smooth curve.
//! * [`replay`] — JSONL pen-capture parser / replayer using real timestamps.
//! * [`width`] — pressure → stroke-width mapping.
//! * [`engine`] — the [`InkEngine`] that ties it together and reports the
//!   incremental damage rect for eink Fast-refresh.
//!
//! See [`crate::store`] for persistence of completed strokes.

pub mod engine;
pub mod eraser;
pub mod history;
pub mod lasso;
pub mod one_euro;
pub mod replay;
pub mod shape_fit;
pub mod spline;
pub mod width;

pub use engine::{InkConfig, InkEngine, InkUpdate};
pub use eraser::{EraserConfig, EraserEngine, EraserUpdate};
pub use history::{ApplyOp, InkEdit, InkHistory, InkHistoryMap, DEFAULT_INK_HISTORY_DEPTH};
pub use lasso::{
    ensure_closed, point_in_polygon, stroke_inside_lasso, strokes_inside_lasso, translate_stroke,
    translate_strokes_by_id,
};
pub use one_euro::{OneEuroConfig, OneEuroFilter, OneEuroFilter2D};
pub use shape_fit::{fit_smart_shape, FittedShape, ShapeKind, MIN_CONFIDENCE};
pub use width::{
    pen_width_for_label, width_for, WidthConfig, PEN_WIDTH_MEDIUM, PEN_WIDTH_THICK, PEN_WIDTH_THIN,
};
