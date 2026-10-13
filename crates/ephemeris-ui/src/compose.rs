//! Physical-resolution frame composition.
//!
//! The Slint UI renders at logical (design) resolution while ink is rasterised
//! at panel resolution. Composition keeps two physical buffers:
//!
//! * **background cache** — logical UI gray (Slint chrome + page template)
//!   bilinear-upscaled to physical size, refreshed only inside UI-damage rects
//!   ([`upscale_bilinear_rect`]);
//! * **present buffer** — `min(background, ink)` per physical pixel
//!   ([`compose_rect`]), handed to the display 1:1.
//!
//! Live ink presents therefore never resample: they are integer `min()`s over
//! the physical damage rect.

use ephemeris_pal::display::{PixelBuf, Rect};

/// Logical ↔ physical frame geometry (per-axis integer mapping).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameGeom {
    pub logical_w: u32,
    pub logical_h: u32,
    pub phys_w: u32,
    pub phys_h: u32,
}

impl FrameGeom {
    pub fn new(logical: (u32, u32), phys: (u32, u32)) -> Self {
        FrameGeom {
            logical_w: logical.0.max(1),
            logical_h: logical.1.max(1),
            phys_w: phys.0,
            phys_h: phys.1,
        }
    }

    pub fn is_one_to_one(&self) -> bool {
        self.logical_w == self.phys_w && self.logical_h == self.phys_h
    }

    /// Physical column of logical column `lx` (floor).
    #[inline]
    pub fn phys_x(&self, lx: u32) -> u32 {
        (u64::from(lx) * u64::from(self.phys_w) / u64::from(self.logical_w)) as u32
    }

    /// Physical row of logical row `ly` (floor).
    #[inline]
    pub fn phys_y(&self, ly: u32) -> u32 {
        (u64::from(ly) * u64::from(self.phys_h) / u64::from(self.logical_h)) as u32
    }

    #[inline]
    fn phys_x_ceil(&self, lx: u32) -> u32 {
        (u64::from(lx) * u64::from(self.phys_w)).div_ceil(u64::from(self.logical_w)) as u32
    }

    #[inline]
    fn phys_y_ceil(&self, ly: u32) -> u32 {
        (u64::from(ly) * u64::from(self.phys_h)).div_ceil(u64::from(self.logical_h)) as u32
    }

    /// Physical rect covering logical `r` grown by `margin` logical pixels on
    /// every side, clamped to the frame. `None` when empty.
    pub fn to_phys(&self, r: &Rect, margin: u32) -> Option<Rect> {
        let lx0 = r.x.saturating_sub(margin).min(self.logical_w);
        let ly0 = r.y.saturating_sub(margin).min(self.logical_h);
        let lx1 =
            r.x.saturating_add(r.width)
                .saturating_add(margin)
                .min(self.logical_w);
        let ly1 =
            r.y.saturating_add(r.height)
                .saturating_add(margin)
                .min(self.logical_h);
        if lx1 <= lx0 || ly1 <= ly0 {
            return None;
        }
        let px0 = self.phys_x(lx0);
        let py0 = self.phys_y(ly0);
        let px1 = self.phys_x_ceil(lx1).min(self.phys_w);
        let py1 = self.phys_y_ceil(ly1).min(self.phys_h);
        (px1 > px0 && py1 > py0).then(|| Rect::new(px0, py0, px1 - px0, py1 - py0))
    }

    /// Logical rect grown by `margin`, clamped to the logical frame.
    pub fn grow_logical(&self, r: &Rect, margin: u32) -> Option<Rect> {
        let x0 = r.x.saturating_sub(margin).min(self.logical_w);
        let y0 = r.y.saturating_sub(margin).min(self.logical_h);
        let x1 =
            r.x.saturating_add(r.width)
                .saturating_add(margin)
                .min(self.logical_w);
        let y1 =
            r.y.saturating_add(r.height)
                .saturating_add(margin)
                .min(self.logical_h);
        (x1 > x0 && y1 > y0).then(|| Rect::new(x0, y0, x1 - x0, y1 - y0))
    }
}

/// Clamp `r` to `buf`, returning `(x0, y0, x1, y1)` or `None` when empty.
#[inline]
fn clip(r: &Rect, w: u32, h: u32) -> Option<(usize, usize, usize, usize)> {
    let x0 = r.x.min(w);
    let y0 = r.y.min(h);
    let x1 = r.x.saturating_add(r.width).min(w);
    let y1 = r.y.saturating_add(r.height).min(h);
    (x1 > x0 && y1 > y0).then_some((x0 as usize, y0 as usize, x1 as usize, y1 as usize))
}

/// One axis of a bilinear sampling table: source taps and 8-bit weight of the
/// second tap for each destination index.
fn bilinear_taps(d0: usize, d1: usize, src_len: usize, dst_len: usize) -> Vec<(usize, usize, u32)> {
    let max = src_len.saturating_sub(1);
    let ratio = src_len as f32 / dst_len.max(1) as f32;
    (d0..d1)
        .map(|d| {
            let f = ((d as f32 + 0.5) * ratio - 0.5).max(0.0);
            let i0 = (f.floor() as usize).min(max);
            let i1 = (i0 + 1).min(max);
            let w = ((f - i0 as f32) * 256.0).round().clamp(0.0, 256.0) as u32;
            (i0, i1, w)
        })
        .collect()
}

/// Bilinear-resample `src` onto `dst` for the destination rect `r` only.
///
/// `src` and `dst` may have any sizes; when they match this is a plain copy.
/// Used for the background cache (UI damage) — never on the per-sample ink path.
pub fn upscale_bilinear_rect(src: &PixelBuf, dst: &mut PixelBuf, r: Rect) {
    let Some((x0, y0, x1, y1)) = clip(&r, dst.width, dst.height) else {
        return;
    };
    if src.width == 0 || src.height == 0 {
        return;
    }
    let ss = src.stride as usize;
    let ds = dst.stride as usize;
    if src.width == dst.width && src.height == dst.height {
        for y in y0..y1 {
            dst.data[y * ds + x0..y * ds + x1].copy_from_slice(&src.data[y * ss + x0..y * ss + x1]);
        }
        return;
    }
    let cols = bilinear_taps(x0, x1, src.width as usize, dst.width as usize);
    let rows = bilinear_taps(y0, y1, src.height as usize, dst.height as usize);
    for (ry, &(sy0, sy1, wy)) in rows.iter().enumerate() {
        let r0 = &src.data[sy0 * ss..sy0 * ss + ss];
        let r1 = &src.data[sy1 * ss..sy1 * ss + ss];
        let out = &mut dst.data[(y0 + ry) * ds + x0..(y0 + ry) * ds + x1];
        for (o, &(sx0, sx1, wx)) in out.iter_mut().zip(cols.iter()) {
            let top = u32::from(r0[sx0]) * (256 - wx) + u32::from(r0[sx1]) * wx;
            let bot = u32::from(r1[sx0]) * (256 - wx) + u32::from(r1[sx1]) * wx;
            *o = ((top * (256 - wy) + bot * wy + (1 << 15)) >> 16) as u8;
        }
    }
}

/// Resample a whole gray buffer to `dw × dh`: bilinear when enlarging, box
/// average when shrinking (keeps thin ink visible). Identity sizes clone.
pub fn resample_gray(src: &PixelBuf, dw: u32, dh: u32) -> PixelBuf {
    let mut dst = PixelBuf::new(dw, dh);
    if dw == 0 || dh == 0 || src.width == 0 || src.height == 0 {
        return dst;
    }
    if dw >= src.width && dh >= src.height {
        upscale_bilinear_rect(src, &mut dst, Rect::new(0, 0, dw, dh));
        return dst;
    }
    let (sw, sh) = (src.width as u64, src.height as u64);
    let ss = src.stride as usize;
    for y in 0..dh as u64 {
        let sy0 = (y * sh / dh as u64) as usize;
        let sy1 = ((y + 1) * sh).div_ceil(dh as u64).max(sy0 as u64 + 1) as usize;
        for x in 0..dw as u64 {
            let sx0 = (x * sw / dw as u64) as usize;
            let sx1 = ((x + 1) * sw).div_ceil(dw as u64).max(sx0 as u64 + 1) as usize;
            let mut sum = 0u32;
            for row in sy0..sy1.min(src.height as usize) {
                for col in sx0..sx1.min(src.width as usize) {
                    sum += u32::from(src.data[row * ss + col]);
                }
            }
            let n = ((sy1 - sy0) * (sx1 - sx0)) as u32;
            dst.data[y as usize * dw as usize + x as usize] = (sum / n.max(1)) as u8;
        }
    }
    dst
}

/// Ink planes composited over the background (canvas-sized, physical).
pub struct InkPlanes<'a> {
    /// Committed strokes layer.
    pub committed: &'a PixelBuf,
    /// Live stroke layer, when a stroke is in progress.
    pub live: Option<&'a PixelBuf>,
    /// Live layer encodes an eraser (max-blend) instead of ink (min-blend).
    pub live_is_eraser: bool,
    /// Physical frame row of the canvas top edge.
    pub canvas_y0: u32,
}

/// Write `present = min(bg, ink)` over physical rect `r` (1:1, integer only).
///
/// Pixels outside the ink planes (or all pixels when `ink` is `None`) copy the
/// background unchanged.
pub fn compose_rect(present: &mut PixelBuf, bg: &PixelBuf, ink: Option<&InkPlanes>, r: Rect) {
    let Some((x0, y0, x1, y1)) = clip(
        &r,
        present.width.min(bg.width),
        present.height.min(bg.height),
    ) else {
        return;
    };
    let ps = present.stride as usize;
    let bs = bg.stride as usize;
    for y in y0..y1 {
        let out = &mut present.data[y * ps + x0..y * ps + x1];
        let back = &bg.data[y * bs + x0..y * bs + x1];
        out.copy_from_slice(back);
        let Some(planes) = ink else {
            continue;
        };
        let Some(cy) = (y as u32).checked_sub(planes.canvas_y0) else {
            continue;
        };
        let c = planes.committed;
        if cy >= c.height {
            continue;
        }
        let ix1 = x1.min(c.width as usize);
        if ix1 <= x0 {
            continue;
        }
        let n = ix1 - x0;
        let crow = &c.data[cy as usize * c.stride as usize + x0..][..n];
        match planes.live.filter(|l| cy < l.height && l.width >= c.width) {
            Some(live) => {
                let lrow = &live.data[cy as usize * live.stride as usize + x0..][..n];
                if planes.live_is_eraser {
                    for ((o, &cv), &lv) in out[..n].iter_mut().zip(crow).zip(lrow) {
                        *o = (*o).min(cv.max(lv));
                    }
                } else {
                    for ((o, &cv), &lv) in out[..n].iter_mut().zip(crow).zip(lrow) {
                        *o = (*o).min(cv.min(lv));
                    }
                }
            }
            None => {
                for (o, &cv) in out[..n].iter_mut().zip(crow) {
                    *o = (*o).min(cv);
                }
            }
        }
    }
}

/// Copy `src` into `dst` for rect `r` (same-size buffers).
pub fn copy_rect(dst: &mut PixelBuf, src: &PixelBuf, r: Rect) {
    let Some((x0, y0, x1, y1)) = clip(&r, dst.width.min(src.width), dst.height.min(src.height))
    else {
        return;
    };
    let ds = dst.stride as usize;
    let ss = src.stride as usize;
    for y in y0..y1 {
        dst.data[y * ds + x0..y * ds + x1].copy_from_slice(&src.data[y * ss + x0..y * ss + x1]);
    }
}

/// Intersection of two rects, `None` when disjoint.
pub fn intersect(a: &Rect, b: &Rect) -> Option<Rect> {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = (a.x + a.width).min(b.x + b.width);
    let y1 = (a.y + a.height).min(b.y + b.height);
    (x1 > x0 && y1 > y0).then(|| Rect::new(x0, y0, x1 - x0, y1 - y0))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PINENOTE: FrameGeom = FrameGeom {
        logical_w: 800,
        logical_h: 1067,
        phys_w: 1404,
        phys_h: 1872,
    };

    #[test]
    fn to_phys_covers_logical_rect() {
        let r = PINENOTE.to_phys(&Rect::new(100, 200, 10, 10), 0).unwrap();
        assert_eq!(r.x, 175);
        assert_eq!(r.y, 350);
        assert!(r.x + r.width >= (110.0f32 * 1.755) as u32);
        assert!(r.y + r.height >= (210.0f32 * 1.7544) as u32);
        let full = PINENOTE.to_phys(&Rect::new(0, 0, 800, 1067), 4).unwrap();
        assert_eq!(full, Rect::new(0, 0, 1404, 1872));
    }

    #[test]
    fn upscale_identity_is_copy() {
        let mut src = PixelBuf::new(4, 4);
        src.set_pixel(1, 2, 7);
        let mut dst = PixelBuf::new(4, 4);
        upscale_bilinear_rect(&src, &mut dst, Rect::new(0, 0, 4, 4));
        assert_eq!(dst.data, src.data);
    }

    #[test]
    fn upscale_blends_between_logical_pixels() {
        let mut src = PixelBuf::new(2, 2);
        src.set_pixel(0, 0, 0);
        let mut dst = PixelBuf::new(4, 4);
        upscale_bilinear_rect(&src, &mut dst, Rect::new(0, 0, 4, 4));
        assert_eq!(dst.data[0], 0);
        assert_eq!(dst.data[3], 255);
        assert!((1..255).contains(&dst.data[5]), "got {}", dst.data[5]);
    }

    #[test]
    fn compose_min_blends_ink_over_background() {
        let mut bg = PixelBuf::new(4, 4);
        bg.fill(200);
        let mut committed = PixelBuf::new(4, 3);
        committed.set_pixel(1, 0, 10);
        let mut live = PixelBuf::new(4, 3);
        live.set_pixel(2, 0, 20);
        let mut present = PixelBuf::new(4, 4);
        let planes = InkPlanes {
            committed: &committed,
            live: Some(&live),
            live_is_eraser: false,
            canvas_y0: 1,
        };
        compose_rect(&mut present, &bg, Some(&planes), Rect::new(0, 0, 4, 4));
        assert_eq!(present.data[0..4], [200, 200, 200, 200], "row above canvas");
        assert_eq!(present.data[4..8], [200, 10, 20, 200]);
    }

    #[test]
    fn resample_down_keeps_thin_ink() {
        let mut src = PixelBuf::new(1404, 10);
        for y in 0..10 {
            src.set_pixel(700, y, 0);
        }
        let dst = resample_gray(&src, 800, 6);
        assert!(dst.data.iter().any(|&v| v < 200));
        assert_eq!((dst.width, dst.height), (800, 6));
    }
}
