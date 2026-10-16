//! Lasso selection helpers for note ink (ephemeris-u9u).
//!
//! Point-in-polygon containment used to select strokes whose centre-line
//! intersects a freehand loop, plus a small translate helper that preserves
//! [`StrokeId`]s (required so recognition line hashes stay stable when a
//! whole line is moved).

use crate::geom::Rect;
use crate::model::{Stroke, StrokeId, Tool};

/// Minimum vertices for a usable lasso (triangle).
const MIN_LASSO_POINTS: usize = 3;

/// Fraction of centre-line samples that must lie inside the polygon for a
/// stroke to count as selected (avoids grabbing a stroke that only grazes).
const INSIDE_FRAC: f32 = 0.35;

/// Ray-casting point-in-polygon (even-odd). Boundary points may fall either way.
pub fn point_in_polygon(x: f32, y: f32, poly: &[(f32, f32)]) -> bool {
    if poly.len() < MIN_LASSO_POINTS {
        return false;
    }
    let mut inside = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let (xi, yi) = poly[i];
        let (xj, yj) = poly[j];
        let intersect =
            ((yi > y) != (yj > y)) && (x < (xj - xi) * (y - yi) / (yj - yi + f32::EPSILON) + xi);
        if intersect {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Axis-aligned bbox of a polyline.
pub fn polyline_bbox(poly: &[(f32, f32)]) -> Rect {
    let mut r = Rect::empty();
    for &(x, y) in poly {
        r.union_point(x, y);
    }
    r
}

/// Close the lasso by appending the first point if the ends don't already meet.
pub fn ensure_closed(poly: &[(f32, f32)]) -> Vec<(f32, f32)> {
    if poly.len() < 2 {
        return poly.to_vec();
    }
    let mut out = poly.to_vec();
    // Lasso selection implies a loop, so always close it — even a wide gap.
    if dist(out[0], *out.last().unwrap()) > 1.0 {
        out.push(out[0]);
    }
    out
}

fn dist(a: (f32, f32), b: (f32, f32)) -> f32 {
    let dx = a.0 - b.0;
    let dy = a.1 - b.1;
    (dx * dx + dy * dy).sqrt()
}

/// True if `stroke` is sufficiently inside `lasso` (already closed preferred).
pub fn stroke_inside_lasso(stroke: &Stroke, lasso: &[(f32, f32)]) -> bool {
    if stroke.points.is_empty() || lasso.len() < MIN_LASSO_POINTS {
        return false;
    }
    // Highlighter is selectable (moved with ink); eraser strokes are never stored.
    if stroke.tool == Tool::Eraser {
        return false;
    }
    let lasso_bb = polyline_bbox(lasso);
    let sb = stroke_centre_bbox(stroke);
    if sb.is_empty()
        || sb.max_x < lasso_bb.min_x
        || sb.min_x > lasso_bb.max_x
        || sb.max_y < lasso_bb.min_y
        || sb.min_y > lasso_bb.max_y
    {
        return false;
    }
    let n = stroke.points.len();
    let step = (n / 32).max(1);
    let mut inside = 0usize;
    let mut tested = 0usize;
    for p in stroke.points.iter().step_by(step) {
        tested += 1;
        if point_in_polygon(p.x, p.y, lasso) {
            inside += 1;
        }
    }
    if tested == 0 {
        return false;
    }
    (inside as f32 / tested as f32) >= INSIDE_FRAC
}

fn stroke_centre_bbox(stroke: &Stroke) -> Rect {
    let mut r = Rect::empty();
    for p in &stroke.points {
        r.union_point(p.x, p.y);
    }
    r
}

/// IDs of strokes that lie inside the lasso loop.
pub fn strokes_inside_lasso(strokes: &[Stroke], lasso: &[(f32, f32)]) -> Vec<StrokeId> {
    let closed = ensure_closed(lasso);
    if closed.len() < MIN_LASSO_POINTS {
        return Vec::new();
    }
    strokes
        .iter()
        .filter(|s| stroke_inside_lasso(s, &closed))
        .map(|s| s.id)
        .collect()
}

/// Translate stroke centre-line points in place. Preserves [`StrokeId`], tool,
/// colour, and width so recognition line hashes remain stable when a whole
/// line moves together.
pub fn translate_stroke(stroke: &mut Stroke, dx: f32, dy: f32) {
    if dx == 0.0 && dy == 0.0 {
        return;
    }
    for p in &mut stroke.points {
        p.x += dx;
        p.y += dy;
    }
}

/// Translate every stroke whose id is in `ids`. Returns how many were moved.
pub fn translate_strokes_by_id(
    strokes: &mut [Stroke],
    ids: &[StrokeId],
    dx: f32,
    dy: f32,
) -> usize {
    let mut n = 0;
    for s in strokes.iter_mut() {
        if ids.contains(&s.id) {
            translate_stroke(s, dx, dy);
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Color, Point, Stroke};

    fn poly_square() -> Vec<(f32, f32)> {
        vec![
            (0.0, 0.0),
            (100.0, 0.0),
            (100.0, 100.0),
            (0.0, 100.0),
            (0.0, 0.0),
        ]
    }

    fn stroke_at(id_seed: u8, pts: &[(f32, f32)]) -> Stroke {
        let mut s = Stroke::new(Tool::Pen, Color::BLACK, 3.0);
        // Deterministic-ish id from seed for assert equality.
        s.id = StrokeId(uuid::Uuid::from_bytes([
            id_seed, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, id_seed,
        ]));
        s.points = pts
            .iter()
            .enumerate()
            .map(|(i, &(x, y))| Point::new(x, y, 0.5, 0.0, i as u32 * 10))
            .collect();
        s
    }

    #[test]
    fn point_in_square() {
        let p = poly_square();
        assert!(point_in_polygon(50.0, 50.0, &p));
        assert!(!point_in_polygon(150.0, 50.0, &p));
        assert!(!point_in_polygon(-1.0, 50.0, &p));
    }

    #[test]
    fn selects_stroke_inside_lasso() {
        let inside = stroke_at(1, &[(40.0, 40.0), (60.0, 40.0), (60.0, 60.0)]);
        let outside = stroke_at(2, &[(200.0, 200.0), (220.0, 200.0)]);
        let ids = strokes_inside_lasso(&[inside.clone(), outside], &poly_square());
        assert_eq!(ids, vec![inside.id]);
    }

    #[test]
    fn rejects_grazing_stroke() {
        // Only one endpoint inside — below INSIDE_FRAC for a 2-point stroke
        // with step 1 → 50% would pass; use a longer stroke mostly outside.
        let mut pts = vec![(50.0, 50.0)];
        for i in 0..20 {
            pts.push((150.0 + i as f32, 50.0));
        }
        let grazing = stroke_at(3, &pts);
        let ids = strokes_inside_lasso(&[grazing], &poly_square());
        assert!(ids.is_empty());
    }

    #[test]
    fn translate_preserves_id() {
        let mut s = stroke_at(7, &[(10.0, 10.0), (20.0, 10.0)]);
        let id = s.id;
        translate_stroke(&mut s, 5.0, -3.0);
        assert_eq!(s.id, id);
        assert!((s.points[0].x - 15.0).abs() < 1e-5);
        assert!((s.points[0].y - 7.0).abs() < 1e-5);
    }

    #[test]
    fn ensure_closed_appends_start() {
        let open = vec![(0.0, 0.0), (50.0, 0.0), (50.0, 50.0), (2.0, 2.0)];
        let closed = ensure_closed(&open);
        assert_eq!(closed.first(), closed.last());
    }
}
