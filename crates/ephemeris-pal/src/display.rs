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

/// Live desktop window backed by winit + softbuffer.
///
/// Created inside an `ApplicationHandler::resumed` callback; presents an
/// 8-bpp grayscale [`PixelBuf`] as XRGB8888 to the OS compositor.
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
    pub fn new(
        event_loop: &winit::event_loop::ActiveEventLoop,
        width: u32,
        height: u32,
    ) -> Result<Self, DisplayError> {
        use winit::dpi::LogicalSize;
        use winit::window::Window;

        let attrs = Window::default_attributes()
            .with_title("Ephemeris")
            .with_inner_size(LogicalSize::new(width, height))
            .with_resizable(false);

        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .map_err(|e| DisplayError::init(e.to_string()))?,
        );

        let ctx = softbuffer::Context::new(window.clone())
            .map_err(|e| DisplayError::init(e.to_string()))?;

        let mut surface = softbuffer::Surface::new(&ctx, window.clone())
            .map_err(|e| DisplayError::init(e.to_string()))?;

        surface
            .resize(
                NonZeroU32::new(width).unwrap(),
                NonZeroU32::new(height).unwrap(),
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

    /// Ask the OS to schedule a redraw (triggers `WindowEvent::RedrawRequested`).
    pub fn request_redraw(&self) {
        self.window.request_redraw();
    }
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
        let mut sb = self
            .surface
            .buffer_mut()
            .map_err(|e| DisplayError::present(e.to_string()))?;

        let pixel_count = (self.width * self.height) as usize;
        for (i, dst) in sb.iter_mut().enumerate().take(pixel_count) {
            // 8-bpp gray (0=black, 255=white) → XRGB8888
            let g = buf.data.get(i).copied().unwrap_or(255) as u32;
            *dst = (g << 16) | (g << 8) | g;
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
