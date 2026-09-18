//! Small geometry helpers shared by the ink engine.

use serde::{Deserialize, Serialize};

/// An axis-aligned bounding box / damage rectangle in logical pixels.
///
/// Represented by its inclusive-exclusive corners `(min, max)`.  An empty rect
/// (produced by [`Rect::empty`]) has `min > max` on both axes so that unioning
/// it with any real point yields exactly that point's degenerate rect.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub min_x: f32,
    pub min_y: f32,
    pub max_x: f32,
    pub max_y: f32,
}

impl Rect {
    /// An empty rect that acts as the identity for [`Rect::union`] and
    /// [`Rect::union_point`].
    pub fn empty() -> Self {
        Self {
            min_x: f32::INFINITY,
            min_y: f32::INFINITY,
            max_x: f32::NEG_INFINITY,
            max_y: f32::NEG_INFINITY,
        }
    }

    /// Construct a rect from explicit corners (does not normalise).
    pub fn new(min_x: f32, min_y: f32, max_x: f32, max_y: f32) -> Self {
        Self {
            min_x,
            min_y,
            max_x,
            max_y,
        }
    }

    /// `true` if this rect covers no area (e.g. the result of [`Rect::empty`]).
    pub fn is_empty(&self) -> bool {
        self.min_x > self.max_x || self.min_y > self.max_y
    }

    /// Width of the rect (`0` if empty).
    pub fn width(&self) -> f32 {
        if self.is_empty() {
            0.0
        } else {
            self.max_x - self.min_x
        }
    }

    /// Height of the rect (`0` if empty).
    pub fn height(&self) -> f32 {
        if self.is_empty() {
            0.0
        } else {
            self.max_y - self.min_y
        }
    }

    /// Expand this rect to include point `(x, y)`.
    pub fn union_point(&mut self, x: f32, y: f32) {
        self.min_x = self.min_x.min(x);
        self.min_y = self.min_y.min(y);
        self.max_x = self.max_x.max(x);
        self.max_y = self.max_y.max(y);
    }

    /// Return the union of `self` and `other`.
    pub fn union(&self, other: &Rect) -> Rect {
        Rect {
            min_x: self.min_x.min(other.min_x),
            min_y: self.min_y.min(other.min_y),
            max_x: self.max_x.max(other.max_x),
            max_y: self.max_y.max(other.max_y),
        }
    }

    /// Grow the rect outward by `pad` on every side.
    ///
    /// Used to account for stroke half-width so the damage rect covers the
    /// rendered brush, not just the centre-line path.  A no-op on empty rects.
    pub fn inflate(&self, pad: f32) -> Rect {
        if self.is_empty() {
            return *self;
        }
        Rect {
            min_x: self.min_x - pad,
            min_y: self.min_y - pad,
            max_x: self.max_x + pad,
            max_y: self.max_y + pad,
        }
    }

    /// `true` if `(x, y)` lies within `[min, max]` on both axes.
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.min_x && x <= self.max_x && y >= self.min_y && y <= self.max_y
    }
}

impl Default for Rect {
    fn default() -> Self {
        Rect::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_rect_is_empty() {
        let r = Rect::empty();
        assert!(r.is_empty());
        assert_eq!(r.width(), 0.0);
        assert_eq!(r.height(), 0.0);
    }

    #[test]
    fn union_point_from_empty_gives_degenerate_rect() {
        let mut r = Rect::empty();
        r.union_point(3.0, 4.0);
        assert!(!r.is_empty());
        assert_eq!((r.min_x, r.min_y, r.max_x, r.max_y), (3.0, 4.0, 3.0, 4.0));
    }

    #[test]
    fn union_point_grows_bounds() {
        let mut r = Rect::empty();
        r.union_point(1.0, 5.0);
        r.union_point(-2.0, 3.0);
        r.union_point(4.0, 10.0);
        assert_eq!((r.min_x, r.min_y, r.max_x, r.max_y), (-2.0, 3.0, 4.0, 10.0));
        assert_eq!(r.width(), 6.0);
        assert_eq!(r.height(), 7.0);
    }

    #[test]
    fn inflate_pads_all_sides() {
        let r = Rect::new(0.0, 0.0, 10.0, 10.0).inflate(2.0);
        assert_eq!(
            (r.min_x, r.min_y, r.max_x, r.max_y),
            (-2.0, -2.0, 12.0, 12.0)
        );
    }

    #[test]
    fn inflate_empty_is_noop() {
        assert!(Rect::empty().inflate(5.0).is_empty());
    }

    #[test]
    fn union_of_two_rects() {
        let a = Rect::new(0.0, 0.0, 5.0, 5.0);
        let b = Rect::new(3.0, -1.0, 8.0, 2.0);
        let u = a.union(&b);
        assert_eq!((u.min_x, u.min_y, u.max_x, u.max_y), (0.0, -1.0, 8.0, 5.0));
    }
}
