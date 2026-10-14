//! Vector stroke → gray8 rendering for line HTR / Tesseract (ephemeris-ktg.10).
//!
//! Renders each [`StrokeLine`] (or a full page of strokes) directly at the
//! target resolution — no screen raster, no nearest-neighbour upscale.
//!
//! # DE·HTR v2 polarity / normalisation
//!
//! Reference: [naeyn/de-htr-web-v2](https://huggingface.co/naeyn/de-htr-web-v2)
//!
//! - **Polarity**: dark ink on light paper (white ≈ 255 / 1.0).
//! - **Norm**: feed float32 in `[0, 1]` as `pixel / 255`; **no** ImageNet mean/std
//!   (v2 normalises inside the model). Do **not** invert.
//! - **Geometry**: height 64, aspect preserved, width right-padded to a multiple
//!   of 4 (cap 1024).

use crate::line_htr::{
    ImageLineBand, LineHtrInput, LINE_HTR_HEIGHT, LINE_HTR_MAX_WIDTH, LINE_HTR_WIDTH_ALIGN,
};
use crate::model::{Stroke, Tool};
use crate::stroke_htr::{is_text_ink, stroke_bounds, StrokeLine};

/// Preset for DE·HTR v2-class line models.
pub const HTR_LINE_STROKE_WIDTH_PX: f32 = 1.75;
/// Vertical / horizontal pad inside the 64-px HTR strip (source space after scale).
pub const HTR_LINE_PAD_PX: f32 = 4.0;

/// Preset for Tesseract-oriented page / line raster (~300 DPI feel, thicker ink).
pub const OCR_STROKE_WIDTH_PX: f32 = 3.5;
pub const OCR_TARGET_LINE_HEIGHT_PX: f32 = 48.0;

/// Render parameters for anti-aliased stroke rasterisation.
#[derive(Debug, Clone, Copy)]
pub struct StrokeRenderParams {
    /// Output image height (fixed for HTR; derived for OCR).
    pub target_height: u32,
    /// Maximum output width (0 = uncapped aside from safety).
    pub max_width: u32,
    /// Pad around the ink bbox in **output** pixels.
    pub pad_px: f32,
    /// Stroke radius in **output** pixels.
    pub stroke_width_px: f32,
    /// When true, use coverage AA; otherwise binary disks.
    pub anti_alias: bool,
}

impl StrokeRenderParams {
    /// Height-64 AA strip for DE·HTR v2.
    pub fn for_line_htr() -> Self {
        Self {
            target_height: LINE_HTR_HEIGHT,
            max_width: LINE_HTR_MAX_WIDTH,
            pad_px: HTR_LINE_PAD_PX,
            stroke_width_px: HTR_LINE_STROKE_WIDTH_PX,
            anti_alias: true,
        }
    }

    /// Thicker AA render sized so the line's ink height ≈ [`OCR_TARGET_LINE_HEIGHT_PX`].
    pub fn for_ocr_line(line_height: f32) -> Self {
        let h = line_height.max(8.0);
        let scale = OCR_TARGET_LINE_HEIGHT_PX / h;
        let target = ((h * scale) + 2.0 * 6.0).round().clamp(32.0, 160.0) as u32;
        Self {
            target_height: target,
            max_width: 3200,
            pad_px: 6.0,
            stroke_width_px: OCR_STROKE_WIDTH_PX,
            anti_alias: true,
        }
    }
}

/// Rasterise one [`StrokeLine`] at `params.target_height` (dark-on-light gray8).
pub fn render_stroke_line(line: &StrokeLine, params: &StrokeRenderParams) -> (u32, u32, Vec<u8>) {
    render_strokes_in_bbox(
        &line.strokes,
        line.x,
        line.y,
        line.width,
        line.height,
        params,
    )
}

/// Rasterise a page of strokes into a tight padded gray8 (OCR / eval fallback).
pub fn render_strokes_page(strokes: &[Stroke], params: &StrokeRenderParams) -> (u32, u32, Vec<u8>) {
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    let mut any = false;
    let strokes: Vec<Stroke> = strokes.iter().filter(|s| is_text_ink(s)).cloned().collect();
    for s in &strokes {
        if let Some((x0, y0, x1, y1)) = stroke_bounds(s) {
            any = true;
            min_x = min_x.min(x0);
            min_y = min_y.min(y0);
            max_x = max_x.max(x1);
            max_y = max_y.max(y1);
        }
    }
    if !any {
        let w = 64u32;
        let h = params.target_height.max(64);
        return (w, h, vec![255u8; (w * h) as usize]);
    }
    render_strokes_in_bbox(
        &strokes,
        min_x,
        min_y,
        (max_x - min_x).max(1.0),
        (max_y - min_y).max(1.0),
        params,
    )
}

fn render_strokes_in_bbox(
    strokes: &[Stroke],
    bx: f32,
    by: f32,
    bw: f32,
    bh: f32,
    params: &StrokeRenderParams,
) -> (u32, u32, Vec<u8>) {
    let pad = params.pad_px.max(0.0);
    let inner_h = (params.target_height as f32 - 2.0 * pad).max(1.0);
    let scale = inner_h / bh.max(1.0);
    let mut tw = ((bw * scale) + 2.0 * pad).round().max(8.0) as u32;
    let th = params.target_height.max(1);
    if params.max_width > 0 && tw > params.max_width {
        tw = params.max_width;
    }
    // Safety cap for pathological pages.
    tw = tw.min(4096);

    let mut gray = vec![255u8; (tw as usize) * (th as usize)];
    let ox = bx - pad / scale;
    let oy = by - pad / scale;
    // If width was capped, recompute scale from width so ink still fits.
    let scale_x = (tw as f32 - 2.0 * pad) / bw.max(1.0);
    let scale_y = scale;
    let scale_use = scale_x.min(scale_y);

    let radius = (params.stroke_width_px * 0.5).max(0.4);
    for s in strokes {
        if s.tool == Tool::Eraser || s.points.len() < 2 {
            // Single-point marks (dots): stamp a disk.
            if s.tool != Tool::Eraser && s.points.len() == 1 {
                let p = &s.points[0];
                let x = (p.x - ox) * scale_use;
                let y = (p.y - oy) * scale_use;
                stamp_disk(&mut gray, tw, th, x, y, radius, params.anti_alias);
            }
            continue;
        }
        let width = if s.base_width > 0.0 {
            (s.base_width * scale_use * 0.35)
                .clamp(params.stroke_width_px * 0.65, params.stroke_width_px * 1.4)
                * 0.5
        } else {
            radius
        };
        for win in s.points.windows(2) {
            let x0 = (win[0].x - ox) * scale_use;
            let y0 = (win[0].y - oy) * scale_use;
            let x1 = (win[1].x - ox) * scale_use;
            let y1 = (win[1].y - oy) * scale_use;
            draw_segment_aa(&mut gray, tw, th, x0, y0, x1, y1, width, params.anti_alias);
        }
    }
    (tw, th, gray)
}

/// Prepare a DE·HTR v2 [`LineHtrInput`] by vector-rendering `line` at height 64.
///
/// Polarity: dark-on-light, pixels in `[0, 1]` with white = 1.0 (see module docs).
pub fn prepare_line_htr_from_strokes(line: &StrokeLine) -> Option<LineHtrInput> {
    if line.strokes.is_empty() || line.width <= 0.0 || line.height <= 0.0 {
        return None;
    }
    let params = StrokeRenderParams::for_line_htr();
    let (w, h, gray) = render_stroke_line(line, &params);
    if w == 0 || h == 0 {
        return None;
    }
    // Right-pad width to multiple of 4; centre vertically if h < 64 (should be 64).
    let pad_w = w
        .div_ceil(LINE_HTR_WIDTH_ALIGN)
        .saturating_mul(LINE_HTR_WIDTH_ALIGN)
        .min(LINE_HTR_MAX_WIDTH)
        .max(w);
    let mut pixels = vec![1.0f32; (pad_w as usize) * (LINE_HTR_HEIGHT as usize)];
    let y_off = LINE_HTR_HEIGHT.saturating_sub(h) / 2;
    let copy_h = h.min(LINE_HTR_HEIGHT);
    let copy_w = w.min(pad_w);
    for y in 0..copy_h {
        for x in 0..copy_w {
            let g = gray[(y * w + x) as usize] as f32 / 255.0;
            pixels[((y + y_off) * pad_w + x) as usize] = g;
        }
    }
    Some(LineHtrInput {
        width: pad_w,
        height: LINE_HTR_HEIGHT,
        pixels,
        band: ImageLineBand {
            y0: line.y.max(0.0) as u32,
            y1: (line.y + line.height).max(0.0).ceil() as u32,
            x0: line.x.max(0.0) as u32,
            x1: (line.x + line.width).max(0.0).ceil() as u32,
        },
    })
}

fn stamp_disk(buf: &mut [u8], w: u32, h: u32, cx: f32, cy: f32, radius: f32, aa: bool) {
    let r = radius + if aa { 0.75 } else { 0.0 };
    let x0 = ((cx - r).floor() as i32).max(0);
    let y0 = ((cy - r).floor() as i32).max(0);
    let x1 = ((cx + r).ceil() as i32).min(w as i32 - 1);
    let y1 = ((cy + r).ceil() as i32).min(h as i32 - 1);
    for y in y0..=y1 {
        for x in x0..=x1 {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let d = (dx * dx + dy * dy).sqrt();
            let cov = if aa {
                (radius + 0.5 - d).clamp(0.0, 1.0)
            } else if d <= radius {
                1.0
            } else {
                0.0
            };
            if cov > 0.0 {
                blend_ink(buf, w, x as u32, y as u32, cov);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_segment_aa(
    buf: &mut [u8],
    w: u32,
    h: u32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    radius: f32,
    aa: bool,
) {
    let dx = x1 - x0;
    let dy = y1 - y0;
    let len = dx.hypot(dy);
    if len < 1e-3 {
        stamp_disk(buf, w, h, x0, y0, radius, aa);
        return;
    }
    let pad = radius + 1.0;
    let min_x = ((x0.min(x1) - pad).floor() as i32).max(0);
    let min_y = ((y0.min(y1) - pad).floor() as i32).max(0);
    let max_x = ((x0.max(x1) + pad).ceil() as i32).min(w as i32 - 1);
    let max_y = ((y0.max(y1) + pad).ceil() as i32).min(h as i32 - 1);
    let inv_len2 = 1.0 / (len * len);
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let t = ((px - x0) * dx + (py - y0) * dy) * inv_len2;
            let t = t.clamp(0.0, 1.0);
            let qx = x0 + t * dx;
            let qy = y0 + t * dy;
            let d = (px - qx).hypot(py - qy);
            let cov = if aa {
                (radius + 0.5 - d).clamp(0.0, 1.0)
            } else if d <= radius {
                1.0
            } else {
                0.0
            };
            if cov > 0.0 {
                blend_ink(buf, w, x as u32, y as u32, cov);
            }
        }
    }
}

fn blend_ink(buf: &mut [u8], w: u32, x: u32, y: u32, cov: f32) {
    let i = (y * w + x) as usize;
    let src = buf[i] as f32;
    // Darken toward 0 with coverage.
    let out = src * (1.0 - cov);
    buf[i] = out.round().clamp(0.0, 255.0) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Color, Point, StrokeId};

    fn sample_line() -> StrokeLine {
        let s = Stroke {
            id: StrokeId::new(),
            tool: Tool::Pen,
            color: Color::BLACK,
            base_width: 2.0,
            points: vec![
                Point::new(10.0, 20.0, 0.5, 0.0, 0),
                Point::new(80.0, 28.0, 0.5, 0.0, 100),
                Point::new(140.0, 22.0, 0.5, 0.0, 200),
            ],
            t0_ms: 0,
        };
        StrokeLine {
            strokes: vec![s],
            x: 10.0,
            y: 20.0,
            width: 130.0,
            height: 8.0,
        }
    }

    #[test]
    fn vector_line_htr_height_64_dark_on_light() {
        let line = sample_line();
        let input = prepare_line_htr_from_strokes(&line).expect("render");
        assert_eq!(input.height, LINE_HTR_HEIGHT);
        assert_eq!(input.width % LINE_HTR_WIDTH_ALIGN, 0);
        assert!(input.width <= LINE_HTR_MAX_WIDTH);
        assert_eq!(input.pixels.len(), (input.width * input.height) as usize);
        // White background (= 1.0) and some dark ink (< 0.5).
        assert!(input.pixels.iter().any(|&p| p > 0.95));
        assert!(
            input.pixels.iter().any(|&p| p < 0.5),
            "expected dark-on-light ink"
        );
        // Must not be inverted (mostly white).
        let mean: f32 = input.pixels.iter().sum::<f32>() / input.pixels.len() as f32;
        assert!(mean > 0.7, "mean={mean}: expected light paper dominant");
    }

    #[test]
    fn render_stroke_line_matches_target_height() {
        let line = sample_line();
        let (w, h, gray) = render_stroke_line(&line, &StrokeRenderParams::for_line_htr());
        assert_eq!(h, 64);
        assert!(w >= 8);
        assert!(gray.iter().any(|&g| g < 128));
        assert!(gray.iter().any(|&g| g > 200));
    }
}
