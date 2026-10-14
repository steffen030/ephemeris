//! Armrest-style online-HWR spline preprocessing (Apache-2.0).
//!
//! Ported from [bkirwi/armrest](https://github.com/bkirwi/armrest) (`Ink::normalize`,
//! `smooth`, `math::min_distance`, `math::douglas_peucker`, and the Spline
//! `ModelInput` feature writer). Original author: Ben Kirwin.
//!
//! Licensed under the Apache License, Version 2.0. See the project NOTICE /
//! LICENSE-APACHE for the full text.
//!
//! Pipeline (matches armrest `ml::ModelInput<Spline> for Ink`):
//! 1. `normalize(1.0)` — scale so ink height is 1, origin at (xmin, ymin), t-=tmin
//! 2. `smooth(0.01)` — time-weighted local average (span ±3)
//! 3. `min_distance(0.05)` — drop points closer than the threshold (keep ends)
//! 4. `douglas_peucker(0.01)` — polyline simplification
//! 5. Emit `[dx, dy, dt, pen_up]` with `pen_up=1` on the last point of each stroke

use crate::model::{Point, Stroke};
use crate::stroke_htr::{HtrFeature, StrokeLine};

/// Armrest `english_ascii` alphabet (no CTC blank). Blank index is [`BLANK_INDEX`].
pub const CHARSET: &str =
    " 0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~";

/// CTC blank class index for armrest `english_ascii` (blank-last = `CHARSET.len()`).
pub const BLANK_INDEX: usize = 95;

/// Default meta.json `features` value for this pipeline.
pub const FEATURES_ID: &str = "armrest_spline";

/// meta.json value for Carbune2020 / OnlineHTR.
pub const CARBUNE_FEATURES_ID: &str = "carbune2020";

const NORMALIZE_HEIGHT: f32 = 1.0;
const SMOOTH_HALF_LIFE: f32 = 0.01;
const MIN_DISTANCE: f32 = 0.05;
const DOUGLAS_PEUCKER: f32 = 0.01;

#[derive(Debug, Clone, Copy)]
struct Pt {
    x: f32,
    y: f32,
    t: f32,
}

#[derive(Debug, Clone, Default)]
struct ArmrestInk {
    points: Vec<Pt>,
    /// Exclusive end indices of each stroke (same as armrest `stroke_ends`).
    stroke_ends: Vec<usize>,
}

impl ArmrestInk {
    fn len(&self) -> usize {
        self.points.len()
    }

    fn is_pen_up(&self, index: usize) -> bool {
        self.stroke_ends.binary_search(&(index + 1)).is_ok()
    }

    fn push(&mut self, x: f32, y: f32, t: f32) {
        self.points.push(Pt { x, y, t });
    }

    fn pen_up(&mut self) {
        let next = self.points.len();
        if next > 0 && self.stroke_ends.last().copied() != Some(next) {
            self.stroke_ends.push(next);
        }
    }

    fn strokes(&self) -> impl Iterator<Item = &[Pt]> {
        let points = &self.points[..];
        let mut start = 0usize;
        self.stroke_ends.iter().map(move |&end| {
            let slice = &points[start..end];
            start = end;
            slice
        })
    }

    fn from_string(input: &str) -> Self {
        let mut ink = Self::default();
        if input.is_empty() {
            return ink;
        }
        for stroke in input.split(';') {
            if stroke.is_empty() {
                continue;
            }
            for point in stroke.split(',') {
                let mut coords = point.split(' ').filter(|s| !s.is_empty());
                let x: f32 = coords
                    .next()
                    .and_then(|s| s.parse().ok())
                    .expect("armrest ink x");
                let y: f32 = coords
                    .next()
                    .and_then(|s| s.parse().ok())
                    .expect("armrest ink y");
                let t: f32 = coords
                    .next()
                    .and_then(|s| s.parse().ok())
                    .expect("armrest ink t");
                ink.push(x, y, t);
            }
            ink.pen_up();
        }
        ink
    }

    fn ranges(&self) -> (f32, f32, f32, f32, f32, f32) {
        let mut xmin = f32::INFINITY;
        let mut xmax = f32::NEG_INFINITY;
        let mut ymin = f32::INFINITY;
        let mut ymax = f32::NEG_INFINITY;
        let mut tmin = f32::INFINITY;
        let mut tmax = f32::NEG_INFINITY;
        for p in &self.points {
            xmin = xmin.min(p.x);
            xmax = xmax.max(p.x);
            ymin = ymin.min(p.y);
            ymax = ymax.max(p.y);
            tmin = tmin.min(p.t);
            tmax = tmax.max(p.t);
        }
        (xmin, xmax, ymin, ymax, tmin, tmax)
    }

    /// Armrest `Ink::normalize(target_height)`.
    fn normalize(&mut self, target_height: f32) {
        if self.points.is_empty() {
            return;
        }
        let (xmin, _, ymin, ymax, tmin, _) = self.ranges();
        let y_size = ymax - ymin;
        let ink_scale = if y_size > 1e-12 {
            target_height / y_size
        } else {
            1.0
        };
        for p in &mut self.points {
            p.x = (p.x - xmin) * ink_scale;
            p.y = (p.y - ymin) * ink_scale;
            p.t -= tmin;
        }
    }

    /// Armrest `Ink::smooth(half_life)`.
    fn smooth(&mut self, half_life: f32) {
        let span = 3usize;
        let ends = self.stroke_ends.clone();
        let mut start = 0usize;
        for &end in &ends {
            let stroke_len = end - start;
            if stroke_len == 0 {
                start = end;
                continue;
            }
            // Read from a snapshot so weights use original coordinates.
            let snapshot: Vec<Pt> = self.points[start..end].to_vec();
            for i in 0..stroke_len {
                let current_range = i.min(span).min(stroke_len - 1 - i);
                let current_t = snapshot[i].t;
                let mut x = 0.0f32;
                let mut y = 0.0f32;
                let mut total_weight = 0.0f32;
                let lo = i - current_range;
                let hi = i + current_range;
                for p in &snapshot[lo..=hi] {
                    let weight = 0.5f32.powf((p.t - current_t).abs() / half_life);
                    x += p.x * weight;
                    y += p.y * weight;
                    total_weight += weight;
                }
                self.points[start + i].x = x / total_weight;
                self.points[start + i].y = y / total_weight;
            }
            start = end;
        }
    }
}

fn xy_distance2(a: Pt, b: Pt) -> f32 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    dx * dx + dy * dy
}

fn point_segment_distance2(p0: Pt, p1: Pt, q: Pt) -> f32 {
    let ux = p1.x - p0.x;
    let uy = p1.y - p0.y;
    if ux == 0.0 && uy == 0.0 {
        return xy_distance2(p0, q);
    }
    let vx = q.x - p0.x;
    let vy = q.y - p0.y;
    let mag2 = ux * ux + uy * uy;
    let dist_between = (ux * vx + uy * vy) / mag2;
    if dist_between <= 0.0 {
        xy_distance2(p0, q)
    } else if dist_between >= 1.0 {
        xy_distance2(p1, q)
    } else {
        let px = p0.x + ux * dist_between;
        let py = p0.y + uy * dist_between;
        let dx = px - q.x;
        let dy = py - q.y;
        dx * dx + dy * dy
    }
}

/// Armrest `math::min_distance`.
fn min_distance(data: &ArmrestInk, distance: f32) -> ArmrestInk {
    let distance2 = distance * distance;
    let mut result = ArmrestInk::default();
    for stroke in data.strokes() {
        if stroke.is_empty() {
            result.pen_up();
            continue;
        }
        let mut last_kept = stroke[0];
        result.push(last_kept.x, last_kept.y, last_kept.t);
        for &next in &stroke[1..] {
            if xy_distance2(last_kept, next) >= distance2 {
                result.push(next.x, next.y, next.t);
                last_kept = next;
            }
        }
        if let Some(&last) = stroke.last() {
            if last.x != last_kept.x || last.y != last_kept.y || last.t != last_kept.t {
                result.push(last.x, last.y, last.t);
            }
        }
        result.pen_up();
    }
    result
}

/// Armrest `math::douglas_peucker`.
fn douglas_peucker(data: &ArmrestInk, distance: f32) -> ArmrestInk {
    let distance2 = distance * distance;
    let mut result = ArmrestInk::default();
    for stroke in data.strokes() {
        if stroke.is_empty() {
            result.pen_up();
            continue;
        }
        if stroke.len() == 1 {
            let p = stroke[0];
            result.push(p.x, p.y, p.t);
            result.pen_up();
            continue;
        }
        let mut stack = vec![(0usize, stroke.len() - 1)];
        while let Some((start, end)) = stack.pop() {
            let p0 = stroke[start];
            let p1 = stroke[end];
            let inner = ((start + 1)..end)
                .map(|i| (i, point_segment_distance2(p0, p1, stroke[i])))
                .max_by(|(_, d0), (_, d1)| d0.partial_cmp(d1).unwrap_or(std::cmp::Ordering::Equal));
            match inner {
                Some((split, d)) if d > distance2 => {
                    stack.push((split, end));
                    stack.push((start, split));
                }
                _ => {
                    let p = stroke[start];
                    result.push(p.x, p.y, p.t);
                }
            }
        }
        let last = stroke[stroke.len() - 1];
        result.push(last.x, last.y, last.t);
        result.pen_up();
    }
    result
}

fn from_stroke_line(line: &StrokeLine) -> ArmrestInk {
    let mut ink = ArmrestInk::default();
    let base_ms = super::line_time_base_ms(line);
    let mut next_synth_t0 = 0.0f32;
    for stroke in &line.strokes {
        if stroke.points.is_empty() {
            continue;
        }
        let t0 = if stroke.t0_ms == 0 {
            next_synth_t0
        } else {
            stroke.t0_ms.saturating_sub(base_ms) as f32 / 1000.0
        };
        let mut last_t = t0;
        for p in &stroke.points {
            let t = t0 + p.t_ms as f32 / 1000.0;
            last_t = t;
            ink.push(p.x, p.y, t);
        }
        ink.pen_up();
        next_synth_t0 = last_t + 0.150;
    }
    ink
}

/// Run the full armrest spline pipeline and emit `[dx, dy, dt, pen_up]` features.
pub fn featurize_armrest_ink(ink_literal: &str) -> Vec<HtrFeature> {
    let ink = ArmrestInk::from_string(ink_literal);
    featurize_armrest(&ink)
}

/// Featurize a [`StrokeLine`] with the armrest spline pipeline.
pub fn featurize_line_armrest(line: &StrokeLine) -> Vec<HtrFeature> {
    let ink = from_stroke_line(line);
    featurize_armrest(&ink)
}

/// Like [`crate::stroke_htr::featurize_line_pieces`], but with armrest features.
pub fn featurize_line_armrest_pieces(
    line: &StrokeLine,
    max_timesteps: usize,
) -> Vec<(StrokeLine, Vec<HtrFeature>)> {
    use crate::stroke_htr::split_stroke_line_at_largest_x_gap;

    let feats = featurize_line_armrest(line);
    if feats.len() <= max_timesteps || line.strokes.len() < 2 {
        return vec![(line.clone(), feats)];
    }
    let Some((left, right)) = split_stroke_line_at_largest_x_gap(line) else {
        return vec![(line.clone(), feats)];
    };
    if left.strokes.is_empty()
        || right.strokes.is_empty()
        || left.strokes.len() + right.strokes.len() != line.strokes.len()
    {
        return vec![(line.clone(), feats)];
    }
    let mut out = featurize_line_armrest_pieces(&left, max_timesteps);
    out.extend(featurize_line_armrest_pieces(&right, max_timesteps));
    out
}

fn featurize_armrest(ink: &ArmrestInk) -> Vec<HtrFeature> {
    if ink.points.is_empty() {
        return Vec::new();
    }
    let mut normal = ink.clone();
    normal.normalize(NORMALIZE_HEIGHT);
    normal.smooth(SMOOTH_HALF_LIFE);
    normal = min_distance(&normal, MIN_DISTANCE);
    normal = douglas_peucker(&normal, DOUGLAS_PEUCKER);

    if normal.points.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::with_capacity(normal.len());
    let mut last = normal.points[0];
    for (i, point) in normal.points.iter().enumerate() {
        out.push([
            point.x - last.x,
            point.y - last.y,
            point.t - last.t,
            if normal.is_pen_up(i) { 1.0 } else { 0.0 },
        ]);
        last = *point;
    }
    out
}

/// Build a [`StrokeLine`] from an armrest ink literal (for tests / fixtures).
pub fn stroke_line_from_armrest_ink(ink_literal: &str) -> StrokeLine {
    let ink = ArmrestInk::from_string(ink_literal);
    let mut strokes = Vec::new();
    let mut start = 0usize;
    for &end in &ink.stroke_ends {
        let pts = &ink.points[start..end];
        start = end;
        if pts.is_empty() {
            continue;
        }
        let t0_ms = (pts[0].t * 1000.0).round() as u64;
        let points: Vec<Point> = pts
            .iter()
            .map(|p| {
                let abs_ms = (p.t * 1000.0).round() as u32;
                Point {
                    x: p.x,
                    y: p.y,
                    pressure: 0.5,
                    tilt: 0.0,
                    t_ms: abs_ms.saturating_sub(t0_ms as u32),
                }
            })
            .collect();
        strokes.push(Stroke {
            id: crate::model::StrokeId::new(),
            tool: crate::model::Tool::Pen,
            color: crate::model::Color::BLACK,
            base_width: 2.0,
            points,
            t0_ms,
        });
    }
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for s in &strokes {
        for p in &s.points {
            min_x = min_x.min(p.x);
            min_y = min_y.min(p.y);
            max_x = max_x.max(p.x);
            max_y = max_y.max(p.y);
        }
    }
    if !min_x.is_finite() {
        min_x = 0.0;
        min_y = 0.0;
        max_x = 1.0;
        max_y = 1.0;
    }
    StrokeLine {
        strokes,
        x: min_x,
        y: min_y,
        width: (max_x - min_x).max(1.0),
        height: (max_y - min_y).max(1.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn featurize_is_invariant_to_epoch_ms_t0() {
        use crate::stroke_htr::tests::{assert_features_close, epoch_offset_line};
        let small = featurize_line_armrest(&epoch_offset_line(1_000));
        let epoch = featurize_line_armrest(&epoch_offset_line(4_293_604_864));
        assert_features_close(&small, &epoch);
    }

    #[test]
    fn charset_blank_index_matches_armrest() {
        assert_eq!(CHARSET.chars().count(), BLANK_INDEX);
        assert_eq!(BLANK_INDEX, 95);
        // Leading space, digits, then lowercase…
        assert_eq!(CHARSET.chars().next(), Some(' '));
        assert!(CHARSET.contains('a'));
        assert!(CHARSET.contains('Z'));
        assert!(CHARSET.contains('~'));
    }

    #[test]
    fn douglas_peucker_example_from_armrest() {
        let mut example = ArmrestInk::default();
        example.push(0.0, 0.0, 0.0);
        example.push(1.0, 1.0, 0.1);
        example.push(2.0, 2.0, 0.2);
        example.push(3.0, 1.0, 0.3);
        example.push(4.0, 0.0, 0.4);
        example.pen_up();
        let sampled = douglas_peucker(&example, 1.2);
        assert_eq!(sampled.len(), 3);
    }

    #[test]
    fn synthetic_two_stroke_emits_pen_up() {
        // Two short strokes; after preprocessing we should see at least one pen_up=1.
        let ink = "0.0 0.0 0.0,0.1 0.5 0.1,0.2 1.0 0.2;0.3 0.2 0.4,0.5 0.8 0.5,0.7 0.3 0.6";
        let feats = featurize_armrest_ink(ink);
        assert!(!feats.is_empty());
        assert!(
            feats.iter().any(|f| f[3] > 0.5),
            "expected pen_up flag on stroke ends: {feats:?}"
        );
        // First feature is always [0,0,0,?] (dx=dy=dt=0 vs first point).
        assert!((feats[0][0]).abs() < 1e-6);
        assert!((feats[0][1]).abs() < 1e-6);
        assert!((feats[0][2]).abs() < 1e-6);
    }

    #[test]
    fn stroke_line_roundtrip_featurize() {
        let ink = "0.0 0.0 0.0,0.2 1.0 0.2,0.4 0.0 0.4;0.5 0.5 0.6,0.8 0.2 0.8";
        let from_lit = featurize_armrest_ink(ink);
        let line = stroke_line_from_armrest_ink(ink);
        let from_line = featurize_line_armrest(&line);
        assert_eq!(from_lit.len(), from_line.len());
        for (a, b) in from_lit.iter().zip(from_line.iter()) {
            for i in 0..4 {
                assert!(
                    (a[i] - b[i]).abs() < 1e-4,
                    "mismatch channel {i}: {a:?} vs {b:?}"
                );
            }
        }
    }
}
