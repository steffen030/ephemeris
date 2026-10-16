//! Hold-to-snap smart shapes for note ink (ephemeris-9co).
//!
//! Given a freehand stroke polyline, try to recognise a single-stroke
//! rectangle, triangle, ellipse/circle, or straight line and return a clean
//! replacement polyline. Low-confidence fits return [`None`] so handwriting
//! is never force-squared.

use crate::model::Point;

/// Recognised geometric primitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeKind {
    Line,
    Rectangle,
    Triangle,
    Ellipse,
}

/// Result of a successful fit.
#[derive(Debug, Clone, PartialEq)]
pub struct FittedShape {
    pub kind: ShapeKind,
    /// Replacement centre-line samples (closed shapes repeat the first point
    /// at the end). Pressure / tilt / `t_ms` are filled from the source stroke.
    pub points: Vec<Point>,
    /// Rough quality in `[0, 1]`; callers typically require ≥ [`MIN_CONFIDENCE`].
    pub confidence: f32,
}

/// Minimum confidence to accept a snap (handwriting stays freehand below this).
pub const MIN_CONFIDENCE: f32 = 0.72;

/// Shortest usable path length in logical pixels.
const MIN_PATH_LEN: f32 = 40.0;

/// Minimum samples after light decimation.
const MIN_SAMPLES: usize = 6;

/// Endpoints within this fraction of path length ⇒ closed shape.
const CLOSURE_FRAC: f32 = 0.12;

/// Samples along generated perimeters (enough for smooth raster capsules).
const PERIM_SAMPLES_RECT: usize = 48;
const PERIM_SAMPLES_TRI: usize = 36;
const PERIM_SAMPLES_ELLIPSE: usize = 64;
const LINE_SAMPLES: usize = 24;

/// Best-so-far fit: kind, confidence, replacement polyline.
type Candidate = (ShapeKind, f32, Vec<(f32, f32)>);

/// Try to snap `raw` into a clean shape. Returns [`None`] when the stroke
/// looks like handwriting or no candidate clears [`MIN_CONFIDENCE`].
pub fn fit_smart_shape(raw: &[Point]) -> Option<FittedShape> {
    if raw.len() < MIN_SAMPLES {
        return None;
    }
    let xy: Vec<(f32, f32)> = raw.iter().map(|p| (p.x, p.y)).collect();
    let path_len = polyline_length(&xy);
    if path_len < MIN_PATH_LEN {
        return None;
    }

    let (pressure, tilt) = typical_attrs(raw);
    let closed = dist(xy[0], *xy.last().unwrap()) <= path_len * CLOSURE_FRAC;

    let mut best: Option<Candidate> = None;
    let consider =
        |best: &mut Option<Candidate>, kind: ShapeKind, conf: f32, pts: Vec<(f32, f32)>| {
            if conf < MIN_CONFIDENCE || pts.len() < 2 {
                return;
            }
            match best {
                None => *best = Some((kind, conf, pts)),
                Some((_, c, _)) if conf > *c => *best = Some((kind, conf, pts)),
                _ => {}
            }
        };

    if let Some((conf, pts)) = try_line(&xy, closed) {
        consider(&mut best, ShapeKind::Line, conf, pts);
    }
    if closed {
        if let Some((conf, pts)) = try_rectangle(&xy) {
            consider(&mut best, ShapeKind::Rectangle, conf, pts);
        }
        if let Some((conf, pts)) = try_triangle(&xy) {
            consider(&mut best, ShapeKind::Triangle, conf, pts);
        }
        if let Some((conf, pts)) = try_ellipse(&xy) {
            consider(&mut best, ShapeKind::Ellipse, conf, pts);
        }
    }

    let (kind, confidence, pts) = best?;
    Some(FittedShape {
        kind,
        points: stamp_points(&pts, pressure, tilt),
        confidence,
    })
}

fn typical_attrs(raw: &[Point]) -> (f32, f32) {
    let n = raw.len() as f32;
    let pressure = (raw.iter().map(|p| p.pressure).sum::<f32>() / n).clamp(0.05, 1.0);
    let tilt = raw.iter().map(|p| p.tilt).sum::<f32>() / n;
    (pressure, tilt)
}

fn stamp_points(xy: &[(f32, f32)], pressure: f32, tilt: f32) -> Vec<Point> {
    let n = xy.len().max(2);
    xy.iter()
        .enumerate()
        .map(|(i, &(x, y))| {
            let t_ms = ((i as f32 / (n - 1) as f32) * 1000.0).round() as u32;
            Point::new(x, y, pressure, tilt, t_ms)
        })
        .collect()
}

fn dist(a: (f32, f32), b: (f32, f32)) -> f32 {
    let dx = a.0 - b.0;
    let dy = a.1 - b.1;
    (dx * dx + dy * dy).sqrt()
}

fn polyline_length(xy: &[(f32, f32)]) -> f32 {
    xy.windows(2).map(|w| dist(w[0], w[1])).sum()
}

fn bbox(xy: &[(f32, f32)]) -> (f32, f32, f32, f32) {
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for &(x, y) in xy {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    (min_x, min_y, max_x, max_y)
}

/// Mean perpendicular residual normalised by `scale`, mapped to confidence.
fn residual_confidence(xy: &[(f32, f32)], dist_fn: impl Fn((f32, f32)) -> f32, scale: f32) -> f32 {
    if xy.is_empty() || scale < 1.0 {
        return 0.0;
    }
    let mean: f32 = xy.iter().map(|&p| dist_fn(p)).sum::<f32>() / xy.len() as f32;
    let norm = (mean / scale).clamp(0.0, 1.0);
    (1.0 - norm * 2.5).clamp(0.0, 1.0)
}

fn try_line(xy: &[(f32, f32)], closed: bool) -> Option<(f32, Vec<(f32, f32)>)> {
    // Closed loops are almost never intended as lines.
    if closed {
        return None;
    }
    let path_len = polyline_length(xy);
    let chord = dist(xy[0], *xy.last().unwrap());
    // Handwriting wanders: path much longer than endpoint chord.
    if chord < MIN_PATH_LEN * 0.8 || path_len / chord.max(1.0) > 1.15 {
        return None;
    }
    let (cx, cy) = centroid(xy);
    let mut sxx = 0.0f32;
    let mut syy = 0.0f32;
    let mut sxy = 0.0f32;
    for &(x, y) in xy {
        let dx = x - cx;
        let dy = y - cy;
        sxx += dx * dx;
        syy += dy * dy;
        sxy += dx * dy;
    }
    // Principal axis angle via covariance eigen.
    let diff = sxx - syy;
    let angle = 0.5 * (2.0 * sxy).atan2(diff);
    let (ux, uy) = (angle.cos(), angle.sin());
    // Project endpoints of the point cloud onto the axis.
    let mut t_min = f32::INFINITY;
    let mut t_max = f32::NEG_INFINITY;
    let mut max_perp = 0.0f32;
    for &(x, y) in xy {
        let t = (x - cx) * ux + (y - cy) * uy;
        t_min = t_min.min(t);
        t_max = t_max.max(t);
        max_perp = max_perp.max(((x - cx) * -uy + (y - cy) * ux).abs());
    }
    let len = t_max - t_min;
    if len < MIN_PATH_LEN * 0.8 {
        return None;
    }
    // Reject wavy strokes even if mean residual looks ok.
    if max_perp > len * 0.06 {
        return None;
    }
    let conf = residual_confidence(xy, |(x, y)| ((x - cx) * -uy + (y - cy) * ux).abs(), len);
    if conf < MIN_CONFIDENCE {
        return None;
    }
    let a = (cx + ux * t_min, cy + uy * t_min);
    let b = (cx + ux * t_max, cy + uy * t_max);
    // Keep stroke direction similar to drawing order.
    let (start, end) = if dist(xy[0], a) <= dist(xy[0], b) {
        (a, b)
    } else {
        (b, a)
    };
    Some((conf, lerp_line(start, end, LINE_SAMPLES)))
}

fn lerp_line(a: (f32, f32), b: (f32, f32), n: usize) -> Vec<(f32, f32)> {
    let n = n.max(2);
    (0..n)
        .map(|i| {
            let t = i as f32 / (n - 1) as f32;
            (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
        })
        .collect()
}

fn centroid(xy: &[(f32, f32)]) -> (f32, f32) {
    let n = xy.len() as f32;
    let sx: f32 = xy.iter().map(|p| p.0).sum();
    let sy: f32 = xy.iter().map(|p| p.1).sum();
    (sx / n, sy / n)
}

fn try_rectangle(xy: &[(f32, f32)]) -> Option<(f32, Vec<(f32, f32)>)> {
    let corners = dominant_corners(xy, 4)?;
    if corners.len() != 4 {
        return None;
    }
    // Order corners around their centroid (CCW).
    let ordered = order_corners_ccw(&corners);
    // Side lengths and angle check (prefer near-orthogonal).
    let sides: Vec<f32> = (0..4)
        .map(|i| dist(ordered[i], ordered[(i + 1) % 4]))
        .collect();
    if sides.iter().any(|&s| s < 12.0) {
        return None;
    }
    let mut angle_score = 0.0f32;
    for i in 0..4 {
        let a = ordered[i];
        let b = ordered[(i + 1) % 4];
        let c = ordered[(i + 2) % 4];
        let v1 = (a.0 - b.0, a.1 - b.1);
        let v2 = (c.0 - b.0, c.1 - b.1);
        let n1 = (v1.0 * v1.0 + v1.1 * v1.1).sqrt().max(1e-3);
        let n2 = (v2.0 * v2.0 + v2.1 * v2.1).sqrt().max(1e-3);
        let cos = ((v1.0 * v2.0 + v1.1 * v2.1) / (n1 * n2)).clamp(-1.0, 1.0);
        // Right angle ⇒ cos ≈ 0.
        angle_score += 1.0 - cos.abs();
    }
    angle_score /= 4.0;

    // Smooth ovals also yield 4 axis extrema with ~90° chords — require
    // sharp corners (high turn) so ellipses don't win as rectangles.
    if angle_score < 0.82 {
        return None;
    }
    let perim = sides.iter().sum::<f32>();
    let ring = close_ring(&ordered);
    let conf_fit = residual_confidence(xy, |p| dist_to_polyline(p, &ring), perim);
    let max_res = xy
        .iter()
        .map(|&p| dist_to_polyline(p, &ring))
        .fold(0.0f32, f32::max);
    if max_res > perim * 0.04 {
        return None;
    }
    let conf = (0.55 * conf_fit + 0.45 * angle_score).clamp(0.0, 1.0);
    if conf < MIN_CONFIDENCE {
        return None;
    }
    // If an ellipse explains the stroke better, defer to it.
    if let Some((e_conf, _)) = try_ellipse(xy) {
        if e_conf + 0.03 >= conf {
            return None;
        }
    }
    Some((conf, sample_ring(&ordered, PERIM_SAMPLES_RECT)))
}

fn try_triangle(xy: &[(f32, f32)]) -> Option<(f32, Vec<(f32, f32)>)> {
    let corners = dominant_corners(xy, 3)?;
    if corners.len() != 3 {
        return None;
    }
    let ordered = order_corners_ccw(&corners);
    let sides: Vec<f32> = (0..3)
        .map(|i| dist(ordered[i], ordered[(i + 1) % 3]))
        .collect();
    if sides.iter().any(|&s| s < 16.0) {
        return None;
    }
    // Reject near-degenerate (almost collinear) triangles.
    let area = {
        let (x0, y0) = ordered[0];
        let (x1, y1) = ordered[1];
        let (x2, y2) = ordered[2];
        ((x0 * (y1 - y2) + x1 * (y2 - y0) + x2 * (y0 - y1)) * 0.5).abs()
    };
    let perim = sides.iter().sum::<f32>();
    if area < perim * perim * 0.02 {
        return None;
    }
    let ring = close_ring(&ordered);
    let conf = residual_confidence(xy, |p| dist_to_polyline(p, &ring), perim);
    let max_res = xy
        .iter()
        .map(|&p| dist_to_polyline(p, &ring))
        .fold(0.0f32, f32::max);
    // Smooth ovals can RDP to 3 points; reject when max residual is large.
    if max_res > perim * 0.045 {
        return None;
    }
    if conf < MIN_CONFIDENCE {
        return None;
    }
    if let Some((e_conf, _)) = try_ellipse(xy) {
        if e_conf + 0.03 >= conf {
            return None;
        }
    }
    Some((conf, sample_ring(&ordered, PERIM_SAMPLES_TRI)))
}

fn try_ellipse(xy: &[(f32, f32)]) -> Option<(f32, Vec<(f32, f32)>)> {
    let (min_x, min_y, max_x, max_y) = bbox(xy);
    let rx = (max_x - min_x) * 0.5;
    let ry = (max_y - min_y) * 0.5;
    if rx < 12.0 || ry < 12.0 {
        return None;
    }
    let cx = (min_x + max_x) * 0.5;
    let cy = (min_y + max_y) * 0.5;
    // Prefer roughly elliptical / circular occupancy: path length ≈ ellipse perimeter.
    let perim_est =
        std::f32::consts::PI * (3.0 * (rx + ry) - ((2.0 * rx + ry) * (rx + 2.0 * ry)).sqrt());
    let path_len = polyline_length(xy);
    let len_ratio = (path_len / perim_est.max(1.0)).clamp(0.5, 1.5);
    let len_score = 1.0 - (len_ratio - 1.0).abs() * 2.0;

    let scale = rx.max(ry);
    let conf_fit = residual_confidence(
        xy,
        |(x, y)| {
            // Radial residual vs unit ellipse.
            let dx = (x - cx) / rx.max(1e-3);
            let dy = (y - cy) / ry.max(1e-3);
            let r = (dx * dx + dy * dy).sqrt();
            (r - 1.0).abs() * scale
        },
        scale,
    );
    // Slight boost so clean ovals beat thin polygon approximations.
    let conf = (0.7 * conf_fit + 0.3 * len_score.clamp(0.0, 1.0)).clamp(0.0, 1.0) + 0.04;
    let conf = conf.min(1.0);
    if conf < MIN_CONFIDENCE {
        return None;
    }
    let pts: Vec<(f32, f32)> = (0..=PERIM_SAMPLES_ELLIPSE)
        .map(|i| {
            let t = i as f32 / PERIM_SAMPLES_ELLIPSE as f32 * std::f32::consts::TAU;
            (cx + rx * t.cos(), cy + ry * t.sin())
        })
        .collect();
    Some((conf, pts))
}

fn close_ring(corners: &[(f32, f32)]) -> Vec<(f32, f32)> {
    let mut v = corners.to_vec();
    if let Some(&first) = corners.first() {
        v.push(first);
    }
    v
}

fn sample_ring(corners: &[(f32, f32)], n: usize) -> Vec<(f32, f32)> {
    let ring = close_ring(corners);
    let total = polyline_length(&ring).max(1.0);
    let mut out = Vec::with_capacity(n + 1);
    for i in 0..n {
        let target = total * (i as f32 / n as f32);
        out.push(point_at_arclen(&ring, target));
    }
    out.push(corners[0]);
    out
}

fn point_at_arclen(poly: &[(f32, f32)], target: f32) -> (f32, f32) {
    let mut acc = 0.0f32;
    for w in poly.windows(2) {
        let seg = dist(w[0], w[1]);
        if acc + seg >= target || seg < 1e-6 {
            let t = if seg < 1e-6 {
                0.0
            } else {
                ((target - acc) / seg).clamp(0.0, 1.0)
            };
            return (
                w[0].0 + (w[1].0 - w[0].0) * t,
                w[0].1 + (w[1].1 - w[0].1) * t,
            );
        }
        acc += seg;
    }
    *poly.last().unwrap_or(&(0.0, 0.0))
}

fn dist_to_polyline(p: (f32, f32), poly: &[(f32, f32)]) -> f32 {
    let mut best = f32::INFINITY;
    for w in poly.windows(2) {
        best = best.min(dist_point_segment(p, w[0], w[1]));
    }
    best
}

fn dist_point_segment(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let vx = b.0 - a.0;
    let vy = b.1 - a.1;
    let len2 = vx * vx + vy * vy;
    if len2 < 1e-8 {
        return dist(p, a);
    }
    let t = (((p.0 - a.0) * vx + (p.1 - a.1) * vy) / len2).clamp(0.0, 1.0);
    dist(p, (a.0 + vx * t, a.1 + vy * t))
}

fn order_corners_ccw(corners: &[(f32, f32)]) -> Vec<(f32, f32)> {
    let (cx, cy) = centroid(corners);
    let mut indexed: Vec<_> = corners.to_vec();
    indexed.sort_by(|a, b| {
        let aa = (a.1 - cy).atan2(a.0 - cx);
        let bb = (b.1 - cy).atan2(b.0 - cx);
        aa.partial_cmp(&bb).unwrap_or(std::cmp::Ordering::Equal)
    });
    indexed
}

/// Ramer–Douglas–Peucker style corner pick, forced to `want` vertices when possible.
fn dominant_corners(xy: &[(f32, f32)], want: usize) -> Option<Vec<(f32, f32)>> {
    if xy.len() < want {
        return None;
    }
    // Start from a light RDP, then refine epsilon until we get ~want corners.
    let path_len = polyline_length(xy);
    let diag = {
        let (min_x, min_y, max_x, max_y) = bbox(xy);
        dist((min_x, min_y), (max_x, max_y)).max(1.0)
    };
    let mut eps = (path_len * 0.02).max(diag * 0.03).max(4.0);
    let mut best = rdp(xy, eps);
    for _ in 0..8 {
        let n =
            if closed_enough(xy) && best.len() >= 2 && dist(best[0], *best.last().unwrap()) < eps {
                // Drop duplicate closure vertex for counting.
                best.len().saturating_sub(1)
            } else {
                best.len()
            };
        if n == want {
            break;
        }
        if n > want {
            eps *= 1.35;
        } else {
            eps *= 0.72;
        }
        best = rdp(xy, eps);
    }
    // Normalise closed polylines: drop repeated endpoint.
    if best.len() >= 2 && dist(best[0], *best.last().unwrap()) < eps.max(8.0) {
        best.pop();
    }
    if best.len() != want {
        // Fallback: pick `want` highest-curvature samples + keep spread.
        best = curvature_corners(xy, want)?;
    }
    if best.len() != want {
        return None;
    }
    Some(best)
}

fn closed_enough(xy: &[(f32, f32)]) -> bool {
    let path_len = polyline_length(xy);
    dist(xy[0], *xy.last().unwrap()) <= path_len * CLOSURE_FRAC
}

fn rdp(xy: &[(f32, f32)], eps: f32) -> Vec<(f32, f32)> {
    if xy.len() < 3 {
        return xy.to_vec();
    }
    let mut stack = vec![(0usize, xy.len() - 1)];
    let mut keep = vec![false; xy.len()];
    keep[0] = true;
    keep[xy.len() - 1] = true;
    while let Some((s, e)) = stack.pop() {
        let mut max_d = 0.0f32;
        let mut idx = s;
        for i in s + 1..e {
            let d = dist_point_segment(xy[i], xy[s], xy[e]);
            if d > max_d {
                max_d = d;
                idx = i;
            }
        }
        if max_d > eps {
            keep[idx] = true;
            stack.push((s, idx));
            stack.push((idx, e));
        }
    }
    xy.iter()
        .enumerate()
        .filter(|(i, _)| keep[*i])
        .map(|(_, p)| *p)
        .collect()
}

fn curvature_corners(xy: &[(f32, f32)], want: usize) -> Option<Vec<(f32, f32)>> {
    if xy.len() < want + 2 {
        return None;
    }
    let mut scores: Vec<(usize, f32)> = Vec::new();
    let n = xy.len();
    let closed = closed_enough(xy);
    let range = if closed { 0..n } else { 1..n - 1 };
    for i in range {
        let prev = if i == 0 { xy[n - 1] } else { xy[i - 1] };
        let next = if i + 1 == n { xy[0] } else { xy[i + 1] };
        let cur = xy[i];
        let v1 = (prev.0 - cur.0, prev.1 - cur.1);
        let v2 = (next.0 - cur.0, next.1 - cur.1);
        let n1 = (v1.0 * v1.0 + v1.1 * v1.1).sqrt().max(1e-3);
        let n2 = (v2.0 * v2.0 + v2.1 * v2.1).sqrt().max(1e-3);
        let cos = ((v1.0 * v2.0 + v1.1 * v2.1) / (n1 * n2)).clamp(-1.0, 1.0);
        let turn = 1.0 - cos; // 0 = straight, 2 = reverse
        scores.push((i, turn));
    }
    scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let min_sep = (xy.len() / (want * 2)).max(3);
    let mut picked: Vec<usize> = Vec::new();
    for (i, _) in scores {
        if picked.iter().any(|&j| {
            let d = (i as isize - j as isize).unsigned_abs();
            let d = d.min(xy.len() - d);
            d < min_sep
        }) {
            continue;
        }
        picked.push(i);
        if picked.len() == want {
            break;
        }
    }
    if picked.len() != want {
        return None;
    }
    picked.sort_unstable();
    Some(picked.into_iter().map(|i| xy[i]).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stroke_line(a: (f32, f32), b: (f32, f32), n: usize, wobble: f32) -> Vec<Point> {
        (0..n)
            .map(|i| {
                let t = i as f32 / (n - 1) as f32;
                let x = a.0 + (b.0 - a.0) * t + wobble * ((i as f32) * 0.7).sin();
                let y = a.1 + (b.1 - a.1) * t + wobble * ((i as f32) * 0.9).cos();
                Point::new(x, y, 0.5, 0.0, (i * 10) as u32)
            })
            .collect()
    }

    fn stroke_rect(x0: f32, y0: f32, w: f32, h: f32, n_side: usize, wobble: f32) -> Vec<Point> {
        let corners = [
            (x0, y0),
            (x0 + w, y0),
            (x0 + w, y0 + h),
            (x0, y0 + h),
            (x0, y0),
        ];
        let mut pts = Vec::new();
        let mut t = 0u32;
        for side in corners.windows(2) {
            for i in 0..n_side {
                let u = i as f32 / n_side as f32;
                let x = side[0].0 + (side[1].0 - side[0].0) * u + wobble * (t as f32 * 0.3).sin();
                let y = side[0].1 + (side[1].1 - side[0].1) * u + wobble * (t as f32 * 0.4).cos();
                pts.push(Point::new(x, y, 0.5, 0.0, t));
                t += 8;
            }
        }
        pts
    }

    fn stroke_ellipse(cx: f32, cy: f32, rx: f32, ry: f32, n: usize, wobble: f32) -> Vec<Point> {
        (0..=n)
            .map(|i| {
                let a = i as f32 / n as f32 * std::f32::consts::TAU;
                let x = cx + rx * a.cos() + wobble * (i as f32 * 0.5).sin();
                let y = cy + ry * a.sin() + wobble * (i as f32 * 0.6).cos();
                Point::new(x, y, 0.5, 0.0, (i * 8) as u32)
            })
            .collect()
    }

    fn stroke_triangle(n_side: usize, wobble: f32) -> Vec<Point> {
        let corners = [(100.0, 40.0), (180.0, 160.0), (40.0, 160.0), (100.0, 40.0)];
        let mut pts = Vec::new();
        let mut t = 0u32;
        for side in corners.windows(2) {
            for i in 0..n_side {
                let u = i as f32 / n_side as f32;
                let x = side[0].0 + (side[1].0 - side[0].0) * u + wobble * (t as f32 * 0.25).sin();
                let y = side[0].1 + (side[1].1 - side[0].1) * u + wobble * (t as f32 * 0.3).cos();
                pts.push(Point::new(x, y, 0.5, 0.0, t));
                t += 8;
            }
        }
        pts
    }

    /// Scribble that should not snap (handwriting-like zig-zag).
    fn scribble() -> Vec<Point> {
        let mut pts = Vec::new();
        for i in 0..40 {
            let t = i as f32 * 0.35;
            let x = 80.0 + t * 8.0 + 12.0 * (t * 2.1).sin();
            let y = 100.0 + 18.0 * (t * 1.7).sin() + 9.0 * (t * 3.3).cos();
            pts.push(Point::new(x, y, 0.5, 0.0, i * 10));
        }
        pts
    }

    #[test]
    fn fits_wobbly_line() {
        let pts = stroke_line((20.0, 30.0), (200.0, 140.0), 40, 2.5);
        let fit = fit_smart_shape(&pts).expect("line");
        assert_eq!(fit.kind, ShapeKind::Line);
        assert!(fit.confidence >= MIN_CONFIDENCE);
        assert!(fit.points.len() >= 2);
    }

    #[test]
    fn fits_wobbly_rectangle() {
        let pts = stroke_rect(40.0, 50.0, 120.0, 90.0, 18, 2.0);
        let fit = fit_smart_shape(&pts).expect("rect");
        assert_eq!(fit.kind, ShapeKind::Rectangle);
        assert!(fit.confidence >= MIN_CONFIDENCE);
        // Closed: first ≈ last.
        let a = fit.points.first().unwrap();
        let b = fit.points.last().unwrap();
        assert!((a.x - b.x).abs() < 1.0 && (a.y - b.y).abs() < 1.0);
    }

    #[test]
    fn fits_wobbly_triangle() {
        let pts = stroke_triangle(20, 2.0);
        let fit = fit_smart_shape(&pts).expect("triangle");
        assert_eq!(fit.kind, ShapeKind::Triangle);
    }

    #[test]
    fn fits_wobbly_ellipse() {
        let pts = stroke_ellipse(120.0, 130.0, 70.0, 50.0, 48, 2.0);
        let fit = fit_smart_shape(&pts).expect("ellipse");
        assert_eq!(fit.kind, ShapeKind::Ellipse);
    }

    #[test]
    fn rejects_handwriting_scribble() {
        assert!(fit_smart_shape(&scribble()).is_none());
    }

    #[test]
    fn rejects_tiny_stroke() {
        let pts = stroke_line((10.0, 10.0), (20.0, 12.0), 8, 0.5);
        assert!(fit_smart_shape(&pts).is_none());
    }
}
