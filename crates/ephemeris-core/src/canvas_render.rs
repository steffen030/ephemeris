//! Software render of a canvas board for e-ink overview (gray8).
//!
//! High-contrast rectangles + 2px edges; no soft anti-alias. Coordinates are
//! board-space; `Viewport` maps board → screen.

use crate::canvas_store::{CanvasDoc, CanvasView, CanvasZoomPreset};
use crate::json_canvas::{CanvasNode, NodeSide};

const WHITE: u8 = 255;
const BLACK: u8 = 0;
const GRAY: u8 = 180;
const SELECT: u8 = 40;

/// Screen viewport over the board.
#[derive(Debug, Clone, Copy)]
pub struct Viewport {
    pub origin_x: f32,
    pub origin_y: f32,
    /// Screen pixels per board unit (1.0 = 100%).
    pub scale: f32,
}

impl Viewport {
    pub fn from_view(view: &CanvasView, screen_w: u32, screen_h: u32, doc: &CanvasDoc) -> Self {
        let scale = match view.zoom {
            CanvasZoomPreset::Fit => fit_scale(doc, screen_w, screen_h),
            CanvasZoomPreset::OneHundred | CanvasZoomPreset::FocusSelected => {
                let s = if view.scale.is_finite() && view.scale > 0.0 {
                    view.scale
                } else {
                    1.0
                };
                s.clamp(MIN_BOARD_SCALE, MAX_BOARD_SCALE)
            }
        };
        Self {
            origin_x: view.x,
            origin_y: view.y,
            scale,
        }
    }

    pub fn board_to_screen(&self, bx: f32, by: f32) -> (f32, f32) {
        (
            (bx - self.origin_x) * self.scale,
            (by - self.origin_y) * self.scale,
        )
    }

    pub fn screen_to_board(&self, sx: f32, sy: f32) -> (f32, f32) {
        (
            sx / self.scale + self.origin_x,
            sy / self.scale + self.origin_y,
        )
    }
}

/// Pinch / continuous zoom clamps (board space → screen).
pub const MIN_BOARD_SCALE: f32 = 0.25;
pub const MAX_BOARD_SCALE: f32 = 4.0;

fn fit_scale(doc: &CanvasDoc, screen_w: u32, screen_h: u32) -> f32 {
    let Some((min_x, min_y, max_x, max_y)) = content_bounds(doc) else {
        return 1.0;
    };
    let bw = (max_x - min_x).max(1.0) + 80.0;
    let bh = (max_y - min_y).max(1.0) + 80.0;
    let sx = screen_w as f32 / bw;
    let sy = screen_h as f32 / bh;
    sx.min(sy).clamp(MIN_BOARD_SCALE, MAX_BOARD_SCALE)
}

/// Zoom the view around a screen-space point by `factor` (e.g. 1.05 = +5%).
///
/// Converts Fit/Focus presets into a continuous scale first so pinch feels smooth.
pub fn zoom_view_at(
    view: &mut CanvasView,
    doc: &CanvasDoc,
    screen_w: u32,
    screen_h: u32,
    factor: f32,
    screen_x: f32,
    screen_y: f32,
) {
    if !factor.is_finite() || factor <= 0.0 {
        return;
    }
    let vp = Viewport::from_view(view, screen_w, screen_h, doc);
    let (bx, by) = vp.screen_to_board(screen_x, screen_y);
    let new_scale = (vp.scale * factor).clamp(MIN_BOARD_SCALE, MAX_BOARD_SCALE);
    view.zoom = CanvasZoomPreset::OneHundred;
    view.scale = new_scale;
    view.x = bx - screen_x / new_scale;
    view.y = by - screen_y / new_scale;
}

fn content_bounds(doc: &CanvasDoc) -> Option<(f32, f32, f32, f32)> {
    if doc.nodes.is_empty() {
        return None;
    }
    let mut min_x = f32::MAX;
    let mut min_y = f32::MAX;
    let mut max_x = f32::MIN;
    let mut max_y = f32::MIN;
    for n in &doc.nodes {
        min_x = min_x.min(n.x() as f32);
        min_y = min_y.min(n.y() as f32);
        max_x = max_x.max((n.x() + n.width()) as f32);
        max_y = max_y.max((n.y() + n.height()) as f32);
    }
    Some((min_x, min_y, max_x, max_y))
}

/// Adjust view origin so Fit frames all content.
pub fn view_for_fit(doc: &CanvasDoc, screen_w: u32, screen_h: u32) -> CanvasView {
    let scale = fit_scale(doc, screen_w, screen_h);
    let (ox, oy) = if let Some((min_x, min_y, max_x, max_y)) = content_bounds(doc) {
        let bw = max_x - min_x;
        let bh = max_y - min_y;
        let cx = min_x + bw * 0.5;
        let cy = min_y + bh * 0.5;
        (
            cx - (screen_w as f32 * 0.5) / scale,
            cy - (screen_h as f32 * 0.5) / scale,
        )
    } else {
        (-40.0, -40.0)
    };
    CanvasView {
        x: ox,
        y: oy,
        zoom: CanvasZoomPreset::Fit,
        scale,
    }
}

/// Focus viewport on one node at 100%.
pub fn view_for_focus(doc: &CanvasDoc, node_id: &str, screen_w: u32, screen_h: u32) -> CanvasView {
    if let Some(n) = doc.find_node(node_id) {
        let cx = n.x() as f32 + n.width() as f32 * 0.5;
        let cy = n.y() as f32 + n.height() as f32 * 0.5;
        return CanvasView {
            x: cx - screen_w as f32 * 0.5,
            y: cy - screen_h as f32 * 0.5,
            zoom: CanvasZoomPreset::FocusSelected,
            scale: 1.0,
        };
    }
    CanvasView::default()
}

/// Hit-test topmost non-group node, then groups.
pub fn hit_test(doc: &CanvasDoc, vp: &Viewport, sx: f32, sy: f32) -> Option<String> {
    let (bx, by) = vp.screen_to_board(sx, sy);
    // Reverse z-order (last = top).
    for n in doc.nodes.iter().rev() {
        if matches!(n, CanvasNode::Group(_)) {
            continue;
        }
        if point_in_node(n, bx, by) {
            return Some(n.id().to_string());
        }
    }
    for n in doc.nodes.iter().rev() {
        if matches!(n, CanvasNode::Group(_)) && point_in_node(n, bx, by) {
            return Some(n.id().to_string());
        }
    }
    None
}

fn point_in_node(n: &CanvasNode, bx: f32, by: f32) -> bool {
    let x = n.x() as f32;
    let y = n.y() as f32;
    let w = n.width() as f32;
    let h = n.height() as f32;
    bx >= x && by >= y && bx < x + w && by < y + h
}

/// Which side of a node was tapped (for Connect mode), within `margin` board px.
pub fn hit_side(doc: &CanvasDoc, node_id: &str, bx: f32, by: f32, margin: f32) -> Option<NodeSide> {
    let n = doc.find_node(node_id)?;
    let x = n.x() as f32;
    let y = n.y() as f32;
    let w = n.width() as f32;
    let h = n.height() as f32;
    if bx < x - margin || by < y - margin || bx > x + w + margin || by > y + h + margin {
        return None;
    }
    let dl = (bx - x).abs();
    let dr = (bx - (x + w)).abs();
    let dt = (by - y).abs();
    let db = (by - (y + h)).abs();
    let m = dl.min(dr).min(dt).min(db);
    if m > margin {
        // Interior — pick nearest side anyway for Connect UX.
    }
    if m == dl {
        Some(NodeSide::Left)
    } else if m == dr {
        Some(NodeSide::Right)
    } else if m == dt {
        Some(NodeSide::Top)
    } else {
        Some(NodeSide::Bottom)
    }
}

fn side_anchor(n: &CanvasNode, side: NodeSide) -> (f32, f32) {
    let x = n.x() as f32;
    let y = n.y() as f32;
    let w = n.width() as f32;
    let h = n.height() as f32;
    match side {
        NodeSide::Top => (x + w * 0.5, y),
        NodeSide::Bottom => (x + w * 0.5, y + h),
        NodeSide::Left => (x, y + h * 0.5),
        NodeSide::Right => (x + w, y + h * 0.5),
    }
}

/// Options for board overview rasterization.
#[derive(Debug, Clone, Copy)]
pub struct BoardDrawOpts<'a> {
    pub selected_ids: &'a [String],
    pub connect_from: Option<&'a str>,
    /// Draw mid-side connection ports (single selection).
    pub show_ports: bool,
    /// Optional ink thumbnails keyed by node id: (w, h, gray8).
    pub ink_previews: &'a std::collections::HashMap<String, (u32, u32, Vec<u8>)>,
}

/// Screen-space hit radius for connection ports (logical px).
pub const PORT_HIT_RADIUS: f32 = 22.0;

/// Hit-test a connection port on `node_id` (mid-side circles).
pub fn hit_port(
    doc: &CanvasDoc,
    vp: &Viewport,
    sx: f32,
    sy: f32,
    node_id: &str,
    radius_screen: f32,
) -> Option<NodeSide> {
    let n = doc.find_node(node_id)?;
    let sides = [
        NodeSide::Top,
        NodeSide::Right,
        NodeSide::Bottom,
        NodeSide::Left,
    ];
    let mut best: Option<(NodeSide, f32)> = None;
    for side in sides {
        let (bx, by) = side_anchor(n, side);
        let (px, py) = vp.board_to_screen(bx, by);
        let d = ((sx - px).powi(2) + (sy - py).powi(2)).sqrt();
        if d <= radius_screen && best.map(|(_, bd)| d < bd).unwrap_or(true) {
            best = Some((side, d));
        }
    }
    best.map(|(s, _)| s)
}

/// Hit-test the bottom-right resize handle of `node_id` (screen px radius).
pub fn hit_resize_handle(
    doc: &CanvasDoc,
    vp: &Viewport,
    sx: f32,
    sy: f32,
    node_id: &str,
    radius_screen: f32,
) -> bool {
    let Some(n) = doc.find_node(node_id) else {
        return false;
    };
    let (hx, hy) = vp.board_to_screen((n.x() + n.width()) as f32, (n.y() + n.height()) as f32);
    (sx - hx).abs() <= radius_screen && (sy - hy).abs() <= radius_screen
}

/// Render the board into a gray8 buffer (`width * height` bytes).
pub fn render_board(
    doc: &CanvasDoc,
    vp: &Viewport,
    width: u32,
    height: u32,
    opts: &BoardDrawOpts<'_>,
) -> Vec<u8> {
    let mut buf = vec![WHITE; (width * height) as usize];
    // Groups first (bottom).
    for n in &doc.nodes {
        if matches!(n, CanvasNode::Group(_)) {
            draw_node(&mut buf, width, height, vp, n, opts);
        }
    }
    // Edges under cards.
    for e in &doc.edges {
        let Some(from) = doc.find_node(&e.from_node) else {
            continue;
        };
        let Some(to) = doc.find_node(&e.to_node) else {
            continue;
        };
        let fs = e.from_side.unwrap_or(NodeSide::Right);
        let ts = e.to_side.unwrap_or(NodeSide::Left);
        let (x0, y0) = side_anchor(from, fs);
        let (x1, y1) = side_anchor(to, ts);
        let (sx0, sy0) = vp.board_to_screen(x0, y0);
        let (sx1, sy1) = vp.board_to_screen(x1, y1);
        draw_line(
            &mut buf, width, height, sx0 as i32, sy0 as i32, sx1 as i32, sy1 as i32, BLACK,
        );
        draw_arrowhead(&mut buf, width, height, sx0, sy0, sx1, sy1);
    }
    for n in &doc.nodes {
        if !matches!(n, CanvasNode::Group(_)) {
            draw_node(&mut buf, width, height, vp, n, opts);
        }
    }
    // Ports on top when a single card is selected.
    if opts.show_ports {
        if let Some(id) = opts.selected_ids.first() {
            if let Some(n) = doc.find_node(id) {
                draw_ports(&mut buf, width, height, vp, n);
            }
        }
    }
    buf
}

fn draw_ports(buf: &mut [u8], width: u32, height: u32, vp: &Viewport, n: &CanvasNode) {
    let r = (8.0 * vp.scale).clamp(6.0, 14.0) as i32;
    for side in [
        NodeSide::Top,
        NodeSide::Right,
        NodeSide::Bottom,
        NodeSide::Left,
    ] {
        let (bx, by) = side_anchor(n, side);
        let (sx, sy) = vp.board_to_screen(bx, by);
        fill_circle(buf, width, height, sx as i32, sy as i32, r, BLACK);
        fill_circle(buf, width, height, sx as i32, sy as i32, r - 2, WHITE);
    }
    // Bottom-right resize handle: solid square so it reads apart from ports.
    let (cx, cy) = vp.board_to_screen((n.x() + n.width()) as f32, (n.y() + n.height()) as f32);
    let (cx, cy) = (cx as i32, cy as i32);
    fill_rect(buf, width, height, cx - r, cy - r, 2 * r, 2 * r, BLACK);
    fill_rect(
        buf,
        width,
        height,
        cx - r + 2,
        cy - r + 2,
        2 * r - 4,
        2 * r - 4,
        WHITE,
    );
    fill_rect(
        buf,
        width,
        height,
        cx - r + 5,
        cy - r + 5,
        2 * r - 10,
        2 * r - 10,
        BLACK,
    );
}

fn fill_circle(buf: &mut [u8], w: u32, h: u32, cx: i32, cy: i32, r: i32, color: u8) {
    if r <= 0 {
        return;
    }
    let r2 = r * r;
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy <= r2 {
                let x = cx + dx;
                let y = cy + dy;
                if x >= 0 && y >= 0 && x < w as i32 && y < h as i32 {
                    buf[(y as u32 * w + x as u32) as usize] = color;
                }
            }
        }
    }
}

fn draw_node(
    buf: &mut [u8],
    width: u32,
    height: u32,
    vp: &Viewport,
    n: &CanvasNode,
    opts: &BoardDrawOpts<'_>,
) {
    let (sx, sy) = vp.board_to_screen(n.x() as f32, n.y() as f32);
    let sw = (n.width() as f32 * vp.scale) as i32;
    let sh = (n.height() as f32 * vp.scale) as i32;
    let x0 = sx as i32;
    let y0 = sy as i32;
    let selected =
        opts.selected_ids.iter().any(|id| id == n.id()) || opts.connect_from == Some(n.id());
    let fill = WHITE;
    let border = if selected { SELECT } else { BLACK };
    let border_w = if selected { 3 } else { 2 };

    match n {
        CanvasNode::Group(g) => {
            draw_rect_outline(buf, width, height, x0, y0, sw, sh, border, border_w, true);
            if let Some(label) = &g.label {
                // 2× glyphs so the group name stays legible on e-ink.
                let max = ((sw - 16) / 12).max(0) as usize;
                for (i, c) in label.chars().take(max).enumerate() {
                    blit_glyph_scaled(buf, width, height, x0 + 8 + i as i32 * 12, y0 + 8, c, 2);
                }
            }
        }
        CanvasNode::Text(t) => {
            fill_rect(buf, width, height, x0, y0, sw, sh, fill);
            if let Some((iw, ih, px)) = opts.ink_previews.get(n.id()) {
                blit_ink_preview(
                    buf,
                    width,
                    height,
                    x0 + 2,
                    y0 + 2,
                    sw - 4,
                    sh - 4,
                    *iw,
                    *ih,
                    px,
                );
            } else if t.text.trim().is_empty() {
                blit_label(buf, width, height, x0 + 8, y0 + 8, "Empty");
            } else {
                blit_text_fit(
                    buf,
                    width,
                    height,
                    x0 + 8,
                    y0 + 8,
                    sw - 16,
                    sh - 16,
                    &t.text,
                );
            }
            draw_rect_outline(buf, width, height, x0, y0, sw, sh, border, border_w, false);
        }
        CanvasNode::File(f) => {
            fill_rect(buf, width, height, x0, y0, sw, sh, fill);
            draw_rect_outline(buf, width, height, x0, y0, sw, sh, border, border_w, false);
            blit_label(buf, width, height, x0 + 8, y0 + 8, "File");
            blit_label(buf, width, height, x0 + 8, y0 + 28, &f.file);
        }
        CanvasNode::Link(l) => {
            fill_rect(buf, width, height, x0, y0, sw, sh, fill);
            draw_rect_outline(buf, width, height, x0, y0, sw, sh, border, border_w, false);
            blit_label(buf, width, height, x0 + 8, y0 + 8, "Link");
            blit_label(buf, width, height, x0 + 8, y0 + 28, &l.url);
        }
    }
}

/// Ink darker than this counts as a stroke; lighter is page background.
const INK_THRESHOLD: u8 = 220;
/// Longest side kept for a cached ink preview (min-pooled down to this).
const INK_PREVIEW_MAX: u32 = 640;

/// Crop gray8 page pixels to the ink bounding box (plus a small margin) and
/// min-pool down so the longest side is at most `INK_PREVIEW_MAX`. Returns
/// `None` for a blank page. Run once on load, not per frame.
pub fn crop_ink_preview(w: u32, h: u32, px: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    if w == 0 || h == 0 || px.len() < (w as usize) * (h as usize) {
        return None;
    }
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0u32, 0u32);
    for y in 0..h {
        let row = &px[(y * w) as usize..((y + 1) * w) as usize];
        for (x, &p) in row.iter().enumerate() {
            if p < INK_THRESHOLD {
                let x = x as u32;
                x0 = x0.min(x);
                x1 = x1.max(x);
                y0 = y0.min(y);
                y1 = y1.max(y);
            }
        }
    }
    if x1 < x0 {
        return None;
    }
    let margin = 8;
    let x0 = x0.saturating_sub(margin);
    let y0 = y0.saturating_sub(margin);
    let x1 = (x1 + margin).min(w - 1);
    let y1 = (y1 + margin).min(h - 1);
    let (cw, ch) = (x1 - x0 + 1, y1 - y0 + 1);
    let f = cw.max(ch).div_ceil(INK_PREVIEW_MAX).max(1);
    let (ow, oh) = (cw.div_ceil(f), ch.div_ceil(f));
    let mut out = vec![WHITE; (ow * oh) as usize];
    for oy in 0..oh {
        for ox in 0..ow {
            let mut v = WHITE;
            for sy in (y0 + oy * f)..(y0 + (oy + 1) * f).min(y1 + 1) {
                for sx in (x0 + ox * f)..(x0 + (ox + 1) * f).min(x1 + 1) {
                    v = v.min(px[(sy * w + sx) as usize]);
                }
            }
            out[(oy * ow + ox) as usize] = v;
        }
    }
    Some((ow, oh, out))
}

/// Fit gray8 ink into a card rect with a uniform scale (aspect kept, centred).
/// Downscaling min-pools so thin strokes survive.
#[allow(clippy::too_many_arguments)]
fn blit_ink_preview(
    buf: &mut [u8],
    bw: u32,
    bh: u32,
    x0: i32,
    y0: i32,
    dw: i32,
    dh: i32,
    iw: u32,
    ih: u32,
    pixels: &[u8],
) {
    if dw <= 0 || dh <= 0 || iw == 0 || ih == 0 {
        return;
    }
    let expect = (iw as usize).saturating_mul(ih as usize);
    if pixels.len() < expect {
        return;
    }
    let scale = (dw as f32 / iw as f32).min(dh as f32 / ih as f32);
    let fw = ((iw as f32 * scale) as i32).max(1);
    let fh = ((ih as f32 * scale) as i32).max(1);
    let ox = x0 + (dw - fw) / 2;
    let oy = y0 + (dh - fh) / 2;
    for dy in 0..fh {
        let y = oy + dy;
        if y < 0 || y >= bh as i32 {
            continue;
        }
        let sy0 = (dy as u32 * ih) / fh as u32;
        let sy1 = (((dy + 1) as u32 * ih) / fh as u32).clamp(sy0 + 1, ih);
        for dx in 0..fw {
            let x = ox + dx;
            if x < 0 || x >= bw as i32 {
                continue;
            }
            let sx0 = (dx as u32 * iw) / fw as u32;
            let sx1 = (((dx + 1) as u32 * iw) / fw as u32).clamp(sx0 + 1, iw);
            let mut src = WHITE;
            for sy in sy0..sy1 {
                let row = (sy * iw) as usize;
                for sx in sx0..sx1 {
                    src = src.min(pixels[row + sx as usize]);
                }
            }
            // Only stamp dark ink so white page doesn't fight the card fill.
            if src >= INK_THRESHOLD {
                continue;
            }
            buf[(y as u32 * bw + x as u32) as usize] = src;
        }
    }
}

/// Word-wrap `text` into a `cw`×`ch` box at the largest integer glyph scale
/// (1–4×) that fits; at 1× overflowing lines are dropped.
#[allow(clippy::too_many_arguments)]
fn blit_text_fit(buf: &mut [u8], w: u32, h: u32, x0: i32, y0: i32, cw: i32, ch: i32, text: &str) {
    if cw <= 0 || ch <= 0 {
        return;
    }
    for s in (1..=4).rev() {
        let cols = (cw / (6 * s)).max(1) as usize;
        let rows = (ch / (9 * s)).max(0) as usize;
        let lines = wrap_text(text, cols);
        if lines.len() <= rows || s == 1 {
            for (i, line) in lines.iter().take(rows).enumerate() {
                let ly = y0 + i as i32 * 9 * s;
                for (j, c) in line.chars().enumerate() {
                    blit_glyph_scaled(buf, w, h, x0 + j as i32 * 6 * s, ly, c, s);
                }
            }
            return;
        }
    }
}

fn wrap_text(text: &str, cols: usize) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.lines() {
        let mut line = String::new();
        for word in para.split_whitespace() {
            let mut word: Vec<char> = word.chars().collect();
            // Hard-break words longer than a line.
            while word.len() > cols {
                if !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                }
                out.push(word.drain(..cols).collect());
            }
            let word: String = word.into_iter().collect();
            let len = line.chars().count();
            if len > 0 && len + 1 + word.chars().count() > cols {
                out.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(&word);
        }
        out.push(line);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn fill_rect(buf: &mut [u8], w: u32, h: u32, x0: i32, y0: i32, rw: i32, rh: i32, color: u8) {
    for y in y0.max(0)..(y0 + rh).min(h as i32) {
        for x in x0.max(0)..(x0 + rw).min(w as i32) {
            buf[(y as u32 * w + x as u32) as usize] = color;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_rect_outline(
    buf: &mut [u8],
    w: u32,
    h: u32,
    x0: i32,
    y0: i32,
    rw: i32,
    rh: i32,
    color: u8,
    thickness: i32,
    dashed: bool,
) {
    for t in 0..thickness {
        hline(buf, w, h, x0, y0 + t, rw, color, dashed);
        hline(buf, w, h, x0, y0 + rh - 1 - t, rw, color, dashed);
        vline(buf, w, h, x0 + t, y0, rh, color, dashed);
        vline(buf, w, h, x0 + rw - 1 - t, y0, rh, color, dashed);
    }
}

#[allow(clippy::too_many_arguments)]
fn hline(buf: &mut [u8], w: u32, h: u32, x0: i32, y: i32, len: i32, color: u8, dashed: bool) {
    if y < 0 || y >= h as i32 {
        return;
    }
    for i in 0..len {
        if dashed && (i / 4) % 2 == 1 {
            continue;
        }
        let x = x0 + i;
        if x >= 0 && x < w as i32 {
            buf[(y as u32 * w + x as u32) as usize] = color;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn vline(buf: &mut [u8], w: u32, h: u32, x: i32, y0: i32, len: i32, color: u8, dashed: bool) {
    if x < 0 || x >= w as i32 {
        return;
    }
    for i in 0..len {
        if dashed && (i / 4) % 2 == 1 {
            continue;
        }
        let y = y0 + i;
        if y >= 0 && y < h as i32 {
            buf[(y as u32 * w + x as u32) as usize] = color;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_line(
    buf: &mut [u8],
    w: u32,
    h: u32,
    mut x0: i32,
    mut y0: i32,
    x1: i32,
    y1: i32,
    color: u8,
) {
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        if x0 >= 0 && y0 >= 0 && x0 < w as i32 && y0 < h as i32 {
            buf[(y0 as u32 * w + x0 as u32) as usize] = color;
            // 2px thickness
            if x0 + 1 < w as i32 {
                buf[(y0 as u32 * w + (x0 + 1) as u32) as usize] = color;
            }
        }
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}

fn draw_arrowhead(buf: &mut [u8], w: u32, h: u32, x0: f32, y0: f32, x1: f32, y1: f32) {
    let angle = (y1 - y0).atan2(x1 - x0);
    let len = 10.0;
    let a1 = angle + std::f32::consts::PI * 0.85;
    let a2 = angle - std::f32::consts::PI * 0.85;
    let ax = x1 + len * a1.cos();
    let ay = y1 + len * a1.sin();
    let bx = x1 + len * a2.cos();
    let by = y1 + len * a2.sin();
    draw_line(buf, w, h, x1 as i32, y1 as i32, ax as i32, ay as i32, BLACK);
    draw_line(buf, w, h, x1 as i32, y1 as i32, bx as i32, by as i32, BLACK);
}

/// Minimal 5×7 bitmap font for e-ink labels (ASCII subset).
fn blit_label(buf: &mut [u8], w: u32, h: u32, x0: i32, y0: i32, text: &str) {
    let mut cx = x0;
    for ch in text.chars().take(40) {
        if ch == '\n' {
            break;
        }
        blit_glyph(buf, w, h, cx, y0, ch);
        cx += 6;
        if cx > w as i32 - 6 {
            break;
        }
    }
    let _ = GRAY; // reserved for future secondary ink
}

fn blit_glyph(buf: &mut [u8], w: u32, h: u32, x0: i32, y0: i32, ch: char) {
    let bits = glyph5x7(ch);
    for (row, &row_bits) in bits.iter().enumerate() {
        for col in 0..5 {
            if row_bits & (1 << (4 - col)) != 0 {
                let x = x0 + col;
                let y = y0 + row as i32;
                if x >= 0 && y >= 0 && x < w as i32 && y < h as i32 {
                    buf[(y as u32 * w + x as u32) as usize] = BLACK;
                }
            }
        }
    }
}

fn blit_glyph_scaled(buf: &mut [u8], w: u32, h: u32, x0: i32, y0: i32, ch: char, s: i32) {
    if s <= 1 {
        blit_glyph(buf, w, h, x0, y0, ch);
        return;
    }
    let bits = glyph5x7(ch);
    for (row, &row_bits) in bits.iter().enumerate() {
        for col in 0..5 {
            if row_bits & (1 << (4 - col)) != 0 {
                fill_rect(buf, w, h, x0 + col * s, y0 + row as i32 * s, s, s, BLACK);
            }
        }
    }
}

fn glyph5x7(ch: char) -> [u8; 7] {
    // Very small set; unknown → block.
    match ch.to_ascii_uppercase() {
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        'D' => [0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0E],
        'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        'M' => [0x11, 0x1B, 0x15, 0x11, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x1B, 0x11],
        'X' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
        'Y' => [0x11, 0x11, 0x0A, 0x04, 0x04, 0x04, 0x04],
        ' ' => [0; 7],
        '/' => [0x01, 0x02, 0x04, 0x04, 0x08, 0x10, 0x10],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C],
        '-' => [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00],
        ':' => [0x00, 0x0C, 0x0C, 0x00, 0x0C, 0x0C, 0x00],
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x06, 0x08, 0x10, 0x1F],
        _ => [0x1F, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1F],
    }
}

/// Snap board coords to a coarse grid (e-ink move tool).
pub fn snap_grid(v: i32, grid: i32) -> i32 {
    if grid <= 0 {
        return v;
    }
    let half = grid / 2;
    if v >= 0 {
        ((v + half) / grid) * grid
    } else {
        ((v - half) / grid) * grid
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas_store::CanvasDoc;

    #[test]
    fn hit_test_text_card() {
        let mut doc = CanvasDoc::default();
        let id = doc.add_text_card(100, 100);
        let vp = Viewport {
            origin_x: 0.0,
            origin_y: 0.0,
            scale: 1.0,
        };
        assert_eq!(
            hit_test(&doc, &vp, 150.0, 150.0).as_deref(),
            Some(id.as_str())
        );
        assert!(hit_test(&doc, &vp, 10.0, 10.0).is_none());
    }

    #[test]
    fn render_nonempty() {
        let mut doc = CanvasDoc::default();
        let a = doc.add_text_card(20, 20);
        let b = doc.add_file_card(320, 20, "Notes/x.md");
        doc.edges.push(crate::json_canvas::CanvasEdge {
            id: "e1".into(),
            from_node: a,
            from_side: Some(NodeSide::Right),
            from_end: None,
            to_node: b,
            to_side: Some(NodeSide::Left),
            to_end: Some(crate::json_canvas::EdgeEnd::Arrow),
            color: None,
            label: None,
        });
        let vp = Viewport {
            origin_x: 0.0,
            origin_y: 0.0,
            scale: 1.0,
        };
        let ids: [String; 0] = [];
        let previews = std::collections::HashMap::new();
        let opts = BoardDrawOpts {
            selected_ids: &ids,
            connect_from: None,
            show_ports: false,
            ink_previews: &previews,
        };
        let buf = render_board(&doc, &vp, 800, 600, &opts);
        assert_eq!(buf.len(), 800 * 600);
        assert!(buf.iter().any(|&p| p < 255));
    }

    #[test]
    fn crop_ink_preview_trims_blank_page() {
        let (w, h) = (200u32, 100u32);
        let mut px = vec![WHITE; (w * h) as usize];
        for x in 50..60 {
            px[(40 * w + x) as usize] = BLACK;
        }
        let (cw, ch, out) = crop_ink_preview(w, h, &px).unwrap();
        assert_eq!((cw, ch), (10 + 16, 1 + 16));
        assert!(out.contains(&BLACK));
        assert!(crop_ink_preview(w, h, &vec![WHITE; (w * h) as usize]).is_none());
    }

    #[test]
    fn crop_ink_preview_minpools_large_pages() {
        let (w, h) = (2000u32, 1000u32);
        let mut px = vec![WHITE; (w * h) as usize];
        px[0] = BLACK;
        px[(h * w - 1) as usize] = BLACK;
        // Thin 1px stroke must survive the downsample.
        px[(500 * w + 1001) as usize] = BLACK;
        let (cw, ch, out) = crop_ink_preview(w, h, &px).unwrap();
        assert!(cw <= INK_PREVIEW_MAX && ch <= INK_PREVIEW_MAX);
        assert_eq!(out.iter().filter(|&&p| p == BLACK).count(), 3);
    }

    #[test]
    fn ink_preview_keeps_aspect_ratio() {
        // 10×10 solid ink into a 100×50 rect → 50×50 centred square.
        let ink = vec![BLACK; 100];
        let (bw, bh) = (100u32, 50u32);
        let mut buf = vec![WHITE; (bw * bh) as usize];
        blit_ink_preview(&mut buf, bw, bh, 0, 0, 100, 50, 10, 10, &ink);
        let dark: Vec<u32> = (0..bw)
            .filter(|&x| buf[(25 * bw + x) as usize] == BLACK)
            .collect();
        assert_eq!((dark[0], *dark.last().unwrap()), (25, 74));
    }

    #[test]
    fn text_fit_wraps_and_scales() {
        assert_eq!(wrap_text("hello big world", 9), vec!["hello big", "world"]);
        assert_eq!(wrap_text("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        // Short text in a roomy card renders at > 1× (glyph rows taller than 7px).
        let (bw, bh) = (300u32, 200u32);
        let mut buf = vec![WHITE; (bw * bh) as usize];
        blit_text_fit(&mut buf, bw, bh, 0, 0, 300, 200, "HI");
        let rows = (0..bh)
            .filter(|&y| (0..bw).any(|x| buf[(y * bw + x) as usize] == BLACK))
            .count();
        assert_eq!(rows, 7 * 4);
    }

    #[test]
    fn resize_handle_hits_bottom_right_corner_only() {
        let mut doc = CanvasDoc::default();
        let id = doc.add_text_card(100, 100); // 280×160
        let vp = Viewport {
            origin_x: 0.0,
            origin_y: 0.0,
            scale: 0.5,
        };
        assert!(hit_resize_handle(&doc, &vp, 190.0, 130.0, &id, 22.0));
        assert!(!hit_resize_handle(&doc, &vp, 50.0, 50.0, &id, 22.0));
        assert!(!hit_resize_handle(&doc, &vp, 190.0, 130.0, "nope", 22.0));
    }

    #[test]
    fn snap_works() {
        assert_eq!(snap_grid(23, 20), 20);
        assert_eq!(snap_grid(28, 20), 20);
        assert_eq!(snap_grid(30, 20), 40);
    }

    #[test]
    fn zoom_view_at_keeps_anchor_and_clamps() {
        let mut doc = CanvasDoc::default();
        let _ = doc.add_text_card(0, 0);
        let mut view = CanvasView {
            x: 0.0,
            y: 0.0,
            zoom: CanvasZoomPreset::OneHundred,
            scale: 1.0,
        };
        let sx = 200.0;
        let sy = 150.0;
        let (bx0, by0) = Viewport::from_view(&view, 800, 600, &doc).screen_to_board(sx, sy);
        zoom_view_at(&mut view, &doc, 800, 600, 2.0, sx, sy);
        assert!((view.scale - 2.0).abs() < 1e-4);
        assert_eq!(view.zoom, CanvasZoomPreset::OneHundred);
        let (bx1, by1) = Viewport::from_view(&view, 800, 600, &doc).screen_to_board(sx, sy);
        assert!((bx0 - bx1).abs() < 1e-3);
        assert!((by0 - by1).abs() < 1e-3);

        zoom_view_at(&mut view, &doc, 800, 600, 100.0, sx, sy);
        assert!((view.scale - MAX_BOARD_SCALE).abs() < 1e-4);
    }

    #[test]
    fn zoom_view_at_leaves_fit_for_continuous_scale() {
        let mut doc = CanvasDoc::default();
        let _ = doc.add_text_card(0, 0);
        let mut view = view_for_fit(&doc, 800, 600);
        assert_eq!(view.zoom, CanvasZoomPreset::Fit);
        let before = Viewport::from_view(&view, 800, 600, &doc).scale;
        zoom_view_at(&mut view, &doc, 800, 600, 1.1, 400.0, 300.0);
        assert_eq!(view.zoom, CanvasZoomPreset::OneHundred);
        assert!((view.scale - before * 1.1).abs() < 1e-3);
    }
}
