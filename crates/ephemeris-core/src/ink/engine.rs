//! The ink engine: turn an `InputEvent` pen stream into rendered [`Stroke`]s.
//!
//! Pipeline for a single stroke:
//!
//! 1. `PenDown` starts a new [`StrokeBuilder`].
//! 2. Each accepted `PenMove` sample is smoothed by a [`OneEuroFilter2D`] and
//!    appended as a *control* point. When a new control makes a Catmull-Rom
//!    span available (1-sample lookbehind), the engine emits the incremental
//!    spline points plus their damage rect so the UI can stamp live ink that
//!    already matches the committed stroke.
//! 3. `PenUp` anchors the tip, emits the remaining open spans, and returns the
//!    accumulated smoothed polyline as the [`Stroke`] (no second full spline).
//!    Finished damage is the **last span only**.

use ephemeris_pal::input::{InputEvent, PenSample};

use crate::geom::Rect;
use crate::model::{Color, Point, Stroke, Tool};

use super::one_euro::{OneEuroConfig, OneEuroFilter2D};
use super::spline::{adaptive_span_segments, catmull_rom_span};
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
    /// Catmull-Rom segments per span (legacy / tests). Live emission uses
    /// [`adaptive_span_segments`] instead.
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
    /// A stroke was started; no ink drawn yet beyond the seeded tip.
    Started,
    /// The in-progress stroke grew; `points` are the newly emitted Catmull-Rom
    /// samples for this update and `damage` is the region that changed
    /// (already inflated to cover the brush half-width).
    Extended { points: Vec<Point>, damage: Rect },
    /// The stroke finished; the completed [`Stroke`] is returned along with
    /// tip-local `damage` covering only the span(s) emitted at PenUp (not the
    /// full stroke bbox).
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
    /// One-Euro-smoothed control points (Catmull-Rom inputs).
    points: Vec<Point>,
    /// Incrementally built Catmull-Rom polyline (becomes [`Stroke::points`]).
    smoothed: Vec<Point>,
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
            smoothed: Vec::new(),
            start_ms: None,
        }
    }

    /// Half-width of the brush at a given pressure (for damage inflation).
    fn half_width(&self, pressure: f32) -> f32 {
        self.width_cfg.width_for(self.base_width, pressure) * 0.5
    }

    /// Inflate a centre-line bbox of `pts` by the local half-width + AA margin.
    fn damage_for_points(&self, pts: &[Point]) -> Rect {
        let mut seg = Rect::empty();
        let mut hw = 0.0_f32;
        for p in pts {
            seg.union_point(p.x, p.y);
            hw = hw.max(self.half_width(p.pressure));
        }
        seg.inflate(hw + 1.0)
    }

    /// Append span samples to `smoothed`, skipping a duplicate join with the
    /// previous tip.
    fn append_smoothed(&mut self, pts: &[Point]) {
        for p in pts {
            if let Some(last) = self.smoothed.last() {
                if (last.x - p.x).abs() < 1e-6 && (last.y - p.y).abs() < 1e-6 {
                    continue;
                }
            }
            self.smoothed.push(*p);
        }
    }

    /// Emit the Catmull-Rom span that became available when control index
    /// `i+1` arrived: span `i-1 → i` with window `(p_{i-2}, p_{i-1}, p_i, p_{i+1})`.
    ///
    /// Call after a push that leaves `points.len() >= 3`. Returns the newly
    /// emitted spline points (excluding the join at the span start).
    fn emit_span_for_lookbehind(&mut self) -> Vec<Point> {
        let n = self.points.len();
        debug_assert!(n >= 3);
        // i+1 = n-1 ⇒ i = n-2; span (i-1)→i = (n-3)→(n-2).
        let i = n - 2;
        let p1 = self.points[i - 1];
        let p2 = self.points[i];
        let p3 = self.points[i + 1];
        let p0 = if i >= 2 {
            self.points[i - 2]
        } else {
            p1 // duplicate endpoint when i-2 is missing
        };
        let segs = adaptive_span_segments(&p1, &p2);
        let span = catmull_rom_span(&p0, &p1, &p2, &p3, segs);
        self.append_smoothed(&span);
        span
    }

    /// Emit the final open span from the penultimate control to the tip, with
    /// the tip duplicated as the look-ahead neighbour.
    fn emit_final_tip_span(&mut self) -> Vec<Point> {
        let n = self.points.len();
        if n < 2 {
            return Vec::new();
        }
        let p1 = self.points[n - 2];
        let p2 = self.points[n - 1];
        let p3 = p2; // duplicate tip
        let p0 = if n >= 3 {
            self.points[n - 3]
        } else {
            p1 // duplicate for a 2-point stroke
        };
        let segs = adaptive_span_segments(&p1, &p2);
        let span = catmull_rom_span(&p0, &p1, &p2, &p3, segs);
        self.append_smoothed(&span);
        span
    }

    /// Smooth and (maybe) append a sample. Returns newly emitted spline points
    /// and their damage when a Catmull-Rom span became available.
    ///
    /// When `anchor` is set (PenDown / PenUp), the raw position is used instead
    /// of the filtered one so the stroke begins and ends exactly where the pen
    /// touched and lifted — the One-Euro filter otherwise lags the endpoints.
    fn push_sample(
        &mut self,
        s: &PenSample,
        t_ms: u32,
        anchor: bool,
    ) -> Option<(Vec<Point>, Rect)> {
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

        self.points.push(new);

        // Seed the smoothed polyline with the first control so live tip and
        // Finished stroke share the same start.
        if self.points.len() == 1 {
            self.smoothed.push(new);
            return None;
        }

        // A span i-1→i needs look-ahead i+1 ⇒ at least 3 controls.
        if self.points.len() < 3 {
            return None;
        }

        let emitted = self.emit_span_for_lookbehind();
        if emitted.is_empty() {
            return None;
        }
        let damage = self.damage_for_points(&emitted);
        Some((emitted, damage))
    }

    /// Finalise into a [`Stroke`] from the already-built smoothed polyline.
    fn finish(self) -> Stroke {
        Stroke {
            id: crate::model::StrokeId::new(),
            tool: self.tool,
            color: self.color,
            base_width: self.base_width,
            points: self.smoothed,
            t0_ms: self.start_ms.map(u64::from).unwrap_or(0),
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
        Self {
            cfg,
            active: None,
            clock_ms: 0,
        }
    }

    /// Whether a stroke is currently in progress.
    pub fn is_drawing(&self) -> bool {
        self.active.is_some()
    }

    /// Last accepted smoothed (Catmull-Rom) point of the in-progress stroke,
    /// falling back to the last control point when no span has been emitted yet.
    ///
    /// Callers should stamp this — not the raw `PenSample` — into the live
    /// ink layer so preview damage and tip position match the engine.
    pub fn last_point(&self) -> Option<Point> {
        self.active.as_ref().and_then(|b| {
            b.smoothed
                .last()
                .copied()
                .or_else(|| b.points.last().copied())
        })
    }

    /// Newest control point not yet covered by an emitted Catmull-Rom span.
    ///
    /// Spans wait one sample of look-ahead, so the emitted polyline ends one
    /// control behind the pen. Live previews bridge that gap (plus the
    /// One-Euro lag) to the raw tip; the committed stroke is unaffected.
    pub fn pending_tip(&self) -> Option<Point> {
        self.active.as_ref().and_then(|b| {
            if b.points.len() >= 2 {
                b.points.last().copied()
            } else {
                None
            }
        })
    }

    /// Read-only access to the engine's current configuration.
    pub fn config(&self) -> &InkConfig {
        &self.cfg
    }

    /// Switch the active drawing tool. Takes effect on the next stroke.
    pub fn set_tool(&mut self, tool: crate::model::Tool) {
        self.cfg.tool = tool;
    }

    /// Set the nominal stroke width in logical pixels. Takes effect on the next stroke.
    pub fn set_base_width(&mut self, width: f32) {
        self.cfg.base_width = width;
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
                    Some((points, damage)) => InkUpdate::Extended { points, damage },
                    None => InkUpdate::Idle,
                },
                None => InkUpdate::Idle, // move without down: ignore
            },
            InputEvent::PenUp(s) => match self.active.take() {
                Some(mut b) => {
                    // Tip-up often reports pressure 0 — if we store that, Catmull
                    // interpolates width to a needle tip. Hold last contact
                    // pressure while anchoring the lift position.
                    let mut tip = s.clone();
                    if tip.pressure < 0.05 {
                        if let Some(prev) = b.points.last() {
                            tip.pressure = prev.pressure;
                        } else {
                            tip.pressure = 0.5;
                        }
                    }

                    // Push tip anchor; may emit the lookbehind span that tip unlocks.
                    // Finished damage is the union of every span emitted at PenUp
                    // (lookbehind + final tip) — tip-local, not the full stroke.
                    let mut tip_damage = Rect::empty();
                    if let Some((_pts, dmg)) = b.push_sample(&tip, t_ms, true) {
                        tip_damage = tip_damage.union(&dmg);
                    }

                    // Emit the final open span through the tip.
                    let final_span = b.emit_final_tip_span();
                    if !final_span.is_empty() {
                        tip_damage = tip_damage.union(&b.damage_for_points(&final_span));
                    }

                    // Degenerate tap: no span emitted — damage the tip itself.
                    if tip_damage.is_empty() {
                        if let Some(p) = b.smoothed.last().or_else(|| b.points.last()) {
                            tip_damage = b.damage_for_points(std::slice::from_ref(p));
                        }
                    }

                    let damage = tip_damage;
                    let stroke = b.finish();
                    InkUpdate::Finished {
                        stroke: Box::new(stroke),
                        damage,
                    }
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
        PenSample {
            x,
            y,
            pressure,
            tilt: 0.0,
            in_range: true,
            t_us: 0,
        }
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
        (0..n)
            .map(|i| (10.0 + i as f32 * 4.0, 20.0 + i as f32 * 3.0))
            .collect()
    }

    #[test]
    fn lifecycle_started_extended_finished() {
        let (updates, _stroke) = drive(&diagonal(6), 0.5);
        assert!(matches!(updates.first().unwrap(), InkUpdate::Started));
        assert!(matches!(
            updates.last().unwrap(),
            InkUpdate::Finished { .. }
        ));
        assert_eq!(
            updates
                .iter()
                .filter(|u| matches!(u, InkUpdate::Started))
                .count(),
            1
        );
        assert_eq!(
            updates
                .iter()
                .filter(|u| matches!(u, InkUpdate::Finished { .. }))
                .count(),
            1
        );
        assert!(
            updates
                .iter()
                .any(|u| matches!(u, InkUpdate::Extended { .. })),
            "the growing stroke must report incremental Extended damage"
        );
    }

    #[test]
    fn move_before_down_is_idle() {
        let mut engine = InkEngine::new(InkConfig::default());
        assert_eq!(
            engine.update_now(&InputEvent::PenMove(sample(1.0, 1.0, 0.5))),
            InkUpdate::Idle
        );
        assert!(!engine.is_drawing());
    }

    #[test]
    fn hover_button_touch_are_idle() {
        let mut engine = InkEngine::new(InkConfig::default());
        assert_eq!(
            engine.update_now(&InputEvent::Hover {
                x: 1.0,
                y: 2.0,
                t_us: 0,
            }),
            InkUpdate::Idle
        );
        assert_eq!(
            engine.update_now(&InputEvent::PenButton { pressed: true }),
            InkUpdate::Idle
        );
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
        // rect must fall within this region.
        let mut bbox = Rect::empty();
        for p in &stroke.points {
            bbox.union_point(p.x, p.y);
        }
        let max_half = cfg.width.width_for(cfg.base_width, 1.0) * 0.5 + 2.0;
        let allowed = bbox.inflate(max_half + 4.0);

        let mut union = Rect::empty();
        let mut saw = false;
        for u in &updates {
            let d = match u {
                InkUpdate::Extended { damage, .. } => Some(*damage),
                InkUpdate::Finished { damage, .. } => Some(*damage),
                _ => None,
            };
            if let Some(d) = d {
                saw = true;
                union = union.union(&d);
                assert!(
                    d.min_x >= allowed.min_x
                        && d.min_y >= allowed.min_y
                        && d.max_x <= allowed.max_x
                        && d.max_y <= allowed.max_y,
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
        // Independent check that the union of reported rects stays bounded.
        let coords = diagonal(8);
        let (updates, _stroke) = drive(&coords, 0.5);
        let mut union = Rect::empty();
        for u in &updates {
            match u {
                InkUpdate::Extended { damage, .. } => union = union.union(damage),
                InkUpdate::Finished { damage, .. } => union = union.union(damage),
                _ => {}
            }
        }
        assert!(!union.is_empty());
        // A single short diagonal stroke: bounded, tens of px, never thousands.
        assert!(
            union.width() < 200.0 && union.height() < 200.0,
            "damage union too large: {union:?}"
        );
    }

    #[test]
    fn finished_damage_is_last_span_only() {
        // PenUp damage must be ≤ the last-span inflated bbox — not the whole
        // stroke — so e-ink Fast refresh stays tip-local.
        let coords = diagonal(12);
        let cfg = InkConfig::default();
        let (updates, stroke) = drive(&coords, 0.5);
        let InkUpdate::Finished { damage, .. } = updates.last().unwrap() else {
            panic!("expected Finished");
        };

        // Reconstruct an upper bound on the last-span bbox from the finished
        // polyline: take a trailing window of points and inflate generously.
        let n = stroke.points.len();
        let take = n.clamp(2, 16);
        let mut last_span = Rect::empty();
        for p in &stroke.points[n.saturating_sub(take)..] {
            last_span.union_point(p.x, p.y);
        }
        let max_half = cfg.width.width_for(cfg.base_width, 1.0) * 0.5 + 1.0;
        let allowed = last_span.inflate(max_half + 4.0);

        assert!(
            damage.width() <= allowed.width() + 1e-3 && damage.height() <= allowed.height() + 1e-3,
            "Finished damage {damage:?} must fit last-span bound {allowed:?}"
        );
        // And must be much smaller than the full stroke.
        let full = stroke.approx_bbox().inflate(max_half);
        assert!(
            damage.width() * damage.height() < full.width() * full.height() * 0.5
                || stroke.points.len() < 8,
            "Finished damage {damage:?} should be << full stroke {full:?}"
        );
    }

    #[test]
    fn extended_points_accumulate_to_finished_stroke() {
        // Live Extended points plus the Started seed must equal Finished.points
        // (the spline is built once, not twice at PenUp).
        let coords = diagonal(10);
        let (updates, stroke) = drive(&coords, 0.5);

        let mut live: Vec<Point> = Vec::new();
        // Started seeds the first control into smoothed; recover it from the
        // finished stroke's first point (same seed).
        if let Some(first) = stroke.points.first() {
            live.push(*first);
        }
        for u in &updates {
            if let InkUpdate::Extended { points, .. } = u {
                for p in points {
                    if live
                        .last()
                        .is_some_and(|l| (l.x - p.x).abs() < 1e-6 && (l.y - p.y).abs() < 1e-6)
                    {
                        continue;
                    }
                    live.push(*p);
                }
            }
        }
        // Finished also emits the final tip span — those points are not in any
        // Extended. Compare the Extended prefix against stroke.points.
        assert!(
            live.len() <= stroke.points.len(),
            "live prefix {} longer than finished {}",
            live.len(),
            stroke.points.len()
        );
        for (i, (a, b)) in live.iter().zip(stroke.points.iter()).enumerate() {
            assert!(
                (a.x - b.x).abs() < 1e-3 && (a.y - b.y).abs() < 1e-3,
                "mismatch at {i}: live={a:?} finished={b:?}"
            );
        }
        // Finished must continue from the live prefix through the tip.
        assert_eq!(
            live.first().map(|p| (p.x, p.y)),
            stroke.points.first().map(|p| (p.x, p.y))
        );
        let tip = stroke.points.last().unwrap();
        let raw_last = *coords.last().unwrap();
        assert!((tip.x - raw_last.0).abs() < 1e-4 && (tip.y - raw_last.1).abs() < 1e-4);
    }

    #[test]
    fn pen_up_zero_pressure_keeps_tip_width() {
        // Tip-up with pressure 0 must not write a needle endpoint.
        let mut engine = InkEngine::new(InkConfig::default());
        assert!(matches!(
            engine.update_now(&InputEvent::PenDown(sample(0.0, 0.0, 0.7))),
            InkUpdate::Started
        ));
        // Need enough travel for a span to form before PenUp.
        assert!(matches!(
            engine.update_now(&InputEvent::PenMove(sample(20.0, 0.0, 0.7))),
            InkUpdate::Idle | InkUpdate::Extended { .. }
        ));
        assert!(matches!(
            engine.update_now(&InputEvent::PenMove(sample(40.0, 0.0, 0.7))),
            InkUpdate::Idle | InkUpdate::Extended { .. }
        ));
        match engine.update_now(&InputEvent::PenUp(sample(60.0, 0.0, 0.0))) {
            InkUpdate::Finished { stroke, .. } => {
                let tip = stroke.points.last().unwrap();
                assert!(
                    tip.pressure >= 0.5,
                    "tip pressure {} must hold contact width, not collapse",
                    tip.pressure
                );
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[test]
    fn last_point_tracks_accepted_samples() {
        let mut engine = InkEngine::new(InkConfig::default());
        assert!(engine.last_point().is_none());
        engine.update_now(&InputEvent::PenDown(sample(10.0, 20.0, 0.5)));
        let p0 = engine.last_point().expect("after PenDown");
        assert!((p0.x - 10.0).abs() < 1e-4 && (p0.y - 20.0).abs() < 1e-4);
        engine.update_now(&InputEvent::PenMove(sample(30.0, 40.0, 0.5)));
        let p1 = engine.last_point().expect("after PenMove");
        assert!(p1.x.is_finite() && p1.y.is_finite());
        let _ = engine.update_now(&InputEvent::PenUp(sample(50.0, 60.0, 0.5)));
        assert!(engine.last_point().is_none());
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
        assert!(matches!(
            engine.update_now(&InputEvent::PenDown(sample(0.0, 0.0, 0.5))),
            InkUpdate::Started
        ));
        for _ in 0..5 {
            // Same coordinate: below min_sample_dist -> dropped.
            assert_eq!(
                engine.update_now(&InputEvent::PenMove(sample(0.0, 0.0, 0.5))),
                InkUpdate::Idle
            );
        }
        let fin = engine.update_now(&InputEvent::PenUp(sample(0.05, 0.05, 0.5)));
        match fin {
            InkUpdate::Finished { stroke, .. } => {
                assert!(
                    stroke.points.len() >= 2,
                    "endpoints must be recorded even if moves dropped"
                );
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[test]
    fn single_tap_down_up_produces_a_stroke() {
        // Degenerate stroke: PenDown immediately followed by PenUp.
        let mut engine = InkEngine::new(InkConfig::default());
        assert!(matches!(
            engine.update_now(&InputEvent::PenDown(sample(3.0, 4.0, 0.5))),
            InkUpdate::Started
        ));
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
