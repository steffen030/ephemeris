//! Map a digitizer's normalized axes onto the window.
//!
//! The PineNote w9013 digitizer reports landscape axes (≈1872×1404) while the
//! app window is portrait (≈1404×1872). Without a 90° remap, a horizontal
//! stroke on glass is recorded as a vertical stroke.

/// Map normalized digitizer `(nx, ny)` into the window's axis orientation.
pub fn orient_axes(nx: f32, ny: f32, digitizer: (f32, f32), window: (f32, f32)) -> (f32, f32) {
    let (dig_w, dig_h) = digitizer;
    let (win_w, win_h) = window;
    if win_h > win_w && dig_w > dig_h {
        // Landscape panel in a portrait window: 90° clockwise.
        return (1.0 - ny, nx);
    }
    if win_w > win_h && dig_h > dig_w {
        return (ny, 1.0 - nx);
    }
    (nx, ny)
}

/// Map a digitizer sample into window-local logical pixels.
///
/// `origin` is the window's top-left on the output (the GNOME bar sits in that
/// gap when the window is only maximized). The digitizer covers the full
/// output, including that strip.
pub fn digitizer_to_window(
    nx: f32,
    ny: f32,
    digitizer: (f32, f32),
    origin: (f32, f32),
    window: (f32, f32),
) -> (f32, f32) {
    let output = (
        (origin.0 + window.0).max(window.0),
        (origin.1 + window.1).max(window.1),
    );
    let (sx, sy) = orient_axes(nx, ny, digitizer, output);
    (sx * output.0 - origin.0, sy * output.1 - origin.1)
}

/// Map window-local logical pixels into design-UI logical pixels.
pub fn window_to_ui(wx: f32, wy: f32, window: (f32, f32), ui: (f32, f32)) -> (f32, f32) {
    let (win_w, win_h) = window;
    let (ui_w, ui_h) = ui;
    let x = if win_w > 0.0 {
        (wx / win_w) * ui_w
    } else {
        0.0
    };
    let y = if win_h > 0.0 {
        (wy / win_h) * ui_h
    } else {
        0.0
    };
    (x, y)
}

#[cfg(test)]
mod tests {
    #[test]
    fn portrait_window_keeps_horizontal_pen_strokes_horizontal() {
        let dig = (1872.0, 1404.0);
        let win = (1404.0, 1872.0);
        let a = super::orient_axes(0.25, 0.10, dig, win);
        let b = super::orient_axes(0.25, 0.90, dig, win);
        assert!(
            (a.1 - b.1).abs() < 0.001,
            "Y should stay put for a horizontal stroke, got {a:?} {b:?}"
        );
        assert!(
            (a.0 - b.0).abs() > 0.5,
            "X should change for a horizontal stroke, got {a:?} {b:?}"
        );
    }

    #[test]
    fn matching_landscape_axes_stay_identity() {
        assert_eq!(
            super::orient_axes(0.2, 0.8, (1872.0, 1404.0), (1872.0, 1404.0)),
            (0.2, 0.8)
        );
    }

    #[test]
    fn panel_gap_shifts_window_local_y() {
        let dig = (1404.0, 1872.0);
        let origin = (0.0, 32.0);
        let window = (1404.0, 1840.0);
        let (_x, y) = super::digitizer_to_window(0.5, 32.0 / 1872.0, dig, origin, window);
        assert!(
            y.abs() < 1.0,
            "top of the window (just below the bar) should be y≈0, got {y}"
        );
    }

    #[test]
    fn window_to_ui_scales_design_space() {
        let (x, y) = super::window_to_ui(702.0, 936.0, (1404.0, 1872.0), (800.0, 1067.0));
        assert!((x - 400.0).abs() < 0.5, "got {x}");
        assert!((y - 533.5).abs() < 1.0, "got {y}");
    }
}
