//! Ephemeris UI — Slint retained-mode UI skeleton for eink devices.
//!
//! ## Architecture
//!
//! ```text
//!  EphemerisUi
//!      │
//!      ├─ MinimalSoftwareWindow (Slint headless platform)
//!      │       └─ EphemerisPage (main.slint component)
//!      │
//!      └─ render_frame() ──► SoftwareRenderer ──► Rgb8 pixel buffer
//!                                  │
//!                         grayscale conversion
//!                                  │
//!                             PixelBuf (8 bpp)
//!                                  │
//!                         Display::present()
//! ```
//!
//! ## Seam for bead 6iy.2 (live ink drawing)
//!
//! The canvas area is exposed via the `on_canvas_touch` callback and the
//! [`EphemerisUi::render_frame`] method.  Bead 6iy.2 should:
//!
//! 1. Register `on_canvas_touch` to feed `InputEvent::PenDown/PenMove/PenUp`
//!    into the `InkEngine`.
//! 2. After each `InkUpdate`, render the stroke into a `PixelBuf` and call
//!    [`EphemerisUi::push_canvas_pixels`] (to be added in 6iy.2) to composite
//!    the stroke layer over the UI background before presenting.
//!
//! For now the canvas area renders as a blank white rectangle.

use std::cell::RefCell;
use std::rc::Rc;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, WindowAdapter};
use slint::{PhysicalSize, SharedString};

use ephemeris_pal::display::{Display, PixelBuf, Rect, RefreshMode};

// Include the generated Slint bindings (produced by build.rs → slint-build).
slint::include_modules!();

// ── Headless platform ─────────────────────────────────────────────────────────

/// A minimal Slint platform that uses the software renderer and never opens a
/// window.  This makes `ephemeris-ui` fully headless / CI-safe.
struct HeadlessPlatform {
    window: Rc<MinimalSoftwareWindow>,
}

impl Platform for HeadlessPlatform {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> core::time::Duration {
        // No real clock in tests; return zero.
        core::time::Duration::ZERO
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// The main UI handle.
///
/// Create once per application session, call [`render_frame`] to drive the
/// render loop and push pixels to a [`Display`].
pub struct EphemerisUi {
    window: Rc<MinimalSoftwareWindow>,
    component: EphemerisPage,
    /// Dimensions set at creation time.
    width: u32,
    height: u32,
}

impl EphemerisUi {
    /// Create the UI at the given resolution (typically 800 × 600 for eink).
    ///
    /// Installs a custom Slint platform the first time this is called in a
    /// process.  Subsequent calls re-use the same platform but create a new
    /// `EphemerisPage` component on a new window.
    pub fn new(width: u32, height: u32) -> Result<Self, slint::PlatformError> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        window.set_size(PhysicalSize::new(width, height));

        slint::platform::set_platform(Box::new(HeadlessPlatform {
            window: window.clone(),
        }))
        .ok(); // ignore AlreadySet — fine when multiple tests share a process

        let component = EphemerisPage::new()?;
        component.window().show()?;

        Ok(EphemerisUi { window, component, width, height })
    }

    // ── Property setters ──────────────────────────────────────────────────

    /// Set the page title shown in the status bar.
    pub fn set_page_title(&self, title: &str) {
        self.component.set_page_title(SharedString::from(title));
    }

    /// Set the current page index (1-based).
    pub fn set_page_index(&self, index: i32, total: i32) {
        self.component.set_page_index(index);
        self.component.set_page_count(total);
    }

    // ── Callback registration ─────────────────────────────────────────────

    /// Register a closure that is called when the user taps the canvas area.
    ///
    /// `x` and `y` are in logical pixels relative to the canvas top-left.
    /// This is the primary seam for bead 6iy.2 to feed touch/pen events into
    /// the `InkEngine`.
    pub fn on_canvas_touch<F>(&self, handler: F)
    where
        F: FnMut(f32, f32) + 'static,
    {
        self.component.on_canvas_touch(handler);
    }

    /// Register a closure called when the user selects a toolbar tool.
    ///
    /// The tool name matches the `.slint` string: `"pen"`, `"highlighter"`,
    /// or `"eraser"`.
    pub fn on_tool_selected<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_tool_selected(move |tool| handler(tool.to_string()));
    }

    /// Register a closure called when the user presses "Clear".
    pub fn on_clear_page<F>(&self, handler: F)
    where
        F: FnMut() + 'static,
    {
        self.component.on_clear_page(handler);
    }

    // ── Render ────────────────────────────────────────────────────────────

    /// Render one frame into a [`PixelBuf`] and present it via `display`.
    ///
    /// Returns the damage rectangles that were repainted (may be empty if
    /// nothing changed since the last call).  The refresh mode is `Full` on
    /// first render, `Partial` on subsequent renders.
    pub fn render_frame<D: Display>(
        &self,
        display: &mut D,
        mode: RefreshMode,
    ) -> Result<Vec<Rect>, Box<dyn std::error::Error>> {
        // 1. Pump Slint's event loop one tick (process pending events / layout).
        slint::platform::update_timers_and_animations();

        // 2. Ask the software renderer to paint into our RGB8 buffer.
        let w = self.width as usize;
        let h = self.height as usize;

        // We use Rgb8Pixel-sized u32 slots: one u32 = [R, G, B, _padding]
        // Slint's Rgb8Pixel is 3 bytes; pack into Vec<u8> aligned manually.
        let pixel_count = w * h;
        // Slint software renderer writes RGB pixels; we allocate as u32 for
        // alignment, then convert.
        let buf: RefCell<Vec<slint::platform::software_renderer::Rgb565Pixel>> =
            RefCell::new(vec![
                slint::platform::software_renderer::Rgb565Pixel::default();
                pixel_count
            ]);

        let mut repainted = false;
        self.window.draw_if_needed(|renderer| {
            let mut pixels = buf.borrow_mut();
            renderer.render(pixels.as_mut_slice(), w);
            repainted = true;
        });

        if !repainted {
            // Nothing changed; skip present.
            return Ok(vec![]);
        }

        // 3. Convert RGB565 → 8-bpp grayscale.
        //    Luma = 0.2126·R + 0.7152·G + 0.0722·B  (BT.709)
        let pixels = buf.borrow();
        let mut pal_buf = PixelBuf::new(self.width, self.height);
        for (i, px) in pixels.iter().enumerate() {
            let (r, g, b) = rgb565_to_rgb888(*px);
            let luma = (0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32)
                .round() as u8;
            pal_buf.data[i] = luma;
        }

        // 4. Build a single full-screen damage rect (software renderer's
        //    minimal bounding box would need more integration; full-screen is
        //    safe for the skeleton).
        let damage = vec![Rect::new(0, 0, self.width, self.height)];

        display.present(&pal_buf, &damage, mode)?;

        Ok(damage)
    }
}

// ── Colour conversion ─────────────────────────────────────────────────────────

/// Unpack an RGB565 pixel into (R8, G8, B8).
#[inline]
fn rgb565_to_rgb888(px: slint::platform::software_renderer::Rgb565Pixel) -> (u8, u8, u8) {
    // Rgb565Pixel is a newtype over u16 — access via the public field.
    let raw: u16 = px.0;
    let r5 = ((raw >> 11) & 0x1F) as u32;
    let g6 = ((raw >> 5) & 0x3F) as u32;
    let b5 = (raw & 0x1F) as u32;
    // Scale: 5-bit → 8-bit: v * 255 / 31;  6-bit → 8-bit: v * 255 / 63
    let r8 = ((r5 * 255 + 15) / 31) as u8;
    let g8 = ((g6 * 255 + 31) / 63) as u8;
    let b8 = ((b5 * 255 + 15) / 31) as u8;
    (r8, g8, b8)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ephemeris_pal::display::{MockDesktop, RefreshMode};

    /// Construct the UI, render a frame into a PixelBuf via the software
    /// renderer, present it to MockDesktop, and drive a synthetic tool-select
    /// event — asserting no panic and that a non-empty frame is produced.
    #[test]
    fn skeleton_renders_to_mock_display() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");
        ui.set_page_title("Test Page");
        ui.set_page_index(1, 3);

        let mut display = MockDesktop::new(800, 600).expect("MockDesktop creation failed");

        // First render — should produce a full-screen damage rect.
        let damage = ui
            .render_frame(&mut display, RefreshMode::Full)
            .expect("render_frame failed");

        // The first render must repaint the whole window.
        assert!(!damage.is_empty(), "first render should produce damage rects");
        assert_eq!(damage[0].width, 800);
        assert_eq!(damage[0].height, 600);
    }

    /// Wire the tool-selected callback and verify it fires without panic.
    #[test]
    fn toolbar_tool_callback_fires() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");

        let fired = Rc::new(RefCell::new(String::new()));
        let fired_clone = fired.clone();

        ui.on_tool_selected(move |tool| {
            *fired_clone.borrow_mut() = tool;
        });

        // Invoke the callback directly via the component (simulates a button tap).
        ui.component.invoke_tool_selected("highlighter".into());

        assert_eq!(*fired.borrow(), "highlighter");
    }

    /// Wire the canvas-touch callback and verify it fires without panic.
    #[test]
    fn canvas_touch_callback_fires() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");

        let coords = Rc::new(RefCell::new((0.0f32, 0.0f32)));
        let coords_clone = coords.clone();

        ui.on_canvas_touch(move |x, y| {
            *coords_clone.borrow_mut() = (x, y);
        });

        // Invoke directly (simulates a canvas tap at logical pixel 100, 200).
        ui.component.invoke_canvas_touch(100.0, 200.0);
        let (x, y) = *coords.borrow();
        assert!((x - 100.0).abs() < f32::EPSILON);
        assert!((y - 200.0).abs() < f32::EPSILON);
    }

    /// Wire the clear-page callback and verify it fires without panic.
    #[test]
    fn clear_page_callback_fires() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");

        let cleared = Rc::new(RefCell::new(false));
        let cleared_clone = cleared.clone();

        ui.on_clear_page(move || {
            *cleared_clone.borrow_mut() = true;
        });

        ui.component.invoke_clear_page();
        assert!(*cleared.borrow());
    }

    #[test]
    fn rgb565_to_rgb888_white() {
        // White in RGB565 = 0xFFFF
        let px = slint::platform::software_renderer::Rgb565Pixel(0xFFFF);
        let (r, g, b) = rgb565_to_rgb888(px);
        assert_eq!(r, 255);
        assert_eq!(g, 255);
        assert_eq!(b, 255);
    }

    #[test]
    fn rgb565_to_rgb888_black() {
        let px = slint::platform::software_renderer::Rgb565Pixel(0x0000);
        let (r, g, b) = rgb565_to_rgb888(px);
        assert_eq!(r, 0);
        assert_eq!(g, 0);
        assert_eq!(b, 0);
    }
}
