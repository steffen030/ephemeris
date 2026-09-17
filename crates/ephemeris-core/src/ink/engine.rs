//! The ink engine: turn an `InputEvent` pen stream into rendered [`Stroke`]s.
//!
//! Pipeline for a single stroke:
//!
//! 1. `PenDown` starts a new [`StrokeBuilder`].
//! 2. Each `PenMove` sample is smoothed by a [`OneEuroFilter2D`] and appended.
//!    The engine reports the **incremental damage rect** — the bounding box of
//!    just the newly rendered segment (inflated by the local half-width) — so
//!    the caller can drive the eink *Fast* refresh path on that region only.
//! 3. `PenUp` finalises the stroke: the collected samples are resampled with a
//!    centripetal Catmull-Rom spline and returned as a [`Stroke`].
//!
//! The engine never redraws the whole canvas: total stroke damage is the union
//! of the per-segment damage rects, always bounded by the stroke's bbox.

use ephemeris_pal::input::{InputEvent, PenSample};

use crate::geom::Rect;
use crate::model::{Color, Point, Stroke, Tool};

use super::one_euro::{OneEuroConfig, OneEuroFilter2D};
use super::spline::catmull_rom;
use super::width::WidthConfig;

/// Tunable parameters for the ink engine.
#[derive(Debug, Clone)]
pub struct InkConfig {
    /// Nominal pen width in logical pixels.
    pub base_width: f32,
    /// Stroke colour.
    pub color: Color,
    /// Active tool.
    pub tool: Tool,
    /// One-Euro smoothing configuration.
    pub one_euro: OneEuroConfig,
    /// Pressure→width configuration.
    pub width: WidthConfig,
    /// Catmull-Rom segments generated per input span at finalisation.
    pub spline_segments: usize,
    /// Minimum distance (logical px) between accepted raw samples.  Filters out
    /// near-duplicate points that add cost without adding shape.
    pub min_sample_dist: f32,
}

impl Default for InkConfig {
    fn default() -> Self {
        Self {
            base_width: 3.0,
            color: Color::BLACK,
            tool: Tool::Pen,
            one_euro: OneEuroConfig::default(),
            width: WidthConfig::default(),
            spline_segments: 6,
            min_sample_dist: 0.75,
        }
    }
}

/// Result of feeding one event into [`InkEngine::update`].
#[derive(Debug, Clone, PartialEq)]
pub enum InkUpdate {
    /// Nothing happened for this event (e.g. hover, touch, no-op sample).
    Idle,
    /// A stroke was started; no ink drawn yet.
    Started,
    /// The in-progress stroke grew; `damage` is the region that changed and
    /// must be repainted (already inflated to cover the brush half-width).
    Extended { damage: Rect },
    /// The stroke finished; the completed [`Stroke`] is returned along with the
    /// final segment's `damage`.
    Finished { stroke: Box<Stroke>, damage: Rect },
}

/// Stateful builder for one stroke.
struct StrokeBuilder {
    tool: Tool,
    color: Color,
    base_width: f32,
    filter: OneEuroFilter2D,
    width_cfg: WidthConfig,
    min_sample_dist: f32,
    /// Smoothed raw samples collected so far.
    points: Vec<Point>,
    /// Union of all damage rects reported for this stroke (== stroke bbox
    /// inflated by half-width). Handy for validation and final flush.
    total_damage: Rect,
    /// Timestamp of the PenDown, used to make `t_ms` stroke-relative.
    start_ms: Option<u32>,
}

impl StrokeBuilder {
    fn new(cfg: &InkConfig) -> Self {
        Self {
            tool: cfg.tool,
            color: cfg.color,
            base_width: cfg.base_width,
            filter: OneEuroFilter2D::new(cfg.one_euro),
            width_cfg: cfg.width,
            min_sample_dist: cfg.min_sample_dist,
            points: Vec::new(),
            total_damage: Rect::empty(),
            start_ms: None,
        }
    }

    /// Half-width of the brush at a given pressure (for damage inflation).
    fn half_width(&self, pressure: f32) -> f32 {
        self.width_cfg.width_for(self.base_width, pressure) * 0.5
    }

    /// Smooth and (maybe) append a sample. Returns the incremental damage rect
    /// if a point was actually added, else `None`.
    ///
    /// When `anchor` is set (PenDown / PenUp), the raw position is used instead
    /// of the filtered one so the stroke begins and ends exactly where the pen
    /// touched and lifted — the One-Euro filter otherwise lags the endpoints.
    fn push_sample(&mut self, s: &PenSample, t_ms: u32, anchor: bool) -> Option<Rect> {
        let start = *self.start_ms.get_or_insert(t_ms);
        let rel_ms = t_ms.saturating_sub(start);

        // One-Euro expects seconds. We always run the filter to keep its state
        // consistent, but use the raw coordinate for anchor samples.
        let (fx, fy) = self.filter.filter(s.x, s.y, rel_ms as f32 / 1000.0);
        let (sx, sy) = if anchor { (s.x, s.y) } else { (fx, fy) };

        let new = Point {
            x: sx,
            y: sy,
            pressure: s.pressure.clamp(0.0, 1.0),
            tilt: s.tilt,
            t_ms: rel_ms,
        };

        // Drop near-duplicate samples (except the very first point and anchors,
        // which must always be recorded).
        if !anchor {
            if let Some(prev) = self.points.last() {
                let d = ((new.x - prev.x).powi(2) + (new.y - prev.y).powi(2)).sqrt();
                if d < self.min_sample_dist {
                    return None;
                }
            }
        }

        // Incremental damage: bbox of the segment from prev -> new, inflated by
        // the larger of the two half-widths so the brush is fully covered.
        let mut seg = Rect::empty();
        if let Some(prev) = self.points.last() {
            seg.union_point(prev.x, prev.y);
        }
        seg.union_point(new.x, new.y);

        let hw = self
            .points
            .last()
            .map(|p| self.half_width(p.pressure))
            .unwrap_or(0.0)
            .max(self.half_width(new.pressure));
        let damage = seg.inflate(hw + 1.0); // +1 px anti-alias margin

        self.points.push(new);
        self.total_damage = self.total_damage.union(&damage);
        Some(damage)
    }

    /// Finalise into a [`Stroke`], applying Catmull-Rom smoothing.
    fn finish(self, spline_segments: usize) -> Stroke {
        let smoothed = catmull_rom(&self.points, spline_segments);
        Stroke {
            id: crate::model::StrokeId::new(),
            tool: self.tool,
            color: self.color,
            base_width: self.base_width,
            points: smoothed,
        }
    }
}

/// The ink engine. Feed it [`InputEvent`]s; it emits [`InkUpdate`]s.
pub struct InkEngine {
    cfg: InkConfig,
    active: Option<StrokeBuilder>,
    /// Monotonic sample clock in ms, advanced when events carry no timestamp.
    clock_ms: u32,
}

impl InkEngine {
    pub fn new(cfg: InkConfig) -> Self {
        Self { cfg, active: None, clock_ms: 0 }
    }

    /// Whether a stroke is currently in progress.
    pub fn is_drawing(&self) -> bool {
        self.active.is_some()
    }

    /// Feed one input event. `t_ms` is the sample's absolute timestamp; pass a
    /// monotonically increasing clock (see [`InkEngine::update_now`] if you
    /// don't have real timestamps).
    pub fn update(&mut self, event: &InputEvent, t_ms: u32) -> InkUpdate {
        self.clock_ms = self.clock_ms.max(t_ms);
        match event {
            InputEvent::PenDown(s) => {
                let mut b = StrokeBuilder::new(&self.cfg);
                // Seed the first point so the stroke starts exactly at PenDown.
                let _ = b.push_sample(s, t_ms, true);
                self.active = Some(b);
                InkUpdate::Started
            }
            InputEvent::PenMove(s) => match self.active.as_mut() {
                Some(b) => match b.push_sample(s, t_ms, false) {
                    Some(damage) => InkUpdate::Extended { damage },
                    None => InkUpdate::Idle,
                },
                None => InkUpdate::Idle, // move without down: ignore
            },
            InputEvent::PenUp(s) => match self.active.take() {
                Some(mut b) => {
                    let damage = b.push_sample(s, t_ms, true).unwrap_or(b.total_damage);
                    let stroke = b.finish(self.cfg.spline_segments);
                    InkUpdate::Finished { stroke: Box::new(stroke), damage }
                }
                None => InkUpdate::Idle,
            },
            // Hover / buttons / touch are not ink for this engine.
            _ => InkUpdate::Idle,
        }
    }

    /// Feed an event using the engine's internal monotonic clock (advances by
    /// 8 ms per call ~ 120 Hz). Useful when the input source carries no time.
    pub fn update_now(&mut self, event: &InputEvent) -> InkUpdate {
        let t = self.clock_ms;
        self.clock_ms = self.clock_ms.wrapping_add(8);
        self.update(event, t)
    }
}
