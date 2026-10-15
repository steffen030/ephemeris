//! Line-level layout detection (ephemeris-fmt.5).
//!
//! Turns recognised lines plus the page's mark and highlighter strokes into a
//! [`LineFormat`] per line: heading level, bullet / numbered / checkbox list
//! item with indent, paragraph breaks and whole-line highlight / underline.
//! Pure geometry + text, so the recognition cache recomputes it on every sync
//! (a new highlight changes the format without re-running HTR).
//!
//! # Heuristics
//!
//! `H` is the lower median of the lines' glyph heights (see
//! [`LayoutLine::glyph_height`]).
//!
//! * Heading: glyph height `>= 2.0H` → H1, `>= 1.5H` → H2. An underline
//!   covering `>= 80%` of a short line (`<= 6` words) at the top of a block
//!   (first line, after a paragraph gap, or after a non-paragraph line) → H2;
//!   the underline is consumed.
//! * Checkbox: a [`MarkKind::Checkbox`] left of the line, checked when a
//!   [`MarkKind::CheckMark`] lies in it.
//! * Numbered: text `^(\d{1,3})[.)]\s+rest` (prefix stripped, number kept).
//! * Bullet: a [`MarkKind::BulletDot`] / [`MarkKind::BulletDash`] left of the
//!   line, or text `^[-•*·]\s*rest` (glyph stripped; `-5` stays a number).
//! * Indent (lists only): per run of consecutive list lines, item start (the
//!   leading mark, else the line) relative to the run's leftmost start, in
//!   steps of `1.5H`, rounded and clamped to `0..=3`.
//! * Paragraph break: centre-to-centre distance to the previous line
//!   `> 1.6×` the median distance (centres are less noisy than bbox gaps,
//!   which swing with descenders).
//! * Whole-line highlight / underline: highlighter or underline strokes cover
//!   `>= 80%` of the line width → mark over the trimmed text (so `==text==`
//!   never has inner spaces).
//! * Word-level highlight / underline (ephemeris-fmt.10): see below.
//!
//! # Word-level marks
//!
//! HTR yields no character positions, so marks are mapped through words.
//! The line's text strokes ([`LayoutLine::stroke_spans`]) are clustered into
//! `k` word boxes by cutting at the `k − 1` largest x-gaps, `k` being the
//! whitespace word count of the detected text (after prefix stripping; if a
//! textual prefix was stripped, `k + 1` boxes with the first dropped is tried
//! first, as the prefix glyph is ink). The clustering is accepted only when the
//! smallest cut gap is `>= 1.5×` the largest uncut gap and `>= 0.25×` the
//! line's glyph height; otherwise the ink and the text disagree (HTR merged
//! or split words, or a user correction changed the word count).
//!
//! With word boxes, a mark selects every word whose box it covers `>= 50%`
//! in x; contiguous selected words merge into one [`InlineMark`] (inner
//! spaces included, surrounding spaces never). Selecting every word is the
//! whole-line case when coverage is `>= 80%` (heading-by-underline applies);
//! a strict subset is always inline, so an underlined word on a short line is
//! never a heading. Without word boxes, coverage `>= 80%` → whole line, else
//! the mark is dropped (logged at debug).
//!
//! # Mark ↔ line association
//!
//! Leading marks (bullets, checkboxes) overlap the line's vertical band and
//! end left of the line start. An underline belongs to the nearest line whose
//! centre is above it and whose bottom is at most `0.8H` above it. A
//! highlighter stroke belongs to the line whose band contains its centre.

use crate::ink_layout::{MarkKind, MarkStroke};
use crate::model::Stroke;
use crate::stroke_htr::stroke_bounds;
use crate::text_format::{BlockKind, InlineKind, InlineMark, LineFormat};

/// Glyph height ratio for H1.
pub const HEADING1_RATIO: f32 = 2.0;
/// Glyph height ratio for H2.
pub const HEADING2_RATIO: f32 = 1.5;
/// Paragraph break when line distance exceeds this × the median distance.
pub const PARA_GAP_RATIO: f32 = 1.6;
/// Fraction of the line width a highlight / underline must cover.
pub const WHOLE_LINE_COVERAGE: f32 = 0.8;
/// Longest line (in words) an underline can turn into an H2.
pub const UNDERLINE_HEADING_MAX_WORDS: usize = 6;
/// Fraction of a word box a mark must cover to select that word.
pub const WORD_COVERAGE: f32 = 0.5;
/// A word-cut gap must be this × the largest gap left uncut.
pub const WORD_GAP_RATIO: f32 = 1.5;
/// A word-cut gap must be at least this × the line's glyph height.
pub const WORD_GAP_MIN: f32 = 0.25;
/// List indent step in units of `H`.
const INDENT_STEP: f32 = 1.5;
const MAX_INDENT: u8 = 3;

/// One line's geometry and text, in reading order.
#[derive(Debug, Clone, Copy, Default)]
pub struct LayoutLine<'a> {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Typical letter height on the line (e.g.
    /// [`crate::ink_layout::estimate_text_height`] of its strokes). Less noisy
    /// than `height`, which depends on ascenders / descenders; `<= 0` falls
    /// back to `height`.
    pub glyph_height: f32,
    /// Recognised text; empty for pending lines (geometry still counts).
    pub text: &'a str,
    /// Interpret and strip textual list prefixes (`- `, `1. `). False for
    /// user-corrected text, which is never rewritten.
    pub strip_prefix: bool,
    /// `(min_x, max_x)` of each text stroke on the line, any order; used to
    /// cluster words for partial marks. Empty when unknown, in which case
    /// only whole-line marks are detected.
    pub stroke_spans: &'a [(f32, f32)],
}

/// Detection result: the text with any list prefix stripped, plus its format.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DetectedLine {
    pub text: String,
    pub format: LineFormat,
}

#[derive(Clone, Copy)]
struct Bb {
    min_x: f32,
    min_y: f32,
    max_x: f32,
    max_y: f32,
}

impl Bb {
    fn of(s: &Stroke) -> Option<Self> {
        stroke_bounds(s).map(|(min_x, min_y, max_x, max_y)| Self {
            min_x,
            min_y,
            max_x,
            max_y,
        })
    }
    fn cx(&self) -> f32 {
        (self.min_x + self.max_x) * 0.5
    }
    fn cy(&self) -> f32 {
        (self.min_y + self.max_y) * 0.5
    }
}

impl LayoutLine<'_> {
    fn cy(&self) -> f32 {
        self.y + self.height * 0.5
    }
    fn bottom(&self) -> f32 {
        self.y + self.height
    }
    fn size(&self) -> f32 {
        if self.glyph_height > 0.0 {
            self.glyph_height
        } else {
            self.height
        }
    }
}

fn lower_median(mut v: Vec<f32>) -> Option<f32> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    Some(v[(v.len() - 1) / 2])
}

/// Detect formats for `lines` (top to bottom). Output is index-aligned.
pub fn detect_line_formats(
    lines: &[LayoutLine],
    marks: &[MarkStroke],
    highlights: &[Stroke],
) -> Vec<DetectedLine> {
    let Some(h) = lower_median(
        lines
            .iter()
            .map(LayoutLine::size)
            .filter(|s| *s > 0.0)
            .collect(),
    ) else {
        return lines
            .iter()
            .map(|l| DetectedLine {
                text: l.text.to_string(),
                format: LineFormat::default(),
            })
            .collect();
    };

    // Associate marks with lines.
    let n = lines.len();
    let mut lead: Vec<Option<(MarkKind, Bb)>> = vec![None; n];
    let mut ticks: Vec<Bb> = Vec::new();
    let mut underlines: Vec<Vec<(f32, f32)>> = vec![Vec::new(); n];
    let mut highlit: Vec<Vec<(f32, f32)>> = vec![Vec::new(); n];
    for (kind, s) in marks {
        let Some(b) = Bb::of(s) else { continue };
        match kind {
            MarkKind::CheckMark => ticks.push(b),
            MarkKind::Underline => {
                if let Some(i) = line_above(lines, &b, h) {
                    underlines[i].push((b.min_x, b.max_x));
                }
            }
            _ => {
                if let Some(i) = line_right_of(lines, &b, h) {
                    // Keep the mark closest to the text.
                    if lead[i].is_none_or(|(_, o)| b.max_x > o.max_x) {
                        lead[i] = Some((*kind, b));
                    }
                }
            }
        }
    }
    for s in highlights {
        let Some(b) = Bb::of(s) else { continue };
        if let Some(i) = line_containing(lines, &b, h) {
            highlit[i].push((b.min_x, b.max_x));
        }
    }

    // Paragraph breaks from centre-to-centre distances.
    let deltas: Vec<f32> = lines.windows(2).map(|w| w[1].cy() - w[0].cy()).collect();
    let median_delta = lower_median(deltas.clone()).unwrap_or(0.0);
    let para_break =
        |i: usize| i > 0 && median_delta > 0.0 && deltas[i - 1] > PARA_GAP_RATIO * median_delta;

    let mut out = Vec::with_capacity(n);
    let mut starts = vec![0.0f32; n];
    let mut prev_block = BlockKind::Paragraph;
    for (i, line) in lines.iter().enumerate() {
        starts[i] = lead[i].map_or(line.x, |(_, b)| b.min_x);
        let mut body = line.text;
        let mut block = BlockKind::Paragraph;
        match lead[i] {
            Some((MarkKind::Checkbox, b)) => {
                let checked = ticks.iter().any(|t| {
                    let m = 0.3 * h;
                    (b.min_x - m..=b.max_x + m).contains(&t.cx())
                        && (b.min_y - m..=b.max_y + m).contains(&t.cy())
                });
                block = BlockKind::Checkbox(checked);
            }
            lead_mark => {
                if let Some((num, rest)) = numbered_prefix(body).filter(|_| line.strip_prefix) {
                    block = BlockKind::Numbered(num);
                    body = rest;
                } else if lead_mark.is_some() {
                    block = BlockKind::Bullet;
                    if line.strip_prefix {
                        body = bullet_prefix(body).unwrap_or(body);
                    }
                } else if let Some(rest) = bullet_prefix(body).filter(|_| line.strip_prefix) {
                    block = BlockKind::Bullet;
                    body = rest;
                }
            }
        }
        if block == BlockKind::Paragraph {
            let ratio = line.size() / h;
            if ratio >= HEADING1_RATIO {
                block = BlockKind::Heading(1);
            } else if ratio >= HEADING2_RATIO {
                block = BlockKind::Heading(2);
            }
        }

        let mut marks = Vec::new();
        let words = if underlines[i].is_empty() && highlit[i].is_empty() {
            None
        } else {
            word_boxes(line, body)
        };
        let top_of_block = i == 0 || para_break(i) || prev_block != BlockKind::Paragraph;
        match mark_scope(
            &underlines[i],
            line,
            words.as_deref(),
            InlineKind::Underline,
        ) {
            MarkScope::WholeLine => {
                if block == BlockKind::Paragraph
                    && top_of_block
                    && body.split_whitespace().count() <= UNDERLINE_HEADING_MAX_WORDS
                {
                    block = BlockKind::Heading(2);
                } else if !matches!(block, BlockKind::Heading(_)) {
                    marks.extend(whole_line_mark(body, InlineKind::Underline));
                }
            }
            MarkScope::Words(m) if !matches!(block, BlockKind::Heading(_)) => marks.extend(m),
            _ => {}
        }
        match mark_scope(&highlit[i], line, words.as_deref(), InlineKind::Highlight) {
            MarkScope::WholeLine => marks.extend(whole_line_mark(body, InlineKind::Highlight)),
            MarkScope::Words(m) => marks.extend(m),
            MarkScope::Drop => {}
        }
        prev_block = if body.trim().is_empty() {
            BlockKind::Paragraph
        } else {
            block
        };
        out.push(DetectedLine {
            text: body.to_string(),
            format: LineFormat {
                block,
                indent: 0,
                marks,
                para_break_before: para_break(i),
            },
        });
    }

    // Indent per run of consecutive list items, relative to its leftmost start.
    let is_list = |d: &DetectedLine| {
        matches!(
            d.format.block,
            BlockKind::Bullet | BlockKind::Numbered(_) | BlockKind::Checkbox(_)
        )
    };
    let mut i = 0;
    while i < n {
        if !is_list(&out[i]) {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < n && is_list(&out[j]) {
            j += 1;
        }
        let base = starts[i..j].iter().copied().fold(f32::INFINITY, f32::min);
        for k in i..j {
            let steps = ((starts[k] - base) / (INDENT_STEP * h)).round();
            out[k].format.indent = steps.clamp(0.0, MAX_INDENT as f32) as u8;
        }
        i = j;
    }
    out
}

/// Line whose band holds the mark's centre and which starts right of it.
fn line_right_of(lines: &[LayoutLine], b: &Bb, h: f32) -> Option<usize> {
    let slack = 0.25 * h;
    lines
        .iter()
        .enumerate()
        .filter(|(_, l)| {
            (l.y - slack..=l.bottom() + slack).contains(&b.cy())
                && b.max_x <= l.x + 0.5 * h
                && l.x - b.max_x <= 4.0 * h
        })
        .min_by(|(_, a), (_, c)| (a.cy() - b.cy()).abs().total_cmp(&(c.cy() - b.cy()).abs()))
        .map(|(i, _)| i)
}

/// Nearest line above an underline (centre above, bottom at most `0.8H` up).
fn line_above(lines: &[LayoutLine], b: &Bb, h: f32) -> Option<usize> {
    lines
        .iter()
        .enumerate()
        .filter(|(_, l)| {
            l.cy() < b.cy()
                && b.cy() - l.bottom() <= 0.8 * h
                && b.max_x > l.x
                && b.min_x < l.x + l.width
        })
        .max_by(|(_, a), (_, c)| a.cy().total_cmp(&c.cy()))
        .map(|(i, _)| i)
}

/// Line whose band (with `0.25H` slack) contains a highlighter stroke's centre.
fn line_containing(lines: &[LayoutLine], b: &Bb, h: f32) -> Option<usize> {
    let slack = 0.25 * h;
    lines
        .iter()
        .enumerate()
        .filter(|(_, l)| {
            (l.y - slack..=l.bottom() + slack).contains(&b.cy())
                && b.max_x > l.x
                && b.min_x < l.x + l.width
        })
        .min_by(|(_, a), (_, c)| (a.cy() - b.cy()).abs().total_cmp(&(c.cy() - b.cy()).abs()))
        .map(|(i, _)| i)
}

/// Fraction of the line's x-extent covered by the union of `spans`.
fn coverage(spans: &[(f32, f32)], line: &LayoutLine) -> f32 {
    if line.width <= 0.0 {
        return 0.0;
    }
    coverage_of(spans, line.x, line.x + line.width)
}

/// One word: its ink x-range and its char range in the detected text.
#[derive(Debug, Clone, Copy, PartialEq)]
struct WordBox {
    min_x: f32,
    max_x: f32,
    start: usize,
    end: usize,
}

/// How a line's highlight / underline strokes apply to its text.
#[derive(Debug, PartialEq)]
enum MarkScope {
    Drop,
    WholeLine,
    Words(Vec<InlineMark>),
}

/// Whole line, selected words, or nothing (see the module docs).
fn mark_scope(
    spans: &[(f32, f32)],
    line: &LayoutLine,
    words: Option<&[WordBox]>,
    kind: InlineKind,
) -> MarkScope {
    if spans.is_empty() {
        return MarkScope::Drop;
    }
    let whole = coverage(spans, line) >= WHOLE_LINE_COVERAGE;
    let Some(words) = words else {
        if whole {
            return MarkScope::WholeLine;
        }
        tracing::debug!(
            "layout: dropping partial {kind:?} on {:?}: words do not match ink",
            line.text
        );
        return MarkScope::Drop;
    };
    let selected: Vec<bool> = words
        .iter()
        .map(|w| coverage_of(spans, w.min_x, w.max_x) >= WORD_COVERAGE)
        .collect();
    if whole && selected.iter().all(|s| *s) {
        return MarkScope::WholeLine;
    }
    // Contiguous selected words merge (inner spaces included).
    let mut marks: Vec<InlineMark> = Vec::new();
    for (j, w) in words.iter().enumerate().filter(|(j, _)| selected[*j]) {
        match marks.last_mut() {
            Some(m) if j > 0 && selected[j - 1] => m.end = w.end,
            _ => marks.push(InlineMark {
                kind,
                start: w.start,
                end: w.end,
            }),
        }
    }
    if marks.is_empty() {
        MarkScope::Drop
    } else {
        MarkScope::Words(marks)
    }
}

/// Cluster the line's text strokes into one box per word of `body`, or
/// `None` when the ink's gaps don't clearly split into that many words.
fn word_boxes(line: &LayoutLine, body: &str) -> Option<Vec<WordBox>> {
    let tokens = word_char_ranges(body);
    if tokens.is_empty() || line.stroke_spans.is_empty() {
        return None;
    }
    let min_gap = WORD_GAP_MIN * line.size();
    let k = tokens.len();
    // A stripped textual prefix (`-`, `1.`) is still ink on the left: try one
    // extra leading cluster first, then the plain word count.
    let with_prefix = (body.len() != line.text.len())
        .then(|| cluster_spans(line.stroke_spans, k + 1, min_gap))
        .flatten()
        .map(|mut b| {
            b.remove(0);
            b
        });
    let boxes = with_prefix.or_else(|| cluster_spans(line.stroke_spans, k, min_gap))?;
    Some(
        boxes
            .into_iter()
            .zip(tokens)
            .map(|((min_x, max_x), (start, end))| WordBox {
                min_x,
                max_x,
                start,
                end,
            })
            .collect(),
    )
}

/// Char ranges of the whitespace-separated words of `text`.
fn word_char_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = None;
    let mut n = 0;
    for (i, c) in text.chars().enumerate() {
        match (c.is_whitespace(), start) {
            (true, Some(s)) => {
                out.push((s, i));
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
        n = i + 1;
    }
    if let Some(s) = start {
        out.push((s, n));
    }
    out
}

/// Split stroke x-spans into `k` clusters at the `k − 1` largest gaps.
/// Accepted only if every cut gap is `>= min_gap` and `>= WORD_GAP_RATIO` ×
/// the largest uncut gap; returns the clusters' x-ranges left to right.
fn cluster_spans(spans: &[(f32, f32)], k: usize, min_gap: f32) -> Option<Vec<(f32, f32)>> {
    if k == 0 || spans.len() < k {
        return None;
    }
    let mut sorted = spans.to_vec();
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0));
    // Gap before stroke i+1: its left edge minus the furthest right edge so
    // far (overlapping strokes give negative gaps, never cut).
    let mut reach = sorted[0].1;
    let mut gaps: Vec<(f32, usize)> = Vec::with_capacity(sorted.len() - 1);
    for (i, s) in sorted.iter().enumerate().skip(1) {
        gaps.push((s.0 - reach, i));
        reach = reach.max(s.1);
    }
    gaps.sort_by(|a, b| b.0.total_cmp(&a.0));
    let (cut, kept) = gaps.split_at(k - 1);
    if let Some(&(smallest_cut, _)) = cut.last() {
        let largest_kept = kept.first().map_or(f32::NEG_INFINITY, |g| g.0);
        if smallest_cut < min_gap || smallest_cut < WORD_GAP_RATIO * largest_kept {
            return None;
        }
    }
    let mut cuts: Vec<usize> = cut.iter().map(|g| g.1).collect();
    cuts.sort_unstable();
    cuts.push(sorted.len());
    let mut out = Vec::with_capacity(k);
    let mut from = 0;
    for to in cuts {
        let part = &sorted[from..to];
        let lo = part[0].0;
        let hi = part.iter().map(|s| s.1).fold(f32::NEG_INFINITY, f32::max);
        out.push((lo, hi));
        from = to;
    }
    Some(out)
}

/// Fraction of `lo..hi` covered by the union of `spans`. A degenerate
/// range (a dot-sized word) counts as covered when a span contains it.
fn coverage_of(spans: &[(f32, f32)], lo: f32, hi: f32) -> f32 {
    if spans.is_empty() {
        return 0.0;
    }
    if hi - lo <= f32::EPSILON {
        return if spans.iter().any(|(a, b)| *a <= lo && hi <= *b) {
            1.0
        } else {
            0.0
        };
    }
    let mut clipped: Vec<(f32, f32)> = spans
        .iter()
        .map(|(a, b)| (a.max(lo), b.min(hi)))
        .filter(|(a, b)| a < b)
        .collect();
    clipped.sort_by(|a, b| a.0.total_cmp(&b.0));
    let (mut covered, mut end) = (0.0, lo);
    for (a, b) in clipped {
        let a = a.max(end);
        if b > a {
            covered += b - a;
            end = b;
        }
    }
    covered / (hi - lo)
}

/// Mark spanning the trimmed text (char offsets), if any.
fn whole_line_mark(text: &str, kind: InlineKind) -> Option<InlineMark> {
    let lead = text.chars().take_while(|c| c.is_whitespace()).count();
    let trail = text.chars().rev().take_while(|c| c.is_whitespace()).count();
    let len = text.chars().count();
    (lead + trail < len).then_some(InlineMark {
        kind,
        start: lead,
        end: len - trail,
    })
}

/// `^(\d{1,3})[.)]\s+rest` → `(number, rest)`.
fn numbered_prefix(text: &str) -> Option<(u32, &str)> {
    let t = text.trim_start();
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    if !(1..=3).contains(&digits) {
        return None;
    }
    let after = &t[digits..];
    let after = after
        .strip_prefix('.')
        .or_else(|| after.strip_prefix(')'))?;
    if !after.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = after.trim_start();
    if rest.is_empty() {
        return None;
    }
    Some((t[..digits].parse().ok()?, rest))
}

/// `^[-•*·]\s*rest` → `rest`. A `-` directly before a digit is a sign.
fn bullet_prefix(text: &str) -> Option<&str> {
    let t = text.trim_start();
    let c = t.chars().next()?;
    if !matches!(c, '-' | '•' | '*' | '·') {
        return None;
    }
    let after = &t[c.len_utf8()..];
    if c == '-' && after.starts_with(|d: char| d.is_ascii_digit()) {
        return None;
    }
    let rest = after.trim_start();
    (!rest.is_empty()).then_some(rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ink_layout::split_page_ink;
    use crate::model::{Color, Point, StrokeId, Tool};
    use crate::text_format::lines_to_markdown;

    /// Body letter height used throughout.
    const H: f32 = 20.0;
    /// Baseline-to-baseline line pitch.
    const PITCH: f32 = 40.0;

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

    /// `n` stems of height `size` with top at `top`, starting at `x0`.
    fn word_sized(x0: f32, top: f32, n: usize, size: f32) -> Vec<Stroke> {
        let step = 0.4 * size;
        (0..n)
            .map(|i| {
                let x = x0 + step * i as f32;
                poly(&[(x, top), (x + 1.0, top + size * 0.5), (x, top + size)])
            })
            .collect()
    }

    /// A body-sized text line of `words` words (4 stems each) at row `row`.
    fn text_line(x0: f32, row: f32, words: usize) -> Vec<Stroke> {
        let top = row * PITCH;
        (0..words)
            .flat_map(|w| word_sized(x0 + w as f32 * 45.0, top, 4, H))
            .collect()
    }

    fn dot(x: f32, row: f32) -> Stroke {
        let y = row * PITCH + 10.0;
        poly(&[(x, y), (x + 2.0, y + 1.0), (x + 3.0, y)])
    }

    fn square(x: f32, row: f32) -> Stroke {
        let y = row * PITCH + 2.0;
        poly(&[
            (x, y),
            (x + 8.0, y),
            (x + 16.0, y),
            (x + 16.0, y + 8.0),
            (x + 16.0, y + 16.0),
            (x + 8.0, y + 16.0),
            (x, y + 16.0),
            (x, y + 8.0),
            (x, y + 0.5),
        ])
    }

    fn tick(x: f32, row: f32) -> Stroke {
        let y = row * PITCH + 2.0;
        poly(&[(x + 3.0, y + 8.0), (x + 6.0, y + 13.0), (x + 13.0, y + 3.0)])
    }

    /// Horizontal stroke just under row `row`'s baseline from `x0` to `x1`.
    fn underline(x0: f32, x1: f32, row: f32) -> Stroke {
        let y = row * PITCH + H + 6.0;
        poly(&[(x0, y), ((x0 + x1) / 2.0, y + 0.5), (x1, y)])
    }

    fn highlight(x0: f32, x1: f32, row: f32) -> Stroke {
        let y = row * PITCH + H / 2.0;
        let mut s = poly(&[(x0, y), ((x0 + x1) / 2.0, y + 0.5), (x1, y)]);
        s.tool = Tool::Highlighter;
        s
    }

    /// Strokes → lines (as the cache sees them) → detection, with fixed texts.
    fn detect(strokes: &[Stroke], texts: &[&str]) -> Vec<DetectedLine> {
        let ink = split_page_ink(strokes, None);
        assert_eq!(ink.lines.len(), texts.len(), "line grouping");
        let spans: Vec<Vec<(f32, f32)>> = ink
            .lines
            .iter()
            .map(|l| {
                l.strokes
                    .iter()
                    .filter_map(stroke_bounds)
                    .map(|(x0, _, x1, _)| (x0, x1))
                    .collect()
            })
            .collect();
        let lines: Vec<LayoutLine> = ink
            .lines
            .iter()
            .zip(texts)
            .zip(&spans)
            .map(|((l, t), sp)| LayoutLine {
                x: l.x,
                y: l.y,
                width: l.width,
                height: l.height,
                glyph_height: crate::ink_layout::estimate_text_height(&l.strokes),
                text: t,
                strip_prefix: true,
                stroke_spans: sp,
            })
            .collect();
        detect_line_formats(&lines, &ink.marks, &ink.highlights)
    }

    fn blocks(d: &[DetectedLine]) -> Vec<BlockKind> {
        d.iter().map(|l| l.format.block).collect()
    }

    fn markdown(d: &[DetectedLine]) -> String {
        let v: Vec<(String, LineFormat)> = d
            .iter()
            .map(|l| (l.text.clone(), l.format.clone()))
            .collect();
        lines_to_markdown(&v)
    }

    #[test]
    fn same_size_lines_are_plain_paragraphs() {
        let mut s = Vec::new();
        for row in 0..5 {
            s.extend(text_line(0.0, row as f32, 5));
        }
        let d = detect(&s, &["a b", "c d", "e f", "g h", "i j"]);
        for l in &d {
            assert_eq!(l.format, LineFormat::default(), "{l:?}");
        }
    }

    #[test]
    fn h1_by_height() {
        let mut s = word_sized(0.0, 0.0, 6, 2.2 * H);
        for row in 2..5 {
            s.extend(text_line(0.0, row as f32, 4));
        }
        let d = detect(&s, &["Title", "a", "b", "c"]);
        assert_eq!(d[0].format.block, BlockKind::Heading(1));
        assert!(d[1..]
            .iter()
            .all(|l| l.format.block == BlockKind::Paragraph));
    }

    #[test]
    fn h2_by_height() {
        let mut s = word_sized(0.0, 0.0, 6, 1.6 * H);
        for row in 1..4 {
            s.extend(text_line(0.0, row as f32 + 0.3, 4));
        }
        let d = detect(&s, &["Sub", "a", "b", "c"]);
        assert_eq!(d[0].format.block, BlockKind::Heading(2));
    }

    #[test]
    fn h2_by_underline_on_short_line() {
        let mut s = text_line(0.0, 0.0, 2); // x 0..~57
        s.push(underline(-2.0, 60.0, 0.0));
        for row in 1..4 {
            s.extend(text_line(0.0, row as f32, 5));
        }
        let d = detect(&s, &["My topic", "one two", "three", "four"]);
        assert_eq!(d[0].format.block, BlockKind::Heading(2));
        assert!(d[0].format.marks.is_empty());
        assert!(markdown(&d).starts_with("## My topic\n\none two"));
    }

    #[test]
    fn whole_line_underline_on_long_line_is_not_a_heading() {
        let mut s = text_line(0.0, 0.0, 8); // x 0..~332
        s.push(underline(0.0, 330.0, 0.0));
        s.extend(text_line(0.0, 1.0, 8));
        let text = " one two three four five six seven eight ";
        let d = detect(&s, &[text, "next"]);
        assert_eq!(d[0].format.block, BlockKind::Paragraph);
        assert_eq!(
            d[0].format.marks,
            vec![InlineMark {
                kind: InlineKind::Underline,
                start: 1,
                end: text.chars().count() - 1
            }]
        );
    }

    #[test]
    fn whole_line_highlight_snaps_to_text() {
        let mut s = text_line(0.0, 0.0, 3); // x 0..~102
        s.push(highlight(-4.0, 105.0, 0.0));
        s.extend(text_line(0.0, 1.0, 3));
        let d = detect(&s, &["remember  this ", "plain"]);
        assert_eq!(markdown(&d), "==remember  this== \nplain");
    }

    // Word-level marks (ephemeris-fmt.10). `text_line(0, row, 4)` puts word
    // boxes at x 0..25, 45..70, 90..115, 135..160 (gaps 20, in-word gaps 7).

    #[test]
    fn highlight_over_second_of_four_words() {
        let mut s = text_line(0.0, 0.0, 4);
        s.push(highlight(43.0, 72.0, 0.0));
        let d = detect(&s, &["a b c d"]);
        assert_eq!(markdown(&d), "a ==b== c d");
    }

    #[test]
    fn underline_over_last_two_words_is_inline_not_heading() {
        let mut s = text_line(0.0, 0.0, 4);
        s.push(underline(88.0, 162.0, 0.0));
        s.extend(text_line(0.0, 1.0, 4));
        let d = detect(&s, &["a b c d", "next"]);
        assert_eq!(d[0].format.block, BlockKind::Paragraph);
        assert_eq!(
            markdown(&d),
            "a b <u>c d</u>
next"
        );
    }

    #[test]
    fn near_full_underline_over_a_word_subset_is_not_a_heading() {
        // Covers 95 of 115 (> 80%) but only 20% of the last word.
        let mut s = text_line(0.0, 0.0, 3);
        s.push(underline(0.0, 95.0, 0.0));
        s.extend(text_line(0.0, 1.0, 4));
        let d = detect(&s, &["my short topic", "body"]);
        assert_eq!(d[0].format.block, BlockKind::Paragraph);
        assert_eq!(
            markdown(&d),
            "<u>my short</u> topic
body"
        );
    }

    #[test]
    fn word_count_mismatch_drops_partial_mark() {
        // Four ink words, three recognised: gaps don't separate cleanly.
        let mut s = text_line(0.0, 0.0, 4);
        s.push(highlight(43.0, 72.0, 0.0));
        let d = detect(&s, &["one two three"]);
        assert!(d[0].format.marks.is_empty());
    }

    #[test]
    fn word_count_mismatch_keeps_near_full_mark_as_whole_line() {
        let mut s = text_line(0.0, 0.0, 4);
        s.push(highlight(-2.0, 158.0, 0.0));
        let d = detect(&s, &["one two three"]);
        assert_eq!(markdown(&d), "==one two three==");
    }

    #[test]
    fn umlaut_words_use_char_offsets() {
        let mut s = text_line(0.0, 0.0, 4);
        s.push(highlight(88.0, 117.0, 0.0));
        let d = detect(&s, &["Bär über Öl Fuß"]);
        assert_eq!(
            d[0].format.marks,
            vec![InlineMark {
                kind: InlineKind::Highlight,
                start: 9,
                end: 11
            }]
        );
        assert_eq!(markdown(&d), "Bär über ==Öl== Fuß");
    }

    #[test]
    fn two_separate_highlighted_words_give_two_marks() {
        let mut s = text_line(0.0, 0.0, 4);
        s.push(highlight(-2.0, 27.0, 0.0));
        s.push(highlight(88.0, 117.0, 0.0));
        let d = detect(&s, &["a b c d"]);
        assert_eq!(d[0].format.marks.len(), 2);
        assert_eq!(markdown(&d), "==a== b ==c== d");
    }

    #[test]
    fn stripped_prefix_ink_is_skipped_when_mapping_words() {
        // Ink cluster 1 is the textual "1." prefix.
        let mut s = text_line(0.0, 0.0, 3);
        s.push(highlight(88.0, 117.0, 0.0));
        let d = detect(&s, &["1. alpha beta"]);
        assert_eq!(d[0].format.block, BlockKind::Numbered(1));
        assert_eq!(markdown(&d), "1. alpha ==beta==");
    }

    #[test]
    fn user_text_word_mismatch_falls_back() {
        let spans = [(0.0, 25.0), (45.0, 70.0), (90.0, 115.0)];
        let line = |text| LayoutLine {
            x: 0.0,
            y: 0.0,
            width: 115.0,
            height: H,
            glyph_height: H,
            text,
            strip_prefix: false,
            stroke_spans: &spans,
        };
        let mark = |x0, x1| {
            let mut s = poly(&[(x0, 10.0), (x1, 10.0)]);
            s.tool = Tool::Highlighter;
            s
        };
        let partial = [mark(43.0, 72.0)];
        let d = detect_line_formats(&[line("x y z")], &[], &partial);
        assert_eq!(d[0].format.marks.len(), 1);
        let d = detect_line_formats(&[line("xyz")], &[], &partial);
        assert!(d[0].format.marks.is_empty());
        let d = detect_line_formats(&[line("xyz")], &[], &[mark(0.0, 110.0)]);
        assert_eq!(d[0].format.marks.len(), 1);
    }

    #[test]
    fn cluster_spans_rules() {
        let spans = [(0.0, 5.0), (7.0, 12.0), (30.0, 35.0), (4.0, 6.0)];
        assert_eq!(
            cluster_spans(&spans, 2, 5.0),
            Some(vec![(0.0, 12.0), (30.0, 35.0)])
        );
        assert_eq!(cluster_spans(&spans, 1, 5.0), Some(vec![(0.0, 35.0)]));
        // Cut gap 18 is not >= 1.5× the next gap when that is 15.
        let close = [(0.0, 5.0), (20.0, 25.0), (43.0, 48.0)];
        assert_eq!(cluster_spans(&close, 2, 5.0), None);
        // Too few strokes or below the absolute minimum gap.
        assert_eq!(cluster_spans(&spans, 5, 5.0), None);
        assert_eq!(cluster_spans(&spans, 2, 20.0), None);
        assert_eq!(word_char_ranges(" ab  ü c "), vec![(1, 3), (5, 6), (7, 8)]);
    }

    #[test]
    fn bullet_list_with_two_indent_levels() {
        let mut s = Vec::new();
        // Level 0: dot at x 0, text at 20. Level 1: dot at 30, text at 50.
        for (row, x) in [(0.0, 0.0), (1.0, 30.0), (2.0, 30.0), (3.0, 0.0)] {
            s.push(dot(x, row));
            s.extend(text_line(x + 20.0, row, 2));
        }
        let d = detect(&s, &["alpha", "beta", "gamma", "delta"]);
        assert!(d.iter().all(|l| l.format.block == BlockKind::Bullet));
        let indents: Vec<u8> = d.iter().map(|l| l.format.indent).collect();
        assert_eq!(indents, vec![0, 1, 1, 0]);
        assert_eq!(markdown(&d), "- alpha\n  - beta\n  - gamma\n- delta");
    }

    #[test]
    fn textual_bullets_are_stripped() {
        let mut s = Vec::new();
        for row in 0..3 {
            s.extend(text_line(0.0, row as f32, 3));
        }
        let d = detect(&s, &["- milk", "•eggs", "-5 degrees"]);
        assert_eq!(
            blocks(&d),
            vec![BlockKind::Bullet, BlockKind::Bullet, BlockKind::Paragraph]
        );
        assert_eq!(d[0].text, "milk");
        assert_eq!(d[1].text, "eggs");
        assert_eq!(d[2].text, "-5 degrees");
    }

    #[test]
    fn numbered_list_keeps_number_and_strips_prefix() {
        let mut s = Vec::new();
        for row in 0..3 {
            s.extend(text_line(0.0, row as f32, 3));
        }
        let d = detect(&s, &["1. first", "2) second", "10 apples"]);
        assert_eq!(
            blocks(&d),
            vec![
                BlockKind::Numbered(1),
                BlockKind::Numbered(2),
                BlockKind::Paragraph
            ]
        );
        assert_eq!(d[0].text, "first");
        assert_eq!(d[1].text, "second");
        assert_eq!(d[2].text, "10 apples");
    }

    #[test]
    fn checkbox_checked_and_unchecked() {
        let mut s = vec![square(0.0, 0.0), tick(0.0, 0.0)];
        s.extend(text_line(30.0, 0.0, 2));
        s.push(square(0.0, 1.0));
        s.extend(text_line(30.0, 1.0, 2));
        let d = detect(&s, &["done", "todo"]);
        assert_eq!(
            blocks(&d),
            vec![BlockKind::Checkbox(true), BlockKind::Checkbox(false)]
        );
        assert_eq!(markdown(&d), "- [x] done\n- [ ] todo");
    }

    #[test]
    fn paragraph_gap_sets_break() {
        let mut s = Vec::new();
        for row in [0.0, 1.0, 2.0, 4.0, 5.0] {
            s.extend(text_line(0.0, row, 4));
        }
        let d = detect(&s, &["a", "b", "c", "d", "e"]);
        let breaks: Vec<bool> = d.iter().map(|l| l.format.para_break_before).collect();
        assert_eq!(breaks, vec![false, false, false, true, false]);
        assert_eq!(markdown(&d), "a\nb\nc\n\nd\ne");
    }

    #[test]
    fn user_text_is_not_prefix_stripped() {
        let lines = [LayoutLine {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: H,
            glyph_height: H,
            text: "1. kept",
            strip_prefix: false,
            stroke_spans: &[],
        }];
        let d = detect_line_formats(&lines, &[], &[]);
        assert_eq!(d[0].text, "1. kept");
        assert_eq!(d[0].format.block, BlockKind::Paragraph);
    }

    /// Fake recogniser: line `idx` reads `texts[idx]`; counts invocations.
    fn recognise(
        cache: &mut crate::recognition_cache::PageRecognitionCache,
        strokes: &[Stroke],
        texts: &[&str],
    ) -> usize {
        cache
            .recognise_pending(strokes, None, &|| false, |line, idx| {
                Ok(Some(crate::transcriber::TextSpan::new(
                    texts[idx as usize],
                    line.x,
                    line.y,
                    line.width,
                    line.height,
                )))
            })
            .unwrap()
            .recognised
    }

    #[test]
    fn end_to_end_cache_markdown() {
        use crate::recognition_cache::PageRecognitionCache;
        let mut s = text_line(0.0, 0.0, 2);
        s.push(underline(-2.0, 60.0, 0.0));
        for row in [1.0, 2.0] {
            s.push(dot(0.0, row));
            s.extend(text_line(20.0, row, 2));
        }
        s.extend(text_line(0.0, 3.0, 3));
        s.extend(text_line(0.0, 4.0, 3));
        let texts = ["Shopping", "- milk", "eggs", "call mum", "1. later"];
        let mut cache = PageRecognitionCache::new("v");
        cache.sync_strokes(&s, None, "v");
        assert_eq!(recognise(&mut cache, &s, &texts), 5);
        assert_eq!(
            cache.completed_markdown(),
            "## Shopping\n\n- milk\n- eggs\n\ncall mum\n\n1. later"
        );
        // Re-detection is idempotent: stripped prefixes stay stripped.
        cache.sync_strokes(&s, None, "v");
        assert_eq!(cache.lines[4].text, "later");
        assert_eq!(cache.lines[4].raw_text.as_deref(), Some("1. later"));
        assert_eq!(cache.lines[4].format.block, BlockKind::Numbered(1));

        // A highlight changes the format on sync, without re-running HTR.
        s.push(highlight(-4.0, 110.0, 3.0));
        let stats = cache.sync_strokes(&s, None, "v");
        assert_eq!(stats.invalidated, 0);
        assert_eq!(recognise(&mut cache, &s, &texts), 0);
        assert!(cache.completed_markdown().contains("\n\n==call mum==\n\n"));
    }

    #[test]
    fn end_to_end_cache_word_marks() {
        use crate::recognition_cache::PageRecognitionCache;
        let mut s = text_line(0.0, 0.0, 4);
        s.push(highlight(43.0, 72.0, 0.0));
        s.extend(text_line(0.0, 1.0, 4));
        s.push(underline(88.0, 162.0, 1.0));
        s.extend(text_line(0.0, 2.0, 4));
        let texts = ["one two three four", "a b c d", "zu viele wörter hier drin"];
        let mut cache = PageRecognitionCache::new("v");
        cache.sync_strokes(&s, None, "v");
        assert_eq!(recognise(&mut cache, &s, &texts), 3);
        assert_eq!(
            cache.completed_markdown(),
            "one ==two== three four\na b <u>c d</u>\nzu viele wörter hier drin"
        );
        // A partial highlight on the mismatched line is dropped.
        s.push(highlight(43.0, 72.0, 2.0));
        cache.sync_strokes(&s, None, "v");
        assert!(cache.lines[2].format.marks.is_empty());
    }

    #[test]
    fn user_correction_is_never_rewritten() {
        use crate::recognition_cache::PageRecognitionCache;
        let mut s = text_line(0.0, 0.0, 3);
        s.extend(text_line(0.0, 1.0, 3));
        let mut cache = PageRecognitionCache::new("v");
        cache.sync_strokes(&s, None, "v");
        recognise(&mut cache, &s, &["a", "b"]);
        cache.set_line_correction(1, Some("- mine ".into()), None);
        cache.sync_strokes(&s, None, "v");
        assert_eq!(cache.lines[1].effective_text(), "- mine ");
        assert_eq!(cache.lines[1].format.block, BlockKind::Paragraph);
    }

    #[test]
    fn prefix_parsers() {
        assert_eq!(numbered_prefix("  3. x"), Some((3, "x")));
        assert_eq!(numbered_prefix("1234. x"), None);
        assert_eq!(numbered_prefix("1.x"), None);
        assert_eq!(numbered_prefix("1. "), None);
        assert_eq!(bullet_prefix("· dot"), Some("dot"));
        assert_eq!(bullet_prefix("*"), None);
        assert_eq!(bullet_prefix("word"), None);
    }
}
