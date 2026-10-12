//! Line-image handwriting recognition helpers (ephemeris-9hh.1).
//!
//! Splits a page gray8 into horizontal ink bands, resizes each to height 64
//! for a CTC line model (DE·HTR v2 / similar), and provides preprocess tensors
//! for RTen. The ONNX session lives in [`crate::transcriber::LineImageHtrTranscriber`].

/// One horizontal ink band in source page coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageLineBand {
    pub y0: u32,
    pub y1: u32,
    pub x0: u32,
    pub x1: u32,
}

/// Target geometry for DE·HTR v2-class models.
pub const LINE_HTR_HEIGHT: u32 = 64;
pub const LINE_HTR_MAX_WIDTH: u32 = 1024;
pub const LINE_HTR_WIDTH_ALIGN: u32 = 4;

/// Alphabet for `naeyn/de-htr-web-v2` (111 chars; blank is class 0 at runtime).
pub const DE_HTR_V2_ALPHABET: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZäöüÄÖÜß0123456789 .,;:!?\"'()[]{}<>-–—_/\\@#%&*+=|€§°²³→←↑↓×÷";

/// Ink darkness threshold (gray8); below this counts as ink.
const INK_THRESHOLD: u8 = 200;

/// Split a page into horizontal handwriting lines via row-ink projection.
///
/// Returns bands in top-to-bottom order. Empty when no ink is found.
pub fn split_ink_line_bands(width: u32, height: u32, gray8: &[u8]) -> Vec<ImageLineBand> {
    let expected = (width as usize).saturating_mul(height as usize);
    if width == 0 || height == 0 || gray8.len() < expected {
        return Vec::new();
    }

    let mut row_ink = vec![false; height as usize];
    for y in 0..height {
        let mut ink = false;
        for x in 0..width {
            if gray8[(y * width + x) as usize] < INK_THRESHOLD {
                ink = true;
                break;
            }
        }
        row_ink[y as usize] = ink;
    }

    // Gap between lines: ~0.35× median ink-run height, floored at 6 px.
    let mut run_heights = Vec::new();
    let mut y = 0u32;
    while y < height {
        if !row_ink[y as usize] {
            y += 1;
            continue;
        }
        let start = y;
        while y < height && row_ink[y as usize] {
            y += 1;
        }
        run_heights.push(y - start);
    }
    if run_heights.is_empty() {
        return Vec::new();
    }
    run_heights.sort_unstable();
    let med = run_heights[run_heights.len() / 2].max(1);
    let merge_gap = ((med as f32) * 0.35).round().max(6.0) as u32;

    let mut bands_y: Vec<(u32, u32)> = Vec::new();
    y = 0;
    while y < height {
        if !row_ink[y as usize] {
            y += 1;
            continue;
        }
        let start = y;
        let mut end = y;
        while y < height {
            if row_ink[y as usize] {
                end = y + 1;
                y += 1;
            } else {
                // Peek across a small white gap to merge broken lines.
                let mut gap = 0u32;
                let mut yy = y;
                while yy < height && !row_ink[yy as usize] && gap < merge_gap {
                    gap += 1;
                    yy += 1;
                }
                if yy < height && row_ink[yy as usize] && gap <= merge_gap {
                    y = yy;
                    continue;
                }
                break;
            }
        }
        bands_y.push((start, end));
        y = end;
    }

    let pad = 4u32;
    bands_y
        .into_iter()
        .filter_map(|(y0, y1)| {
            if y1 <= y0 {
                return None;
            }
            let mut min_x = width;
            let mut max_x = 0u32;
            let mut any = false;
            for yy in y0..y1 {
                for x in 0..width {
                    if gray8[(yy * width + x) as usize] < INK_THRESHOLD {
                        any = true;
                        min_x = min_x.min(x);
                        max_x = max_x.max(x);
                    }
                }
            }
            if !any {
                return None;
            }
            let x0 = min_x.saturating_sub(pad);
            let x1 = (max_x + 1 + pad).min(width);
            let y0p = y0.saturating_sub(pad / 2);
            let y1p = (y1 + pad / 2).min(height);
            Some(ImageLineBand {
                y0: y0p,
                y1: y1p,
                x0,
                x1,
            })
        })
        .collect()
}

/// Prepared line tensor for DE·HTR-style models: `1 × 1 × H × W` float32 in `[0, 1]`.
#[derive(Debug, Clone)]
pub struct LineHtrInput {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<f32>,
    /// Source band (for placing [`crate::transcriber::TextSpan`]s).
    pub band: ImageLineBand,
}

/// Crop `band` from `gray8`, resize to height [`LINE_HTR_HEIGHT`], right-pad
/// width to a multiple of [`LINE_HTR_WIDTH_ALIGN`] (cap [`LINE_HTR_MAX_WIDTH`]),
/// and scale to `[0, 1]` (white = 1.0).
pub fn prepare_line_htr_input(
    width: u32,
    height: u32,
    gray8: &[u8],
    band: &ImageLineBand,
) -> Option<LineHtrInput> {
    let expected = (width as usize).saturating_mul(height as usize);
    if gray8.len() < expected || band.x1 <= band.x0 || band.y1 <= band.y0 {
        return None;
    }
    let cw = band.x1 - band.x0;
    let ch = band.y1 - band.y0;
    if cw == 0 || ch == 0 {
        return None;
    }

    let mut crop = vec![255u8; (cw as usize) * (ch as usize)];
    for y in 0..ch {
        let src = ((band.y0 + y) * width + band.x0) as usize;
        let dst = (y * cw) as usize;
        crop[dst..dst + cw as usize].copy_from_slice(&gray8[src..src + cw as usize]);
    }

    // Scale to height 64, preserve aspect; optionally shrink if too wide.
    let scale = LINE_HTR_HEIGHT as f32 / ch as f32;
    let mut tw = ((cw as f32) * scale).round().max(1.0) as u32;
    let mut th = LINE_HTR_HEIGHT;
    if tw > LINE_HTR_MAX_WIDTH {
        let s2 = LINE_HTR_MAX_WIDTH as f32 / tw as f32;
        tw = LINE_HTR_MAX_WIDTH;
        th = ((th as f32) * s2).round().max(1.0) as u32;
    }
    let mut scaled = vec![255u8; (tw as usize) * (th as usize)];
    for y in 0..th {
        let sy = ((y as f32 + 0.5) * ch as f32 / th as f32).floor() as u32;
        let sy = sy.min(ch - 1);
        for x in 0..tw {
            let sx = ((x as f32 + 0.5) * cw as f32 / tw as f32).floor() as u32;
            let sx = sx.min(cw - 1);
            scaled[(y * tw + x) as usize] = crop[(sy * cw + sx) as usize];
        }
    }

    // Right-pad to multiple of 4 with white.
    let pad_w = ((tw + LINE_HTR_WIDTH_ALIGN - 1) / LINE_HTR_WIDTH_ALIGN) * LINE_HTR_WIDTH_ALIGN;
    let pad_w = pad_w.min(LINE_HTR_MAX_WIDTH).max(tw);
    let mut pixels = vec![1.0f32; (pad_w as usize) * (LINE_HTR_HEIGHT as usize)];
    // If th < 64 (extreme shrink), center vertically on white.
    let y_off = (LINE_HTR_HEIGHT.saturating_sub(th)) / 2;
    for y in 0..th {
        for x in 0..tw {
            let g = scaled[(y * tw + x) as usize] as f32 / 255.0;
            pixels[((y + y_off) * pad_w + x) as usize] = g;
        }
    }

    Some(LineHtrInput {
        width: pad_w,
        height: LINE_HTR_HEIGHT,
        pixels,
        band: band.clone(),
    })
}

/// Greedy CTC decode for raw logits `[T * C]` (not necessarily log-softmax).
pub fn ctc_greedy_decode_logits(
    logits: &[f32],
    time_steps: usize,
    num_classes: usize,
    blank: usize,
) -> Vec<usize> {
    crate::stroke_htr::ctc_greedy_decode(logits, time_steps, num_classes, blank)
}

/// Map blank-first class indices through an alphabet string (no blank char).
pub fn decode_line_alphabet(indices: &[usize], alphabet: &str, blank: usize) -> String {
    crate::stroke_htr::decode_charset_with_blank(indices, alphabet, blank)
}

/// Mean peak softmax confidence over non-blank timesteps (raw logits OK).
pub fn line_logits_confidence(
    logits: &[f32],
    time_steps: usize,
    num_classes: usize,
    blank: usize,
) -> f32 {
    crate::stroke_htr::ctc_softmax_confidence(logits, time_steps, num_classes, blank)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alphabet_len_matches_de_htr_v2() {
        assert_eq!(DE_HTR_V2_ALPHABET.chars().count(), 111);
    }

    #[test]
    fn splits_two_ink_rows() {
        let w = 40u32;
        let h = 40u32;
        let mut g = vec![255u8; (w * h) as usize];
        for y in 5..12 {
            for x in 4..30 {
                g[(y * w + x) as usize] = 0;
            }
        }
        for y in 25..32 {
            for x in 6..28 {
                g[(y * w + x) as usize] = 0;
            }
        }
        let bands = split_ink_line_bands(w, h, &g);
        assert_eq!(bands.len(), 2);
        assert!(bands[0].y1 <= bands[1].y0);
    }

    #[test]
    fn prepare_line_resizes_to_height_64() {
        let w = 80u32;
        let h = 40u32;
        let mut g = vec![255u8; (w * h) as usize];
        for y in 8..20 {
            for x in 10..60 {
                g[(y * w + x) as usize] = 0;
            }
        }
        let band = ImageLineBand {
            y0: 6,
            y1: 22,
            x0: 8,
            x1: 62,
        };
        let prep = prepare_line_htr_input(w, h, &g, &band).expect("prep");
        assert_eq!(prep.height, 64);
        assert_eq!(prep.width % 4, 0);
        assert!(prep.width <= LINE_HTR_MAX_WIDTH);
        assert_eq!(prep.pixels.len(), (prep.width * prep.height) as usize);
        // Some ink (< 1.0) present.
        assert!(prep.pixels.iter().any(|&p| p < 0.5));
    }
}
