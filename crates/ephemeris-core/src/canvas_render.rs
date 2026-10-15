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
            CanvasZoomPreset::OneHundred => 1.0,
            CanvasZoomPreset::Fit => fit_scale(doc, screen_w, screen_h),
            CanvasZoomPreset::FocusSelected => 1.0,
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

fn fit_scale(doc: &CanvasDoc, screen_w: u32, screen_h: u32) -> f32 {
    let Some((min_x, min_y, max_x, max_y)) = content_bounds(doc) else {
        return 1.0;
    };
    let bw = (max_x - min_x).max(1.0) + 80.0;
    let bh = (max_y - min_y).max(1.0) + 80.0;
    let sx = screen_w as f32 / bw;
    let sy = screen_h as f32 / bh;
    sx.min(sy).clamp(0.25, 2.0)
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

/// Render the board into a gray8 buffer (`width * height` bytes).
pub fn render_board(
    doc: &CanvasDoc,
    vp: &Viewport,
    width: u32,
    height: u32,
    selected_id: Option<&str>,
    connect_from: Option<&str>,
) -> Vec<u8> {
    let mut buf = vec![WHITE; (width * height) as usize];
    // Groups first (bottom).
    for n in &doc.nodes {
        if matches!(n, CanvasNode::Group(_)) {
            draw_node(&mut buf, width, height, vp, n, selected_id, connect_from);
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
            draw_node(&mut buf, width, height, vp, n, selected_id, connect_from);
        }
    }
    buf
}

fn draw_node(
    buf: &mut [u8],
    width: u32,
    height: u32,
    vp: &Viewport,
    n: &CanvasNode,
    selected_id: Option<&str>,
    connect_from: Option<&str>,
) {
    let (sx, sy) = vp.board_to_screen(n.x() as f32, n.y() as f32);
    let sw = (n.width() as f32 * vp.scale) as i32;
    let sh = (n.height() as f32 * vp.scale) as i32;
    let x0 = sx as i32;
    let y0 = sy as i32;
    let selected = selected_id == Some(n.id()) || connect_from == Some(n.id());
    let fill = if matches!(n, CanvasNode::Group(_)) {
        WHITE
    } else {
        WHITE
    };
    let border = if selected { SELECT } else { BLACK };
    let border_w = if selected { 3 } else { 2 };

    match n {
        CanvasNode::Group(g) => {
            draw_rect_outline(buf, width, height, x0, y0, sw, sh, border, border_w, true);
            if let Some(label) = &g.label {
                blit_label(buf, width, height, x0 + 8, y0 + 8, label);
            }
        }
        CanvasNode::Text(t) => {
            fill_rect(buf, width, height, x0, y0, sw, sh, fill);
            draw_rect_outline(buf, width, height, x0, y0, sw, sh, border, border_w, false);
            let preview = if t.text.trim().is_empty() {
                "Handwritten"
            } else {
                t.text.lines().next().unwrap_or("Text")
            };
            blit_label(buf, width, height, x0 + 8, y0 + 8, "Text");
            blit_label(buf, width, height, x0 + 8, y0 + 28, preview);
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

fn fill_rect(buf: &mut [u8], w: u32, h: u32, x0: i32, y0: i32, rw: i32, rh: i32, color: u8) {
    for y in y0.max(0)..(y0 + rh).min(h as i32) {
        for x in x0.max(0)..(x0 + rw).min(w as i32) {
            buf[(y as u32 * w + x as u32) as usize] = color;
        }
    }
}

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
    for row in 0..7 {
        let row_bits = bits[row];
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
        let buf = render_board(&doc, &vp, 800, 600, None, None);
        assert_eq!(buf.len(), 800 * 600);
        assert!(buf.iter().any(|&p| p < 255));
    }

    #[test]
    fn snap_works() {
        assert_eq!(snap_grid(23, 20), 20);
        assert_eq!(snap_grid(28, 20), 20);
        assert_eq!(snap_grid(30, 20), 40);
    }
}
