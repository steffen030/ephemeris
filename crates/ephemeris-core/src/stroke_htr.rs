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

/// Lowercased word set for final-hypothesis lexicon boost.
#[derive(Debug, Clone, Default)]
pub struct HtrLexicon {
    words: std::collections::HashSet<String>,
}

impl HtrLexicon {
    pub fn from_word_list(text: &str) -> Self {
        let mut words = std::collections::HashSet::new();
        for line in text.lines() {
            let w = line.trim().to_lowercase();
            if w.is_empty() || w.starts_with('#') {
                continue;
            }
            words.insert(w);
        }
        Self { words }
    }

    /// Fraction of alphabetic tokens found in the lexicon ∈ [0, 1].
    pub fn hit_ratio(&self, text: &str) -> f32 {
        let mut total = 0u32;
        let mut hits = 0u32;
        for tok in text.split(|c: char| !c.is_alphabetic()) {
            if tok.is_empty() {
                continue;
            }
            total += 1;
            if self.words.contains(&tok.to_lowercase()) {
                hits += 1;
            }
        }
        if total == 0 {
            0.0
        } else {
            hits as f32 / total as f32
        }
    }
}

/// Bundled EN+DE note lexicon (compile-time).
pub fn bundled_htr_lexicon_text() -> &'static str {
    include_str!("../data/htr_lexicon.txt")
}

/// Default LM + lexicon built from the bundled word list and charset.
pub fn default_htr_language_context(charset: &str, blank: usize) -> (CharBigramLm, HtrLexicon) {
    let text = bundled_htr_lexicon_text();
    (
        CharBigramLm::from_word_list(text, charset, blank),
        HtrLexicon::from_word_list(text),
    )
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

/// Prefix beam search over CTC log-probabilities (Hannun-style) with optional
/// character LM and lexicon rescoring of the final beam.
///
/// `log_probs` is row-major `[T * C]` **log**-probabilities (OnlineHTR LogSoftmax).
pub fn ctc_prefix_beam_decode(
    log_probs: &[f32],
    time_steps: usize,
    num_classes: usize,
    blank: usize,
    config: CtcBeamConfig,
    lm: Option<&CharBigramLm>,
    lexicon: Option<&HtrLexicon>,
    charset: &str,
) -> Vec<usize> {
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

            for c in 0..num_classes {
                if c == blank {
                    continue;
                }
                let lp = row[c];

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
    log_probs: &[f32],
    time_steps: usize,
    num_classes: usize,
    blank: usize,
    charset: &str,
    config: CtcBeamConfig,
    lm: Option<&CharBigramLm>,
    lexicon: Option<&HtrLexicon>,
) -> Vec<usize> {
    let beam = ctc_prefix_beam_decode(
        log_probs,
        time_steps,
        num_classes,
        blank,
        config,
        lm,
        lexicon,
        charset,
    );
    if beam.is_empty() {
        ctc_greedy_decode(log_probs, time_steps, num_classes, blank)
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
            &log_probs,
            3,
            3,
            2,
            CtcBeamConfig {
                beam_width: 4,
                lm_weight: 0.0,
                lexicon_weight: 0.0,
            },
            None,
            None,
            "ab",
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
        let mut log_probs = vec![-20.0f32; 3 * 5];
        // t
        log_probs[0 * 5 + 1] = 0.0;
        // h
        log_probs[1 * 5 + 2] = 0.0;
        // prefer x slightly over e acoustically
        log_probs[2 * 5 + 4] = -0.1;
        log_probs[2 * 5 + 3] = -0.3;

        let greedy = ctc_greedy_decode(&log_probs, 3, 5, blank);
        let greedy_text = decode_charset_with_blank(&greedy, charset, blank);
        assert_eq!(greedy_text, "thx");

        let lex = HtrLexicon::from_word_list("the\n");
        let beam = ctc_prefix_beam_decode(
            &log_probs,
            3,
            5,
            blank,
            CtcBeamConfig {
                beam_width: 8,
                lm_weight: 0.0,
                lexicon_weight: 2.0, // strong boost for the test
            },
            None,
            Some(&lex),
            charset,
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
}
