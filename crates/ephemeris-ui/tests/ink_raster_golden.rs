//! Geometric golden tests for panel-resolution ink (ephemeris-ktg.4).
//!
//! A circle and the handwritten word "ago" are rasterised at the PineNote ink
//! scale (s = 1404/800 = 1.755) and measured against their analytic outline:
//!
//! * **width** — coverage integrated along the stroke normal; must vary < 1 px
//!   along a constant-pressure stroke;
//! * **edge** — sub-pixel position where coverage crosses 50 % on each side of
//!   the stroke; a stair-step shows up as an edge displaced from the outline.
//!
//! The same strokes rendered at logical resolution and nearest-upscaled (the
//! pre-ktg.4 Fast present) are measured too, proving the physical raster is
//! measurably smoother.
//!
//! Set `EPHEMERIS_GOLDEN_DIR=/some/dir` to dump PGMs for visual inspection.

use ephemeris_core::ink::WidthConfig;
use ephemeris_core::model::{Color, Point, Stroke, Tool};
use ephemeris_pal::display::PixelBuf;
use ephemeris_ui::raster::{rasterize_stroke, InkScale};

const S: f32 = 1404.0 / 800.0;
const BASE_WIDTH: f32 = 3.0;
const PRESSURE: f32 = 0.5;

/// Logical half-width of a constant-pressure stroke ([`WidthConfig::default`]).
fn logical_hw() -> f32 {
    WidthConfig::default().width_for(BASE_WIDTH, PRESSURE) * 0.5
}

fn stroke(points: &[(f32, f32)]) -> Stroke {
    let mut s = Stroke::new(Tool::Pen, Color::BLACK, BASE_WIDTH);
    for (i, &(x, y)) in points.iter().enumerate() {
        s.points.push(Point {
            x,
            y,
            pressure: PRESSURE,
            tilt: 0.0,
            t_ms: i as u32 * 8,
        });
    }
    s
}

/// Points on an ellipse arc, ~`step` logical px apart (pen-sample density).
fn arc(cx: f32, cy: f32, rx: f32, ry: f32, a0: f32, a1: f32, step: f32) -> Vec<(f32, f32)> {
    let len = (a1 - a0).abs() * rx.max(ry);
    let n = (len / step).ceil().max(2.0) as usize;
    (0..=n)
        .map(|i| {
            let a = a0 + (a1 - a0) * i as f32 / n as f32;
            (cx + rx * a.cos(), cy + ry * a.sin())
        })
        .collect()
}

fn line(a: (f32, f32), b: (f32, f32), step: f32) -> Vec<(f32, f32)> {
    let len = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
    let n = (len / step).ceil().max(1.0) as usize;
    (0..=n)
        .map(|i| {
            let t = i as f32 / n as f32;
            (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
        })
        .collect()
}

/// Handwriting-sized "ago" (x-height ≈ 24 logical px ≈ 5 mm on PineNote).
fn ago_strokes() -> Vec<Stroke> {
    use std::f32::consts::PI;
    let step = 2.0;
    let mut out = Vec::new();
    // a: bowl + stem.
    out.push(stroke(&arc(
        30.0,
        40.0,
        10.0,
        12.0,
        -0.2,
        2.0 * PI - 0.2,
        step,
    )));
    out.push(stroke(&line((40.0, 28.0), (40.0, 52.0), step)));
    // g: bowl + descender with hook.
    out.push(stroke(&arc(
        70.0,
        40.0,
        10.0,
        12.0,
        -0.2,
        2.0 * PI - 0.2,
        step,
    )));
    let mut g = line((80.0, 28.0), (80.0, 66.0), step);
    g.extend(
        arc(70.0, 66.0, 10.0, 8.0, 0.0, PI, step)
            .into_iter()
            .skip(1),
    );
    out.push(stroke(&g));
    // o.
    out.push(stroke(&arc(110.0, 40.0, 10.0, 12.0, 0.0, 2.0 * PI, step)));
    out
}

fn circle_strokes() -> Vec<Stroke> {
    vec![stroke(&arc(
        60.0,
        60.0,
        40.0,
        40.0,
        0.0,
        2.0 * std::f32::consts::PI,
        2.0,
    ))]
}

/// Block-capitals "Mmmm" — four humps, handwriting x-height ≈ 28 logical px.
fn mmmm_strokes() -> Vec<Stroke> {
    let step = 2.0;
    let mut out = Vec::new();
    // Capital M.
    out.push(stroke(&line((20.0, 52.0), (20.0, 20.0), step)));
    out.push(stroke(&line((20.0, 20.0), (34.0, 40.0), step)));
    out.push(stroke(&line((34.0, 40.0), (48.0, 20.0), step)));
    out.push(stroke(&line((48.0, 20.0), (48.0, 52.0), step)));
    // Three lowercase m arches (each: up-arch-down × 2).
    for i in 0..3 {
        let x0 = 62.0 + i as f32 * 36.0;
        let mut pts = line((x0, 52.0), (x0, 28.0), step);
        pts.extend(
            arc(x0 + 8.0, 28.0, 8.0, 10.0, std::f32::consts::PI, 0.0, step)
                .into_iter()
                .skip(1),
        );
        pts.extend(
            arc(x0 + 24.0, 28.0, 8.0, 10.0, std::f32::consts::PI, 0.0, step)
                .into_iter()
                .skip(1),
        );
        pts.extend(
            line((x0 + 32.0, 28.0), (x0 + 32.0, 52.0), step)
                .into_iter()
                .skip(1),
        );
        out.push(stroke(&pts));
    }
    out
}

/// German ß (Eszett) — tall bowl + descender hook (synthetic).
fn eszett_strokes() -> Vec<Stroke> {
    use std::f32::consts::PI;
    let step = 2.0;
    let mut pts = arc(40.0, 28.0, 14.0, 12.0, PI * 0.85, -0.15, step);
    pts.extend(
        arc(40.0, 48.0, 14.0, 14.0, -0.15, PI * 0.75, step)
            .into_iter()
            .skip(1),
    );
    pts.extend(
        arc(28.0, 62.0, 10.0, 8.0, 0.0, PI, step)
            .into_iter()
            .skip(1),
    );
    vec![stroke(&pts)]
}

/// Archimedean spiral — continuous curvature for AA / width stability.
fn spiral_strokes() -> Vec<Stroke> {
    let step = 2.0;
    let mut pts = Vec::new();
    let turns = 2.5_f32;
    let n = 120;
    for i in 0..=n {
        let t = i as f32 / n as f32;
        let a = t * turns * 2.0 * std::f32::consts::PI;
        let r = 6.0 + t * 42.0;
        pts.push((70.0 + r * a.cos(), 70.0 + r * a.sin()));
    }
    // Resample at ~step spacing.
    let mut out = vec![pts[0]];
    for &p in &pts[1..] {
        let last = *out.last().unwrap();
        if ((p.0 - last.0).powi(2) + (p.1 - last.1).powi(2)).sqrt() >= step * 0.85 {
            out.push(p);
        }
    }
    vec![stroke(&out)]
}

/// Dark-pixel count + bbox smoke for glyphs too branched for the edge profile.
fn assert_physical_ink(name: &str, strokes: &[Stroke], lw: u32, lh: u32, min_dark: usize) {
    let buf = render_physical(strokes, lw, lh);
    dump(name, &buf);
    let dark = buf.data.iter().filter(|&&p| p < 200).count();
    assert!(
        dark >= min_dark,
        "{name}: expected ≥{min_dark} dark px, got {dark}"
    );
    // At least some anti-aliased fringe (not a binary stamp).
    let aa = buf.data.iter().filter(|&&p| (1..254).contains(&p)).count();
    assert!(aa > 50, "{name}: expected AA fringe, got {aa} mid-tone px");
}

/// Ink plane at panel resolution (the ktg.4 path).
fn render_physical(strokes: &[Stroke], lw: u32, lh: u32) -> PixelBuf {
    let mut buf = PixelBuf::new((lw as f32 * S) as u32, (lh as f32 * S) as u32);
    for s in strokes {
        rasterize_stroke(&mut buf, s, InkScale::uniform(S));
    }
    buf
}

/// Logical raster nearest-upscaled ×S (the pre-ktg.4 Fast present).
fn render_nearest_upscaled(strokes: &[Stroke], lw: u32, lh: u32) -> PixelBuf {
    let mut logical = PixelBuf::new(lw, lh);
    for s in strokes {
        rasterize_stroke(&mut logical, s, InkScale::IDENTITY);
    }
    let pw = (lw as f32 * S) as u32;
    let ph = (lh as f32 * S) as u32;
    let mut out = PixelBuf::new(pw, ph);
    for py in 0..ph {
        let ly = (((py as f32 + 0.5) / S).floor() as u32).min(lh - 1);
        for px in 0..pw {
            let lx = (((px as f32 + 0.5) / S).floor() as u32).min(lw - 1);
            out.data[(py * pw + px) as usize] = logical.data[(ly * lw + lx) as usize];
        }
    }
    out
}

/// Ink coverage (0 = paper, 1 = full ink) sampled bilinearly at pixel-space
/// position `(x, y)` (pixel centres at +0.5).
fn coverage(buf: &PixelBuf, x: f32, y: f32) -> f32 {
    let fx = x - 0.5;
    let fy = y - 0.5;
    let x0 = fx.floor();
    let y0 = fy.floor();
    let tx = fx - x0;
    let ty = fy - y0;
    let at = |xi: f32, yi: f32| -> f32 {
        if xi < 0.0 || yi < 0.0 || xi >= buf.width as f32 || yi >= buf.height as f32 {
            return 0.0;
        }
        (255 - buf.data[(yi as u32 * buf.stride + xi as u32) as usize]) as f32 / 255.0
    };
    let top = at(x0, y0) * (1.0 - tx) + at(x0 + 1.0, y0) * tx;
    let bot = at(x0, y0 + 1.0) * (1.0 - tx) + at(x0 + 1.0, y0 + 1.0) * tx;
    top * (1.0 - ty) + bot * ty
}

/// Measurements along stroke normals.
#[derive(Debug)]
struct Profile {
    /// Effective widths (∫ coverage along the normal), physical px.
    widths: Vec<f32>,
    /// |50 % edge − ideal outline| on either side, physical px.
    edge_errors: Vec<f32>,
}

impl Profile {
    fn width_range(&self) -> f32 {
        let max = self.widths.iter().cloned().fold(f32::MIN, f32::max);
        let min = self.widths.iter().cloned().fold(f32::MAX, f32::min);
        max - min
    }

    fn max_edge_error(&self) -> f32 {
        self.edge_errors.iter().cloned().fold(0.0, f32::max)
    }

    /// Edge samples displaced by more than half a physical pixel — the
    /// signature of an isolated stair-step.
    fn stair_steps(&self) -> usize {
        self.edge_errors.iter().filter(|&&e| e > 0.5).count()
    }
}

/// Distance along `dir` from `p` where coverage first drops below 50 %.
fn edge_distance(buf: &PixelBuf, p: (f32, f32), dir: (f32, f32), max: f32) -> f32 {
    const STEP: f32 = 0.02;
    let mut prev = coverage(buf, p.0, p.1);
    let mut t = 0.0;
    while t < max {
        let next_t = t + STEP;
        let c = coverage(buf, p.0 + dir.0 * next_t, p.1 + dir.1 * next_t);
        if prev >= 0.5 && c < 0.5 {
            return t + STEP * (prev - 0.5) / (prev - c).max(1e-6);
        }
        prev = c;
        t = next_t;
    }
    max
}

/// Sample every stroke vertex whose normal neighbourhood is free of other ink
/// (crossings, the other half of a bowl, stroke ends).
fn profile(buf: &PixelBuf, strokes: &[Stroke]) -> Profile {
    let hw = logical_hw() * S;
    let reach = hw + 3.0;
    let all: Vec<(usize, usize, f32, f32)> = strokes
        .iter()
        .enumerate()
        .flat_map(|(si, s)| {
            s.points
                .iter()
                .enumerate()
                .map(move |(pi, p)| (si, pi, p.x * S, p.y * S))
        })
        .collect();

    let mut widths = Vec::new();
    let mut edge_errors = Vec::new();
    for (si, s) in strokes.iter().enumerate() {
        let n = s.points.len();
        for i in 3..n.saturating_sub(3) {
            let (px, py) = (s.points[i].x * S, s.points[i].y * S);
            let (ax, ay) = (s.points[i - 1].x * S, s.points[i - 1].y * S);
            let (bx, by) = (s.points[i + 1].x * S, s.points[i + 1].y * S);
            let (tx, ty) = (bx - ax, by - ay);
            let tl = (tx * tx + ty * ty).sqrt();
            let normal = (-ty / tl, tx / tl);

            let crowded = all.iter().any(|&(sj, pj, qx, qy)| {
                let near = ((qx - px).powi(2) + (qy - py).powi(2)).sqrt() < 2.0 * reach;
                near && (sj != si || pj.abs_diff(i) > 4)
            });
            if crowded {
                continue;
            }

            const STEP: f32 = 0.05;
            let mut w = 0.0;
            let mut t = -reach;
            while t <= reach {
                w += coverage(buf, px + normal.0 * t, py + normal.1 * t) * STEP;
                t += STEP;
            }
            widths.push(w);
            for dir in [normal, (-normal.0, -normal.1)] {
                let d = edge_distance(buf, (px, py), dir, reach);
                edge_errors.push((d - hw).abs());
            }
        }
    }
    assert!(
        widths.len() >= 20,
        "too few clean samples ({})",
        widths.len()
    );
    Profile {
        widths,
        edge_errors,
    }
}

fn dump(name: &str, buf: &PixelBuf) {
    let Some(dir) = std::env::var_os("EPHEMERIS_GOLDEN_DIR") else {
        return;
    };
    let path = std::path::Path::new(&dir).join(format!("{name}.pgm"));
    let mut bytes = format!("P5\n{} {}\n255\n", buf.width, buf.height).into_bytes();
    bytes.extend_from_slice(&buf.data);
    std::fs::write(&path, bytes).expect("write golden dump");
}

fn check(name: &str, strokes: &[Stroke], lw: u32, lh: u32) {
    let phys = render_physical(strokes, lw, lh);
    let nearest = render_nearest_upscaled(strokes, lw, lh);
    dump(&format!("{name}_physical"), &phys);
    dump(&format!("{name}_nearest"), &nearest);

    let p = profile(&phys, strokes);
    let q = profile(&nearest, strokes);
    eprintln!(
        "{name}: physical width_range={:.3} max_edge={:.3} steps={} | nearest width_range={:.3} max_edge={:.3} steps={}",
        p.width_range(),
        p.max_edge_error(),
        p.stair_steps(),
        q.width_range(),
        q.max_edge_error(),
        q.stair_steps(),
    );

    let ideal = 2.0 * logical_hw() * S;
    let mean = p.widths.iter().sum::<f32>() / p.widths.len() as f32;
    assert!(
        (mean - ideal).abs() < 0.5,
        "{name}: mean width {mean:.2} px should match ideal {ideal:.2} px"
    );
    assert!(
        p.width_range() < 1.0,
        "{name}: constant-pressure width must vary < 1 px (got {:.3})",
        p.width_range()
    );
    assert_eq!(
        p.stair_steps(),
        0,
        "{name}: physical raster has stair-step edges (max error {:.3} px)",
        p.max_edge_error()
    );

    // The old nearest-upscale path is measurably worse on the same strokes.
    assert!(
        q.max_edge_error() > p.max_edge_error() + 0.2,
        "{name}: nearest ({:.3}) should be clearly rougher than physical ({:.3})",
        q.max_edge_error(),
        p.max_edge_error()
    );
    assert!(
        q.stair_steps() > 0,
        "{name}: expected stair-steps in the nearest-upscaled reference"
    );
}

#[test]
fn circle_at_pinenote_scale_is_smooth() {
    check("circle", &circle_strokes(), 120, 120);
}

#[test]
fn ago_at_pinenote_scale_is_smooth() {
    check("ago", &ago_strokes(), 140, 90);
}

#[test]
fn mmmm_at_pinenote_scale_has_ink() {
    assert_physical_ink("mmmm", &mmmm_strokes(), 200, 80, 800);
}

#[test]
fn eszett_at_pinenote_scale_has_ink() {
    assert_physical_ink("eszett", &eszett_strokes(), 80, 90, 400);
}

#[test]
fn spiral_at_pinenote_scale_is_smooth() {
    // Outer turns have clean normals for the edge profile.
    check("spiral", &spiral_strokes(), 150, 150);
}

#[test]
fn physical_raster_differs_from_nearest_upscale() {
    let strokes = ago_strokes();
    let phys = render_physical(&strokes, 140, 90);
    let nearest = render_nearest_upscaled(&strokes, 140, 90);
    assert_eq!((phys.width, phys.height), (nearest.width, nearest.height));
    let differing = phys
        .data
        .iter()
        .zip(&nearest.data)
        .filter(|(a, b)| a.abs_diff(**b) > 32)
        .count();
    assert!(
        differing > 200,
        "physical raster should not equal a nearest-upscaled logical raster ({differing} px differ)"
    );
}
