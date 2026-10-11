//! Online stroke handwriting recognition helpers (ephemeris-5g2).
//!
//! Line grouping + Carbune/OnlineHTR-style features `(dx, dy, dt, n)` with
//! linear interpolation to ~20 points per unit length. The ONNX/RTen backend
//! lives in [`crate::transcriber::StrokeHtrTranscriber`].

use crate::model::{Point, Stroke, Tool};

/// One text line as an ordered group of strokes (left-to-right by bbox).
#[derive(Debug, Clone)]
pub struct StrokeLine {
    pub strokes: Vec<Stroke>,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Axis-aligned bounds of a stroke's points (empty if no points).
pub fn stroke_bounds(stroke: &Stroke) -> Option<(f32, f32, f32, f32)> {
    if stroke.points.is_empty() {
        return None;
    }
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for p in &stroke.points {
        min_x = min_x.min(p.x);
        min_y = min_y.min(p.y);
        max_x = max_x.max(p.x);
        max_y = max_y.max(p.y);
    }
    Some((min_x, min_y, max_x, max_y))
}

/// Group pen/highlighter strokes into horizontal lines by y-overlap / gaps.
///
/// Strokes are sorted by vertical centre, then clustered when their vertical
/// centres fall within `line_gap` of the running line mean. Within each line,
/// strokes are ordered left-to-right.
pub fn group_strokes_into_lines(strokes: &[Stroke], line_gap: f32) -> Vec<StrokeLine> {
    let mut items: Vec<(Stroke, f32, f32, f32, f32, f32)> = Vec::new();
    for s in strokes {
        if s.tool == Tool::Eraser {
            continue;
        }
        let Some((min_x, min_y, max_x, max_y)) = stroke_bounds(s) else {
            continue;
        };
        if s.points.len() < 2 {
            continue;
        }
        let cy = (min_y + max_y) * 0.5;
        items.push((s.clone(), cy, min_x, min_y, max_x, max_y));
    }
    items.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

    type LinedStroke = (Stroke, f32, f32, f32, f32, f32);
    let mut lines: Vec<Vec<LinedStroke>> = Vec::new();
    for item in items {
        let cy = item.1;
        if let Some(line) = lines.last_mut() {
            let mean_cy: f32 = line.iter().map(|i| i.1).sum::<f32>() / line.len() as f32;
            if (cy - mean_cy).abs() <= line_gap {
                line.push(item);
                continue;
            }
        }
        lines.push(vec![item]);
    }

    lines
        .into_iter()
        .map(|mut line| {
            line.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
            let mut min_x = f32::INFINITY;
            let mut min_y = f32::INFINITY;
            let mut max_x = f32::NEG_INFINITY;
            let mut max_y = f32::NEG_INFINITY;
            let mut strokes = Vec::with_capacity(line.len());
            for (s, _, x0, y0, x1, y1) in line {
                min_x = min_x.min(x0);
                min_y = min_y.min(y0);
                max_x = max_x.max(x1);
                max_y = max_y.max(y1);
                strokes.push(s);
            }
            StrokeLine {
                strokes,
                x: min_x,
                y: min_y,
                width: (max_x - min_x).max(1.0),
                height: (max_y - min_y).max(1.0),
            }
        })
        .collect()
}

/// Default line-gap heuristic (~0.6× median stroke height, floored at 18 px).
pub fn default_line_gap(strokes: &[Stroke]) -> f32 {
    let mut heights: Vec<f32> = strokes
        .iter()
        .filter_map(|s| stroke_bounds(s).map(|(_, y0, _, y1)| (y1 - y0).max(1.0)))
        .collect();
    if heights.is_empty() {
        return 24.0;
    }
    heights.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let med = heights[heights.len() / 2];
    (med * 0.6).max(18.0)
}

/// Feature vector per interpolated point: `[dx, dy, dt, n]`.
pub type HtrFeature = [f32; 4];

/// Linearly interpolate stroke polylines to `points_per_unit` samples per
/// unit path length, then emit OnlineHTR-style `(dx, dy, dt, n)` features.
///
/// `n` is 1.0 at the first point of each stroke and 0.0 elsewhere.
pub fn featurize_line(line: &StrokeLine, points_per_unit: f32) -> Vec<HtrFeature> {
    let ppu = points_per_unit.max(1.0);
    let mut raw: Vec<(f32, f32, f32, bool)> = Vec::new();
    for stroke in &line.strokes {
        let interp = interpolate_stroke(&stroke.points, ppu);
        for (i, (x, y, t)) in interp.iter().enumerate() {
            raw.push((*x, *y, *t, i == 0));
        }
    }
    if raw.len() < 2 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(raw.len().saturating_sub(1));
    for w in raw.windows(2) {
        let (x0, y0, t0, _) = w[0];
        let (x1, y1, t1, n1) = w[1];
        let dt = ((t1 - t0) / 1000.0).max(0.0);
        out.push([x1 - x0, y1 - y0, dt, if n1 { 1.0 } else { 0.0 }]);
    }
    // First point of the sequence: zero deltas with n=1 (stroke start).
    if let Some(first) = raw.first() {
        let mut with_start = Vec::with_capacity(out.len() + 1);
        with_start.push([0.0, 0.0, 0.0, 1.0]);
        // Mark subsequent stroke starts already encoded in `out` via n.
        let _ = first;
        with_start.extend(out);
        return with_start;
    }
    out
}

fn interpolate_stroke(points: &[Point], points_per_unit: f32) -> Vec<(f32, f32, f32)> {
    if points.is_empty() {
        return Vec::new();
    }
    if points.len() == 1 {
        return vec![(points[0].x, points[0].y, points[0].t_ms as f32)];
    }
    let mut out = Vec::new();
    out.push((points[0].x, points[0].y, points[0].t_ms as f32));
    for win in points.windows(2) {
        let a = &win[0];
        let b = &win[1];
        let dist = (b.x - a.x).hypot(b.y - a.y);
        let n = (dist * points_per_unit).ceil().max(1.0) as i32;
        for i in 1..=n {
            let t = i as f32 / n as f32;
            out.push((
                a.x + (b.x - a.x) * t,
                a.y + (b.y - a.y) * t,
                a.t_ms as f32 + (b.t_ms as f32 - a.t_ms as f32) * t,
            ));
        }
    }
    out
}

/// Flatten features to a row-major `T×4` buffer for ONNX input `[1, T, 4]`.
pub fn features_to_tensor(features: &[HtrFeature]) -> (Vec<f32>, usize) {
    let t = features.len();
    let mut buf = Vec::with_capacity(t * 4);
    for f in features {
        buf.extend_from_slice(f);
    }
    (buf, t)
}

/// Greedy CTC decode: collapse repeats, drop blank index.
pub fn ctc_greedy_decode(
    logits: &[f32],
    time_steps: usize,
    num_classes: usize,
    blank: usize,
) -> Vec<usize> {
    if time_steps == 0 || num_classes == 0 || logits.len() < time_steps * num_classes {
        return Vec::new();
    }
    let mut path = Vec::new();
    let mut prev = None;
    for t in 0..time_steps {
        let row = &logits[t * num_classes..(t + 1) * num_classes];
        let mut best_i = 0usize;
        let mut best_v = f32::NEG_INFINITY;
        for (i, &v) in row.iter().enumerate() {
            if v > best_v {
                best_v = v;
                best_i = i;
            }
        }
        if Some(best_i) != prev && best_i != blank {
            path.push(best_i);
        }
        prev = Some(best_i);
    }
    path
}

/// Map CTC class indices through a charset that does **not** include the blank.
///
/// - `blank == 0` (OnlineHTR / Carbune): alphabet indices are `1..`; map via `i - 1`.
/// - `blank == charset.len()` (blank-last): alphabet indices are `0..`; map via `i`.
pub fn decode_charset(indices: &[usize], charset: &str) -> String {
    decode_charset_with_blank(indices, charset, 0)
}

/// Like [`decode_charset`] but with an explicit CTC blank index.
pub fn decode_charset_with_blank(indices: &[usize], charset: &str, blank: usize) -> String {
    let chars: Vec<char> = charset.chars().collect();
    indices
        .iter()
        .filter_map(|&i| {
            if i == blank {
                return None;
            }
            let idx = if blank == 0 { i.checked_sub(1)? } else { i };
            chars.get(idx).copied()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Color, StrokeId};

    fn stroke_at(y: f32, x0: f32, x1: f32) -> Stroke {
        Stroke {
            id: StrokeId::new(),
            tool: Tool::Pen,
            color: Color::BLACK,
            base_width: 2.0,
            points: vec![
                Point::new(x0, y, 0.5, 0.0, 0),
                Point::new(x1, y, 0.5, 0.0, 100),
            ],
        }
    }

    #[test]
    fn groups_two_horizontal_lines() {
        let strokes = vec![
            stroke_at(10.0, 0.0, 40.0),
            stroke_at(12.0, 50.0, 90.0),
            stroke_at(80.0, 0.0, 40.0),
        ];
        let lines = group_strokes_into_lines(&strokes, 20.0);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].strokes.len(), 2);
        assert_eq!(lines[1].strokes.len(), 1);
        // Left-to-right within first line.
        assert!(lines[0].strokes[0].points[0].x < lines[0].strokes[1].points[0].x);
    }

    #[test]
    fn featurize_emits_dx_dy_channels() {
        let line = StrokeLine {
            strokes: vec![stroke_at(0.0, 0.0, 20.0)],
            x: 0.0,
            y: 0.0,
            width: 20.0,
            height: 1.0,
        };
        let feats = featurize_line(&line, 2.0);
        assert!(feats.len() >= 2);
        assert_eq!(feats[0][3], 1.0); // start marker
                                      // Some step should move in +x.
        assert!(feats.iter().any(|f| f[0] > 0.0));
    }

    #[test]
    fn ctc_decode_collapses_repeats() {
        // 3 timesteps, 3 classes (0=a, 1=b, 2=blank)
        let logits = [
            2.0, 0.0, 0.0, // a
            2.0, 0.0, 0.0, // a (repeat)
            0.0, 3.0, 0.0, // b
        ];
        let path = ctc_greedy_decode(&logits, 3, 3, 2);
        assert_eq!(path, vec![0, 1]);
        // blank-last: indices map directly into charset
        assert_eq!(decode_charset_with_blank(&path, "ab", 2), "ab");
    }

    #[test]
    fn decode_blank_zero_shifts_indices() {
        // blank=0, alphabet "ab" at indices 1,2
        assert_eq!(decode_charset_with_blank(&[1, 2, 1], "ab", 0), "aba");
    }
}
