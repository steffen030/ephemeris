//! PDF export of note pages (ephemeris-97x.1 / 97x.5).
//!
//! Renders grayscale canvas rasters (and optionally vector strokes) into a
//! multi-page PDF. When transcription spans are provided, they are embedded as
//! an invisible selectable text layer (PDF text rendering mode 3).

use crate::model::{Color, Stroke, Tool};
use crate::transcriber::TextSpan;
use printpdf::{
    BuiltinFont, Color as PdfColor, ColorBits, ColorSpace, Greyscale, Image, ImageTransform,
    ImageXObject, Line, Mm, PdfDocument, PdfDocumentReference, PdfLayerReference, PdfPageIndex,
    Point as PdfPoint, Pt, Px, Rgb, TextRenderingMode,
};
use std::collections::hash_map::DefaultHasher;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::BufWriter;
use std::path::Path;

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
}

/// Stable content hash for incremental export skip (ephemeris-97x.2).
pub fn raster_export_hash(pages: &[RasterPage], searchable_text: Option<&str>) -> String {
    let mut hasher = DefaultHasher::new();
    for page in pages {
        page.width.hash(&mut hasher);
        page.height.hash(&mut hasher);
        page.gray8.hash(&mut hasher);
    }
    searchable_text.unwrap_or("").hash(&mut hasher);
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
pub fn export_raster_pages_to_pdf(
    pages: &[RasterPage],
    out_path: &Path,
    opts: &PdfExportOptions,
) -> crate::Result<usize> {
    if pages.is_empty() {
        return Err(crate::AppError::InvalidInput("pdf export: no pages".into()));
    }

    let hash = raster_export_hash(pages, opts.searchable_text.as_deref());
    if pdf_export_is_current(out_path, &hash) {
        return Ok(0);
    }

    let (page_w_mm, page_h_mm) = page_size_mm(pages[0].width, pages[0].height);
    let (doc, page1, layer1) = PdfDocument::new(
        if opts.title.is_empty() {
            "Ephemeris Note"
        } else {
            &opts.title
        },
        page_w_mm,
        page_h_mm,
        "Page 1",
    );

    write_raster_page(
        &doc,
        page1,
        layer1,
        &pages[0],
        page_w_mm,
        page_h_mm,
        opts.searchable_text.as_deref(),
        opts.page_spans.first().map(|s| s.as_slice()).unwrap_or(&[]),
    )?;

    for (i, page) in pages.iter().enumerate().skip(1) {
        let (pw, ph) = page_size_mm(page.width, page.height);
        let (pdf_page, layer) = doc.add_page(pw, ph, format!("Page {}", i + 1));
        let spans = opts.page_spans.get(i).map(|s| s.as_slice()).unwrap_or(&[]);
        write_raster_page(&doc, pdf_page, layer, page, pw, ph, None, spans)?;
    }

    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).map_err(crate::AppError::Io)?;
    }
    let file = File::create(out_path).map_err(crate::AppError::Io)?;
    doc.save(&mut BufWriter::new(file))
        .map_err(|e| crate::AppError::Storage(format!("pdf save: {e}")))?;
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
    let (doc, page1, layer1) = PdfDocument::new(
        if opts.title.is_empty() {
            "Ephemeris Note"
        } else {
            &opts.title
        },
        page_w_mm,
        page_h_mm,
        "Page 1",
    );

    draw_stroke_page(
        &doc,
        page1,
        layer1,
        &pages[0],
        canvas_w,
        canvas_h,
        page_w_mm,
        page_h_mm,
        opts.searchable_text.as_deref(),
        opts.page_spans.first().map(|s| s.as_slice()).unwrap_or(&[]),
    )?;

    for (i, strokes) in pages.iter().enumerate().skip(1) {
        let (pdf_page, layer) = doc.add_page(page_w_mm, page_h_mm, format!("Page {}", i + 1));
        let spans = opts.page_spans.get(i).map(|s| s.as_slice()).unwrap_or(&[]);
        draw_stroke_page(
            &doc, pdf_page, layer, strokes, canvas_w, canvas_h, page_w_mm, page_h_mm, None, spans,
        )?;
    }

    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).map_err(crate::AppError::Io)?;
    }
    let file = File::create(out_path).map_err(crate::AppError::Io)?;
    doc.save(&mut BufWriter::new(file))
        .map_err(|e| crate::AppError::Storage(format!("pdf save: {e}")))?;
    Ok(pages.len())
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

#[allow(clippy::too_many_arguments)]
fn write_raster_page(
    doc: &PdfDocumentReference,
    page: PdfPageIndex,
    layer_idx: printpdf::PdfLayerIndex,
    raster: &RasterPage,
    page_w_mm: Mm,
    page_h_mm: Mm,
    full_text: Option<&str>,
    spans: &[TextSpan],
) -> crate::Result<()> {
    let layer = doc.get_page(page).get_layer(layer_idx);

    let expected = (raster.width as usize).saturating_mul(raster.height as usize);
    if raster.gray8.len() < expected {
        return Err(crate::AppError::InvalidInput(format!(
            "pdf export: gray buffer too short ({} < {expected})",
            raster.gray8.len()
        )));
    }
    let xobj = ImageXObject {
        width: Px(raster.width as usize),
        height: Px(raster.height as usize),
        color_space: ColorSpace::Greyscale,
        bits_per_component: ColorBits::Bit8,
        interpolate: true,
        image_data: raster.gray8[..expected].to_vec(),
        image_filter: None,
        smask: None,
        clipping_bbox: None,
    };
    // dpi such that image width in points equals page width in points.
    let dpi = (raster.width.max(1) as f32) * 25.4 / page_w_mm.0.max(0.1);
    Image::from(xobj).add_to_layer(
        layer.clone(),
        ImageTransform {
            translate_x: Some(Mm(0.0)),
            translate_y: Some(Mm(0.0)),
            dpi: Some(dpi),
            ..ImageTransform::default()
        },
    );

    embed_text_layer(
        doc,
        &layer,
        page_w_mm,
        page_h_mm,
        raster.width as f32,
        raster.height as f32,
        full_text,
        spans,
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn draw_stroke_page(
    doc: &PdfDocumentReference,
    page: PdfPageIndex,
    layer_idx: printpdf::PdfLayerIndex,
    strokes: &[Stroke],
    canvas_w: f32,
    canvas_h: f32,
    page_w_mm: Mm,
    page_h_mm: Mm,
    full_text: Option<&str>,
    spans: &[TextSpan],
) -> crate::Result<()> {
    let layer = doc.get_page(page).get_layer(layer_idx);
    let sx = page_w_mm.0 / canvas_w.max(1.0);
    let sy = page_h_mm.0 / canvas_h.max(1.0);

    for stroke in strokes {
        if stroke.tool == Tool::Eraser || stroke.points.len() < 2 {
            continue;
        }
        let points: Vec<(PdfPoint, bool)> = stroke
            .points
            .iter()
            .enumerate()
            .map(|(i, p)| {
                // PDF y grows upward; canvas y grows downward.
                let x = Mm(p.x * sx);
                let y = Mm((canvas_h - p.y) * sy);
                (PdfPoint::new(x, y), i == 0)
            })
            .collect();
        let line = Line {
            points,
            is_closed: false,
        };
        let (r, g, b) = color_to_rgb(stroke.color);
        layer.set_outline_color(PdfColor::Rgb(Rgb::new(r, g, b, None)));
        let width_pt = Pt(stroke.base_width.max(0.5) * sx * 2.83465); // mm→approx via scale
        layer.set_outline_thickness(width_pt.0);
        layer.add_line(line);
    }

    embed_text_layer(
        doc, &layer, page_w_mm, page_h_mm, canvas_w, canvas_h, full_text, spans,
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn embed_text_layer(
    doc: &PdfDocumentReference,
    layer: &PdfLayerReference,
    page_w_mm: Mm,
    page_h_mm: Mm,
    canvas_w: f32,
    canvas_h: f32,
    full_text: Option<&str>,
    spans: &[TextSpan],
) -> crate::Result<()> {
    let font = doc
        .add_builtin_font(BuiltinFont::Helvetica)
        .map_err(|e| crate::AppError::Storage(format!("pdf font: {e}")))?;

    layer.set_text_rendering_mode(TextRenderingMode::Invisible);
    layer.set_fill_color(PdfColor::Greyscale(Greyscale::new(0.0, None)));

    let sx = page_w_mm.0 / canvas_w.max(1.0);
    let sy = page_h_mm.0 / canvas_h.max(1.0);

    if let Some(text) = full_text.map(str::trim).filter(|s| !s.is_empty()) {
        // Lay out as wrapped lines near the top so the whole note is searchable.
        let mut y = page_h_mm.0 - 8.0;
        for line in text.lines() {
            if y < 4.0 {
                break;
            }
            layer.use_text(line, 8.0, Mm(4.0), Mm(y), &font);
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
        layer.use_text(&span.text, font_size, x, y, &font);
    }

    // Restore normal rendering for any subsequent drawing (none expected).
    layer.set_text_rendering_mode(TextRenderingMode::Fill);
    Ok(())
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
        let spans = vec![vec![TextSpan {
            text: "ephemeris".into(),
            x: 20.0,
            y: 40.0,
            width: 80.0,
            height: 14.0,
        }]];
        export_raster_pages_to_pdf(
            &pages,
            &out,
            &PdfExportOptions {
                title: "Spans".into(),
                searchable_text: None,
                page_spans: spans,
            },
        )
        .unwrap();
        let bytes = std::fs::read(&out).unwrap();
        assert!(pdf_has_invisible_text_mode(&bytes));
        assert!(pdf_has_text_show_ops(&bytes));
    }

    #[test]
    fn export_strokes_produces_pdf() {
        use crate::model::{Color, Point, Tool};
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
    fn incremental_export_skips_unchanged() {
        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("incr.pdf");
        let pages = vec![blank_page(40, 30)];
        let opts = PdfExportOptions {
            title: "Incr".into(),
            searchable_text: Some("stable".into()),
            page_spans: vec![],
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
