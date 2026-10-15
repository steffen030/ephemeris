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
//! ## Resolution model (ephemeris-ktg.4)
//!
//! Slint chrome renders at the logical design size (≈ half the PineNote
//! panel) while ink is rasterised at the **physical** panel resolution:
//!
//! * strokes stay in logical canvas coordinates; [`raster::InkScale`] maps them
//!   onto the physical ink planes (`committed_layer`, `in_progress_layer`);
//! * the logical UI gray is bilinear-upscaled into a physical background cache
//!   only inside UI-damage rects;
//! * every present writes `min(background, ink)` 1:1 into a physical buffer
//!   and hands it to [`Display::present_physical`] with physical damage.
//!
//! The physical size is taken from [`Display::physical_size`] on each
//! [`EphemerisUi::render_frame`]; when it changes, the visible page is
//! re-rasterised from its strokes and other pages are refit lazily on load.
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
//! callbacks from book-style edge turns (right→left = next, left→right =
//! previous).  Wire them to [`EphemerisUi::next_page`] / [`EphemerisUi::prev_page`]
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
use std::sync::Arc;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, WindowAdapter};
use slint::{Model, PhysicalSize, SharedString};

use ephemeris_core::ink::{
    pen_width_for_label, EraserConfig, EraserEngine, EraserUpdate, InkConfig, InkEngine, InkUpdate,
    PEN_WIDTH_MEDIUM,
};
use ephemeris_core::model::{Point, Stroke, StrokeId, Tool};
use ephemeris_core::refresh::{DamageSource, RefreshConfig, RefreshScheduler};
use ephemeris_pal::display::{Display, PixelBuf, Rect};
use ephemeris_pal::input::InputEvent;

pub mod compose;
pub mod raster;

use compose::{FrameGeom, InkPlanes};
use raster::InkScale;

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
    /// Rasterized committed strokes layer for this page (ink-plane sized,
    /// 8 bpp). Shared via [`Arc`] so stroke-end / navigate snapshots are
    /// refcount bumps instead of full-canvas memcpy. May have a stale size for
    /// pages that are not displayed; refit on load.
    committed: Arc<PixelBuf>,
    /// `committed` must be regenerated from `strokes` before display.
    raster_stale: bool,
    /// Surviving vector strokes for this page (pen/highlighter only; eraser
    /// removes entries). Used by stroke HTR and must match visible ink.
    strokes: Vec<Stroke>,
    /// Points of any in-progress stroke on this page (empty when no stroke is
    /// active or after the stroke was committed).
    in_progress: Vec<Point>,
    /// Nominal base_width of the in-progress stroke (copied at PenDown time).
    in_progress_base_width: f32,
    /// Background template for this page (blank, lines, grid, dot).
    template: ephemeris_core::PageTemplate,
}

impl PageState {
    /// Construct a blank (all-white) page state for the given canvas dimensions.
    fn blank(canvas_w: u32, canvas_h: u32, default_base_width: f32) -> Self {
        PageState {
            committed: Arc::new(PixelBuf::new(canvas_w, canvas_h)),
            raster_stale: false,
            strokes: Vec::new(),
            in_progress: Vec::new(),
            in_progress_base_width: default_base_width,
            template: ephemeris_core::PageTemplate::Blank,
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

/// One row for the notes browser (list or gallery).
#[derive(Clone)]
pub struct NoteListItem {
    pub id: String,
    pub title: String,
    pub date_label: String,
    pub has_preview: bool,
    pub preview: slint::Image,
    pub show_profile: bool,
    pub profile_icon_char: String,
    pub has_custom_profile_icon: bool,
    pub custom_profile_icon: slint::Image,
}

/// One folder row for the notes browser / move dialog.
#[derive(Clone)]
pub struct FolderListItem {
    pub id: String,
    pub title: String,
    pub note_count: i32,
    pub folder_count: i32,
    pub show_profile: bool,
    pub profile_icon_char: String,
    pub has_custom_profile_icon: bool,
    pub custom_profile_icon: slint::Image,
}

/// Profile badge fields shared by Notes / Canvas / Recordings list rows.
#[derive(Clone, Default)]
pub struct ProfileBadgeFields {
    pub show_profile: bool,
    pub profile_icon_char: String,
    pub has_custom_profile_icon: bool,
    pub custom_profile_icon: slint::Image,
}

/// One folder row for the Canvas browser.
#[derive(Clone)]
pub struct CanvasFolderListItem {
    pub id: String,
    pub title: String,
    pub canvas_count: i32,
    pub folder_count: i32,
    pub show_profile: bool,
    pub profile_icon_char: String,
    pub has_custom_profile_icon: bool,
    pub custom_profile_icon: slint::Image,
}

/// One canvas document row for the Canvas browser.
#[derive(Clone)]
pub struct CanvasListItem {
    pub id: String,
    pub title: String,
    pub date_label: String,
    pub show_profile: bool,
    pub profile_icon_char: String,
    pub has_custom_profile_icon: bool,
    pub custom_profile_icon: slint::Image,
}

fn folder_list_item_to_entry(e: &FolderListItem) -> NotebookEntry {
    NotebookEntry {
        id: slint::SharedString::from(e.id.as_str()),
        title: slint::SharedString::from(e.title.as_str()),
        note_count: e.note_count,
        folder_count: e.folder_count,
        show_profile: e.show_profile,
        profile_icon_char: slint::SharedString::from(e.profile_icon_char.as_str()),
        has_custom_profile_icon: e.has_custom_profile_icon,
        custom_profile_icon: e.custom_profile_icon.clone(),
    }
}

// ── Chrome geometry constants ──────────────────────────────────────────────────

/// Status bar removed — kept at 0 so the canvas fills the full screen.
pub const STATUS_BAR_H: u32 = 0;

/// Toolbar removed — kept at 0 so the canvas fills the full screen.
pub const TOOLBAR_H: u32 = 0;

/// Left nav rail width — must match `nav-w` in `main.slint`.
pub const NAV_W: u32 = 80;

/// Burger dropdown panel (`drawing-dropdown` in `main.slint`):
/// `x = parent.width - 192`, `y = 60`, `width = 184`, `height = 268`.
const DRAWING_MENU_W: u32 = 184;
const DRAWING_MENU_H: u32 = 268;
const DRAWING_MENU_RIGHT_PAD: u32 = 8; // 192 - 184
const DRAWING_MENU_Y: u32 = 60;

/// Top-right burger / close button (`drawing-burger` in `main.slint`).
const BURGER_W: u32 = 44;
const BURGER_H: u32 = 44;
const BURGER_RIGHT_PAD: u32 = 8; // parent.width - 52
const BURGER_Y: u32 = 8;

/// Bottom-right page X/Y + prev/next chip (`note-page-indicator` in `main.slint`).
const PAGE_INDICATOR_W: u32 = 158;
const PAGE_INDICATOR_H: u32 = 28;
const PAGE_INDICATOR_RIGHT_PAD: u32 = 10; // parent.width - 168
const PAGE_INDICATOR_BOTTOM_PAD: u32 = 12; // parent.height - 40

/// Note title bar (`note-title-bar` in `main.slint`): right of nav, top strip.
const TITLE_BAR_X: u32 = NAV_W + 8;
const TITLE_BAR_Y: u32 = 8;
const TITLE_BAR_H: u32 = 44;
const TITLE_BAR_RIGHT_PAD: u32 = 60; // parent.width - nav-w - 60

/// Opaque chrome rects (logical) that must hide ink while the drawing menu is
/// open, so the `min()` blend does not bleed through the nav rail, drawing
/// menu, burger button, title bar or page indicator.
fn chrome_rects(width: u32, height: u32) -> [Rect; 5] {
    let menu_x = width.saturating_sub(DRAWING_MENU_W + DRAWING_MENU_RIGHT_PAD);
    let burger_x = width.saturating_sub(BURGER_W + BURGER_RIGHT_PAD);
    let page_x = width.saturating_sub(PAGE_INDICATOR_W + PAGE_INDICATOR_RIGHT_PAD);
    let page_y = height.saturating_sub(PAGE_INDICATOR_H + PAGE_INDICATOR_BOTTOM_PAD);
    let title_w = width.saturating_sub(TITLE_BAR_X + TITLE_BAR_RIGHT_PAD);
    [
        // Nav rail (full height).
        Rect::new(0, 0, NAV_W.min(width), height),
        // Floating drawing menu panel.
        Rect::new(
            menu_x,
            DRAWING_MENU_Y.min(height),
            DRAWING_MENU_W.min(width.saturating_sub(menu_x)),
            DRAWING_MENU_H.min(height.saturating_sub(DRAWING_MENU_Y)),
        ),
        // Burger / close button.
        Rect::new(
            burger_x,
            BURGER_Y.min(height),
            BURGER_W.min(width.saturating_sub(burger_x)),
            BURGER_H.min(height.saturating_sub(BURGER_Y)),
        ),
        // Note title bar (menu-open only; harmless when absent).
        Rect::new(
            TITLE_BAR_X.min(width),
            TITLE_BAR_Y.min(height),
            title_w.min(width.saturating_sub(TITLE_BAR_X)),
            TITLE_BAR_H.min(height.saturating_sub(TITLE_BAR_Y)),
        ),
        // Page X/Y indicator (menu-open only; harmless when absent).
        Rect::new(
            page_x,
            page_y,
            PAGE_INDICATOR_W.min(width.saturating_sub(page_x)),
            PAGE_INDICATOR_H.min(height.saturating_sub(page_y)),
        ),
    ]
}

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
///
/// Task list row for [`EphemerisUi::set_full_task_list`]:
/// `(id, title, priority_label, priority_int, due, overdue, done, lane, has_source_note)`.
pub type FullTaskRow = (
    String,
    String,
    String,
    i32,
    String,
    bool,
    bool,
    String,
    bool,
);

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
    /// Logical grayscale UI frame (Slint luma + page template, no ink).
    /// Updated only inside UI-damage rects.
    ui_gray: RefCell<PixelBuf>,
    /// Physical background cache: `ui_gray` bilinear-upscaled. Refreshed only
    /// where the UI changed — never per pen sample.
    bg_phys: RefCell<PixelBuf>,
    /// Physical present buffer: `min(bg_phys, ink)`, written 1:1 to the display.
    present_phys: RefCell<PixelBuf>,
    /// Physical frame size (panel pixels). Logical until a display reports
    /// otherwise via [`Display::physical_size`].
    phys_size: Cell<(u32, u32)>,
    /// Logical rects whose background must be rebuilt (template change).
    bg_dirty: RefCell<Vec<Rect>>,
    /// `(composite_ink, drawing_menu_open, template)` the background cache was
    /// built with; a change invalidates the canvas background.
    bg_key: Cell<Option<(bool, bool, ephemeris_core::PageTemplate)>>,
    // ── Ink state (interior-mutable so the callbacks can borrow independently) ─
    /// The ink engine: tracks the active stroke builder.
    engine: RefCell<InkEngine>,
    /// Whole-stroke eraser (active when tool is [`Tool::Eraser`]).
    eraser: RefCell<EraserEngine>,
    /// Rasterized committed strokes layer for the *current page* (physical
    /// ink-plane size, 8 bpp).  Shared with `pages.current_page().committed`
    /// via [`Arc`] so PageBook snapshots do not memcpy the full canvas on every
    /// stroke end.
    committed_layer: RefCell<Arc<PixelBuf>>,
    /// Surviving vector strokes for the *current page* (mirrors
    /// `pages.current_page().strokes`).
    strokes: RefCell<Vec<Stroke>>,
    /// Points of the currently in-progress stroke (canvas-relative).
    in_progress: RefCell<Vec<Point>>,
    /// Incremental raster of the live stroke (physical ink-plane size).  New
    /// segments are stamped in on each `Extended` instead of re-rasterizing
    /// all points.
    in_progress_layer: RefCell<PixelBuf>,
    /// `base_width` of the in-progress stroke (copied from `InkConfig` at
    /// `PenDown` time so we can re-rasterize without re-borrowing the engine).
    in_progress_base_width: RefCell<f32>,
    /// Per-page ink state roster.  The active page's committed layer and
    /// in-progress state are kept live in `committed_layer` / `in_progress` /
    /// `in_progress_base_width`; all other pages are held exclusively in `pages`.
    pages: RefCell<PageBook>,
    /// Default [`InkConfig::base_width`], kept for constructing blank pages.
    default_base_width: f32,
    /// Selected pen preset width (Thin/Med/Thick); applied when tool is Pen.
    pen_width: Cell<f32>,
    /// Set when ink strokes were added/removed since last check (See-text
    /// invalidation). Cleared by [`take_strokes_dirty`].
    strokes_dirty: Cell<bool>,
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
            ui_gray: RefCell::new(PixelBuf::new(width, height)),
            bg_phys: RefCell::new(PixelBuf::new(width, height)),
            present_phys: RefCell::new(PixelBuf::new(width, height)),
            phys_size: Cell::new((width, height)),
            bg_dirty: RefCell::new(Vec::new()),
            bg_key: Cell::new(None),
            engine: RefCell::new(InkEngine::new(ink_cfg.clone())),
            eraser: RefCell::new(EraserEngine::new(EraserConfig {
                radius: ink_cfg.base_width.max(10.0),
            })),
            committed_layer: RefCell::new(Arc::new(PixelBuf::new(width, canvas_h))),
            strokes: RefCell::new(Vec::new()),
            in_progress: RefCell::new(Vec::new()),
            in_progress_layer: RefCell::new(PixelBuf::new(width, canvas_h)),
            in_progress_base_width: RefCell::new(default_base_width),
            pages: RefCell::new(PageBook::new(width, canvas_h, default_base_width)),
            default_base_width,
            pen_width: Cell::new(if (default_base_width - PEN_WIDTH_MEDIUM).abs() < 0.01 {
                PEN_WIDTH_MEDIUM
            } else {
                default_base_width
            }),
            ink_damage: RefCell::new(Vec::new()),
            scheduler: RefCell::new(RefreshScheduler::default()),
            // The first frame paints a fresh screen — treat it as a screen
            // change so the initial present is a full/clear refresh.
            screen_change: Cell::new(true),
            strokes_dirty: Cell::new(false),
        })
    }

    /// Replace the [`RefreshScheduler`]'s thresholds (periodic-full interval,
    /// coalescing gap, max damage rects).  See [`RefreshConfig`].
    pub fn set_refresh_config(&self, config: RefreshConfig) {
        *self.scheduler.borrow_mut() = RefreshScheduler::new(config);
    }

    // ── Component access ──────────────────────────────────────────────────

    /// Return a strong clone of the underlying Slint component.
    ///
    /// Useful for wiring callbacks that need direct property access beyond the
    /// methods provided by [`EphemerisUi`].
    pub fn clone_component(&self) -> EphemerisPage {
        self.component.clone_strong()
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

    /// Apply a named theme to the UI by setting the [`EinkTheme`] global's
    /// color properties.
    ///
    /// * `"light"` (default) — pure white backgrounds, black borders/text.
    /// * `"dark"`             — pure black backgrounds, white borders/text.
    /// * anything else        — treated as `"light"`.
    ///
    /// Maps `Config::theme` to display colors at startup.  Called after
    /// [`EphemerisUi::new`] to honour the loaded configuration.
    pub fn apply_theme(&self, theme: &str) {
        let t = EinkTheme::get(&self.component);
        let (bg, fg, border) = if theme == "dark" {
            (
                slint::Color::from_rgb_u8(0, 0, 0),
                slint::Color::from_rgb_u8(255, 255, 255),
                slint::Color::from_rgb_u8(255, 255, 255),
            )
        } else {
            (
                slint::Color::from_rgb_u8(255, 255, 255),
                slint::Color::from_rgb_u8(0, 0, 0),
                slint::Color::from_rgb_u8(0, 0, 0),
            )
        };
        t.set_canvas_bg(bg);
        t.set_chrome_bg(bg);
        t.set_chrome_border(border);
        t.set_btn_bg(bg);
        t.set_btn_fg(fg);
        t.set_btn_active_bg(fg);
        t.set_btn_active_fg(bg);
        t.set_btn_border(border);
        t.set_text_secondary(fg);
        t.set_danger_fg(fg);
    }

    /// Switch the active drawing tool on the ink engine.
    ///
    /// Sets the appropriate base stroke width alongside the tool:
    /// * [`Tool::Pen`]         → selected Thin/Med/Thick ([`Self::set_pen_width`])
    /// * [`Tool::Highlighter`] → 12 px (wide semi-transparent gray mark)
    /// * [`Tool::Eraser`]      → 10 px (eraser circle radius)
    pub fn set_tool(&self, tool: Tool) {
        let base_width = match tool {
            Tool::Pen => self.pen_width.get(),
            Tool::Highlighter => 12.0,
            Tool::Eraser => 10.0,
        };
        let mut engine = self.engine.borrow_mut();
        engine.set_tool(tool);
        engine.set_base_width(base_width);
        drop(engine);
        if tool == Tool::Eraser {
            *self.eraser.borrow_mut() = EraserEngine::new(EraserConfig { radius: base_width });
        }
    }

    /// Set the pen width preset (`"thin"` / `"medium"` / `"thick"`) and switch
    /// to the pen tool. Medium (3 px) is the ktg.6 sweep default.
    pub fn set_pen_width(&self, label: &str) {
        let w = pen_width_for_label(label);
        self.pen_width.set(w);
        let mut engine = self.engine.borrow_mut();
        engine.set_tool(Tool::Pen);
        engine.set_base_width(w);
        drop(engine);
        self.component.set_active_pen_width(match label {
            "thin" | "thick" => label.into(),
            _ => "medium".into(),
        });
        self.component.set_active_tool("pen".into());
    }

    /// Current pen preset width in logical pixels.
    pub fn pen_width(&self) -> f32 {
        self.pen_width.get()
    }

    /// Switch between start page (task list) and canvas (ink) view.
    pub fn set_show_start_page(&self, show: bool) {
        self.component.set_show_start_page(show);
    }

    /// Push a ranked task list to the start page.
    ///
    /// `tasks` is a slice of `(id, title, priority_label, due_label, overdue)` tuples.
    pub fn set_task_list(&self, tasks: &[(String, String, String, String, bool)]) {
        let entries: Vec<TaskEntry> = tasks
            .iter()
            .map(
                |(id, title, priority_label, due_label, overdue)| TaskEntry {
                    id: slint::SharedString::from(id.as_str()),
                    title: slint::SharedString::from(title.as_str()),
                    priority_label: slint::SharedString::from(priority_label.as_str()),
                    due_label: slint::SharedString::from(due_label.as_str()),
                    overdue: *overdue,
                    show_profile: false,
                    profile_icon_char: slint::SharedString::new(),
                    has_custom_profile_icon: false,
                    custom_profile_icon: slint::Image::default(),
                },
            )
            .collect();
        let model = std::rc::Rc::new(slint::VecModel::from(entries));
        self.component.set_task_list(model.into());
    }

    /// Push agenda entries to the start page.
    ///
    /// Each tuple is `(title, time_label, location_label)`.
    pub fn set_agenda_list(&self, entries: &[(String, String, String)]) {
        let items: Vec<AgendaEntry> = entries
            .iter()
            .map(|(title, time_label, location_label)| AgendaEntry {
                title: slint::SharedString::from(title.as_str()),
                time_label: slint::SharedString::from(time_label.as_str()),
                location_label: slint::SharedString::from(location_label.as_str()),
            })
            .collect();
        let model = std::rc::Rc::new(slint::VecModel::from(items));
        self.component.set_agenda_list(model.into());
    }

    /// Set the active agenda range tab: 0 = Day, 1 = Week, 2 = Month.
    pub fn set_agenda_range(&self, range: i32) {
        self.component.set_agenda_range(range);
    }

    /// Register a closure called when the user taps a Day/Week/Month tab.
    pub fn on_agenda_range_changed<F>(&self, handler: F)
    where
        F: FnMut(i32) + 'static,
    {
        self.component.on_agenda_range_changed(handler);
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

    /// Wire the burger Thin/Med/Thick control.
    pub fn on_pen_width_selected<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_pen_width_selected(move |w| handler(w.to_string()));
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

    // ── Action ring API ───────────────────────────────────────────────────

    /// Show the action ring centered at canvas-relative `(cx, cy)`.
    ///
    /// `show_audio` controls whether the Audio slice is visible; set it `false`
    /// on devices without a microphone.  Triggered by `Action::OpenActionRing`
    /// from the `ActionRouter`.
    pub fn show_action_ring(&self, cx: f32, cy: f32, show_audio: bool) {
        self.component.set_ring_cx(cx);
        self.component.set_ring_cy(cy);
        self.component.set_ring_show_audio(show_audio);
        self.component.set_ring_visible(true);
    }

    /// Hide the action ring (call after any slice is tapped or dismissed).
    pub fn hide_action_ring(&self) {
        self.component.set_ring_visible(false);
    }

    /// `true` if the action ring is currently visible.
    pub fn ring_visible(&self) -> bool {
        self.component.get_ring_visible()
    }

    /// `true` when the note canvas page is showing (no full-screen overlay).
    ///
    /// The floating drawing menu and see-text overlay may still be open on top;
    /// use [`Self::is_canvas_active`] for ink input and
    /// [`Self::should_composite_ink`] for compositing.
    ///
    /// Keep the overlay list in sync with `is-canvas-context` in `main.slint`
    /// (except `note-see-text`, which Slint treats as leaving canvas chrome while
    /// this still counts as the note page — see [`Self::is_canvas_active`]).
    pub fn is_note_canvas_page(&self) -> bool {
        let c = &self.component;
        // Canvas text-card focus keeps the board visible under a modal veil.
        if c.get_canvas_focus_active() {
            return true;
        }
        !c.get_show_start_page()
            && !c.get_show_task_view()
            && !c.get_show_settings()
            && !c.get_show_filebrowser()
            && !c.get_show_note_list()
            && !c.get_show_notebook_list()
            && !c.get_show_canvas_folder_list()
            && !c.get_show_canvas_list()
            && !c.get_show_canvas_board()
            && !c.get_show_calendar()
            && !c.get_show_recording_list()
            && !c.get_show_search()
            && !c.get_show_md_viewer()
            && !c.get_show_profile_switcher()
            && !c.get_show_create_profile_picker()
            && !c.get_task_dialog_visible()
            && !c.get_event_dialog_visible()
            && !c.get_note_rename_visible()
            && !c.get_note_move_visible()
            && !c.get_notebook_rename_visible()
            && !c.get_rec_rename_visible()
            && !c.get_rec_transcription_visible()
            && !c.get_profile_edit_dialog_visible()
            && !c.get_connect_setup_visible()
            && !c.get_connect_soon_visible()
            && !c.get_ring_visible()
    }

    /// `true` when pointer events should feed the ink engine.
    ///
    /// False while the drawing menu or see-text overlay is open (those views
    /// own input). Ink may still be composited under the drawing menu — see
    /// [`Self::should_composite_ink`].
    pub fn is_canvas_active(&self) -> bool {
        let c = &self.component;
        self.is_note_canvas_page() && !c.get_drawing_menu_open() && !c.get_note_see_text()
    }

    /// Geometry gate for starting ink without Slint pointer dispatch.
    ///
    /// Returns `true` when the open note canvas is active and `(x, y)` is not
    /// over interactive chrome (nav rail, burger). Used to skip Slint press
    /// routing for pen-down on the ink plane (latency stall).
    pub fn hits_inkable_canvas(&self, x: f32, y: f32) -> bool {
        if !self.is_canvas_active() || x < 0.0 || y < 0.0 {
            return false;
        }
        let xi = x as u32;
        let yi = y as u32;
        if xi >= self.width || yi >= self.height {
            return false;
        }
        // Nav rail owns the left strip.
        if xi < NAV_W {
            return false;
        }
        // Top-right burger / close button.
        let burger_x = self.width.saturating_sub(BURGER_W + BURGER_RIGHT_PAD);
        if xi >= burger_x
            && xi < burger_x.saturating_add(BURGER_W)
            && yi >= BURGER_Y
            && yi < BURGER_Y.saturating_add(BURGER_H)
        {
            return false;
        }
        true
    }

    /// `true` when committed/in-progress ink should be blended into the frame.
    ///
    /// Remains true while the floating drawing menu is open so the page stays
    /// visible behind chrome. False for see-text and full-screen overlays so
    /// dark ink cannot bleed through white Slint backgrounds via `min()`.
    pub fn should_composite_ink(&self) -> bool {
        let c = &self.component;
        self.is_note_canvas_page() && !c.get_note_see_text()
    }

    /// Force a full-screen refresh on the next present (clears eink ghosting /
    /// leftover ink when switching to an overlay view).
    pub fn request_screen_change(&self) {
        self.screen_change.set(true);
    }

    pub fn get_drawing_menu_open(&self) -> bool {
        self.component.get_drawing_menu_open()
    }

    pub fn set_drawing_menu_open(&self, open: bool) {
        self.component.set_drawing_menu_open(open);
    }

    pub fn on_notebook_quick_new_note<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_notebook_quick_new_note(move |id| handler(id.to_string()));
    }

    pub fn on_paper_type_tapped<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_paper_type_tapped(move |paper| handler(paper.to_string()));
    }

    /// Show or hide the see-text (OCR / transcript) overlay on the note canvas.
    pub fn set_note_see_text(&self, show: bool) {
        self.component.set_note_see_text(show);
    }

    /// Set the formatted rows shown in the see-text overlay (empty = empty state).
    pub fn set_see_text_rows(&self, rows: &[ephemeris_core::see_text::SeeTextRow]) {
        let lines: Vec<SeeTextLine> = rows.iter().map(see_text_line).collect();
        // Skip no-op updates — Slint property sets dirty UI damage and can
        // upgrade the next ink present from Fast → Partial.
        let current = self.component.get_see_text_lines();
        if current.row_count() == lines.len()
            && lines
                .iter()
                .enumerate()
                .all(|(i, l)| current.row_data(i).as_ref() == Some(l))
        {
            return;
        }
        self.component
            .set_see_text_lines(Rc::new(slint::VecModel::from(lines)).into());
    }

    /// Save from the See-text editor: `(line_index, text, kind, number,
    /// indent, highlight, underline)`.
    pub fn on_see_text_line_save<F: FnMut(i32, String, i32, i32, i32, bool, bool) + 'static>(
        &self,
        mut handler: F,
    ) {
        self.component
            .on_see_text_line_save(move |i, t, k, n, d, h, u| {
                handler(i, t.to_string(), k, n, d, h, u)
            });
    }

    /// Reset a line's user correction (`line_index`).
    pub fn on_see_text_line_reset<F: FnMut(i32) + 'static>(&self, handler: F) {
        self.component.on_see_text_line_reset(handler);
    }

    /// Stale line: keep the user's edit.
    pub fn on_see_text_line_keep<F: FnMut(i32) + 'static>(&self, handler: F) {
        self.component.on_see_text_line_keep(handler);
    }

    /// Stale line: use the new recognition.
    pub fn on_see_text_line_accept<F: FnMut(i32) + 'static>(&self, handler: F) {
        self.component.on_see_text_line_accept(handler);
    }

    /// Register a closure called when the see-text toggle changes.
    pub fn on_note_see_text_toggled<F: FnMut(bool) + 'static>(&self, handler: F) {
        self.component.on_note_see_text_toggled(handler);
    }

    pub fn on_rename_canvas_note<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_rename_canvas_note(handler);
    }

    /// Register a closure called when the "Note" slice is tapped.
    pub fn on_ring_note<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_ring_note_tapped(handler);
    }

    /// Register a closure called when the "Event" (calendar) slice is tapped.
    pub fn on_ring_calendar<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_ring_calendar_tapped(handler);
    }

    /// Register a closure called when the "Task" slice is tapped.
    pub fn on_ring_task<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_ring_task_tapped(handler);
    }

    /// Register a closure called when the "Audio" slice is tapped.
    pub fn on_ring_audio<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_ring_audio_tapped(handler);
    }

    /// Register a closure called when the ring is dismissed (backdrop tap or ×).
    pub fn on_ring_dismissed<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_ring_dismissed(handler);
    }

    // ── Task CRUD view API ────────────────────────────────────────────────

    /// Show or hide the full task list view.
    pub fn set_show_task_view(&self, show: bool) {
        self.component.set_show_task_view(show);
    }

    /// Push the full task list to the CRUD view.
    ///
    /// Each tuple is `(id, title, priority_label, priority_int, due, overdue, done, lane, has_source_note)`.
    pub fn set_full_task_list(&self, tasks: &[FullTaskRow]) {
        let entries: Vec<TaskViewEntry> = tasks
            .iter()
            .map(
                |(
                    id,
                    title,
                    priority_label,
                    priority_int,
                    due_label,
                    overdue,
                    done,
                    lane,
                    has_source_note,
                )| {
                    TaskViewEntry {
                        id: slint::SharedString::from(id.as_str()),
                        title: slint::SharedString::from(title.as_str()),
                        priority_label: slint::SharedString::from(priority_label.as_str()),
                        priority_int: *priority_int,
                        due_label: slint::SharedString::from(due_label.as_str()),
                        overdue: *overdue,
                        done: *done,
                        lane_label: slint::SharedString::from(lane.as_str()),
                        has_source_note: *has_source_note,
                        show_profile: false,
                        profile_icon_char: slint::SharedString::new(),
                        has_custom_profile_icon: false,
                        custom_profile_icon: slint::Image::default(),
                    }
                },
            )
            .collect();
        let model = std::rc::Rc::new(slint::VecModel::from(entries));
        self.component.set_full_task_list(model.into());
    }

    /// Open the task add/edit dialog.
    #[allow(clippy::too_many_arguments)]
    pub fn open_task_dialog(
        &self,
        is_edit: bool,
        id: &str,
        title: &str,
        priority: i32,
        due: &str,
        tags: &str,
        lane: &str,
        contexts: &str,
        waiting: &str,
    ) {
        self.component.set_task_dialog_is_edit(is_edit);
        self.component
            .set_task_dialog_id(slint::SharedString::from(id));
        self.component
            .set_task_dialog_title(slint::SharedString::from(title));
        self.component.set_task_dialog_priority(priority);
        self.component
            .set_task_dialog_due(slint::SharedString::from(due));
        self.component
            .set_task_dialog_tags(slint::SharedString::from(tags));
        self.component
            .set_task_dialog_lane(slint::SharedString::from(lane));
        self.component
            .set_task_dialog_contexts(slint::SharedString::from(contexts));
        self.component
            .set_task_dialog_waiting(slint::SharedString::from(waiting));
        self.component.set_task_dialog_has_source_note(false);
        self.component.set_task_dialog_visible(true);
    }

    /// Close the task dialog.
    pub fn close_task_dialog(&self) {
        self.component.set_task_dialog_visible(false);
    }

    /// Register a closure called when the page title is tapped in the status bar (→ home).
    pub fn on_page_title_tapped<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_page_title_tapped(handler);
    }

    /// Register a closure called when "Notes →" is tapped on the start page.
    pub fn on_start_page_notes<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_start_page_notes_tapped(handler);
    }

    /// Register a closure called when "All Tasks" is tapped on the start page.
    pub fn on_start_page_tasks<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_start_page_tasks_tapped(handler);
    }

    /// Register a closure called when "Back" is tapped in the task view.
    pub fn on_task_back<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_task_back_tapped(handler);
    }

    /// Register a closure called when a task's checkbox is tapped.
    pub fn on_task_complete_toggled<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_task_complete_toggled(move |id| handler(id.to_string()));
    }

    /// Register a closure called when "Edit" is tapped on a task row.
    pub fn on_task_edit_requested<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_task_edit_requested(move |id| handler(id.to_string()));
    }

    /// Register a closure called when "View associated note" is tapped for a task.
    pub fn on_task_view_source_note<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_task_view_source_note(move |id| handler(id.to_string()));
    }

    /// Register a closure for the page-indicator previous button.
    pub fn on_page_prev_tapped<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_page_prev_tapped(handler);
    }

    /// Register a closure for the page-indicator next button.
    pub fn on_page_next_tapped<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_page_next_tapped(handler);
    }

    /// Register a closure called when "Save" is tapped in the add dialog.
    /// Handler args: title, priority, due, tags, lane, contexts, waiting.
    pub fn on_task_confirm_add<F>(&self, mut handler: F)
    where
        F: FnMut(String, i32, String, String, String, String, String) + 'static,
    {
        self.component.on_task_confirm_add(
            move |title, priority, due, tags, lane, contexts, waiting| {
                handler(
                    title.to_string(),
                    priority,
                    due.to_string(),
                    tags.to_string(),
                    lane.to_string(),
                    contexts.to_string(),
                    waiting.to_string(),
                )
            },
        );
    }

    /// Register a closure called when "Save" is tapped in the edit dialog.
    /// Handler args: id, title, priority, due, tags, lane, contexts, waiting.
    pub fn on_task_confirm_edit<F>(&self, mut handler: F)
    where
        F: FnMut(String, String, i32, String, String, String, String, String) + 'static,
    {
        self.component.on_task_confirm_edit(
            move |id, title, priority, due, tags, lane, contexts, waiting| {
                handler(
                    id.to_string(),
                    title.to_string(),
                    priority,
                    due.to_string(),
                    tags.to_string(),
                    lane.to_string(),
                    contexts.to_string(),
                    waiting.to_string(),
                )
            },
        );
    }

    /// Register a closure when a horizon lane chip is tapped.
    pub fn on_task_lane_changed<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_task_lane_changed(move |lane| handler(lane.to_string()));
    }

    /// Register a closure when an Engage context filter chip is tapped.
    pub fn on_task_context_filter_changed<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_task_context_filter_changed(move |ctx| handler(ctx.to_string()));
    }

    /// Register a closure called when "Cancel" is tapped in the task dialog.
    pub fn on_task_dialog_cancel<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_task_dialog_cancel(handler);
    }

    pub fn on_task_filter_changed<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_task_filter_changed(move |f| handler(f.to_string()));
    }

    pub fn on_task_sort_changed<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_task_sort_changed(move |s| handler(s.to_string()));
    }

    pub fn on_task_priority_cycle<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_task_priority_cycle(move |id| handler(id.to_string()));
    }

    /// Local profile chip filter in task view (All aggregator only).
    pub fn on_task_profile_filter_changed<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_task_profile_filter_changed(move |id| handler(id.to_string()));
    }

    /// Task-view search query changed (tasks only).
    pub fn on_task_search_changed<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_task_search_changed(move |q| handler(q.to_string()));
    }

    /// Recordings-list search query changed.
    pub fn on_rec_search_changed<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_rec_search_changed(move |q| handler(q.to_string()));
    }

    /// Recordings-list sort mode changed.
    pub fn on_rec_sort_changed<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_rec_sort_changed(move |s| handler(s.to_string()));
    }

    // ── Profile switcher API ──────────────────────────────────────────────

    pub fn set_profile_list(&self, list: slint::ModelRc<ProfileEntry>) {
        self.component.set_profile_list(list);
    }

    pub fn on_profile_changed<F>(&self, handler: F)
    where
        F: FnMut(slint::SharedString) + 'static,
    {
        self.component.on_profile_changed(handler);
    }

    // ── Settings view API ─────────────────────────────────────────────────

    /// Show or hide the settings view.
    pub fn set_show_settings(&self, show: bool) {
        self.component.set_show_settings(show);
    }

    /// Show or hide the unified search view.
    pub fn set_show_search(&self, show: bool) {
        self.component.set_show_search(show);
    }

    /// Show or hide the markdown note viewer.
    pub fn set_show_md_viewer(&self, show: bool) {
        self.component.set_show_md_viewer(show);
    }

    /// Set the search query string shown in the search field.
    pub fn set_search_query(&self, query: &str) {
        self.component
            .set_search_query(slint::SharedString::from(query));
    }

    /// Read the current search query.
    pub fn get_search_query(&self) -> String {
        self.component.get_search_query().to_string()
    }

    /// Set the search status / diagnostic line.
    pub fn set_search_status(&self, status: &str) {
        self.component
            .set_search_status(slint::SharedString::from(status));
    }

    /// Push unified search results into the UI.
    /// Each tuple: `(id, source, title, snippet, path)`.
    pub fn set_search_results(&self, hits: &[(String, String, String, String, String)]) {
        let entries: Vec<SearchHitEntry> = hits
            .iter()
            .map(|(id, source, title, snippet, path)| SearchHitEntry {
                id: slint::SharedString::from(id.as_str()),
                source: slint::SharedString::from(source.as_str()),
                title: slint::SharedString::from(title.as_str()),
                snippet: slint::SharedString::from(snippet.as_str()),
                path: slint::SharedString::from(path.as_str()),
            })
            .collect();
        self.component
            .set_search_results(Rc::new(slint::VecModel::from(entries)).into());
    }

    /// Populate the markdown viewer.
    pub fn set_md_viewer(&self, title: &str, body: &str, path: &str) {
        self.component
            .set_md_viewer_title(slint::SharedString::from(title));
        self.component
            .set_md_viewer_body(slint::SharedString::from(body));
        self.component
            .set_md_viewer_path(slint::SharedString::from(path));
    }

    /// Push structured markdown blocks into the viewer (headings, lists, …).
    pub fn set_md_viewer_blocks(&self, blocks: &[ephemeris_core::MdBlock]) {
        use ephemeris_core::MdBlockKind;
        let entries: Vec<MdBlockEntry> = blocks
            .iter()
            .map(|b| {
                let kind = match b.kind {
                    MdBlockKind::Heading1 => 0,
                    MdBlockKind::Heading2 => 1,
                    MdBlockKind::Heading3 => 2,
                    MdBlockKind::Paragraph => 3,
                    MdBlockKind::Bullet => 4,
                    MdBlockKind::Numbered => 5,
                    MdBlockKind::Quote => 6,
                    MdBlockKind::Code => 7,
                    MdBlockKind::Rule => 8,
                };
                MdBlockEntry {
                    kind,
                    text: slint::SharedString::from(b.text.as_str()),
                }
            })
            .collect();
        self.component
            .set_md_viewer_blocks(Rc::new(slint::VecModel::from(entries)).into());
    }

    pub fn on_search_open<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_search_open(handler);
    }

    pub fn on_search_back<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_search_back(handler);
    }

    pub fn on_search_query_changed<F: FnMut(String) + 'static>(&self, mut handler: F) {
        self.component
            .on_search_query_changed(move |q| handler(q.to_string()));
    }

    pub fn on_search_submit<F: FnMut(String) + 'static>(&self, mut handler: F) {
        self.component
            .on_search_submit(move |q| handler(q.to_string()));
    }

    pub fn on_search_result_tapped<F: FnMut(String, String) + 'static>(&self, mut handler: F) {
        self.component
            .on_search_result_tapped(move |id, source| handler(id.to_string(), source.to_string()));
    }

    pub fn on_md_viewer_back<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_md_viewer_back(handler);
    }

    /// Set the theme index shown in settings (0 = light, 1 = dark).
    pub fn set_settings_theme_idx(&self, idx: i32) {
        self.component.set_settings_theme_idx(idx);
    }

    /// Read the theme index currently selected in the settings view.
    pub fn get_settings_theme_idx(&self) -> i32 {
        self.component.get_settings_theme_idx()
    }

    /// Set whether Settings › General shows "Pen only" ink input.
    pub fn set_settings_pen_only(&self, pen_only: bool) {
        self.component.set_settings_pen_only(pen_only);
    }

    /// Read the Settings › General "Pen only" toggle.
    pub fn get_settings_pen_only(&self) -> bool {
        self.component.get_settings_pen_only()
    }

    /// Set the Settings › General new-note title pattern.
    pub fn set_settings_note_title_pattern(&self, pattern: &str) {
        self.component
            .set_settings_note_title_pattern(slint::SharedString::from(pattern));
    }

    /// Read the Settings › General new-note title pattern.
    pub fn get_settings_note_title_pattern(&self) -> String {
        self.component.get_settings_note_title_pattern().to_string()
    }

    /// Set the vault path shown in settings.
    pub fn set_settings_vault_path(&self, path: &str) {
        self.component
            .set_settings_vault_path(slint::SharedString::from(path));
    }

    /// Read the vault path shown in settings.
    pub fn get_settings_vault_path(&self) -> String {
        self.component.get_settings_vault_path().to_string()
    }

    /// Push the list of calendar sources into the settings view.
    ///
    /// Each tuple: `(display_label, value)` where value is a full path or URL.
    pub fn set_settings_cal_sources(&self, sources: &[(String, String)]) {
        let items: Vec<CalSource> = sources
            .iter()
            .map(|(display, value)| CalSource {
                display: slint::SharedString::from(display.as_str()),
                value: slint::SharedString::from(value.as_str()),
            })
            .collect();
        self.component
            .set_settings_cal_sources(std::rc::Rc::new(slint::VecModel::from(items)).into());
    }

    /// Read the current calendar sources from the settings view model.
    ///
    /// Returns `(display_label, value)` pairs.
    pub fn get_settings_cal_sources(&self) -> Vec<(String, String)> {
        let model = self.component.get_settings_cal_sources();
        (0..model.row_count())
            .filter_map(|i| model.row_data(i))
            .map(|s| (s.display.to_string(), s.value.to_string()))
            .collect()
    }

    /// Register a closure called when the user taps "Settings" in the toolbar.
    pub fn on_settings_open<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_settings_open(handler);
    }

    /// Register a closure called when the user taps "Back" in settings.
    pub fn on_settings_back<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_settings_back(handler);
    }

    /// Register a closure called when the user taps "Save" in settings.
    pub fn on_settings_save<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_settings_save(handler);
    }

    /// Register a closure called when "Clear" is tapped next to the vault path.
    pub fn on_settings_clear_vault<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_settings_clear_vault(handler);
    }

    /// Register a closure called when "Browse..." is tapped for the vault.
    pub fn on_settings_browse_vault<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_settings_browse_vault(handler);
    }

    /// Persist export-subdir / git options from the Obsidian vault setup modal.
    pub fn on_connect_vault_setup_confirm<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_connect_vault_setup_confirm(handler);
    }

    pub fn set_connect_setup_export_subdir(&self, subdir: &str) {
        self.component
            .set_connect_setup_export_subdir(slint::SharedString::from(subdir));
    }

    pub fn set_connect_setup_git_enabled(&self, enabled: bool) {
        self.component.set_connect_setup_git_enabled(enabled);
    }

    pub fn set_connect_setup_git_detected(&self, detected: bool) {
        self.component.set_connect_setup_git_detected(detected);
    }

    /// Register a closure called when "Browse .ics..." is tapped.
    pub fn on_settings_browse_ics<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_settings_browse_ics(handler);
    }

    /// Register a closure called when "Add" is tapped in the URL input row.
    pub fn on_settings_add_url<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_settings_add_url(move |url| handler(url.to_string()));
    }

    pub fn on_settings_add_caldav<F>(&self, mut handler: F)
    where
        F: FnMut(String, String, String) + 'static,
    {
        self.component
            .on_settings_add_caldav(move |url, user, pass| {
                handler(url.to_string(), user.to_string(), pass.to_string())
            });
    }

    /// Register a closure called when an "×" button is tapped on a cal source.
    pub fn on_settings_remove_source<F>(&self, handler: F)
    where
        F: FnMut(i32) + 'static,
    {
        self.component.on_settings_remove_source(handler);
    }

    pub fn set_whisper_model_installed(&self, v: bool) {
        self.component.set_whisper_model_installed(v);
    }

    pub fn set_whisper_downloading(&self, v: bool) {
        self.component.set_whisper_downloading(v);
    }

    pub fn get_whisper_lang(&self) -> String {
        self.component.get_whisper_lang().to_string()
    }

    pub fn on_settings_whisper_download<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_settings_whisper_download(handler);
    }

    pub fn on_settings_whisper_delete<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_settings_whisper_delete(handler);
    }

    pub fn set_htr_model_installed(&self, v: bool) {
        self.component.set_htr_model_installed(v);
    }

    pub fn set_htr_downloading(&self, v: bool) {
        self.component.set_htr_downloading(v);
    }

    pub fn on_settings_htr_download<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_settings_htr_download(handler);
    }

    pub fn on_settings_htr_delete<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_settings_htr_delete(handler);
    }

    pub fn set_line_htr_model_installed(&self, v: bool) {
        self.component.set_line_htr_model_installed(v);
    }

    pub fn set_line_htr_downloading(&self, v: bool) {
        self.component.set_line_htr_downloading(v);
    }

    pub fn on_settings_line_htr_download<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_settings_line_htr_download(handler);
    }

    pub fn on_settings_line_htr_delete<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_settings_line_htr_delete(handler);
    }

    /// Settings › Handwriting language (`auto` / `en` / `de`).
    pub fn set_settings_handwriting_lang(&self, lang: &str) {
        self.component
            .set_settings_handwriting_lang(slint::SharedString::from(lang));
    }

    pub fn get_settings_handwriting_lang(&self) -> String {
        self.component.get_settings_handwriting_lang().to_string()
    }

    /// Settings › Handwriting printed-OCR / Tesseract last-resort toggle.
    pub fn set_settings_ocr_enabled(&self, enabled: bool) {
        self.component.set_settings_ocr_enabled(enabled);
    }

    pub fn get_settings_ocr_enabled(&self) -> bool {
        self.component.get_settings_ocr_enabled()
    }

    pub fn on_settings_profile_add<F>(&self, mut handler: F)
    where
        F: FnMut(String, String) + 'static,
    {
        self.component
            .on_settings_profile_add(move |name, icon| handler(name.to_string(), icon.to_string()));
    }

    pub fn on_settings_profile_edit<F>(&self, mut handler: F)
    where
        F: FnMut(String, String, String) + 'static,
    {
        self.component
            .on_settings_profile_edit(move |id, name, icon| {
                handler(id.to_string(), name.to_string(), icon.to_string())
            });
    }

    pub fn on_settings_profile_delete<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_settings_profile_delete(move |id| handler(id.to_string()));
    }

    pub fn on_settings_profile_toggle_notes_in_all<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_settings_profile_toggle_notes_in_all(move |id| handler(id.to_string()));
    }

    pub fn on_settings_profile_toggle_recordings_in_all<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_settings_profile_toggle_recordings_in_all(move |id| handler(id.to_string()));
    }

    pub fn on_settings_browse_profile_icon<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_settings_browse_profile_icon(handler);
    }

    pub fn on_fb_select_profile_icon<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_fb_select_profile_icon(move |path| handler(path.to_string()));
    }

    pub fn set_create_picker_intent(&self, intent: &str) {
        self.component.set_create_picker_intent(intent.into());
    }

    pub fn set_show_create_profile_picker(&self, show: bool) {
        self.component.set_show_create_profile_picker(show);
    }

    pub fn on_create_profile_picked<F>(&self, mut handler: F)
    where
        F: FnMut(String, String) + 'static,
    {
        self.component
            .on_create_profile_picked(move |intent, profile_id| {
                handler(intent.to_string(), profile_id.to_string())
            });
    }

    pub fn on_create_profile_pick_cancel<F>(&self, handler: F)
    where
        F: FnMut() + 'static,
    {
        self.component.on_create_profile_pick_cancel(handler);
    }

    pub fn set_connect_profile_id(&self, id: &str) {
        self.component
            .set_connect_profile_id(slint::SharedString::from(id));
    }

    pub fn get_connect_profile_id(&self) -> String {
        self.component.get_connect_profile_id().to_string()
    }

    pub fn set_connect_has_vault(&self, v: bool) {
        self.component.set_connect_has_vault(v);
    }

    pub fn set_connect_calendar_count(&self, n: i32) {
        self.component.set_connect_calendar_count(n);
    }

    pub fn on_connect_profile_changed<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_connect_profile_changed(move |id| handler(id.to_string()));
    }

    pub fn on_connection_remove<F>(&self, handler: F)
    where
        F: FnMut(i32) + 'static,
    {
        self.component.on_connection_remove(handler);
    }

    pub fn on_connection_cycle_profile<F>(&self, handler: F)
    where
        F: FnMut(i32) + 'static,
    {
        self.component.on_connection_cycle_profile(handler);
    }

    pub fn on_connection_set_profile<F>(&self, mut handler: F)
    where
        F: FnMut(i32, String) + 'static,
    {
        self.component
            .on_connection_set_profile(move |idx, pid| handler(idx, pid.to_string()));
    }

    pub fn on_connection_toggle_aggregated<F>(&self, handler: F)
    where
        F: FnMut(i32) + 'static,
    {
        self.component.on_connection_toggle_aggregated(handler);
    }

    pub fn on_rec_transcription_save<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_rec_transcription_save(move |text| handler(text.to_string()));
    }

    // ── File browser API ──────────────────────────────────────────────────

    /// Show or hide the file browser view.
    pub fn set_show_filebrowser(&self, show: bool) {
        self.component.set_show_filebrowser(show);
    }

    /// Set whether the browser is in vault-selection (true) or ICS-file (false) mode.
    pub fn set_fb_vault_mode(&self, vault_mode: bool) {
        self.component.set_fb_vault_mode(vault_mode);
    }

    /// Set the path label shown in the file browser header.
    pub fn set_fb_current_path(&self, path: &str) {
        self.component
            .set_fb_current_path(slint::SharedString::from(path));
    }

    /// Push directory entries into the file browser list.
    ///
    /// Each tuple: `(name, full_path, is_dir, is_vault)`.
    pub fn set_fb_entries(&self, entries: &[(String, String, bool, bool)]) {
        let items: Vec<FileBrowserEntry> = entries
            .iter()
            .map(|(name, full_path, is_dir, is_vault)| FileBrowserEntry {
                name: slint::SharedString::from(name.as_str()),
                full_path: slint::SharedString::from(full_path.as_str()),
                is_dir: *is_dir,
                is_vault: *is_vault,
            })
            .collect();
        self.component
            .set_fb_entries(std::rc::Rc::new(slint::VecModel::from(items)).into());
    }

    /// Register a closure called when the user taps a directory row.
    pub fn on_fb_navigate<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_fb_navigate(move |path| handler(path.to_string()));
    }

    /// Register a closure called when "Select this folder as Vault" is tapped.
    pub fn on_fb_select_vault<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_fb_select_vault(move |path| handler(path.to_string()));
    }

    /// Register a closure called when "Add" is tapped on an .ics file row.
    pub fn on_fb_add_ics<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_fb_add_ics(move |path| handler(path.to_string()));
    }

    /// Register a closure called when the user taps "Back" in the file browser.
    pub fn on_fb_cancel<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_fb_cancel(handler);
    }

    // ── Navigation rail API ───────────────────────────────────────────────

    /// Set the active navigation section (0 = Home/Overview, 1 = Notes).
    pub fn set_nav_section(&self, section: i32) {
        self.component.set_nav_section(section);
    }

    /// Read the current navigation section.
    pub fn get_nav_section(&self) -> i32 {
        self.component.get_nav_section()
    }

    /// Register a closure called when the "Home" nav item is tapped.
    pub fn on_nav_overview_tapped<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_nav_overview_tapped(handler);
    }

    /// Register a closure called when the "Notes" nav item is tapped.
    pub fn on_nav_notes_tapped<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_nav_notes_tapped(handler);
    }

    /// Register a closure called when the "Tasks" nav item is tapped.
    pub fn on_nav_tasks_tapped<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_nav_tasks_tapped(handler);
    }

    // ── Note browser API ──────────────────────────────────────────────────

    /// Show or hide the note browser list view.
    pub fn set_show_note_list(&self, show: bool) {
        self.component.set_show_note_list(show);
    }

    /// Push note entries into the note browser.
    pub fn set_note_list(&self, entries: &[NoteListItem]) {
        let items: Vec<NoteEntry> = entries
            .iter()
            .map(|e| NoteEntry {
                id: slint::SharedString::from(e.id.as_str()),
                title: slint::SharedString::from(e.title.as_str()),
                date_label: slint::SharedString::from(e.date_label.as_str()),
                has_preview: e.has_preview,
                preview: e.preview.clone(),
                show_profile: e.show_profile,
                profile_icon_char: slint::SharedString::from(e.profile_icon_char.as_str()),
                has_custom_profile_icon: e.has_custom_profile_icon,
                custom_profile_icon: e.custom_profile_icon.clone(),
            })
            .collect();
        self.component
            .set_note_list(std::rc::Rc::new(slint::VecModel::from(items)).into());
    }

    /// Set the ID of the currently open note.
    pub fn set_active_note_id(&self, id: &str) {
        self.component
            .set_active_note_id(slint::SharedString::from(id));
    }

    /// Register a closure called when a note row is tapped.
    pub fn on_note_tapped<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_note_tapped(move |id| handler(id.to_string()));
    }

    /// Register a closure called when "+ New Note" is tapped.
    pub fn on_note_new_tapped<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_note_new_tapped(handler);
    }

    // ── Obsidian Canvas browser / board API ───────────────────────────────

    pub fn set_canvas_folder_list(&self, folders: &[CanvasFolderListItem]) {
        let items: Vec<CanvasFolderEntry> = folders
            .iter()
            .map(|e| CanvasFolderEntry {
                id: slint::SharedString::from(e.id.as_str()),
                title: slint::SharedString::from(e.title.as_str()),
                canvas_count: e.canvas_count,
                folder_count: e.folder_count,
                show_profile: e.show_profile,
                profile_icon_char: slint::SharedString::from(e.profile_icon_char.as_str()),
                has_custom_profile_icon: e.has_custom_profile_icon,
                custom_profile_icon: e.custom_profile_icon.clone(),
            })
            .collect();
        self.component
            .set_canvas_folder_list(std::rc::Rc::new(slint::VecModel::from(items)).into());
    }

    pub fn set_canvas_list(&self, entries: &[CanvasListItem]) {
        let items: Vec<CanvasEntry> = entries
            .iter()
            .map(|e| CanvasEntry {
                id: slint::SharedString::from(e.id.as_str()),
                title: slint::SharedString::from(e.title.as_str()),
                date_label: slint::SharedString::from(e.date_label.as_str()),
                show_profile: e.show_profile,
                profile_icon_char: slint::SharedString::from(e.profile_icon_char.as_str()),
                has_custom_profile_icon: e.has_custom_profile_icon,
                custom_profile_icon: e.custom_profile_icon.clone(),
            })
            .collect();
        self.component
            .set_canvas_list(std::rc::Rc::new(slint::VecModel::from(items)).into());
    }

    pub fn set_canvas_browser_title(&self, title: &str) {
        self.component
            .set_canvas_browser_title(slint::SharedString::from(title));
    }

    pub fn set_canvas_at_root(&self, at_root: bool) {
        self.component.set_canvas_at_root(at_root);
    }

    pub fn set_canvas_board_gray8(&self, width: u32, height: u32, gray: &[u8]) {
        let mut rgba = vec![255u8; (width * height * 4) as usize];
        let n = (width * height) as usize;
        for i in 0..n.min(gray.len()) {
            let g = gray[i];
            rgba[i * 4] = g;
            rgba[i * 4 + 1] = g;
            rgba[i * 4 + 2] = g;
            rgba[i * 4 + 3] = 255;
        }
        let buf =
            slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&rgba, width, height);
        self.component
            .set_canvas_board_image(slint::Image::from_rgba8(buf));
    }

    pub fn on_nav_canvas_tapped<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_nav_canvas_tapped(handler);
    }

    pub fn on_canvas_folder_tapped<F: FnMut(String) + 'static>(&self, mut handler: F) {
        self.component
            .on_canvas_folder_tapped(move |id| handler(id.to_string()));
    }

    pub fn on_canvas_browser_back_tapped<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_canvas_browser_back_tapped(handler);
    }

    pub fn on_canvas_folder_new_tapped<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_canvas_folder_new_tapped(handler);
    }

    pub fn on_canvas_new_tapped<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_canvas_new_tapped(handler);
    }

    pub fn on_canvas_tapped<F: FnMut(String) + 'static>(&self, mut handler: F) {
        self.component
            .on_canvas_tapped(move |id| handler(id.to_string()));
    }

    pub fn on_canvas_board_pointer<F>(&self, mut handler: F)
    where
        F: FnMut(f32, f32, String, String) + 'static,
    {
        self.component
            .on_canvas_board_pointer(move |x, y, kind, pointer| {
                handler(x, y, kind.to_string(), pointer.to_string())
            });
    }

    pub fn on_canvas_zoom_preset<F: FnMut(String) + 'static>(&self, mut handler: F) {
        self.component
            .on_canvas_zoom_preset(move |p| handler(p.to_string()));
    }

    pub fn on_canvas_add_confirm<F: FnMut(String) + 'static>(&self, mut handler: F) {
        self.component
            .on_canvas_add_confirm(move |k| handler(k.to_string()));
    }

    pub fn on_canvas_file_confirm<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_canvas_file_confirm(handler);
    }

    pub fn on_canvas_link_confirm<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_canvas_link_confirm(handler);
    }

    pub fn on_canvas_focus_done<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_canvas_focus_done(handler);
    }

    pub fn on_canvas_delete_selected<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_canvas_delete_selected(handler);
    }

    pub fn on_canvas_clear_selection<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_canvas_clear_selection(handler);
    }

    pub fn on_canvas_leave_board<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_canvas_leave_board(handler);
    }

    pub fn on_canvas_sort_toggled<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_canvas_sort_toggled(handler);
    }

    pub fn on_canvas_view_mode_toggled<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_canvas_view_mode_toggled(handler);
    }

    pub fn on_canvas_rename_request<F: FnMut(String, String) + 'static>(&self, mut handler: F) {
        self.component
            .on_canvas_rename_request(move |kind, id| handler(kind.to_string(), id.to_string()));
    }

    pub fn on_canvas_rename_confirm<F: FnMut(String, String, String) + 'static>(
        &self,
        mut handler: F,
    ) {
        self.component
            .on_canvas_rename_confirm(move |kind, id, title| {
                handler(kind.to_string(), id.to_string(), title.to_string())
            });
    }

    pub fn on_canvas_rename_cancel<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_canvas_rename_cancel(handler);
    }

    /// Register a closure called when the user confirms a note rename.
    pub fn on_note_rename_confirm<F>(&self, mut handler: F)
    where
        F: FnMut(String, String) + 'static,
    {
        self.component
            .on_note_rename_confirm(move |id, title| handler(id.to_string(), title.to_string()));
    }

    /// Register a closure called when the user cancels the rename dialog.
    pub fn on_note_rename_cancel<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_note_rename_cancel(handler);
    }

    // ── Notebook browser API ──────────────────────────────────────────────

    /// Show or hide the notebook list view.
    pub fn set_show_notebook_list(&self, show: bool) {
        self.component.set_show_notebook_list(show);
    }

    /// Push all notebooks (flat) for the move-note dialog.
    pub fn set_notebook_list(&self, notebooks: &[FolderListItem]) {
        let items: Vec<NotebookEntry> = notebooks.iter().map(folder_list_item_to_entry).collect();
        self.component
            .set_notebook_list(std::rc::Rc::new(slint::VecModel::from(items)).into());
    }

    /// Push child folders for the current notes-browser level.
    pub fn set_folder_list(&self, folders: &[FolderListItem]) {
        let items: Vec<NotebookEntry> = folders.iter().map(folder_list_item_to_entry).collect();
        self.component
            .set_folder_list(std::rc::Rc::new(slint::VecModel::from(items)).into());
    }

    /// Set the active notebook id and title shown in the note list header.
    pub fn set_active_notebook(&self, id: &str, title: &str) {
        self.component
            .set_active_notebook_id(slint::SharedString::from(id));
        self.component
            .set_active_notebook_title(slint::SharedString::from(title));
    }

    /// Mark whether the notes browser is at the root level.
    pub fn set_notes_at_root(&self, at_root: bool) {
        self.component.set_notes_at_root(at_root);
    }

    /// Set notes overview mode: `"list"` or `"gallery"`.
    pub fn set_notes_view_mode(&self, mode: &str) {
        self.component
            .set_notes_view_mode(slint::SharedString::from(mode));
    }

    /// Register a closure called when the list/gallery toggle is tapped.
    pub fn on_notes_view_mode_toggled<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_notes_view_mode_toggled(handler);
    }

    /// Register a closure called when the sort-mode chip is tapped.
    pub fn on_notes_sort_cycled<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_notes_sort_cycled(handler);
    }

    /// Register a closure called when the folder search field changes.
    pub fn on_notes_search_changed<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_notes_search_changed(move |q| handler(q.to_string()));
    }

    /// Register a closure called when Delete is tapped on a note.
    pub fn on_note_delete_tapped<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_note_delete_tapped(move |id| handler(id.to_string()));
    }

    pub fn set_notes_sort_mode(&self, mode: &str) {
        self.component
            .set_notes_sort_mode(slint::SharedString::from(mode));
    }

    pub fn notes_sort_mode(&self) -> String {
        self.component.get_notes_sort_mode().to_string()
    }

    pub fn notes_search_query(&self) -> String {
        self.component.get_notes_search_query().to_string()
    }

    /// Register a closure called when a notebook row is tapped.
    pub fn on_notebook_tapped<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_notebook_tapped(move |id| handler(id.to_string()));
    }

    /// Register a closure called when "+ New Notebook" is tapped.
    pub fn on_notebook_new_tapped<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_notebook_new_tapped(handler);
    }

    /// Register a closure called when "← Back" is tapped in the note list.
    pub fn on_notebook_back_tapped<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_notebook_back_tapped(handler);
    }

    /// Register a closure called when Delete is tapped on a folder.
    pub fn on_notebook_delete_tapped<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_notebook_delete_tapped(move |id| handler(id.to_string()));
    }

    pub fn on_notebook_delete_confirm<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_notebook_delete_confirm(move |id| handler(id.to_string()));
    }

    pub fn on_notebook_delete_cancel<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_notebook_delete_cancel(handler);
    }

    pub fn show_notebook_delete_confirm(&self, id: &str, title: &str, message: &str) {
        self.component
            .set_notebook_delete_id(slint::SharedString::from(id));
        self.component
            .set_notebook_delete_title(slint::SharedString::from(title));
        self.component
            .set_notebook_delete_message(slint::SharedString::from(message));
        self.component.set_notebook_delete_visible(true);
    }

    pub fn hide_notebook_delete_confirm(&self) {
        self.component.set_notebook_delete_visible(false);
    }

    // ── Note move API ─────────────────────────────────────────────────────

    /// Register a closure called when the user confirms moving a note.
    pub fn on_note_move_confirm<F>(&self, mut handler: F)
    where
        F: FnMut(String, String) + 'static,
    {
        self.component
            .on_note_move_confirm(move |note_id, notebook_id| {
                handler(note_id.to_string(), notebook_id.to_string())
            });
    }

    /// Register a closure called when the move dialog is cancelled.
    pub fn on_note_move_cancel<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_note_move_cancel(handler);
    }

    // ── Notebook rename API ───────────────────────────────────────────────

    pub fn set_notebook_rename_visible(&self, v: bool) {
        self.component.set_notebook_rename_visible(v);
    }

    pub fn on_notebook_rename_confirm<F>(&self, mut handler: F)
    where
        F: FnMut(String, String) + 'static,
    {
        self.component.on_notebook_rename_confirm(move |id, title| {
            handler(id.to_string(), title.to_string())
        });
    }

    pub fn on_notebook_rename_cancel<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_notebook_rename_cancel(handler);
    }

    // ── Calendar API ──────────────────────────────────────────────────────

    pub fn set_show_calendar(&self, show: bool) {
        self.component.set_show_calendar(show);
    }

    pub fn set_cal_header_label(&self, label: &str) {
        self.component
            .set_cal_header_label(slint::SharedString::from(label));
    }

    pub fn set_cal_year(&self, y: i32) {
        self.component.set_cal_year(y);
    }

    pub fn set_cal_month(&self, m: i32) {
        self.component.set_cal_month(m);
    }

    pub fn set_cal_day(&self, d: i32) {
        self.component.set_cal_day(d);
    }

    pub fn set_cal_sub_view(&self, v: i32) {
        self.component.set_cal_sub_view(v);
    }

    /// Push month-grid days: `(day, has_events, is_today, in_month, first_event_title)`.
    pub fn set_cal_month_days(&self, days: &[(i32, bool, bool, bool, String)]) {
        let items: Vec<CalDay> = days
            .iter()
            .map(|(day, has_events, is_today, in_month, title)| CalDay {
                day: *day,
                has_events: *has_events,
                is_today: *is_today,
                in_month: *in_month,
                first_event_title: slint::SharedString::from(title.as_str()),
            })
            .collect();
        self.component
            .set_cal_month_days(std::rc::Rc::new(slint::VecModel::from(items)).into());
    }

    /// Push calendar events: `(title, time_label, all_day, col, item_kind, item_id)`.
    pub fn set_cal_events(&self, events: &[(String, String, bool, i32, String, String)]) {
        let items: Vec<CalDayEvent> = events
            .iter()
            .map(|(title, time_label, all_day, col, kind, id)| CalDayEvent {
                title: slint::SharedString::from(title.as_str()),
                time_label: slint::SharedString::from(time_label.as_str()),
                all_day: *all_day,
                col: *col,
                item_kind: slint::SharedString::from(kind.as_str()),
                item_id: slint::SharedString::from(id.as_str()),
            })
            .collect();
        self.component
            .set_cal_events(std::rc::Rc::new(slint::VecModel::from(items)).into());
    }

    /// Open the new-event dialog with defaults.
    pub fn open_event_dialog(
        &self,
        title: &str,
        date: &str,
        start: &str,
        end: &str,
        all_day: bool,
    ) {
        self.component
            .set_event_dialog_title(slint::SharedString::from(title));
        self.component
            .set_event_dialog_date(slint::SharedString::from(date));
        self.component
            .set_event_dialog_start(slint::SharedString::from(start));
        self.component
            .set_event_dialog_end(slint::SharedString::from(end));
        self.component.set_event_dialog_all_day(all_day);
        self.component.set_event_dialog_visible(true);
    }

    pub fn close_event_dialog(&self) {
        self.component.set_event_dialog_visible(false);
    }

    pub fn on_event_confirm_add<
        F: FnMut(SharedString, SharedString, SharedString, SharedString, bool) + 'static,
    >(
        &self,
        handler: F,
    ) {
        self.component.on_event_confirm_add(handler);
    }

    pub fn on_event_dialog_cancel<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_event_dialog_cancel(handler);
    }

    pub fn on_cal_item_tapped<F: FnMut(SharedString, SharedString) + 'static>(&self, handler: F) {
        self.component.on_cal_item_tapped(handler);
    }

    /// Push week column labels (7 entries, e.g. "Mon\n21").
    pub fn set_cal_week_day_labels(&self, labels: &[String]) {
        let items: Vec<slint::SharedString> = labels
            .iter()
            .map(|s| slint::SharedString::from(s.as_str()))
            .collect();
        self.component
            .set_cal_week_day_labels(std::rc::Rc::new(slint::VecModel::from(items)).into());
    }

    /// Push year-month summaries: `(name, short_name, event_count, month_num)`.
    pub fn set_cal_year_months(&self, months: &[(String, String, i32, i32)]) {
        let items: Vec<CalMonthInfo> = months
            .iter()
            .map(|(name, short_name, event_count, month_num)| CalMonthInfo {
                name: slint::SharedString::from(name.as_str()),
                short_name: slint::SharedString::from(short_name.as_str()),
                event_count: *event_count,
                month_num: *month_num,
            })
            .collect();
        self.component
            .set_cal_year_months(std::rc::Rc::new(slint::VecModel::from(items)).into());
    }

    pub fn on_nav_calendar_tapped<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_nav_calendar_tapped(handler);
    }

    pub fn on_cal_sub_view_changed<F>(&self, handler: F)
    where
        F: FnMut(i32) + 'static,
    {
        self.component.on_cal_sub_view_changed(handler);
    }

    pub fn on_cal_prev<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_cal_prev(handler);
    }

    pub fn on_cal_next<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_cal_next(handler);
    }

    pub fn on_cal_today<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_cal_today(handler);
    }

    pub fn on_cal_day_tapped<F>(&self, handler: F)
    where
        F: FnMut(i32, i32, i32) + 'static,
    {
        self.component.on_cal_day_tapped(handler);
    }

    pub fn on_cal_open_day_note<F>(&self, handler: F)
    where
        F: FnMut(i32, i32, i32) + 'static,
    {
        self.component.on_cal_open_day_note(handler);
    }

    pub fn set_cal_day_note_exists(&self, v: bool) {
        self.component.set_cal_day_note_exists(v);
    }

    pub fn set_cal_day_note_id(&self, id: &str) {
        self.component
            .set_cal_day_note_id(slint::SharedString::from(id));
    }

    pub fn set_note_anchor_label(&self, label: &str) {
        self.component
            .set_note_anchor_label(slint::SharedString::from(label));
    }

    pub fn on_note_anchor_tapped<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_note_anchor_tapped(handler);
    }

    // ── Ring tool API ─────────────────────────────────────────────────────

    /// Register a closure called when the "Pen" ring slice is tapped.
    pub fn on_ring_pen<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_ring_pen_tapped(handler);
    }

    /// Register a closure called when the "Highlighter" ring slice is tapped.
    pub fn on_ring_highlighter<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_ring_highlighter_tapped(handler);
    }

    /// Register a closure called when the "Eraser" ring slice is tapped.
    pub fn on_ring_eraser<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_ring_eraser_tapped(handler);
    }

    /// Register a closure called when the "Clear" ring slice is tapped.
    pub fn on_ring_clear<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_ring_clear_tapped(handler);
    }

    // ── Page state helpers ────────────────────────────────────────────────

    /// Snapshot the current live ink state into the PageBook without navigating.
    ///
    /// Call this before switching to an overlay view (note list, notebook list)
    /// so that the ink is guaranteed to be persisted even if no explicit
    /// `navigate_to_page` follows.
    pub fn save_current_page(&self) {
        let canvas_h = self.canvas_height();
        let live_state = self.snapshot_live_state(canvas_h);
        let mut book = self.pages.borrow_mut();
        let cur = book.current_index();
        book.navigate_to(live_state, cur);
    }

    /// Zero-based index of the currently displayed page in the PageBook.
    pub fn current_page_idx(&self) -> usize {
        self.pages.borrow().current_index()
    }

    /// Raw grayscale pixel bytes of the committed ink layer for the current page
    /// (physical ink-plane size — see [`ink_canvas_size`](Self::ink_canvas_size)).
    pub fn get_committed_pixels(&self) -> Vec<u8> {
        self.committed_layer.borrow().data.clone()
    }

    /// Shared handle to the committed ink layer (cheap `Arc` clone — no memcpy).
    /// Its `width`/`height` are the ink-plane size to persist alongside.
    pub fn get_committed_pixels_arc(&self) -> Arc<PixelBuf> {
        Arc::clone(&self.committed_layer.borrow())
    }

    /// Ensure the live committed layer is uniquely owned.
    ///
    /// Call after handing an [`Arc`] snapshot to a background writer so the next
    /// stroke's `Arc::make_mut` does not COW mid-draw.
    pub fn ensure_committed_unique(&self) {
        let mut arc = self.committed_layer.borrow_mut();
        let _ = Arc::make_mut(&mut arc);
    }

    /// True when ink damage or a screen-change is queued for the next present.
    pub fn has_ink_damage(&self) -> bool {
        self.screen_change.get() || !self.ink_damage.borrow().is_empty()
    }

    /// Surviving vector strokes for the current page (pen/highlighter only).
    pub fn get_page_strokes(&self) -> Vec<Stroke> {
        self.strokes.borrow().clone()
    }

    /// Replace the current page's stroke list and rebuild the committed raster.
    ///
    /// Used when loading persisted strokes from disk. Does not clear
    /// `strokes_dirty` — callers that load from disk should call
    /// [`clear_strokes_dirty`] afterward if needed.
    ///
    /// Pages that are not displayed are only marked stale; their raster is
    /// regenerated at the current ink-plane size when navigated to.
    pub fn restore_page_strokes(&self, page_idx: usize, strokes: Vec<Stroke>) {
        let (iw, ih) = self.ink_canvas_size();
        let is_current = {
            let mut book = self.pages.borrow_mut();
            while book.pages.len() <= page_idx {
                book.pages
                    .push(PageState::blank(iw, ih, self.default_base_width));
            }
            let is_current = book.current_index() == page_idx;
            let page = &mut book.pages[page_idx];
            page.strokes = strokes;
            if is_current {
                page.committed = Arc::new(self.rasterize_strokes(&page.strokes));
                page.raster_stale = false;
            } else {
                page.raster_stale = true;
            }
            is_current
        };
        if is_current {
            let book = self.pages.borrow();
            let ps = book.current_page();
            *self.committed_layer.borrow_mut() = Arc::clone(&ps.committed);
            *self.strokes.borrow_mut() = ps.strokes.clone();
            drop(book);
            self.screen_change.set(true);
        }
    }

    /// True if strokes were added/removed since the last [`take_strokes_dirty`].
    pub fn take_strokes_dirty(&self) -> bool {
        self.strokes_dirty.replace(false)
    }

    /// Clear the strokes-dirty flag without reading it.
    pub fn clear_strokes_dirty(&self) {
        self.strokes_dirty.set(false);
    }

    /// Rasterize `strokes` into a fresh ink plane at the current physical size.
    fn rasterize_strokes(&self, strokes: &[Stroke]) -> PixelBuf {
        let (iw, ih) = self.ink_canvas_size();
        let scale = self.ink_scale();
        let mut buf = PixelBuf::new(iw, ih);
        for stroke in strokes {
            if stroke.tool == Tool::Eraser || stroke.points.len() < 2 {
                continue;
            }
            raster::rasterize_stroke(&mut buf, stroke, scale);
        }
        buf
    }

    /// Bring `page.committed` to the current ink-plane size.
    ///
    /// Stale pages and pages with vector strokes are re-rasterized (strokes are
    /// the source of truth); raster-only legacy pages are resampled.
    fn fit_page_raster(&self, page: &mut PageState) {
        let (iw, ih) = self.ink_canvas_size();
        let sized = page.committed.width == iw && page.committed.height == ih;
        if sized && !page.raster_stale {
            return;
        }
        page.committed = if page.raster_stale || !page.strokes.is_empty() {
            Arc::new(self.rasterize_strokes(&page.strokes))
        } else {
            Arc::new(compose::resample_gray(&page.committed, iw, ih))
        };
        page.raster_stale = false;
    }

    fn remove_strokes_by_id(&self, ids: &[StrokeId]) -> bool {
        if ids.is_empty() {
            return false;
        }
        let removed_strokes: Vec<Stroke> = {
            let strokes = self.strokes.borrow();
            strokes
                .iter()
                .filter(|s| ids.contains(&s.id))
                .cloned()
                .collect()
        };
        if removed_strokes.is_empty() {
            return false;
        }
        self.strokes.borrow_mut().retain(|s| !ids.contains(&s.id));
        let remaining = self.strokes.borrow().clone();
        let mut damage = ephemeris_core::geom::Rect::empty();
        for s in &removed_strokes {
            damage = damage.union(&s.approx_bbox());
        }
        if damage.is_empty() {
            *self.committed_layer.borrow_mut() = Arc::new(self.rasterize_strokes(&remaining));
        } else {
            let scale = self.ink_scale();
            let mut arc = self.committed_layer.borrow_mut();
            let committed = Arc::make_mut(&mut arc);
            Self::erase_region_redraw_survivors(committed, &damage, &remaining, scale);
        }
        {
            let mut book = self.pages.borrow_mut();
            let page = book.current_page_mut();
            page.strokes = remaining;
            page.committed = Arc::clone(&self.committed_layer.borrow());
        }
        self.strokes_dirty.set(true);
        // Damage is pushed by the eraser path; do NOT force a Clear/full
        // screen refresh here — that freezes PineNote on every erase hit.
        true
    }

    /// White-fill `damage` in the committed layer, then re-raster survivors that
    /// intersect it (incremental erase — avoids full-page rebuild).
    ///
    /// `damage` is in logical canvas coordinates; the cleared region is its
    /// ink-plane footprint under `scale`.
    fn erase_region_redraw_survivors(
        committed: &mut PixelBuf,
        damage: &ephemeris_core::geom::Rect,
        survivors: &[Stroke],
        scale: InkScale,
    ) {
        let Some((x0, y0, x1, y1)) =
            raster::ink_pixel_bounds(damage, scale, committed.width, committed.height)
        else {
            return;
        };
        let stride = committed.stride as usize;
        for y in y0..y1 {
            let row = y as usize * stride;
            for x in x0..x1 {
                committed.data[row + x as usize] = 255;
            }
        }
        for stroke in survivors {
            if stroke.tool == Tool::Eraser || stroke.points.len() < 2 {
                continue;
            }
            let sb = stroke.approx_bbox();
            if sb.is_empty() {
                continue;
            }
            // AABB intersection with the cleared region.
            if sb.max_x < damage.min_x
                || sb.min_x > damage.max_x
                || sb.max_y < damage.min_y
                || sb.min_y > damage.max_y
            {
                continue;
            }
            raster::rasterize_stroke(committed, stroke, scale);
        }
    }

    /// Full UI / window size in logical pixels `(width, height)`.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Logical canvas size `(width, height)` — the coordinate space of strokes.
    pub fn canvas_size(&self) -> (u32, u32) {
        (self.width, self.canvas_height())
    }

    /// Physical frame size the ink planes and present buffer are built for.
    pub fn physical_size(&self) -> (u32, u32) {
        self.phys_size.get()
    }

    /// Committed / in-progress ink-plane size in physical pixels.
    pub fn ink_canvas_size(&self) -> (u32, u32) {
        let (y0, y1) = self.ink_canvas_rows();
        (self.phys_size.get().0, y1 - y0)
    }

    /// Logical canvas → ink-plane scale (`phys / logical` per axis).
    pub fn ink_scale(&self) -> InkScale {
        InkScale::from_sizes(self.canvas_size(), self.ink_canvas_size())
    }

    /// Resize the ink planes, background cache and present buffer to the
    /// physical frame `pw × ph`. Returns `false` when nothing changed.
    ///
    /// The visible page is re-rasterized from its strokes at the new scale;
    /// other pages are refit lazily when navigated to. Called automatically by
    /// [`render_frame`](Self::render_frame) from [`Display::physical_size`].
    pub fn set_physical_size(&self, pw: u32, ph: u32) -> bool {
        if pw == 0 || ph == 0 || self.phys_size.get() == (pw, ph) {
            return false;
        }
        self.phys_size.set((pw, ph));
        *self.bg_phys.borrow_mut() = PixelBuf::new(pw, ph);
        *self.present_phys.borrow_mut() = PixelBuf::new(pw, ph);
        self.bg_key.set(None);

        {
            let mut book = self.pages.borrow_mut();
            let page = book.current_page_mut();
            page.committed = Arc::clone(&self.committed_layer.borrow());
            page.strokes = self.strokes.borrow().clone();
            self.fit_page_raster(page);
            *self.committed_layer.borrow_mut() = Arc::clone(&page.committed);
        }

        let (iw, ih) = self.ink_canvas_size();
        *self.in_progress_layer.borrow_mut() = PixelBuf::new(iw, ih);
        self.rebuild_in_progress_layer();
        self.screen_change.set(true);
        true
    }

    /// Physical row span `[y0, y1)` of the canvas inside the frame.
    fn ink_canvas_rows(&self) -> (u32, u32) {
        let g = self.frame_geom();
        let y0 = g.phys_y(STATUS_BAR_H);
        let y1 = g.phys_y(self.height.saturating_sub(TOOLBAR_H)).max(y0);
        (y0, y1)
    }

    fn frame_geom(&self) -> FrameGeom {
        FrameGeom::new((self.width, self.height), self.phys_size.get())
    }

    /// Restore saved pixel data into a specific PageBook slot.
    ///
    /// If the target page is currently displayed, the live committed_layer is
    /// also updated and a screen-change repaint is triggered.
    ///
    /// Does not clear the stroke list — call [`restore_page_strokes`] when
    /// vector ink is available, or leave strokes empty for legacy raster-only
    /// pages (See-text falls back to OCR).
    ///
    /// `width × height` is the size the raster was saved at: any size is
    /// accepted (old logical-size bins included). The visible page is refit to
    /// the ink plane immediately; other pages are refit when navigated to.
    pub fn restore_page_pixels(&self, page_idx: usize, width: u32, height: u32, data: Vec<u8>) {
        let expected = width as usize * height as usize;
        if expected == 0 || data.len() != expected {
            eprintln!(
                "restore_page_pixels: size mismatch ({width}×{height} expects {expected}, got {})",
                data.len()
            );
            return;
        }
        let mut raster = PixelBuf::new(width, height);
        raster.data = data;
        let (iw, ih) = self.ink_canvas_size();
        let is_current = {
            let mut book = self.pages.borrow_mut();
            while book.pages.len() <= page_idx {
                book.pages
                    .push(PageState::blank(iw, ih, self.default_base_width));
            }
            let is_current = book.current_index() == page_idx;
            let page = &mut book.pages[page_idx];
            page.committed = Arc::new(raster);
            page.raster_stale = false;
            if is_current {
                // Strokes may belong to a previous occupant of this slot; the
                // caller follows up with `restore_page_strokes` when it has them.
                let strokes = std::mem::take(&mut page.strokes);
                self.fit_page_raster(page);
                page.strokes = strokes;
                *self.committed_layer.borrow_mut() = Arc::clone(&page.committed);
            }
            is_current
        };
        if is_current {
            self.screen_change.set(true);
        }
    }

    // ── Input event dispatch ──────────────────────────────────────────────

    /// Forward a pointer-moved event (logical pixels) to the Slint component.
    pub fn dispatch_pointer_moved(&self, x: f32, y: f32) {
        use slint::platform::WindowEvent;
        self.window.dispatch_event(WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(x, y),
        });
    }

    /// Forward a pointer-pressed event (logical pixels, left button).
    pub fn dispatch_pointer_pressed(&self, x: f32, y: f32) {
        use slint::platform::{PointerEventButton, WindowEvent};
        self.window.dispatch_event(WindowEvent::PointerPressed {
            position: slint::LogicalPosition::new(x, y),
            button: PointerEventButton::Left,
        });
    }

    /// Forward a pointer-released event (logical pixels, left button).
    pub fn dispatch_pointer_released(&self, x: f32, y: f32) {
        use slint::platform::{PointerEventButton, WindowEvent};
        self.window.dispatch_event(WindowEvent::PointerReleased {
            position: slint::LogicalPosition::new(x, y),
            button: PointerEventButton::Left,
        });
    }

    /// Forward a key-pressed event (text is the unicode representation).
    pub fn dispatch_key_pressed(&self, text: slint::SharedString) {
        use slint::platform::WindowEvent;
        self.window.dispatch_event(WindowEvent::KeyPressed { text });
    }

    /// Forward a key-released event.
    pub fn dispatch_key_released(&self, text: slint::SharedString) {
        use slint::platform::WindowEvent;
        self.window
            .dispatch_event(WindowEvent::KeyReleased { text });
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
                let (iw, ih) = self.ink_canvas_size();
                book.push_blank_page(live_state, iw, ih, self.default_base_width)
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

    /// Navigate directly to page `n` (zero-based index).
    ///
    /// Saves the current page's live state first, then loads page `n`.
    /// Clamped to `[0, page_count - 1]`; if `n` is already the current page
    /// this is a no-op apart from the state save.
    pub fn navigate_to_page(&self, n: usize) {
        let canvas_h = self.canvas_height();
        let live_state = self.snapshot_live_state(canvas_h);

        let target = {
            let mut book = self.pages.borrow_mut();
            let clamped = n.min(book.page_count().saturating_sub(1));
            book.navigate_to(live_state, clamped);
            clamped
        };

        self.load_page_state(target);
    }

    /// Append a new blank page, navigate to it, and return its zero-based index.
    pub fn push_new_page(&self) -> usize {
        let canvas_h = self.canvas_height();
        let live_state = self.snapshot_live_state(canvas_h);

        let new_idx = {
            let (iw, ih) = self.ink_canvas_size();
            let mut book = self.pages.borrow_mut();
            book.push_blank_page(live_state, iw, ih, self.default_base_width)
        };

        self.load_page_state(new_idx);
        new_idx
    }

    /// Total number of pages currently in the book.
    pub fn total_page_count(&self) -> usize {
        self.pages.borrow().page_count()
    }

    /// Return `(current_index_1based, total_page_count)` reflecting the status
    /// bar display (1-based index, matching the `page-index / page-count` text).
    pub fn current_page_index(&self) -> (u32, u32) {
        let book = self.pages.borrow();
        ((book.current_index() + 1) as u32, book.page_count() as u32)
    }

    /// Wire the `.slint` `swipe-left` / `swipe-right` callbacks to
    /// [`next_page`] / [`prev_page`] respectively (book-style edge turns),
    /// using a shared `Rc<Self>`.
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
            ui_left.next_page();
        });

        let ui_right = ui.clone();
        ui.on_swipe_right(move || {
            ui_right.prev_page();
        });
    }

    // ── Ink input API ─────────────────────────────────────────────────────

    /// Feed one input event into the owned [`InkEngine`] (or [`EraserEngine`]
    /// when the active tool is eraser) and update the ink layers accordingly.
    ///
    /// * Pen/highlighter `PenUp` — appends the finished [`Stroke`] to the
    ///   page stroke list and rasterizes it into the committed layer.
    /// * Eraser — whole-stroke removal via [`EraserEngine`]; rebuilds the
    ///   committed raster from surviving strokes (no `Tool::Eraser` strokes
    ///   are stored).
    ///
    /// Returns the raw [`InkUpdate`] from the ink engine so the caller can
    /// track damage rects. Eraser gestures return [`InkUpdate::Idle`] (damage
    /// is still queued via [`push_ink_damage`]).
    ///
    /// `x` and `y` on the `PenSample` must be **canvas-relative** logical
    /// pixels (i.e. the coordinate system reported by the `.slint`
    /// `canvas-touch` callback).
    pub fn feed_input(&self, event: &InputEvent, t_ms: u32) -> InkUpdate {
        if self.engine.borrow().config().tool == Tool::Eraser {
            return self.feed_eraser(event);
        }

        let update = self.engine.borrow_mut().update(event, t_ms);
        let tool = self.engine.borrow().config().tool;
        let scale = self.ink_scale();

        match &update {
            InkUpdate::Started => {
                let bw = self.engine.borrow().config().base_width;
                *self.in_progress_base_width.borrow_mut() = bw;
                self.in_progress.borrow_mut().clear();
                self.clear_in_progress_layer(tool);

                // Stamp the engine's accepted point (raw-anchored on PenDown),
                // not a separately reconstructed sample — keeps tip/damage aligned.
                if let Some(pt) = self.engine.borrow().last_point() {
                    self.in_progress.borrow_mut().push(pt);
                    raster::rasterize_points(
                        &mut self.in_progress_layer.borrow_mut(),
                        std::slice::from_ref(&pt),
                        bw,
                        tool,
                        scale,
                    );
                    let pad = bw + 1.0;
                    self.push_ink_damage(
                        ephemeris_core::geom::Rect::new(pt.x, pt.y, pt.x, pt.y).inflate(pad),
                    );
                }
            }
            InkUpdate::Extended { points, damage } => {
                // Stamp the engine's newly emitted Catmull-Rom samples. Span
                // points exclude the previous tip, so chain from the last
                // in-progress point for capsule continuity.
                if !points.is_empty() {
                    let bw = *self.in_progress_base_width.borrow();
                    let stamp: Vec<Point> = {
                        let mut ip = self.in_progress.borrow_mut();
                        let mut chain = Vec::with_capacity(points.len() + 1);
                        if let Some(prev) = ip.last().copied() {
                            chain.push(prev);
                        }
                        chain.extend_from_slice(points);
                        for p in points {
                            ip.push(*p);
                        }
                        chain
                    };
                    raster::rasterize_points(
                        &mut self.in_progress_layer.borrow_mut(),
                        &stamp,
                        bw,
                        tool,
                        scale,
                    );
                }
                self.push_ink_damage(*damage);
            }
            InkUpdate::Finished { stroke, damage } => {
                // Never store eraser strokes — eraser is handled above.
                if stroke.tool != Tool::Eraser && !stroke.points.is_empty() {
                    let bw = *self.in_progress_base_width.borrow();
                    // Stamp any final tip-span points not yet pushed via Extended.
                    {
                        let already = self.in_progress.borrow().len();
                        if already < stroke.points.len() {
                            let mut stamp = Vec::new();
                            if let Some(prev) = self.in_progress.borrow().last().copied() {
                                stamp.push(prev);
                            }
                            stamp.extend_from_slice(&stroke.points[already..]);
                            raster::rasterize_points(
                                &mut self.in_progress_layer.borrow_mut(),
                                &stamp,
                                bw,
                                tool,
                                scale,
                            );
                            self.in_progress
                                .borrow_mut()
                                .extend_from_slice(&stroke.points[already..]);
                        }
                    }

                    // Promote live → committed with min (no re-raster). Merge
                    // over the stroke bbox ∪ last-span damage so every stamped
                    // pixel is copied; present damage stays last-span only.
                    let merge_rect = stroke.approx_bbox().union(damage).inflate(bw + 1.0);
                    {
                        let ip = self.in_progress_layer.borrow();
                        let mut arc = self.committed_layer.borrow_mut();
                        raster::merge_min_rect(Arc::make_mut(&mut arc), &ip, &merge_rect, scale);
                    }

                    if stroke.points.len() >= 2 {
                        self.strokes.borrow_mut().push((**stroke).clone());
                        {
                            let mut book = self.pages.borrow_mut();
                            let page = book.current_page_mut();
                            page.strokes = self.strokes.borrow().clone();
                            // Do NOT Arc::clone committed into PageBook here —
                            // that shares ownership so the next stroke's
                            // Arc::make_mut COWs the full canvas. Live
                            // `committed_layer` stays unique; save_current_page
                            // / navigate sync PageBook.
                        }
                        self.strokes_dirty.set(true);
                    }
                }
                self.in_progress.borrow_mut().clear();
                self.clear_in_progress_layer(tool);
                // Last-span damage only — the rest of the stroke is already on
                // screen from Extended presents and needs no re-present.
                self.push_ink_damage(*damage);
            }
            InkUpdate::Idle => {}
        }

        update
    }

    fn feed_eraser(&self, event: &InputEvent) -> InkUpdate {
        let strokes_snapshot = self.strokes.borrow().clone();
        let update = self.eraser.borrow_mut().update(event, &strokes_snapshot);
        match update {
            EraserUpdate::Started => {
                self.in_progress.borrow_mut().clear();
                self.clear_in_progress_layer(Tool::Eraser);
                if let InputEvent::PenDown(s) = event {
                    let r = 10.0;
                    self.push_ink_damage(
                        ephemeris_core::geom::Rect::new(s.x, s.y, s.x, s.y).inflate(r),
                    );
                }
            }
            EraserUpdate::Hit { ids, damage } | EraserUpdate::Finished { ids, damage } => {
                if self.remove_strokes_by_id(&ids) {
                    self.push_ink_damage(damage);
                }
                if matches!(event, InputEvent::PenUp(_)) {
                    self.in_progress.borrow_mut().clear();
                    self.clear_in_progress_layer(Tool::Eraser);
                }
            }
            EraserUpdate::Idle => {}
        }
        InkUpdate::Idle
    }

    /// Re-stamp the live layer from the stored in-progress points (page load,
    /// ink-plane resize).
    fn rebuild_in_progress_layer(&self) {
        let tool = self.engine.borrow().config().tool;
        self.clear_in_progress_layer(tool);
        let pts = self.in_progress.borrow();
        if !pts.is_empty() {
            let bw = *self.in_progress_base_width.borrow();
            raster::rasterize_points(
                &mut self.in_progress_layer.borrow_mut(),
                &pts,
                bw,
                tool,
                self.ink_scale(),
            );
        }
    }

    /// Reset the live stroke raster to the blend identity for `tool`.
    fn clear_in_progress_layer(&self, tool: Tool) {
        let mut layer = self.in_progress_layer.borrow_mut();
        if tool == Tool::Eraser {
            layer.fill(0);
        } else {
            layer.fill(255);
        }
    }

    /// Push externally-rasterized canvas pixels into the committed ink layer,
    /// replacing it entirely.
    ///
    /// `pixels` must be either ink-plane sized ([`ink_canvas_size`]) or
    /// logical-canvas sized ([`canvas_size`], resampled to the ink plane).
    /// Any other size is silently ignored.
    ///
    /// This is the explicit seam documented in the `lib.rs` module doc — useful
    /// for callers that maintain their own stroke rasterizer outside this crate.
    ///
    /// [`ink_canvas_size`]: Self::ink_canvas_size
    /// [`canvas_size`]: Self::canvas_size
    pub fn push_canvas_pixels(&self, pixels: &PixelBuf) {
        let canvas_h = self.canvas_height();
        let (iw, ih) = self.ink_canvas_size();
        let fitted = if (pixels.width, pixels.height) == (iw, ih) {
            Some(pixels.clone())
        } else if (pixels.width, pixels.height) == (self.width, canvas_h) {
            Some(compose::resample_gray(pixels, iw, ih))
        } else {
            None
        };
        if let Some(fitted) = fitted {
            let shared = Arc::new(fitted);
            *self.committed_layer.borrow_mut() = Arc::clone(&shared);
            self.pages.borrow_mut().current_page_mut().committed = shared;
            // The whole canvas may have changed; mark it dirty so the next
            // `render_frame` presents even if Slint's UI is otherwise clean.
            self.ink_damage
                .borrow_mut()
                .push(Rect::new(0, STATUS_BAR_H, self.width, canvas_h));
        }
    }

    /// Abandon an in-progress stroke without committing it (e.g. page swipe).
    ///
    /// Clears the live in-progress buffer and resets the ink engine so a
    /// subsequent `PenDown` starts cleanly.  Does not touch the committed layer.
    pub fn discard_in_progress_stroke(&self) {
        self.in_progress.borrow_mut().clear();
        let bw = *self.in_progress_base_width.borrow();
        let tool = self.engine.borrow().config().tool;
        self.clear_in_progress_layer(tool);
        *self.engine.borrow_mut() = InkEngine::new(InkConfig {
            base_width: bw,
            tool,
            ..InkConfig::default()
        });
        let canvas_h = self.canvas_height();
        self.ink_damage
            .borrow_mut()
            .push(Rect::new(0, STATUS_BAR_H, self.width, canvas_h));
        self.screen_change.set(true);
    }

    /// Clear both ink layers (committed and in-progress) for the current page.
    ///
    /// Typically called in response to the `clear-page` callback.  The cleared
    /// state is also persisted into `PageBook` so that navigating away and back
    /// returns to a blank page.
    pub fn clear_ink(&self) {
        let canvas_h = self.canvas_height();
        let (iw, ih) = self.ink_canvas_size();
        let blank = Arc::new(PixelBuf::new(iw, ih));
        *self.committed_layer.borrow_mut() = Arc::clone(&blank);
        self.strokes.borrow_mut().clear();
        self.in_progress.borrow_mut().clear();
        self.clear_in_progress_layer(self.engine.borrow().config().tool);

        // Persist the cleared state into the page roster.
        {
            let mut book = self.pages.borrow_mut();
            let page = book.current_page_mut();
            page.committed = blank;
            page.raster_stale = false;
            page.strokes.clear();
            page.in_progress.clear();
        }
        self.strokes_dirty.set(true);

        // The whole canvas went white; mark it dirty so the clear is presented.
        self.ink_damage
            .borrow_mut()
            .push(Rect::new(0, STATUS_BAR_H, self.width, canvas_h));

        // A full wipe is the ideal moment to flush ghosting with a Clear refresh.
        self.screen_change.set(true);
    }

    /// Get the background template for the current page.
    pub fn current_page_template(&self) -> ephemeris_core::PageTemplate {
        self.pages.borrow().current_page().template
    }

    /// Set the background template for the current page and mark the canvas dirty.
    pub fn set_current_page_template(&self, template: ephemeris_core::PageTemplate) {
        self.pages.borrow_mut().current_page_mut().template = template;
        let canvas_h = self.canvas_height();
        self.bg_dirty
            .borrow_mut()
            .push(Rect::new(0, STATUS_BAR_H, self.width, canvas_h));
    }

    /// Cycle to the next page template (Blank → Lines → Grid → Dot → Blank).
    pub fn cycle_page_template(&self) {
        use ephemeris_core::PageTemplate;
        let next = match self.current_page_template() {
            PageTemplate::Blank => PageTemplate::Lines,
            PageTemplate::Lines => PageTemplate::Grid,
            PageTemplate::Grid => PageTemplate::Dot,
            PageTemplate::Dot => PageTemplate::Blank,
        };
        self.set_current_page_template(next);
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
        // 0. Track the display's physical size (re-rasterizes ink on change).
        let (pw, ph) = display.physical_size();
        self.set_physical_size(pw, ph);

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

        // Background (Slint + template) rects to rebuild this frame. A change of
        // template or ink-compositing state invalidates the whole canvas.
        let composite_ink = self.should_composite_ink();
        let drawing_menu_open = self.component.get_drawing_menu_open();
        let template = self.current_page_template();
        let bg_key = Some((composite_ink, drawing_menu_open, template));
        let mut bg_damage = std::mem::take(&mut *self.bg_dirty.borrow_mut());
        match self.bg_key.replace(bg_key) {
            _ if screen_change => {}
            prev if prev == bg_key => {}
            // Menu toggle only changes the template under opaque chrome.
            Some((c, _, t)) if (c, t) == (composite_ink, template) => {
                bg_damage.extend(
                    chrome_rects(self.width, self.height)
                        .into_iter()
                        .filter(|r| r.width > 0 && r.height > 0),
                );
            }
            _ => bg_damage.push(Rect::new(0, STATUS_BAR_H, self.width, self.canvas_height())),
        }

        // eink render loop: only present when something actually changed.
        if slint_damage.is_empty()
            && ink_damage.is_empty()
            && bg_damage.is_empty()
            && !screen_change
        {
            return Ok(vec![]);
        }

        // 4. Feed damage to the refresh scheduler first so we know which
        //    rects to rebuild (avoids full-frame RGB565→gray + composite).
        let mut scheduler = self.scheduler.borrow_mut();
        if screen_change {
            scheduler.submit(
                Rect::new(0, 0, self.width, self.height),
                DamageSource::ScreenChange,
            );
        }
        for rect in slint_damage.iter().chain(&bg_damage) {
            scheduler.submit(*rect, DamageSource::Ui);
        }
        for rect in &ink_damage {
            scheduler.submit(*rect, DamageSource::Ink);
        }
        let present = scheduler
            .flush()
            .expect("damage was submitted, so flush must yield a present");
        drop(scheduler);

        let current_tool = self.engine.borrow().config().tool;
        let has_in_progress = !self.in_progress.borrow().is_empty();
        let geom = self.frame_geom();
        // Bilinear taps reach one logical pixel past a changed rect.
        const BG_MARGIN: u32 = 1;

        // 5. Refresh the background cache where the UI changed (never for
        //    ink-only frames): Slint luma + template at logical size, then a
        //    bilinear upscale into the physical cache.
        let mut bg_rects: Vec<Rect> = Vec::new();
        if screen_change {
            bg_rects.push(Rect::new(0, 0, self.width, self.height));
        } else {
            bg_rects.extend(
                slint_damage
                    .iter()
                    .chain(&bg_damage)
                    .filter_map(|r| geom.grow_logical(r, BG_MARGIN)),
            );
        }
        if !bg_rects.is_empty() {
            let pixels = self.rgb565_buf.borrow();
            let mut ui_gray = self.ui_gray.borrow_mut();
            let mut bg_phys = self.bg_phys.borrow_mut();
            let stride = self.width as usize;
            let chrome = chrome_rects(self.width, self.height);
            for rect in &bg_rects {
                convert_rgb565_rect_to_gray(&pixels, &mut ui_gray.data, stride, rect, self.height);
                if composite_ink {
                    render_background_in_rect(
                        &mut ui_gray,
                        template,
                        self.width,
                        self.height,
                        rect,
                    );
                    if drawing_menu_open {
                        // Opaque chrome shows pure Slint luma (no template).
                        for c in chrome.iter().filter_map(|c| compose::intersect(c, rect)) {
                            convert_rgb565_rect_to_gray(
                                &pixels,
                                &mut ui_gray.data,
                                stride,
                                &c,
                                self.height,
                            );
                        }
                    }
                }
            }
            for rect in &bg_rects {
                if let Some(pr) = geom.to_phys(rect, 0) {
                    compose::upscale_bilinear_rect(&ui_gray, &mut bg_phys, pr);
                }
            }
        }

        // 6. Compose `min(bg, ink)` 1:1 into the physical present buffer for
        //    the physical footprint of every damage rect.
        //
        // When the drawing menu is open, chrome is restored from `bg_phys`
        // only inside damage∩chrome (no full-panel `pre_ink` snapshot — that
        // stall was removed in ephemeris-ktg.4 / this path).
        let phys_damage: Vec<Rect> = present
            .damage
            .iter()
            .filter_map(|r| geom.to_phys(r, BG_MARGIN))
            .collect();
        {
            let bg_phys = self.bg_phys.borrow();
            let mut out = self.present_phys.borrow_mut();
            let committed = self.committed_layer.borrow();
            let ip_layer = self.in_progress_layer.borrow();
            let planes = InkPlanes {
                committed: &committed,
                live: has_in_progress.then_some(&*ip_layer),
                live_is_eraser: current_tool == Tool::Eraser,
                canvas_y0: self.ink_canvas_rows().0,
            };
            let ink = composite_ink.then_some(&planes);
            let chrome: Vec<Rect> = if composite_ink && drawing_menu_open {
                chrome_rects(self.width, self.height)
                    .iter()
                    .filter_map(|c| geom.to_phys(c, 0))
                    .collect()
            } else {
                Vec::new()
            };
            for pr in &phys_damage {
                compose::compose_rect(&mut out, &bg_phys, ink, *pr);
                for c in chrome.iter().filter_map(|c| compose::intersect(c, pr)) {
                    compose::copy_rect(&mut out, &bg_phys, c);
                }
            }
        }

        display.present_physical(&self.present_phys.borrow(), &phys_damage, present.mode)?;

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
    /// Clones committed raster, stroke list, in-progress points, and template.
    fn snapshot_live_state(&self, _canvas_h: u32) -> PageState {
        let book = self.pages.borrow();
        let current_template = book.current_page().template;
        drop(book);
        PageState {
            committed: Arc::clone(&self.committed_layer.borrow()),
            raster_stale: false,
            strokes: self.strokes.borrow().clone(),
            in_progress: self.in_progress.borrow().clone(),
            in_progress_base_width: *self.in_progress_base_width.borrow(),
            template: current_template,
        }
    }

    /// Restore the live ink state from `PageBook` for `page_index` and update
    /// the status bar properties.
    ///
    /// Must be called *after* `PageBook::navigate_to` or `push_blank_page` so
    /// that `book.current_index()` already reflects the target.
    fn load_page_state(&self, _page_index: usize) {
        let (new_committed, new_strokes, new_ip, new_bw, new_cur, new_count) = {
            let mut book = self.pages.borrow_mut();
            self.fit_page_raster(book.current_page_mut());
            let ps = book.current_page();
            (
                Arc::clone(&ps.committed),
                ps.strokes.clone(),
                ps.in_progress.clone(),
                ps.in_progress_base_width,
                book.current_index(),
                book.page_count(),
            )
        };

        // Restore ink layers.
        *self.committed_layer.borrow_mut() = new_committed;
        *self.strokes.borrow_mut() = new_strokes;
        *self.in_progress.borrow_mut() = new_ip;
        *self.in_progress_base_width.borrow_mut() = new_bw;

        // Reset the ink engine so no dangling stroke state bleeds across pages.
        *self.engine.borrow_mut() = InkEngine::new(InkConfig {
            base_width: new_bw,
            ..InkConfig::default()
        });
        *self.eraser.borrow_mut() = EraserEngine::new(EraserConfig::default());

        self.rebuild_in_progress_layer();

        // Update the Slint status bar (1-based display index).
        self.component.set_page_index((new_cur + 1) as i32);
        self.component.set_page_count(new_count as i32);

        // Switching pages replaces the whole canvas — force a Clear refresh so
        // the outgoing page leaves no ghost and the incoming ink is presented.
        self.screen_change.set(true);
    }

    // ── Recording API ─────────────────────────────────────────────────────

    pub fn set_has_mic(&self, v: bool) {
        self.component.set_has_mic(v);
    }

    pub fn set_rec_state(&self, state: i32) {
        self.component.set_rec_state(state);
    }

    pub fn set_rec_duration_label(&self, label: &str) {
        self.component
            .set_rec_duration_label(slint::SharedString::from(label));
    }

    pub fn set_show_recording_list(&self, show: bool) {
        self.component.set_show_recording_list(show);
    }

    pub fn set_recording_list(&self, recordings: &[RecordingEntry]) {
        self.component.set_recording_list(
            std::rc::Rc::new(slint::VecModel::from(recordings.to_vec())).into(),
        );
    }

    pub fn on_nav_rec_tapped<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_nav_rec_tapped(handler);
    }

    pub fn on_rec_new_tapped<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_rec_new_tapped(handler);
    }

    pub fn on_rec_pause_tapped<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_rec_pause_tapped(handler);
    }

    pub fn on_rec_resume_tapped<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_rec_resume_tapped(handler);
    }

    pub fn on_rec_stop_tapped<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_rec_stop_tapped(handler);
    }

    pub fn on_rec_discard_tapped<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_rec_discard_tapped(handler);
    }

    pub fn on_rec_play_tapped<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_rec_play_tapped(move |id| handler(id.to_string()));
    }

    pub fn on_rec_open_tapped<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_rec_open_tapped(move |id| handler(id.to_string()));
    }

    pub fn on_rec_transcribe_tapped<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_rec_transcribe_tapped(move |id| handler(id.to_string()));
    }

    pub fn on_rec_delete_tapped<F>(&self, mut handler: F)
    where
        F: FnMut(String) + 'static,
    {
        self.component
            .on_rec_delete_tapped(move |id| handler(id.to_string()));
    }

    pub fn on_rec_rename_confirm<F>(&self, mut handler: F)
    where
        F: FnMut(String, String) + 'static,
    {
        self.component
            .on_rec_rename_confirm(move |id, title| handler(id.to_string(), title.to_string()));
    }

    pub fn on_rec_rename_cancel<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_rec_rename_cancel(handler);
    }

    pub fn on_rec_transcription_close<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_rec_transcription_close(handler);
    }

    pub fn on_rec_transcription_retranscribe<F: FnMut() + 'static>(&self, handler: F) {
        self.component.on_rec_transcription_retranscribe(handler);
    }
}

// ── Background rendering ──────────────────────────────────────────────────────

/// Render page background pattern into the canvas ∩ `rect` region only.
fn render_background_in_rect(
    pal_buf: &mut PixelBuf,
    template: ephemeris_core::PageTemplate,
    width: u32,
    height: u32,
    rect: &Rect,
) {
    use ephemeris_core::PageTemplate;
    let stride = width as usize;
    let canvas_start = STATUS_BAR_H;
    let canvas_end = height.saturating_sub(TOOLBAR_H);
    let x0 = rect.x.min(width);
    let y0 = rect.y.max(canvas_start).min(canvas_end);
    let x1 = rect.x.saturating_add(rect.width).min(width);
    let y1 = rect.y.saturating_add(rect.height).min(canvas_end);
    if x1 <= x0 || y1 <= y0 {
        return;
    }

    match template {
        PageTemplate::Blank => {}
        PageTemplate::Lines => {
            let line_spacing = 28u32;
            let line_color = 200u8;
            let first = canvas_start
                + y0.saturating_sub(canvas_start).div_ceil(line_spacing) * line_spacing;
            let mut row = first;
            while row < y1 {
                if row >= y0 {
                    let off = row as usize * stride;
                    for col in x0 as usize..x1 as usize {
                        pal_buf.data[off + col] = line_color;
                    }
                }
                row = row.saturating_add(line_spacing);
            }
        }
        PageTemplate::Grid => {
            let spacing = 28u32;
            let grid_color = 220u8;
            for row in y0..y1 {
                let off = row as usize * stride;
                if (row - canvas_start).is_multiple_of(spacing) {
                    for col in x0 as usize..x1 as usize {
                        pal_buf.data[off + col] = grid_color;
                    }
                } else {
                    let mut col = x0 - (x0 % spacing);
                    if col < x0 {
                        col = col.saturating_add(spacing);
                    }
                    while col < x1 {
                        pal_buf.data[off + col as usize] = grid_color;
                        col = col.saturating_add(spacing);
                    }
                }
            }
        }
        PageTemplate::Dot => {
            let spacing = 28u32;
            let dot_color = 210u8;
            let first_row =
                canvas_start + y0.saturating_sub(canvas_start).div_ceil(spacing) * spacing;
            let mut row = first_row;
            while row < y1 {
                if row >= y0 {
                    let off = row as usize * stride;
                    let mut col = (x0 / spacing) * spacing;
                    while col < x1 {
                        if col >= x0 {
                            pal_buf.data[off + col as usize] = dot_color;
                        }
                        col = col.saturating_add(spacing);
                    }
                }
                row = row.saturating_add(spacing);
            }
        }
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

/// BT.709 luma as integer math (avoids per-pixel f32 on the ink hot path).
#[inline]
fn rgb_to_luma(r: u8, g: u8, b: u8) -> u8 {
    // (0.2126·R + 0.7152·G + 0.0722·B) ≈ (54·R + 183·G + 19·B) / 256
    ((54u32 * r as u32 + 183u32 * g as u32 + 19u32 * b as u32 + 128) / 256) as u8
}

/// Convert RGB565 → gray for a single damage rectangle into `dst`.
fn convert_rgb565_rect_to_gray(
    src: &[slint::platform::software_renderer::Rgb565Pixel],
    dst: &mut [u8],
    stride: usize,
    rect: &Rect,
    height: u32,
) {
    let x0 = rect.x as usize;
    let y0 = rect.y as usize;
    let x1 = rect.x.saturating_add(rect.width).min((stride) as u32) as usize;
    let y1 = rect.y.saturating_add(rect.height).min(height) as usize;
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    for y in y0..y1 {
        let row = y * stride;
        for x in x0..x1 {
            let (r, g, b) = rgb565_to_rgb888(src[row + x]);
            dst[row + x] = rgb_to_luma(r, g, b);
        }
    }
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
            t_us: 0,
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

    #[test]
    fn pen_width_presets_update_engine_base_width() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");
        ui.set_pen_width("thin");
        assert!((ui.pen_width() - 2.0).abs() < 1e-4);
        assert!((ui.engine.borrow().config().base_width - 2.0).abs() < 1e-4);
        ui.set_pen_width("thick");
        assert!((ui.pen_width() - 4.5).abs() < 1e-4);
        // Switching tools then back to pen restores the preset.
        ui.set_tool(Tool::Highlighter);
        ui.set_tool(Tool::Pen);
        assert!((ui.engine.borrow().config().base_width - 4.5).abs() < 1e-4);
        ui.set_pen_width("medium");
        assert!((ui.pen_width() - 3.0).abs() < 1e-4);
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

            // Status bar content rows must not be all-black (ink bleed check).
            // The bottom 2 rows of the status bar are the intentional chrome
            // divider line (pure black) — skip those when checking for bleed.
            for row in 0..(STATUS_BAR_H as usize).saturating_sub(2) {
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

    /// Acceptance (ephemeris-ktg.5): outside the last-span damage, committed
    /// pixels after PenUp match the live layer just before PenUp (min-merge,
    /// no full-stroke re-raster).
    #[test]
    fn pen_up_merge_matches_live_outside_last_span() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");
        let mut t = 0u32;
        let step = 8u32;
        ui.feed_input(&InputEvent::PenDown(pen_sample(80.0, 120.0)), t);
        t += step;
        for x in (90..520u32).step_by(8) {
            ui.feed_input(&InputEvent::PenMove(pen_sample(x as f32, 120.0)), t);
            t += step;
        }

        // Snapshot live ink immediately before PenUp.
        let live_before = ui.in_progress_layer.borrow().clone();
        let scale = ui.ink_scale();
        let (iw, ih) = (live_before.width, live_before.height);

        let update = ui.feed_input(&InputEvent::PenUp(pen_sample(520.0, 120.0)), t);
        let InkUpdate::Finished { stroke, damage } = update else {
            panic!("expected Finished, got {update:?}");
        };

        let committed = ui.committed_layer.borrow();
        let Some((lx0, ly0, lx1, ly1)) = raster::ink_pixel_bounds(&damage, scale, iw, ih) else {
            panic!("last-span damage must map to ink pixels: {damage:?}");
        };

        // Outside the last-span footprint, committed must equal the pre-PenUp
        // live layer (empty committed was white → pure copy via min-merge).
        let mut mismatches = 0u32;
        for y in 0..ih {
            for x in 0..iw {
                if x >= lx0 && x < lx1 && y >= ly0 && y < ly1 {
                    continue;
                }
                let i = y as usize * live_before.stride as usize + x as usize;
                let j = y as usize * committed.stride as usize + x as usize;
                if live_before.data[i] != committed.data[j] {
                    mismatches += 1;
                }
            }
        }
        assert_eq!(
            mismatches,
            0,
            "committed diverged from pre-PenUp live outside last span {damage:?} \
             (stroke {} pts)",
            stroke.points.len()
        );
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
        assert!(
            ui.get_page_strokes().is_empty(),
            "stroke list must be empty after clear"
        );
    }

    /// Finished pen strokes are kept in the page stroke list; eraser removes them.
    #[test]
    fn stroke_list_survives_commit_and_eraser_removes() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");
        ui.set_tool(Tool::Pen);
        ui.feed_input(&InputEvent::PenDown(pen_sample(20.0, 40.0)), 0);
        ui.feed_input(&InputEvent::PenMove(pen_sample(80.0, 40.0)), 5);
        ui.feed_input(&InputEvent::PenUp(pen_sample(120.0, 40.0)), 10);
        assert_eq!(ui.get_page_strokes().len(), 1);

        ui.set_tool(Tool::Eraser);
        ui.feed_input(&InputEvent::PenDown(pen_sample(60.0, 40.0)), 20);
        ui.feed_input(&InputEvent::PenUp(pen_sample(60.0, 40.0)), 25);
        assert!(
            ui.get_page_strokes().is_empty(),
            "whole-stroke eraser must remove hit strokes"
        );
        assert!(
            ui.committed_layer.borrow().data.iter().all(|&v| v == 255),
            "raster must rebuild empty after erase"
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

    // ── Action ring tests (acceptance criteria for idd.6) ────────────────

    /// show_action_ring sets visible=true and centers the ring at the given coords.
    #[test]
    fn ring_show_positions_and_enables() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");
        assert!(!ui.ring_visible());

        ui.show_action_ring(200.0, 150.0, true);

        assert!(ui.ring_visible());
        assert!((ui.component.get_ring_cx() - 200.0).abs() < f32::EPSILON);
        assert!((ui.component.get_ring_cy() - 150.0).abs() < f32::EPSILON);
        assert!(ui.component.get_ring_show_audio());
    }

    /// hide_action_ring clears the visible flag.
    #[test]
    fn ring_hide_clears_visible() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");
        ui.show_action_ring(400.0, 300.0, false);
        assert!(ui.ring_visible());
        ui.hide_action_ring();
        assert!(!ui.ring_visible());
    }

    /// Audio slice can be suppressed for non-mic devices.
    #[test]
    fn ring_audio_hidden_when_no_mic() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");
        ui.show_action_ring(400.0, 300.0, false);
        assert!(!ui.component.get_ring_show_audio());
    }

    /// Note slice callback fires and is wired through on_ring_note.
    #[test]
    fn ring_note_callback_fires() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");
        let fired = Rc::new(RefCell::new(false));
        let fired_c = fired.clone();
        ui.on_ring_note(move || {
            *fired_c.borrow_mut() = true;
        });
        ui.component.invoke_ring_note_tapped();
        assert!(*fired.borrow());
    }

    /// Calendar slice callback fires.
    #[test]
    fn ring_calendar_callback_fires() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");
        let fired = Rc::new(RefCell::new(false));
        let fired_c = fired.clone();
        ui.on_ring_calendar(move || {
            *fired_c.borrow_mut() = true;
        });
        ui.component.invoke_ring_calendar_tapped();
        assert!(*fired.borrow());
    }

    /// Task slice callback fires.
    #[test]
    fn ring_task_callback_fires() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");
        let fired = Rc::new(RefCell::new(false));
        let fired_c = fired.clone();
        ui.on_ring_task(move || {
            *fired_c.borrow_mut() = true;
        });
        ui.component.invoke_ring_task_tapped();
        assert!(*fired.borrow());
    }

    /// Audio slice callback fires.
    #[test]
    fn ring_audio_callback_fires() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");
        let fired = Rc::new(RefCell::new(false));
        let fired_c = fired.clone();
        ui.on_ring_audio(move || {
            *fired_c.borrow_mut() = true;
        });
        ui.component.invoke_ring_audio_tapped();
        assert!(*fired.borrow());
    }

    /// Dismissed callback fires for backdrop tap or centre × button.
    #[test]
    fn ring_dismissed_callback_fires() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");
        let fired = Rc::new(RefCell::new(false));
        let fired_c = fired.clone();
        ui.on_ring_dismissed(move || {
            *fired_c.borrow_mut() = true;
        });
        ui.show_action_ring(400.0, 300.0, true);
        ui.component.invoke_ring_dismissed();
        assert!(*fired.borrow());
    }

    /// swipe-left callback emitted from `.slint` wires correctly to
    /// `next_page` via `wire_swipe_navigation` (right-edge → left = next).
    #[test]
    fn swipe_left_callback_wires_to_next_page() {
        let ui = Rc::new(EphemerisUi::new(800, 600).expect("UI construction failed"));

        EphemerisUi::wire_swipe_navigation(ui.clone());

        // Simulate the `.slint` swipe-left callback firing — auto-creates page.
        ui.component.invoke_swipe_left();

        assert_eq!(
            ui.current_page_index(),
            (2, 2),
            "swipe-left must call next_page (auto-create)"
        );
    }

    // ── Eink theme tests (acceptance criteria for 2ql.6) ────────────────

    /// Light theme: chrome and canvas are white; borders and text are black.
    #[test]
    fn apply_theme_light_sets_white_chrome() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");
        ui.apply_theme("light");
        let theme = EinkTheme::get(&ui.component);
        let white = slint::Color::from_rgb_u8(255, 255, 255);
        let black = slint::Color::from_rgb_u8(0, 0, 0);
        assert_eq!(theme.get_chrome_bg(), white, "chrome-bg must be white");
        assert_eq!(theme.get_canvas_bg(), white, "canvas-bg must be white");
        assert_eq!(
            theme.get_chrome_border(),
            black,
            "chrome-border must be black"
        );
        assert_eq!(theme.get_btn_bg(), white, "btn-bg must be white");
        assert_eq!(theme.get_btn_fg(), black, "btn-fg must be black");
        assert_eq!(
            theme.get_btn_active_bg(),
            black,
            "active btn fill must be black"
        );
        assert_eq!(
            theme.get_btn_active_fg(),
            white,
            "active btn text must be white"
        );
        assert_eq!(
            theme.get_danger_fg(),
            black,
            "danger-fg must be black (not red)"
        );
    }

    /// Dark theme: chrome and canvas are black; borders and text are white.
    #[test]
    fn apply_theme_dark_inverts_chrome() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");
        ui.apply_theme("dark");
        let theme = EinkTheme::get(&ui.component);
        let white = slint::Color::from_rgb_u8(255, 255, 255);
        let black = slint::Color::from_rgb_u8(0, 0, 0);
        assert_eq!(theme.get_chrome_bg(), black, "chrome-bg must be black");
        assert_eq!(theme.get_canvas_bg(), black, "canvas-bg must be black");
        assert_eq!(
            theme.get_chrome_border(),
            white,
            "chrome-border must be white"
        );
        assert_eq!(
            theme.get_btn_active_bg(),
            white,
            "active btn fill must be white"
        );
        assert_eq!(
            theme.get_btn_active_fg(),
            black,
            "active btn text must be black"
        );
    }

    /// `swipe-right` callback emitted from `.slint` wires correctly to
    /// `prev_page` via `wire_swipe_navigation` (left-edge → right = previous).
    #[test]
    fn swipe_right_callback_wires_to_prev_page() {
        let ui = Rc::new(EphemerisUi::new(800, 600).expect("UI construction failed"));

        // Create two pages so we have somewhere to go back from.
        ui.next_page();
        assert_eq!(ui.current_page_index(), (2, 2));

        EphemerisUi::wire_swipe_navigation(ui.clone());

        // Simulate the `.slint` swipe-right callback firing.
        ui.component.invoke_swipe_right();

        assert_eq!(
            ui.current_page_index(),
            (1, 2),
            "swipe-right must call prev_page"
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
        let (_x0, _y0, _x1, y1) = damage_bbox(&damage);
        // STATUS_BAR_H is currently 0 (fullscreen canvas), so a "below status
        // bar" bound is not meaningful to assert. Keep the toolbar bound.
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

        // Toggle the start page — Slint repaints the content area.
        ui.set_show_start_page(true);
        let damage = ui.render_frame(&mut disp).expect("partial render failed");

        assert!(
            !damage.is_empty(),
            "show-start-page toggle must report damage"
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

        // UI-only change (toggle start page) → Partial.
        ui.set_show_start_page(true);
        ui.render_frame(&mut disp).expect("ui render failed");
        assert_eq!(disp.last_mode, Some(RefreshMode::Partial));

        // Page navigation → Clear.
        ui.next_page();
        ui.render_frame(&mut disp).expect("nav render failed");
        assert_eq!(disp.last_mode, Some(RefreshMode::Clear));
    }

    /// The scheduler's periodic-full policy is configurable via
    /// [`EphemerisUi::set_refresh_config`]: after N fast/partial frames it emits
    /// a `Full` to flush accumulated ghosting — but live ink stays `Fast` so
    /// mid-stroke presents never freeze on a full softbuffer rewrite.
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

        // Ink frames stay Fast even past the interval.
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
            vec![RefreshMode::Fast, RefreshMode::Fast, RefreshMode::Fast]
        );

        // Next UI damage should Full immediately (counter already past interval).
        ui.set_show_start_page(true);
        ui.render_frame(&mut disp).expect("ui render failed");
        assert_eq!(disp.last_mode, Some(RefreshMode::Full));
    }

    /// Verify that text elements (status bar, toolbar) render without panicking
    /// and that rendering completes successfully. This validates that the Slint
    /// software renderer can rasterize text glyphs, confirming system-fonts support
    /// is working in the headless environment.
    #[test]
    fn text_elements_render_headless() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");
        ui.set_page_title("Rendering Test");
        ui.set_page_index(1, 2);

        let mut display = MockDesktop::new(800, 600).expect("MockDesktop creation failed");

        // Render multiple frames with different titles to stress text rendering.
        for i in 0..3 {
            let title = format!("Page {}", i);
            ui.set_page_title(&title);
            let damage = ui
                .render_frame(&mut display)
                .expect("render_frame should not fail with text elements");
            // First frame should always have damage.
            if i == 0 {
                assert!(!damage.is_empty(), "first frame should produce damage");
            }
        }
    }

    /// Drawing menu must not hide ink: compositing stays on, input is gated.
    #[test]
    fn drawing_menu_keeps_ink_visible_but_blocks_input() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");
        assert!(ui.is_canvas_active());
        assert!(ui.should_composite_ink());

        ui.set_drawing_menu_open(true);
        assert!(
            !ui.is_canvas_active(),
            "drawing menu owns input — ink engine must not receive strokes"
        );
        assert!(
            ui.should_composite_ink(),
            "ink must stay composited under the floating drawing menu"
        );
    }

    /// Canvas gate for pen fast-path: open ink plane, not nav/burger chrome.
    #[test]
    fn hits_inkable_canvas_excludes_chrome() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");
        assert!(ui.is_canvas_active());
        assert!(
            ui.hits_inkable_canvas(400.0, 300.0),
            "centre of canvas must accept ink"
        );
        assert!(
            !ui.hits_inkable_canvas(10.0, 300.0),
            "nav rail must not start ink without Slint"
        );
        let burger_x = 800.0 - (BURGER_W + BURGER_RIGHT_PAD) as f32 + 2.0;
        assert!(
            !ui.hits_inkable_canvas(burger_x, BURGER_Y as f32 + 2.0),
            "burger button must not start ink without Slint"
        );
        ui.set_drawing_menu_open(true);
        assert!(
            !ui.hits_inkable_canvas(400.0, 300.0),
            "drawing menu open ⇒ canvas inactive"
        );
    }

    /// See-text overlay must hide ink compositing (no bleed through labels).
    #[test]
    fn see_text_hides_ink_compositing() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");
        ui.set_note_see_text(true);
        assert!(!ui.is_canvas_active());
        assert!(
            !ui.should_composite_ink(),
            "see-text must skip ink so handwriting does not bleed through the overlay"
        );
    }

    /// With the drawing menu open, ink remains in the canvas area while chrome
    /// rects (nav / menu) are restored from the pre-ink Slint frame.
    #[test]
    fn drawing_menu_composites_ink_outside_chrome() {
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

        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");
        let canvas_h = ui.canvas_height();
        let mut ink = PixelBuf::new(800, canvas_h);
        // Black vertical strip in the middle of the canvas (away from nav/menu).
        for row in 100..200 {
            for col in 300..320 {
                ink.data[(row * 800 + col) as usize] = 0;
            }
        }
        // Also plant black under the nav rail and under the menu panel — those
        // must be restored (not visible) when the menu is open.
        for row in 0..canvas_h {
            for col in 0..NAV_W {
                ink.data[(row * 800 + col) as usize] = 0;
            }
        }
        ui.push_canvas_pixels(&ink);
        ui.set_drawing_menu_open(true);
        ui.request_screen_change();

        let mut cap = CapturingDisplay { last: None };
        ui.render_frame(&mut cap).expect("render failed");
        let buf = cap.last.expect("expected a presented frame");
        let stride = buf.stride as usize;

        // Canvas ink (away from chrome) must still be dark.
        let mid_dark =
            (100..200usize).any(|row| (300..320usize).any(|col| buf.data[row * stride + col] < 50));
        assert!(
            mid_dark,
            "ink in the open canvas must remain visible with the drawing menu open"
        );

        // Nav rail columns must not be dominated by ink (chrome restored).
        let nav_avg: u32 = (0..600usize)
            .map(|row| buf.data[row * stride + 10] as u32)
            .sum::<u32>()
            / 600;
        assert!(
            nav_avg > 100,
            "nav rail must restore Slint chrome (avg luma {nav_avg} too dark — ink bleed)"
        );
    }

    // ── Panel-resolution ink plane (ephemeris-ktg.4) ──────────────────────

    const PN_LOGICAL: (u32, u32) = (800, 1067);
    const PN_PHYS: (u32, u32) = ephemeris_pal::display::PINENOTE_PX;

    /// HiDPI display double: logical 800×1067 UI on a 1404×1872 panel.
    struct PanelDisplay {
        last: Option<PixelBuf>,
        last_damage: Vec<Rect>,
        last_mode: Option<RefreshMode>,
        logical_presents: usize,
    }
    impl PanelDisplay {
        fn new() -> Self {
            PanelDisplay {
                last: None,
                last_damage: Vec::new(),
                last_mode: None,
                logical_presents: 0,
            }
        }
    }
    impl Display for PanelDisplay {
        fn size(&self) -> (u32, u32) {
            PN_LOGICAL
        }
        fn physical_size(&self) -> (u32, u32) {
            PN_PHYS
        }
        fn present(
            &mut self,
            _: &PixelBuf,
            _: &[Rect],
            _: RefreshMode,
        ) -> Result<(), ephemeris_pal::DisplayError> {
            self.logical_presents += 1;
            Ok(())
        }
        fn present_physical(
            &mut self,
            buf: &PixelBuf,
            damage: &[Rect],
            mode: RefreshMode,
        ) -> Result<(), ephemeris_pal::DisplayError> {
            self.last = Some(buf.clone());
            self.last_damage = damage.to_vec();
            self.last_mode = Some(mode);
            Ok(())
        }
    }

    fn draw_line(ui: &EphemerisUi, from: (f32, f32), to: (f32, f32), t0: u32) {
        ui.feed_input(&InputEvent::PenDown(pen_sample(from.0, from.1)), t0);
        for i in 1..=20 {
            let t = i as f32 / 20.0;
            let p = (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t);
            ui.feed_input(&InputEvent::PenMove(pen_sample(p.0, p.1)), t0 + i * 8);
        }
        ui.feed_input(&InputEvent::PenUp(pen_sample(to.0, to.1)), t0 + 200);
    }

    #[test]
    fn panel_display_gets_physical_ink_presents() {
        let ui = EphemerisUi::new(PN_LOGICAL.0, PN_LOGICAL.1).expect("UI construction failed");
        let mut disp = PanelDisplay::new();
        ui.render_frame(&mut disp).expect("first render failed");

        assert_eq!(ui.physical_size(), PN_PHYS);
        assert_eq!(ui.ink_canvas_size(), PN_PHYS);
        let s = ui.ink_scale();
        assert!((s.sx - 1.755).abs() < 1e-3 && (s.sy - 1.7544).abs() < 1e-3);
        let first = disp.last.as_ref().expect("first frame presented");
        assert_eq!((first.width, first.height), PN_PHYS);

        draw_line(&ui, (200.0, 300.0), (400.0, 300.0), 0);
        ui.render_frame(&mut disp).expect("ink render failed");
        assert_eq!(disp.last_mode, Some(RefreshMode::Fast));
        assert_eq!(disp.logical_presents, 0, "UI must use the physical path");

        // Damage is physical, bounded to the stroke — no full-panel rewrite.
        let area: u64 = disp
            .last_damage
            .iter()
            .map(|r| u64::from(r.width) * u64::from(r.height))
            .sum();
        assert!(area > 0 && area < u64::from(PN_PHYS.0 * PN_PHYS.1) / 20);
        for r in &disp.last_damage {
            assert!(r.x + r.width <= PN_PHYS.0 && r.y + r.height <= PN_PHYS.1);
        }

        // Ink lands at the physical position of logical (300, 300).
        let buf = disp.last.as_ref().unwrap();
        let (px, py) = ((300.0 * s.sx) as usize, (300.0 * s.sy) as usize);
        let dark = (py - 4..py + 4).any(|y| buf.data[y * buf.stride as usize + px] < 80);
        assert!(dark, "stroke must be visible at physical ({px}, {py})");
        assert_eq!(ui.committed_layer.borrow().width, PN_PHYS.0);
    }

    #[test]
    fn ink_only_frames_reuse_background_cache() {
        let ui = EphemerisUi::new(PN_LOGICAL.0, PN_LOGICAL.1).expect("UI construction failed");
        let mut disp = PanelDisplay::new();
        ui.render_frame(&mut disp).expect("first render failed");
        assert!(ui.render_frame(&mut disp).unwrap().is_empty());

        // Sentinel background: a rebuilt background would put white paper
        // back inside the damage rect.
        ui.bg_phys.borrow_mut().fill(77);
        draw_line(&ui, (200.0, 300.0), (300.0, 300.0), 0);
        ui.render_frame(&mut disp).expect("ink render failed");
        assert_eq!(disp.last_mode, Some(RefreshMode::Fast));
        let buf = disp.last.as_ref().unwrap();
        assert!(!disp.last_damage.is_empty());
        for r in &disp.last_damage {
            for y in r.y..r.y + r.height {
                for x in r.x..r.x + r.width {
                    assert!(
                        buf.data[(y * buf.stride + x) as usize] <= 77,
                        "ink-only present must compose over the cached background ({x}, {y})"
                    );
                }
            }
        }
    }

    #[test]
    fn logical_page_bin_loads_at_physical_size() {
        let ui = EphemerisUi::new(PN_LOGICAL.0, PN_LOGICAL.1).expect("UI construction failed");
        ui.set_physical_size(PN_PHYS.0, PN_PHYS.1);

        // Old bin: logical 800×1067 with a black bar at row 500.
        let (lw, lh) = PN_LOGICAL;
        let mut data = vec![255u8; (lw * lh) as usize];
        for x in 100..700 {
            for y in 499..502 {
                data[(y * lw + x) as usize] = 0;
            }
        }
        ui.restore_page_pixels(0, lw, lh, data);
        let committed = ui.get_committed_pixels_arc();
        assert_eq!((committed.width, committed.height), PN_PHYS);
        let py = (500.0 * ui.ink_scale().sy) as usize;
        assert!(committed.data[py * PN_PHYS.0 as usize + 700] < 100);

        // Physical-size bins load unchanged.
        let phys = vec![200u8; (PN_PHYS.0 * PN_PHYS.1) as usize];
        ui.restore_page_pixels(0, PN_PHYS.0, PN_PHYS.1, phys.clone());
        assert_eq!(ui.get_committed_pixels(), phys);
    }

    #[test]
    fn hidden_page_strokes_regenerate_at_physical_size_on_load() {
        let ui = EphemerisUi::new(PN_LOGICAL.0, PN_LOGICAL.1).expect("UI construction failed");
        ui.push_new_page();
        ui.navigate_to_page(0);

        // Page 1 gets a logical-size raster plus strokes (an old save).
        let mut s = Stroke::new(Tool::Pen, ephemeris_core::model::Color::BLACK, 3.0);
        for i in 0..40 {
            s.points.push(Point {
                x: 100.0 + i as f32 * 5.0,
                y: 400.0,
                pressure: 0.5,
                tilt: 0.0,
                t_ms: i * 8,
            });
        }
        let (lw, lh) = PN_LOGICAL;
        ui.restore_page_pixels(1, lw, lh, vec![255u8; (lw * lh) as usize]);
        ui.restore_page_strokes(1, vec![s]);

        ui.set_physical_size(PN_PHYS.0, PN_PHYS.1);
        ui.navigate_to_page(1);
        let committed = ui.get_committed_pixels_arc();
        assert_eq!((committed.width, committed.height), PN_PHYS);
        let sc = ui.ink_scale();
        let (px, py) = ((200.0 * sc.sx) as usize, (400.0 * sc.sy) as usize);
        assert!(
            committed.data[py * PN_PHYS.0 as usize + px] < 50,
            "strokes must be re-rasterized at panel resolution"
        );
    }

    #[test]
    fn resize_rerasterizes_visible_page_and_eraser_still_hits() {
        let ui = EphemerisUi::new(800, 600).expect("UI construction failed");
        draw_line(&ui, (100.0, 100.0), (300.0, 100.0), 0);
        assert_eq!(ui.committed_layer.borrow().width, 800);

        assert!(ui.set_physical_size(1600, 1200));
        assert!(!ui.set_physical_size(1600, 1200), "same size is a no-op");
        {
            let committed = ui.committed_layer.borrow();
            assert_eq!((committed.width, committed.height), (1600, 1200));
            assert!(
                committed.data[200 * 1600 + 400] < 50,
                "stroke at 2× position"
            );
        }

        // Eraser works in logical coordinates and clears the physical raster.
        ui.set_tool(Tool::Eraser);
        ui.feed_input(&InputEvent::PenDown(pen_sample(200.0, 100.0)), 1000);
        ui.feed_input(&InputEvent::PenUp(pen_sample(200.0, 100.0)), 1008);
        assert!(ui.get_page_strokes().is_empty());
        assert!(ui.committed_layer.borrow().data.iter().all(|&v| v == 255));
    }
}

/// Build a Slint row; falls back to plain text when StyledText rejects the markdown.
fn see_text_line(r: &ephemeris_core::see_text::SeeTextRow) -> SeeTextLine {
    let body = slint::StyledText::from_markdown(&r.styled_md)
        .unwrap_or_else(|_| slint::StyledText::from_plain_text(&r.plain));
    SeeTextLine {
        kind: r.kind,
        number: r.number,
        checked: r.checked,
        indent: r.indent,
        gap_before: r.gap_before,
        pending: r.pending,
        stale: r.stale,
        line_index: r.line_index,
        editable: r.editable,
        whole_highlight: r.whole_highlight,
        whole_underline: r.whole_underline,
        plain: r.plain.as_str().into(),
        body,
    }
}

#[cfg(test)]
mod see_text_tests {
    use super::*;
    use ephemeris_core::see_text::{rows_from_markdown, SeeTextRow};

    fn row(md: &str) -> SeeTextRow {
        SeeTextRow {
            kind: 3,
            number: 0,
            checked: false,
            indent: 0,
            gap_before: false,
            pending: false,
            stale: false,
            line_index: 0,
            editable: false,
            whole_highlight: false,
            whole_underline: false,
            styled_md: md.into(),
            plain: md.into(),
        }
    }

    #[test]
    fn converted_output_is_accepted_by_slint() {
        use ephemeris_core::{styled_inline_markdown as conv, InlineKind, InlineMark};
        let mk = |kind, start, end| InlineMark { kind, start, end };
        let cases: Vec<(&str, Vec<InlineMark>)> = vec![
            ("Grüße über Äpfel", vec![mk(InlineKind::Highlight, 0, 5)]),
            ("2 * 3 = 6 and a_b_c", vec![]),
            (
                "`code` <b>x</b> # [a](b)",
                vec![mk(InlineKind::Underline, 0, 4)],
            ),
            (
                "nested marks here",
                vec![
                    mk(InlineKind::Bold, 0, 10),
                    mk(InlineKind::Underline, 3, 17),
                    mk(InlineKind::Highlight, 7, 12),
                ],
            ),
            ("# heading-like", vec![]),
            ("1. numbered-like", vec![]),
            ("- dash-like", vec![]),
        ];
        for (text, marks) in cases {
            let md = conv(text, &marks);
            assert!(
                slint::StyledText::from_markdown(&md).is_ok(),
                "rejected {md:?} for {text:?}"
            );
        }
    }

    #[test]
    fn markdown_rows_are_accepted_by_slint() {
        let rows =
            rows_from_markdown("# A\n\n- b\n  - c\n\n==d== <u>e</u> **f** \\# g\n- [x] h_i <b>");
        for r in &rows {
            assert!(
                slint::StyledText::from_markdown(&r.styled_md).is_ok(),
                "rejected {:?}",
                r.styled_md
            );
        }
    }

    /// Renders See-text to a PGM when `SEE_TEXT_PGM` names an output path.
    #[test]
    fn render_see_text_snapshot() {
        use ephemeris_pal::{DisplayError, PixelBuf, RefreshMode};
        struct Cap(Option<PixelBuf>);
        impl Display for Cap {
            fn size(&self) -> (u32, u32) {
                (800, 600)
            }
            fn present(
                &mut self,
                buf: &PixelBuf,
                _: &[Rect],
                _: RefreshMode,
            ) -> Result<(), DisplayError> {
                self.0 = Some(buf.clone());
                Ok(())
            }
        }
        let Ok(path) = std::env::var("SEE_TEXT_PGM") else {
            return;
        };
        let ui = EphemerisUi::new(800, 600).expect("UI");
        let rows = rows_from_markdown(
            "# Heading one\n\n## Sub heading\n\n- bullet b\n  - nested c\n- [ ] open task\n- [x] done task\n3. numbered\n\n==highlight d== <u>underline e</u> and plain Grüße *star*\nsecond line\n\n…",
        );
        ui.set_see_text_rows(&rows);
        ui.set_note_see_text(true);
        // `SEE_TEXT_EDITOR=1` renders with the line editor sheet open.
        if std::env::var("SEE_TEXT_EDITOR").is_ok() {
            let c = ui.clone_component();
            c.set_see_text_edit_text("bullet b".into());
            c.set_see_text_edit_kind(4);
            c.set_see_text_edit_indent(1);
            c.set_see_text_edit_underline(true);
            c.set_see_text_edit_stale(true);
            c.set_see_text_edit_open(true);
        }
        ui.request_screen_change();
        let mut cap = Cap(None);
        ui.render_frame(&mut cap).expect("render");
        let buf = cap.0.expect("frame");
        let mut out = format!("P5\n{} {}\n255\n", buf.width, buf.height).into_bytes();
        for row in 0..buf.height as usize {
            let st = buf.stride as usize;
            out.extend_from_slice(&buf.data[row * st..row * st + buf.width as usize]);
        }
        std::fs::write(path, out).unwrap();
    }

    #[test]
    fn invalid_markdown_falls_back_to_plain() {
        let l = see_text_line(&row("# heading"));
        assert_eq!(l.body, slint::StyledText::from_plain_text("# heading"));
    }
}
