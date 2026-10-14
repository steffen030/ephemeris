//! HTR evaluation corpus + scoring (ephemeris-ktg.2).
//!
//! "Recognition is useless" cannot be fixed without a number. This module is
//! the model-free half of the `htr-eval` CLI: corpus loading, Levenshtein
//! CER/WER, per-line alignment, aggregation, and report/table rendering. It has
//! no feature gates so the CER math is unit-tested in every CI configuration;
//! the recognisers themselves are driven from `src/bin/htr-eval.rs`.
//!
//! # Corpus layout
//!
//! ```text
//! corpus/<sample>/strokes.json   StrokePage JSON (the app's strokes_{id}_p{n}.json)
//! corpus/<sample>/truth.txt      one line per handwritten line, top-to-bottom
//! corpus/<sample>/meta.json      optional {"lang": "de"|"en", "style": "neat"|"fast"|"cursive"}
//! ```
//!
//! See `docs/htr-corpus.md` for the capture guide.
//!
//! # Scoring
//!
//! Each recogniser transcribes the whole page; recognised spans are grouped
//! into reading-order lines ([`crate::transcriber::group_spans_into_lines`])
//! and aligned to the truth lines **by index** (line *i* ↔ truth line *i*).
//! Missing predicted lines score as empty (CER 1.0); surplus predicted lines
//! are counted in `extra_pred_lines` and do not affect per-line CER.
//!
//! * **CER** — character Levenshtein distance / truth length (Unicode scalars).
//!   Per-line values are uncapped (insertions can push them above 1.0). The
//!   headline `cer` is micro-averaged (total edits / total truth chars);
//!   `mean_line_cer` is the macro mean.
//! * **WER** — the same over whitespace-separated words.
//! * **empty rate** — fraction of truth lines with no predicted text.
//! * **mean confidence** — over non-empty predicted lines with a known score.
//! * **calibration gap** — `mean_conf − mean(max(0, 1 − CER))` over those same
//!   lines. `0` = calibrated, `> 0` = over-confident.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::Stroke;
use crate::store::StrokePage;
use crate::transcriber::{group_spans_into_lines, TextSpan};

// ── Edit-distance math ──────────────────────────────────────────────────────

/// Levenshtein distance (insert / delete / substitute, unit cost).
pub fn levenshtein<T: PartialEq>(a: &[T], b: &[T]) -> usize {
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ai) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, bj) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ai != bj);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Scoring options.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScoreOptions {
    /// Compare case-insensitively.
    pub ignore_case: bool,
}

/// Collapse runs of whitespace, trim, optionally lowercase.
pub fn normalize_text(s: &str, opts: ScoreOptions) -> String {
    let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if opts.ignore_case {
        joined.to_lowercase()
    } else {
        joined
    }
}

/// Edit count + reference length for one comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditCount {
    pub edits: usize,
    pub ref_len: usize,
}

impl EditCount {
    /// `edits / ref_len`; an empty reference scores `0.0` if the hypothesis is
    /// also empty (`edits == 0`), else `1.0`.
    pub fn rate(&self) -> f64 {
        if self.ref_len == 0 {
            if self.edits == 0 {
                0.0
            } else {
                1.0
            }
        } else {
            self.edits as f64 / self.ref_len as f64
        }
    }
}

/// Character-level edit count (after normalisation).
pub fn char_edits(truth: &str, pred: &str, opts: ScoreOptions) -> EditCount {
    let t: Vec<char> = normalize_text(truth, opts).chars().collect();
    let p: Vec<char> = normalize_text(pred, opts).chars().collect();
    EditCount {
        edits: levenshtein(&t, &p),
        ref_len: t.len(),
    }
}

/// Word-level edit count (after normalisation).
pub fn word_edits(truth: &str, pred: &str, opts: ScoreOptions) -> EditCount {
    let t_norm = normalize_text(truth, opts);
    let p_norm = normalize_text(pred, opts);
    let t: Vec<&str> = t_norm.split(' ').filter(|w| !w.is_empty()).collect();
    let p: Vec<&str> = p_norm.split(' ').filter(|w| !w.is_empty()).collect();
    EditCount {
        edits: levenshtein(&t, &p),
        ref_len: t.len(),
    }
}

/// Character error rate for one line: Levenshtein / truth length.
pub fn cer(truth: &str, pred: &str, opts: ScoreOptions) -> f64 {
    char_edits(truth, pred, opts).rate()
}

/// Word error rate for one line.
pub fn wer(truth: &str, pred: &str, opts: ScoreOptions) -> f64 {
    word_edits(truth, pred, opts).rate()
}

// ── Corpus ──────────────────────────────────────────────────────────────────

/// Optional `meta.json` of a sample.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SampleMeta {
    /// `"de"` | `"en"` (free-form).
    #[serde(default)]
    pub lang: Option<String>,
    /// `"neat"` | `"fast"` | `"cursive"` (free-form).
    #[serde(default)]
    pub style: Option<String>,
}

/// One corpus sample: a page of strokes plus line-wise ground truth.
#[derive(Debug, Clone)]
pub struct Sample {
    pub name: String,
    pub strokes: Vec<Stroke>,
    /// Truth lines, top-to-bottom (blank lines removed).
    pub truth: Vec<String>,
    pub meta: SampleMeta,
}

/// Parse `truth.txt`: one handwritten line per text line; blank lines skipped.
pub fn parse_truth(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.trim_end_matches('\r').trim())
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Load one sample directory (`strokes.json`, `truth.txt`, optional `meta.json`).
pub fn load_sample(dir: &Path) -> Result<Sample, String> {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.display().to_string());
    let strokes_path = dir.join("strokes.json");
    let raw = std::fs::read_to_string(&strokes_path)
        .map_err(|e| format!("{}: {e}", strokes_path.display()))?;
    let page: StrokePage =
        serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", strokes_path.display()))?;
    let truth_path = dir.join("truth.txt");
    let truth_raw = std::fs::read_to_string(&truth_path)
        .map_err(|e| format!("{}: {e}", truth_path.display()))?;
    let truth = parse_truth(&truth_raw);
    if truth.is_empty() {
        return Err(format!("{}: no truth lines", truth_path.display()));
    }
    let meta_path = dir.join("meta.json");
    let meta = if meta_path.is_file() {
        let m = std::fs::read_to_string(&meta_path)
            .map_err(|e| format!("{}: {e}", meta_path.display()))?;
        serde_json::from_str(&m).map_err(|e| format!("{}: {e}", meta_path.display()))?
    } else {
        SampleMeta::default()
    };
    Ok(Sample {
        name,
        strokes: page.strokes,
        truth,
        meta,
    })
}

/// Load a corpus root. If `root` itself holds `strokes.json` it is treated as
/// a single sample; otherwise every immediate subdirectory with a
/// `strokes.json` is a sample (sorted by name). Broken samples are reported in
/// the second tuple element rather than aborting the run.
pub fn load_corpus(root: &Path) -> Result<(Vec<Sample>, Vec<String>), String> {
    if !root.is_dir() {
        return Err(format!("{}: not a directory", root.display()));
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    if root.join("strokes.json").is_file() {
        dirs.push(root.to_path_buf());
    } else {
        let rd = std::fs::read_dir(root).map_err(|e| format!("{}: {e}", root.display()))?;
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() && p.join("strokes.json").is_file() {
                dirs.push(p);
            }
        }
        dirs.sort();
    }
    let mut samples = Vec::new();
    let mut errors = Vec::new();
    for d in dirs {
        match load_sample(&d) {
            Ok(s) => samples.push(s),
            Err(e) => errors.push(e),
        }
    }
    Ok((samples, errors))
}

// ── Per-line scoring ────────────────────────────────────────────────────────

/// A recognised line (spans grouped in reading order).
#[derive(Debug, Clone, PartialEq)]
pub struct PredLine {
    pub text: String,
    /// Mean of known span confidences; `None` when no span has a score.
    pub confidence: Option<f32>,
}

/// Group recogniser spans into reading-order [`PredLine`]s.
pub fn spans_to_pred_lines(spans: &[TextSpan]) -> Vec<PredLine> {
    group_spans_into_lines(spans)
        .into_iter()
        .map(|line| {
            let text = line
                .iter()
                .map(|s| s.text.trim())
                .collect::<Vec<_>>()
                .join(" ");
            let scored: Vec<f32> = line
                .iter()
                .map(|s| s.confidence)
                .filter(|c| *c >= 0.0)
                .collect();
            let confidence = if scored.is_empty() {
                None
            } else {
                Some(scored.iter().sum::<f32>() / scored.len() as f32)
            };
            PredLine { text, confidence }
        })
        .collect()
}

/// Score of one truth line against its aligned prediction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineScore {
    pub truth: String,
    pub pred: String,
    pub cer: f64,
    pub wer: f64,
    pub char_edits: usize,
    pub truth_chars: usize,
    pub word_edits: usize,
    pub truth_words: usize,
    /// No predicted text for this line.
    pub empty: bool,
    pub confidence: Option<f32>,
}

/// Align `pred` to `truth` by index and score every truth line.
///
/// Returns the line scores and the number of surplus predicted lines.
pub fn score_lines(
    truth: &[String],
    pred: &[PredLine],
    opts: ScoreOptions,
) -> (Vec<LineScore>, usize) {
    let lines = truth
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let p = pred.get(i);
            let ptext = p.map(|p| p.text.as_str()).unwrap_or("");
            let ce = char_edits(t, ptext, opts);
            let we = word_edits(t, ptext, opts);
            LineScore {
                truth: t.clone(),
                pred: ptext.to_string(),
                cer: ce.rate(),
                wer: we.rate(),
                char_edits: ce.edits,
                truth_chars: ce.ref_len,
                word_edits: we.edits,
                truth_words: we.ref_len,
                empty: ptext.trim().is_empty(),
                confidence: p.and_then(|p| p.confidence),
            }
        })
        .collect();
    (lines, pred.len().saturating_sub(truth.len()))
}

// ── Report ──────────────────────────────────────────────────────────────────

/// Result of one recogniser on one sample.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SampleResult {
    pub sample: String,
    pub lang: Option<String>,
    pub style: Option<String>,
    /// Wall time of the page transcription.
    pub wall_ms: f64,
    /// Recogniser error (the sample then scores as all-empty).
    pub error: Option<String>,
    pub extra_pred_lines: usize,
    pub lines: Vec<LineScore>,
}

/// Aggregate over every line of every sample for one recogniser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub samples: usize,
    pub lines: usize,
    /// Micro CER: total char edits / total truth chars.
    pub cer: f64,
    /// Macro mean of per-line CER.
    pub mean_line_cer: f64,
    /// Micro WER: total word edits / total truth words.
    pub wer: f64,
    /// Fraction of truth lines with no predicted text.
    pub empty_rate: f64,
    /// Mean confidence of non-empty predicted lines with a known score.
    pub mean_conf: Option<f64>,
    /// `mean_conf − mean(max(0, 1 − CER))` on the same lines (`> 0` = over-confident).
    pub calibration_gap: Option<f64>,
    pub extra_pred_lines: usize,
    pub total_wall_ms: f64,
    pub mean_ms_per_line: f64,
}

/// Aggregate sample results into a [`Summary`]. `None` when there are no lines.
pub fn summarize(results: &[SampleResult]) -> Option<Summary> {
    let all: Vec<&LineScore> = results.iter().flat_map(|r| r.lines.iter()).collect();
    if all.is_empty() {
        return None;
    }
    let n = all.len() as f64;
    let sum_usize = |f: fn(&LineScore) -> usize| all.iter().map(|l| f(l)).sum::<usize>();
    let char_edits = sum_usize(|l| l.char_edits);
    let truth_chars = sum_usize(|l| l.truth_chars);
    let word_edits = sum_usize(|l| l.word_edits);
    let truth_words = sum_usize(|l| l.truth_words);
    let rate = |e: usize, r: usize| {
        if r == 0 {
            if e == 0 {
                0.0
            } else {
                1.0
            }
        } else {
            e as f64 / r as f64
        }
    };
    let scored: Vec<(f64, f64)> = all
        .iter()
        .filter(|l| !l.empty)
        .filter_map(|l| l.confidence.map(|c| (f64::from(c), (1.0 - l.cer).max(0.0))))
        .collect();
    let (mean_conf, calibration_gap) = if scored.is_empty() {
        (None, None)
    } else {
        let m = scored.iter().map(|(c, _)| c).sum::<f64>() / scored.len() as f64;
        let acc = scored.iter().map(|(_, a)| a).sum::<f64>() / scored.len() as f64;
        (Some(m), Some(m - acc))
    };
    let total_wall_ms: f64 = results.iter().map(|r| r.wall_ms).sum();
    Some(Summary {
        samples: results.len(),
        lines: all.len(),
        cer: rate(char_edits, truth_chars),
        mean_line_cer: all.iter().map(|l| l.cer).sum::<f64>() / n,
        wer: rate(word_edits, truth_words),
        empty_rate: all.iter().filter(|l| l.empty).count() as f64 / n,
        mean_conf,
        calibration_gap,
        extra_pred_lines: results.iter().map(|r| r.extra_pred_lines).sum(),
        total_wall_ms,
        mean_ms_per_line: total_wall_ms / n,
    })
}

/// One recogniser's slice of the report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecogniserReport {
    /// `stroke-htr` | `line-htr` | `ocr` | `cascade`.
    pub id: String,
    pub available: bool,
    /// Why it is unavailable (e.g. "model missing: …"), or other remarks.
    pub note: Option<String>,
    pub summary: Option<Summary>,
    pub samples: Vec<SampleResult>,
}

impl RecogniserReport {
    /// A recogniser that could not run.
    pub fn unavailable(id: &str, note: impl Into<String>) -> Self {
        Self {
            id: id.to_string(),
            available: false,
            note: Some(note.into()),
            summary: None,
            samples: Vec::new(),
        }
    }

    /// A recogniser that ran; computes the summary.
    pub fn from_results(id: &str, samples: Vec<SampleResult>) -> Self {
        Self {
            id: id.to_string(),
            available: true,
            note: None,
            summary: summarize(&samples),
            samples,
        }
    }
}

/// Where/how the report was produced (relevant for aarch64 timing comparisons).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvInfo {
    pub arch: String,
    pub os: String,
    pub version: String,
    /// Cargo features of the `htr-eval` build (`htr`, `ocr`, …).
    pub features: Vec<String>,
    pub htr_model_dir: Option<String>,
    pub line_htr_model_dir: Option<String>,
    pub ocr_languages: String,
    pub ignore_case: bool,
    /// Whether the first sample was transcribed once untimed (model load warm-up).
    pub warmup: bool,
}

/// Full `htr-eval` report (`--json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalReport {
    pub schema_version: u32,
    pub corpus: String,
    pub samples: usize,
    pub truth_lines: usize,
    pub env: EnvInfo,
    /// Samples that failed to load.
    pub load_errors: Vec<String>,
    pub recognisers: Vec<RecogniserReport>,
}

fn pct(v: f64) -> String {
    format!("{:.1}", v * 100.0)
}

/// Render the human-readable table.
pub fn render_table(report: &EvalReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "htr-eval  corpus={}  samples={}  lines={}  arch={}  features=[{}]{}",
        report.corpus,
        report.samples,
        report.truth_lines,
        report.env.arch,
        report.env.features.join(","),
        if report.env.warmup {
            ""
        } else {
            "  (cold: first call includes model load)"
        },
    );
    let _ = writeln!(
        out,
        "{:<11} {:>5} {:>7} {:>9} {:>7} {:>7} {:>6} {:>7} {:>9} {:>9}",
        "recogniser",
        "lines",
        "CER%",
        "lineCER%",
        "WER%",
        "empty%",
        "conf",
        "calib",
        "ms/line",
        "total_ms"
    );
    for r in &report.recognisers {
        match (&r.summary, r.available) {
            (Some(s), true) => {
                let conf = s
                    .mean_conf
                    .map(|c| format!("{c:.2}"))
                    .unwrap_or_else(|| "-".into());
                let gap = s
                    .calibration_gap
                    .map(|g| format!("{g:+.2}"))
                    .unwrap_or_else(|| "-".into());
                let _ = writeln!(
                    out,
                    "{:<11} {:>5} {:>7} {:>9} {:>7} {:>7} {:>6} {:>7} {:>9.1} {:>9.1}",
                    r.id,
                    s.lines,
                    pct(s.cer),
                    pct(s.mean_line_cer),
                    pct(s.wer),
                    pct(s.empty_rate),
                    conf,
                    gap,
                    s.mean_ms_per_line,
                    s.total_wall_ms
                );
            }
            _ => {
                let _ = writeln!(
                    out,
                    "{:<11} n/a  -- {}",
                    r.id,
                    r.note.as_deref().unwrap_or("unavailable")
                );
            }
        }
    }
    for e in &report.load_errors {
        let _ = writeln!(out, "load error: {e}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const O: ScoreOptions = ScoreOptions { ignore_case: false };

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-6, "{a} != {b}");
    }

    #[test]
    fn levenshtein_classic_cases() {
        let l = |a: &str, b: &str| {
            levenshtein(
                &a.chars().collect::<Vec<_>>(),
                &b.chars().collect::<Vec<_>>(),
            )
        };
        assert_eq!(l("", ""), 0);
        assert_eq!(l("abc", ""), 3);
        assert_eq!(l("", "abc"), 3);
        assert_eq!(l("kitten", "sitting"), 3);
        assert_eq!(l("flaw", "lawn"), 2);
        assert_eq!(l("same", "same"), 0);
    }

    #[test]
    fn cer_basic_rates() {
        approx(cer("hello", "hello", O), 0.0);
        approx(cer("hello", "hallo", O), 0.2); // 1 sub / 5
        approx(cer("hello", "", O), 1.0); // 5 deletions / 5
        approx(cer("abcd", "ab", O), 0.5);
        // Insertions can push CER above 1.0.
        approx(cer("ab", "abcdef", O), 2.0);
    }

    #[test]
    fn cer_empty_reference() {
        approx(cer("", "", O), 0.0);
        approx(cer("", "x", O), 1.0);
    }

    #[test]
    fn cer_counts_unicode_scalars_not_bytes() {
        approx(cer("für", "fur", O), 1.0 / 3.0);
        // ö→o, ß→s, +s = 3 edits over 5 scalars (would be 7 bytes of truth).
        approx(cer("Größe", "Grosse", O), 3.0 / 5.0);
    }

    #[test]
    fn cer_normalises_whitespace_and_case_option() {
        approx(cer("a  b", "a b", O), 0.0);
        approx(cer("  a b ", "a b", O), 0.0);
        approx(cer("Call Anna", "call anna", O), 2.0 / 9.0);
        let ic = ScoreOptions { ignore_case: true };
        approx(cer("Call Anna", "call anna", ic), 0.0);
    }

    #[test]
    fn wer_basic_rates() {
        approx(wer("call anna tomorrow", "call anna tomorrow", O), 0.0);
        approx(
            wer("call anna tomorrow", "call hanna tomorrow", O),
            1.0 / 3.0,
        );
        approx(wer("call anna tomorrow", "call anna", O), 1.0 / 3.0);
        approx(wer("call anna", "", O), 1.0);
        approx(wer("a b", "a b c d", O), 1.0);
        approx(wer("", "", O), 0.0);
    }

    #[test]
    fn parse_truth_skips_blank_lines_and_crlf() {
        let t = parse_truth("one line\r\n\n  second  \n\n");
        assert_eq!(t, vec!["one line".to_string(), "second".to_string()]);
    }

    fn pl(text: &str, c: Option<f32>) -> PredLine {
        PredLine {
            text: text.into(),
            confidence: c,
        }
    }

    #[test]
    fn score_lines_aligns_by_index_and_counts_missing_and_extra() {
        let truth = vec!["hello".to_string(), "world".to_string()];
        let (lines, extra) = score_lines(&truth, &[pl("hallo", Some(0.9))], O);
        assert_eq!(extra, 0);
        assert_eq!(lines.len(), 2);
        approx(lines[0].cer, 0.2);
        assert!(!lines[0].empty);
        // Missing predicted line → all deletions.
        approx(lines[1].cer, 1.0);
        assert!(lines[1].empty);

        let (_, extra) = score_lines(
            &truth,
            &[pl("hello", None), pl("world", None), pl("junk", None)],
            O,
        );
        assert_eq!(extra, 1);
    }

    #[test]
    fn spans_group_into_lines_with_mean_confidence() {
        let mut a = TextSpan::new("Call", 0.0, 0.0, 10.0, 10.0);
        a.confidence = 0.8;
        a.line = 0;
        let mut b = TextSpan::new("Anna", 20.0, 0.0, 10.0, 10.0);
        b.confidence = 0.6;
        b.line = 0;
        let mut c = TextSpan::new("tomorrow", 0.0, 40.0, 10.0, 10.0);
        c.line = 1; // unknown confidence
        let lines = spans_to_pred_lines(&[a, b, c]);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "Call Anna");
        assert!((lines[0].confidence.unwrap() - 0.7).abs() < 1e-6);
        assert_eq!(lines[1].text, "tomorrow");
        assert_eq!(lines[1].confidence, None);
    }

    fn result(sample: &str, wall_ms: f64, lines: Vec<LineScore>) -> SampleResult {
        SampleResult {
            sample: sample.into(),
            lang: None,
            style: None,
            wall_ms,
            error: None,
            extra_pred_lines: 0,
            lines,
        }
    }

    #[test]
    fn summary_micro_macro_empty_and_calibration() {
        let truth = vec!["hello".to_string(), "ab".to_string(), "xy".to_string()];
        // line0: 1 sub (cer .2, conf .9); line1: empty; line2: exact (conf .5)
        let pred = vec![pl("hallo", Some(0.9)), pl("", None), pl("xy", Some(0.5))];
        let (lines, _) = score_lines(&truth, &pred, O);
        let s = summarize(&[result("s", 30.0, lines)]).unwrap();
        assert_eq!(s.lines, 3);
        // micro: (1 + 2 + 0) / (5 + 2 + 2)
        approx(s.cer, 3.0 / 9.0);
        // macro: (0.2 + 1.0 + 0.0) / 3
        approx(s.mean_line_cer, 1.2 / 3.0);
        approx(s.empty_rate, 1.0 / 3.0);
        // wer: words ["hello"] wrong, ["ab"] deleted, ["xy"] right → 2/3
        approx(s.wer, 2.0 / 3.0);
        // conf over non-empty scored lines: (0.9+0.5)/2 = 0.7; acc (0.8+1.0)/2 = 0.9
        approx(s.mean_conf.unwrap(), 0.7);
        approx(s.calibration_gap.unwrap(), -0.2);
        approx(s.mean_ms_per_line, 10.0);
        approx(s.total_wall_ms, 30.0);
    }

    #[test]
    fn summary_none_without_lines_and_no_conf_when_unscored() {
        assert!(summarize(&[]).is_none());
        let (lines, _) = score_lines(&["a".to_string()], &[pl("a", None)], O);
        let s = summarize(&[result("s", 1.0, lines)]).unwrap();
        assert_eq!(s.mean_conf, None);
        assert_eq!(s.calibration_gap, None);
    }

    #[test]
    fn table_marks_unavailable_recognisers() {
        let (lines, _) = score_lines(&["abc".to_string()], &[pl("abd", Some(0.5))], O);
        let report = EvalReport {
            schema_version: 1,
            corpus: "c".into(),
            samples: 1,
            truth_lines: 1,
            env: EnvInfo {
                arch: "x86_64".into(),
                os: "linux".into(),
                version: "0".into(),
                features: vec!["htr".into()],
                htr_model_dir: None,
                line_htr_model_dir: None,
                ocr_languages: "eng".into(),
                ignore_case: false,
                warmup: true,
            },
            load_errors: vec![],
            recognisers: vec![
                RecogniserReport::unavailable("stroke-htr", "model missing: /nope"),
                RecogniserReport::from_results("ocr", vec![result("s", 5.0, lines)]),
            ],
        };
        let t = render_table(&report);
        assert!(t.contains("stroke-htr"));
        assert!(t.contains("n/a"));
        assert!(t.contains("model missing"));
        assert!(t.contains("33.3")); // CER 1/3
                                     // JSON round-trips.
        let j = serde_json::to_string(&report).unwrap();
        let back: EvalReport = serde_json::from_str(&j).unwrap();
        assert_eq!(back.recognisers.len(), 2);
    }

    #[test]
    fn committed_fixtures_load() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/htr-corpus");
        let (samples, errors) = load_corpus(&root).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(samples.len(), 3);
        for s in &samples {
            assert!(!s.truth.is_empty(), "{}", s.name);
            assert!(
                s.strokes.iter().any(|st| st.points.len() >= 2),
                "{} has no ink",
                s.name
            );
        }
        // meta.json optional: at least one sample carries it, at least one does not.
        assert!(samples.iter().any(|s| s.meta.lang.is_some()));
        assert!(samples.iter().any(|s| s.meta == SampleMeta::default()));
    }
}
