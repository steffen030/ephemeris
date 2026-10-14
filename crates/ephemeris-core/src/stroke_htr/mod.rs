//! Online stroke handwriting recognition helpers (ephemeris-5g2).
//!
//! Line grouping + Carbune2020 / OnlineHTR `featuresLinInterpol20DxDyDtN`
//! featurization: per-line (x,y) normalisation, per-stroke time resampling at
//! 20 points per unit length, then `(dx, dy, dt, n)` with absolute time in
//! seconds. The ONNX/RTen backend lives in
//! [`crate::transcriber::StrokeHtrTranscriber`].
//!
//! Also: [`armrest`] — ported armrest spline preprocessing (`armrest_spline`).

pub mod armrest;

use crate::model::{Stroke, Tool};

/// OnlineHTR / Carbune2020 default points per unit length after normalisation.
pub const CARBUNE_POINTS_PER_UNIT: f32 = 20.0;

/// Helper: true only for pen strokes with at least one point (suitable for HTR input).
///
/// Highlighter strokes are kept on the page for format detection but excluded
/// from handwriting recognition.
fn is_text_ink(s: &Stroke) -> bool {
    s.tool == Tool::Pen && !s.points.is_empty()
}

/// Safety cap: split a line when featurized length exceeds this (BiLSTM cost).
pub const MAX_STROKE_HTR_TIMESTEPS: usize = 4000;

/// Inter-stroke gap inserted for legacy strokes with `t0_ms == 0`.
const LEGACY_INTER_STROKE_GAP_MS: f32 = 150.0;

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

/// Lined paper ruling pitch used by the UI template (`PageTemplate::Lines`).
pub const LINED_TEMPLATE_PITCH: f32 = 28.0;

/// One ink stroke with axis-aligned bounds for line clustering.
#[derive(Clone)]
struct StrokeItem {
    stroke: Stroke,
    cy: f32,
    min_x: f32,
    min_y: f32,
    max_x: f32,
    max_y: f32,
    height: f32,
    width: f32,
    is_mark: bool,
}

/// Group pen/highlighter strokes into horizontal lines.
///
/// Body strokes cluster when their **y-extent overlaps the line's core band**
/// (median body centre ± half-band), not merely when centres are close. Short
/// marks (i-dots, umlaut dots, t-bars) attach afterward if they overlap an
/// expanded core band. Optional `ruling_pitch` (e.g. [`LINED_TEMPLATE_PITCH`])
/// sets the preferred half-band from lined paper.
pub fn group_strokes_into_lines(strokes: &[Stroke], line_gap: f32) -> Vec<StrokeLine> {
    group_strokes_into_lines_with_ruling(strokes, line_gap, None)
}

/// Like [`group_strokes_into_lines`], with an optional lined-template pitch.
pub fn group_strokes_into_lines_with_ruling(
    strokes: &[Stroke],
    line_gap: f32,
    ruling_pitch: Option<f32>,
) -> Vec<StrokeLine> {
    let mut raw: Vec<StrokeItem> = Vec::new();
    for s in strokes {
        if !is_text_ink(s) {
            continue;
        }
        let Some((min_x, min_y, max_x, max_y)) = stroke_bounds(s) else {
            continue;
        };
        let height = (max_y - min_y).max(0.5);
        let width = (max_x - min_x).max(0.5);
        let cy = (min_y + max_y) * 0.5;
        raw.push(StrokeItem {
            stroke: s.clone(),
            cy,
            min_x,
            min_y,
            max_x,
            max_y,
            height,
            width,
            is_mark: false,
        });
    }
    if raw.is_empty() {
        return Vec::new();
    }

    let mut body_heights: Vec<f32> = raw
        .iter()
        .filter(|i| i.stroke.points.len() >= 2)
        .map(|i| i.height)
        .collect();
    if body_heights.is_empty() {
        body_heights = raw.iter().map(|i| i.height).collect();
    }
    body_heights.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let med_h = body_heights[body_heights.len() / 2].max(1.0);
    let mark_h = (med_h * 0.32).max(3.0);
    let mark_w = (med_h * 0.55).max(4.0);

    for item in &mut raw {
        let short = item.height <= mark_h && item.width <= mark_w;
        let few_pts = item.stroke.points.len() <= 4 && item.height <= med_h * 0.45;
        item.is_mark = short || few_pts;
    }

    let mut bodies: Vec<StrokeItem> = raw.iter().filter(|i| !i.is_mark).cloned().collect();
    let mut marks: Vec<StrokeItem> = raw.into_iter().filter(|i| i.is_mark).collect();
    // Lone marks with no body strokes still form lines (skip re-attach).
    if bodies.is_empty() {
        bodies = std::mem::take(&mut marks);
    }

    bodies.sort_by(|a, b| a.cy.partial_cmp(&b.cy).unwrap_or(std::cmp::Ordering::Equal));

    let half_band = ruling_pitch
        .filter(|p| *p > 1.0)
        .map(|p| p * 0.32)
        .unwrap_or_else(|| (med_h * 0.42).max(line_gap * 0.45).min(line_gap.max(8.0)));
    // With lined paper, refuse to merge centres that are nearly a full ruling apart.
    let centre_join = ruling_pitch
        .filter(|p| *p > 1.0)
        .map(|p| p * 0.45)
        .unwrap_or_else(|| line_gap.max(half_band * 1.6));

    let mut clusters: Vec<Vec<StrokeItem>> = Vec::new();
    for item in bodies {
        let mut best: Option<(usize, f32)> = None;
        for (ci, cluster) in clusters.iter().enumerate() {
            let (core_lo, core_hi, mean_cy) = cluster_core_band(cluster, half_band);
            let overlaps = ranges_overlap(item.min_y, item.max_y, core_lo, core_hi);
            let centre_ok = (item.cy - mean_cy).abs() <= centre_join;
            // Slant / continuation: x-near a member whose local core overlaps.
            let chain = cluster.iter().any(|m| {
                let x_gap = if item.max_x < m.min_x {
                    m.min_x - item.max_x
                } else if m.max_x < item.min_x {
                    item.min_x - m.max_x
                } else {
                    0.0
                };
                let local_lo = m.cy - half_band;
                let local_hi = m.cy + half_band;
                x_gap <= med_h * 2.5 && ranges_overlap(item.min_y, item.max_y, local_lo, local_hi)
            });
            if overlaps || centre_ok || chain {
                let dist = (item.cy - mean_cy).abs();
                if best.map(|(_, d)| dist < d).unwrap_or(true) {
                    best = Some((ci, dist));
                }
            }
        }
        if let Some((ci, _)) = best {
            clusters[ci].push(item);
        } else {
            clusters.push(vec![item]);
        }
    }

    // Attach diacritics / t-bars / hanging marks to the nearest overlapping core.
    // Umlaut / i-dots sit well above the x-height — allow ~1.6× median body height up.
    let mark_up = (med_h * 1.6).max(half_band * 2.5);
    let mark_down = (med_h * 0.45).max(half_band * 0.55);
    for mark in marks {
        let mut best: Option<(usize, f32)> = None;
        for (ci, cluster) in clusters.iter().enumerate() {
            let (core_lo, core_hi, mean_cy) = cluster_core_band(cluster, half_band);
            let lo = core_lo - mark_down;
            let hi = core_hi + mark_up;
            if !ranges_overlap(mark.min_y, mark.max_y, lo, hi)
                && (mark.cy - mean_cy).abs() > line_gap * 1.25
            {
                continue;
            }
            let dist = (mark.cy - mean_cy).abs();
            if best.map(|(_, d)| dist < d).unwrap_or(true) {
                best = Some((ci, dist));
            }
        }
        if let Some((ci, _)) = best {
            clusters[ci].push(mark);
        } else if clusters.is_empty() {
            clusters.push(vec![mark]);
        } else {
            // Orphan mark → closest line by centre.
            let mut nearest = 0usize;
            let mut nearest_d = f32::INFINITY;
            for (ci, cluster) in clusters.iter().enumerate() {
                let mean_cy: f32 = cluster.iter().map(|i| i.cy).sum::<f32>() / cluster.len() as f32;
                let d = (mark.cy - mean_cy).abs();
                if d < nearest_d {
                    nearest_d = d;
                    nearest = ci;
                }
            }
            clusters[nearest].push(mark);
        }
    }

    // Top-to-bottom by mean centre; LTR within each line.
    clusters.sort_by(|a, b| {
        let ma: f32 = a.iter().map(|i| i.cy).sum::<f32>() / a.len() as f32;
        let mb: f32 = b.iter().map(|i| i.cy).sum::<f32>() / b.len() as f32;
        ma.partial_cmp(&mb).unwrap_or(std::cmp::Ordering::Equal)
    });

    clusters
        .into_iter()
        .map(|mut line| {
            line.sort_by(|a, b| {
                a.min_x
                    .partial_cmp(&b.min_x)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let mut min_x = f32::INFINITY;
            let mut min_y = f32::INFINITY;
            let mut max_x = f32::NEG_INFINITY;
            let mut max_y = f32::NEG_INFINITY;
            let mut strokes = Vec::with_capacity(line.len());
            for item in line {
                min_x = min_x.min(item.min_x);
                min_y = min_y.min(item.min_y);
                max_x = max_x.max(item.max_x);
                max_y = max_y.max(item.max_y);
                strokes.push(item.stroke);
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

fn cluster_core_band(cluster: &[StrokeItem], half_band: f32) -> (f32, f32, f32) {
    let mut centres: Vec<f32> = cluster.iter().map(|i| i.cy).collect();
    centres.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mean_cy = centres[centres.len() / 2];
    (mean_cy - half_band, mean_cy + half_band, mean_cy)
}

fn ranges_overlap(a0: f32, a1: f32, b0: f32, b1: f32) -> bool {
    a0 <= b1 && b0 <= a1
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

/// One point in the flat Carbune table: `(x, y, t_seconds, stroke_nr)`.
#[derive(Debug, Clone, Copy)]
struct CarbunePoint {
    x: f32,
    y: f32,
    t: f32,
    stroke_nr: u32,
}

/// Build the flat `(x, y, t_seconds, stroke_nr)` table with absolute time.
///
/// Time is `stroke.t0_ms + point.t_ms` relative to the line's earliest
/// `t0_ms` (see [`line_time_base_ms`]), converted to seconds. When
/// `t0_ms == 0` (legacy), a synthetic clock is used and advanced by
/// [`LEGACY_INTER_STROKE_GAP_MS`] after each stroke so inter-stroke `dt` is
/// not zero.
pub fn absolute_points_seconds(line: &StrokeLine) -> Vec<(f32, f32, f32, u32)> {
    build_absolute_points(line)
        .into_iter()
        .map(|p| (p.x, p.y, p.t, p.stroke_nr))
        .collect()
}

/// Earliest `t0_ms` in the line, or 0 if any stroke is legacy (`t0_ms == 0`).
///
/// `t0_ms` is wall-clock epoch ms (~4e9); as `f32` that has a 256 ms ULP and
/// collapses every in-stroke timestamp. Featurisers subtract this base in
/// integer arithmetic first — features only use `dt`, so the offset is free.
/// Lines mixing legacy and timed strokes keep the old absolute behaviour.
pub(crate) fn line_time_base_ms(line: &StrokeLine) -> u64 {
    let mut t0s = line
        .strokes
        .iter()
        .filter(|s| !s.points.is_empty())
        .map(|s| s.t0_ms);
    let first = t0s.next().unwrap_or(0);
    t0s.try_fold(first, |acc, t0| (t0 != 0).then(|| acc.min(t0)))
        .filter(|&b| b != 0)
        .unwrap_or(0)
}

fn build_absolute_points(line: &StrokeLine) -> Vec<CarbunePoint> {
    let mut out = Vec::new();
    let base_ms = line_time_base_ms(line);
    // Online models read ink in writing order. Segmentation sorts strokes
    // left-to-right, which puts i-dots, t-bars and retouches in the wrong
    // place; restore pen order whenever every stroke has a real start time.
    let mut strokes: Vec<&Stroke> = line.strokes.iter().collect();
    if base_ms != 0 {
        strokes.sort_by_key(|s| s.t0_ms);
    }
    let mut next_synth_t0_ms = 0.0f32;
    for (stroke_nr, stroke) in strokes.into_iter().enumerate() {
        if stroke.points.is_empty() {
            continue;
        }
        let t0_ms = if stroke.t0_ms == 0 {
            next_synth_t0_ms
        } else {
            stroke.t0_ms.saturating_sub(base_ms) as f32
        };
        let mut last_abs_ms = t0_ms;
        for p in &stroke.points {
            let abs_ms = t0_ms + p.t_ms as f32;
            last_abs_ms = abs_ms;
            out.push(CarbunePoint {
                x: p.x,
                y: p.y,
                t: abs_ms / 1000.0,
                stroke_nr: stroke_nr as u32,
            });
        }
        next_synth_t0_ms = last_abs_ms + LEGACY_INTER_STROKE_GAP_MS;
    }
    out
}

/// Carbune2020 / OnlineHTR `featuresLinInterpol20DxDyDtN` featurization.
///
/// 1. Flat table with absolute time in **seconds**, strokes in pen order
/// 2. Normalise: `y = -y` (y-up), `x -= x[0]`, `y -= y.min()`,
///    `scale = y.max()`, then `/scale`
/// 3. Per stroke, resample by **time** at `ceil(length * points_per_unit)`
///    (bump 1→2); fall back to arc-length if time is constant / non-monotone
/// 4. Emit `[0,0,0,1]` then `[dx, dy, dt, n]` (`n=1` when `stroke_nr` changes)
///
/// `points_per_unit` defaults to [`CARBUNE_POINTS_PER_UNIT`] (20) in callers.
pub fn featurize_line(line: &StrokeLine, points_per_unit: f32) -> Vec<HtrFeature> {
    let ppu = points_per_unit.max(1.0);
    let mut pts = build_absolute_points(line);
    if pts.is_empty() {
        return Vec::new();
    }

    // --- Normalise (Carbune2020 / OnlineHTR) ---
    // OnlineHTR is trained on y-up ink; screen coordinates are y-down. Without
    // the flip the model reads every line upside down (Note 19 replay:
    // 81% → ~5% CER).
    let x0 = pts[0].x;
    for p in &mut pts {
        p.y = -p.y;
    }
    let y_min = pts.iter().map(|p| p.y).fold(f32::INFINITY, f32::min);
    for p in &mut pts {
        p.x -= x0;
        p.y -= y_min;
    }
    let scale = pts.iter().map(|p| p.y).fold(f32::NEG_INFINITY, f32::max);
    let scale = if scale > 1e-12 { scale } else { 1.0 };
    for p in &mut pts {
        p.x /= scale;
        p.y /= scale;
    }

    // --- Per-stroke resample ---
    let mut resampled: Vec<CarbunePoint> = Vec::new();
    let mut i = 0;
    while i < pts.len() {
        let sn = pts[i].stroke_nr;
        let start = i;
        while i < pts.len() && pts[i].stroke_nr == sn {
            i += 1;
        }
        let stroke_pts = &pts[start..i];
        let length = stroke_path_length(stroke_pts);
        let mut nr = (length * ppu).ceil() as usize;
        if nr == 1 {
            nr = 2;
        }
        if stroke_pts.len() == 1 {
            resampled.push(stroke_pts[0]);
            continue;
        }
        if nr < 2 {
            nr = 2;
        }
        resampled.extend(resample_stroke(stroke_pts, nr));
    }

    if resampled.is_empty() {
        return Vec::new();
    }

    // --- Differencing ---
    let mut out = Vec::with_capacity(resampled.len());
    out.push([0.0, 0.0, 0.0, 1.0]);
    for w in resampled.windows(2) {
        let a = &w[0];
        let b = &w[1];
        let n = if b.stroke_nr != a.stroke_nr { 1.0 } else { 0.0 };
        out.push([b.x - a.x, b.y - a.y, b.t - a.t, n]);
    }
    out
}

fn stroke_path_length(pts: &[CarbunePoint]) -> f32 {
    let mut len = 0.0;
    for w in pts.windows(2) {
        len += (w[1].x - w[0].x).hypot(w[1].y - w[0].y);
    }
    len
}

/// Resample one stroke to `nr_points` via time parameterisation, or arc-length
/// when time is constant / non-monotone (production-safe fallback).
fn resample_stroke(pts: &[CarbunePoint], nr_points: usize) -> Vec<CarbunePoint> {
    if pts.is_empty() {
        return Vec::new();
    }
    if pts.len() == 1 || nr_points <= 1 {
        return pts.to_vec();
    }

    let use_time = stroke_time_is_usable(pts);
    let param: Vec<f32> = if use_time {
        let t_min = pts[0].t;
        let t_max = pts[pts.len() - 1].t;
        let denom = t_max - t_min;
        if denom.abs() < 1e-12 {
            return pts.to_vec();
        }
        pts.iter().map(|p| (p.t - t_min) / denom).collect()
    } else {
        let mut cum = Vec::with_capacity(pts.len());
        cum.push(0.0f32);
        for w in pts.windows(2) {
            let d = (w[1].x - w[0].x).hypot(w[1].y - w[0].y);
            cum.push(cum[cum.len() - 1] + d);
        }
        let total = *cum.last().unwrap_or(&0.0);
        if total < 1e-12 {
            return pts.to_vec();
        }
        cum.iter().map(|c| c / total).collect()
    };

    let p0 = param[0];
    let p1 = param[param.len() - 1];
    let grid = linspace(p0, p1, nr_points);
    let xs: Vec<f32> = pts.iter().map(|p| p.x).collect();
    let ys: Vec<f32> = pts.iter().map(|p| p.y).collect();
    let ts: Vec<f32> = pts.iter().map(|p| p.t).collect();
    let sn = pts[0].stroke_nr;

    grid.into_iter()
        .map(|u| CarbunePoint {
            x: interp1d(&param, &xs, u),
            y: interp1d(&param, &ys, u),
            t: interp1d(&param, &ts, u),
            stroke_nr: sn,
        })
        .collect()
}

fn stroke_time_is_usable(pts: &[CarbunePoint]) -> bool {
    if pts.len() < 2 {
        return false;
    }
    let mut all_zero = true;
    for w in pts.windows(2) {
        let dt = w[1].t - w[0].t;
        if dt < 0.0 {
            return false;
        }
        if dt > 1e-12 {
            all_zero = false;
        }
    }
    !all_zero
}

fn linspace(a: f32, b: f32, n: usize) -> Vec<f32> {
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![a];
    }
    let mut out = Vec::with_capacity(n);
    let denom = (n - 1) as f32;
    for i in 0..n {
        out.push(a + (b - a) * (i as f32) / denom);
    }
    out
}

/// Linear interpolation matching `scipy.interpolate.interp1d(..., kind='linear')`.
fn interp1d(xs: &[f32], ys: &[f32], x: f32) -> f32 {
    debug_assert_eq!(xs.len(), ys.len());
    if xs.is_empty() {
        return 0.0;
    }
    if x <= xs[0] {
        return ys[0];
    }
    if x >= xs[xs.len() - 1] {
        return ys[ys.len() - 1];
    }
    for i in 0..xs.len() - 1 {
        if xs[i] <= x && x <= xs[i + 1] {
            let d = xs[i + 1] - xs[i];
            if d.abs() < 1e-12 {
                return ys[i];
            }
            let t = (x - xs[i]) / d;
            return ys[i] + t * (ys[i + 1] - ys[i]);
        }
    }
    ys[ys.len() - 1]
}

/// Axis-aligned gap between the right edge of `left` and left edge of `right`.
fn stroke_x_gap(left: &Stroke, right: &Stroke) -> f32 {
    let Some((_, _, left_max_x, _)) = stroke_bounds(left) else {
        return 0.0;
    };
    let Some((right_min_x, _, _, _)) = stroke_bounds(right) else {
        return 0.0;
    };
    right_min_x - left_max_x
}

fn stroke_line_from_strokes(strokes: Vec<Stroke>) -> StrokeLine {
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for s in &strokes {
        if let Some((x0, y0, x1, y1)) = stroke_bounds(s) {
            min_x = min_x.min(x0);
            min_y = min_y.min(y0);
            max_x = max_x.max(x1);
            max_y = max_y.max(y1);
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

/// Split a line at the largest horizontal gap between consecutive strokes.
///
/// Returns `None` when there are fewer than two strokes.
pub fn split_stroke_line_at_largest_x_gap(line: &StrokeLine) -> Option<(StrokeLine, StrokeLine)> {
    if line.strokes.len() < 2 {
        return None;
    }
    let mut best_i = 0usize;
    let mut best_gap = f32::NEG_INFINITY;
    for i in 0..line.strokes.len() - 1 {
        let gap = stroke_x_gap(&line.strokes[i], &line.strokes[i + 1]);
        if gap > best_gap {
            best_gap = gap;
            best_i = i;
        }
    }
    let left = line.strokes[..=best_i].to_vec();
    let right = line.strokes[best_i + 1..].to_vec();
    Some((
        stroke_line_from_strokes(left),
        stroke_line_from_strokes(right),
    ))
}

/// Featurize a line, recursively splitting at the largest x-gap when the
/// feature sequence exceeds [`MAX_STROKE_HTR_TIMESTEPS`].
pub fn featurize_line_pieces(
    line: &StrokeLine,
    points_per_unit: f32,
    max_timesteps: usize,
) -> Vec<(StrokeLine, Vec<HtrFeature>)> {
    let feats = featurize_line(line, points_per_unit);
    if feats.len() <= max_timesteps || line.strokes.len() < 2 {
        return vec![(line.clone(), feats)];
    }
    let Some((left, right)) = split_stroke_line_at_largest_x_gap(line) else {
        return vec![(line.clone(), feats)];
    };
    // Avoid infinite recursion if a split somehow keeps all strokes on one side.
    if left.strokes.is_empty()
        || right.strokes.is_empty()
        || left.strokes.len() + right.strokes.len() != line.strokes.len()
    {
        return vec![(line.clone(), feats)];
    }
    let mut out = featurize_line_pieces(&left, points_per_unit, max_timesteps);
    out.extend(featurize_line_pieces(
        &right,
        points_per_unit,
        max_timesteps,
    ));
    out
}

/// Legacy raw-pixel resampling count (pre-Carbune normalisation) for tests.
///
/// Approximates the old `interpolate_stroke` behaviour: ~`ceil(dist * ppu)`
/// samples per segment, summed over the line (plus the first point).
#[cfg(test)]
pub fn legacy_raw_ppu_point_count(line: &StrokeLine, points_per_unit: f32) -> usize {
    let ppu = points_per_unit.max(1.0);
    let mut n = 0usize;
    for stroke in &line.strokes {
        if stroke.points.is_empty() {
            continue;
        }
        n += 1; // first point
        for w in stroke.points.windows(2) {
            let dist = (w[1].x - w[0].x).hypot(w[1].y - w[0].y);
            n += (dist * ppu).ceil().max(1.0) as usize;
        }
    }
    n
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

/// Mean softmax probability of the argmax class over non-blank timesteps.
///
/// Used as a cascade gate so weak OnlineHTR guesses fall through to OCR.
pub fn ctc_softmax_confidence(
    logits: &[f32],
    time_steps: usize,
    num_classes: usize,
    blank: usize,
) -> f32 {
    if time_steps == 0 || num_classes == 0 || logits.len() < time_steps * num_classes {
        return 0.0;
    }
    let mut sum = 0.0f32;
    let mut n = 0u32;
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
        if best_i == blank {
            continue;
        }
        // Stable softmax of the winning logit.
        let mut m = f32::NEG_INFINITY;
        for &v in row {
            m = m.max(v);
        }
        let mut denom = 0.0f32;
        for &v in row {
            denom += (v - m).exp();
        }
        if denom > 0.0 {
            sum += (best_v - m).exp() / denom;
            n += 1;
        }
    }
    if n == 0 {
        0.0
    } else {
        (sum / n as f32).clamp(0.0, 1.0)
    }
}

/// Settings for CTC prefix beam search with optional char-LM / lexicon boost.
#[derive(Debug, Clone, Copy)]
pub struct CtcBeamConfig {
    /// Number of prefixes kept per timestep (32 is cheap on RK3566).
    pub beam_width: usize,
    /// Weight for character bigram LM log-prob (0 = acoustic only).
    pub lm_weight: f32,
    /// Bonus added per lexicon-hit word in the final hypothesis.
    pub lexicon_weight: f32,
}

impl Default for CtcBeamConfig {
    fn default() -> Self {
        Self {
            beam_width: 32,
            lm_weight: 0.35,
            lexicon_weight: 0.45,
        }
    }
}

fn log_add(a: f32, b: f32) -> f32 {
    if a == f32::NEG_INFINITY {
        return b;
    }
    if b == f32::NEG_INFINITY {
        return a;
    }
    let m = a.max(b);
    m + ((a - m).exp() + (b - m).exp()).ln()
}

/// Character bigram LM over CTC label indices (blank excluded from context).
#[derive(Debug, Clone)]
pub struct CharBigramLm {
    /// `log P(next | prev)` with add-1 smoothing; indexed `[prev][next]`.
    /// Index 0 is BOS / after-blank context; label `i` maps to slot `i+1` when
    /// blank is 0, else slot `i` (blank-last alphabets).
    table: Vec<Vec<f32>>,
    slots: usize,
}

impl CharBigramLm {
    /// Build a smoothed bigram table from a newline-separated word list and the
    /// model charset (alphabet only, no blank).
    pub fn from_word_list(words: &str, charset: &str, blank: usize) -> Self {
        let chars: Vec<char> = charset.chars().collect();
        let n = chars.len();
        // slots = 1 (BOS) + n labels
        let slots = n + 1;
        let mut counts = vec![vec![0u32; slots]; slots];
        let mut uni = vec![0u32; slots];

        let char_to_label = |ch: char| -> Option<usize> {
            let idx = chars.iter().position(|&c| c == ch)?;
            Some(if blank == 0 { idx + 1 } else { idx })
        };
        let label_to_slot = |label: usize| -> Option<usize> {
            if label == blank {
                return None;
            }
            let idx = if blank == 0 {
                label.checked_sub(1)?
            } else {
                label
            };
            if idx >= n {
                return None;
            }
            Some(idx + 1) // +1 for BOS at 0
        };

        for word in words.lines() {
            let w = word.trim();
            if w.is_empty() || w.starts_with('#') {
                continue;
            }
            let mut prev_slot = 0usize; // BOS
            uni[prev_slot] += 1;
            for ch in w.chars() {
                let Some(label) = char_to_label(ch) else {
                    continue;
                };
                let Some(slot) = label_to_slot(label) else {
                    continue;
                };
                counts[prev_slot][slot] += 1;
                uni[slot] += 1;
                prev_slot = slot;
            }
            // End-of-word → space if present in charset (helps multi-word lines).
            if let Some(sp) = char_to_label(' ') {
                if let Some(slot) = label_to_slot(sp) {
                    counts[prev_slot][slot] += 1;
                    uni[slot] += 1;
                }
            }
        }

        let mut table = vec![vec![0.0f32; slots]; slots];
        for i in 0..slots {
            let row_sum = counts[i].iter().sum::<u32>() + slots as u32;
            for j in 0..slots {
                let c = counts[i][j] + 1;
                table[i][j] = (c as f32 / row_sum as f32).ln();
            }
        }
        let _ = uni;
        Self { table, slots }
    }

    fn slot_for_label(&self, label: usize, blank: usize, alphabet_len: usize) -> Option<usize> {
        if label == blank {
            return None;
        }
        let idx = if blank == 0 {
            label.checked_sub(1)?
        } else {
            label
        };
        if idx >= alphabet_len || idx + 1 >= self.slots {
            return None;
        }
        Some(idx + 1)
    }

    /// Log-prob of emitting `next` after prefix ending in `prev` (None = BOS).
    pub fn log_prob(
        &self,
        prev: Option<usize>,
        next: usize,
        blank: usize,
        alphabet_len: usize,
    ) -> f32 {
        let prev_slot = prev
            .and_then(|p| self.slot_for_label(p, blank, alphabet_len))
            .unwrap_or(0);
        let Some(next_slot) = self.slot_for_label(next, blank, alphabet_len) else {
            return 0.0;
        };
        self.table[prev_slot][next_slot]
    }
}

/// Relative weight for bundled / generic lexicon entries.
pub const GENERIC_LEXICON_WEIGHT: f32 = 1.0;
/// Relative weight for user-vault / task / calendar tokens (above generic).
pub const USER_LEXICON_WEIGHT: f32 = 1.75;
/// Weight applied when a hypothesis token matches a structured pattern
/// (`10:00`, `13.10.`, `morgen`, `@phone`, `#lane/now`) even if absent from the map.
pub const STRUCTURED_TOKEN_WEIGHT: f32 = USER_LEXICON_WEIGHT;

/// Weighted token map for final-hypothesis lexicon boost (ephemeris-ktg.13).
///
/// Values are relative weights: generic ≈ [`GENERIC_LEXICON_WEIGHT`], user
/// vocabulary ≈ [`USER_LEXICON_WEIGHT`]. [`Self::hit_ratio`] averages lookup
/// weights (0 for misses), so user hits outrank generic hits in beam rescoring.
#[derive(Debug, Clone, Default)]
pub struct HtrLexicon {
    words: std::collections::HashMap<String, f32>,
}

impl HtrLexicon {
    /// Build from a newline-separated word list at [`GENERIC_LEXICON_WEIGHT`].
    pub fn from_word_list(text: &str) -> Self {
        Self::from_word_list_weighted(text, GENERIC_LEXICON_WEIGHT)
    }

    /// Build from a newline-separated word list at a uniform weight.
    pub fn from_word_list_weighted(text: &str, weight: f32) -> Self {
        let mut lex = Self::default();
        for line in text.lines() {
            let w = line.trim();
            if w.is_empty() || w.starts_with('#') {
                continue;
            }
            lex.insert(w, weight);
        }
        lex
    }

    /// Number of stored tokens (excludes pure structured-pattern matches).
    pub fn len(&self) -> usize {
        self.words.len()
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// Insert or raise the weight for `token` (lowercased).
    pub fn insert(&mut self, token: &str, weight: f32) {
        let key = token.trim().to_lowercase();
        if key.is_empty() {
            return;
        }
        self.words
            .entry(key)
            .and_modify(|w| *w = (*w).max(weight))
            .or_insert(weight);
    }

    /// Merge another lexicon, keeping the higher weight per token.
    pub fn merge(&mut self, other: &HtrLexicon) {
        for (k, &w) in &other.words {
            self.insert(k, w);
        }
    }

    /// Merge `other` after scaling its weights by `weight` (e.g. user list).
    pub fn merge_weighted(&mut self, other: &HtrLexicon, weight: f32) {
        for (k, &w) in &other.words {
            self.insert(
                k,
                w.max(GENERIC_LEXICON_WEIGHT) * weight / GENERIC_LEXICON_WEIGHT,
            );
        }
    }

    /// Stored weight for `token`, or `0.0` if absent (ignores structured patterns).
    pub fn weight_of(&self, token: &str) -> f32 {
        self.words
            .get(&token.trim().to_lowercase())
            .copied()
            .unwrap_or(0.0)
    }

    /// Lookup weight including structured-token recognition.
    pub fn lookup_weight(&self, token: &str) -> f32 {
        let key = token.trim().to_lowercase();
        if key.is_empty() {
            return 0.0;
        }
        if let Some(&w) = self.words.get(&key) {
            return w;
        }
        if is_structured_htr_token(&key) {
            return STRUCTURED_TOKEN_WEIGHT;
        }
        0.0
    }

    /// Mean token weight over the hypothesis (0 for misses). User / structured
    /// hits score above generic hits.
    pub fn hit_ratio(&self, text: &str) -> f32 {
        let toks = tokenize_htr_text(text);
        if toks.is_empty() {
            return 0.0;
        }
        let sum: f32 = toks.iter().map(|t| self.lookup_weight(t)).sum();
        sum / toks.len() as f32
    }
}

/// Bundled EN+DE note lexicon (compile-time).
pub fn bundled_htr_lexicon_text() -> &'static str {
    include_str!("../../data/htr_lexicon.txt")
}

/// Default LM + lexicon built from the bundled word list and charset.
pub fn default_htr_language_context(charset: &str, blank: usize) -> (CharBigramLm, HtrLexicon) {
    let text = bundled_htr_lexicon_text();
    (
        CharBigramLm::from_word_list(text, charset, blank),
        HtrLexicon::from_word_list(text),
    )
}

/// Bundled lexicon merged with an optional on-disk user vocabulary.
pub fn merged_htr_lexicon(user_lexicon_text: Option<&str>) -> HtrLexicon {
    let mut lex = HtrLexicon::from_word_list(bundled_htr_lexicon_text());
    if let Some(user) = user_lexicon_text {
        let user_lex = HtrLexicon::from_word_list_weighted(user, USER_LEXICON_WEIGHT);
        lex.merge(&user_lex);
    }
    lex
}

/// Tokenise hypothesis / source text for lexicon scoring (words + structured forms).
pub fn tokenize_htr_text(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        // @context / @mention
        if chars[i] == '@' {
            let start = i;
            i += 1;
            while i < chars.len()
                && (chars[i].is_alphanumeric()
                    || chars[i] == '_'
                    || chars[i] == '-'
                    || chars[i] == '/')
            {
                i += 1;
            }
            if i > start + 1 {
                out.push(chars[start..i].iter().collect::<String>().to_lowercase());
                continue;
            }
            continue;
        }
        // #lane/… #ctx/…
        if chars[i] == '#' {
            let start = i;
            i += 1;
            while i < chars.len()
                && (chars[i].is_alphanumeric()
                    || chars[i] == '_'
                    || chars[i] == '-'
                    || chars[i] == '/')
            {
                i += 1;
            }
            if i > start + 1 {
                out.push(chars[start..i].iter().collect::<String>().to_lowercase());
                continue;
            }
            continue;
        }
        // Digits → time / date candidates
        if chars[i].is_ascii_digit() {
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_digit() || chars[i] == ':' || chars[i] == '.')
            {
                i += 1;
            }
            let tok: String = chars[start..i].iter().collect::<String>().to_lowercase();
            if is_structured_htr_token(&tok) || tok.chars().any(|c| c.is_alphabetic()) {
                out.push(tok);
            } else if tok.len() >= 2 {
                // Keep multi-digit runs that look like dates without trailing dot.
                if is_loose_numeric_date(&tok) {
                    out.push(tok);
                }
            }
            continue;
        }
        // Alphabetic word (incl. hyphen / apostrophe)
        if chars[i].is_alphabetic() {
            let start = i;
            while i < chars.len()
                && (chars[i].is_alphabetic() || chars[i] == '\'' || chars[i] == '-')
            {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect::<String>().to_lowercase();
            if !word.is_empty() {
                out.push(word);
            }
            continue;
        }
        i += 1;
    }
    out
}

fn is_loose_numeric_date(tok: &str) -> bool {
    // DD.MM or DD.MM.YYYY without requiring trailing dot
    let parts: Vec<&str> = tok.split('.').filter(|p| !p.is_empty()).collect();
    if parts.len() == 2 || parts.len() == 3 {
        return parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) && p.len() <= 4);
    }
    false
}

/// True for dates/times, relative day words, `@context`, and `#lane` / `#ctx` tags.
pub fn is_structured_htr_token(tok: &str) -> bool {
    let t = tok.trim().to_lowercase();
    if t.is_empty() {
        return false;
    }
    if let Some(name) = t.strip_prefix('@') {
        return !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '/');
    }
    if let Some(rest) = t.strip_prefix('#') {
        return rest.starts_with("lane/")
            || rest.starts_with("ctx/")
            || rest.starts_with("context/");
    }
    match t.as_str() {
        "morgen" | "tomorrow" | "heute" | "today" | "gestern" | "yesterday" | "übermorgen"
        | "uebermorgen" => return true,
        _ => {}
    }
    if is_time_token(&t) || is_de_date_token(&t) {
        return true;
    }
    false
}

fn is_time_token(t: &str) -> bool {
    // 10:00, 9:30, 10.00 (German)
    let sep = if t.contains(':') {
        ':'
    } else if t.matches('.').count() == 1 {
        '.'
    } else {
        return false;
    };
    let mut parts = t.split(sep);
    let (Some(h), Some(m), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    if h.is_empty() || m.len() != 2 {
        return false;
    }
    if !h.chars().all(|c| c.is_ascii_digit()) || !m.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let Ok(hh) = h.parse::<u32>() else {
        return false;
    };
    let Ok(mm) = m.parse::<u32>() else {
        return false;
    };
    hh <= 23 && mm <= 59 && h.len() <= 2
}

fn is_de_date_token(t: &str) -> bool {
    // 13.10. or 13.10.2024
    if !t.contains('.') {
        return false;
    }
    let trailing_dot = t.ends_with('.');
    let core = t.trim_end_matches('.');
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 2 && parts.len() != 3 {
        return false;
    }
    if !parts
        .iter()
        .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    {
        return false;
    }
    let Ok(d) = parts[0].parse::<u32>() else {
        return false;
    };
    let Ok(m) = parts[1].parse::<u32>() else {
        return false;
    };
    if !(1..=31).contains(&d) || !(1..=12).contains(&m) {
        return false;
    }
    if parts.len() == 3 {
        let y = parts[2];
        if y.len() != 2 && y.len() != 4 {
            return false;
        }
    }
    // Prefer forms with trailing dot (13.10.) or full year; bare DD.MM also ok.
    trailing_dot || parts.len() == 3 || parts.len() == 2
}

#[derive(Clone)]
struct BeamState {
    /// Collapsed label prefix (no blanks / no CTC repeats).
    prefix: Vec<usize>,
    /// Log-prob ending in blank.
    pb: f32,
    /// Log-prob ending in non-blank.
    pnb: f32,
}

impl BeamState {
    fn score(&self) -> f32 {
        log_add(self.pb, self.pnb)
    }
}

/// Shared CTC frame layout for beam / LM decode helpers.
#[derive(Debug, Clone, Copy)]
pub struct CtcFrame<'a> {
    /// Row-major `[T * C]` **log**-probabilities (OnlineHTR LogSoftmax).
    pub log_probs: &'a [f32],
    pub time_steps: usize,
    pub num_classes: usize,
    pub blank: usize,
    pub charset: &'a str,
}

/// Prefix beam search over CTC log-probabilities (Hannun-style) with optional
/// character LM and lexicon rescoring of the final beam.
pub fn ctc_prefix_beam_decode(
    frame: CtcFrame<'_>,
    config: CtcBeamConfig,
    lm: Option<&CharBigramLm>,
    lexicon: Option<&HtrLexicon>,
) -> Vec<usize> {
    let CtcFrame {
        log_probs,
        time_steps,
        num_classes,
        blank,
        charset,
    } = frame;
    if time_steps == 0 || num_classes == 0 || log_probs.len() < time_steps * num_classes {
        return Vec::new();
    }
    let alphabet_len = charset.chars().count();
    let beam_width = config.beam_width.max(1);

    let mut beams = vec![BeamState {
        prefix: Vec::new(),
        pb: 0.0, // log(1)
        pnb: f32::NEG_INFINITY,
    }];

    for t in 0..time_steps {
        let row = &log_probs[t * num_classes..(t + 1) * num_classes];
        // Merge by prefix key.
        let mut next: std::collections::HashMap<Vec<usize>, BeamState> =
            std::collections::HashMap::new();

        for beam in &beams {
            let last = beam.prefix.last().copied();
            let total = beam.score();

            // Blank → same prefix, ending blank.
            {
                let p = total + row[blank];
                let e = next.entry(beam.prefix.clone()).or_insert(BeamState {
                    prefix: beam.prefix.clone(),
                    pb: f32::NEG_INFINITY,
                    pnb: f32::NEG_INFINITY,
                });
                e.pb = log_add(e.pb, p);
            }

            for (c, &lp) in row.iter().enumerate() {
                if c == blank {
                    continue;
                }

                if last == Some(c) {
                    // Emitting `c` again without a blank stays on the same
                    // collapsed prefix (non-blank path only).
                    let p_stay = beam.pnb + lp;
                    let e = next.entry(beam.prefix.clone()).or_insert(BeamState {
                        prefix: beam.prefix.clone(),
                        pb: f32::NEG_INFINITY,
                        pnb: f32::NEG_INFINITY,
                    });
                    e.pnb = log_add(e.pnb, p_stay);

                    // A *new* occurrence of `c` requires the blank path first.
                    let mut p_new = beam.pb + lp;
                    if let Some(lm) = lm {
                        p_new += config.lm_weight * lm.log_prob(last, c, blank, alphabet_len);
                    }
                    let mut new_prefix = beam.prefix.clone();
                    new_prefix.push(c);
                    let e = next.entry(new_prefix.clone()).or_insert(BeamState {
                        prefix: new_prefix,
                        pb: f32::NEG_INFINITY,
                        pnb: f32::NEG_INFINITY,
                    });
                    e.pnb = log_add(e.pnb, p_new);
                } else {
                    let mut p_ext = total + lp;
                    if let Some(lm) = lm {
                        p_ext += config.lm_weight * lm.log_prob(last, c, blank, alphabet_len);
                    }
                    let mut new_prefix = beam.prefix.clone();
                    new_prefix.push(c);
                    let e = next.entry(new_prefix.clone()).or_insert(BeamState {
                        prefix: new_prefix,
                        pb: f32::NEG_INFINITY,
                        pnb: f32::NEG_INFINITY,
                    });
                    e.pnb = log_add(e.pnb, p_ext);
                }
            }
        }

        let mut ranked: Vec<BeamState> = next.into_values().collect();
        ranked.sort_by(|a, b| {
            b.score()
                .partial_cmp(&a.score())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        ranked.truncate(beam_width);
        beams = ranked;
        if beams.is_empty() {
            break;
        }
    }

    if beams.is_empty() {
        return Vec::new();
    }

    // Lexicon rescoring of survivors.
    if let Some(lex) = lexicon {
        for b in &mut beams {
            let text = decode_charset_with_blank(&b.prefix, charset, blank);
            let bonus = config.lexicon_weight * lex.hit_ratio(&text);
            // Fold bonus into pb so score() picks it up.
            b.pb = log_add(b.pb, bonus);
        }
        beams.sort_by(|a, b| {
            b.score()
                .partial_cmp(&a.score())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    beams
        .into_iter()
        .next()
        .map(|b| b.prefix)
        .unwrap_or_default()
}

/// Decode with beam+LM when helpful; fall back to greedy if beam is empty.
pub fn ctc_decode_with_lm(
    frame: CtcFrame<'_>,
    config: CtcBeamConfig,
    lm: Option<&CharBigramLm>,
    lexicon: Option<&HtrLexicon>,
) -> Vec<usize> {
    let beam = ctc_prefix_beam_decode(frame, config, lm, lexicon);
    if beam.is_empty() {
        ctc_greedy_decode(
            frame.log_probs,
            frame.time_steps,
            frame.num_classes,
            frame.blank,
        )
    } else {
        beam
    }
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
    use crate::model::{Color, Point, StrokeId};

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
            t0_ms: 0,
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

    fn body_stroke(x0: f32, y0: f32, x1: f32, y1: f32) -> Stroke {
        Stroke {
            id: StrokeId::new(),
            tool: Tool::Pen,
            color: Color::BLACK,
            base_width: 2.0,
            points: vec![
                Point::new(x0, y0, 0.5, 0.0, 0),
                Point::new(x1, y1, 0.5, 0.0, 80),
            ],
            t0_ms: 0,
        }
    }

    fn mark_dot(x: f32, y: f32) -> Stroke {
        Stroke {
            id: StrokeId::new(),
            tool: Tool::Pen,
            color: Color::BLACK,
            base_width: 1.5,
            points: vec![
                Point::new(x, y, 0.4, 0.0, 0),
                Point::new(x + 1.2, y + 0.8, 0.4, 0.0, 20),
            ],
            t0_ms: 0,
        }
    }

    /// "Bäume über Tür" — umlaut / i-dots sit above the x-height but stay one line.
    #[test]
    fn baeume_ueber_tuer_dots_one_line() {
        let mut strokes = vec![
            body_stroke(10.0, 100.0, 30.0, 118.0),   // B
            body_stroke(35.0, 102.0, 55.0, 120.0),   // ä body
            body_stroke(60.0, 100.0, 85.0, 118.0),   // u
            body_stroke(90.0, 102.0, 115.0, 120.0),  // m
            body_stroke(120.0, 100.0, 145.0, 118.0), // e
            body_stroke(160.0, 102.0, 185.0, 120.0), // ü body
            body_stroke(190.0, 100.0, 215.0, 118.0), // b
            body_stroke(220.0, 102.0, 245.0, 120.0), // e
            body_stroke(250.0, 100.0, 275.0, 118.0), // r
            body_stroke(290.0, 102.0, 320.0, 120.0), // T
            body_stroke(325.0, 100.0, 350.0, 118.0), // ü body
            body_stroke(355.0, 102.0, 380.0, 120.0), // r
        ];
        // Umlaut dots + i-dot above the line core.
        strokes.push(mark_dot(42.0, 78.0));
        strokes.push(mark_dot(48.0, 77.0));
        strokes.push(mark_dot(168.0, 76.0));
        strokes.push(mark_dot(174.0, 77.0));
        strokes.push(mark_dot(333.0, 78.0));
        strokes.push(mark_dot(339.0, 76.0));
        let gap = default_line_gap(&strokes);
        let lines = group_strokes_into_lines(&strokes, gap);
        assert_eq!(lines.len(), 1, "dots must attach to the writing line");
        assert!(lines[0].strokes.len() >= 15);
    }

    /// Descenders from line 1 may touch line 2 ink without merging the lines.
    #[test]
    fn two_lines_descenders_touching_stay_separate() {
        let mut strokes = Vec::new();
        // Line 1 bodies (core ~ y=50).
        for i in 0..4 {
            let x = 10.0 + i as f32 * 40.0;
            strokes.push(body_stroke(x, 42.0, x + 28.0, 58.0));
        }
        // Descender on line 1 extending down to ~95 (touches line 2).
        strokes.push(body_stroke(50.0, 48.0, 55.0, 96.0));
        // Line 2 bodies (core ~ y=110); top at 95 so extents touch.
        for i in 0..4 {
            let x = 10.0 + i as f32 * 40.0;
            strokes.push(body_stroke(x, 98.0, x + 28.0, 122.0));
        }
        let gap = default_line_gap(&strokes);
        let lines = group_strokes_into_lines(&strokes, gap);
        assert_eq!(lines.len(), 2, "touching descenders must not merge lines");
    }

    /// A gently slanted baseline still clusters as one line.
    #[test]
    fn slightly_slanted_line_stays_one() {
        let mut strokes = Vec::new();
        for i in 0..8 {
            let x = 10.0 + i as f32 * 35.0;
            let y = 80.0 + i as f32 * 2.5; // ~11° over the run
            strokes.push(body_stroke(x, y, x + 28.0, y + 16.0));
        }
        let gap = default_line_gap(&strokes);
        let lines = group_strokes_into_lines(&strokes, gap);
        assert_eq!(lines.len(), 1, "gentle slant should remain a single line");
    }

    #[test]
    fn ruling_pitch_keeps_lined_rows_apart() {
        let strokes = vec![
            body_stroke(10.0, 40.0, 80.0, 54.0),
            body_stroke(10.0, 68.0, 80.0, 82.0), // 28 px ruling
        ];
        let lines =
            group_strokes_into_lines_with_ruling(&strokes, 40.0, Some(LINED_TEMPLATE_PITCH));
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn featurize_emits_dx_dy_channels() {
        // Non-zero height so Carbune y-scale is defined.
        let line = StrokeLine {
            strokes: vec![Stroke {
                id: StrokeId::new(),
                tool: Tool::Pen,
                color: Color::BLACK,
                base_width: 2.0,
                points: vec![
                    Point::new(0.0, 0.0, 0.5, 0.0, 0),
                    Point::new(20.0, 10.0, 0.5, 0.0, 100),
                ],
                t0_ms: 1000,
            }],
            x: 0.0,
            y: 0.0,
            width: 20.0,
            height: 10.0,
        };
        let feats = featurize_line(&line, CARBUNE_POINTS_PER_UNIT);
        assert!(feats.len() >= 2);
        assert_eq!(feats[0], [0.0, 0.0, 0.0, 1.0]);
        // Normalised steps should be O(0.01–0.1), not raw-pixel O(1+).
        let mean_abs_dx: f32 =
            feats.iter().skip(1).map(|f| f[0].abs()).sum::<f32>() / (feats.len() - 1) as f32;
        assert!(
            mean_abs_dx > 0.001 && mean_abs_dx < 0.5,
            "mean |dx|={mean_abs_dx} out of Carbune range"
        );
        assert!(feats.iter().any(|f| f[0] > 0.0));
    }

    fn parity_stroke(t0_ms: u64, points: &[(f32, f32, u32)]) -> Stroke {
        Stroke {
            id: StrokeId::new(),
            tool: Tool::Pen,
            color: Color::BLACK,
            base_width: 2.0,
            points: points
                .iter()
                .map(|&(x, y, t)| Point::new(x, y, 0.5, 0.0, t))
                .collect(),
            t0_ms,
        }
    }

    #[test]
    fn carbune_parity_real_t0_matches_fixture() {
        let line = StrokeLine {
            strokes: vec![
                parity_stroke(1000, &[(0.0, 0.0, 0), (8.0, 6.0, 100)]),
                parity_stroke(2000, &[(20.0, 100.0, 0), (28.0, 100.0, 100)]),
            ],
            x: 0.0,
            y: 0.0,
            width: 28.0,
            height: 100.0,
        };
        let feats = featurize_line(&line, CARBUNE_POINTS_PER_UNIT);
        let expected: [[f32; 4]; 4] = [
            [0.0, 0.0, 0.0, 1.0],
            [0.08, -0.06, 0.1, 0.0],
            [0.12, -0.94, 0.9, 1.0],
            [0.08, 0.0, 0.1, 0.0],
        ];
        assert_eq!(feats.len(), expected.len());
        for (got, exp) in feats.iter().zip(expected.iter()) {
            for i in 0..4 {
                assert!(
                    (got[i] - exp[i]).abs() < 1e-3,
                    "channel {i}: got {} expected {}",
                    got[i],
                    exp[i]
                );
            }
        }
    }

    /// Two-stroke line with uneven in-stroke timing, starting at `t0_ms`.
    pub(crate) fn epoch_offset_line(t0_ms: u64) -> StrokeLine {
        StrokeLine {
            strokes: vec![
                parity_stroke(
                    t0_ms,
                    &[
                        (0.0, 0.0, 0),
                        (6.0, 10.0, 8),
                        (10.0, 30.0, 40),
                        (12.0, 60.0, 90),
                    ],
                ),
                parity_stroke(
                    t0_ms + 230,
                    &[(20.0, 20.0, 0), (30.0, 25.0, 16), (45.0, 22.0, 70)],
                ),
            ],
            x: 0.0,
            y: 0.0,
            width: 45.0,
            height: 60.0,
        }
    }

    pub(crate) fn assert_features_close(a: &[HtrFeature], b: &[HtrFeature]) {
        assert_eq!(a.len(), b.len());
        for (fa, fb) in a.iter().zip(b) {
            for i in 0..4 {
                assert!(
                    (fa[i] - fb[i]).abs() < 1e-5,
                    "channel {i}: {fa:?} vs {fb:?}"
                );
            }
        }
    }

    #[test]
    fn featurize_is_invariant_to_epoch_ms_t0() {
        // Live strokes carry wall-clock epoch ms truncated to u32 (~4.29e9),
        // where f32 has a 256 ms ULP. Features must match a small-t0 line.
        let small = featurize_line(&epoch_offset_line(1_000), CARBUNE_POINTS_PER_UNIT);
        let epoch = featurize_line(&epoch_offset_line(4_293_604_864), CARBUNE_POINTS_PER_UNIT);
        assert_features_close(&small, &epoch);
        // Inter-stroke dt survives: 230 ms start gap − 90 ms first-stroke span.
        let pen_up = epoch.iter().skip(1).find(|f| f[3] == 1.0).unwrap();
        assert!(
            (pen_up[2] - 0.140).abs() < 1e-4,
            "inter-stroke dt {}",
            pen_up[2]
        );
    }

    #[test]
    fn featurize_uses_pen_order_not_left_to_right() {
        // An i-dot written last but sorted first by x must still come last.
        let mut line = epoch_offset_line(1_000);
        let dot = parity_stroke(1_000 + 500, &[(1.0, -10.0, 0), (1.5, -10.0, 10)]);
        line.strokes.insert(0, dot.clone());
        let mut pen_order = epoch_offset_line(1_000);
        pen_order.strokes.push(dot);
        assert_features_close(
            &featurize_line(&line, CARBUNE_POINTS_PER_UNIT),
            &featurize_line(&pen_order, CARBUNE_POINTS_PER_UNIT),
        );
    }

    #[test]
    fn carbune_parity_synth_t0_matches_fixture() {
        // Hand-computed: legacy t0_ms=0 → synthetic clock with 150 ms gaps.
        // S0 abs ms 0,100; S1 starts at 100+150=250 → 250,350 → seconds 0.25,0.35.
        let line = StrokeLine {
            strokes: vec![
                parity_stroke(0, &[(0.0, 0.0, 0), (8.0, 6.0, 100)]),
                parity_stroke(0, &[(20.0, 100.0, 0), (28.0, 100.0, 100)]),
            ],
            x: 0.0,
            y: 0.0,
            width: 28.0,
            height: 100.0,
        };
        let feats = featurize_line(&line, CARBUNE_POINTS_PER_UNIT);
        let expected: [[f32; 4]; 4] = [
            [0.0, 0.0, 0.0, 1.0],
            [0.08, -0.06, 0.1, 0.0],
            [0.12, -0.94, 0.15, 1.0],
            [0.08, 0.0, 0.1, 0.0],
        ];
        assert_eq!(feats.len(), expected.len());
        for (got, exp) in feats.iter().zip(expected.iter()) {
            for i in 0..4 {
                assert!(
                    (got[i] - exp[i]).abs() < 1e-3,
                    "channel {i}: got {} expected {}",
                    got[i],
                    exp[i]
                );
            }
        }
    }

    #[test]
    fn featurize_reduces_timesteps_vs_legacy_raw_ppu() {
        // ~100 px tall line with a few strokes — old 20 ppu on raw pixels
        // produced thousands of steps; Carbune normalise → O(path/height).
        let mut strokes = Vec::new();
        for i in 0..4 {
            let x0 = i as f32 * 40.0;
            strokes.push(Stroke {
                id: StrokeId::new(),
                tool: Tool::Pen,
                color: Color::BLACK,
                base_width: 2.0,
                points: vec![
                    Point::new(x0, 0.0, 0.5, 0.0, 0),
                    Point::new(x0, 50.0, 0.5, 0.0, 50),
                    Point::new(x0 + 20.0, 100.0, 0.5, 0.0, 100),
                    Point::new(x0 + 30.0, 80.0, 0.5, 0.0, 150),
                ],
                t0_ms: 1000 + i as u64 * 500,
            });
        }
        let line = stroke_line_from_strokes(strokes);
        let legacy = legacy_raw_ppu_point_count(&line, CARBUNE_POINTS_PER_UNIT);
        let feats = featurize_line(&line, CARBUNE_POINTS_PER_UNIT);
        assert!(
            feats.len() * 10 <= legacy,
            "expected ≥10× reduction: carbune T={} legacy≈{legacy}",
            feats.len()
        );
        // Surface the numbers for the bead summary / CI logs.
        eprintln!(
            "timestep reduction: carbune={} legacy_raw≈{} ratio={:.1}×",
            feats.len(),
            legacy,
            legacy as f32 / feats.len() as f32
        );
    }

    #[test]
    fn splits_when_over_timestep_cap() {
        // Many well-separated short strokes → force a split path via pieces API
        // by using a tiny max_timesteps.
        let strokes: Vec<Stroke> = (0..6)
            .map(|i| {
                let x = i as f32 * 50.0;
                Stroke {
                    id: StrokeId::new(),
                    tool: Tool::Pen,
                    color: Color::BLACK,
                    base_width: 2.0,
                    points: vec![
                        Point::new(x, 0.0, 0.5, 0.0, 0),
                        Point::new(x + 10.0, 20.0, 0.5, 0.0, 50),
                    ],
                    t0_ms: 1000 + i as u64 * 200,
                }
            })
            .collect();
        let line = stroke_line_from_strokes(strokes);
        let pieces = featurize_line_pieces(&line, CARBUNE_POINTS_PER_UNIT, 8);
        assert!(pieces.len() >= 2, "expected split into multiple pieces");
        assert!(
            pieces
                .iter()
                .all(|(p, _)| p.strokes.len() < line.strokes.len()),
            "each piece should be a strict subset of the original line"
        );
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

    #[test]
    fn softmax_confidence_high_for_peaked_logits() {
        // One non-blank timestep with a clear winner.
        let logits = [
            0.0, 5.0, 0.0, // class 1
            5.0, 0.0, 0.0, // blank (0)
        ];
        let conf = ctc_softmax_confidence(&logits, 2, 3, 0);
        assert!(conf > 0.9, "expected high confidence, got {conf}");
    }

    #[test]
    fn beam_decode_matches_greedy_on_clear_path() {
        // blank-last: classes 0=a, 1=b, 2=blank — clear "ab"
        let log_probs = [
            0.0, -10.0, -10.0, // a
            0.0, -10.0, -10.0, // a repeat
            -10.0, 0.0, -10.0, // b
        ];
        let greedy = ctc_greedy_decode(&log_probs, 3, 3, 2);
        let beam = ctc_prefix_beam_decode(
            CtcFrame {
                log_probs: &log_probs,
                time_steps: 3,
                num_classes: 3,
                blank: 2,
                charset: "ab",
            },
            CtcBeamConfig {
                beam_width: 4,
                lm_weight: 0.0,
                lexicon_weight: 0.0,
            },
            None,
            None,
        );
        assert_eq!(greedy, vec![0, 1]);
        assert_eq!(beam, greedy);
    }

    #[test]
    fn lexicon_prefers_dictionary_word() {
        // blank=0; labels 1=t, 2=h, 3=e, 4=x  charset "thex"
        // Ambiguous last char: acoustic slightly prefers "x", lexicon prefers "the"
        let charset = "thex";
        let blank = 0usize;
        // timesteps: t, h, e/x
        // classes: 0=blank, 1=t, 2=h, 3=e, 4=x
        let mut log_probs = vec![-20.0f32; 15];
        // t @ t=0 class 1; h @ t=1 class 2; prefer x over e at t=2
        log_probs[1] = 0.0;
        log_probs[7] = 0.0;
        log_probs[14] = -0.1;
        log_probs[13] = -0.3;

        let greedy = ctc_greedy_decode(&log_probs, 3, 5, blank);
        let greedy_text = decode_charset_with_blank(&greedy, charset, blank);
        assert_eq!(greedy_text, "thx");

        let lex = HtrLexicon::from_word_list("the\n");
        let beam = ctc_prefix_beam_decode(
            CtcFrame {
                log_probs: &log_probs,
                time_steps: 3,
                num_classes: 5,
                blank,
                charset,
            },
            CtcBeamConfig {
                beam_width: 8,
                lm_weight: 0.0,
                lexicon_weight: 2.0, // strong boost for the test
            },
            None,
            Some(&lex),
        );
        let beam_text = decode_charset_with_blank(&beam, charset, blank);
        assert_eq!(beam_text, "the");
    }

    #[test]
    fn bundled_lexicon_loads() {
        let text = bundled_htr_lexicon_text();
        let lex = HtrLexicon::from_word_list(text);
        assert!(lex.hit_ratio("the") > 0.9);
        assert!(lex.hit_ratio("und") > 0.9);
        assert!(lex.hit_ratio("the meeting") > 0.9);
    }

    #[test]
    fn structured_tokens_recognised() {
        assert!(is_structured_htr_token("10:00"));
        assert!(is_structured_htr_token("9:30"));
        assert!(is_structured_htr_token("13.10."));
        assert!(is_structured_htr_token("morgen"));
        assert!(is_structured_htr_token("tomorrow"));
        assert!(is_structured_htr_token("@phone"));
        assert!(is_structured_htr_token("#lane/now"));
        assert!(!is_structured_htr_token("hello"));
        let toks = tokenize_htr_text("Call @phone at 10:00 tomorrow #lane/now 13.10.");
        assert!(toks.iter().any(|t| t == "10:00"));
        assert!(toks.iter().any(|t| t == "@phone"));
        assert!(toks.iter().any(|t| t == "#lane/now"));
        assert!(toks.iter().any(|t| t == "tomorrow"));
        assert!(toks.iter().any(|t| t == "13.10."));
    }

    #[test]
    fn merge_keeps_higher_user_weight() {
        let mut lex = HtrLexicon::from_word_list("alpha\n");
        let user = HtrLexicon::from_word_list_weighted("alpha\nbeta\n", USER_LEXICON_WEIGHT);
        lex.merge(&user);
        assert!((lex.weight_of("alpha") - USER_LEXICON_WEIGHT).abs() < f32::EPSILON);
        assert!((lex.weight_of("beta") - USER_LEXICON_WEIGHT).abs() < f32::EPSILON);
        let merged = merged_htr_lexicon(Some("customname\n"));
        assert!(merged.weight_of("customname") > merged.weight_of("the"));
    }

    /// Pen strokes forming "Hallo" with a wide highlighter stroke over them
    /// groups into exactly 1 line containing no Highlighter strokes.
    #[test]
    fn highlighter_excluded_from_grouped_lines() {
        let mut strokes = vec![
            // Five pen strokes roughly forming "Hallo"
            body_stroke(10.0, 100.0, 15.0, 118.0), // H
            body_stroke(30.0, 102.0, 35.0, 118.0), // a
            body_stroke(50.0, 100.0, 55.0, 118.0), // l
            body_stroke(70.0, 100.0, 75.0, 118.0), // l
            body_stroke(90.0, 102.0, 95.0, 118.0), // o
        ];
        // Add a wide highlighter stroke spanning the entire line
        let highlighter = Stroke {
            id: StrokeId::new(),
            tool: Tool::Highlighter,
            color: Color::BLACK,
            base_width: 15.0, // much wider than pen strokes
            points: vec![
                Point::new(5.0, 110.0, 0.5, 0.0, 0),
                Point::new(100.0, 110.0, 0.5, 0.0, 100),
            ],
            t0_ms: 0,
        };
        strokes.push(highlighter);

        let gap = default_line_gap(&strokes);
        let lines = group_strokes_into_lines(&strokes, gap);

        // Should group into exactly 1 line
        assert_eq!(lines.len(), 1, "pen + highlighter should group as 1 line");

        // The line must contain no Highlighter strokes (only pen strokes)
        let line = &lines[0];
        assert!(
            line.strokes.iter().all(|s| s.tool == Tool::Pen),
            "line should contain only Pen strokes, no Highlighter"
        );
        assert_eq!(
            line.strokes.len(),
            5,
            "line should contain the 5 pen strokes only"
        );
    }

    /// rasterize_strokes output is identical with and without a highlighter stroke.
    #[test]
    fn rasterize_ignores_highlighter_strokes() {
        let pen_strokes = vec![
            body_stroke(10.0, 10.0, 50.0, 30.0),
            body_stroke(60.0, 10.0, 100.0, 30.0),
        ];

        // Rasterize without highlighter
        let (w1, h1, gray1) = crate::transcriber::rasterize_strokes(&pen_strokes);

        // Add a wide highlighter stroke that would affect the bounds if included
        let mut strokes_with_hl = pen_strokes.clone();
        strokes_with_hl.push(Stroke {
            id: StrokeId::new(),
            tool: Tool::Highlighter,
            color: Color::BLACK,
            base_width: 20.0,
            points: vec![
                Point::new(5.0, 15.0, 0.5, 0.0, 0),
                Point::new(110.0, 25.0, 0.5, 0.0, 100),
            ],
            t0_ms: 0,
        });

        // Rasterize with highlighter
        let (w2, h2, gray2) = crate::transcriber::rasterize_strokes(&strokes_with_hl);

        // Dimensions must be identical (highlighter doesn't expand bounds)
        assert_eq!(
            w1, w2,
            "rasterize width should be identical with/without highlighter"
        );
        assert_eq!(
            h1, h2,
            "rasterize height should be identical with/without highlighter"
        );

        // Pixel data must be identical
        assert_eq!(
            gray1, gray2,
            "rasterized pixels should be identical with/without highlighter"
        );
    }
}
