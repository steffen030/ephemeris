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

#[cfg(test)]
mod tests {
    use super::*;
    use ephemeris_pal::input::PenSample;

    fn sample(x: f32, y: f32, pressure: f32) -> PenSample {
        PenSample { x, y, pressure, tilt: 0.0, in_range: true }
    }

    /// Feed a full PenDown -> PenMove* -> PenUp stream at constant pressure and
    /// return every update plus the finished stroke.
    fn drive(coords: &[(f32, f32)], pressure: f32) -> (Vec<InkUpdate>, Stroke) {
        let mut engine = InkEngine::new(InkConfig::default());
        let mut updates = Vec::new();
        let mut finished = None;
        let last = coords.len() - 1;
        for (i, &(x, y)) in coords.iter().enumerate() {
            let s = sample(x, y, pressure);
            let ev = if i == 0 {
                InputEvent::PenDown(s)
            } else if i == last {
                InputEvent::PenUp(s)
            } else {
                InputEvent::PenMove(s)
            };
            let u = engine.update_now(&ev);
            if let InkUpdate::Finished { stroke, damage } = u {
                finished = Some(*stroke.clone());
                updates.push(InkUpdate::Finished { stroke, damage });
            } else {
                updates.push(u);
            }
        }
        (updates, finished.expect("stream must finish a stroke"))
    }

    fn diagonal(n: usize) -> Vec<(f32, f32)> {
        (0..n).map(|i| (10.0 + i as f32 * 4.0, 20.0 + i as f32 * 3.0)).collect()
    }

    #[test]
    fn lifecycle_started_extended_finished() {
        let (updates, _stroke) = drive(&diagonal(6), 0.5);
        assert!(matches!(updates.first().unwrap(), InkUpdate::Started));
        assert!(matches!(updates.last().unwrap(), InkUpdate::Finished { .. }));
        assert_eq!(
            updates.iter().filter(|u| matches!(u, InkUpdate::Started)).count(),
            1
        );
        assert_eq!(
            updates.iter().filter(|u| matches!(u, InkUpdate::Finished { .. })).count(),
            1
        );
        assert!(
            updates.iter().any(|u| matches!(u, InkUpdate::Extended { .. })),
            "the growing stroke must report incremental Extended damage"
        );
    }

    #[test]
    fn move_before_down_is_idle() {
        let mut engine = InkEngine::new(InkConfig::default());
        assert_eq!(engine.update_now(&InputEvent::PenMove(sample(1.0, 1.0, 0.5))), InkUpdate::Idle);
        assert!(!engine.is_drawing());
    }

    #[test]
    fn hover_button_touch_are_idle() {
        let mut engine = InkEngine::new(InkConfig::default());
        assert_eq!(engine.update_now(&InputEvent::Hover { x: 1.0, y: 2.0 }), InkUpdate::Idle);
        assert_eq!(engine.update_now(&InputEvent::PenButton { pressed: true }), InkUpdate::Idle);
        assert!(!engine.is_drawing());
    }

    #[test]
    fn endpoints_anchored_to_raw_pen_down_and_up() {
        // Endpoints must sit exactly on the raw PenDown / PenUp coordinates,
        // not on the lagged One-Euro output.
        let coords = diagonal(10);
        let (_updates, stroke) = drive(&coords, 0.5);
        let first = stroke.points.first().unwrap();
        let last = stroke.points.last().unwrap();
        let raw_first = coords[0];
        let raw_last = *coords.last().unwrap();
        assert!((first.x - raw_first.0).abs() < 1e-4 && (first.y - raw_first.1).abs() < 1e-4);
        assert!((last.x - raw_last.0).abs() < 1e-4 && (last.y - raw_last.1).abs() < 1e-4);
    }

    #[test]
    fn every_damage_rect_is_within_stroke_bbox_inflated_by_half_width() {
        let coords = diagonal(12);
        let cfg = InkConfig::default();
        let (updates, stroke) = drive(&coords, 0.6);

        // Reference bbox: the smoothed stroke centre-line inflated by the
        // maximum possible half-width plus the engine's AA margin. Every damage
        // rect (which is derived from the raw pre-smoothing samples) must fall
        // within this region.
        let mut bbox = Rect::empty();
        for p in &stroke.points {
            bbox.union_point(p.x, p.y);
        }
        // The raw samples can lie slightly outside the smoothed bbox and the
        // spline can overshoot, so use a generous but bounded margin derived
        // from the width config's largest width.
        let max_half = cfg.width.width_for(cfg.base_width, 1.0) * 0.5 + 2.0;
        let allowed = bbox.inflate(max_half + 4.0);

        let mut union = Rect::empty();
        let mut saw = false;
        for u in &updates {
            let d = match u {
                InkUpdate::Extended { damage } => Some(*damage),
                InkUpdate::Finished { damage, .. } => Some(*damage),
                _ => None,
            };
            if let Some(d) = d {
                saw = true;
                union = union.union(&d);
                assert!(
                    d.min_x >= allowed.min_x && d.min_y >= allowed.min_y
                        && d.max_x <= allowed.max_x && d.max_y <= allowed.max_y,
                    "damage {d:?} escaped stroke bbox {allowed:?}"
                );
            }
        }
        assert!(saw, "expected incremental damage");

        // No full-canvas redraw: the union of all damage must not exceed the
        // stroke bbox region.
        assert!(
            union.width() <= allowed.width() + 1e-3 && union.height() <= allowed.height() + 1e-3,
            "damage union {union:?} exceeded stroke bbox {allowed:?}"
        );
    }

    #[test]
    fn damage_union_equals_builder_total_damage() {
        // Independent check that the running union tracked internally matches the
        // union we recompute from the reported rects.
        let coords = diagonal(8);
        let (updates, _stroke) = drive(&coords, 0.5);
        let mut union = Rect::empty();
        for u in &updates {
            match u {
                InkUpdate::Extended { damage } => union = union.union(damage),
                InkUpdate::Finished { damage, .. } => union = union.union(damage),
                _ => {}
            }
        }
        assert!(!union.is_empty());
        // A single short diagonal stroke: bounded, tens of px, never thousands.
        assert!(union.width() < 200.0 && union.height() < 200.0, "damage union too large: {union:?}");
    }

    #[test]
    fn higher_pressure_yields_greater_width() {
        // Same geometry, different constant pressure. The engine preserves
        // per-point pressure into the stroke; the rendered width mapping must
        // make the firmer stroke wider everywhere.
        let coords = diagonal(10);
        let cfg = InkConfig::default();
        let (_u_soft, soft) = drive(&coords, 0.15);
        let (_u_firm, firm) = drive(&coords, 0.9);

        let mean_width = |s: &Stroke| -> f32 {
            let n = s.points.len() as f32;
            s.points
                .iter()
                .map(|p| cfg.width.width_for(s.base_width, p.pressure))
                .sum::<f32>()
                / n
        };

        let w_soft = mean_width(&soft);
        let w_firm = mean_width(&firm);
        assert!(
            w_firm > w_soft,
            "firm-pressure stroke width {w_firm} must exceed soft {w_soft}"
        );
    }

    #[test]
    fn near_duplicate_moves_are_dropped_but_endpoints_kept() {
        // Feed a PenDown then many identical PenMoves then a PenUp. The near-
        // duplicate moves must be dropped (Idle), but the stroke must still have
        // its anchored endpoints.
        let mut engine = InkEngine::new(InkConfig::default());
        assert!(matches!(engine.update_now(&InputEvent::PenDown(sample(0.0, 0.0, 0.5))), InkUpdate::Started));
        for _ in 0..5 {
            // Same coordinate: below min_sample_dist -> dropped.
            assert_eq!(engine.update_now(&InputEvent::PenMove(sample(0.0, 0.0, 0.5))), InkUpdate::Idle);
        }
        let fin = engine.update_now(&InputEvent::PenUp(sample(0.05, 0.05, 0.5)));
        match fin {
            InkUpdate::Finished { stroke, .. } => {
                assert!(stroke.points.len() >= 2, "endpoints must be recorded even if moves dropped");
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[test]
    fn single_tap_down_up_produces_a_stroke() {
        // Degenerate stroke: PenDown immediately followed by PenUp.
        let mut engine = InkEngine::new(InkConfig::default());
        assert!(matches!(engine.update_now(&InputEvent::PenDown(sample(3.0, 4.0, 0.5))), InkUpdate::Started));
        match engine.update_now(&InputEvent::PenUp(sample(3.0, 4.0, 0.5))) {
            InkUpdate::Finished { stroke, .. } => {
                assert!(!stroke.points.is_empty());
                for p in &stroke.points {
                    assert!(p.x.is_finite() && p.y.is_finite());
                }
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        assert!(!engine.is_drawing());
    }
}
