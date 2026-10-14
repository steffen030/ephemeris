//! Geometric classification of "mark strokes" (ephemeris-fmt.2).
//!
//! Underlines, bullet dots / dashes and checkbox squares are drawn with the
//! pen, so they would otherwise reach HTR as junk glyphs. This module splits a
//! page's pen strokes into **text strokes** (what HTR should read) and
//! **mark strokes** tagged with a [`MarkKind`] (what layout detection, bead
//! ephemeris-fmt.5, consumes).
//!
//! # Page-level, not per-line
//!
//! Classification runs on the whole page *before* line grouping. A mark is
//! defined relative to a single page-wide text height `H` (median of the
//! non-tiny stroke heights), and several tests need neighbours that line
//! grouping would only produce *after* the marks are gone (an underline is
//! swept into the line above; a bullet dot would otherwise be attached to the
//! line as an i-dot). "Leftmost on its line" is therefore decided with a
//! local vertical band around the candidate instead of a first grouping pass.
//! The trade-off: a page mixing very different handwriting sizes shares one
//! `H`; that only makes the classifier miss marks (it is conservative), it
//! does not delete letters.
//!
//! # How the next stages obtain the marks
//!
//! Use [`split_page_ink`] (text lines + marks in one call) or
//! [`crate::transcriber::CascadingTranscriber::transcribe_page_with_marks`]
//! (spans + marks). `StrokeLine`s from `split_page_ink` are exactly the lines
//! HTR sees and the recognition cache hashes.
//!
//! # Heuristics (all relative to `H`)
//!
//! * [`MarkKind::Underline`]: one stroke, bbox `h/w < 0.12`, `w >= 1.5H`,
//!   at least half of its x-extent spanned by the text strokes just above it
//!   (centre above, bottom within `1.5H`), whose
//!   median bottom is `0.12H..=0.6H` above the underline (a baseline cursive
//!   connector sits at ~0 and stays text).
//! * [`MarkKind::BulletDot`]: bbox <= `0.25H` on both axes, nothing to its
//!   left on the line, and the nearest stroke to its right is >= `0.4H` away
//!   (an i-dot is directly above its stem, gap 0).
//! * [`MarkKind::BulletDash`]: `0.25H < w <= 0.8H`, `h/w < 0.25`, leftmost,
//!   at least `0.4H` gap, vertically in the middle of the following text (not under
//!   it). A '-' inside a word has left neighbours and stays text.
//! * [`MarkKind::Checkbox`]: single closed stroke (endpoints within `0.2H`),
//!   bbox `0.6H..=1.2H` and roughly square, with >= 3 sharp bbox corners (a
//!   round 'o' has none), leftmost, >= `0.4H` gap. Multi-stroke checkboxes
//!   are not recognised. Strokes inside the box (tolerance `0.3H`) are
//!   ignored for the gap test and become [`MarkKind::CheckMark`].
//! * [`MarkKind::CheckMark`]: the tick / cross inside a checkbox, so a checked
//!   box is still detected and the tick never reaches HTR.

use crate::model::{Stroke, Tool};
use crate::stroke_htr::{
    default_line_gap, group_strokes_into_lines_with_ruling, is_text_ink, stroke_bounds, StrokeLine,
};

/// Kind of a classified mark stroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MarkKind {
    Underline,
    BulletDot,
    BulletDash,
    Checkbox,
    /// Pen stroke inside a [`MarkKind::Checkbox`] (the box is checked).
    CheckMark,
}

/// A classified mark stroke.
pub type MarkStroke = (MarkKind, Stroke);

/// Result of [`split_page_ink`]: text lines for HTR plus the marks.
#[derive(Debug, Clone, Default)]
pub struct PageInk {
    /// Lines built from text strokes only (shared with the recognition cache).
    pub lines: Vec<StrokeLine>,
    /// Classified mark strokes, in input order.
    pub marks: Vec<MarkStroke>,
    /// Highlighter strokes (never text, never marks), in input order.
    pub highlights: Vec<Stroke>,
}

/// Classify marks, then group the remaining text strokes into lines.
///
/// This is the single entry point shared by `transcribe_page` and the
/// recognition cache so both always agree on line stroke sets.
pub fn split_page_ink(strokes: &[Stroke], ruling_pitch: Option<f32>) -> PageInk {
    let (text, marks) = classify_mark_strokes(strokes, 0.0);
    let gap = default_line_gap(&text);
    let lines = group_strokes_into_lines_with_ruling(&text, gap, ruling_pitch);
    let highlights = strokes
        .iter()
        .filter(|s| s.tool == Tool::Highlighter && !s.points.is_empty())
        .cloned()
        .collect();
    PageInk {
        lines,
        marks,
        highlights,
    }
}

/// Median height of the non-tiny pen strokes (i-dots, bullets, bars excluded).
pub fn estimate_text_height(strokes: &[Stroke]) -> f32 {
    let mut hs: Vec<f32> = strokes
        .iter()
        .filter(|s| is_text_ink(s) && s.points.len() >= 2)
        .filter_map(stroke_bounds)
        .map(|(_, y0, _, y1)| y1 - y0)
        .filter(|h| *h > 0.0)
        .collect();
    if hs.is_empty() {
        return 0.0;
    }
    hs.sort_by(|a, b| a.total_cmp(b));
    let p75 = hs[(hs.len() * 3) / 4];
    let body: Vec<f32> = hs.into_iter().filter(|h| *h >= 0.4 * p75).collect();
    body[body.len() / 2]
}

#[derive(Clone, Copy)]
struct Bb {
    min_x: f32,
    min_y: f32,
    max_x: f32,
    max_y: f32,
}

impl Bb {
    fn w(&self) -> f32 {
        self.max_x - self.min_x
    }
    fn h(&self) -> f32 {
        self.max_y - self.min_y
    }
    fn cy(&self) -> f32 {
        (self.min_y + self.max_y) * 0.5
    }
}

fn bb(s: &Stroke) -> Option<Bb> {
    stroke_bounds(s).map(|(min_x, min_y, max_x, max_y)| Bb {
        min_x,
        min_y,
        max_x,
        max_y,
    })
}

/// Split `strokes` into `(text, marks)`.
///
/// `line_height_hint` is the median text height `H`; pass `0.0` (or a
/// non-finite / non-positive value) to estimate it with
/// [`estimate_text_height`]. `text` keeps every non-mark stroke in input order
/// (including non-pen strokes, which downstream grouping ignores); `marks` are
/// classified pen strokes. Conservative by design: ambiguous strokes stay text.
pub fn classify_mark_strokes(
    strokes: &[Stroke],
    line_height_hint: f32,
) -> (Vec<Stroke>, Vec<MarkStroke>) {
    let h = if line_height_hint.is_finite() && line_height_hint > 0.0 {
        line_height_hint
    } else {
        estimate_text_height(strokes)
    };
    let boxes: Vec<Option<Bb>> = strokes
        .iter()
        .map(|s| if is_text_ink(s) { bb(s) } else { None })
        .collect();
    let mut kinds: Vec<Option<MarkKind>> = vec![None; strokes.len()];
    if h > 0.0 {
        // Pass 1: underlines (so they do not act as neighbours in pass 2).
        for i in 0..strokes.len() {
            if let Some(b) = boxes[i] {
                if is_underline(i, &b, &boxes, h) {
                    kinds[i] = Some(MarkKind::Underline);
                }
            }
        }
        let ctx: Vec<Option<Bb>> = boxes
            .iter()
            .zip(&kinds)
            .map(|(b, k)| if k.is_some() { None } else { *b })
            .collect();
        for i in 0..strokes.len() {
            if kinds[i].is_some() {
                continue;
            }
            if let Some(b) = ctx[i] {
                kinds[i] = classify_leading(i, &strokes[i], &b, &ctx, h);
            }
        }
        // Pass 3: ticks / crosses inside a checkbox.
        for i in 0..strokes.len() {
            let (Some(MarkKind::Checkbox), Some(b)) = (kinds[i], ctx[i]) else {
                continue;
            };
            for j in 0..strokes.len() {
                if kinds[j].is_none() && ctx[j].is_some_and(|o| inside_box(&o, &b, h)) {
                    kinds[j] = Some(MarkKind::CheckMark);
                }
            }
        }
    }
    let mut text = Vec::with_capacity(strokes.len());
    let mut marks = Vec::new();
    for (s, k) in strokes.iter().zip(kinds) {
        match k {
            Some(k) => marks.push((k, s.clone())),
            None => text.push(s.clone()),
        }
    }
    (text, marks)
}

/// `o` lies within checkbox `b` (with `0.3H` slack for ticks poking out).
fn inside_box(o: &Bb, b: &Bb, h: f32) -> bool {
    let t = 0.3 * h;
    o.min_x >= b.min_x - t
        && o.max_x <= b.max_x + t
        && o.min_y >= b.min_y - t
        && o.max_y <= b.max_y + t
}

fn x_overlap(a: &Bb, b: &Bb) -> f32 {
    (a.max_x.min(b.max_x) - a.min_x.max(b.min_x)).max(0.0)
}

fn is_underline(i: usize, b: &Bb, boxes: &[Option<Bb>], h: f32) -> bool {
    let w = b.w();
    if w < 1.5 * h || b.h() / w >= 0.12 {
        return false;
    }
    let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
    let mut bottoms = Vec::new();
    for (j, o) in boxes.iter().enumerate() {
        let Some(o) = o else { continue };
        if j == i || o.h() < 0.3 * h || (o.w() > 1.5 * h && o.h() / o.w() < 0.12) {
            continue;
        }
        // Only the text just above counts; other lines in the same column
        // (above or below) must not drag the median bottom around.
        if o.cy() >= b.cy() || o.max_y < b.cy() - 1.5 * h {
            continue;
        }
        if x_overlap(b, o) > 0.0 {
            lo = lo.min(o.min_x.max(b.min_x));
            hi = hi.max(o.max_x.min(b.max_x));
            bottoms.push(o.max_y);
        }
    }
    if bottoms.is_empty() || hi - lo < 0.5 * w {
        return false;
    }
    bottoms.sort_by(|a, b| a.total_cmp(b));
    let gap = b.cy() - bottoms[bottoms.len() / 2];
    (0.12 * h..=0.6 * h).contains(&gap)
}

/// Bullet dot / dash / checkbox: all must be leftmost with a gap after them.
fn classify_leading(i: usize, s: &Stroke, b: &Bb, ctx: &[Option<Bb>], h: f32) -> Option<MarkKind> {
    let (w, ht) = (b.w(), b.h());
    let kind = if w <= 0.25 * h && ht <= 0.25 * h {
        MarkKind::BulletDot
    } else if w > 0.25 * h && w <= 0.8 * h && ht / w < 0.25 {
        MarkKind::BulletDash
    } else if (0.6 * h..=1.2 * h).contains(&w)
        && (0.6 * h..=1.2 * h).contains(&ht)
        && (0.7..=1.4).contains(&(w / ht))
        && is_boxy_closed(s, b, h)
    {
        MarkKind::Checkbox
    } else {
        return None;
    };
    let cy = b.cy();
    let mut follower_gap = f32::INFINITY;
    let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
    for (j, o) in ctx.iter().enumerate() {
        let Some(o) = o else { continue };
        if j == i || (kind == MarkKind::Checkbox && inside_box(o, b, h)) {
            continue;
        }
        // Anything left of the candidate on its line: not a line-leading mark.
        if o.max_x <= b.min_x && o.min_y <= cy + 0.5 * h && o.max_y >= cy - 0.5 * h {
            return None;
        }
        if o.max_x > b.min_x && o.min_y <= cy + h && o.max_y >= cy - h {
            follower_gap = follower_gap.min((o.min_x - b.max_x).max(0.0));
        }
        if o.min_x > b.max_x
            && o.min_x - b.max_x <= 3.0 * h
            && o.min_y <= cy + 0.5 * h
            && o.max_y >= cy - 0.5 * h
        {
            lo = lo.min(o.min_y);
            hi = hi.max(o.max_y);
        }
    }
    // A real bullet is followed by text on its line, clearly separated.
    if !follower_gap.is_finite() || follower_gap < 0.4 * h || lo > hi {
        return None;
    }
    if kind == MarkKind::BulletDash {
        let frac = (cy - lo) / (hi - lo).max(1e-3);
        if !(0.2..=0.8).contains(&frac) {
            return None;
        }
    }
    Some(kind)
}

fn is_boxy_closed(s: &Stroke, b: &Bb, h: f32) -> bool {
    let (Some(first), Some(last)) = (s.points.first(), s.points.last()) else {
        return false;
    };
    if s.points.len() < 4 || (first.x - last.x).hypot(first.y - last.y) > 0.2 * h {
        return false;
    }
    let (tx, ty) = (0.1 * b.w(), 0.1 * b.h());
    let corners = [
        (b.min_x, b.min_y),
        (b.max_x, b.min_y),
        (b.min_x, b.max_y),
        (b.max_x, b.max_y),
    ];
    let hit = corners
        .iter()
        .filter(|(cx, cy)| {
            s.points
                .iter()
                .any(|p| (p.x - cx).abs() <= tx && (p.y - cy).abs() <= ty)
        })
        .count();
    hit >= 3
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Color, Point, StrokeId, Tool};

    /// Text height used throughout: letters are 20 units tall.
    const H: f32 = 20.0;

    fn poly(pts: &[(f32, f32)]) -> Stroke {
        Stroke {
            id: StrokeId::new(),
            tool: Tool::Pen,
            color: Color::BLACK,
            base_width: 2.0,
            points: pts
                .iter()
                .enumerate()
                .map(|(i, (x, y))| Point::new(*x, *y, 0.5, 0.0, i as u32 * 10))
                .collect(),
            t0_ms: 0,
        }
    }

    /// Vertical stem 20 tall (a lowercase 'l' / body letter), spanning y 0..20.
    fn stem(x: f32) -> Stroke {
        poly(&[(x, 0.0), (x + 1.0, 10.0), (x, 20.0)])
    }

    /// A word of `n` stems starting at `x0`, 8 apart.
    fn word(x0: f32, n: usize) -> Vec<Stroke> {
        (0..n).map(|i| stem(x0 + 8.0 * i as f32)).collect()
    }

    fn classify(strokes: &[Stroke]) -> (Vec<Stroke>, Vec<(MarkKind, Stroke)>) {
        classify_mark_strokes(strokes, H)
    }

    fn only_mark(strokes: &[Stroke]) -> Option<MarkKind> {
        let (text, marks) = classify(strokes);
        assert_eq!(text.len() + marks.len(), strokes.len());
        marks.first().map(|(k, _)| *k)
    }

    #[test]
    fn underline_below_word() {
        let mut s = word(0.0, 6); // x 0..41
        s.push(poly(&[(0.0, 27.0), (20.0, 27.5), (45.0, 27.0)]));
        let (text, marks) = classify(&s);
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].0, MarkKind::Underline);
        assert_eq!(text.len(), 6);
    }

    #[test]
    fn underline_below_first_of_several_lines() {
        // Lines below in the same column must not hide the underline.
        let mut s = word(0.0, 6);
        s.push(poly(&[(0.0, 27.0), (20.0, 27.5), (45.0, 27.0)]));
        for row in 1..4 {
            s.extend(word(0.0, 6).into_iter().map(|mut st| {
                st.points.iter_mut().for_each(|p| p.y += 40.0 * row as f32);
                st
            }));
        }
        let (_, marks) = classify(&s);
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].0, MarkKind::Underline);
    }

    #[test]
    fn checked_checkbox_keeps_box_and_tick_out_of_text() {
        let sq = poly(&[
            (0.0, 2.0),
            (16.0, 2.0),
            (16.0, 18.0),
            (0.0, 18.0),
            (0.0, 2.5),
        ]);
        let tick = poly(&[(3.0, 10.0), (6.0, 15.0), (13.0, 5.0)]);
        let mut s = vec![sq, tick];
        s.extend(word(30.0, 4));
        let (text, marks) = classify(&s);
        let kinds: Vec<MarkKind> = marks.iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, vec![MarkKind::Checkbox, MarkKind::CheckMark]);
        assert_eq!(text.len(), 4);
    }

    #[test]
    fn bullet_dot_before_text() {
        let mut s = vec![poly(&[(0.0, 10.0), (2.0, 11.0), (3.0, 10.0)])];
        s.extend(word(20.0, 4));
        assert_eq!(only_mark(&s), Some(MarkKind::BulletDot));
    }

    #[test]
    fn bullet_dash_before_text() {
        let mut s = vec![poly(&[(0.0, 10.0), (6.0, 10.5), (12.0, 10.0)])];
        s.extend(word(26.0, 4));
        assert_eq!(only_mark(&s), Some(MarkKind::BulletDash));
    }

    #[test]
    fn checkbox_before_text() {
        // 16x16 square drawn clockwise, closing at the start.
        let sq = poly(&[
            (0.0, 2.0),
            (8.0, 2.0),
            (16.0, 2.0),
            (16.0, 10.0),
            (16.0, 18.0),
            (8.0, 18.0),
            (0.0, 18.0),
            (0.0, 10.0),
            (0.0, 2.5),
        ]);
        let mut s = vec![sq];
        s.extend(word(30.0, 4));
        assert_eq!(only_mark(&s), Some(MarkKind::Checkbox));
    }

    #[test]
    fn round_o_at_line_start_stays_text() {
        let n = 16;
        let pts: Vec<(f32, f32)> = (0..=n)
            .map(|i| {
                let a = i as f32 / n as f32 * std::f32::consts::TAU;
                (8.0 + 8.0 * a.cos(), 10.0 + 8.0 * a.sin())
            })
            .collect();
        let mut s = vec![poly(&pts)];
        s.extend(word(30.0, 4)); // even with a wide gap, no sharp corners
        assert_eq!(only_mark(&s), None);
    }

    #[test]
    fn letter_l_stays_text() {
        let mut s = vec![stem(0.0)];
        s.extend(word(30.0, 3));
        assert_eq!(only_mark(&s), None);
    }

    #[test]
    fn i_dot_stays_text() {
        // 'i' at line start: stem below, dot 6 above its top, followed by text.
        let mut s = vec![
            poly(&[(0.0, 8.0), (1.0, 14.0), (0.0, 20.0)]),
            poly(&[(0.0, 2.0), (1.0, 3.0), (2.0, 2.0)]),
        ];
        s.extend(word(12.0, 4));
        assert_eq!(only_mark(&s), None);
    }

    #[test]
    fn t_bar_stays_text() {
        // 't': stem 0..20 at x=10, bar crossing it near the top.
        let mut s = vec![
            poly(&[(10.0, 0.0), (10.5, 10.0), (10.0, 20.0)]),
            poly(&[(4.0, 6.0), (10.0, 6.3), (16.0, 6.0)]),
        ];
        s.extend(word(24.0, 3));
        assert_eq!(only_mark(&s), None);
    }

    #[test]
    fn dash_inside_word_stays_text() {
        let mut s = word(0.0, 3); // ends x~17
        s.push(poly(&[(20.0, 10.0), (26.0, 10.5), (32.0, 10.0)]));
        s.extend(word(35.0, 3));
        assert_eq!(only_mark(&s), None);
    }

    #[test]
    fn long_cursive_connector_stays_text() {
        // Flat, long, at the baseline of the letters it joins.
        let mut s = word(0.0, 3);
        s.push(poly(&[(0.0, 19.5), (20.0, 20.0), (50.0, 19.5)]));
        s.extend(word(52.0, 3));
        assert_eq!(only_mark(&s), None);
    }

    #[test]
    fn hint_zero_estimates_height_and_highlighter_is_not_a_mark() {
        let mut s = word(0.0, 6);
        s.push(poly(&[(0.0, 27.0), (20.0, 27.5), (45.0, 27.0)]));
        let (_, marks) = classify_mark_strokes(&s, 0.0);
        assert_eq!(marks.len(), 1);
        let mut hl = poly(&[(0.0, 27.0), (20.0, 27.5), (45.0, 27.0)]);
        hl.tool = Tool::Highlighter;
        let mut s = word(0.0, 6);
        s.push(hl);
        let (text, marks) = classify_mark_strokes(&s, 0.0);
        assert!(marks.is_empty());
        assert_eq!(text.len(), 7);
    }

    #[test]
    fn split_page_ink_keeps_marks_out_of_lines() {
        let mut s = vec![poly(&[(0.0, 10.0), (6.0, 10.5), (12.0, 10.0)])];
        s.extend(word(26.0, 4));
        let page = split_page_ink(&s, None);
        assert_eq!(page.marks.len(), 1);
        assert_eq!(page.lines.len(), 1);
        assert_eq!(page.lines[0].strokes.len(), 4);
    }
}
