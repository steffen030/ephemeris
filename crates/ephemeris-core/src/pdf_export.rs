//! PDF export of note pages (ephemeris-97x.1 / 97x.5 / ephemeris-cev).
//!
//! Renders grayscale canvas rasters (and optionally vector strokes) into a
//! multi-page PDF. When transcription spans are provided, they are embedded as
//! an invisible selectable text layer (PDF text rendering mode 3).
//!
//! Highlighter strokes are painted as a classic yellow marker multiply over the
//! RGB page so exports match highlighter-pen expectation on colour displays.
//! On mono e-ink the live canvas stays mid-gray; colour e-ink panels that can
//! show yellow would see this only if the device path rendered colour (PDF
//! viewers and colour e-ink always show the yellow export).

use crate::ink::WidthConfig;
use crate::model::{Color, Stroke, Tool};
use crate::transcriber::TextSpan;
use printpdf::{
    BuiltinFont, Color as PdfColor, Greyscale, Line, LinePoint, Mm, Op, PdfDocument, PdfFontHandle,
    PdfPage, PdfSaveOptions, Point as PdfPoint, Pt, RawImage, RawImageData, RawImageFormat, Rgb,
    TextItem, TextRenderingMode, XObjectTransform,
};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;

/// Classic highlighter-pen yellow (≈ Stabilo / Post-it marker).
pub const HIGHLIGHTER_YELLOW: Color = Color {
    r: 255,
    g: 230,
    b: 80,
    a: 255,
};

/// Gray the canvas bakes highlighter strokes into (ephemeris-ui raster).
const CANVAS_HIGHLIGHT_GRAY: f32 = 160.0;

/// Extra half-width (px) for the PDF yellow so it fully covers the anti-aliased
/// gray rim of the canvas stroke.
const HIGHLIGHT_RIM_PX: f32 = 1.5;

/// One page of raster ink (8-bpp grayscale, 0 = black, 255 = white).
#[derive(Debug, Clone)]
pub struct RasterPage {
    pub width: u32,
    pub height: u32,
    pub gray8: Vec<u8>,
}

/// Options for PDF export.
#[derive(Debug, Clone, Default)]
pub struct PdfExportOptions {
    pub title: String,
    /// Optional full-note searchable text (placed as invisible layer on page 1).
    pub searchable_text: Option<String>,
    /// Optional per-page positioned spans (from [`crate::Transcriber`]).
    pub page_spans: Vec<Vec<TextSpan>>,
    /// Optional per-page highlighter strokes. When present, the gray8 page is
    /// promoted to RGB and these strokes are multiply-blended in
    /// [`HIGHLIGHTER_YELLOW`] so exported PDFs look like a yellow marker.
    pub page_highlight_strokes: Vec<Vec<Stroke>>,
}

/// Stable content hash for incremental export skip (ephemeris-97x.2).
pub fn raster_export_hash(pages: &[RasterPage], searchable_text: Option<&str>) -> String {
    raster_export_hash_with_highlights(pages, searchable_text, &[])
}

/// Content hash including highlighter stroke geometry (yellow PDF layer).
pub fn raster_export_hash_with_highlights(
    pages: &[RasterPage],
    searchable_text: Option<&str>,
    highlight_pages: &[Vec<Stroke>],
) -> String {
    let mut hasher = DefaultHasher::new();
    for page in pages {
        page.width.hash(&mut hasher);
        page.height.hash(&mut hasher);
        page.gray8.hash(&mut hasher);
    }
    searchable_text.unwrap_or("").hash(&mut hasher);
    for strokes in highlight_pages {
        for stroke in strokes {
            stroke.base_width.to_bits().hash(&mut hasher);
            stroke.points.len().hash(&mut hasher);
            for p in &stroke.points {
                p.x.to_bits().hash(&mut hasher);
                p.y.to_bits().hash(&mut hasher);
                p.pressure.to_bits().hash(&mut hasher);
            }
        }
    }
    format!("{:016x}", hasher.finish())
}

/// Sidecar path storing the hash of the last successful PDF export.
pub fn pdf_hash_sidecar(pdf_path: &Path) -> std::path::PathBuf {
    let mut s = pdf_path.as_os_str().to_os_string();
    s.push(".ephemeris-hash");
    std::path::PathBuf::from(s)
}

/// Returns true when `out_path` already reflects `content_hash`.
pub fn pdf_export_is_current(out_path: &Path, content_hash: &str) -> bool {
    if !out_path.exists() {
        return false;
    }
    let sidecar = pdf_hash_sidecar(out_path);
    std::fs::read_to_string(sidecar)
        .map(|s| s.trim() == content_hash)
        .unwrap_or(false)
}

fn write_pdf_hash_sidecar(out_path: &Path, content_hash: &str) {
    let sidecar = pdf_hash_sidecar(out_path);
    let _ = std::fs::write(sidecar, content_hash);
}

/// Export raster pages to a PDF file. Returns the number of pages written.
///
/// When the content hash matches the previous export sidecar, returns `Ok(0)`
/// without rewriting the file (incremental skip).
///
/// When [`PdfExportOptions::page_highlight_strokes`] contains highlighter
/// strokes for a page, that page is embedded as RGB with yellow multiply marks.
pub fn export_raster_pages_to_pdf(
    pages: &[RasterPage],
    out_path: &Path,
    opts: &PdfExportOptions,
) -> crate::Result<usize> {
    if pages.is_empty() {
        return Err(crate::AppError::InvalidInput("pdf export: no pages".into()));
    }

    let hash = raster_export_hash_with_highlights(
        pages,
        opts.searchable_text.as_deref(),
        &opts.page_highlight_strokes,
    );
    if pdf_export_is_current(out_path, &hash) {
        return Ok(0);
    }

    let mut doc = PdfDocument::new(if opts.title.is_empty() {
        "Ephemeris Note"
    } else {
        &opts.title
    });

    let mut pdf_pages = Vec::with_capacity(pages.len());
    for (i, page) in pages.iter().enumerate() {
        let (page_w_mm, page_h_mm) = page_size_mm(page.width, page.height);
        let full_text = if i == 0 {
            opts.searchable_text.as_deref()
        } else {
            None
        };
        let spans = opts.page_spans.get(i).map(|s| s.as_slice()).unwrap_or(&[]);
        let highlights = opts
            .page_highlight_strokes
            .get(i)
            .map(|s| s.as_slice())
            .unwrap_or(&[]);
        let ops = build_raster_page_ops(
            &mut doc, page, page_w_mm, page_h_mm, full_text, spans, highlights,
        )?;
        pdf_pages.push(PdfPage::new(page_w_mm, page_h_mm, ops));
    }

    save_doc(&mut doc, pdf_pages, out_path)?;
    write_pdf_hash_sidecar(out_path, &hash);
    Ok(pages.len())
}

/// Export vector strokes (one `Vec<Stroke>` per page) to PDF.
pub fn export_stroke_pages_to_pdf(
    pages: &[Vec<Stroke>],
    canvas_w: f32,
    canvas_h: f32,
    out_path: &Path,
    opts: &PdfExportOptions,
) -> crate::Result<usize> {
    if pages.is_empty() {
        return Err(crate::AppError::InvalidInput("pdf export: no pages".into()));
    }
    let (page_w_mm, page_h_mm) = page_size_mm(canvas_w as u32, canvas_h as u32);
    let mut doc = PdfDocument::new(if opts.title.is_empty() {
        "Ephemeris Note"
    } else {
        &opts.title
    });

    let mut pdf_pages = Vec::with_capacity(pages.len());
    for (i, strokes) in pages.iter().enumerate() {
        let full_text = if i == 0 {
            opts.searchable_text.as_deref()
        } else {
            None
        };
        let spans = opts.page_spans.get(i).map(|s| s.as_slice()).unwrap_or(&[]);
        let ops = build_stroke_page_ops(
            strokes, canvas_w, canvas_h, page_w_mm, page_h_mm, full_text, spans,
        );
        pdf_pages.push(PdfPage::new(page_w_mm, page_h_mm, ops));
    }

    save_doc(&mut doc, pdf_pages, out_path)?;
    Ok(pages.len())
}

fn save_doc(doc: &mut PdfDocument, pages: Vec<PdfPage>, out_path: &Path) -> crate::Result<()> {
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).map_err(crate::AppError::Io)?;
    }
    let mut warnings = Vec::new();
    let bytes = doc
        .with_pages(pages)
        .save(&PdfSaveOptions::default(), &mut warnings);
    std::fs::write(out_path, bytes).map_err(crate::AppError::Io)?;
    Ok(())
}

fn page_size_mm(width_px: u32, height_px: u32) -> (Mm, Mm) {
    // Fit the longer edge to 210mm (A4 width), preserve aspect.
    let w = width_px.max(1) as f32;
    let h = height_px.max(1) as f32;
    if w >= h {
        let page_w = 210.0;
        let page_h = page_w * (h / w);
        (Mm(page_w), Mm(page_h))
    } else {
        let page_h = 297.0;
        let page_w = page_h * (w / h);
        (Mm(page_w), Mm(page_h))
    }
}

fn build_raster_page_ops(
    doc: &mut PdfDocument,
    raster: &RasterPage,
    page_w_mm: Mm,
    page_h_mm: Mm,
    full_text: Option<&str>,
    spans: &[TextSpan],
    highlight_strokes: &[Stroke],
) -> crate::Result<Vec<Op>> {
    let expected = (raster.width as usize).saturating_mul(raster.height as usize);
    if raster.gray8.len() < expected {
        return Err(crate::AppError::InvalidInput(format!(
            "pdf export: gray buffer too short ({} < {expected})",
            raster.gray8.len()
        )));
    }

    let highlights: Vec<&Stroke> = highlight_strokes
        .iter()
        .filter(|s| s.tool == Tool::Highlighter && !s.points.is_empty())
        .collect();

    let image = if highlights.is_empty() {
        RawImage {
            pixels: RawImageData::U8(raster.gray8[..expected].to_vec()),
            width: raster.width as usize,
            height: raster.height as usize,
            data_format: RawImageFormat::R8,
            tag: Vec::new(),
        }
    } else {
        let rgb = gray8_to_rgb_with_yellow_highlights(
            &raster.gray8[..expected],
            raster.width,
            raster.height,
            &highlights,
        );
        RawImage {
            pixels: RawImageData::U8(rgb),
            width: raster.width as usize,
            height: raster.height as usize,
            data_format: RawImageFormat::RGB8,
            tag: Vec::new(),
        }
    };
    let image_id = doc.add_image(&image);

    // dpi such that image width in points equals page width in points.
    let dpi = (raster.width.max(1) as f32) * 25.4 / page_w_mm.0.max(0.1);
    let mut ops = vec![Op::UseXobject {
        id: image_id,
        transform: XObjectTransform {
            translate_x: Some(Pt(0.0)),
            translate_y: Some(Pt(0.0)),
            dpi: Some(dpi),
            ..XObjectTransform::default()
        },
    }];

    ops.extend(embed_text_ops(
        page_w_mm,
        page_h_mm,
        raster.width as f32,
        raster.height as f32,
        full_text,
        spans,
    ));
    Ok(ops)
}

/// Promote gray8 → RGB and multiply-blend highlighter coverage in yellow.
///
/// Live canvas stays mid-gray for mono e-ink; PDF (and colour viewers /
/// colour e-ink) get a typical yellow marker look while black ink stays dark
/// under the multiply.
pub fn gray8_to_rgb_with_yellow_highlights(
    gray8: &[u8],
    width: u32,
    height: u32,
    highlights: &[&Stroke],
) -> Vec<u8> {
    let n = (width as usize).saturating_mul(height as usize);
    let mut rgb = Vec::with_capacity(n.saturating_mul(3));
    for &g in gray8.iter().take(n) {
        rgb.push(g);
        rgb.push(g);
        rgb.push(g);
    }
    if highlights.is_empty() || width == 0 || height == 0 {
        return rgb;
    }

    let mut coverage = vec![0.0f32; n];
    for stroke in highlights {
        stamp_highlighter_coverage(&mut coverage, width, height, stroke);
    }

    let yellow = [
        HIGHLIGHTER_YELLOW.r as f32 / 255.0,
        HIGHLIGHTER_YELLOW.g as f32 / 255.0,
        HIGHLIGHTER_YELLOW.b as f32 / 255.0,
    ];
    for (i, cov) in coverage.iter().copied().enumerate() {
        if cov <= 0.0 {
            continue;
        }
        let c = cov.clamp(0.0, 1.0);
        let o = i * 3;
        // The canvas already holds the highlighter as gray 160; multiplying
        // yellow over it gives a muddy olive with a gray rim. Lift that gray
        // back to paper white first (black ink stays black), then multiply.
        let g = gray8[i] as f32;
        let lifted = (g * 255.0 / CANVAS_HIGHLIGHT_GRAY).min(255.0);
        let base = g + (lifted - g) * c;
        for (ch, y) in yellow.iter().enumerate() {
            let f = 1.0 - c + c * y;
            rgb[o + ch] = (base * f).round().clamp(0.0, 255.0) as u8;
        }
    }
    rgb
}

fn stamp_highlighter_coverage(coverage: &mut [f32], width: u32, height: u32, stroke: &Stroke) {
    let points = &stroke.points;
    if points.is_empty() {
        return;
    }
    if points.len() == 1 {
        let p = &points[0];
        stamp_capsule_coverage(
            coverage,
            width,
            height,
            p.x,
            p.y,
            half_width(stroke.base_width, p.pressure),
            p.x,
            p.y,
            half_width(stroke.base_width, p.pressure),
        );
        return;
    }
    for w in points.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        stamp_capsule_coverage(
            coverage,
            width,
            height,
            a.x,
            a.y,
            half_width(stroke.base_width, a.pressure),
            b.x,
            b.y,
            half_width(stroke.base_width, b.pressure),
        );
    }
}

/// Same width curve as the canvas raster, widened to cover its AA rim.
#[inline]
fn half_width(base_width: f32, pressure: f32) -> f32 {
    WidthConfig::default().width_for(base_width, pressure) * 0.5 + HIGHLIGHT_RIM_PX
}

#[allow(clippy::too_many_arguments)]
fn stamp_capsule_coverage(
    coverage: &mut [f32],
    width: u32,
    height: u32,
    ax: f32,
    ay: f32,
    hw_a: f32,
    bx: f32,
    by: f32,
    hw_b: f32,
) {
    let pad = hw_a.max(hw_b) + 1.0;
    let min_x = (ax.min(bx) - pad).floor().max(0.0) as i64;
    let min_y = (ay.min(by) - pad).floor().max(0.0) as i64;
    let max_x = ((ax.max(bx) + pad).ceil() as i64).min(width as i64 - 1);
    let max_y = ((ay.max(by) + pad).ceil() as i64).min(height as i64 - 1);
    if max_x < min_x || max_y < min_y {
        return;
    }
    let dx = bx - ax;
    let dy = by - ay;
    let len_sq = dx * dx + dy * dy;
    let inv_len_sq = if len_sq < 1e-8 { 0.0 } else { 1.0 / len_sq };
    let stride = width as usize;

    for py in min_y..=max_y {
        let cy = py as f32 + 0.5;
        let row = py as usize * stride;
        for px in min_x..=max_x {
            let cx = px as f32 + 0.5;
            let t = (((cx - ax) * dx + (cy - ay) * dy) * inv_len_sq).clamp(0.0, 1.0);
            let qx = ax + t * dx;
            let qy = ay + t * dy;
            let dist = ((cx - qx).powi(2) + (cy - qy).powi(2)).sqrt();
            let hw = hw_a + (hw_b - hw_a) * t;
            let cov = (hw + 0.5 - dist).clamp(0.0, 1.0);
            if cov > 0.0 {
                let i = row + px as usize;
                coverage[i] = coverage[i].max(cov);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_stroke_page_ops(
    strokes: &[Stroke],
    canvas_w: f32,
    canvas_h: f32,
    page_w_mm: Mm,
    page_h_mm: Mm,
    full_text: Option<&str>,
    spans: &[TextSpan],
) -> Vec<Op> {
    let sx = page_w_mm.0 / canvas_w.max(1.0);
    let sy = page_h_mm.0 / canvas_h.max(1.0);
    let mut ops = Vec::new();

    for stroke in strokes {
        if stroke.tool == Tool::Eraser || stroke.points.len() < 2 {
            continue;
        }
        let points: Vec<LinePoint> = stroke
            .points
            .iter()
            .map(|p| {
                // PDF y grows upward; canvas y grows downward.
                let x = Mm(p.x * sx);
                let y = Mm((canvas_h - p.y) * sy);
                LinePoint {
                    p: PdfPoint::new(x, y),
                    bezier: false,
                }
            })
            .collect();
        let (r, g, b) = stroke_pdf_rgb(stroke);
        let width_pt = Pt(stroke.base_width.max(0.5) * sx * 2.83465); // mm→approx via scale
        ops.push(Op::SetOutlineColor {
            col: PdfColor::Rgb(Rgb::new(r, g, b, None)),
        });
        ops.push(Op::SetOutlineThickness { pt: width_pt });
        ops.push(Op::DrawLine {
            line: Line {
                points,
                is_closed: false,
            },
        });
    }

    ops.extend(embed_text_ops(
        page_w_mm, page_h_mm, canvas_w, canvas_h, full_text, spans,
    ));
    ops
}

fn stroke_pdf_rgb(stroke: &Stroke) -> (f32, f32, f32) {
    if stroke.tool == Tool::Highlighter {
        return color_to_rgb(HIGHLIGHTER_YELLOW);
    }
    color_to_rgb(stroke.color)
}

fn embed_text_ops(
    page_w_mm: Mm,
    page_h_mm: Mm,
    canvas_w: f32,
    canvas_h: f32,
    full_text: Option<&str>,
    spans: &[TextSpan],
) -> Vec<Op> {
    let sx = page_w_mm.0 / canvas_w.max(1.0);
    let sy = page_h_mm.0 / canvas_h.max(1.0);
    let mut ops = vec![
        Op::StartTextSection,
        Op::SetTextRenderingMode {
            mode: TextRenderingMode::Invisible,
        },
        Op::SetFillColor {
            col: PdfColor::Greyscale(Greyscale::new(0.0, None)),
        },
        Op::SetFont {
            font: PdfFontHandle::Builtin(BuiltinFont::Helvetica),
            size: Pt(8.0),
        },
        Op::SetLineHeight { lh: Pt(8.0) },
    ];

    if let Some(text) = full_text.map(str::trim).filter(|s| !s.is_empty()) {
        // Lay out as wrapped lines near the top so the whole note is searchable.
        let mut y = page_h_mm.0 - 8.0;
        for line in text.lines() {
            if y < 4.0 {
                break;
            }
            ops.push(Op::SetTextCursor {
                pos: PdfPoint::new(Mm(4.0), Mm(y)),
            });
            ops.push(Op::SetFont {
                font: PdfFontHandle::Builtin(BuiltinFont::Helvetica),
                size: Pt(8.0),
            });
            ops.push(Op::ShowText {
                items: vec![TextItem::Text(line.to_string())],
            });
            y -= 4.0;
        }
    }

    for span in spans {
        if span.text.trim().is_empty() {
            continue;
        }
        let x = Mm(span.x * sx);
        let y = Mm((canvas_h - span.y - span.height) * sy);
        let font_size = (span.height * sy * 2.83465).clamp(6.0, 24.0);
        ops.push(Op::SetTextCursor {
            pos: PdfPoint::new(x, y),
        });
        ops.push(Op::SetFont {
            font: PdfFontHandle::Builtin(BuiltinFont::Helvetica),
            size: Pt(font_size),
        });
        ops.push(Op::ShowText {
            items: vec![TextItem::Text(span.text.clone())],
        });
    }

    ops.push(Op::EndTextSection);
    ops
}

fn color_to_rgb(c: Color) -> (f32, f32, f32) {
    (c.r as f32 / 255.0, c.g as f32 / 255.0, c.b as f32 / 255.0)
}

/// True when the PDF content uses invisible text rendering mode (Tr 3).
pub fn pdf_has_invisible_text_mode(pdf_bytes: &[u8]) -> bool {
    // printpdf emits `3 Tr` when TextRenderingMode::Invisible is set.
    pdf_bytes.windows(4).any(|w| w == b"3 Tr")
}

/// True when the PDF content stream paints text (`Tj` / `TJ` operators).
pub fn pdf_has_text_show_ops(pdf_bytes: &[u8]) -> bool {
    pdf_bytes.windows(2).any(|w| w == b"Tj") || pdf_bytes.windows(2).any(|w| w == b"TJ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Color, Point, Tool};
    use crate::transcriber::TextSpan;
    use tempfile::TempDir;

    fn blank_page(w: u32, h: u32) -> RasterPage {
        RasterPage {
            width: w,
            height: h,
            gray8: vec![255u8; (w * h) as usize],
        }
    }

    #[test]
    fn export_raster_writes_valid_pdf_header() {
        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("note.pdf");
        let pages = vec![blank_page(100, 80)];
        let n = export_raster_pages_to_pdf(
            &pages,
            &out,
            &PdfExportOptions {
                title: "Test".into(),
                searchable_text: Some("machine readable hello".into()),
                page_spans: vec![],
                page_highlight_strokes: vec![],
            },
        )
        .unwrap();
        assert_eq!(n, 1);
        let bytes = std::fs::read(&out).unwrap();
        assert!(bytes.starts_with(b"%PDF"));
        assert!(
            pdf_has_invisible_text_mode(&bytes),
            "expected invisible text rendering mode (3 Tr)"
        );
        assert!(
            pdf_has_text_show_ops(&bytes),
            "expected text show operators for searchable layer"
        );
    }

    #[test]
    fn export_with_text_spans_embeds_words() {
        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("spans.pdf");
        let pages = vec![blank_page(200, 200)];
        let spans = vec![vec![TextSpan::new("ephemeris", 20.0, 40.0, 80.0, 14.0)]];
        export_raster_pages_to_pdf(
            &pages,
            &out,
            &PdfExportOptions {
                title: "Spans".into(),
                searchable_text: None,
                page_spans: spans,
                page_highlight_strokes: vec![],
            },
        )
        .unwrap();
        let bytes = std::fs::read(&out).unwrap();
        assert!(pdf_has_invisible_text_mode(&bytes));
        assert!(pdf_has_text_show_ops(&bytes));
    }

    #[test]
    fn export_strokes_produces_pdf() {
        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("strokes.pdf");
        let mut stroke = Stroke::new(Tool::Pen, Color::BLACK, 2.0);
        stroke.points = vec![
            Point::new(10.0, 10.0, 1.0, 0.0, 0),
            Point::new(50.0, 40.0, 1.0, 0.0, 10),
            Point::new(90.0, 20.0, 1.0, 0.0, 20),
        ];
        export_stroke_pages_to_pdf(
            &[vec![stroke]],
            200.0,
            200.0,
            &out,
            &PdfExportOptions::default(),
        )
        .unwrap();
        let bytes = std::fs::read(&out).unwrap();
        assert!(bytes.starts_with(b"%PDF"));
    }

    #[test]
    fn yellow_highlight_tint_preserves_black_ink() {
        let mut page = blank_page(40, 20);
        // Simulate mid-gray highlighter already baked into gray8 + black ink.
        for x in 5..35 {
            for y in 8..12 {
                page.gray8[(y * 40 + x) as usize] = 160;
            }
        }
        page.gray8[(10 * 40 + 20) as usize] = 0; // black pen on top

        let mut hi = Stroke::new(Tool::Highlighter, Color::BLACK, 8.0);
        hi.points = vec![
            Point::new(5.0, 10.0, 1.0, 0.0, 0),
            Point::new(35.0, 10.0, 1.0, 0.0, 10),
        ];
        let rgb = gray8_to_rgb_with_yellow_highlights(&page.gray8, page.width, page.height, &[&hi]);
        // White far from stroke stays white.
        assert_eq!(&rgb[0..3], &[255, 255, 255]);
        // Black ink pixel stays near-black under multiply.
        let bi = ((10 * 40 + 20) * 3) as usize;
        assert!(rgb[bi] < 40 && rgb[bi + 1] < 40 && rgb[bi + 2] < 40);
        // A highlighted white-ish neighbour picks up yellow (G and B drop vs R).
        let yi = ((10 * 40 + 10) * 3) as usize;
        assert!(rgb[yi] > rgb[yi + 2], "yellow: R should exceed B");
        assert!(rgb[yi + 1] > rgb[yi + 2], "yellow: G should exceed B");
        // Baked canvas gray is lifted: marker yellow, not olive.
        assert_eq!(
            &rgb[yi..yi + 3],
            &[
                HIGHLIGHTER_YELLOW.r,
                HIGHLIGHTER_YELLOW.g,
                HIGHLIGHTER_YELLOW.b
            ]
        );
    }

    #[test]
    fn export_with_highlights_writes_rgb_pdf() {
        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("hi.pdf");
        let pages = vec![blank_page(60, 40)];
        let mut hi = Stroke::new(Tool::Highlighter, Color::BLACK, 10.0);
        hi.points = vec![
            Point::new(5.0, 20.0, 1.0, 0.0, 0),
            Point::new(55.0, 20.0, 1.0, 0.0, 10),
        ];
        let n = export_raster_pages_to_pdf(
            &pages,
            &out,
            &PdfExportOptions {
                title: "Hi".into(),
                searchable_text: None,
                page_spans: vec![],
                page_highlight_strokes: vec![vec![hi]],
            },
        )
        .unwrap();
        assert_eq!(n, 1);
        let bytes = std::fs::read(&out).unwrap();
        assert!(bytes.starts_with(b"%PDF"));
        // DeviceRGB / colour space should appear for the yellow page.
        let as_str = String::from_utf8_lossy(&bytes);
        assert!(
            as_str.contains("DeviceRGB")
                || as_str.contains("/CS")
                || bytes.windows(3).any(|w| w == b"RGB"),
            "expected colour PDF embedding for yellow highlighter"
        );
    }

    #[test]
    fn incremental_export_skips_unchanged() {
        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("incr.pdf");
        let pages = vec![blank_page(40, 30)];
        let opts = PdfExportOptions {
            title: "Incr".into(),
            searchable_text: Some("stable".into()),
            page_spans: vec![],
            page_highlight_strokes: vec![],
        };
        assert_eq!(export_raster_pages_to_pdf(&pages, &out, &opts).unwrap(), 1);
        let first = std::fs::metadata(&out).unwrap().modified().unwrap();
        assert_eq!(export_raster_pages_to_pdf(&pages, &out, &opts).unwrap(), 0);
        let second = std::fs::metadata(&out).unwrap().modified().unwrap();
        assert_eq!(first, second, "unchanged export must not rewrite PDF");

        let mut dirty = pages.clone();
        dirty[0].gray8[0] = 0;
        assert_eq!(export_raster_pages_to_pdf(&dirty, &out, &opts).unwrap(), 1);
    }
}
