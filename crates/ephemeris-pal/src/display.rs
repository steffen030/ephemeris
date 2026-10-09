use thiserror::Error;

/// Display errors that can occur during rendering or presentation.
#[derive(Error, Debug)]
pub enum DisplayError {
    #[error("display initialization failed: {0}")]
    Init(String),

    #[error("present failed: {0}")]
    Present(String),

    #[error("invalid dimensions: {0}")]
    InvalidDimensions(String),

    #[error("out of bounds: {0}")]
    OutOfBounds(String),

    #[error("backend error: {0}")]
    Backend(String),
}

impl DisplayError {
    /// Create a new `Init` error with the given message.
    pub fn init<S: Into<String>>(msg: S) -> Self {
        DisplayError::Init(msg.into())
    }

    /// Create a new `Present` error with the given message.
    pub fn present<S: Into<String>>(msg: S) -> Self {
        DisplayError::Present(msg.into())
    }

    /// Create a new `InvalidDimensions` error with the given message.
    pub fn invalid_dimensions<S: Into<String>>(msg: S) -> Self {
        DisplayError::InvalidDimensions(msg.into())
    }

    /// Create a new `OutOfBounds` error with the given message.
    pub fn out_of_bounds<S: Into<String>>(msg: S) -> Self {
        DisplayError::OutOfBounds(msg.into())
    }

    /// Create a new `Backend` error with the given message.
    pub fn backend<S: Into<String>>(msg: S) -> Self {
        DisplayError::Backend(msg.into())
    }
}

/// Pixel buffer for display rendering (grayscale, 8-bit per pixel).
#[derive(Debug, Clone)]
pub struct PixelBuf {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub data: Vec<u8>,
}

impl PixelBuf {
    /// Create a new pixel buffer (white background).
    pub fn new(width: u32, height: u32) -> Self {
        let stride = width;
        let data = vec![255u8; (width * height) as usize];
        PixelBuf {
            width,
            height,
            stride,
            data,
        }
    }

    /// Fill entire buffer with a value (0=black, 255=white).
    pub fn fill(&mut self, value: u8) {
        self.data.fill(value);
    }

    /// Set a single pixel.
    pub fn set_pixel(&mut self, x: u32, y: u32, value: u8) {
        if x < self.width && y < self.height {
            let idx = (y * self.stride + x) as usize;
            if idx < self.data.len() {
                self.data[idx] = value;
            }
        }
    }
}

/// Refresh mode hint for e-ink displays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RefreshMode {
    #[default]
    Full,
    Partial,
    Fast,
    Clear,
}

/// Rectangle for damage/dirty regions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Rect {
            x,
            y,
            width,
            height,
        }
    }
}

/// Display abstraction for rendering pixel buffers.
pub trait Display {
    /// Get display size in pixels.
    fn size(&self) -> (u32, u32);

    /// Present a pixel buffer to the display.
    fn present(
        &mut self,
        buf: &PixelBuf,
        damage: &[Rect],
        mode: RefreshMode,
    ) -> Result<(), DisplayError>;
}

// ── Desktop window backend (winit + softbuffer) ──────────────────────────────

use std::num::NonZeroU32;
use std::sync::Arc;

/// Slop when comparing window size to output size (rounding / CSD).
const COVER_SLOP: u32 = 8;

/// PineNote panel physical size; fallback before a monitor is known.
pub const PINENOTE_PX: (u32, u32) = (1404, 1872);

/// True when `window` fills `output` aside from rounding.
pub fn covers_output(window: (u32, u32), output: (u32, u32)) -> bool {
    output.0 >= 64
        && output.1 >= 64
        && window.0 + COVER_SLOP >= output.0
        && window.1 + COVER_SLOP >= output.1
}

/// Escape hatch: leave a normal decorated window (desktop debugging).
pub fn kiosk_enabled() -> bool {
    std::env::var_os("EPHEMERIS_WINDOWED").is_none()
}

/// Live desktop window backed by winit + softbuffer.
///
/// Created inside an `ApplicationHandler::resumed` callback; presents an
/// 8-bpp grayscale [`PixelBuf`] as XRGB8888 to the OS compositor.
///
/// On Linux/GNOME (PineNote), default mode is borderless exclusive fullscreen
/// so the top panel stays hidden. Leave fullscreen (stay maximized) to peek
/// the panel; re-enter fullscreen on the next tap — same protocol as Calibread.
pub struct DesktopWindow {
    window: Arc<winit::window::Window>,
    // Context must outlive surface; prefixed `_` because it is only kept alive.
    _ctx: softbuffer::Context<Arc<winit::window::Window>>,
    surface: softbuffer::Surface<Arc<winit::window::Window>, Arc<winit::window::Window>>,
    width: u32,
    height: u32,
}

impl DesktopWindow {
    /// Create a window of the given logical size from within a winit
    /// `ApplicationHandler::resumed` callback.
    ///
    /// On HiDPI / Retina displays the softbuffer surface is sized to the
    /// physical window dimensions and `present` upscales the 8-bpp logical
    /// buffer to fill the physical surface.
    pub fn new(
        event_loop: &winit::event_loop::ActiveEventLoop,
        width: u32,
        height: u32,
    ) -> Result<Self, DisplayError> {
        use winit::dpi::LogicalSize;
        use winit::window::Window;

        let kiosk = kiosk_enabled();
        let attrs = Window::default_attributes()
            .with_title("Ephemeris")
            .with_inner_size(LogicalSize::new(width, height))
            .with_decorations(!kiosk)
            .with_maximized(true)
            .with_resizable(true);

        // Wayland app_id must match StartupWMClass in the .desktop file so the
        // shell can associate the running window with the launcher entry.
        #[cfg(target_os = "linux")]
        let attrs = {
            use winit::platform::wayland::WindowAttributesExtWayland;
            attrs.with_name("ephemeris", "ephemeris")
        };

        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .map_err(|e| DisplayError::init(e.to_string()))?,
        );

        if kiosk {
            apply_borderless_fullscreen(&window);
        }

        let ctx = softbuffer::Context::new(window.clone())
            .map_err(|e| DisplayError::init(e.to_string()))?;

        let mut surface = softbuffer::Surface::new(&ctx, window.clone())
            .map_err(|e| DisplayError::init(e.to_string()))?;

        // Resize the surface to the PHYSICAL window dimensions so that the
        // buffer covers the entire window on HiDPI / Retina displays.
        let phys = window.inner_size();
        surface
            .resize(
                NonZeroU32::new(phys.width.max(1)).unwrap(),
                NonZeroU32::new(phys.height.max(1)).unwrap(),
            )
            .map_err(|e| DisplayError::init(e.to_string()))?;

        Ok(DesktopWindow {
            window,
            _ctx: ctx,
            surface,
            width,
            height,
        })
    }

    /// Enter exclusive borderless fullscreen (hides GNOME/Phosh top panel).
    pub fn enter_kiosk(&self) {
        if !kiosk_enabled() {
            return;
        }
        apply_borderless_fullscreen(&self.window);
    }

    /// Leave exclusive fullscreen but stay maximized so the shell panel stays
    /// visible on the workarea until the next tap.
    pub fn show_panel(&self) {
        if !kiosk_enabled() {
            return;
        }
        self.window.set_fullscreen(None);
        self.window.set_maximized(true);
        self.window.set_decorations(false);
    }

    /// True when the window fills the current monitor output.
    pub fn covers_output(&self) -> bool {
        let Some(output) = self.output_px() else {
            return false;
        };
        let inner = self.window.inner_size();
        covers_output((inner.width, inner.height), output)
    }

    /// Current monitor physical size, if known.
    pub fn output_px(&self) -> Option<(u32, u32)> {
        let monitor = self
            .window
            .current_monitor()
            .or_else(|| self.window.primary_monitor())?;
        let size = monitor.size();
        if size.width < 64 || size.height < 64 {
            return None;
        }
        Some((size.width, size.height))
    }

    /// Window inner size and outer position in logical pixels (for pen mapping).
    pub fn logical_geometry(&self) -> (u32, u32, u32, u32) {
        let scale = self.window.scale_factor().max(0.01);
        let inner = self.window.inner_size();
        let pos = self.window.outer_position().unwrap_or_default();
        let win_w = ((inner.width as f64) / scale).round().max(1.0) as u32;
        let win_h = ((inner.height as f64) / scale).round().max(1.0) as u32;
        let origin_x = ((pos.x as f64) / scale).round().max(0.0) as u32;
        let origin_y = ((pos.y as f64) / scale).round().max(0.0) as u32;
        (win_w, win_h, origin_x, origin_y)
    }

    /// Resize the softbuffer surface to the current physical window dimensions.
    ///
    /// Must be called whenever `WindowEvent::Resized` or
    /// `WindowEvent::ScaleFactorChanged` fires.
    pub fn resize_surface(&mut self) -> Result<(), DisplayError> {
        let phys = self.window.inner_size();
        self.surface
            .resize(
                NonZeroU32::new(phys.width.max(1)).unwrap(),
                NonZeroU32::new(phys.height.max(1)).unwrap(),
            )
            .map_err(|e| DisplayError::init(e.to_string()))
    }

    /// Ask the OS to schedule a redraw (triggers `WindowEvent::RedrawRequested`).
    pub fn request_redraw(&self) {
        self.window.request_redraw();
    }

    /// Request keyboard/pointer focus so the shell dismisses the top panel.
    pub fn focus(&self) {
        self.window.focus_window();
    }

    /// Returns the window's current scale factor (physical / logical pixels).
    pub fn scale_factor(&self) -> f64 {
        self.window.scale_factor()
    }

    /// Physical window size in pixels (for mapping touch → UI coordinates).
    pub fn physical_size(&self) -> (u32, u32) {
        let s = self.window.inner_size();
        (s.width.max(1), s.height.max(1))
    }

    /// Logical buffer size the UI renders into.
    pub fn logical_size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

fn apply_borderless_fullscreen(window: &winit::window::Window) {
    use winit::window::Fullscreen;
    window.set_decorations(false);
    window.set_maximized(true);
    let monitor = window.current_monitor();
    window.set_fullscreen(Some(Fullscreen::Borderless(monitor)));
}

impl Display for DesktopWindow {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn present(
        &mut self,
        buf: &PixelBuf,
        _damage: &[Rect],
        _mode: RefreshMode,
    ) -> Result<(), DisplayError> {
        let phys = self.window.inner_size();
        let phys_w = phys.width as usize;
        let phys_h = phys.height as usize;

        let lw = self.width as usize;
        let lh = self.height as usize;

        if lw == 0 || lh == 0 || phys_w == 0 || phys_h == 0 {
            return Ok(());
        }

        let mut sb = self
            .surface
            .buffer_mut()
            .map_err(|e| DisplayError::present(e.to_string()))?;

        // Nearest-neighbour upscale: each physical pixel maps back to its
        // logical source pixel.  Handles any integer or fractional scale factor
        // (e.g. 2× Retina, 1.5× fractional-DPI displays).
        for py in 0..phys_h {
            for px in 0..phys_w {
                let lx = (px * lw / phys_w).min(lw - 1);
                let ly = (py * lh / phys_h).min(lh - 1);
                let g = buf.data.get(ly * lw + lx).copied().unwrap_or(255) as u32;
                sb[py * phys_w + px] = (g << 16) | (g << 8) | g;
            }
        }

        sb.present()
            .map_err(|e| DisplayError::present(e.to_string()))?;

        Ok(())
    }
}

// ── Mock desktop display (headless stub for tests) ────────────────────────────

/// Mock desktop display (stub for future winit/softbuffer integration).
pub struct MockDesktop {
    width: u32,
    height: u32,
}

impl MockDesktop {
    /// Create a new mock desktop display.
    pub fn new(width: u32, height: u32) -> Result<Self, DisplayError> {
        if width == 0 || height == 0 {
            return Err(DisplayError::invalid_dimensions(
                "display dimensions must be > 0",
            ));
        }
        Ok(MockDesktop { width, height })
    }
}

impl Display for MockDesktop {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn present(
        &mut self,
        buf: &PixelBuf,
        _damage: &[Rect],
        _mode: RefreshMode,
    ) -> Result<(), DisplayError> {
        if buf.data.is_empty() {
            return Err(DisplayError::present("buffer data is empty"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixelbuf_creation() {
        let buf = PixelBuf::new(800, 600);
        assert_eq!(buf.width, 800);
        assert_eq!(buf.height, 600);
        assert_eq!(buf.stride, 800);
        assert_eq!(buf.data.len(), 480000);
    }

    #[test]
    fn pixelbuf_fill() {
        let mut buf = PixelBuf::new(10, 10);
        buf.fill(0);
        assert!(buf.data.iter().all(|&v| v == 0));
    }

    #[test]
    fn pixelbuf_set_pixel() {
        let mut buf = PixelBuf::new(10, 10);
        buf.set_pixel(5, 5, 128);
        assert_eq!(buf.data[55], 128);
    }

    #[test]
    fn rect_creation() {
        let r = Rect::new(10, 20, 100, 200);
        assert_eq!(r.x, 10);
        assert_eq!(r.y, 20);
        assert_eq!(r.width, 100);
        assert_eq!(r.height, 200);
    }

    #[test]
    fn refresh_mode_default() {
        assert_eq!(RefreshMode::default(), RefreshMode::Full);
    }

    #[test]
    fn fullscreen_pinenote_covers() {
        assert!(covers_output((1404, 1872), (1404, 1872)));
    }

    #[test]
    fn workarea_under_gnome_bar_does_not_cover() {
        assert!(!covers_output((1404, 1808), (1404, 1872)));
        assert!(!covers_output((1404, 1840), (1404, 1872)));
        // GNOME panel ~64px: treating this as covering would re-fullscreen and
        // the bar would only flash.
        assert!(!covers_output((1404, 1872 - 64), (1404, 1872)));
    }

    #[test]
    fn preferred_window_does_not_cover() {
        assert!(!covers_output((900, 1200), PINENOTE_PX));
    }

    #[test]
    fn mock_desktop_creation() {
        let display = MockDesktop::new(800, 600).expect("should create");
        assert_eq!(display.size(), (800, 600));
    }

    #[test]
    fn mock_desktop_present() {
        let mut display = MockDesktop::new(800, 600).expect("should create");
        let buf = PixelBuf::new(800, 600);
        display
            .present(&buf, &[], RefreshMode::Full)
            .expect("should present");
    }

    #[test]
    fn display_trait_is_object_safe() {
        let _: &dyn Display;
    }

    #[test]
    fn mock_desktop_rejects_zero_dimensions() {
        let result = MockDesktop::new(0, 600);
        assert!(result.is_err());
        match result {
            Err(DisplayError::InvalidDimensions(msg)) => {
                assert!(msg.contains("must be > 0"));
            }
            _ => panic!("expected InvalidDimensions error"),
        }
    }

    #[test]
    fn mock_desktop_present_fails_on_empty_buffer() {
        let mut display = MockDesktop::new(800, 600).expect("should create");
        let mut buf = PixelBuf::new(800, 600);
        buf.data.clear(); // empty the buffer

        let result = display.present(&buf, &[], RefreshMode::Full);
        assert!(result.is_err());
        match result {
            Err(DisplayError::Present(msg)) => {
                assert!(msg.contains("empty"));
            }
            _ => panic!("expected Present error"),
        }
    }
}
