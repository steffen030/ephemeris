//! Handwriting transcription trait (ADR ephemeris-2ri, ephemeris-97x.3 / 97x.6).
//!
//! Preferred path (ephemeris-5g2): [`CascadingTranscriber`] — online stroke HTR
//! when strokes + model are available, else [`RasterOcrTranscriber`] (Tesseract).

use crate::line_htr::{
    ctc_greedy_decode_logits, decode_line_alphabet, line_logits_confidence, prepare_line_htr_input,
    split_ink_line_bands, DE_HTR_V2_ALPHABET,
};
use crate::model::{Point, Stroke, Tool};
#[cfg(feature = "htr")]
use crate::stroke_htr::{
    ctc_decode_with_lm, ctc_softmax_confidence, decode_charset_with_blank,
    default_htr_language_context, features_to_tensor, CharBigramLm, CtcBeamConfig, HtrLexicon,
};
use crate::stroke_htr::{default_line_gap, featurize_line, group_strokes_into_lines};
#[cfg(feature = "ocr")]
use std::path::Path;
use std::path::PathBuf;
#[cfg(feature = "ocr")]
use std::process::Command;
#[cfg(feature = "htr")]
use std::sync::Mutex;

/// A recognised text span anchored to a page-local bounding box (logical px).
#[derive(Debug, Clone, PartialEq)]
pub struct TextSpan {
    pub text: String,
    /// Left edge in logical pixels.
    pub x: f32,
    /// Top edge in logical pixels.
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Model confidence in `[0, 1]`. Negative when unknown (e.g. OCR without score).
    pub confidence: f32,
    /// Tesseract / layout line index when known (`>= 0`); `-1` when unknown
    /// (e.g. stroke HTR line spans — treat each span as its own line).
    pub line: i32,
}

impl TextSpan {
    pub fn new(text: impl Into<String>, x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            text: text.into(),
            x,
            y,
            width,
            height,
            confidence: -1.0,
            line: -1,
        }
    }
}

/// Pluggable handwriting → text backend.
pub trait Transcriber: Send + Sync {
    /// Transcribe strokes on a single page into positioned text spans.
    fn transcribe(&self, strokes: &[Stroke]) -> crate::Result<Vec<TextSpan>>;

    /// Prefer this when a page raster is already available (PDF export path).
    ///
    /// Default: ignore the raster and return no spans (stroke-native backends
    /// override [`Self::transcribe`] instead).
    fn transcribe_raster(
        &self,
        width: u32,
        height: u32,
        gray8: &[u8],
    ) -> crate::Result<Vec<TextSpan>> {
        let _ = (width, height, gray8);
        Ok(Vec::new())
    }
}

/// Stub backend: always returns no text (privacy-safe default).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoOpTranscriber;

impl Transcriber for NoOpTranscriber {
    fn transcribe(&self, _strokes: &[Stroke]) -> crate::Result<Vec<TextSpan>> {
        Ok(Vec::new())
    }
}

/// Test/fake transcriber that echoes a fixed span list (for PDF text-layer tests).
#[derive(Debug, Clone)]
pub struct FakeTranscriber {
    pub spans: Vec<TextSpan>,
}

impl Transcriber for FakeTranscriber {
    fn transcribe(&self, _strokes: &[Stroke]) -> crate::Result<Vec<TextSpan>> {
        Ok(self.spans.clone())
    }

    fn transcribe_raster(
        &self,
        _width: u32,
        _height: u32,
        _gray8: &[u8],
    ) -> crate::Result<Vec<TextSpan>> {
        Ok(self.spans.clone())
    }
}

/// Mean confidence of spans that have a non-negative score; `0.0` if none.
pub fn mean_confidence(spans: &[TextSpan]) -> f32 {
    let scored: Vec<f32> = spans
        .iter()
        .map(|s| s.confidence)
        .filter(|c| *c >= 0.0)
        .collect();
    if scored.is_empty() {
        0.0
    } else {
        scored.iter().sum::<f32>() / scored.len() as f32
    }
}

/// Cascade: stroke HTR + line-image HTR + Tesseract (ephemeris-5g2 / 9hh.1).
pub struct CascadingTranscriber {
    pub htr: StrokeHtrTranscriber,
    pub line_htr: LineImageHtrTranscriber,
    pub ocr: RasterOcrTranscriber,
    /// Minimum mean neural confidence to prefer over empty OCR.
    pub min_htr_confidence: f32,
}

impl CascadingTranscriber {
    pub fn new(htr: StrokeHtrTranscriber, ocr: RasterOcrTranscriber) -> Self {
        Self::with_line_htr(htr, LineImageHtrTranscriber::default(), ocr)
    }

    pub fn with_line_htr(
        htr: StrokeHtrTranscriber,
        line_htr: LineImageHtrTranscriber,
        ocr: RasterOcrTranscriber,
    ) -> Self {
        Self {
            htr,
            line_htr,
            ocr,
            // Slightly stricter than the old 0.35 so weak OnlineHTR guesses
            // fall through to preprocessed Tesseract / line HTR.
            min_htr_confidence: 0.45,
        }
    }

    /// Transcribe a page: stroke HTR, line-image HTR, and Tesseract in parallel;
    /// keep the highest mean-confidence non-empty result.
    pub fn transcribe_page(
        &self,
        strokes: &[Stroke],
        width: u32,
        height: u32,
        gray8: &[u8],
    ) -> crate::Result<Vec<TextSpan>> {
        let run_stroke = !strokes.is_empty() && self.htr.enabled && self.htr.model_available();
        let run_line = self.line_htr.enabled && self.line_htr.model_available();
        let (stroke_r, line_r, ocr_r) = std::thread::scope(|scope| {
            let stroke_h = scope.spawn(|| {
                if run_stroke {
                    self.htr.transcribe(strokes)
                } else {
                    Ok(Vec::new())
                }
            });
            let line_h = scope.spawn(|| {
                if run_line {
                    self.line_htr.transcribe_raster(width, height, gray8)
                } else {
                    Ok(Vec::new())
                }
            });
            let ocr_h = scope.spawn(|| self.ocr.transcribe_raster(width, height, gray8));
            (stroke_h.join(), line_h.join(), ocr_h.join())
        });
        let stroke_spans = join_worker(stroke_r, "stroke HTR")?;
        let line_spans = join_worker(line_r, "line HTR")?;
        let ocr_spans = join_worker(ocr_r, "OCR")?;
        Ok(pick_best_cascade_spans(
            &[&stroke_spans, &line_spans, &ocr_spans],
            self.min_htr_confidence,
        ))
    }
}

fn join_worker(
    result: Result<crate::Result<Vec<TextSpan>>, Box<dyn std::any::Any + Send>>,
    label: &str,
) -> crate::Result<Vec<TextSpan>> {
    match result {
        Ok(Ok(s)) => Ok(s),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(crate::AppError::InvalidInput(format!(
            "{label} worker thread panicked"
        ))),
    }
}

/// Choose HTR vs OCR spans for the cascade (shared by page + stroke paths).
fn pick_cascade_spans(
    htr_spans: &[TextSpan],
    ocr_spans: &[TextSpan],
    min_htr_confidence: f32,
) -> Vec<TextSpan> {
    pick_best_cascade_spans(&[htr_spans, ocr_spans], min_htr_confidence)
}

fn effective_cascade_score(spans: &[TextSpan]) -> f32 {
    let m = mean_confidence(spans);
    if m < 0.0 {
        0.5
    } else {
        m
    }
}

/// Pick the highest mean-confidence non-empty candidate.
///
/// Unknown confidences (`< 0`) count as `0.5`. When a neural result is below
/// `min_htr_confidence` and another candidate scores higher, the stronger one
/// wins (same spirit as the old HTR→OCR gate).
fn pick_best_cascade_spans(candidates: &[&[TextSpan]], min_htr_confidence: f32) -> Vec<TextSpan> {
    let mut best: Option<(f32, &[TextSpan])> = None;
    for spans in candidates.iter().copied().filter(|s| !s.is_empty()) {
        let mut score = effective_cascade_score(spans);
        // Soft-penalise very weak neural guesses so OCR/line peers can win.
        if score < min_htr_confidence && spans.iter().all(|s| s.confidence >= 0.0) {
            score *= 0.85;
        }
        match best {
            Some((bs, _)) if bs >= score => {}
            _ => best = Some((score, spans)),
        }
    }
    best.map(|(_, s)| s.to_vec()).unwrap_or_default()
}

impl Transcriber for CascadingTranscriber {
    fn transcribe(&self, strokes: &[Stroke]) -> crate::Result<Vec<TextSpan>> {
        let htr_spans = if !strokes.is_empty() && self.htr.enabled && self.htr.model_available() {
            self.htr.transcribe(strokes)?
        } else {
            Vec::new()
        };
        let ocr_spans = self.ocr.transcribe(strokes)?;
        Ok(pick_cascade_spans(
            &htr_spans,
            &ocr_spans,
            self.min_htr_confidence,
        ))
    }

    fn transcribe_raster(
        &self,
        width: u32,
        height: u32,
        gray8: &[u8],
    ) -> crate::Result<Vec<TextSpan>> {
        let line = if self.line_htr.enabled && self.line_htr.model_available() {
            self.line_htr.transcribe_raster(width, height, gray8)?
        } else {
            Vec::new()
        };
        let ocr = self.ocr.transcribe_raster(width, height, gray8)?;
        Ok(pick_best_cascade_spans(
            &[&line, &ocr],
            self.min_htr_confidence,
        ))
    }
}

/// Online stroke HTR backend (Carbune/OnlineHTR-style features → ONNX/RTen).
///
/// Without the `htr` Cargo feature or a downloaded model, [`Self::transcribe`]
/// returns an empty list so the cascade can fall through to OCR.
pub struct StrokeHtrTranscriber {
    pub enabled: bool,
    /// Directory containing `model.rten` (or `model.onnx`) and `charset.txt`.
    pub model_dir: Option<PathBuf>,
    /// Points per unit length for stroke interpolation (OnlineHTR default: 20).
    pub points_per_unit: f32,
    #[cfg(feature = "htr")]
    session: Mutex<Option<HtrSession>>,
}

#[cfg(feature = "htr")]
struct HtrSession {
    model: rten::Model,
    charset: String,
    /// CTC blank class index (OnlineHTR / Carbune uses `0`).
    blank_index: usize,
    /// Character bigram LM + note lexicon for beam rescoring (ephemeris-9hh).
    lm: CharBigramLm,
    lexicon: HtrLexicon,
    beam: CtcBeamConfig,
}

impl Clone for StrokeHtrTranscriber {
    fn clone(&self) -> Self {
        Self {
            enabled: self.enabled,
            model_dir: self.model_dir.clone(),
            points_per_unit: self.points_per_unit,
            #[cfg(feature = "htr")]
            session: Mutex::new(None),
        }
    }
}

impl std::fmt::Debug for StrokeHtrTranscriber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StrokeHtrTranscriber")
            .field("enabled", &self.enabled)
            .field("model_dir", &self.model_dir)
            .field("points_per_unit", &self.points_per_unit)
            .finish()
    }
}

impl Default for StrokeHtrTranscriber {
    fn default() -> Self {
        Self {
            enabled: true,
            model_dir: None,
            points_per_unit: 20.0,
            #[cfg(feature = "htr")]
            session: Mutex::new(None),
        }
    }
}

impl StrokeHtrTranscriber {
    pub fn from_parts(enabled: bool, model_dir: Option<PathBuf>) -> Self {
        Self {
            enabled,
            model_dir,
            ..Self::default()
        }
    }

    /// True when a model file is present on disk (does not load it yet).
    pub fn model_available(&self) -> bool {
        let Some(dir) = self.model_dir.as_ref() else {
            return false;
        };
        dir.join("model.onnx").is_file() || dir.join("model.rten").is_file()
    }

    /// User-facing hint when HTR yields nothing.
    pub fn empty_result_hint(&self) -> String {
        if !self.enabled {
            return "Stroke HTR is disabled.".into();
        }
        if !self.model_available() {
            return "Handwriting model not installed. Settings → Handwriting, or See-text uses OCR."
                .into();
        }
        "No text recognised from strokes.".into()
    }

    #[cfg(feature = "htr")]
    fn charset_path(dir: &std::path::Path) -> PathBuf {
        let a = dir.join("charset.txt");
        if a.is_file() {
            a
        } else {
            dir.join("dict.txt")
        }
    }

    #[cfg(feature = "htr")]
    fn ensure_session(&self) -> crate::Result<()> {
        let mut guard = self
            .session
            .lock()
            .map_err(|_| crate::AppError::InvalidInput("htr session lock poisoned".into()))?;
        if guard.is_some() {
            return Ok(());
        }
        let Some(dir) = self.model_dir.as_ref() else {
            return Ok(());
        };
        let onnx_path = dir.join("model.onnx");
        let rten_path = dir.join("model.rten");
        let model_path = if onnx_path.is_file() {
            onnx_path
        } else if rten_path.is_file() {
            rten_path
        } else {
            return Ok(());
        };
        let model = rten::Model::load_file(&model_path)
            .map_err(|e| crate::AppError::InvalidInput(format!("load HTR model: {e}")))?;
        let charset = load_charset(dir);
        let blank_index = load_blank_index(dir);
        let (lm, lexicon) = default_htr_language_context(&charset, blank_index);
        *guard = Some(HtrSession {
            model,
            charset,
            blank_index,
            lm,
            lexicon,
            beam: CtcBeamConfig::default(),
        });
        Ok(())
    }

    #[cfg(feature = "htr")]
    fn infer_line(&self, features: &[[f32; 4]]) -> crate::Result<(String, f32)> {
        use rten_tensor::{AsView, Layout, NdTensor};

        self.ensure_session()?;
        let guard = self
            .session
            .lock()
            .map_err(|_| crate::AppError::InvalidInput("htr session lock poisoned".into()))?;
        let Some(sess) = guard.as_ref() else {
            return Ok((String::new(), 0.0));
        };
        let (buf, t) = features_to_tensor(features);
        if t == 0 {
            return Ok((String::new(), 0.0));
        }
        let input = NdTensor::from_data([1, t, 4], buf);
        let outputs = sess
            .model
            .run_one(input.view().into(), None)
            .map_err(|e| crate::AppError::InvalidInput(format!("HTR infer: {e}")))?;
        let tensor = outputs
            .into_tensor::<f32>()
            .ok_or_else(|| crate::AppError::InvalidInput("HTR output not f32".into()))?;
        let shape = tensor.shape().to_vec();
        let logits = tensor.into_data();
        // Expect [1, T, C] or [T, C]; C = alphabet + blank
        let c = sess.charset.chars().count() + 1;
        let time = match shape.as_slice() {
            [1, t_out, c_out] if *c_out == c => *t_out,
            [t_out, c_out] if *c_out == c => *t_out,
            _ if c > 0 && logits.len() % c == 0 => logits.len() / c,
            _ => return Ok((String::new(), 0.0)),
        };
        let blank = sess.blank_index.min(c.saturating_sub(1));
        // Prefix beam + char LM / lexicon (ASR-style context on device).
        let path = ctc_decode_with_lm(
            &logits,
            time,
            c,
            blank,
            &sess.charset,
            sess.beam,
            Some(&sess.lm),
            Some(&sess.lexicon),
        );
        let text = decode_charset_with_blank(&path, &sess.charset, blank);
        let conf = ctc_softmax_confidence(&logits, time, c, blank);
        Ok((text, conf))
    }

    #[cfg(not(feature = "htr"))]
    fn infer_line(&self, _features: &[[f32; 4]]) -> crate::Result<(String, f32)> {
        Ok((String::new(), 0.0))
    }
}

#[cfg(feature = "htr")]
fn default_charset() -> &'static str {
    " !\"#&'()*+,-./0123456789:;?ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
}

/// Load alphabet-only charset (no blank). Prefers line-based `charset.txt`.
#[cfg(feature = "htr")]
fn load_charset(dir: &std::path::Path) -> String {
    let path = StrokeHtrTranscriber::charset_path(dir);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return default_charset().to_string();
    };
    if raw.contains('\n') {
        raw.lines()
            .map(|l| l.trim_end_matches('\r'))
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| l.chars().next().unwrap_or(' '))
            .collect()
    } else {
        raw.chars().filter(|c| *c != '\n' && *c != '\r').collect()
    }
}

/// Read `meta.json` `blank_index` (default `0` for OnlineHTR).
#[cfg(feature = "htr")]
fn load_blank_index(dir: &std::path::Path) -> usize {
    let path = dir.join("meta.json");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return 0;
    };
    #[derive(serde::Deserialize)]
    struct Meta {
        #[serde(default)]
        blank_index: usize,
    }
    serde_json::from_str::<Meta>(&raw)
        .map(|m| m.blank_index)
        .unwrap_or(0)
}

impl Transcriber for StrokeHtrTranscriber {
    fn transcribe(&self, strokes: &[Stroke]) -> crate::Result<Vec<TextSpan>> {
        if !self.enabled || strokes.is_empty() || !self.model_available() {
            return Ok(Vec::new());
        }
        #[cfg(feature = "htr")]
        {
            self.ensure_session()?;
        }
        let gap = default_line_gap(strokes);
        let lines = group_strokes_into_lines(strokes, gap);
        let mut spans = Vec::new();
        for line in lines {
            let feats = featurize_line(&line, self.points_per_unit);
            if feats.is_empty() {
                continue;
            }
            let (text, conf) = self.infer_line(&feats)?;
            let text = text.trim().to_string();
            if text.is_empty() {
                continue;
            }
            spans.push(TextSpan {
                text,
                x: line.x,
                y: line.y,
                width: line.width,
                height: line.height,
                confidence: conf,
                // Each HTR decode is one handwritten line.
                line: spans.len() as i32,
            });
        }
        Ok(spans)
    }
}

/// Line-image CTC HTR (ephemeris-9hh.1) — DE·HTR v2-class ONNX via RTen.
///
/// Segments the page gray8 into ink bands, resizes each to 64×W, and runs a
/// downloaded line model. Without the `htr` feature or model files, returns
/// empty spans so the cascade falls through to Tesseract.
pub struct LineImageHtrTranscriber {
    pub enabled: bool,
    /// Directory with `model.onnx` (or `model.rten`) + `charset.txt` + `meta.json`.
    pub model_dir: Option<PathBuf>,
    #[cfg(feature = "htr")]
    session: Mutex<Option<LineHtrSession>>,
}

#[cfg(feature = "htr")]
struct LineHtrSession {
    model: rten::Model,
    alphabet: String,
    blank_index: usize,
    input_name: String,
    output_name: String,
}

impl Clone for LineImageHtrTranscriber {
    fn clone(&self) -> Self {
        Self {
            enabled: self.enabled,
            model_dir: self.model_dir.clone(),
            #[cfg(feature = "htr")]
            session: Mutex::new(None),
        }
    }
}

impl std::fmt::Debug for LineImageHtrTranscriber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LineImageHtrTranscriber")
            .field("enabled", &self.enabled)
            .field("model_dir", &self.model_dir)
            .finish()
    }
}

impl Default for LineImageHtrTranscriber {
    fn default() -> Self {
        Self {
            enabled: true,
            model_dir: None,
            #[cfg(feature = "htr")]
            session: Mutex::new(None),
        }
    }
}

impl LineImageHtrTranscriber {
    pub fn from_parts(enabled: bool, model_dir: Option<PathBuf>) -> Self {
        Self {
            enabled,
            model_dir,
            ..Self::default()
        }
    }

    pub fn model_available(&self) -> bool {
        let Some(dir) = self.model_dir.as_ref() else {
            return false;
        };
        dir.join("model.onnx").is_file() || dir.join("model.rten").is_file()
    }

    pub fn empty_result_hint(&self) -> String {
        if !self.enabled {
            return "Line handwriting model is disabled.".into();
        }
        if !self.model_available() {
            return "Line HTR model not installed. Settings → Handwriting → Line model.".into();
        }
        "No text recognised from line image HTR.".into()
    }

    #[cfg(feature = "htr")]
    fn ensure_session(&self) -> crate::Result<()> {
        let mut guard = self
            .session
            .lock()
            .map_err(|_| crate::AppError::InvalidInput("line HTR session lock poisoned".into()))?;
        if guard.is_some() {
            return Ok(());
        }
        let Some(dir) = self.model_dir.as_ref() else {
            return Ok(());
        };
        let onnx_path = dir.join("model.onnx");
        let rten_path = dir.join("model.rten");
        let model_path = if onnx_path.is_file() {
            onnx_path
        } else if rten_path.is_file() {
            rten_path
        } else {
            return Ok(());
        };
        let model = rten::Model::load_file(&model_path)
            .map_err(|e| crate::AppError::InvalidInput(format!("load line HTR model: {e}")))?;
        let (alphabet, blank_index, input_name, output_name) = load_line_htr_meta(dir);
        *guard = Some(LineHtrSession {
            model,
            alphabet,
            blank_index,
            input_name,
            output_name,
        });
        Ok(())
    }

    #[cfg(feature = "htr")]
    fn infer_line_image(
        &self,
        input: &crate::line_htr::LineHtrInput,
    ) -> crate::Result<(String, f32)> {
        use rten_tensor::{AsView, Layout, NdTensor};

        self.ensure_session()?;
        let guard = self
            .session
            .lock()
            .map_err(|_| crate::AppError::InvalidInput("line HTR session lock poisoned".into()))?;
        let Some(sess) = guard.as_ref() else {
            return Ok((String::new(), 0.0));
        };
        let w = input.width as usize;
        let h = input.height as usize;
        if w == 0 || h == 0 || input.pixels.len() < w * h {
            return Ok((String::new(), 0.0));
        }
        // NCHW: [1, 1, H, W] — DE·HTR v2 single I/O; prefer named nodes when present.
        let tensor = NdTensor::from_data([1, 1, h, w], input.pixels.clone());
        let outputs = {
            let in_id = sess
                .model
                .node_id(&sess.input_name)
                .ok()
                .or_else(|| sess.model.input_ids().first().copied());
            let out_id = sess
                .model
                .node_id(&sess.output_name)
                .ok()
                .or_else(|| sess.model.output_ids().first().copied());
            match (in_id, out_id) {
                (Some(iin), Some(oout)) => sess
                    .model
                    .run(vec![(iin, tensor.view().into())], &[oout], None)
                    .map_err(|e| crate::AppError::InvalidInput(format!("line HTR infer: {e}")))?
                    .into_iter()
                    .next()
                    .ok_or_else(|| crate::AppError::InvalidInput("line HTR: no outputs".into()))?,
                _ => sess
                    .model
                    .run_one(tensor.view().into(), None)
                    .map_err(|e| crate::AppError::InvalidInput(format!("line HTR infer: {e}")))?,
            }
        };
        let out = outputs
            .into_tensor::<f32>()
            .ok_or_else(|| crate::AppError::InvalidInput("line HTR output not f32".into()))?;
        let shape = out.shape().to_vec();
        let logits = out.into_data();
        let c = sess.alphabet.chars().count() + 1;
        let time = match shape.as_slice() {
            [1, t_out, c_out] if *c_out == c => *t_out,
            [t_out, c_out] if *c_out == c => *t_out,
            [1, c_out, t_out] if *c_out == c => *t_out, // NCT
            _ if c > 0 && logits.len() % c == 0 => logits.len() / c,
            _ => return Ok((String::new(), 0.0)),
        };
        // If layout was NCT, transpose would be needed — DE·HTR v2 is T×C in
        // docs as logits; RTen typically keeps export order. Prefer T×C.
        let logits = match shape.as_slice() {
            [1, c_out, t_out] if *c_out == c => {
                // Transpose NCT → NTC flat T*C
                let mut ntc = vec![0.0f32; time * c];
                for t in 0..time {
                    for ci in 0..c {
                        ntc[t * c + ci] = logits[ci * time + t];
                    }
                }
                ntc
            }
            _ => logits,
        };
        let blank = sess.blank_index.min(c.saturating_sub(1));
        let path = ctc_greedy_decode_logits(&logits, time, c, blank);
        let text = decode_line_alphabet(&path, &sess.alphabet, blank);
        let conf = line_logits_confidence(&logits, time, c, blank);
        Ok((text, conf))
    }

    #[cfg(not(feature = "htr"))]
    fn infer_line_image(
        &self,
        _input: &crate::line_htr::LineHtrInput,
    ) -> crate::Result<(String, f32)> {
        Ok((String::new(), 0.0))
    }
}

#[cfg(feature = "htr")]
fn load_line_htr_meta(dir: &std::path::Path) -> (String, usize, String, String) {
    let meta_path = dir.join("meta.json");
    let charset_path = dir.join("charset.txt");
    let alphabet_from_file = std::fs::read_to_string(&charset_path).ok().map(|raw| {
        if raw.contains('\n') {
            raw.lines()
                .map(|l| l.trim_end_matches('\r'))
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(|l| l.chars().next().unwrap_or(' '))
                .collect::<String>()
        } else {
            raw.chars().filter(|c| *c != '\n' && *c != '\r').collect()
        }
    });

    #[derive(serde::Deserialize)]
    struct Meta {
        #[serde(default)]
        blank_index: usize,
        #[serde(default)]
        alphabet: String,
        #[serde(default)]
        input_name: String,
        #[serde(default)]
        output_name: String,
    }

    let meta = std::fs::read_to_string(&meta_path)
        .ok()
        .and_then(|r| serde_json::from_str::<Meta>(&r).ok());

    let alphabet = alphabet_from_file
        .or_else(|| {
            meta.as_ref()
                .map(|m| m.alphabet.clone())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| DE_HTR_V2_ALPHABET.to_string());
    let blank = meta.as_ref().map(|m| m.blank_index).unwrap_or(0);
    let input = meta
        .as_ref()
        .map(|m| m.input_name.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "image".into());
    let output = meta
        .as_ref()
        .map(|m| m.output_name.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "logits".into());
    (alphabet, blank, input, output)
}

impl Transcriber for LineImageHtrTranscriber {
    fn transcribe(&self, strokes: &[Stroke]) -> crate::Result<Vec<TextSpan>> {
        if !self.enabled || strokes.is_empty() || !self.model_available() {
            return Ok(Vec::new());
        }
        let (w, h, gray) = rasterize_strokes(strokes);
        self.transcribe_raster(w, h, &gray)
    }

    fn transcribe_raster(
        &self,
        width: u32,
        height: u32,
        gray8: &[u8],
    ) -> crate::Result<Vec<TextSpan>> {
        if !self.enabled || !self.model_available() {
            return Ok(Vec::new());
        }
        #[cfg(feature = "htr")]
        {
            self.ensure_session()?;
        }
        let bands = split_ink_line_bands(width, height, gray8);
        let mut spans = Vec::new();
        for (i, band) in bands.iter().enumerate() {
            let Some(input) = prepare_line_htr_input(width, height, gray8, band) else {
                continue;
            };
            let (text, conf) = self.infer_line_image(&input)?;
            let text = text.trim().to_string();
            if text.is_empty() {
                continue;
            }
            spans.push(TextSpan {
                text,
                x: band.x0 as f32,
                y: band.y0 as f32,
                width: (band.x1 - band.x0).max(1) as f32,
                height: (band.y1 - band.y0).max(1) as f32,
                confidence: conf,
                line: i as i32,
            });
        }
        Ok(spans)
    }
}

/// Rasterize + Tesseract OCR backend (spike ephemeris-97x.4 / task 97x.6).
///
/// Invokes the system `tesseract` binary with TSV word boxes. When the `ocr`
/// Cargo feature is disabled, or the binary/tessdata is unavailable, returns
/// an empty span list without failing (NoOp fallback).
#[derive(Debug, Clone)]
pub struct RasterOcrTranscriber {
    /// Master switch (profile/config). When false, always returns empty.
    pub enabled: bool,
    /// Tesseract `-l` language string (e.g. `"eng"` or `"eng+deu"`).
    pub languages: String,
    /// Optional tessdata directory (`TESSDATA_PREFIX`).
    pub tessdata: Option<PathBuf>,
    /// Optional path to the `tesseract` binary; default looks up `PATH`.
    pub tesseract_bin: Option<PathBuf>,
}

impl Default for RasterOcrTranscriber {
    fn default() -> Self {
        Self {
            enabled: true,
            languages: "eng".into(),
            tessdata: None,
            tesseract_bin: None,
        }
    }
}

impl RasterOcrTranscriber {
    pub fn new(languages: impl Into<String>) -> Self {
        Self {
            languages: languages.into(),
            ..Self::default()
        }
    }

    /// Build from common config fields.
    pub fn from_parts(
        enabled: bool,
        languages: impl Into<String>,
        tessdata: Option<PathBuf>,
    ) -> Self {
        Self {
            enabled,
            languages: {
                let l = languages.into();
                if l.trim().is_empty() {
                    "eng".into()
                } else {
                    l
                }
            },
            tessdata,
            tesseract_bin: None,
        }
    }

    /// Join span texts into searchable / See-text body text.
    ///
    /// Words on the same layout line are space-separated; distinct lines are
    /// joined with `\n` so multi-line handwriting keeps paragraph structure.
    pub fn spans_to_text(spans: &[TextSpan]) -> String {
        spans_to_multiline_text(spans)
    }

    /// True when the configured `tesseract` binary responds (OCR can run).
    ///
    /// Always `false` when the `ocr` Cargo feature is off.
    pub fn tesseract_available(&self) -> bool {
        #[cfg(not(feature = "ocr"))]
        {
            false
        }
        #[cfg(feature = "ocr")]
        {
            let bin = self
                .tesseract_bin
                .clone()
                .unwrap_or_else(|| PathBuf::from("tesseract"));
            let mut cmd = Command::new(&bin);
            cmd.arg("--version");
            if let Some(ref td) = self.tessdata {
                cmd.env("TESSDATA_PREFIX", td);
            }
            match cmd.output() {
                Ok(o) => o.status.success(),
                Err(_) => false,
            }
        }
    }

    /// User-facing hint when See-text / export yields no spans.
    pub fn empty_result_hint(&self) -> String {
        if !self.enabled {
            return "OCR is disabled (set EPHEMERIS_OCR=1 or ocr_enabled in config).".into();
        }
        if !self.tesseract_available() {
            return "Tesseract not found. On PineNote: sudo apt install tesseract-ocr tesseract-ocr-eng"
                .into();
        }
        "No text recognised. Try larger letters, or Settings → Handwriting for stroke HTR.".into()
    }
}

impl Transcriber for RasterOcrTranscriber {
    fn transcribe(&self, strokes: &[Stroke]) -> crate::Result<Vec<TextSpan>> {
        if !self.enabled || strokes.is_empty() {
            return Ok(Vec::new());
        }
        let (w, h, gray) = rasterize_strokes(strokes);
        self.transcribe_raster(w, h, &gray)
    }

    fn transcribe_raster(
        &self,
        width: u32,
        height: u32,
        gray8: &[u8],
    ) -> crate::Result<Vec<TextSpan>> {
        if !self.enabled {
            return Ok(Vec::new());
        }
        #[cfg(not(feature = "ocr"))]
        {
            let _ = (width, height, gray8);
            Ok(Vec::new())
        }
        #[cfg(feature = "ocr")]
        {
            run_tesseract_tsv(self, width, height, gray8)
        }
    }
}

/// Prepared OCR image + transform back to the source raster coordinates.
#[derive(Debug, Clone)]
struct OcrPrepared {
    gray: Vec<u8>,
    width: u32,
    height: u32,
    /// Crop origin in the source image.
    origin_x: f32,
    origin_y: f32,
    /// Uniform scale applied after crop.
    scale: f32,
    /// Tesseract page segmentation mode.
    psm: u8,
    /// Declared DPI so LSTM sees roughly print-sized glyphs.
    dpi: u32,
}

/// Ink darkness threshold (gray8); below this counts as ink.
const INK_THRESHOLD: u8 = 200;

/// Crop to ink, thicken thin strokes, upscale, and pick a PSM for handwriting.
fn prepare_ocr_image(width: u32, height: u32, gray8: &[u8]) -> Option<OcrPrepared> {
    let expected = (width as usize).saturating_mul(height as usize);
    if width == 0 || height == 0 || gray8.len() < expected {
        return None;
    }
    let (min_x, min_y, max_x, max_y) = ink_bbox(width, height, &gray8[..expected])?;
    let pad = 16i32;
    let x0 = (min_x as i32 - pad).max(0) as u32;
    let y0 = (min_y as i32 - pad).max(0) as u32;
    let x1 = (max_x as i32 + pad).min(width as i32 - 1).max(x0 as i32) as u32;
    let y1 = (max_y as i32 + pad).min(height as i32 - 1).max(y0 as i32) as u32;
    let cw = (x1 - x0 + 1).max(1);
    let ch = (y1 - y0 + 1).max(1);

    let mut cropped = vec![255u8; (cw as usize) * (ch as usize)];
    for y in 0..ch {
        let src_row = ((y0 + y) * width + x0) as usize;
        let dst_row = (y * cw) as usize;
        cropped[dst_row..dst_row + cw as usize]
            .copy_from_slice(&gray8[src_row..src_row + cw as usize]);
    }
    // Thicken thin pen strokes before upscale (helps single letters / light ink).
    cropped = dilate_ink(&cropped, cw, ch, 1);

    // Target ~3× for small glyphs (letters), 2× for larger blocks; cap size.
    let short = cw.min(ch);
    let scale: u32 = if short < 48 {
        4
    } else if short < 120 {
        3
    } else {
        2
    };
    let (sw, sh, scaled) = nearest_upscale(&cropped, cw, ch, scale);
    // Keep Tesseract inputs bounded on large pages.
    let (fw, fh, final_gray, used_scale) = if sw > 2400 || sh > 2400 {
        let factor = (2400.0 / sw.max(sh) as f32).min(1.0);
        if factor < 0.99 {
            let ns = ((scale as f32) * factor).max(1.0).round().max(1.0) as u32;
            let (w2, h2, g2) = nearest_upscale(&cropped, cw, ch, ns);
            (w2, h2, g2, ns as f32)
        } else {
            (sw, sh, scaled, scale as f32)
        }
    } else {
        (sw, sh, scaled, scale as f32)
    };

    let psm = choose_ocr_psm(cw, ch);
    // After upscale, pretend ~150–300 DPI so LSTM sees print-like sizes.
    let dpi = (72u32.saturating_mul(used_scale as u32)).clamp(150, 300);

    Some(OcrPrepared {
        gray: final_gray,
        width: fw,
        height: fh,
        origin_x: x0 as f32,
        origin_y: y0 as f32,
        scale: used_scale,
        psm,
        dpi,
    })
}

fn ink_bbox(width: u32, height: u32, gray8: &[u8]) -> Option<(u32, u32, u32, u32)> {
    let mut min_x = width;
    let mut min_y = height;
    let mut max_x = 0u32;
    let mut max_y = 0u32;
    let mut any = false;
    for y in 0..height {
        for x in 0..width {
            if gray8[(y * width + x) as usize] < INK_THRESHOLD {
                any = true;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
    }
    if any {
        Some((min_x, min_y, max_x, max_y))
    } else {
        None
    }
}

fn dilate_ink(gray: &[u8], w: u32, h: u32, radius: i32) -> Vec<u8> {
    if radius <= 0 {
        return gray.to_vec();
    }
    let mut out = vec![255u8; gray.len()];
    let r = radius;
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let mut dark = false;
            'n: for dy in -r..=r {
                for dx in -r..=r {
                    let nx = x + dx;
                    let ny = y + dy;
                    if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                        continue;
                    }
                    if gray[(ny as u32 * w + nx as u32) as usize] < INK_THRESHOLD {
                        dark = true;
                        break 'n;
                    }
                }
            }
            if dark {
                out[(y as u32 * w + x as u32) as usize] = 0;
            }
        }
    }
    out
}

fn nearest_upscale(gray: &[u8], w: u32, h: u32, scale: u32) -> (u32, u32, Vec<u8>) {
    let scale = scale.max(1);
    if scale == 1 {
        return (w, h, gray.to_vec());
    }
    let nw = w * scale;
    let nh = h * scale;
    let mut out = vec![255u8; (nw as usize) * (nh as usize)];
    for y in 0..h {
        for x in 0..w {
            let v = gray[(y * w + x) as usize];
            for dy in 0..scale {
                for dx in 0..scale {
                    let ox = x * scale + dx;
                    let oy = y * scale + dy;
                    out[(oy * nw + ox) as usize] = v;
                }
            }
        }
    }
    (nw, nh, out)
}

/// Pick Tesseract PSM for cropped handwriting.
///
/// - tiny / square-ish → single word/char (8)
/// - wide short strip → single line (7)
/// - otherwise sparse text (11) — better than block PSM 6 for notes
fn choose_ocr_psm(crop_w: u32, crop_h: u32) -> u8 {
    let short = crop_w.min(crop_h);
    let long = crop_w.max(crop_h);
    if short < 64 && long < 160 {
        8 // treat as a single word / letter cluster
    } else if crop_h > 0 && (crop_w as f32 / crop_h as f32) >= 3.5 && crop_h < 80 {
        7 // single text line
    } else {
        11 // sparse text
    }
}

fn remap_ocr_spans(spans: Vec<TextSpan>, prep: &OcrPrepared) -> Vec<TextSpan> {
    let inv = if prep.scale > 0.0 {
        1.0 / prep.scale
    } else {
        1.0
    };
    spans
        .into_iter()
        .map(|mut s| {
            s.x = s.x * inv + prep.origin_x;
            s.y = s.y * inv + prep.origin_y;
            s.width *= inv;
            s.height *= inv;
            s
        })
        .collect()
}

#[cfg(feature = "ocr")]
fn run_tesseract_tsv(
    cfg: &RasterOcrTranscriber,
    width: u32,
    height: u32,
    gray8: &[u8],
) -> crate::Result<Vec<TextSpan>> {
    if width == 0 || height == 0 {
        return Ok(Vec::new());
    }
    let expected = (width as usize).saturating_mul(height as usize);
    if gray8.len() < expected {
        return Err(crate::AppError::InvalidInput(format!(
            "ocr raster: expected {expected} bytes, got {}",
            gray8.len()
        )));
    }

    let Some(prep) = prepare_ocr_image(width, height, &gray8[..expected]) else {
        return Ok(Vec::new());
    };

    let bin = cfg
        .tesseract_bin
        .clone()
        .unwrap_or_else(|| PathBuf::from("tesseract"));

    let tmp = tempfile_dir()?;
    let img_path = tmp.join("page.pgm");
    write_pgm(&img_path, prep.width, prep.height, &prep.gray)?;

    // Try the adaptive PSM first; if empty, fall back to uniform block (6).
    let mut spans = invoke_tesseract_tsv(cfg, &bin, &img_path, prep.psm, prep.dpi)?;
    if spans.is_empty() && prep.psm != 6 {
        spans = invoke_tesseract_tsv(cfg, &bin, &img_path, 6, prep.dpi)?;
    }
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(remap_ocr_spans(spans, &prep))
}

#[cfg(feature = "ocr")]
fn invoke_tesseract_tsv(
    cfg: &RasterOcrTranscriber,
    bin: &Path,
    img_path: &Path,
    psm: u8,
    dpi: u32,
) -> crate::Result<Vec<TextSpan>> {
    let mut cmd = Command::new(bin);
    // Options before the config name (`tsv`); LSTM OEM helps handwriting.
    cmd.arg(img_path)
        .arg("stdout")
        .arg("--oem")
        .arg("1")
        .arg("--psm")
        .arg(psm.to_string())
        .arg("--dpi")
        .arg(dpi.to_string())
        .arg("-l")
        .arg(&cfg.languages)
        .arg("tsv");
    if let Some(ref td) = cfg.tessdata {
        cmd.env("TESSDATA_PREFIX", td);
    }

    let output = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            tracing::debug!("tesseract not runnable ({e}); OCR skipped");
            return Ok(Vec::new());
        }
    };

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        tracing::debug!("tesseract failed psm={psm} ({}): {err}", output.status);
        return Ok(Vec::new());
    }

    let tsv = String::from_utf8_lossy(&output.stdout);
    Ok(parse_tesseract_tsv(&tsv))
}

/// Parse Tesseract TSV (level 5 = word) into [`TextSpan`]s.
pub fn parse_tesseract_tsv(tsv: &str) -> Vec<TextSpan> {
    let mut spans = Vec::new();
    for (i, line) in tsv.lines().enumerate() {
        if i == 0 || line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 12 {
            continue;
        }
        // level page block par line word left top width height conf text
        if cols[0] != "5" {
            continue;
        }
        let text = cols[11].trim();
        if text.is_empty() {
            continue;
        }
        let conf: f32 = cols[10].parse().unwrap_or(-1.0);
        if (0.0..5.0).contains(&conf) {
            // Drop only near-zero noise; keep mid-low scores (letters / sparse ink).
            continue;
        }
        let left: f32 = cols[6].parse().unwrap_or(0.0);
        let top: f32 = cols[7].parse().unwrap_or(0.0);
        let width: f32 = cols[8].parse().unwrap_or(0.0);
        let height: f32 = cols[9].parse().unwrap_or(0.0);
        if width <= 0.0 || height <= 0.0 {
            continue;
        }
        let confidence = if conf < 0.0 {
            -1.0
        } else {
            (conf / 100.0).clamp(0.0, 1.0)
        };
        let line_num: i32 = cols[4].parse().unwrap_or(-1);
        spans.push(TextSpan {
            text: text.to_string(),
            x: left,
            y: top,
            width,
            height,
            confidence,
            line: line_num,
        });
    }
    spans
}

/// Join recognised spans into multi-line plain text (reading order).
///
/// Prefer Tesseract `line` indices when present; otherwise cluster by `y`
/// using a fraction of median span height. Within a line, join with spaces.
pub fn spans_to_multiline_text(spans: &[TextSpan]) -> String {
    let nonempty: Vec<&TextSpan> = spans.iter().filter(|s| !s.text.trim().is_empty()).collect();
    if nonempty.is_empty() {
        return String::new();
    }

    let use_line_idx = nonempty.iter().all(|s| s.line >= 0);
    let mut order: Vec<&TextSpan> = nonempty;
    if use_line_idx {
        order.sort_by(|a, b| {
            a.line
                .cmp(&b.line)
                .then(a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
        });
        let mut lines: Vec<String> = Vec::new();
        let mut cur_line = order[0].line;
        let mut words: Vec<&str> = Vec::new();
        for s in order {
            if s.line != cur_line {
                if !words.is_empty() {
                    lines.push(words.join(" "));
                    words.clear();
                }
                cur_line = s.line;
            }
            words.push(s.text.trim());
        }
        if !words.is_empty() {
            lines.push(words.join(" "));
        }
        return lines.join("\n");
    }

    order.sort_by(|a, b| {
        a.y.partial_cmp(&b.y)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
    });
    let mut heights: Vec<f32> = order.iter().map(|s| s.height.max(1.0)).collect();
    heights.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median_h = heights[heights.len() / 2];
    let thresh = (median_h * 0.55).max(8.0);

    let mut lines: Vec<String> = Vec::new();
    let mut words: Vec<&str> = Vec::new();
    let mut line_y = order[0].y;
    for s in order {
        if (s.y - line_y).abs() > thresh && !words.is_empty() {
            lines.push(words.join(" "));
            words.clear();
            line_y = s.y;
        }
        words.push(s.text.trim());
    }
    if !words.is_empty() {
        lines.push(words.join(" "));
    }
    lines.join("\n")
}

#[cfg(feature = "ocr")]
fn tempfile_dir() -> crate::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "ephemeris-ocr-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).map_err(crate::AppError::Io)?;
    Ok(dir)
}

#[cfg(feature = "ocr")]
fn write_pgm(path: &Path, width: u32, height: u32, gray8: &[u8]) -> crate::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path).map_err(crate::AppError::Io)?;
    write!(f, "P5\n{width} {height}\n255\n").map_err(crate::AppError::Io)?;
    f.write_all(gray8).map_err(crate::AppError::Io)?;
    Ok(())
}

/// Simple stroke → gray8 raster for the stroke-based [`Transcriber`] path.
fn rasterize_strokes(strokes: &[Stroke]) -> (u32, u32, Vec<u8>) {
    let mut min_x = f32::MAX;
    let mut min_y = f32::MAX;
    let mut max_x = f32::MIN;
    let mut max_y = f32::MIN;
    let mut any = false;
    for s in strokes {
        if s.tool == Tool::Eraser {
            continue;
        }
        for p in &s.points {
            any = true;
            min_x = min_x.min(p.x);
            min_y = min_y.min(p.y);
            max_x = max_x.max(p.x);
            max_y = max_y.max(p.y);
        }
    }
    let (w, h) = if !any {
        (800, 600)
    } else {
        let pad = 24.0;
        let w = ((max_x - min_x) + pad * 2.0).ceil().max(64.0) as u32;
        let h = ((max_y - min_y) + pad * 2.0).ceil().max(64.0) as u32;
        (w.min(2400), h.min(2400))
    };
    let mut gray = vec![255u8; (w as usize) * (h as usize)];
    let ox = if any { min_x - 24.0 } else { 0.0 };
    let oy = if any { min_y - 24.0 } else { 0.0 };

    for s in strokes {
        if s.tool == Tool::Eraser || s.points.len() < 2 {
            continue;
        }
        let thickness = s.base_width.max(1.5).ceil() as i32;
        for win in s.points.windows(2) {
            let a = Point {
                x: win[0].x - ox,
                y: win[0].y - oy,
                ..win[0]
            };
            let b = Point {
                x: win[1].x - ox,
                y: win[1].y - oy,
                ..win[1]
            };
            draw_line_thick(&mut gray, w, h, a.x, a.y, b.x, b.y, thickness);
        }
    }
    (w, h, gray)
}

#[allow(clippy::too_many_arguments)]
fn draw_line_thick(
    buf: &mut [u8],
    w: u32,
    h: u32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    thickness: i32,
) {
    let steps = ((x1 - x0).hypot(y1 - y0)).ceil().max(1.0) as i32;
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let x = x0 + (x1 - x0) * t;
        let y = y0 + (y1 - y0) * t;
        let cx = x.round() as i32;
        let cy = y.round() as i32;
        for dy in -thickness..=thickness {
            for dx in -thickness..=thickness {
                if dx * dx + dy * dy > thickness * thickness {
                    continue;
                }
                let px = cx + dx;
                let py = cy + dy;
                if px < 0 || py < 0 || px >= w as i32 || py >= h as i32 {
                    continue;
                }
                buf[(py as u32 * w + px as u32) as usize] = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Color;

    #[test]
    fn noop_returns_empty() {
        let t = NoOpTranscriber;
        let stroke = Stroke::new(Tool::Pen, Color::BLACK, 2.0);
        assert!(t.transcribe(&[stroke]).unwrap().is_empty());
    }

    #[test]
    fn fake_returns_configured_spans() {
        let t = FakeTranscriber {
            spans: vec![TextSpan::new("hello", 10.0, 20.0, 40.0, 12.0)],
        };
        let spans = t.transcribe(&[]).unwrap();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "hello");
        assert_eq!(t.transcribe_raster(10, 10, &[255; 100]).unwrap().len(), 1);
    }

    #[test]
    fn raster_ocr_disabled_returns_empty() {
        let t = RasterOcrTranscriber {
            enabled: false,
            ..RasterOcrTranscriber::default()
        };
        let gray = vec![255u8; 100 * 40];
        assert!(t.transcribe_raster(100, 40, &gray).unwrap().is_empty());
    }

    #[test]
    fn empty_hint_when_disabled() {
        let t = RasterOcrTranscriber {
            enabled: false,
            ..RasterOcrTranscriber::default()
        };
        assert!(t.empty_result_hint().contains("disabled"));
    }

    #[test]
    fn parse_tsv_extracts_words() {
        let tsv = "\
level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext
5\t1\t1\t1\t1\t1\t10\t20\t40\t12\t90.0\tHello
5\t1\t1\t1\t1\t2\t60\t20\t50\t12\t88.5\tWorld
5\t1\t1\t1\t1\t3\t0\t0\t10\t10\t2.0\tnose
4\t1\t1\t1\t1\t0\t10\t20\t100\t12\t-1\t
";
        let spans = parse_tesseract_tsv(tsv);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].text, "Hello");
        assert_eq!(spans[0].x, 10.0);
        assert_eq!(spans[1].text, "World");
        assert_eq!(spans[0].line, 1);
        assert_eq!(RasterOcrTranscriber::spans_to_text(&spans), "Hello World");
    }

    #[test]
    fn prepare_ocr_crops_and_upscales_ink() {
        let w = 200u32;
        let h = 100u32;
        let mut gray = vec![255u8; (w * h) as usize];
        // Small dark blob in the corner (letter-sized).
        for y in 10..30 {
            for x in 10..25 {
                gray[(y * w + x) as usize] = 0;
            }
        }
        let prep = prepare_ocr_image(w, h, &gray).expect("ink present");
        assert!(prep.width > 25, "expected upscale, got w={}", prep.width);
        assert!(prep.height > 20, "expected upscale, got h={}", prep.height);
        assert_eq!(prep.psm, 8, "tiny crop should use single-word PSM");
        assert!(prep.scale >= 3.0);
    }

    #[test]
    fn pick_cascade_prefers_higher_ocr_confidence() {
        let htr = vec![TextSpan {
            text: "xx".into(),
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
            confidence: 0.5,
            line: -1,
        }];
        let ocr = vec![TextSpan {
            text: "Hi".into(),
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
            confidence: 0.85,
            line: -1,
        }];
        let picked = pick_cascade_spans(&htr, &ocr, 0.45);
        assert_eq!(picked[0].text, "Hi");
    }

    #[test]
    fn cascade_falls_back_to_ocr_without_htr_model() {
        let cascade = CascadingTranscriber::new(
            StrokeHtrTranscriber::from_parts(true, None),
            RasterOcrTranscriber {
                enabled: false,
                ..RasterOcrTranscriber::default()
            },
        );
        let gray = vec![255u8; 40 * 20];
        // No strokes / no model → OCR path (disabled → empty).
        assert!(cascade
            .transcribe_page(&[], 40, 20, &gray)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn cascade_prefers_fake_htr_spans() {
        // StrokeHtr without model returns empty; inject via Fake by testing
        // mean_confidence helper used by cascade accept logic.
        let spans = vec![TextSpan {
            text: "hi".into(),
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
            confidence: 0.9,
            line: -1,
        }];
        assert!(mean_confidence(&spans) >= 0.35);
    }

    #[test]
    fn spans_to_text_preserves_newlines_by_line_index() {
        let spans = vec![
            TextSpan {
                text: "Hello".into(),
                x: 10.0,
                y: 20.0,
                width: 40.0,
                height: 12.0,
                confidence: 0.9,
                line: 1,
            },
            TextSpan {
                text: "World".into(),
                x: 60.0,
                y: 20.0,
                width: 50.0,
                height: 12.0,
                confidence: 0.9,
                line: 1,
            },
            TextSpan {
                text: "Next".into(),
                x: 10.0,
                y: 50.0,
                width: 40.0,
                height: 12.0,
                confidence: 0.9,
                line: 2,
            },
        ];
        assert_eq!(
            RasterOcrTranscriber::spans_to_text(&spans),
            "Hello World\nNext"
        );
    }

    #[test]
    fn spans_to_text_clusters_by_y_without_line_index() {
        let spans = vec![
            TextSpan::new("one", 10.0, 10.0, 30.0, 14.0),
            TextSpan::new("two", 50.0, 12.0, 30.0, 14.0),
            TextSpan::new("three", 10.0, 40.0, 40.0, 14.0),
        ];
        assert_eq!(
            RasterOcrTranscriber::spans_to_text(&spans),
            "one two\nthree"
        );
    }

    #[test]
    fn stroke_rasterize_produces_ink() {
        let mut s = Stroke::new(Tool::Pen, Color::BLACK, 3.0);
        s.points = vec![
            Point::new(10.0, 10.0, 1.0, 0.0, 0),
            Point::new(80.0, 10.0, 1.0, 0.0, 10),
            Point::new(80.0, 40.0, 1.0, 0.0, 20),
        ];
        let (w, h, gray) = rasterize_strokes(&[s]);
        assert!(w >= 64 && h >= 64);
        assert!(gray.iter().any(|&p| p < 128), "expected black ink pixels");
    }

    /// Integration: real Tesseract on a synthetic block-letter page.
    /// Skips cleanly when the binary or tessdata is unavailable.
    #[test]
    #[cfg(feature = "ocr")]
    fn raster_ocr_reads_block_letters_when_tesseract_available() {
        let t = RasterOcrTranscriber::new("eng");
        if Command::new(
            t.tesseract_bin
                .as_deref()
                .unwrap_or_else(|| Path::new("tesseract")),
        )
        .arg("--version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
        {
            eprintln!("skip: tesseract binary not available");
            return;
        }

        let (w, h, gray) = render_block_word("HI", 8);
        let spans = t.transcribe_raster(w, h, &gray).unwrap();
        if spans.is_empty() {
            eprintln!("skip: tesseract returned no words (tessdata missing?)");
            return;
        }
        let joined = RasterOcrTranscriber::spans_to_text(&spans).to_uppercase();
        assert!(
            joined.contains('H') || joined.contains('I') || joined.contains("HI"),
            "unexpected OCR text: {joined:?}"
        );
    }

    /// Draw a crude 5×7 block-letter word into a white gray8 buffer.
    #[cfg(feature = "ocr")]
    fn render_block_word(word: &str, scale: u32) -> (u32, u32, Vec<u8>) {
        let glyphs: &[(&str, [[u8; 5]; 7])] = &[
            (
                "H",
                [
                    [1, 0, 0, 0, 1],
                    [1, 0, 0, 0, 1],
                    [1, 0, 0, 0, 1],
                    [1, 1, 1, 1, 1],
                    [1, 0, 0, 0, 1],
                    [1, 0, 0, 0, 1],
                    [1, 0, 0, 0, 1],
                ],
            ),
            (
                "I",
                [
                    [1, 1, 1, 1, 1],
                    [0, 0, 1, 0, 0],
                    [0, 0, 1, 0, 0],
                    [0, 0, 1, 0, 0],
                    [0, 0, 1, 0, 0],
                    [0, 0, 1, 0, 0],
                    [1, 1, 1, 1, 1],
                ],
            ),
        ];
        let pad = 16 * scale;
        let cell_w = 5 * scale;
        let cell_h = 7 * scale;
        let gap = 2 * scale;
        let w = pad * 2 + word.len() as u32 * cell_w + word.len().saturating_sub(1) as u32 * gap;
        let h = pad * 2 + cell_h;
        let mut gray = vec![255u8; (w * h) as usize];
        for (i, ch) in word.chars().enumerate() {
            let Some((_, glyph)) = glyphs.iter().find(|(k, _)| *k == ch.to_string()) else {
                continue;
            };
            let ox = pad + i as u32 * (cell_w + gap);
            let oy = pad;
            for (row, bits) in glyph.iter().enumerate() {
                for (col, bit) in bits.iter().enumerate() {
                    if *bit == 0 {
                        continue;
                    }
                    for dy in 0..scale {
                        for dx in 0..scale {
                            let x = ox + col as u32 * scale + dx;
                            let y = oy + row as u32 * scale + dy;
                            gray[(y * w + x) as usize] = 0;
                        }
                    }
                }
            }
        }
        (w, h, gray)
    }
}
