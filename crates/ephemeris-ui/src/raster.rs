//! Stroke rasterizer for the ink canvas layer.
//!
//! Rasterizes [`Stroke`] and in-progress point lists into a grayscale
//! [`PixelBuf`] (8 bpp, 0 = black, 255 = white).
//!
//! ## Coordinate spaces
//!
//! Stroke points live in **logical canvas coordinates** (the space the ink
//! engine, eraser and persisted strokes use). The target buffer is the
//! **ink plane**: a canvas-sized layer whose resolution may be higher than the
//! logical canvas (panel resolution on PineNote, ×2 on Retina). [`InkScale`]
//! maps logical → ink-plane pixels; every stamp happens in ink-plane pixels so
//! edges are anti-aliased at the resolution that is actually presented.
//!
//! ## Design
//!
//! Each consecutive pair of stroke [`Point`]s is drawn as a tapered
//! **capsule**: every pixel whose distance from the segment is ≤ the
//! interpolated half-width is covered, with a 1 px linear coverage ramp at the
//! edge. Capsules share their endpoints, which yields round caps and joins.
//!
//! Overlapping capsules of the same stroke are combined as a *union* (the
//! darkest coverage wins) rather than multiplied, so joins do not darken the
//! anti-aliased fringe and the visible width stays constant along a stroke.

use ephemeris_core::ink::WidthConfig;
use ephemeris_core::model::{Point, Stroke, Tool};
use ephemeris_pal::display::PixelBuf;

// ── Ink scale ─────────────────────────────────────────────────────────────────

/// Per-axis scale from logical canvas pixels to ink-plane pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InkScale {
    pub sx: f32,
    pub sy: f32,
}

impl InkScale {
    /// Ink plane at logical resolution.
    pub const IDENTITY: InkScale = InkScale { sx: 1.0, sy: 1.0 };

    pub fn new(sx: f32, sy: f32) -> Self {
        InkScale { sx, sy }
    }

    pub fn uniform(s: f32) -> Self {
        InkScale { sx: s, sy: s }
    }

    /// Scale that maps a `logical` canvas onto an `ink` plane of the given size.
    pub fn from_sizes(logical: (u32, u32), ink: (u32, u32)) -> Self {
        let sx = ink.0 as f32 / logical.0.max(1) as f32;
        let sy = ink.1 as f32 / logical.1.max(1) as f32;
        InkScale { sx, sy }
    }

    /// Isotropic factor applied to stroke widths (geometric mean of the axes).
    pub fn width_factor(&self) -> f32 {
        (self.sx * self.sy).sqrt()
    }

    pub fn is_identity(&self) -> bool {
        *self == Self::IDENTITY
    }

    /// Map a logical canvas point to ink-plane pixel coordinates.
    #[inline]
    pub fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (x * self.sx, y * self.sy)
    }
}

impl Default for InkScale {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// Map a logical canvas rect to clamped ink-plane pixel bounds `(x0, y0, x1, y1)`
/// (exclusive end). Returns `None` when the rect is empty or fully clipped.
pub fn ink_pixel_bounds(
    rect: &ephemeris_core::geom::Rect,
    scale: InkScale,
    ink_w: u32,
    ink_h: u32,
) -> Option<(u32, u32, u32, u32)> {
    if rect.is_empty() {
        return None;
    }
    let x0 = (rect.min_x * scale.sx).floor().max(0.0) as u32;
    let y0 = (rect.min_y * scale.sy).floor().max(0.0) as u32;
    let x1 = ((rect.max_x * scale.sx).ceil().max(0.0) as u32).min(ink_w);
    let y1 = ((rect.max_y * scale.sy).ceil().max(0.0) as u32).min(ink_h);
    (x1 > x0 && y1 > y0).then_some((x0, y0, x1, y1))
}

/// Merge `src` into `dst` with per-pixel `min` over the ink-plane footprint of
/// logical `rect` (union blend for pen/highlighter). Used at PenUp to promote
/// the live layer into committed without re-rasterising the stroke.
pub fn merge_min_rect(
    dst: &mut PixelBuf,
    src: &PixelBuf,
    rect: &ephemeris_core::geom::Rect,
    scale: InkScale,
) {
    let w = dst.width.min(src.width);
    let h = dst.height.min(src.height);
    let Some((x0, y0, x1, y1)) = ink_pixel_bounds(rect, scale, w, h) else {
        return;
    };
    let dst_stride = dst.stride as usize;
    let src_stride = src.stride as usize;
    for y in y0..y1 {
        let drow = y as usize * dst_stride;
        let srow = y as usize * src_stride;
        for x in x0..x1 {
            let di = drow + x as usize;
            let si = srow + x as usize;
            dst.data[di] = dst.data[di].min(src.data[si]);
        }
    }
}

// ── Public entry points ───────────────────────────────────────────────────────

/// Rasterize a completed [`Stroke`] into the canvas-sized ink plane `buf`.
///
/// The stroke's [`Tool`] determines rendering:
/// * [`Tool::Pen`]         — black (0), union-blend (only darkens)
/// * [`Tool::Highlighter`] — mid-gray (160), union-blend (dark ink preserved)
/// * [`Tool::Eraser`]      — white (255), overwrite-blend (lightens ink)
///
/// Pixels outside `buf` are clipped.
pub fn rasterize_stroke(buf: &mut PixelBuf, stroke: &Stroke, scale: InkScale) {
    rasterize_points(buf, &stroke.points, stroke.base_width, stroke.tool, scale);
}

/// Rasterize an in-progress (uncommitted) list of logical [`Point`]s into the
/// canvas-sized ink plane `buf`.
///
/// Used during live drawing before the stroke is finalised (typically with a
/// two-point slice for the newest segment). `tool` controls how pixels are
/// blended (see [`rasterize_stroke`]).
pub fn rasterize_points(
    buf: &mut PixelBuf,
    points: &[Point],
    base_width: f32,
    tool: Tool,
    scale: InkScale,
) {
    let wf = scale.width_factor();
    if points.len() < 2 {
        // Single-point tap: stamp a dot at the point location.
        if let Some(p) = points.first() {
            let (x, y) = scale.apply(p.x, p.y);
            let hw = half_width_px(base_width, p.pressure, wf);
            draw_capsule(buf, x, y, hw, x, y, hw, tool);
        }
        return;
    }

    for w in points.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        let (ax, ay) = scale.apply(a.x, a.y);
        let (bx, by) = scale.apply(b.x, b.y);
        draw_capsule(
            buf,
            ax,
            ay,
            half_width_px(base_width, a.pressure, wf),
            bx,
            by,
            half_width_px(base_width, b.pressure, wf),
            tool,
        );
    }
}

/// Half stroke width in ink-plane pixels for `pressure` at width factor `wf`.
///
/// Uses [`WidthConfig::default`] — the same curve as [`ephemeris_core::ink::InkConfig`]
/// — so raster and engine damage stay in lockstep (ephemeris-ktg.6).
#[inline]
pub fn half_width_px(base_width: f32, pressure: f32, wf: f32) -> f32 {
    WidthConfig::default().width_for(base_width, pressure) * 0.5 * wf
}

// ── Capsule renderer ──────────────────────────────────────────────────────────

/// Stamp a tapered capsule from A (half-width `hw_a`) to B (`hw_b`), all in
/// ink-plane pixels. A degenerate segment (A == B) stamps a round dot.
#[allow(clippy::too_many_arguments)]
fn draw_capsule(
    buf: &mut PixelBuf,
    ax: f32,
    ay: f32,
    hw_a: f32,
    bx: f32,
    by: f32,
    hw_b: f32,
    tool: Tool,
) {
    if buf.width == 0 || buf.height == 0 {
        return;
    }
    let pad = hw_a.max(hw_b) + 1.0;
    let min_x = (ax.min(bx) - pad).floor().max(0.0) as i64;
    let min_y = (ay.min(by) - pad).floor().max(0.0) as i64;
    let max_x = ((ax.max(bx) + pad).ceil() as i64).min(buf.width as i64 - 1);
    let max_y = ((ay.max(by) + pad).ceil() as i64).min(buf.height as i64 - 1);
    if max_x < min_x || max_y < min_y {
        return;
    }

    let dx = bx - ax;
    let dy = by - ay;
    let len_sq = dx * dx + dy * dy;
    let inv_len_sq = if len_sq < 1e-8 { 0.0 } else { 1.0 / len_sq };
    let stride = buf.stride as usize;

    for py in min_y..=max_y {
        let cy = py as f32 + 0.5;
        let row = py as usize * stride;
        for px in min_x..=max_x {
            let cx = px as f32 + 0.5;
            // Closest point on [A, B] (t = 0 for a dot).
            let t = (((cx - ax) * dx + (cy - ay) * dy) * inv_len_sq).clamp(0.0, 1.0);
            let qx = ax + t * dx;
            let qy = ay + t * dy;
            let dist = ((cx - qx).powi(2) + (cy - qy).powi(2)).sqrt();
            let hw = hw_a + (hw_b - hw_a) * t;
            let coverage = (hw + 0.5 - dist).clamp(0.0, 1.0);
            if coverage > 0.0 {
                stamp_pixel(&mut buf.data[row + px as usize], coverage, tool);
            }
        }
    }
}

/// Blend one pixel with anti-aliased `coverage`, tool-specific:
/// * [`Tool::Pen`]         — target `255·(1-c)`, keep the darker (union)
/// * [`Tool::Highlighter`] — target lerp(255→160, c), keep the darker
/// * [`Tool::Eraser`]      — lerp toward 255 (white), overwrite (lightens ink)
#[inline]
fn stamp_pixel(px: &mut u8, coverage: f32, tool: Tool) {
    match tool {
        Tool::Pen => {
            let target = (255.0 * (1.0 - coverage)).round() as u8;
            *px = (*px).min(target);
        }
        Tool::Highlighter => {
            const HIGHLIGHT_GRAY: f32 = 160.0;
            let target = (255.0 + (HIGHLIGHT_GRAY - 255.0) * coverage).round() as u8;
            *px = (*px).min(target);
        }
        Tool::Eraser => {
            let existing = *px as f32;
            *px = (existing + (255.0 - existing) * coverage).round() as u8;
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

    fn canvas() -> PixelBuf {
        PixelBuf::new(100, 100)
    }

    fn dark_count(buf: &PixelBuf) -> u32 {
        buf.data.iter().filter(|&&v| v < 200).count() as u32
    }

    #[test]
    fn rasterize_stroke_marks_pixels_dark() {
        let mut buf = canvas();
        let stroke = make_stroke(&[(10.0, 50.0), (90.0, 50.0)], 0.5, 4.0);
        rasterize_stroke(&mut buf, &stroke, InkScale::IDENTITY);
        let any_dark = (10..90).any(|x| buf.data[50 * buf.stride as usize + x] < 200);
        assert!(any_dark, "stroke should mark pixels dark");
    }

    #[test]
    fn rasterize_single_point_stamp_marks_dot() {
        let mut buf = canvas();
        let stroke = make_stroke(&[(50.0, 50.0)], 0.5, 4.0);
        rasterize_stroke(&mut buf, &stroke, InkScale::IDENTITY);
        let idx = 50 * buf.stride as usize + 50;
        assert!(
            buf.data[idx] < 200,
            "single-point stamp should produce a dark dot"
        );
    }

    #[test]
    fn rasterize_stroke_clips_to_buffer() {
        let mut buf = canvas();
        // Runs off every edge; must clip without panicking.
        let stroke = make_stroke(&[(-20.0, -20.0), (50.0, 50.0), (140.0, 130.0)], 0.5, 4.0);
        rasterize_stroke(&mut buf, &stroke, InkScale::uniform(1.755));
        assert!(dark_count(&buf) > 0);
    }

    #[test]
    fn higher_pressure_stamps_wider_mark() {
        let coords: Vec<(f32, f32)> = (10..90).map(|y| (50.0, y as f32)).collect();
        let soft = make_stroke(&coords, 0.1, 4.0);
        let firm = make_stroke(&coords, 1.0, 4.0);

        let mut buf_soft = canvas();
        let mut buf_firm = canvas();
        rasterize_stroke(&mut buf_soft, &soft, InkScale::IDENTITY);
        rasterize_stroke(&mut buf_firm, &firm, InkScale::IDENTITY);

        let soft_dark = dark_count(&buf_soft);
        let firm_dark = dark_count(&buf_firm);
        assert!(
            firm_dark > soft_dark,
            "firm pressure stroke ({firm_dark} dark px) must be wider than soft ({soft_dark} dark px)"
        );
    }

    #[test]
    fn scale_maps_logical_points_to_ink_pixels() {
        // Logical (20, 20) at s=2 lands at ink (40, 40).
        let mut buf = canvas();
        let stroke = make_stroke(&[(20.0, 20.0)], 0.5, 4.0);
        rasterize_stroke(&mut buf, &stroke, InkScale::uniform(2.0));
        assert!(buf.data[40 * 100 + 40] < 50);
        assert_eq!(buf.data[20 * 100 + 20], 255);
    }

    #[test]
    fn scale_widens_stroke_proportionally() {
        let coords: Vec<(f32, f32)> = (5..40).map(|x| (x as f32, 20.0)).collect();
        let stroke = make_stroke(&coords, 0.5, 4.0);
        let mut one = canvas();
        let mut two = canvas();
        rasterize_stroke(&mut one, &stroke, InkScale::IDENTITY);
        rasterize_stroke(&mut two, &stroke, InkScale::uniform(2.0));
        let ink = |b: &PixelBuf, x: usize, y0: usize, y1: usize| -> f32 {
            (y0..y1)
                .map(|y| (255 - b.data[y * 100 + x]) as f32 / 255.0)
                .sum()
        };
        let w1 = ink(&one, 20, 10, 30);
        let w2 = ink(&two, 40, 25, 55);
        assert!(
            (w2 / w1 - 2.0).abs() < 0.15,
            "width should scale ×2 (w1={w1}, w2={w2})"
        );
    }

    #[test]
    fn overlapping_segments_do_not_darken_fringe() {
        // Many short segments over the same line: union blend means the AA
        // fringe stays as light as a single capsule's fringe.
        let mut single = canvas();
        let mut dense = canvas();
        let s1 = make_stroke(&[(10.0, 50.3), (90.0, 50.3)], 0.5, 4.0);
        let coords: Vec<(f32, f32)> = (0..=160).map(|i| (10.0 + i as f32 * 0.5, 50.3)).collect();
        let s2 = make_stroke(&coords, 0.5, 4.0);
        rasterize_stroke(&mut single, &s1, InkScale::IDENTITY);
        rasterize_stroke(&mut dense, &s2, InkScale::IDENTITY);
        for y in 44..57 {
            let a = single.data[y * 100 + 50];
            let b = dense.data[y * 100 + 50];
            assert!(
                a.abs_diff(b) <= 1,
                "row {y}: single={a} dense={b} (fringe must not accumulate)"
            );
        }
    }

    #[test]
    fn ink_pixel_bounds_scales_and_clamps() {
        let r = ephemeris_core::geom::Rect::new(10.0, 10.0, 20.0, 30.0);
        assert_eq!(
            ink_pixel_bounds(&r, InkScale::uniform(2.0), 100, 50),
            Some((20, 20, 40, 50))
        );
        let off = ephemeris_core::geom::Rect::new(200.0, 0.0, 210.0, 5.0);
        assert_eq!(ink_pixel_bounds(&off, InkScale::IDENTITY, 100, 100), None);
    }

    #[test]
    fn merge_min_rect_keeps_darker_pixels() {
        let mut dst = canvas();
        let mut src = canvas();
        dst.data[50 * 100 + 50] = 200;
        src.data[50 * 100 + 50] = 40;
        let r = ephemeris_core::geom::Rect::new(40.0, 40.0, 60.0, 60.0);
        merge_min_rect(&mut dst, &src, &r, InkScale::IDENTITY);
        assert_eq!(dst.data[50 * 100 + 50], 40);
        // Outside the rect is untouched.
        assert_eq!(dst.data[0], 255);
    }
}
