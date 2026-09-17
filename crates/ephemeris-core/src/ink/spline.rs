//! Catmull-Rom spline interpolation.
//!
//! After the One-Euro filter removes jitter, the resampled centre-line can
//! still look faceted at low sample rates.  A centripetal Catmull-Rom spline
//! interpolates a smooth curve that passes *through* every control point (so we
//! don't move the ink away from where the pen actually went) while avoiding the
//! cusps/self-intersections that the uniform variant can produce.
//!
//! Each interpolated attribute (x, y, pressure) is treated as a separate 1-D
//! Catmull-Rom, all sharing the centripetal parameterisation derived from the
//! x/y positions.

use crate::model::Point;

/// A control point for spline interpolation: position plus the attributes we
/// want to carry through the curve (pressure, tilt, time).
#[derive(Debug, Clone, Copy)]
struct Ctrl {
    x: f32,
    y: f32,
    pressure: f32,
    tilt: f32,
    t_ms: f32,
}

impl From<&Point> for Ctrl {
    fn from(p: &Point) -> Self {
        Ctrl {
            x: p.x,
            y: p.y,
            pressure: p.pressure,
            tilt: p.tilt,
            t_ms: p.t_ms as f32,
        }
    }
}

fn dist(a: &Ctrl, b: &Ctrl) -> f32 {
    ((b.x - a.x).powi(2) + (b.y - a.y).powi(2)).sqrt()
}

/// Centripetal (alpha = 0.5) knot spacing between two control points.
fn knot_delta(a: &Ctrl, b: &Ctrl) -> f32 {
    // t_{i+1} = t_i + |P_{i+1} - P_i|^alpha, alpha = 0.5.
    let d = dist(a, b).sqrt();
    // Guard against coincident points producing a zero interval (which would
    // make the basis blow up).
    if d < 1e-6 { 1e-4 } else { d }
}

/// Interpolate a scalar with the non-uniform Catmull-Rom basis on knots
/// `t0..t3` at parameter `t`.
#[allow(clippy::too_many_arguments)]
fn catmull_rom_scalar(
    p0: f32, p1: f32, p2: f32, p3: f32,
    t0: f32, t1: f32, t2: f32, t3: f32,
    t: f32,
) -> f32 {
    // Barry-Goldman pyramidal formulation of non-uniform Catmull-Rom.
    let a1 = (t1 - t) / (t1 - t0) * p0 + (t - t0) / (t1 - t0) * p1;
    let a2 = (t2 - t) / (t2 - t1) * p1 + (t - t1) / (t2 - t1) * p2;
    let a3 = (t3 - t) / (t3 - t2) * p2 + (t - t2) / (t3 - t2) * p3;

    let b1 = (t2 - t) / (t2 - t0) * a1 + (t - t0) / (t2 - t0) * a2;
    let b2 = (t3 - t) / (t3 - t1) * a2 + (t - t1) / (t3 - t1) * a3;

    (t2 - t) / (t2 - t1) * b1 + (t - t1) / (t2 - t1) * b2
}

/// Resample a polyline of [`Point`]s into a smooth curve using a centripetal
/// Catmull-Rom spline.
///
/// `segments_per_span` controls how many output points are generated between
/// each pair of input control points (>= 1).  The returned vector always
/// includes the original endpoints, so the curve passes through them.
///
/// * 0 or 1 input points → returned as-is.
/// * 2 input points → a straight line, resampled `segments_per_span` times.
pub fn catmull_rom(points: &[Point], segments_per_span: usize) -> Vec<Point> {
    let seg = segments_per_span.max(1);

    if points.len() <= 1 {
        return points.to_vec();
    }

    // Build control array with duplicated endpoints so every real span has the
    // two neighbours the basis needs.
    let ctrls: Vec<Ctrl> = points.iter().map(Ctrl::from).collect();
    let first = *ctrls.first().unwrap();
    let last = *ctrls.last().unwrap();

    let mut ext = Vec::with_capacity(ctrls.len() + 2);
    ext.push(first);
    ext.extend_from_slice(&ctrls);
    ext.push(last);

    let mut out: Vec<Point> = Vec::with_capacity((points.len() - 1) * seg + 1);
    out.push(points[0]);

    // Iterate over interior spans p1->p2 (indices 1..len-2 of `ext`).
    for i in 1..ext.len() - 2 {
        let (p0, p1, p2, p3) = (ext[i - 1], ext[i], ext[i + 1], ext[i + 2]);

        let t0 = 0.0;
        let t1 = t0 + knot_delta(&p0, &p1);
        let t2 = t1 + knot_delta(&p1, &p2);
        let t3 = t2 + knot_delta(&p2, &p3);

        // Sample the span (t1..t2], skipping the start (already emitted).
        for s in 1..=seg {
            let u = s as f32 / seg as f32;
            let t = t1 + u * (t2 - t1);

            let x = catmull_rom_scalar(p0.x, p1.x, p2.x, p3.x, t0, t1, t2, t3, t);
            let y = catmull_rom_scalar(p0.y, p1.y, p2.y, p3.y, t0, t1, t2, t3, t);
            let pr = catmull_rom_scalar(
                p0.pressure, p1.pressure, p2.pressure, p3.pressure, t0, t1, t2, t3, t,
            )
            .clamp(0.0, 1.0);
            let tilt = catmull_rom_scalar(
                p0.tilt, p1.tilt, p2.tilt, p3.tilt, t0, t1, t2, t3, t,
            );
            let tm = catmull_rom_scalar(
                p0.t_ms, p1.t_ms, p2.t_ms, p3.t_ms, t0, t1, t2, t3, t,
            );

            out.push(Point {
                x,
                y,
                pressure: pr,
                tilt,
                t_ms: tm.max(0.0).round() as u32,
            });
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(x: f32, y: f32) -> Point {
        Point::new(x, y, 0.5, 0.0, 0)
    }

    #[test]
    fn passes_through_control_points() {
        let input = vec![pt(0.0, 0.0), pt(10.0, 10.0), pt(20.0, 0.0)];
        let out = catmull_rom(&input, 8);

        // First and last output must equal first/last input exactly.
        assert_eq!(out.first().unwrap().x, 0.0);
        assert_eq!(out.first().unwrap().y, 0.0);
        assert_eq!(out.last().unwrap().x, 20.0);
        assert_eq!(out.last().unwrap().y, 0.0);

        // The interior control point must appear (curve passes through it).
        let hits_mid = out
            .iter()
            .any(|p| (p.x - 10.0).abs() < 1e-3 && (p.y - 10.0).abs() < 1e-3);
        assert!(hits_mid, "spline must pass through interior control point");
    }

    #[test]
    fn output_denser_than_input() {
        let input = vec![pt(0.0, 0.0), pt(1.0, 0.0), pt(2.0, 0.0), pt(3.0, 0.0)];
        let out = catmull_rom(&input, 5);
        // 3 spans * 5 + 1 = 16 output points.
        assert_eq!(out.len(), (input.len() - 1) * 5 + 1);
    }

    #[test]
    fn straight_line_stays_straight() {
        let input = vec![pt(0.0, 0.0), pt(10.0, 10.0)];
        let out = catmull_rom(&input, 10);
        for p in &out {
            // On y=x line, x and y should stay equal.
            assert!((p.x - p.y).abs() < 1e-3, "point drifted off the line: {p:?}");
        }
    }

    #[test]
    fn single_point_passthrough() {
        let input = vec![pt(5.0, 7.0)];
        let out = catmull_rom(&input, 4);
        assert_eq!(out, input);
    }

    #[test]
    fn coincident_points_do_not_produce_nan() {
        let input = vec![pt(1.0, 1.0), pt(1.0, 1.0), pt(2.0, 2.0)];
        let out = catmull_rom(&input, 6);
        for p in &out {
            assert!(p.x.is_finite() && p.y.is_finite(), "spline produced NaN/inf");
        }
    }

    #[test]
    fn pressure_is_interpolated_and_clamped() {
        let input = vec![
            Point::new(0.0, 0.0, 0.2, 0.0, 0),
            Point::new(10.0, 0.0, 0.9, 0.0, 10),
            Point::new(20.0, 0.0, 0.2, 0.0, 20),
        ];
        let out = catmull_rom(&input, 8);
        for p in &out {
            assert!((0.0..=1.0).contains(&p.pressure), "pressure out of range: {}", p.pressure);
        }
        // Some interior point should have higher pressure than the endpoints.
        let max_p = out.iter().map(|p| p.pressure).fold(0.0_f32, f32::max);
        assert!(max_p > 0.5);
    }
}
