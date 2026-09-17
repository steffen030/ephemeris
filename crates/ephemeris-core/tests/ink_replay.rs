//! Acceptance tests for the ink engine (ephemeris-6iy.1).
//!
//! These drive the engine end-to-end from a replayed pen stream (via the pal
//! `MockInput` backend) and assert the three acceptance criteria:
//!
//! 1. Replaying a recorded pen stream produces a *smooth* stroke.
//! 2. Only the stroke's bbox (not the whole canvas) is reported as damage.
//! 3. Completed strokes persist and reload identically.

use ephemeris_core::geom::Rect;
use ephemeris_core::ink::{InkConfig, InkEngine, InkUpdate};
use ephemeris_core::model::Stroke;
use ephemeris_core::store::{MemoryStore, StrokeStore};

use ephemeris_pal::input::mock::MockInput;
use ephemeris_pal::input::{Input, InputEvent};

/// A jagged, jittery recorded stroke: a rough diagonal with alternating noise
/// on top. Smoothing should turn this into a clean path.
fn recorded_jittery_points() -> Vec<(f32, f32)> {
    (0..40)
        .map(|i| {
            let t = i as f32;
            let jitter = if i % 2 == 0 { 1.2 } else { -1.2 };
            (t * 3.0 + jitter, t * 2.0 - jitter)
        })
        .collect()
}

/// Run a recorded pen stream through the engine, collecting per-event updates
/// and the finished stroke.
fn replay(points: Vec<(f32, f32)>, pressure: f32) -> (Vec<InkUpdate>, Stroke) {
    // Replay the recorded stream through the real pal mock backend.
    let backend = MockInput::pen_stroke(points, pressure, 0.0);
    let rx = backend.receiver();
    backend.run().expect("mock replay failed");
    let events: Vec<InputEvent> = rx.try_iter().collect();

    let mut engine = InkEngine::new(InkConfig::default());
    let mut updates = Vec::new();
    let mut finished: Option<Stroke> = None;

    for ev in &events {
        match engine.update_now(ev) {
            InkUpdate::Finished { stroke, damage } => {
                updates.push(InkUpdate::Finished { stroke: stroke.clone(), damage });
                finished = Some(*stroke);
            }
            other => updates.push(other),
        }
    }

    (updates, finished.expect("stream should finish a stroke"))
}

/// Total turning ("angle change") along a polyline — a proxy for jaggedness.
fn total_turning(points: &[(f32, f32)]) -> f32 {
    let mut total = 0.0;
    for w in points.windows(3) {
        let (a, b, c) = (w[0], w[1], w[2]);
        let v1 = (b.0 - a.0, b.1 - a.1);
        let v2 = (c.0 - b.0, c.1 - b.1);
        let l1 = (v1.0 * v1.0 + v1.1 * v1.1).sqrt();
        let l2 = (v2.0 * v2.0 + v2.1 * v2.1).sqrt();
        if l1 < 1e-6 || l2 < 1e-6 {
            continue;
        }
        let dot = (v1.0 * v2.0 + v1.1 * v2.1) / (l1 * l2);
        total += dot.clamp(-1.0, 1.0).acos();
    }
    total
}

#[test]
fn replaying_recorded_stream_produces_smooth_stroke() {
    let raw = recorded_jittery_points();
    let (_updates, stroke) = replay(raw.clone(), 0.6);

    assert!(stroke.points.len() >= 2, "stroke should have samples");

    let raw_turning = total_turning(&raw);
    let stroke_xy: Vec<(f32, f32)> = stroke.points.iter().map(|p| (p.x, p.y)).collect();
    let smooth_turning = total_turning(&stroke_xy);

    // The smoothed stroke must have dramatically less cumulative turning than
    // the jittery raw input (jitter creates lots of little reversals).
    assert!(
        smooth_turning < raw_turning * 0.5,
        "smoothed turning {smooth_turning} should be well below raw {raw_turning}"
    );

    // And it should still roughly follow the original: endpoints close to raw.
    let (fx, fy) = (stroke.points.first().unwrap().x, stroke.points.first().unwrap().y);
    let (lx, ly) = (stroke.points.last().unwrap().x, stroke.points.last().unwrap().y);
    assert!((fx - raw[0].0).abs() < 5.0 && (fy - raw[0].1).abs() < 5.0);
    let last_raw = *raw.last().unwrap();
    assert!((lx - last_raw.0).abs() < 5.0 && (ly - last_raw.1).abs() < 5.0);
}

#[test]
fn only_stroke_bbox_is_reported_as_damage() {
    let raw = recorded_jittery_points();
    let (updates, stroke) = replay(raw, 0.6);

    // The stroke's own bounding box (centre-line), inflated by a generous
    // brush margin. Every reported damage rect must lie inside this — never the
    // whole canvas.
    let mut stroke_bbox = Rect::empty();
    for p in &stroke.points {
        stroke_bbox.union_point(p.x, p.y);
    }
    // Allow for the brush half-width + AA margin used by the engine.
    let allowed = stroke_bbox.inflate(stroke.base_width * 2.0 + 2.0);

    // A hypothetical full canvas is much larger than the stroke.
    let canvas = Rect::new(0.0, 0.0, 2000.0, 1500.0);
    assert!(
        allowed.width() < canvas.width() * 0.5 && allowed.height() < canvas.height() * 0.5,
        "sanity: stroke should be far smaller than the canvas"
    );

    let mut saw_damage = false;
    let mut union = Rect::empty();
    for u in &updates {
        let damage = match u {
            InkUpdate::Extended { damage } => Some(*damage),
            InkUpdate::Finished { damage, .. } => Some(*damage),
            _ => None,
        };
        if let Some(d) = damage {
            saw_damage = true;
            union = union.union(&d);
            // Each incremental damage rect stays within the stroke bbox region.
            assert!(
                d.min_x >= allowed.min_x - 1e-3
                    && d.min_y >= allowed.min_y - 1e-3
                    && d.max_x <= allowed.max_x + 1e-3
                    && d.max_y <= allowed.max_y + 1e-3,
                "damage {d:?} escaped the stroke bbox {allowed:?}"
            );
        }
    }

    assert!(saw_damage, "engine must report incremental damage while drawing");

    // The union of all damage must NOT cover the whole canvas.
    assert!(
        union.width() <= allowed.width() + 1e-3 && union.height() <= allowed.height() + 1e-3,
        "total damage union {union:?} exceeded the stroke bbox {allowed:?}"
    );
    assert!(
        union.width() < canvas.width() && union.height() < canvas.height(),
        "total damage must be smaller than the full canvas"
    );
}

#[test]
fn first_event_starts_and_last_finishes() {
    let raw = recorded_jittery_points();
    let (updates, _stroke) = replay(raw, 0.6);

    assert!(matches!(updates.first().unwrap(), InkUpdate::Started));
    assert!(matches!(updates.last().unwrap(), InkUpdate::Finished { .. }));
    // Exactly one stroke finished.
    let finishes = updates
        .iter()
        .filter(|u| matches!(u, InkUpdate::Finished { .. }))
        .count();
    assert_eq!(finishes, 1);
}

#[test]
fn completed_stroke_persists_and_reloads() {
    let raw = recorded_jittery_points();
    let (_updates, stroke) = replay(raw, 0.6);

    // Persist via the Store.
    let mut store = MemoryStore::new();
    store.save(&stroke).unwrap();

    // Reload via a fresh store using the serialized form (simulating restart).
    let json = store.to_json().unwrap();
    let reloaded = MemoryStore::from_json(&json).unwrap();

    let back = reloaded.load(stroke.id).unwrap();
    assert_eq!(back, stroke, "reloaded stroke must be identical");
    assert_eq!(reloaded.load_all().unwrap(), vec![stroke]);
}

#[test]
fn move_without_down_is_ignored() {
    let mut engine = InkEngine::new(InkConfig::default());
    let sample = ephemeris_pal::input::PenSample {
        x: 5.0,
        y: 5.0,
        pressure: 0.5,
        tilt: 0.0,
        in_range: true,
    };
    let u = engine.update_now(&InputEvent::PenMove(sample));
    assert_eq!(u, InkUpdate::Idle);
    assert!(!engine.is_drawing());
}

#[test]
fn hover_and_touch_do_not_draw() {
    let mut engine = InkEngine::new(InkConfig::default());
    assert_eq!(
        engine.update_now(&InputEvent::Hover { x: 1.0, y: 2.0 }),
        InkUpdate::Idle
    );
    assert_eq!(
        engine.update_now(&InputEvent::PenButton { pressed: true }),
        InkUpdate::Idle
    );
    assert!(!engine.is_drawing());
}
