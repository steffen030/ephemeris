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
//!      ├─ InkEngine (owned, behind RefCell for interior mutability)
//!      │
//!      ├─ PageBook — per-page ink state roster
//!      │       └─ PageState × N  (committed layer + in-progress points)
//!      │
//!      └─ render_frame() ──► SoftwareRenderer ──► Rgb8 pixel buffer
//!                                  │
//!                         grayscale conversion
//!                                  │
//!                             PixelBuf (8 bpp)
//!                                  │
//!                         composite ink layers (min() blend)
//!                                  │
//!                         Display::present()
//! ```
//!
//! ## Ink layer compositing
//!
//! After the Slint software renderer paints the UI (status bar + toolbar +
//! blank white canvas) into the full-screen grayscale [`PixelBuf`], the ink
//! layers are composited **before** the buffer is presented to the display:
//!
//! ```text
//!  composite pixel = min(ui_pixel, ink_pixel)
//! ```
//!
//! Because 0 = black and 255 = white in the 8-bpp buffer, `min()` keeps the
//! darker of the two values — i.e. dark ink always shows through a white
//! background, which is exactly what we want for eink.
//!
//! Compositing is restricted to the canvas region (rows `status_bar_h` to
//! `height - toolbar_h`), so chrome pixels are never overwritten by ink.
//!
//! ## Page navigation model (bead 6iy.3)
//!
//! [`EphemerisUi`] owns a [`PageBook`] which stores one [`PageState`] per page.
//! Navigating pages:
//!
//! * [`EphemerisUi::next_page`] — advance to the next page; if already on the
//!   last page, a new blank page is auto-created and appended.
//! * [`EphemerisUi::prev_page`] — move to the previous page (clamped at 0; no
//!   wrap-around).
//!
//! On each navigation the active page's ink state is saved, the target page's
//! state is loaded, and `page-index` / `page-count` in the status bar are
//! updated.
//!
//! The [`PageBook`] is designed so it can later be backed by `SqliteStore`
//! (by serializing/deserializing per-page state on demand), but in this
//! implementation all pages live in memory.
//!
//! Swipe gestures in the `.slint` UI fire `swipe-left` / `swipe-right`
//! callbacks.  Wire them to [`EphemerisUi::prev_page`] / [`EphemerisUi::next_page`]
//! from the application layer, or use the convenience method
//! [`EphemerisUi::wire_swipe_navigation`].
//!
//! ## Live drawing API
//!
//! ```ignore
//! // 1. Create the UI.
//! let ui = EphemerisUi::new(800, 600)?;
//!
//! // 2. Feed pen events directly (call from your input loop):
//! let update = ui.feed_input(&InputEvent::PenDown(sample), t_ms);
//!
//! // 3. Render; ink is composited automatically and the refresh scheduler
//! //    picks the RefreshMode (Fast/Partial/Clear/periodic Full):
//! ui.render_frame(&mut display)?;
//!
//! // 4. Push pre-rasterized pixels explicitly (optional, for external renderers):
//! ui.push_canvas_pixels(&my_ink_buf);
//! ```

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, WindowAdapter};
use slint::{PhysicalSize, SharedString};

use ephemeris_core::ink::{InkConfig, InkEngine, InkUpdate};
use ephemeris_core::model::Point;
use ephemeris_core::refresh::{DamageSource, RefreshConfig, RefreshScheduler};
use ephemeris_pal::display::{Display, PixelBuf, Rect};
use ephemeris_pal::input::InputEvent;

pub mod raster;

// ── Per-page ink state ────────────────────────────────────────────────────────

/// Snapshot of the ink layers for one page.
///
/// Stored inside [`PageBook`] so that navigating away and back preserves all
/// committed strokes and the in-progress state for each page.
///
/// The `base_width` mirrors `EphemerisUi::in_progress_base_width` — it is
/// persisted alongside the in-progress points so that a mid-stroke navigation
/// (edge case) can be recovered without re-borrowing the engine.
#[derive(Clone)]
struct PageState {
    /// Rasterized committed strokes layer for this page (canvas-sized, 8 bpp).
    committed: PixelBuf,
    /// Points of any in-progress stroke on this page (empty when no stroke is
    /// active or after the stroke was committed).
    in_progress: Vec<Point>,
    /// Nominal base_width of the in-progress stroke (copied at PenDown time).
    in_progress_base_width: f32,
}

impl PageState {
    /// Construct a blank (all-white) page state for the given canvas dimensions.
    fn blank(canvas_w: u32, canvas_h: u32, default_base_width: f32) -> Self {
        PageState {
            committed: PixelBuf::new(canvas_w, canvas_h),
            in_progress: Vec::new(),
            in_progress_base_width: default_base_width,
        }
    }
}

// ── Page roster ───────────────────────────────────────────────────────────────

/// Ordered roster of per-page ink states.
///
/// Invariants:
/// * `pages` is never empty (at least one page always exists).
/// * `current` is always a valid index into `pages`.
///
/// Designed so it can later be backed by `SqliteStore`: replace the `Vec`
/// with on-demand serialization/deserialization without changing the public
/// API surface.
struct PageBook {
    pages: Vec<PageState>,
    /// Zero-based index of the currently displayed page.
    current: usize,
}

impl PageBook {
    /// Create a book with a single blank page.
    fn new(canvas_w: u32, canvas_h: u32, default_base_width: f32) -> Self {
        PageBook {
            pages: vec![PageState::blank(canvas_w, canvas_h, default_base_width)],
            current: 0,
        }
    }

    /// Zero-based index of the current page.
    #[inline]
    fn current_index(&self) -> usize {
        self.current
    }

    /// Total number of pages.
    #[inline]
    fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Borrow the current page's state immutably.
    #[inline]
    fn current_page(&self) -> &PageState {
        &self.pages[self.current]
    }

    /// Borrow the current page's state mutably.
    #[inline]
    fn current_page_mut(&mut self) -> &mut PageState {
        &mut self.pages[self.current]
    }

    /// Save `state` as the current page's snapshot, then navigate to `target`.
    ///
    /// Returns `true` if the navigation actually changed the page, `false` if
    /// `target == current` (e.g. clamped at 0 when already on page 0).
    fn navigate_to(&mut self, state: PageState, target: usize) -> bool {
        // Persist the caller's live state into the current page slot.
        self.pages[self.current] = state;

        if target == self.current {
            return false;
        }
        self.current = target;
        true
    }

    /// Append a new blank page and navigate to it, saving `state` first.
    ///
    /// Returns the new zero-based index.
    fn push_blank_page(
        &mut self,
        state: PageState,
        canvas_w: u32,
        canvas_h: u32,
        default_base_width: f32,
    ) -> usize {
        self.pages[self.current] = state;
        let new_idx = self.pages.len();
        self.pages
            .push(PageState::blank(canvas_w, canvas_h, default_base_width));
        self.current = new_idx;
        new_idx
    }
}

// Include the generated Slint bindings (produced by build.rs → slint-build).
slint::include_modules!();

// ── Chrome geometry constants ──────────────────────────────────────────────────

/// Height of the status bar in logical / physical pixels.
/// Must match `status-bar.height` in `main.slint`.
pub const STATUS_BAR_H: u32 = 24;

/// Height of the toolbar in logical / physical pixels.
/// Must match `toolbar.height` in `main.slint`.
pub const TOOLBAR_H: u32 = 48;

// ── Headless platform ─────────────────────────────────────────────────────────

/// A minimal Slint platform that uses the software renderer and never opens a
/// window.  This makes `ephemeris-ui` fully headless / CI-safe.
struct HeadlessPlatform {
    window: Rc<MinimalSoftwareWindow>,
}

impl Platform for HeadlessPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
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
///
/// The `EphemerisUi` owns an [`InkEngine`] and a [`PageBook`] that stores
/// per-page ink layers:
///
/// * **committed layer** — rasterized completed strokes for the current page,
///   persists across frames and is saved/restored on page navigation.
/// * **in-progress layer** — the current unfinished stroke (cleared on `PenUp`).
///
/// Both layers are composited onto the Slint-rendered grayscale frame inside
/// [`render_frame`] using a `min()` blend, restricted to the canvas region.
///
/// ## Page navigation
///
/// Call [`next_page`] / [`prev_page`] from the application layer.  The
/// convenience method [`wire_swipe_navigation`] wires the `.slint` swipe
/// callbacks to these methods automatically.
pub struct EphemerisUi {
    window: Rc<MinimalSoftwareWindow>,
    component: EphemerisPage,
    /// Dimensions set at creation time.
    width: u32,
    height: u32,
    /// Persistent RGB565 pixel buffer reused across frames (required by
    /// `RepaintBufferType::ReusedBuffer`: Slint only repaints dirty regions,
    /// so we must preserve the full frame buffer between calls).
    rgb565_buf: RefCell<Vec<slint::platform::software_renderer::Rgb565Pixel>>,
    // ── Ink state (interior-mutable so the callbacks can borrow independently) ─
    /// The ink engine: tracks the active stroke builder.
    engine: RefCell<InkEngine>,
    /// Rasterized committed strokes layer for the *current page* (canvas-sized,
    /// 8 bpp).  Aliased from `pages.current_page().committed`; updated live as
    /// strokes are drawn, then saved back on navigation.
    committed_layer: RefCell<PixelBuf>,
    /// Points of the currently in-progress stroke (canvas-relative).
    in_progress: RefCell<Vec<Point>>,
    /// `base_width` of the in-progress stroke (copied from `InkConfig` at
    /// `PenDown` time so we can re-rasterize without re-borrowing the engine).
    in_progress_base_width: RefCell<f32>,
    /// Per-page ink state roster.  The active page's committed layer and
    /// in-progress state are kept live in `committed_layer` / `in_progress` /
    /// `in_progress_base_width`; all other pages are held exclusively in `pages`.
    pages: RefCell<PageBook>,
    /// Default [`InkConfig::base_width`], kept for constructing blank pages.
    default_base_width: f32,
    /// Ink damage rects (full-buffer coordinates) accumulated since the last
    /// present.  Drained by [`render_frame`].  This lets ink-only updates drive
    /// a present even when Slint's UI is otherwise clean (Slint's own
    /// `draw_if_needed` never reports the ink layer as dirty because the ink is
    /// composited *after* the software renderer runs).
    ink_damage: RefCell<Vec<Rect>>,
    /// Refresh scheduler (ADR ephemeris-btg, task ephemeris-2ql.7).  Chooses the
    /// [`RefreshMode`](ephemeris_pal::display::RefreshMode) per present from the accumulated damage sources and
    /// injects periodic `Full` refreshes to clear e-ink ghosting, so callers no
    /// longer pass a mode to [`render_frame`](EphemerisUi::render_frame).
    scheduler: RefCell<RefreshScheduler>,
    /// Set when the whole screen changed (page navigation, canvas clear, first
    /// paint) so the next [`render_frame`] forces a `Clear` refresh.  Cleared
    /// once consumed.
    screen_change: Cell<bool>,
}

impl EphemerisUi {
    /// Create the UI at the given resolution (typically 800 × 600 for eink).
    ///
    /// Installs a custom Slint platform the first time this is called in a
    /// process.  Subsequent calls re-use the same platform but create a new
    /// `EphemerisPage` component on a new window.
    pub fn new(width: u32, height: u32) -> Result<Self, slint::PlatformError> {
        Self::new_with_config(width, height, InkConfig::default())
    }

    /// Like [`EphemerisUi::new`] but accepts a custom [`InkConfig`].
    pub fn new_with_config(
        width: u32,
        height: u32,
        ink_cfg: InkConfig,
    ) -> Result<Self, slint::PlatformError> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        window.set_size(PhysicalSize::new(width, height));

        slint::platform::set_platform(Box::new(HeadlessPlatform {
            window: window.clone(),
        }))
        .ok(); // ignore AlreadySet — fine when multiple tests share a process

        let component = EphemerisPage::new()?;
        component.window().show()?;

        // The canvas height is the total height minus both chrome bars.
        let canvas_h = height.saturating_sub(STATUS_BAR_H + TOOLBAR_H);
        let pixel_count = (width * height) as usize;
        let default_base_width = ink_cfg.base_width;

        Ok(EphemerisUi {
            window,
            component,
            width,
            height,
            // Initialise to "white" (0xFFFF in RGB565) so partial repaints don't
            // show black artefacts in regions Slint hasn't touched yet.
            rgb565_buf: RefCell::new(vec![
                slint::platform::software_renderer::Rgb565Pixel(0xFFFF);
                pixel_count
            ]),
            engine: RefCell::new(InkEngine::new(ink_cfg.clone())),
            committed_layer: RefCell::new(PixelBuf::new(width, canvas_h)),
            in_progress: RefCell::new(Vec::new()),
            in_progress_base_width: RefCell::new(default_base_width),
            pages: RefCell::new(PageBook::new(width, canvas_h, default_base_width)),
            default_base_width,
            ink_damage: RefCell::new(Vec::new()),
            scheduler: RefCell::new(RefreshScheduler::default()),
            // The first frame paints a fresh screen — treat it as a screen
            // change so the initial present is a full/clear refresh.
            screen_change: Cell::new(true),
        })
    }

    /// Replace the [`RefreshScheduler`]'s thresholds (periodic-full interval,
    /// coalescing gap, max damage rects).  See [`RefreshConfig`].
    pub fn set_refresh_config(&self, config: RefreshConfig) {
        *self.scheduler.borrow_mut() = RefreshScheduler::new(config);
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

    /// Register a closure called when the user completes a leftward swipe
    /// on the canvas (→ navigate to the previous page).
    pub fn on_swipe_left<F>(&self, handler: F)
    where
        F: FnMut() + 'static,
    {
        self.component.on_swipe_left(handler);
    }

    /// Register a closure called when the user completes a rightward swipe
    /// on the canvas (→ navigate to the next page, auto-creating when needed).
    pub fn on_swipe_right<F>(&self, handler: F)
    where
        F: FnMut() + 'static,
    {
        self.component.on_swipe_right(handler);
    }

    // ── Page navigation API ───────────────────────────────────────────────

    /// Navigate to the next page.
    ///
    /// If the current page is the last page, a new blank page is auto-created
    /// and appended before navigating to it.  In either case the current page's
    /// ink state is saved before switching and the status bar is updated.
    pub fn next_page(&self) {
        let canvas_h = self.canvas_height();
        let live_state = self.snapshot_live_state(canvas_h);

        let new_idx = {
            let mut book = self.pages.borrow_mut();
            let cur = book.current_index();
            let count = book.page_count();
            if cur + 1 >= count {
                // Auto-create a new blank page.
                book.push_blank_page(live_state, self.width, canvas_h, self.default_base_width)
            } else {
                book.navigate_to(live_state, cur + 1);
                cur + 1
            }
        };

        self.load_page_state(new_idx);
    }

    /// Navigate to the previous page.
    ///
    /// Clamped at index 0 — calling `prev_page` on the first page is a no-op
    /// (the current page's state is still saved to the book).
    pub fn prev_page(&self) {
        let canvas_h = self.canvas_height();
        let live_state = self.snapshot_live_state(canvas_h);

        let target = {
            let mut book = self.pages.borrow_mut();
            let cur = book.current_index();
            let target = cur.saturating_sub(1);
            book.navigate_to(live_state, target);
            target
        };

        self.load_page_state(target);
    }

    /// Return `(current_index_1based, total_page_count)` reflecting the status
    /// bar display (1-based index, matching the `page-index / page-count` text).
    pub fn current_page_index(&self) -> (u32, u32) {
        let book = self.pages.borrow();
        ((book.current_index() + 1) as u32, book.page_count() as u32)
    }

    /// Wire the `.slint` `swipe-left` / `swipe-right` callbacks to
    /// [`prev_page`] / [`next_page`] respectively, using a shared `Rc<Self>`.
    ///
    /// This is a convenience helper; you can also call [`on_swipe_left`] /
    /// [`on_swipe_right`] manually if you need custom logic between gestures.
    ///
    /// ```ignore
    /// let ui = Rc::new(EphemerisUi::new(800, 600)?);
    /// EphemerisUi::wire_swipe_navigation(ui.clone());
    /// ```
    pub fn wire_swipe_navigation(ui: Rc<Self>) {
        let ui_left = ui.clone();
        ui.on_swipe_left(move || {
            ui_left.prev_page();
        });

        let ui_right = ui.clone();
        ui.on_swipe_right(move || {
            ui_right.next_page();
        });
    }

    // ── Ink input API ─────────────────────────────────────────────────────

    /// Feed one input event into the owned [`InkEngine`] and update the ink
    /// layers accordingly.
    ///
    /// * `PenDown` — starts a new in-progress stroke; clears the in-progress
    ///   point buffer.
    /// * `PenMove` — appends a point to the in-progress buffer (no layer write;
    ///   the in-progress layer is re-rasterized on every [`render_frame`]).
    /// * `PenUp` — finalises the stroke: merges the finished [`Stroke`] into
    ///   the committed layer and clears the in-progress buffer.
    ///
    /// Returns the raw [`InkUpdate`] from the engine so the caller can track
    /// damage rects for partial refresh optimisation.
    ///
    /// `x` and `y` on the `PenSample` must be **canvas-relative** logical
    /// pixels (i.e. the coordinate system reported by the `.slint`
    /// `canvas-touch` callback).
    pub fn feed_input(&self, event: &InputEvent, t_ms: u32) -> InkUpdate {
        let update = self.engine.borrow_mut().update(event, t_ms);

        match &update {
            InkUpdate::Started => {
                // New stroke: snapshot current base_width from the engine's
                // config.  We borrow the engine briefly to read config, then
                // release before touching other RefCells.
                let bw = self.engine.borrow().config().base_width;
                *self.in_progress_base_width.borrow_mut() = bw;
                self.in_progress.borrow_mut().clear();

                // Record the PenDown anchor point from the event.
                if let InputEvent::PenDown(s) = event {
                    self.in_progress.borrow_mut().push(Point {
                        x: s.x,
                        y: s.y,
                        pressure: s.pressure.clamp(0.0, 1.0),
                        tilt: s.tilt,
                        t_ms: 0,
                    });
                    // The anchor dot is stamped on the next render; report a
                    // small damage rect around it so the frame is presented.
                    let pad = bw + 1.0;
                    self.push_ink_damage(
                        ephemeris_core::geom::Rect::new(s.x, s.y, s.x, s.y).inflate(pad),
                    );
                }
            }
            InkUpdate::Extended { damage } => {
                // Append the new sample to the in-progress buffer.
                if let InputEvent::PenMove(s) = event {
                    self.in_progress.borrow_mut().push(Point {
                        x: s.x,
                        y: s.y,
                        pressure: s.pressure.clamp(0.0, 1.0),
                        tilt: s.tilt,
                        t_ms,
                    });
                }
                self.push_ink_damage(*damage);
            }
            InkUpdate::Finished { stroke, damage } => {
                // Commit the completed stroke to the permanent layer.
                let canvas_h = self.canvas_height();
                raster::rasterize_stroke(
                    &mut self.committed_layer.borrow_mut(),
                    stroke,
                    0, // canvas X offset in the layer is always 0
                    0, // layer Y=0 corresponds to canvas top (buf Y = STATUS_BAR_H)
                    self.width,
                    canvas_h,
                );
                // Clear the in-progress buffer.
                self.in_progress.borrow_mut().clear();
                self.push_ink_damage(*damage);
            }
            InkUpdate::Idle => {}
        }

        update
    }

    /// Push externally-rasterized canvas pixels into the committed ink layer,
    /// replacing it entirely.
    ///
    /// `pixels` must be a `width × canvas_height` grayscale buffer where
    /// `canvas_height = height - STATUS_BAR_H - TOOLBAR_H`.  If the dimensions
    /// do not match the UI's canvas dimensions, the push is silently ignored.
    ///
    /// This is the explicit seam documented in the `lib.rs` module doc — useful
    /// for callers that maintain their own stroke rasterizer outside this crate.
    pub fn push_canvas_pixels(&self, pixels: &PixelBuf) {
        let canvas_h = self.canvas_height();
        if pixels.width == self.width && pixels.height == canvas_h {
            *self.committed_layer.borrow_mut() = pixels.clone();
            // The whole canvas may have changed; mark it dirty so the next
            // `render_frame` presents even if Slint's UI is otherwise clean.
            self.ink_damage
                .borrow_mut()
                .push(Rect::new(0, STATUS_BAR_H, self.width, canvas_h));
        }
    }

    /// Clear both ink layers (committed and in-progress) for the current page.
    ///
    /// Typically called in response to the `clear-page` callback.  The cleared
    /// state is also persisted into `PageBook` so that navigating away and back
    /// returns to a blank page.
    pub fn clear_ink(&self) {
        let canvas_h = self.canvas_height();
        let blank = PixelBuf::new(self.width, canvas_h);
        *self.committed_layer.borrow_mut() = blank.clone();
        self.in_progress.borrow_mut().clear();

        // Persist the cleared state into the page roster.
        self.pages.borrow_mut().current_page_mut().committed = blank;
        self.pages
            .borrow_mut()
            .current_page_mut()
            .in_progress
            .clear();

        // The whole canvas went white; mark it dirty so the clear is presented.
        self.ink_damage
            .borrow_mut()
            .push(Rect::new(0, STATUS_BAR_H, self.width, canvas_h));

        // A full wipe is the ideal moment to flush ghosting with a Clear refresh.
        self.screen_change.set(true);
    }

    // ── Render ────────────────────────────────────────────────────────────

    /// Render one frame into a [`PixelBuf`] and present it via `display`.
    ///
    /// 1. Pumps Slint's event loop (layout + animation).
    /// 2. The software renderer paints the UI (status bar, toolbar, white
    ///    canvas background) into an RGB565 buffer.
    /// 3. The RGB565 buffer is converted to 8-bpp grayscale.
    /// 4. The committed ink layer and the rasterized in-progress stroke are
    ///    composited over the canvas region using `min()` blend (dark ink wins).
    /// 5. The result is presented to `display`.
    ///
    /// Returns the damage rectangles that were presented (empty when nothing
    /// changed since the last call, in which case `display.present` is *not*
    /// invoked — this is the eink render loop's "no idle repaint" guarantee).
    ///
    /// The [`RefreshMode`](ephemeris_pal::display::RefreshMode) is chosen by the owned [`RefreshScheduler`] rather
    /// than passed in: this frame's damage is submitted to the scheduler tagged
    /// by source, and the scheduler returns the coalesced damage and the mode
    /// (`Fast` for ink, `Partial` for UI, `Clear` on screen changes, and a
    /// periodic `Full` to flush ghosting).  Tune it via [`set_refresh_config`].
    ///
    /// The damage submitted to the scheduler comes from three sources:
    ///
    /// * **Slint's partial-render region** — the exact rectangles the software
    ///   renderer repainted this frame, taken from the [`PhysicalRegion`]
    ///   returned by `renderer.render()` (tagged [`DamageSource::Ui`]).
    /// * **Ink damage** — rects accumulated by [`feed_input`],
    ///   [`push_canvas_pixels`] and [`clear_ink`] since the last present, since
    ///   the ink layer is composited *after* Slint runs and is therefore
    ///   invisible to Slint's own dirty-tracking (tagged [`DamageSource::Ink`]).
    /// * **Screen change** — a full-window rect on the first frame, page
    ///   navigation or canvas clear (tagged [`DamageSource::ScreenChange`]).
    ///
    /// [`PhysicalRegion`]: slint::platform::software_renderer::PhysicalRegion
    /// [`set_refresh_config`]: EphemerisUi::set_refresh_config
    pub fn render_frame<D: Display>(
        &self,
        display: &mut D,
    ) -> Result<Vec<Rect>, Box<dyn std::error::Error>> {
        // 1. Pump Slint's event loop one tick (process pending events / layout).
        slint::platform::update_timers_and_animations();

        // 2. Ask the software renderer to paint dirty regions into our persistent
        //    RGB565 buffer.  With `RepaintBufferType::ReusedBuffer` Slint only
        //    updates the regions that changed, so we must carry the buffer across
        //    calls rather than allocating a fresh zero-filled one each time.
        //    The returned `PhysicalRegion` tells us *which* rects were touched —
        //    that is the display damage for the eink refresh.
        let w = self.width as usize;

        let mut slint_damage: Vec<Rect> = Vec::new();
        self.window.draw_if_needed(|renderer| {
            let mut pixels = self.rgb565_buf.borrow_mut();
            let region = renderer.render(pixels.as_mut_slice(), w);
            for (pos, size) in region.iter() {
                if size.width == 0 || size.height == 0 {
                    continue;
                }
                slint_damage.push(Rect::new(
                    pos.x.max(0) as u32,
                    pos.y.max(0) as u32,
                    size.width,
                    size.height,
                ));
            }
            // Slint reported a repaint but no concrete sub-rects (rare): fall
            // back to a full-window rect so a genuine repaint is never dropped.
            if slint_damage.is_empty() {
                slint_damage.push(Rect::new(0, 0, self.width, self.height));
            }
        });

        // 3. Drain ink damage accumulated since the last present.
        let ink_damage = std::mem::take(&mut *self.ink_damage.borrow_mut());

        // Did a page switch / clear / first paint invalidate the whole screen?
        let screen_change = self.screen_change.replace(false);

        // eink render loop: only present when something actually changed.
        if slint_damage.is_empty() && ink_damage.is_empty() && !screen_change {
            return Ok(vec![]);
        }

        // 4. Convert RGB565 → 8-bpp grayscale.
        //    Luma = 0.2126·R + 0.7152·G + 0.0722·B  (BT.709)
        let pixels = self.rgb565_buf.borrow();
        let mut pal_buf = PixelBuf::new(self.width, self.height);
        for (i, px) in pixels.iter().enumerate() {
            let (r, g, b) = rgb565_to_rgb888(*px);
            let luma = (0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32).round() as u8;
            pal_buf.data[i] = luma;
        }

        // 5. Composite ink layers over the canvas region.
        //
        //    The canvas occupies rows [STATUS_BAR_H, height - TOOLBAR_H).
        //    Both the committed layer and any in-progress points are composited
        //    here.  We build a temporary per-frame in-progress layer so that
        //    re-rendering the in-progress points doesn't permanently dirty the
        //    committed layer.
        let canvas_h = self.canvas_height();

        // 5a. Build the in-progress layer for this frame (only if drawing).
        let in_progress_pts = self.in_progress.borrow();
        let has_in_progress = !in_progress_pts.is_empty();
        let ip_layer = if has_in_progress {
            let mut layer = PixelBuf::new(self.width, canvas_h);
            raster::rasterize_points(
                &mut layer,
                &in_progress_pts,
                *self.in_progress_base_width.borrow(),
                0, // canvas X offset within the layer
                0, // layer Y=0 corresponds to canvas top
                self.width,
                canvas_h,
            );
            Some(layer)
        } else {
            None
        };
        drop(in_progress_pts); // release borrow before compositing

        // 5b. Composite committed layer + optional in-progress layer into pal_buf.
        let committed = self.committed_layer.borrow();
        let stride = self.width as usize;

        for row in 0..canvas_h {
            let buf_row = (STATUS_BAR_H + row) as usize;
            for col in 0..self.width as usize {
                let layer_idx = row as usize * stride + col;
                let buf_idx = buf_row * stride + col;

                // Start with the committed layer pixel.
                let mut ink_px = committed.data[layer_idx];

                // Overlay the in-progress layer if present.
                if let Some(ref ip) = ip_layer {
                    ink_px = ink_px.min(ip.data[layer_idx]);
                }

                // min() blend: dark ink over the Slint-rendered background.
                pal_buf.data[buf_idx] = pal_buf.data[buf_idx].min(ink_px);
            }
        }

        // 6. Feed this frame's damage to the refresh scheduler, tagged by source,
        //    and let it pick the RefreshMode and coalesce the rects.
        let mut scheduler = self.scheduler.borrow_mut();
        if screen_change {
            // A whole-screen change: submit the full window so the coalesced
            // damage matches the Clear refresh the scheduler will choose.
            scheduler.submit(
                Rect::new(0, 0, self.width, self.height),
                DamageSource::ScreenChange,
            );
        }
        for rect in slint_damage {
            scheduler.submit(rect, DamageSource::Ui);
        }
        for rect in ink_damage {
            scheduler.submit(rect, DamageSource::Ink);
        }

        // flush() yields Some: the early-return above guarantees we submitted at
        // least one non-empty rect (Slint/ink rects are pre-filtered, and a
        // screen change submits the full window).
        let present = scheduler
            .flush()
            .expect("damage was submitted, so flush must yield a present");
        drop(scheduler);

        display.present(&pal_buf, &present.damage, present.mode)?;

        Ok(present.damage)
    }

    // ── Helpers ───────────────────────────────────────────────────────────

    /// Canvas height in pixels: total height minus the two chrome bars.
    #[inline]
    fn canvas_height(&self) -> u32 {
        self.height.saturating_sub(STATUS_BAR_H + TOOLBAR_H)
    }

    /// Record ink damage for the next present.
    ///
    /// `canvas_rect` is in **canvas-relative** coordinates (the same space as
    /// the ink engine's damage rects).  It is clamped to the canvas bounds and
    /// offset by [`STATUS_BAR_H`] into full-buffer space before being stored.
    /// Empty or fully-clipped rects are ignored.
    fn push_ink_damage(&self, canvas_rect: ephemeris_core::geom::Rect) {
        if canvas_rect.is_empty() {
            return;
        }
        let canvas_h = self.canvas_height();
        let x0 = canvas_rect.min_x.max(0.0).floor() as u32;
        let y0 = canvas_rect.min_y.max(0.0).floor() as u32;
        let x1 = (canvas_rect.max_x.ceil() as i64).clamp(0, self.width as i64) as u32;
        let y1 = (canvas_rect.max_y.ceil() as i64).clamp(0, canvas_h as i64) as u32;
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        self.ink_damage
            .borrow_mut()
            .push(Rect::new(x0, STATUS_BAR_H + y0, x1 - x0, y1 - y0));
    }

    /// Capture the current live ink state into a [`PageState`] snapshot.
    ///
    /// Clones `committed_layer`, `in_progress`, and `in_progress_base_width`
    /// so they can be stored in `PageBook` and later restored.
    fn snapshot_live_state(&self, _canvas_h: u32) -> PageState {
        PageState {
            committed: self.committed_layer.borrow().clone(),
            in_progress: self.in_progress.borrow().clone(),
            in_progress_base_width: *self.in_progress_base_width.borrow(),
        }
    }

    /// Restore the live ink state from `PageBook` for `page_index` and update
    /// the status bar properties.
    ///
    /// Must be called *after* `PageBook::navigate_to` or `push_blank_page` so
    /// that `book.current_index()` already reflects the target.
    fn load_page_state(&self, _page_index: usize) {
        let (new_committed, new_ip, new_bw, new_cur, new_count) = {
            let book = self.pages.borrow();
            let ps = book.current_page();
            (
                ps.committed.clone(),
                ps.in_progress.clone(),
                ps.in_progress_base_width,
                book.current_index(),
                book.page_count(),
            )
        };

        // Restore ink layers.
        *self.committed_layer.borrow_mut() = new_committed;
        *self.in_progress.borrow_mut() = new_ip;
        *self.in_progress_base_width.borrow_mut() = new_bw;

        // Reset the ink engine so no dangling stroke state bleeds across pages.
        *self.engine.borrow_mut() = InkEngine::new(InkConfig {
            base_width: new_bw,
            ..InkConfig::default()
        });

        // Update the Slint status bar (1-based display index).
        self.component.set_page_index((new_cur + 1) as i32);
        self.component.set_page_count(new_count as i32);

        // Switching pages replaces the whole canvas — force a Clear refresh so
        // the outgoing page leaves no ghost and the incoming ink is presented.
        self.screen_change.set(true);
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
    use ephemeris_pal::input::PenSample;

    fn pen_sample(x: f32, y: f32) -> PenSample {
        PenSample {
            x,
            y,
            pressure: 0.5,
            tilt: 0.0,
            in_range: true,
        }
    }

    // ── Original skeleton tests (must remain passing) ─────────────────────

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
        let damage = ui.render_frame(&mut display).expect("render_frame failed");

        // The first render must repaint the whole window.
        assert!(
            !damage.is_empty(),
            "first render should produce damage rects"
        );
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

    // ── New: live ink rendering tests (acceptance criteria for 6iy.2) ─────

    /// Feed PenDown → PenMove → PenUp through feed_input and verify that:
    /// (a) the ink appears in the canvas region, and
    /// (b) the compositing code never writes ink outside the canvas bounds.
    ///
    /// We verify (b) directly through the layer bounds (the committed layer is
    /// canvas-sized; the compositor only applies it to canvas rows) rather than
    /// by pixel-comparing two rendered frames (which would be sensitive to
    /// font-rendering non-determinism introduced by changing the page title).
    #[test]
    fn pen_down_move_up_produces_ink_only_in_canvas() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");

        // ── Draw a horizontal stroke at canvas Y=50 ──────────────────────────
        let mut t = 0u32;
        let step = 8u32;
        ui.feed_input(&InputEvent::PenDown(pen_sample(100.0, 50.0)), t);
        t += step;
        for x in (110..700u32).step_by(5) {
            ui.feed_input(&InputEvent::PenMove(pen_sample(x as f32, 50.0)), t);
            t += step;
        }
        ui.feed_input(&InputEvent::PenUp(pen_sample(700.0, 50.0)), t);

        // ── (b) Ink layer is canvas-sized → can never touch chrome rows ──────
        //
        // The committed layer has height = height - STATUS_BAR_H - TOOLBAR_H.
        // The compositor adds STATUS_BAR_H as a Y offset, so the first row of
        // ink lands at buf row STATUS_BAR_H (first canvas row) and the last at
        // buf row (height - TOOLBAR_H - 1).  There is no path for ink to reach
        // the status bar [0, STATUS_BAR_H) or toolbar [height-TOOLBAR_H, height).
        let canvas_h = 600 - STATUS_BAR_H - TOOLBAR_H;
        let committed = ui.committed_layer.borrow();
        assert_eq!(
            committed.height, canvas_h,
            "committed layer height must equal canvas height (= total - status - toolbar)"
        );
        assert_eq!(
            committed.width, 800,
            "committed layer width must equal screen width"
        );
        // The committed layer itself has no rows for status bar or toolbar,
        // so by construction ink cannot reach those regions in the composite.

        // ── (a) Ink appears in the canvas region ─────────────────────────────
        // Check the committed layer directly at canvas row 50 (the stroke Y).
        let stride = committed.stride as usize;
        let any_dark = (100..700usize).any(|col| committed.data[50 * stride + col] < 200);
        assert!(
            any_dark,
            "committed layer must have dark pixels along the stroke (canvas row 50)"
        );

        // ── Also verify via a rendered frame ─────────────────────────────────
        struct CapturingDisplay {
            last: Option<PixelBuf>,
        }
        impl Display for CapturingDisplay {
            fn size(&self) -> (u32, u32) {
                (800, 600)
            }
            fn present(
                &mut self,
                buf: &PixelBuf,
                _: &[Rect],
                _: RefreshMode,
            ) -> Result<(), ephemeris_pal::DisplayError> {
                self.last = Some(buf.clone());
                Ok(())
            }
        }
        drop(committed); // release borrow before render

        let mut cap = CapturingDisplay { last: None };
        ui.render_frame(&mut cap).expect("render failed");

        if let Some(buf) = cap.last {
            let stride = buf.stride as usize;
            // Canvas row 50 → buf row STATUS_BAR_H + 50 = 74.
            let stroke_buf_row = (STATUS_BAR_H + 50) as usize;
            let any_dark_in_frame =
                (100..700usize).any(|col| buf.data[stroke_buf_row * stride + col] < 200);
            assert!(
                any_dark_in_frame,
                "rendered frame must show dark ink pixels in canvas region (buf row {stroke_buf_row})"
            );

            // Status bar and toolbar pixel values must all be ≥ some reasonable
            // threshold proving Slint rendered UI chrome (not solid ink black).
            // The status bar background is #e0e0e0 → luma ≈ 224; text may be
            // darker but we just check we didn't wipe a whole row to 0.
            for row in 0..STATUS_BAR_H as usize {
                let row_min = (0..800usize)
                    .map(|col| buf.data[row * stride + col])
                    .min()
                    .unwrap_or(255);
                // Even with dark text the minimum should not be 0 everywhere.
                // We only assert the *average* row value is not pitch black:
                let row_sum: u32 = (0..800usize)
                    .map(|col| buf.data[row * stride + col] as u32)
                    .sum();
                let row_avg = row_sum / 800;
                assert!(
                    row_avg > 50,
                    "status bar row {row} average pixel {row_avg} suggests ink leaked (min={row_min})"
                );
            }
        }
    }

    /// Committed strokes survive a subsequent render call.
    #[test]
    fn committed_strokes_survive_subsequent_render() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");

        // Draw and commit a stroke.
        let mut t = 0u32;
        ui.feed_input(&InputEvent::PenDown(pen_sample(200.0, 100.0)), t);
        t += 8;
        for x in (210..400u32).step_by(5) {
            ui.feed_input(&InputEvent::PenMove(pen_sample(x as f32, 100.0)), t);
            t += 8;
        }
        ui.feed_input(&InputEvent::PenUp(pen_sample(400.0, 100.0)), t);

        struct CapturingDisplay {
            last: Option<PixelBuf>,
        }
        impl Display for CapturingDisplay {
            fn size(&self) -> (u32, u32) {
                (800, 600)
            }
            fn present(
                &mut self,
                buf: &PixelBuf,
                _: &[Rect],
                _: RefreshMode,
            ) -> Result<(), ephemeris_pal::DisplayError> {
                self.last = Some(buf.clone());
                Ok(())
            }
        }

        // First render captures the stroke.
        let mut cap1 = CapturingDisplay { last: None };
        ui.render_frame(&mut cap1).expect("first render failed");
        let _buf1 = cap1.last.expect("first render must produce a frame");

        // Second render — Slint may skip (nothing dirty), but the ink layer
        // must still be composited.  Force a re-render by touching a property.
        ui.set_page_title("Page 2");
        let mut cap2 = CapturingDisplay { last: None };
        ui.render_frame(&mut cap2).expect("second render failed");

        // If Slint skipped (nothing repainted) we cannot observe the second frame.
        // The important assertion is that the committed layer still holds the
        // stroke, which we verify by checking its data directly.
        let committed = ui.committed_layer.borrow();
        let stride = committed.stride as usize;
        // Stroke was drawn at canvas Y=100; layer Y=100 holds those pixels.
        let any_dark = (200..400usize).any(|col| committed.data[100 * stride + col] < 200);
        assert!(
            any_dark,
            "committed layer must retain stroke after subsequent render"
        );

        // If we got a second frame, check it too.
        if let Some(buf2) = cap2.last {
            let stroke_row = (STATUS_BAR_H + 100) as usize;
            let any_dark_2 =
                (200..400usize).any(|col| buf2.data[stroke_row * buf2.stride as usize + col] < 200);
            assert!(any_dark_2, "stroke must survive in second rendered frame");
        }
    }

    /// `push_canvas_pixels` replaces the ink layer and appears in the next frame.
    #[test]
    fn push_canvas_pixels_composites_into_frame() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");

        // Build a canvas-sized pixel buffer with a horizontal black bar at row 50.
        let canvas_h = 600 - STATUS_BAR_H - TOOLBAR_H;
        let mut ink = PixelBuf::new(800, canvas_h);
        for col in 0..800usize {
            ink.data[50 * 800 + col] = 0; // black row
        }
        ui.push_canvas_pixels(&ink);

        struct CapturingDisplay {
            last: Option<PixelBuf>,
        }
        impl Display for CapturingDisplay {
            fn size(&self) -> (u32, u32) {
                (800, 600)
            }
            fn present(
                &mut self,
                buf: &PixelBuf,
                _: &[Rect],
                _: RefreshMode,
            ) -> Result<(), ephemeris_pal::DisplayError> {
                self.last = Some(buf.clone());
                Ok(())
            }
        }

        let mut cap = CapturingDisplay { last: None };
        ui.render_frame(&mut cap).expect("render failed");

        if let Some(buf) = cap.last {
            // The black bar at canvas row 50 should appear at buf row 24+50=74.
            let buf_row = (STATUS_BAR_H + 50) as usize;
            let any_dark =
                (0..800usize).any(|col| buf.data[buf_row * buf.stride as usize + col] < 50);
            assert!(
                any_dark,
                "pushed canvas pixels must appear in the rendered frame"
            );
        }
    }

    /// `clear_ink` wipes both layers.
    #[test]
    fn clear_ink_wipes_committed_layer() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");

        // Draw a stroke.
        ui.feed_input(&InputEvent::PenDown(pen_sample(10.0, 10.0)), 0);
        ui.feed_input(&InputEvent::PenUp(pen_sample(90.0, 10.0)), 8);

        // Verify something was written.
        let had_ink = {
            let committed = ui.committed_layer.borrow();
            committed.data.iter().any(|&v| v < 255)
        };
        assert!(had_ink, "should have ink before clear");

        ui.clear_ink();

        // After clear, all pixels must be white.
        let committed = ui.committed_layer.borrow();
        assert!(
            committed.data.iter().all(|&v| v == 255),
            "committed layer must be all-white after clear_ink"
        );
        assert!(
            ui.in_progress.borrow().is_empty(),
            "in-progress must be empty after clear"
        );
    }

    /// Canvas geometry: the canvas height must be height - STATUS_BAR_H - TOOLBAR_H.
    #[test]
    fn canvas_fills_screen_minus_chrome() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");
        let expected_canvas_h = 600 - STATUS_BAR_H - TOOLBAR_H;
        assert_eq!(
            ui.canvas_height(),
            expected_canvas_h,
            "canvas height must equal screen height minus status bar ({STATUS_BAR_H}px) and toolbar ({TOOLBAR_H}px)"
        );
        assert_eq!(
            ui.committed_layer.borrow().height,
            expected_canvas_h,
            "committed layer dimensions must match canvas height"
        );
    }

    // ── Page navigation tests (acceptance criteria for 6iy.3) ────────────

    /// `prev_page` on the first page is a no-op: index stays at 1/1.
    #[test]
    fn prev_page_clamps_at_first_page() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");

        // Start: page 1 of 1.
        assert_eq!(ui.current_page_index(), (1, 1));

        // Calling prev_page should clamp — index must remain 1 of 1.
        ui.prev_page();
        assert_eq!(
            ui.current_page_index(),
            (1, 1),
            "prev_page on page 1 must not change index"
        );

        // A second call must still clamp.
        ui.prev_page();
        assert_eq!(ui.current_page_index(), (1, 1));
    }

    /// `next_page` past the last page auto-creates a new page and increments
    /// the page count.
    #[test]
    fn next_page_past_last_auto_creates_page() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");
        assert_eq!(ui.current_page_index(), (1, 1));

        // Navigate forward — a new blank page should be created.
        ui.next_page();
        assert_eq!(
            ui.current_page_index(),
            (2, 2),
            "next_page past last page must create page 2 of 2"
        );

        // Navigate forward again — page 3 created.
        ui.next_page();
        assert_eq!(ui.current_page_index(), (3, 3));
    }

    /// Round-tripping next_page / prev_page returns to the starting page.
    #[test]
    fn next_then_prev_returns_to_start() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");

        ui.next_page(); // → page 2
        ui.prev_page(); // → page 1

        assert_eq!(
            ui.current_page_index(),
            (1, 2),
            "returning from page 2 to page 1 must show (1, 2)"
        );
    }

    /// Per-page ink layers are preserved across navigation: draw on page 1,
    /// navigate to page 2 (blank), return to page 1 — the committed layer
    /// must still contain the original stroke.
    #[test]
    fn per_page_ink_preserved_across_navigation() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");

        // ── Draw a stroke on page 1 ─────────────────────────────────────
        let mut t = 0u32;
        let step = 8u32;
        ui.feed_input(&InputEvent::PenDown(pen_sample(100.0, 80.0)), t);
        t += step;
        for x in (110..500u32).step_by(5) {
            ui.feed_input(&InputEvent::PenMove(pen_sample(x as f32, 80.0)), t);
            t += step;
        }
        ui.feed_input(&InputEvent::PenUp(pen_sample(500.0, 80.0)), t);

        // Verify ink on page 1.
        let had_ink_page1 = {
            let committed = ui.committed_layer.borrow();
            let stride = committed.stride as usize;
            (100..500usize).any(|col| committed.data[80 * stride + col] < 200)
        };
        assert!(had_ink_page1, "page 1 must have ink before navigation");

        // ── Navigate to page 2 — should be blank ────────────────────────
        ui.next_page();
        assert_eq!(ui.current_page_index(), (2, 2));

        let page2_blank = {
            let committed = ui.committed_layer.borrow();
            committed.data.iter().all(|&v| v == 255)
        };
        assert!(page2_blank, "page 2 must be blank after auto-create");

        // ── Return to page 1 — ink must be intact ───────────────────────
        ui.prev_page();
        assert_eq!(ui.current_page_index(), (1, 2));

        let ink_survived = {
            let committed = ui.committed_layer.borrow();
            let stride = committed.stride as usize;
            (100..500usize).any(|col| committed.data[80 * stride + col] < 200)
        };
        assert!(
            ink_survived,
            "page 1 committed ink layer must survive round-trip navigation"
        );
    }

    /// Status bar properties reflect the correct page index and count after
    /// navigation.
    #[test]
    fn status_bar_reflects_page_after_navigation() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");

        // Initial state.
        assert_eq!(ui.component.get_page_index(), 1);
        assert_eq!(ui.component.get_page_count(), 1);

        ui.next_page();
        assert_eq!(ui.component.get_page_index(), 2);
        assert_eq!(ui.component.get_page_count(), 2);

        ui.prev_page();
        assert_eq!(ui.component.get_page_index(), 1);
        assert_eq!(ui.component.get_page_count(), 2);
    }

    /// `swipe-left` callback emitted from `.slint` wires correctly to
    /// `prev_page` via `wire_swipe_navigation`.
    #[test]
    fn swipe_left_callback_wires_to_prev_page() {
        let ui = Rc::new(EphemerisUi::new(800, 600).expect("UI construction failed"));

        // Create two pages so we have somewhere to go back from.
        ui.next_page();
        assert_eq!(ui.current_page_index(), (2, 2));

        EphemerisUi::wire_swipe_navigation(ui.clone());

        // Simulate the `.slint` swipe-left callback firing.
        ui.component.invoke_swipe_left();

        assert_eq!(
            ui.current_page_index(),
            (1, 2),
            "swipe-left must call prev_page"
        );
    }

    /// `swipe-right` callback emitted from `.slint` wires correctly to
    /// `next_page` via `wire_swipe_navigation`.
    #[test]
    fn swipe_right_callback_wires_to_next_page() {
        let ui = Rc::new(EphemerisUi::new(800, 600).expect("UI construction failed"));

        EphemerisUi::wire_swipe_navigation(ui.clone());

        // Simulate the `.slint` swipe-right callback firing — auto-creates page.
        ui.component.invoke_swipe_right();

        assert_eq!(
            ui.current_page_index(),
            (2, 2),
            "swipe-right must call next_page (auto-create)"
        );
    }

    // ── Eink render-loop / damage tests (acceptance criteria for 2ql.2) ───

    /// A display that records every `present` call: how many times it was
    /// invoked and the damage passed to the last call.
    struct RecordingDisplay {
        presents: usize,
        last_damage: Vec<Rect>,
        last_mode: Option<RefreshMode>,
    }
    impl RecordingDisplay {
        fn new() -> Self {
            RecordingDisplay {
                presents: 0,
                last_damage: Vec::new(),
                last_mode: None,
            }
        }
    }
    impl Display for RecordingDisplay {
        fn size(&self) -> (u32, u32) {
            (800, 600)
        }
        fn present(
            &mut self,
            _buf: &PixelBuf,
            damage: &[Rect],
            mode: RefreshMode,
        ) -> Result<(), ephemeris_pal::DisplayError> {
            self.presents += 1;
            self.last_damage = damage.to_vec();
            self.last_mode = Some(mode);
            Ok(())
        }
    }

    /// Bounding box (x0, y0, x1, y1) of a damage list; panics if empty.
    fn damage_bbox(rects: &[Rect]) -> (u32, u32, u32, u32) {
        let x0 = rects.iter().map(|r| r.x).min().unwrap();
        let y0 = rects.iter().map(|r| r.y).min().unwrap();
        let x1 = rects.iter().map(|r| r.x + r.width).max().unwrap();
        let y1 = rects.iter().map(|r| r.y + r.height).max().unwrap();
        (x0, y0, x1, y1)
    }

    /// The eink loop must not repaint when nothing changed: after the first
    /// frame is flushed, a second `render_frame` with no state change returns
    /// empty damage and never calls `present`.
    #[test]
    fn idle_render_produces_no_damage_and_no_present() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");
        let mut disp = RecordingDisplay::new();

        // First frame: Slint paints the whole window → damage + one present.
        let d1 = ui.render_frame(&mut disp).expect("first render failed");
        assert!(!d1.is_empty(), "first render must report damage");
        assert_eq!(disp.presents, 1, "first render must present once");

        // Second frame: nothing changed → no damage, no present (no idle repaint).
        let d2 = ui.render_frame(&mut disp).expect("second render failed");
        assert!(
            d2.is_empty(),
            "idle render must report no damage, got {d2:?}"
        );
        assert_eq!(
            disp.presents, 1,
            "idle render must not call present (no idle repaint)"
        );
    }

    /// An ink-only update (no Slint property touched) must still present, and
    /// the reported damage must be confined to the canvas region — proving the
    /// damage comes from real partial-render info, not a hardcoded full-screen
    /// rect.
    #[test]
    fn ink_only_update_reports_canvas_bounded_damage() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");
        let mut disp = RecordingDisplay::new();

        // Flush the initial full-window paint so Slint is clean afterwards.
        ui.render_frame(&mut disp).expect("initial render failed");
        // Confirm the UI is now idle.
        assert!(ui
            .render_frame(&mut disp)
            .expect("idle render failed")
            .is_empty());
        let presents_before = disp.presents;

        // Draw a short stroke — this only touches the ink layer.
        ui.feed_input(&InputEvent::PenDown(pen_sample(100.0, 40.0)), 0);
        ui.feed_input(&InputEvent::PenMove(pen_sample(300.0, 40.0)), 8);
        ui.feed_input(&InputEvent::PenUp(pen_sample(300.0, 40.0)), 16);

        let damage = ui.render_frame(&mut disp).expect("ink render failed");

        assert!(
            !damage.is_empty(),
            "ink-only update must produce damage so the frame is presented"
        );
        assert_eq!(
            disp.presents,
            presents_before + 1,
            "ink-only update must trigger exactly one present"
        );

        // Damage must lie strictly inside the canvas region: below the status
        // bar and above the toolbar. This proves it is not a full-screen rect.
        let (_x0, y0, _x1, y1) = damage_bbox(&damage);
        assert!(
            y0 >= STATUS_BAR_H,
            "ink damage top {y0} must not intrude into the status bar (< {STATUS_BAR_H})"
        );
        assert!(
            y1 <= 600 - TOOLBAR_H,
            "ink damage bottom {y1} must not intrude into the toolbar (> {})",
            600 - TOOLBAR_H
        );
    }

    /// A partial UI change (status-bar text) must report damage taken from
    /// Slint's partial-render region, i.e. strictly smaller than the full
    /// window — otherwise we would just be emitting a hardcoded full-screen rect.
    #[test]
    fn partial_ui_change_reports_partial_slint_damage() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");
        let mut disp = RecordingDisplay::new();

        // Flush the initial full-window paint.
        ui.render_frame(&mut disp).expect("initial render failed");
        assert!(ui
            .render_frame(&mut disp)
            .expect("idle render failed")
            .is_empty());

        // Change only the status-bar title → Slint repaints just that text.
        ui.set_page_title("A different title");
        let damage = ui.render_frame(&mut disp).expect("partial render failed");

        assert!(!damage.is_empty(), "title change must report damage");
        let (_x0, _y0, _x1, y1) = damage_bbox(&damage);
        assert!(
            y1 < 600,
            "partial repaint of the status bar must not span the full window \
             height (bottom={y1}); damage should reflect Slint's partial region"
        );
    }

    /// The refresh scheduler drives the [`RefreshMode`] handed to the display:
    /// the first paint is a `Clear`, a subsequent ink-only frame is `Fast`, a
    /// UI-only change is `Partial`, and a page switch is `Clear`.
    #[test]
    fn scheduler_drives_refresh_mode() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");
        let mut disp = RecordingDisplay::new();

        // First paint of a fresh screen → Clear.
        ui.render_frame(&mut disp).expect("first render failed");
        assert_eq!(disp.last_mode, Some(RefreshMode::Clear));
        // Drain to idle.
        assert!(ui
            .render_frame(&mut disp)
            .expect("idle render failed")
            .is_empty());

        // Ink-only frame → Fast.
        ui.feed_input(&InputEvent::PenDown(pen_sample(100.0, 40.0)), 0);
        ui.feed_input(&InputEvent::PenMove(pen_sample(300.0, 40.0)), 8);
        ui.feed_input(&InputEvent::PenUp(pen_sample(300.0, 40.0)), 16);
        ui.render_frame(&mut disp).expect("ink render failed");
        assert_eq!(disp.last_mode, Some(RefreshMode::Fast));

        // UI-only change (status-bar title) → Partial.
        ui.set_page_title("Another title");
        ui.render_frame(&mut disp).expect("ui render failed");
        assert_eq!(disp.last_mode, Some(RefreshMode::Partial));

        // Page navigation → Clear.
        ui.next_page();
        ui.render_frame(&mut disp).expect("nav render failed");
        assert_eq!(disp.last_mode, Some(RefreshMode::Clear));
    }

    /// The scheduler's periodic-full policy is configurable via
    /// [`EphemerisUi::set_refresh_config`]: after N fast/partial frames it emits
    /// a `Full` to flush accumulated ghosting.
    #[test]
    fn set_refresh_config_controls_periodic_full() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");
        ui.set_refresh_config(RefreshConfig {
            full_refresh_interval: 3,
            ..RefreshConfig::default()
        });
        let mut disp = RecordingDisplay::new();

        // Drain the initial screen-change paint (resets the counter).
        ui.render_frame(&mut disp).expect("first render failed");
        assert!(ui
            .render_frame(&mut disp)
            .expect("idle render failed")
            .is_empty());

        // Three ink frames: Fast, Fast, then Full (3rd hits the interval).
        let mut modes = Vec::new();
        for i in 0..3 {
            let t = i * 16;
            ui.feed_input(&InputEvent::PenDown(pen_sample(10.0, 40.0)), t);
            ui.feed_input(&InputEvent::PenUp(pen_sample(12.0, 40.0)), t + 8);
            ui.render_frame(&mut disp).expect("ink render failed");
            modes.push(disp.last_mode.unwrap());
        }
        assert_eq!(
            modes,
            vec![RefreshMode::Fast, RefreshMode::Fast, RefreshMode::Full]
        );
    }
}
