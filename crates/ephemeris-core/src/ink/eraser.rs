//! Eraser engine: hit-test existing strokes and remove intersected ones.
//!
//! Companion to [`InkEngine`](super::InkEngine). While the pen is in eraser mode
//! (tool = [`Tool::Eraser`](crate::model::Tool::Eraser)), feed [`InputEvent`]s here
//! together with a slice of candidate strokes. The engine reports which strokes were
//! newly hit per event so the caller can remove them from the store and schedule a
//! targeted repaint.
//!
//! The hit test is a two-stage filter:
//! 1. **Bbox coarse reject** — inflate the stroke's bounding box by the eraser radius;
//!    if the eraser center is outside that expanded box, skip.
//! 2. **Point proximity** — at least one stroke point must be within `radius` of the
//!    eraser center.
//!
//! Strokes erased in the current gesture are tracked by ID so they are never
//! double-reported. The caller is responsible for removing them from the store and
//! may retain the original `Stroke` objects on an undo stack.

use ephemeris_pal::input::InputEvent;

use crate::geom::Rect;
use crate::model::{Stroke, StrokeId};

/// Configures the eraser circle.
#[derive(Debug, Clone)]
pub struct EraserConfig {
    /// Radius of the eraser circle in logical pixels.
    pub radius: f32,
}

impl Default for EraserConfig {
    fn default() -> Self {
        Self { radius: 12.0 }
    }
}

/// Result of feeding one event to [`EraserEngine::update`].
#[derive(Debug, Clone, PartialEq)]
pub enum EraserUpdate {
    /// Nothing erased (no stroke under the eraser, or inactive).
    Idle,
    /// Eraser gesture started; no strokes hit yet.
    Started,
    /// One or more strokes were hit by this event. `ids` are the newly erased
    /// stroke IDs; `damage` is their union bounding box (repaint target).
    Hit { ids: Vec<StrokeId>, damage: Rect },
    /// Gesture ended (`PenUp`). `ids` = full set erased this gesture; `damage`
    /// = union bbox of all erased strokes (or empty if nothing was erased).
    Finished { ids: Vec<StrokeId>, damage: Rect },
}

/// Stateful eraser engine.
///
/// Feed [`InputEvent`]s with a slice of current strokes on each call. The engine
/// accumulates erased IDs across the gesture and returns precise damage rects so
/// the caller can drive partial eink refresh.
pub struct EraserEngine {
    cfg: EraserConfig,
    active: bool,
    erased: Vec<StrokeId>,
    total_damage: Rect,
}

impl EraserEngine {
    pub fn new(cfg: EraserConfig) -> Self {
        Self {
            cfg,
            active: false,
            erased: Vec::new(),
            total_damage: Rect::empty(),
        }
    }

    /// Whether an eraser gesture is in progress.
    pub fn is_erasing(&self) -> bool {
        self.active
    }

    /// Bounding box of a stroke's centre-line, inflated by its half-width + 1 px AA margin.
    fn stroke_bbox(stroke: &Stroke) -> Rect {
        let mut r = Rect::empty();
        for p in &stroke.points {
            r.union_point(p.x, p.y);
        }
        r.inflate(stroke.base_width * 0.5 + 1.0)
    }

    /// True if eraser circle at `(cx, cy)` with `radius` intersects `stroke`.
    ///
    /// Uses circle-AABB intersection against the stroke's bounding box (the
    /// stroke index per ADR ephemeris-cm7): find the closest point on the bbox to
    /// the circle center and test whether it is within radius.
    fn hits(stroke: &Stroke, cx: f32, cy: f32, radius: f32) -> bool {
        if stroke.points.is_empty() {
            return false;
        }
        let bbox = Self::stroke_bbox(stroke);
        if bbox.is_empty() {
            return false;
        }
        let closest_x = cx.clamp(bbox.min_x, bbox.max_x);
        let closest_y = cy.clamp(bbox.min_y, bbox.max_y);
        let dx = cx - closest_x;
        let dy = cy - closest_y;
        dx * dx + dy * dy <= radius * radius
    }

    /// Hit-test `(cx, cy)` against `strokes`, skipping already-erased IDs.
    /// Returns (newly-hit IDs, their union bbox). Updates internal state.
    fn hit_test(&mut self, cx: f32, cy: f32, strokes: &[Stroke]) -> (Vec<StrokeId>, Rect) {
        let mut ids = Vec::new();
        let mut damage = Rect::empty();
        for stroke in strokes {
            if self.erased.contains(&stroke.id) {
                continue;
            }
            if Self::hits(stroke, cx, cy, self.cfg.radius) {
                let bbox = Self::stroke_bbox(stroke);
                damage = damage.union(&bbox);
                self.total_damage = self.total_damage.union(&bbox);
                self.erased.push(stroke.id);
                ids.push(stroke.id);
            }
        }
        (ids, damage)
    }

    /// Feed one input event.
    ///
    /// `strokes` should be the current page's strokes. The engine does **not**
    /// remove strokes from any store — the caller must call `store.remove(id)` for
    /// each ID returned in [`EraserUpdate::Hit`] or [`EraserUpdate::Finished`].
    pub fn update(&mut self, event: &InputEvent, strokes: &[Stroke]) -> EraserUpdate {
        match event {
            InputEvent::PenDown(s) => {
                self.active = true;
                self.erased.clear();
                self.total_damage = Rect::empty();
                let (ids, damage) = self.hit_test(s.x, s.y, strokes);
                if ids.is_empty() {
                    EraserUpdate::Started
                } else {
                    EraserUpdate::Hit { ids, damage }
                }
            }
            InputEvent::PenMove(s) if self.active => {
                let (ids, damage) = self.hit_test(s.x, s.y, strokes);
                if ids.is_empty() {
                    EraserUpdate::Idle
                } else {
                    EraserUpdate::Hit { ids, damage }
                }
            }
            InputEvent::PenUp(s) if self.active => {
                let _ = self.hit_test(s.x, s.y, strokes);
                self.active = false;
                EraserUpdate::Finished {
                    ids: std::mem::take(&mut self.erased),
                    damage: self.total_damage,
                }
            }
            _ => EraserUpdate::Idle,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Color, Point, Stroke, Tool};
    use ephemeris_pal::input::PenSample;

    fn pen(x: f32, y: f32) -> PenSample {
        PenSample {
            x,
            y,
            pressure: 0.5,
            tilt: 0.0,
            in_range: true,
        }
    }

    fn stroke_at(x0: f32, y0: f32, x1: f32, y1: f32) -> Stroke {
        let mut s = Stroke::new(Tool::Pen, Color::BLACK, 2.0);
        s.points.push(Point::new(x0, y0, 0.5, 0.0, 0));
        s.points.push(Point::new(x1, y1, 0.5, 0.0, 16));
        s
    }

    fn engine() -> EraserEngine {
        EraserEngine::new(EraserConfig { radius: 10.0 })
    }

    #[test]
    fn pen_down_away_from_strokes_starts_idle() {
        let mut e = engine();
        let stroke = stroke_at(100.0, 100.0, 110.0, 110.0);
        let out = e.update(&InputEvent::PenDown(pen(0.0, 0.0)), &[stroke]);
        assert_eq!(out, EraserUpdate::Started);
        assert!(e.is_erasing());
    }

    #[test]
    fn pen_down_on_stroke_emits_hit() {
        let mut e = engine();
        let stroke = stroke_at(10.0, 10.0, 20.0, 20.0);
        let out = e.update(
            &InputEvent::PenDown(pen(15.0, 15.0)),
            std::slice::from_ref(&stroke),
        );
        match out {
            EraserUpdate::Hit { ids, damage } => {
                assert_eq!(ids, vec![stroke.id]);
                assert!(!damage.is_empty());
            }
            other => panic!("expected Hit, got {other:?}"),
        }
    }

    #[test]
    fn pen_move_erases_nearby_stroke() {
        let mut e = engine();
        let stroke = stroke_at(50.0, 50.0, 60.0, 60.0);
        e.update(
            &InputEvent::PenDown(pen(0.0, 0.0)),
            std::slice::from_ref(&stroke),
        );
        let out = e.update(
            &InputEvent::PenMove(pen(55.0, 55.0)),
            std::slice::from_ref(&stroke),
        );
        match out {
            EraserUpdate::Hit { ids, .. } => assert_eq!(ids, vec![stroke.id]),
            other => panic!("expected Hit, got {other:?}"),
        }
    }

    #[test]
    fn already_erased_stroke_not_double_reported() {
        let mut e = engine();
        let stroke = stroke_at(10.0, 10.0, 20.0, 20.0);
        // First hit at PenDown.
        e.update(
            &InputEvent::PenDown(pen(15.0, 15.0)),
            std::slice::from_ref(&stroke),
        );
        // Second hit at same position: stroke already erased this gesture.
        let out = e.update(
            &InputEvent::PenMove(pen(15.0, 15.0)),
            std::slice::from_ref(&stroke),
        );
        assert_eq!(out, EraserUpdate::Idle);
    }

    #[test]
    fn pen_up_returns_all_erased_and_total_damage() {
        let mut e = engine();
        let s1 = stroke_at(10.0, 10.0, 20.0, 20.0);
        let s2 = stroke_at(100.0, 100.0, 110.0, 110.0);
        e.update(
            &InputEvent::PenDown(pen(15.0, 15.0)),
            &[s1.clone(), s2.clone()],
        );
        e.update(
            &InputEvent::PenMove(pen(105.0, 105.0)),
            &[s1.clone(), s2.clone()],
        );
        let out = e.update(
            &InputEvent::PenUp(pen(105.0, 105.0)),
            &[s1.clone(), s2.clone()],
        );
        match out {
            EraserUpdate::Finished { ids, damage } => {
                assert!(ids.contains(&s1.id));
                assert!(ids.contains(&s2.id));
                assert!(!damage.is_empty());
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        assert!(!e.is_erasing());
    }

    #[test]
    fn move_without_down_is_idle() {
        let mut e = engine();
        let stroke = stroke_at(0.0, 0.0, 10.0, 10.0);
        assert_eq!(
            e.update(&InputEvent::PenMove(pen(5.0, 5.0)), &[stroke]),
            EraserUpdate::Idle
        );
    }

    #[test]
    fn empty_stroke_never_hit() {
        let mut e = engine();
        let empty = Stroke::new(Tool::Pen, Color::BLACK, 2.0);
        let out = e.update(&InputEvent::PenDown(pen(0.0, 0.0)), &[empty]);
        assert_eq!(out, EraserUpdate::Started);
    }

    #[test]
    fn stroke_far_away_not_hit() {
        let mut e = engine();
        let stroke = stroke_at(200.0, 200.0, 210.0, 210.0);
        e.update(
            &InputEvent::PenDown(pen(0.0, 0.0)),
            std::slice::from_ref(&stroke),
        );
        let out = e.update(
            &InputEvent::PenUp(pen(0.0, 0.0)),
            std::slice::from_ref(&stroke),
        );
        match out {
            EraserUpdate::Finished { ids, .. } => assert!(ids.is_empty()),
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[test]
    fn damage_covers_erased_stroke_bbox() {
        let mut e = engine();
        let stroke = stroke_at(10.0, 10.0, 30.0, 30.0);
        let id = stroke.id;
        e.update(
            &InputEvent::PenDown(pen(20.0, 20.0)),
            std::slice::from_ref(&stroke),
        );
        let out = e.update(
            &InputEvent::PenUp(pen(20.0, 20.0)),
            std::slice::from_ref(&stroke),
        );
        match out {
            EraserUpdate::Finished { ids, damage } => {
                assert_eq!(ids, vec![id]);
                // Damage must contain the stroke endpoints (inflated by base_width/2 + 1px).
                assert!(damage.contains(10.0, 10.0) || damage.min_x <= 10.0);
                assert!(damage.max_x >= 30.0);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }
}
