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

        // Pen-first / e-ink: hide the OS pointer. Hover and CursorMoved still
        // drive Slint hit-testing; a visible cursor tracks the stylus and looks
        // wrong on the note canvas (and wasted ink on partial refresh).
        // macOS can re-show the cursor after focus changes — re-assert via
        // [`DesktopWindow::hide_cursor`].
        hide_os_cursor(&window);

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
        // Focus regain often restores the OS cursor on macOS; keep pen-first UX.
        self.hide_cursor();
    }

    /// Hide the OS pointer (pen-first note canvas). Safe to call repeatedly.
    pub fn hide_cursor(&self) {
        hide_os_cursor(&self.window);
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

fn hide_os_cursor(window: &winit::window::Window) {
    window.set_cursor_visible(false);
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
        damage: &[Rect],
        mode: RefreshMode,
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

        // Full refresh (or empty damage list): rewrite every physical pixel.
        // Partial/Fast: only touch damage rects — critical for responsive ink
        // on PineNote (1404×1872) where a full softbuffer present freezes.
        // softbuffer age == 0 means the buffer contents are undefined, so we
        // must rewrite the whole surface even for a Partial/Fast present.
        let age = sb.age();
        let full =
            matches!(mode, RefreshMode::Full | RefreshMode::Clear) || damage.is_empty() || age == 0;
        let one_to_one = lw == phys_w && lh == phys_h;
        let ink_trace = std::env::var_os("EPHEMERIS_INK_TRACE").is_some();
        let t0 = if ink_trace {
            Some(std::time::Instant::now())
        } else {
            None
        };

        if full {
            blit_gray_to_softbuffer(
                &mut sb,
                buf,
                SoftbufferBlit {
                    logical_w: lw,
                    logical_h: lh,
                    phys_w,
                    phys_h,
                    lx0: 0,
                    ly0: 0,
                    lw_span: lw,
                    lh_span: lh,
                },
            );
            sb.present()
                .map_err(|e| DisplayError::present(e.to_string()))?;
        } else {
            let mut sb_damage: Vec<softbuffer::Rect> = Vec::with_capacity(damage.len());
            for r in damage {
                if r.width == 0 || r.height == 0 {
                    continue;
                }
                let lx0 = r.x.min(self.width.saturating_sub(1));
                let ly0 = r.y.min(self.height.saturating_sub(1));
                let lx1 = r.x.saturating_add(r.width).min(self.width);
                let ly1 = r.y.saturating_add(r.height).min(self.height);
                if lx1 <= lx0 || ly1 <= ly0 {
                    continue;
                }
                // Map logical damage → physical buffer coordinates.
                let px0 = (lx0 as u64 * phys_w as u64 / lw as u64) as u32;
                let py0 = (ly0 as u64 * phys_h as u64 / lh as u64) as u32;
                let px1 = (lx1 as u64 * phys_w as u64).div_ceil(lw as u64) as u32;
                let py1 = (ly1 as u64 * phys_h as u64).div_ceil(lh as u64) as u32;
                let pw = px1.saturating_sub(px0).min(phys.width.saturating_sub(px0));
                let ph = py1.saturating_sub(py0).min(phys.height.saturating_sub(py0));
                if pw == 0 || ph == 0 {
                    continue;
                }
                blit_gray_to_softbuffer(
                    &mut sb,
                    buf,
                    SoftbufferBlit {
                        logical_w: lw,
                        logical_h: lh,
                        phys_w,
                        phys_h,
                        lx0: lx0 as usize,
                        ly0: ly0 as usize,
                        lw_span: (lx1 - lx0) as usize,
                        lh_span: (ly1 - ly0) as usize,
                    },
                );
                if let (Some(w), Some(h)) = (NonZeroU32::new(pw), NonZeroU32::new(ph)) {
                    sb_damage.push(softbuffer::Rect {
                        x: px0,
                        y: py0,
                        width: w,
                        height: h,
                    });
                }
            }
            if sb_damage.is_empty() {
                sb.present()
                    .map_err(|e| DisplayError::present(e.to_string()))?;
            } else {
                sb.present_with_damage(&sb_damage)
                    .map_err(|e| DisplayError::present(e.to_string()))?;
            }
        }

        if let Some(t0) = t0 {
            let area: u64 = if full {
                (phys_w as u64) * (phys_h as u64)
            } else {
                damage
                    .iter()
                    .map(|r| u64::from(r.width) * u64::from(r.height))
                    .sum()
            };
            log::info!(
                "ink_trace present mode={mode:?} full={full} 1:1={one_to_one} age={age} logical={lw}x{lh} phys={phys_w}x{phys_h} damage_px={area} took={:?}",
                t0.elapsed()
            );
        }

        Ok(())
    }
}

/// Geometry for copying a logical gray rect into a physical softbuffer surface.
struct SoftbufferBlit {
    logical_w: usize,
    logical_h: usize,
    phys_w: usize,
    phys_h: usize,
    lx0: usize,
    ly0: usize,
    lw_span: usize,
    lh_span: usize,
}

/// Copy a logical gray rectangle into the softbuffer XRGB8888 surface.
///
/// When logical and physical sizes match (PineNote design scale 1), this is a
/// direct 1:1 write. Otherwise each physical pixel is bilinear-sampled from
/// the logical buffer (Retina / design-scale upscale). Nearest-neighbour made
/// ink strokes and UI text look blocky at ~1.75× on the PineNote panel.
fn blit_gray_to_softbuffer(sb: &mut [u32], buf: &PixelBuf, g: SoftbufferBlit) {
    if g.lw_span == 0 || g.lh_span == 0 {
        return;
    }
    let lx1 = (g.lx0 + g.lw_span).min(g.logical_w);
    let ly1 = (g.ly0 + g.lh_span).min(g.logical_h);

    if g.logical_w == g.phys_w && g.logical_h == g.phys_h {
        for ly in g.ly0..ly1 {
            let row_off = ly * g.logical_w;
            let sb_off = ly * g.phys_w;
            for lx in g.lx0..lx1 {
                let gray = buf.data.get(row_off + lx).copied().unwrap_or(255) as u32;
                sb[sb_off + lx] = (gray << 16) | (gray << 8) | gray;
            }
        }
        return;
    }

    // Physical span covering the logical rect (ceil on the far edge).
    let px0 = g.lx0 * g.phys_w / g.logical_w;
    let py0 = g.ly0 * g.phys_h / g.logical_h;
    let px1 = (lx1 * g.phys_w).div_ceil(g.logical_w);
    let py1 = (ly1 * g.phys_h).div_ceil(g.logical_h);
    let sx = g.logical_w as f32 / g.phys_w as f32;
    let sy = g.logical_h as f32 / g.phys_h as f32;
    let max_lx = g.logical_w.saturating_sub(1);
    let max_ly = g.logical_h.saturating_sub(1);
    for py in py0..py1.min(g.phys_h) {
        let fy = (py as f32 + 0.5) * sy - 0.5;
        let y0 = fy.floor().max(0.0) as usize;
        let y1 = (y0 + 1).min(max_ly);
        let ty = (fy - y0 as f32).clamp(0.0, 1.0);
        let row0 = y0 * g.logical_w;
        let row1 = y1 * g.logical_w;
        for px in px0..px1.min(g.phys_w) {
            let fx = (px as f32 + 0.5) * sx - 0.5;
            let x0 = fx.floor().max(0.0) as usize;
            let x1 = (x0 + 1).min(max_lx);
            let tx = (fx - x0 as f32).clamp(0.0, 1.0);
            let g00 = buf.data.get(row0 + x0).copied().unwrap_or(255) as f32;
            let g10 = buf.data.get(row0 + x1).copied().unwrap_or(255) as f32;
            let g01 = buf.data.get(row1 + x0).copied().unwrap_or(255) as f32;
            let g11 = buf.data.get(row1 + x1).copied().unwrap_or(255) as f32;
            let top = g00 + (g10 - g00) * tx;
            let bot = g01 + (g11 - g01) * tx;
            let gray = (top + (bot - top) * ty).round().clamp(0.0, 255.0) as u32;
            sb[py * g.phys_w + px] = (gray << 16) | (gray << 8) | gray;
        }
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

    #[test]
    fn bilinear_upscale_blends_neighbours() {
        // 2×2 logical → 4×4 physical: centre of the top-left physical
        // quadrant should sit between black (0,0) and white neighbours.
        let mut buf = PixelBuf::new(2, 2);
        buf.set_pixel(0, 0, 0);
        buf.set_pixel(1, 0, 255);
        buf.set_pixel(0, 1, 255);
        buf.set_pixel(1, 1, 255);
        let mut sb = vec![0u32; 16];
        blit_gray_to_softbuffer(
            &mut sb,
            &buf,
            SoftbufferBlit {
                logical_w: 2,
                logical_h: 2,
                phys_w: 4,
                phys_h: 4,
                lx0: 0,
                ly0: 0,
                lw_span: 2,
                lh_span: 2,
            },
        );
        let gray = |px: u32| (sb[px as usize] & 0xff) as u8;
        // Exact logical corners stay extreme after upscale.
        assert_eq!(gray(0), 0);
        assert_eq!(gray(3), 255);
        // Interior sample must blend (not snap to 0 or 255 like NN).
        let mid = gray(5); // physical (1,1)
        assert!(
            (1..255).contains(&mid),
            "expected bilinear blend at (1,1), got {mid}"
        );
    }
}
