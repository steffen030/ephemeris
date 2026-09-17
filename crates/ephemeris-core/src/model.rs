//! Ink domain models (ADR ephemeris-cm7).
//!
//! A page holds an ordered list of [`Stroke`]s.  A stroke is `tool + color +
//! width + Vec<Point>` where each [`Point`] carries `x, y, pressure, tilt,
//! t_ms`.  These types are pure data: `Serialize`/`Deserialize`, `Send + Sync`,
//! no I/O.
//!
//! NOTE: the full set of Ephemeris domain models (Profile, Note, Page, Task,
//! …) is owned by ephemeris-fna.2.  This module defines only the ink-specific
//! types required by the ink engine; they are expected to align with / be
//! merged into the shared model set.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Newtype id for a stroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StrokeId(pub Uuid);

impl StrokeId {
    /// Generate a fresh random id.
    pub fn new() -> Self {
        StrokeId(Uuid::new_v4())
    }
}

impl Default for StrokeId {
    fn default() -> Self {
        StrokeId::new()
    }
}

/// The drawing tool that produced a stroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Tool {
    /// Standard ink pen.
    #[default]
    Pen,
    /// Highlighter (semi-transparent, wider).
    Highlighter,
    /// Eraser stroke (recorded so erasing is itself undoable/replayable).
    Eraser,
}

/// An sRGB colour with straight alpha, each channel in `[0, 255]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const BLACK: Color = Color { r: 0, g: 0, b: 0, a: 255 };
    pub const WHITE: Color = Color { r: 255, g: 255, b: 255, a: 255 };

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Color { r, g, b, a: 255 }
    }
}

impl Default for Color {
    fn default() -> Self {
        Color::BLACK
    }
}

/// One sample along a stroke's centre line.
///
/// * `x`, `y` — position in logical pixels.
/// * `pressure` — normalised `[0.0, 1.0]` tip pressure at this sample.
/// * `tilt` — pen tilt in degrees `[-90.0, 90.0]`.
/// * `t_ms` — timestamp in milliseconds relative to stroke start.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
    pub tilt: f32,
    pub t_ms: u32,
}

impl Point {
    pub fn new(x: f32, y: f32, pressure: f32, tilt: f32, t_ms: u32) -> Self {
        Point { x, y, pressure, tilt, t_ms }
    }
}

/// A completed (or in-progress) ink stroke.
///
/// The `points` are the *smoothed* centre-line samples that the engine
/// produced from raw input.  `base_width` is the nominal pen width in logical
/// pixels; the per-point rendered width is derived from `base_width` and each
/// point's pressure by the render layer (see `ink::width`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stroke {
    pub id: StrokeId,
    pub tool: Tool,
    pub color: Color,
    /// Nominal stroke width in logical pixels (pressure scales around this).
    pub base_width: f32,
    pub points: Vec<Point>,
}

impl Stroke {
    /// Create an empty stroke with a fresh id.
    pub fn new(tool: Tool, color: Color, base_width: f32) -> Self {
        Stroke {
            id: StrokeId::new(),
            tool,
            color,
            base_width,
            points: Vec::new(),
        }
    }
}

// Compile-time assertion that the core model types are thread-safe.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Stroke>();
    assert_send_sync::<Point>();
    assert_send_sync::<Tool>();
    assert_send_sync::<Color>();
    assert_send_sync::<StrokeId>();
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stroke_serde_round_trip() {
        let mut s = Stroke::new(Tool::Pen, Color::BLACK, 2.5);
        s.points.push(Point::new(1.0, 2.0, 0.5, 10.0, 0));
        s.points.push(Point::new(3.0, 4.0, 0.7, 12.0, 16));

        let json = serde_json::to_string(&s).unwrap();
        let back: Stroke = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn point_serde_round_trip() {
        let p = Point::new(1.5, -2.5, 0.33, -45.0, 1234);
        let json = serde_json::to_string(&p).unwrap();
        let back: Point = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn tool_and_color_round_trip() {
        for tool in [Tool::Pen, Tool::Highlighter, Tool::Eraser] {
            let json = serde_json::to_string(&tool).unwrap();
            assert_eq!(tool, serde_json::from_str::<Tool>(&json).unwrap());
        }
        let c = Color::rgb(10, 20, 30);
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(c, serde_json::from_str::<Color>(&json).unwrap());
    }
}
