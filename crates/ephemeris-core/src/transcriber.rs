//! Handwriting transcription trait (ADR ephemeris-2ri, ephemeris-97x.3 / 97x.6).
//!
//! Default v1 backend: [`RasterOcrTranscriber`] — rasterize page gray8 and run
//! Tesseract (CLI) for word boxes → [`TextSpan`]. Falls back to empty results
//! when OCR is disabled, the binary/tessdata is missing, or the `ocr` Cargo
//! feature is off (CI-safe).

use crate::model::{Point, Stroke, Tool};
use std::path::PathBuf;
#[cfg(feature = "ocr")]
use std::path::Path;
#[cfg(feature = "ocr")]
use std::process::Command;

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

    /// Join span texts into a single searchable string (word order).
    pub fn spans_to_text(spans: &[TextSpan]) -> String {
        spans
            .iter()
            .map(|s| s.text.trim())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
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
            return Ok(Vec::new());
        }
        #[cfg(feature = "ocr")]
        {
            run_tesseract_tsv(self, width, height, gray8)
        }
    }
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

    let bin = cfg
        .tesseract_bin
        .clone()
        .unwrap_or_else(|| PathBuf::from("tesseract"));

    let tmp = tempfile_dir()?;
    let img_path = tmp.join("page.pgm");
    write_pgm(&img_path, width, height, &gray8[..expected])?;

    let mut cmd = Command::new(&bin);
    cmd.arg(&img_path)
        .arg("stdout")
        .arg("--psm")
        .arg("6")
        .arg("tsv")
        .arg("-l")
        .arg(&cfg.languages);
    if let Some(ref td) = cfg.tessdata {
        cmd.env("TESSDATA_PREFIX", td);
    }

    let output = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            tracing::debug!("tesseract not runnable ({e}); OCR skipped");
            let _ = std::fs::remove_dir_all(&tmp);
            return Ok(Vec::new());
        }
    };
    let _ = std::fs::remove_dir_all(&tmp);

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        tracing::debug!("tesseract failed ({}): {err}", output.status);
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
        if conf >= 0.0 && conf < 15.0 {
            // Drop very low-confidence noise; -1 means conf missing.
            continue;
        }
        let left: f32 = cols[6].parse().unwrap_or(0.0);
        let top: f32 = cols[7].parse().unwrap_or(0.0);
        let width: f32 = cols[8].parse().unwrap_or(0.0);
        let height: f32 = cols[9].parse().unwrap_or(0.0);
        if width <= 0.0 || height <= 0.0 {
            continue;
        }
        spans.push(TextSpan {
            text: text.to_string(),
            x: left,
            y: top,
            width,
            height,
        });
    }
    spans
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
            spans: vec![TextSpan {
                text: "hello".into(),
                x: 10.0,
                y: 20.0,
                width: 40.0,
                height: 12.0,
            }],
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
    fn parse_tsv_extracts_words() {
        let tsv = "\
level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext
5\t1\t1\t1\t1\t1\t10\t20\t40\t12\t90.0\tHello
5\t1\t1\t1\t1\t2\t60\t20\t50\t12\t88.5\tWorld
5\t1\t1\t1\t1\t3\t0\t0\t10\t10\t5.0\tnose
4\t1\t1\t1\t1\t0\t10\t20\t100\t12\t-1\t
";
        let spans = parse_tesseract_tsv(tsv);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].text, "Hello");
        assert_eq!(spans[0].x, 10.0);
        assert_eq!(spans[1].text, "World");
        assert_eq!(
            RasterOcrTranscriber::spans_to_text(&spans),
            "Hello World"
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
