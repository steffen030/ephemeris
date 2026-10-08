//! Stroke rasterizer for the ink canvas layer.
//!
//! Rasterizes [`Stroke`] and in-progress point lists into a grayscale
//! [`PixelBuf`] (8 bpp, 0 = black, 255 = white).
//!
//! ## Design
//!
//! Each consecutive pair of stroke [`Point`]s is drawn as a **thick line
//! segment**: a filled rectangle oriented along the segment, with the
//! half-width at each end derived from `point.pressure` × `base_width` via
//! the same [`WidthConfig`] used by the ink engine for damage inflation.
//!
//! Specifically, for each segment we stamp a set of perpendicular scanline
//! strips across the segment bounding box, keeping only pixels whose distance
//! from the *centre line* is ≤ the interpolated half-width at that position.
//! This gives smooth, pressure-varying strokes without requiring a full
//! polygon fill pipeline.
//!
//! All coordinates in this module are in **full-screen PixelBuf space** — the
//! caller is responsible for adding the canvas Y-offset before calling.

use ephemeris_core::ink::WidthConfig;
use ephemeris_core::model::{Point, Stroke, Tool};
use ephemeris_pal::display::PixelBuf;

// ── Canvas bounds helper ──────────────────────────────────────────────────────

/// Describes where the canvas lives inside the full-screen [`PixelBuf`].
///
/// All point coordinates passed to the rasterizer are canvas-relative; the
/// `x0`/`y0` offsets are added when writing to the buffer so that the ink
/// lands in the correct region and never touches chrome rows.
#[derive(Clone, Copy)]
struct CanvasBounds {
    /// X offset of the canvas top-left in pixel-buf space (typically 0).
    x0: u32,
    /// Y offset of the canvas top-left in pixel-buf space (= status bar height).
    y0: u32,
    /// Canvas width in pixels.
    w: u32,
    /// Canvas height in pixels.
    h: u32,
}

// ── Public entry points ───────────────────────────────────────────────────────

/// Rasterize a completed [`Stroke`] into `buf`.
///
/// `canvas_x0` and `canvas_y0` are the pixel-buf coordinates of the canvas
/// top-left corner (i.e. the status-bar height as a Y offset, and 0 for X on
/// a full-width canvas).  Points in the stroke are canvas-relative; they are
/// offset before stamping so they land in the correct buffer rows.
///
/// The stroke's [`Tool`] determines rendering:
/// * [`Tool::Pen`]         — black (0), min-blend (only darkens)
/// * [`Tool::Highlighter`] — mid-gray (160), min-blend (dark ink preserved)
/// * [`Tool::Eraser`]      — white (255), overwrite-blend (lightens ink)
///
/// Strokes outside the canvas clip region are silently clipped.
pub fn rasterize_stroke(
    buf: &mut PixelBuf,
    stroke: &Stroke,
    canvas_x0: u32,
    canvas_y0: u32,
    canvas_w: u32,
    canvas_h: u32,
) {
    rasterize_points(
        buf,
        &stroke.points,
        stroke.base_width,
        canvas_x0,
        canvas_y0,
        canvas_w,
        canvas_h,
        stroke.tool,
    );
}

/// Rasterize an in-progress (uncommitted) list of [`Point`]s into `buf`.
///
/// Used during live drawing before the stroke is finalised.  Parameters are
/// the same as [`rasterize_stroke`].  `tool` controls how pixels are blended
/// (see [`rasterize_stroke`] for per-tool semantics).
#[allow(clippy::too_many_arguments)]
pub fn rasterize_points(
    buf: &mut PixelBuf,
    points: &[Point],
    base_width: f32,
    canvas_x0: u32,
    canvas_y0: u32,
    canvas_w: u32,
    canvas_h: u32,
    tool: Tool,
) {
    let bounds = CanvasBounds {
        x0: canvas_x0,
        y0: canvas_y0,
        w: canvas_w,
        h: canvas_h,
    };

    if points.len() < 2 {
        // Single-point tap: stamp a dot at the point location.
        if let Some(p) = points.first() {
            let hw = WIDTHS.width_for(base_width, p.pressure) * 0.5;
            stamp_dot(buf, p.x, p.y, hw, bounds, tool);
        }
        return;
    }

    for w in points.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        let seg = Segment {
            ax: a.x,
            ay: a.y,
            hw_a: WIDTHS.width_for(base_width, a.pressure) * 0.5,
            bx: b.x,
            by: b.y,
            hw_b: WIDTHS.width_for(base_width, b.pressure) * 0.5,
        };
        draw_segment(buf, &seg, bounds, tool);
    }
}

// ── Width config ──────────────────────────────────────────────────────────────

/// Default pressure→width config, shared with the ink engine's damage math.
const WIDTHS: WidthConfig = WidthConfig {
    min_ratio: 0.35,
    max_ratio: 1.6,
    gamma: 1.4,
    min_px: 0.5,
};

// ── Segment renderer ──────────────────────────────────────────────────────────

/// A thick line segment with pressure-varying half-widths at each endpoint.
struct Segment {
    ax: f32,
    ay: f32,
    /// Half-width at endpoint A.
    hw_a: f32,
    bx: f32,
    by: f32,
    /// Half-width at endpoint B.
    hw_b: f32,
}

/// Draw a thick line segment with pressure-varying width onto `buf`.
///
/// Iterates over every pixel in the segment's bounding box (clamped to the
/// canvas).  For each pixel centre `(px, py)` it computes the closest point
/// on the segment, interpolates the half-width at that point, and stamps the
/// pixel according to `tool`.
///
/// All coordinates are canvas-relative.
fn draw_segment(buf: &mut PixelBuf, seg: &Segment, bounds: CanvasBounds, tool: Tool) {
    let Segment {
        ax,
        ay,
        hw_a,
        bx,
        by,
        hw_b,
    } = *seg;
    // Maximum half-width for bounding box inflation.
    let hw_max = hw_a.max(hw_b);

    // Bounding box of the segment, canvas-relative, inflated by max half-width
    // and padded by 1 for sub-pixel coverage.
    let min_cx = (ax.min(bx) - hw_max - 1.0).max(0.0).floor() as u32;
    let min_cy = (ay.min(by) - hw_max - 1.0).max(0.0).floor() as u32;
    let max_cx = (ax.max(bx) + hw_max + 1.0)
        .min(bounds.w as f32 - 1.0)
        .ceil() as u32;
    let max_cy = (ay.max(by) + hw_max + 1.0)
        .min(bounds.h as f32 - 1.0)
        .ceil() as u32;

    let dx = bx - ax;
    let dy = by - ay;
    let len_sq = dx * dx + dy * dy;

    for cy in min_cy..=max_cy {
        for cx in min_cx..=max_cx {
            // Pixel centre in canvas space.
            let px = cx as f32 + 0.5;
            let py = cy as f32 + 0.5;

            // Closest point on segment [A,B] to (px, py).
            let (t, dist) = if len_sq < 1e-8 {
                // Degenerate segment: treat as a dot.
                let d = ((px - ax).powi(2) + (py - ay).powi(2)).sqrt();
                (0.0f32, d)
            } else {
                let t_raw = ((px - ax) * dx + (py - ay) * dy) / len_sq;
                let t = t_raw.clamp(0.0, 1.0);
                let qx = ax + t * dx;
                let qy = ay + t * dy;
                let d = ((px - qx).powi(2) + (py - qy).powi(2)).sqrt();
                (t, d)
            };

            // Interpolated half-width at parameter t.
            let hw = hw_a + (hw_b - hw_a) * t;

            if dist <= hw + 0.5 {
                // Anti-alias: blend based on coverage within the last 0.5 px.
                let coverage = if dist > hw - 0.5 {
                    (hw + 0.5 - dist).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                stamp_pixel(
                    buf,
                    bounds.x0 + cx,
                    bounds.y0 + cy,
                    coverage,
                    buf.width,
                    buf.height,
                    tool,
                );
            }
        }
    }
}

/// Stamp a filled circle (dot) at a canvas-relative `(cx, cy)` with radius `r`.
fn stamp_dot(buf: &mut PixelBuf, cx: f32, cy: f32, r: f32, bounds: CanvasBounds, tool: Tool) {
    let min_x = (cx - r - 1.0).max(0.0).floor() as u32;
    let min_y = (cy - r - 1.0).max(0.0).floor() as u32;
    let max_x = (cx + r + 1.0).min(bounds.w as f32 - 1.0).ceil() as u32;
    let max_y = (cy + r + 1.0).min(bounds.h as f32 - 1.0).ceil() as u32;

    for py in min_y..=max_y {
        for px in min_x..=max_x {
            let dist = (((px as f32 + 0.5) - cx).powi(2) + ((py as f32 + 0.5) - cy).powi(2)).sqrt();
            if dist <= r + 0.5 {
                let coverage = if dist > r - 0.5 {
                    (r + 0.5 - dist).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                stamp_pixel(
                    buf,
                    bounds.x0 + px,
                    bounds.y0 + py,
                    coverage,
                    buf.width,
                    buf.height,
                    tool,
                );
            }
        }
    }
}

/// Write a single pixel with anti-aliased `coverage`, tool-specific blend:
/// * [`Tool::Pen`]         — lerp toward 0 (black), min-blend (never lightens)
/// * [`Tool::Highlighter`] — lerp toward 160 (gray), min-blend (preserves dark ink)
/// * [`Tool::Eraser`]      — lerp toward 255 (white), overwrite (lightens ink)
#[inline]
fn stamp_pixel(
    buf: &mut PixelBuf,
    bx: u32,
    by: u32,
    coverage: f32,
    buf_w: u32,
    buf_h: u32,
    tool: Tool,
) {
    if bx < buf_w && by < buf_h {
        let idx = (by * buf.stride + bx) as usize;
        let existing = buf.data[idx] as f32;
        match tool {
            Tool::Pen => {
                let new_val = existing * (1.0 - coverage); // lerp toward 0
                buf.data[idx] = buf.data[idx].min(new_val.round() as u8);
            }
            Tool::Highlighter => {
                const HIGHLIGHT_GRAY: f32 = 160.0;
                let new_val = existing + (HIGHLIGHT_GRAY - existing) * coverage;
                // min-blend: never lighten existing dark ink from pen strokes
                buf.data[idx] = buf.data[idx].min(new_val.round() as u8);
            }
            Tool::Eraser => {
                // Lerp toward white (255); overwrite so ink can be removed.
                let new_val = existing + (255.0 - existing) * coverage;
                buf.data[idx] = new_val.round() as u8;
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ephemeris_core::model::{Color, Stroke, Tool};

    /// Build a stroke from a list of (x, y) canvas-relative coordinates at
    /// constant pressure.
    fn make_stroke(coords: &[(f32, f32)], pressure: f32, base_width: f32) -> Stroke {
        let mut s = Stroke::new(Tool::Pen, Color::BLACK, base_width);
        for (i, &(x, y)) in coords.iter().enumerate() {
            s.points.push(Point {
                x,
                y,
                pressure,
                tilt: 0.0,
                t_ms: i as u32 * 8,
            });
        }
        s
    }

    /// A 100×130 buffer: 24 px status + 100 px canvas + 6 px spare.
    fn small_canvas() -> PixelBuf {
        PixelBuf::new(100, 130)
    }

    const CX0: u32 = 0;
    const CY0: u32 = 24;
    const CW: u32 = 100;
    const CH: u32 = 100;

    // ── rasterize_stroke marks expected pixels dark ───────────────────────────

    #[test]
    fn rasterize_stroke_marks_pixels_dark() {
        let mut buf = small_canvas();
        // Horizontal stroke across the canvas at y=50 (canvas-relative).
        let stroke = make_stroke(&[(10.0, 50.0), (90.0, 50.0)], 0.5, 4.0);

        rasterize_stroke(&mut buf, &stroke, CX0, CY0, CW, CH);

        // At least one pixel along the stroke path must be darker than white.
        // Check a pixel on the centre line (canvas-relative y=50 → buf y=74).
        let any_dark =
            (10..90).any(|x| buf.data[(CY0 + 50) as usize * buf.stride as usize + x] < 200);
        assert!(any_dark, "stroke should mark pixels dark");
    }

    #[test]
    fn rasterize_single_point_stamp_marks_dot() {
        let mut buf = small_canvas();
        let stroke = make_stroke(&[(50.0, 50.0)], 0.5, 4.0);

        rasterize_stroke(&mut buf, &stroke, CX0, CY0, CW, CH);

        // The pixel at canvas (50, 50) → buf (50, 74) should be dark.
        let idx = (CY0 + 50) as usize * buf.stride as usize + 50;
        assert!(
            buf.data[idx] < 200,
            "single-point stamp should produce a dark dot"
        );
    }

    #[test]
    fn rasterize_stroke_stays_within_canvas() {
        let mut buf = small_canvas();
        // Stroke that spans the full canvas height.
        let stroke = make_stroke(&[(50.0, 0.0), (50.0, 99.0)], 0.5, 4.0);

        rasterize_stroke(&mut buf, &stroke, CX0, CY0, CW, CH);

        // Status bar rows (0..24) must remain white.
        for row in 0..CY0 {
            for col in 0..buf.width {
                let idx = row as usize * buf.stride as usize + col as usize;
                assert_eq!(
                    buf.data[idx], 255,
                    "status bar row {row} col {col} must stay white"
                );
            }
        }
    }

    #[test]
    fn higher_pressure_stamps_wider_mark() {
        // Rasterize two identical vertical strokes — one with low pressure, one
        // with full pressure — and count the dark pixels in the canvas region.
        let x_mid = 50.0f32;
        let coords: Vec<(f32, f32)> = (10..90).map(|y| (x_mid, y as f32)).collect();

        let soft = make_stroke(&coords, 0.1, 4.0);
        let firm = make_stroke(&coords, 1.0, 4.0);

        let mut buf_soft = small_canvas();
        let mut buf_firm = small_canvas();

        rasterize_stroke(&mut buf_soft, &soft, CX0, CY0, CW, CH);
        rasterize_stroke(&mut buf_firm, &firm, CX0, CY0, CW, CH);

        // Count dark pixels in the canvas rows.
        let dark_cols = |buf: &PixelBuf| -> u32 {
            let mut count = 0;
            for row in CY0..(CY0 + CH) {
                for col in 0..buf.width {
                    let idx = row as usize * buf.stride as usize + col as usize;
                    if buf.data[idx] < 200 {
                        count += 1;
                    }
                }
            }
            count
        };

        let soft_dark = dark_cols(&buf_soft);
        let firm_dark = dark_cols(&buf_firm);
        assert!(
            firm_dark > soft_dark,
            "firm pressure stroke ({firm_dark} dark px) must be wider than soft ({soft_dark} dark px)"
        );
    }
}
