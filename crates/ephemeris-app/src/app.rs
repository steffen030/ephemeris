use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveDateTime, Timelike, Utc};
use ephemeris_core::ics::IcsReader;
use ephemeris_core::ink::InkUpdate;
use ephemeris_core::model::Tool;
use ephemeris_core::profile_manager::ProfileManager;
use ephemeris_core::{
    count_lane, export_raster_pages_to_pdf, filter_agenda, format_note_title, is_stale,
    list_engage, load_merged_htr_lexicon, lower_thread_nice_priority, mark_reviewed,
    markdown_to_blocks, markdown_to_plain, recognition_model_version, resolve_lane,
    resolve_wiki_target, review_queue, search_events, search_local_notes, search_tasks,
    set_lane_tag, spawn_user_lexicon_refresh, strip_inline_markdown, sync_notes_to_vault,
    sync_tags_from_structured, to_agenda_entries, AgendaRange, BgRecogIdle, CalendarEvent,
    CascadingTranscriber, Config, EngageFilter, EventId, InkEdit, InkHistoryMap,
    LineImageHtrTranscriber, MarkdownTaskExtractor, PageRecognitionCache, PdfExportOptions,
    ProfileId, RasterOcrTranscriber, RasterPage, SearchHit, SharedHtrLexicon, StrokeHtrTranscriber,
    Task, TaskContext, TaskContextKind, TaskId, TaskPriority, TaskProvider, UserLexiconSources,
    VaultFtsIndex, VaultNoteExport, BG_RECOG_IDLE_DELAY, DEFAULT_INK_HISTORY_DEPTH,
    RECOGNITION_CACHE_FORMAT,
};
#[cfg(target_os = "linux")]
use ephemeris_pal::input::{EvdevPenSource, PenTarget};
use ephemeris_pal::input::{InputEvent, PenSample};
use slint::{Global, Model};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex, OnceLock, RwLock,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::runtime::Runtime;
use tokio::sync::mpsc::UnboundedSender;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::WindowId;

use ephemeris_pal::display::{kiosk_enabled, DesktopWindow};
use ephemeris_pal::InkLatencyTrace;
use ephemeris_ui::EphemerisUi;

/// Top-edge band (UI logical px) used to detect a swipe-down that reveals the
/// GNOME/Phosh panel while in exclusive fullscreen.
const CHROME_SWIPE_EDGE_Y: f32 = 36.0;
/// Minimum downward travel (UI logical px) to count as a chrome reveal swipe.
const CHROME_SWIPE_MIN_DY: f32 = 20.0;
/// After window creation, un-fullscreen geometry is compositor settling, not
/// a shell-driven reveal.
const KIOSK_SETTLE: Duration = Duration::from_secs(3);

/// Debounce flag for background vault git commit/push.
static VAULT_GIT_PUSH_PENDING: AtomicBool = AtomicBool::new(false);
/// Set when a push is requested while one is already pending — run again after.
static VAULT_GIT_PUSH_AGAIN: AtomicBool = AtomicBool::new(false);

/// How long ink can sit dirty before a background vault OCR/PDF batch runs.
const VAULT_EXPORT_BATCH_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Left nav-rail section indices (`nav-section` in main.slint).
const NAV_HOME: i32 = 0;
const NAV_TASKS: i32 = 1;
const NAV_NOTES: i32 = 2;
const NAV_CAL: i32 = 4;
const NAV_REC: i32 = 5;

/// Coalesce local page `.bin` / strokes JSON writes so rapid strokes do not
/// stall the UI thread on flash I/O. Leave-note still flushes synchronously.
const LOCAL_PAGE_WRITE_DEBOUNCE: Duration = Duration::from_millis(400);

/// Hold-still duration at end of a stroke before smart-shape snap (ephemeris-9co).
const SHAPE_HOLD: Duration = Duration::from_millis(500);
/// Max tip drift (logical px) while holding for a smart-shape snap.
const SHAPE_HOLD_SLOP: f32 = 10.0;

/// Min lasso path length (logical px) before a loop counts as a selection.
const LASSO_MIN_PATH: f32 = 40.0;
/// Tip inside selection bbox (inflated) starts a drag rather than a new lasso.
const SELECT_HIT_PAD: f32 = 16.0;

/// Pending drawing-menu / shortcut command processed on the UI thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingInkCmd {
    Undo,
    Redo,
    Clear,
}

/// Completed background per-line recognition (drained on the UI thread).
/// Main-thread handlers slower than this log a warning (ephemeris-ktg.22).
const MAIN_THREAD_STALL: Duration = Duration::from_millis(100);

/// Logs a warning on drop when the guarded main-thread section was slow.
///
/// Mutter shows "not responding" after ~5 s without a ping reply; this names
/// the section that blocked so device logs point at the culprit.
struct StallWatch {
    label: &'static str,
    start: Instant,
}

impl StallWatch {
    fn new(label: &'static str) -> Self {
        Self {
            label,
            start: Instant::now(),
        }
    }
}

impl Drop for StallWatch {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed();
        if elapsed >= MAIN_THREAD_STALL {
            tracing::warn!(
                "main-thread stall: {} took {} ms",
                self.label,
                elapsed.as_millis()
            );
        }
    }
}

struct BgRecogJobResult {
    note_id: u32,
    doc_page: usize,
    cache: PageRecognitionCache,
}

/// Single low-priority worker for idle per-line recognition (ephemeris-ktg.12).
///
/// Armed ≥ [`BG_RECOG_IDLE_DELAY`] after PenUp; cancelled on PenDown; never while
/// drawing. Results land in [`Self::result`] for `about_to_wait` to apply.
struct BackgroundRecogQueue {
    idle: RefCell<BgRecogIdle>,
    cancel: Arc<AtomicBool>,
    active: Arc<AtomicBool>,
    result: Arc<Mutex<Option<BgRecogJobResult>>>,
    /// In-memory page caches; disk is source of truth across restarts.
    caches: RefCell<HashMap<(u32, usize), PageRecognitionCache>>,
}

impl BackgroundRecogQueue {
    fn new() -> Self {
        Self {
            idle: RefCell::new(BgRecogIdle::new(BG_RECOG_IDLE_DELAY)),
            cancel: Arc::new(AtomicBool::new(false)),
            active: Arc::new(AtomicBool::new(false)),
            result: Arc::new(Mutex::new(None)),
            caches: RefCell::new(HashMap::new()),
        }
    }

    fn on_pen_down(&self) {
        self.cancel.store(true, Ordering::Release);
        self.idle.borrow_mut().on_pen_down();
    }

    fn on_pen_up(&self) {
        self.cancel.store(false, Ordering::Release);
        self.idle.borrow_mut().on_pen_up(Instant::now());
    }

    fn set_drawing(&self, drawing: bool) {
        self.cancel.store(drawing, Ordering::Release);
        self.idle.borrow_mut().set_drawing(drawing);
    }

    fn has_pending_work(&self) -> bool {
        self.active.load(Ordering::Relaxed)
            || self.result.lock().map(|g| g.is_some()).unwrap_or(false)
            || self.idle.borrow().should_run(Instant::now())
    }

    fn get_or_load(
        &self,
        data_dir: &Path,
        note_id: u32,
        doc_page: usize,
        model_version: &str,
    ) -> PageRecognitionCache {
        if let Some(c) = self.caches.borrow().get(&(note_id, doc_page)) {
            return c.clone();
        }
        let loaded = PageRecognitionCache::load(data_dir, note_id, doc_page)
            .unwrap_or_else(|| PageRecognitionCache::new(model_version));
        self.caches
            .borrow_mut()
            .insert((note_id, doc_page), loaded.clone());
        loaded
    }

    fn store(&self, note_id: u32, doc_page: usize, cache: PageRecognitionCache) {
        self.caches.borrow_mut().insert((note_id, doc_page), cache);
    }

    /// In-memory cache for a page, if loaded (holds the latest user edits).
    fn current(&self, note_id: u32, doc_page: usize) -> Option<PageRecognitionCache> {
        self.caches.borrow().get(&(note_id, doc_page)).cloned()
    }

    fn drop_note(&self, note_id: u32, doc_page_count: usize) {
        let mut caches = self.caches.borrow_mut();
        for p in 0..doc_page_count.max(1) {
            caches.remove(&(note_id, p));
        }
    }

    /// Cheap pre-check for [`Self::try_spawn`], so callers can skip cloning the
    /// page's strokes on every event-loop pass (pen samples wake it ~100×/s).
    fn may_spawn(&self, force: bool) -> bool {
        let idle = self.idle.borrow();
        !idle.is_drawing()
            && (force || idle.should_run(Instant::now()))
            && !self.active.load(Ordering::Acquire)
    }

    /// Start recognition for a page when idle (or `force`, e.g. See-text open).
    #[allow(clippy::too_many_arguments)]
    fn try_spawn(
        &self,
        force: bool,
        config: &Config,
        data_dir: &Path,
        note_id: u32,
        doc_page: usize,
        strokes: Vec<ephemeris_core::Stroke>,
        ruling_pitch: Option<f32>,
    ) -> bool {
        // Never compete with live ink (force = See-text still waits for pen up).
        if self.idle.borrow().is_drawing() {
            return false;
        }
        if !force && !self.idle.borrow().should_run(Instant::now()) {
            return false;
        }
        let version = recognition_model_version_for(config);
        let mut cache = self.get_or_load(data_dir, note_id, doc_page, &version);
        cache.sync_strokes(&strokes, ruling_pitch, &version);
        if !cache.has_pending() {
            self.store(note_id, doc_page, cache);
            self.idle.borrow_mut().clear_arm();
            return false;
        }
        if self
            .active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        self.idle.borrow_mut().clear_arm();
        self.cancel.store(false, Ordering::Release);
        let cancel = Arc::clone(&self.cancel);
        let active = Arc::clone(&self.active);
        let mailbox = Arc::clone(&self.result);
        let data_dir = data_dir.to_path_buf();
        let config = config.clone();
        std::thread::spawn(move || {
            lower_thread_nice_priority();
            let cascade = build_cascading_transcriber(&config);
            let cancel_fn = || cancel.load(Ordering::Acquire);
            let _ = cache.recognise_pending(&strokes, ruling_pitch, &cancel_fn, |line, idx| {
                cascade.transcribe_line(line, idx)
            });
            if let Err(e) = cache.save(&data_dir, note_id, doc_page) {
                tracing::warn!("Failed to save recognition cache for note {note_id}: {e}");
            }
            if let Ok(mut guard) = mailbox.lock() {
                *guard = Some(BgRecogJobResult {
                    note_id,
                    doc_page,
                    cache,
                });
            }
            active.store(false, Ordering::Release);
        });
        true
    }
}

pub(crate) fn recognition_model_version_for(config: &Config) -> String {
    recognition_model_version(
        RECOGNITION_CACHE_FORMAT,
        config.handwriting_language.as_str(),
        "cascade",
        config.ocr_enabled,
    )
}

fn note_ruling_pitch(ui: &EphemerisUi) -> Option<f32> {
    template_ruling_pitch(ui.current_page_template())
}

fn template_ruling_pitch(template: ephemeris_core::PageTemplate) -> Option<f32> {
    match template {
        ephemeris_core::PageTemplate::Blank => None,
        ephemeris_core::PageTemplate::Lines
        | ephemeris_core::PageTemplate::Grid
        | ephemeris_core::PageTemplate::Dot => Some(ephemeris_core::LINED_TEMPLATE_PITCH),
    }
}

/// Coalesces vault OCR/PDF export so it never runs on the ink hot path.
///
/// Dirty notes flush on note leave (`flush_soon`) or after
/// [`VAULT_EXPORT_BATCH_INTERVAL`]. Structural index changes set
/// `needs_full_sync` for one deferred library pass (same cascade quality).
struct VaultExportQueue {
    dirty: RefCell<HashSet<u32>>,
    needs_full_sync: Cell<bool>,
    flush_soon: Cell<bool>,
    last_flush_at: Cell<Instant>,
    active: Arc<AtomicBool>,
}

impl VaultExportQueue {
    fn new() -> Self {
        Self {
            dirty: RefCell::new(HashSet::new()),
            needs_full_sync: Cell::new(false),
            flush_soon: Cell::new(false),
            last_flush_at: Cell::new(Instant::now()),
            active: Arc::new(AtomicBool::new(false)),
        }
    }

    fn mark(&self, note_id: u32) {
        self.dirty.borrow_mut().insert(note_id);
        // A mark while a flush is running must schedule a follow-up so the
        // newer ink is not stranded until the next 10‑minute batch.
        if self.active.load(Ordering::Relaxed) {
            self.flush_soon.set(true);
        }
    }

    fn mark_full_sync(&self) {
        self.needs_full_sync.set(true);
    }

    fn request_flush_soon(&self) {
        self.flush_soon.set(true);
    }

    fn has_pending(&self) -> bool {
        self.needs_full_sync.get()
            || self.flush_soon.get()
            || !self.dirty.borrow().is_empty()
            || self.active.load(Ordering::Relaxed)
    }

    /// Start a background flush when leave-note was requested or the batch
    /// interval elapsed. Returns true if a worker was spawned.
    ///
    /// `local_writes` is drained synchronously first so the worker reads
    /// up-to-date page bins (avoids hash-skipping the PDF on stale rasters).
    fn try_flush(
        &self,
        config: &Config,
        notes: &[AppNote],
        notebooks: &[AppNotebook],
        data_dir: &Path,
        connections: &[AppConnection],
        local_writes: &LocalPageWriteQueue,
    ) -> bool {
        // Export is driven by per-profile vault connections, not the legacy
        // single `config.vault_path` (which may point at another profile).
        if !connections.iter().any(|c| c.kind == "vault") {
            self.dirty.borrow_mut().clear();
            self.needs_full_sync.set(false);
            self.flush_soon.set(false);
            return false;
        }
        if self.active.load(Ordering::Relaxed) {
            return false;
        }
        let force = self.flush_soon.replace(false);
        let due = self.last_flush_at.get().elapsed() >= VAULT_EXPORT_BATCH_INTERVAL;
        if !force && !due {
            return false;
        }
        let full = self.needs_full_sync.replace(false);
        let ids: Vec<u32> = self.dirty.borrow_mut().drain().collect();
        if !full && ids.is_empty() {
            return false;
        }
        if self
            .active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            // Race: put work back (including leave-note urgency).
            if full {
                self.needs_full_sync.set(true);
            }
            if force {
                self.flush_soon.set(true);
            }
            self.dirty.borrow_mut().extend(ids);
            return false;
        }
        // Page bins must be on disk before the worker hashes/rasterises them;
        // otherwise a 10‑minute batch can export yesterday's pixels, match the
        // sidecar hash, skip the PDF rewrite, and clear dirty permanently.
        local_writes.flush_sync();
        self.last_flush_at.set(Instant::now());
        let config = config.clone();
        let notes = notes.to_vec();
        let notebooks = notebooks.to_vec();
        let data_dir = data_dir.to_path_buf();
        let connections = connections.to_vec();
        let active = Arc::clone(&self.active);
        std::thread::spawn(move || {
            if full {
                sync_notes_export_to_vault(&config, &notes, &notebooks, &data_dir, &connections);
            } else {
                let mut touched_vaults = Vec::new();
                for note_id in ids {
                    if let Some(vault) = export_single_note_to_vault_no_git(
                        &config,
                        &notes,
                        &notebooks,
                        &data_dir,
                        &connections,
                        note_id,
                    ) {
                        if !touched_vaults.iter().any(|p: &PathBuf| p == &vault) {
                            touched_vaults.push(vault);
                        }
                    }
                }
                for vault in &touched_vaults {
                    schedule_vault_git_push_for(&config, vault);
                }
            }
            active.store(false, Ordering::Release);
        });
        true
    }
}

/// Latest local page raster+strokes waiting for a debounced disk write.
struct PendingPageWrite {
    note_id: u32,
    doc_page: usize,
    width: u32,
    height: u32,
    /// Shared ref to the live committed layer (no UI-thread memcpy).
    pixels: Arc<ephemeris_pal::display::PixelBuf>,
    strokes: Vec<ephemeris_core::Stroke>,
}

/// Debounced async writer for note page `.bin` + strokes JSON.
struct LocalPageWriteQueue {
    pending: Mutex<Option<PendingPageWrite>>,
    last_queued_at: Mutex<Instant>,
    active: Arc<AtomicBool>,
    data_dir: PathBuf,
}

impl LocalPageWriteQueue {
    fn new(data_dir: PathBuf) -> Self {
        Self {
            pending: Mutex::new(None),
            last_queued_at: Mutex::new(Instant::now()),
            active: Arc::new(AtomicBool::new(false)),
            data_dir,
        }
    }

    fn queue(&self, job: PendingPageWrite) {
        if let Ok(mut g) = self.pending.lock() {
            *g = Some(job);
        }
        if let Ok(mut t) = self.last_queued_at.lock() {
            *t = Instant::now();
        }
    }

    fn has_pending(&self) -> bool {
        self.active.load(Ordering::Relaxed)
            || self.pending.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    /// Spawn a background write when debounce elapsed (or `force`).
    fn try_flush_async(&self, force: bool) -> bool {
        if self.active.load(Ordering::Relaxed) {
            return false;
        }
        let ready = force
            || self
                .last_queued_at
                .lock()
                .map(|t| t.elapsed() >= LOCAL_PAGE_WRITE_DEBOUNCE)
                .unwrap_or(true);
        if !ready {
            return false;
        }
        let job = match self.pending.lock() {
            Ok(mut g) => g.take(),
            Err(_) => None,
        };
        let Some(job) = job else {
            return false;
        };
        if self
            .active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            if let Ok(mut g) = self.pending.lock() {
                if g.is_none() {
                    *g = Some(job);
                }
            }
            return false;
        }
        let data_dir = self.data_dir.clone();
        let active = Arc::clone(&self.active);
        std::thread::spawn(move || {
            save_note_page_pixels(
                &data_dir,
                job.note_id,
                job.doc_page,
                job.width,
                job.height,
                &job.pixels.data,
            );
            save_note_page_strokes(&data_dir, job.note_id, job.doc_page, &job.strokes);
            active.store(false, Ordering::Release);
        });
        true
    }

    /// Block until any in-flight write finishes, then write the latest pending
    /// job on this thread (leave-note / switch-note durability).
    fn flush_sync(&self) {
        while self.active.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(2));
        }
        let job = self.pending.lock().ok().and_then(|mut g| g.take());
        if let Some(job) = job {
            save_note_page_pixels(
                &self.data_dir,
                job.note_id,
                job.doc_page,
                job.width,
                job.height,
                &job.pixels.data,
            );
            save_note_page_strokes(&self.data_dir, job.note_id, job.doc_page, &job.strokes);
        }
    }
}

/// Wakes the event loop when the Linux evdev pen thread has samples ready.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // constructed only on Linux via evdev pen wake
enum AppUserEvent {
    PenReady,
}

/// Commands from UI to async core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Ping,
    Shutdown,
}

/// Events from async core to UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Pong,
    Ready,
}

/// Main application controller.
pub struct App {
    version: String,
    #[allow(dead_code)]
    rt: Runtime,
    cmd_tx: UnboundedSender<Command>,
    evt_rx: Receiver<Event>,
}

impl App {
    /// Create a new App instance with async runtime and channel pair.
    pub fn new() -> ephemeris_core::Result<Self> {
        tracing::info!("Initializing Ephemeris app");

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("ephemeris-core")
            .enable_all()
            .build()?;

        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Command>();
        let (evt_tx, evt_rx) = channel::<Event>();
        let evt_tx_clone = evt_tx.clone();

        rt.spawn(async move {
            tracing::info!("Async core task started");
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    Command::Ping => {
                        tracing::debug!("Received ping, sending pong");
                        let _ = evt_tx_clone.send(Event::Pong);
                    }
                    Command::Shutdown => {
                        tracing::info!("Shutdown command received");
                        break;
                    }
                }
            }
            tracing::info!("Async core task ended");
        });

        let _ = evt_tx.send(Event::Ready);

        Ok(App {
            version: env!("CARGO_PKG_VERSION").to_string(),
            rt,
            cmd_tx,
            evt_rx,
        })
    }

    /// Run the application.
    pub fn run(&self) -> ephemeris_core::Result<()> {
        tracing::info!("Ephemeris {} started", self.version);
        tracing::debug!("App is running");
        Ok(())
    }

    /// Send a command to the async core.
    pub fn send_command(&self, cmd: Command) -> std::result::Result<(), String> {
        self.cmd_tx
            .send(cmd)
            .map_err(|_| "Failed to send command".to_string())
    }

    /// Receive an event from the async core (non-blocking).
    pub fn try_recv_event(&self) -> Option<Event> {
        self.evt_rx.try_recv().ok()
    }
}

impl Default for App {
    fn default() -> Self {
        App::new().expect("Failed to create default App")
    }
}

impl App {
    /// Launch the live winit window.
    ///
    /// Loads `Config` from `$XDG_CONFIG_HOME/ephemeris/ephemeris.toml` (falls
    /// back to defaults if absent).  When `vault_path` is set in the config,
    /// tasks are read from the Obsidian vault; otherwise sample tasks are shown.
    /// When `ics_paths` is set, calendar events are loaded from the listed `.ics`
    /// files and displayed in the agenda section.
    pub fn run_windowed(self) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let event_loop = EventLoop::<AppUserEvent>::with_user_event().build()?;
        // Design-resolution UI (not native panel pixels). Softbuffer upscales to
        // the maximized physical window so chrome/icons stay usable on PineNote.
        // Override with EPHEMERIS_SIZE=WxH when needed.
        let (win_w, win_h) = probe_window_size();
        tracing::info!("UI size (design): {win_w}×{win_h}");

        // Load config — silently fall back to defaults if file is absent or malformed.
        let config = Config::load().unwrap_or_default();
        tracing::info!(
            "Config loaded: theme={}, vault={:?}, ics_paths={}, ics_urls={}",
            config.theme,
            config.vault_path,
            config.ics_paths.len(),
            config.ics_urls.len()
        );

        let ui = Rc::new(EphemerisUi::new(win_w, win_h)?);
        ui.set_page_title("Ephemeris");
        ui.apply_theme(&config.theme);
        ui.set_connect_setup_export_subdir(&config.vault_export_subdir.to_string_lossy());
        ui.set_connect_setup_git_enabled(config.vault_git_enabled);
        if let Some(ref vault) = config.vault_path {
            ui.set_connect_setup_git_detected(ephemeris_core::is_git_repo(vault));
            ui.set_settings_vault_path(&vault.to_string_lossy());
        }

        // ── Profiles ──────────────────────────────────────────────────────────
        let profile_mgr = ProfileManager::new();

        // Load persisted profiles or create defaults on first run.
        let (work_pid, personal_pid) = if let Some(idx) = load_profiles_index() {
            for profile in idx.profiles {
                profile_mgr.load_profile(profile);
            }
            let profiles = profile_mgr.list_profiles().unwrap_or_default();
            let first = profiles.first().map(|p| p.id).unwrap_or_default();
            let second = profiles.get(1).map(|p| p.id).unwrap_or(first);
            (first, second)
        } else {
            let work_profile = profile_mgr
                .create_profile_with_icon("Work", "work")
                .expect("create Work profile");
            let personal_profile = profile_mgr
                .create_profile_with_icon("Personal", "person")
                .expect("create Personal profile");
            save_profiles_index(&profile_mgr);
            (work_profile.id, personal_profile.id)
        };

        let profile_id = work_pid; // vault/ICS events tagged to Work by default
                                   // "" = All profiles, UUID string = specific profile
        let active_profile_filter: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));

        let config: Rc<RefCell<Config>> = Rc::new(RefCell::new(config));

        // Vault FTS index (rebuilt when vault set changes; reused on search open).
        let vault_fts: Rc<RefCell<VaultFtsIndex>> = Rc::new(RefCell::new(
            VaultFtsIndex::open_in_memory().unwrap_or_else(|e| {
                tracing::error!("Failed to open vault FTS index: {e}");
                // open_in_memory only fails on catastrophic sqlite errors; retry once.
                VaultFtsIndex::open_in_memory().expect("vault FTS index")
            }),
        ));
        // Paths last successfully indexed into `vault_fts` (skip reindex when unchanged).
        let vault_fts_paths: Rc<RefCell<Vec<PathBuf>>> = Rc::new(RefCell::new(Vec::new()));

        // ── Connections (per-profile assignment) ─────────────────────────────
        // Load before tasks/FTS so a persisted Work vault is available at startup.
        let default_conn_profile = work_pid.0.to_string();
        let init_connections: Vec<AppConnection> = {
            if let Some(saved) = load_connections() {
                tracing::info!("Loaded {} connection(s) from connections.json", saved.len());
                saved
            } else {
                let cfg = config.borrow();
                let mut conns: Vec<AppConnection> = Vec::new();
                if let Some(ref vault) = cfg.vault_path {
                    conns.push(AppConnection {
                        id: 1,
                        kind: "vault".to_string(),
                        display: vault
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| vault.to_string_lossy().to_string()),
                        value: vault.to_string_lossy().to_string(),
                        profile_id: default_conn_profile.clone(),
                        allow_in_aggregated: true,
                        username: String::new(),
                        password: String::new(),
                    });
                }
                let base = conns.len() as u32;
                for (i, path) in cfg.ics_paths.iter().enumerate() {
                    conns.push(AppConnection {
                        id: base + i as u32 + 1,
                        kind: "calendar".to_string(),
                        display: path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| path.to_string_lossy().to_string()),
                        value: path.to_string_lossy().to_string(),
                        profile_id: default_conn_profile.clone(),
                        allow_in_aggregated: true,
                        username: String::new(),
                        password: String::new(),
                    });
                }
                let base2 = conns.len() as u32;
                for (i, url) in cfg.ics_urls.iter().enumerate() {
                    let disp = url
                        .strip_prefix("webcal://")
                        .or_else(|| url.strip_prefix("https://"))
                        .unwrap_or(url.as_str())
                        .chars()
                        .take(40)
                        .collect::<String>();
                    conns.push(AppConnection {
                        id: base2 + i as u32 + 1,
                        kind: "calendar".to_string(),
                        display: disp,
                        value: url.clone(),
                        profile_id: default_conn_profile.clone(),
                        allow_in_aggregated: true,
                        username: String::new(),
                        password: String::new(),
                    });
                }
                conns
            }
        };
        // Sync legacy config.vault_path from the Work (or first) vault connection.
        if config.borrow().vault_path.is_none() {
            if let Some(v) = init_connections
                .iter()
                .find(|c| c.kind == "vault" && c.profile_id == default_conn_profile)
                .or_else(|| init_connections.iter().find(|c| c.kind == "vault"))
            {
                let path = expand_user_path(&v.value);
                if path.is_dir() {
                    config.borrow_mut().vault_path = Some(path.clone());
                    tracing::info!(
                        "Synced config.vault_path from connection: {}",
                        path.display()
                    );
                } else {
                    tracing::warn!("Vault connection value {:?} is not a directory", v.value);
                }
            }
        }
        if let Some(ref vault) = config.borrow().vault_path.clone() {
            let exclude = config.borrow().vault_export_subdir.clone();
            match vault_fts
                .borrow_mut()
                .reindex_vault(vault, Some(exclude.as_path()))
            {
                Ok(n) => {
                    tracing::info!("Indexed {n} vault notes for search");
                    *vault_fts_paths.borrow_mut() = vec![vault.clone()];
                }
                Err(e) => tracing::warn!("Vault FTS reindex failed: {e}"),
            }
            schedule_vault_git_pull(&config.borrow());
        }
        let connections: Rc<RefCell<Vec<AppConnection>>> = Rc::new(RefCell::new(init_connections));
        save_connections(&connections.borrow());

        // ── Tasks ─────────────────────────────────────────────────────────────
        // Load from Obsidian vault when configured; fall back to demo data.
        let initial_tasks = {
            let cfg = config.borrow();
            if let Some(ref vault) = cfg.vault_path {
                match load_vault_tasks_with_inbox(
                    vault,
                    &cfg.vault_tasks_inbox,
                    &cfg.vault_export_subdir,
                    profile_id,
                ) {
                    Ok(ts) => {
                        tracing::info!("Loaded {} tasks from vault {}", ts.len(), vault.display());
                        ts
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Vault task load failed for {}: {e}; using sample tasks",
                            vault.display()
                        );
                        sample_tasks_split(work_pid, personal_pid)
                    }
                }
            } else {
                sample_tasks_split(work_pid, personal_pid)
            }
        };

        // Local + handwritten-checkbox task state (ephemeris-rjz): restored before
        // the note-task startup sync so it sees persisted ticks and ink_seen.
        let task_dir = note_data_dir();
        let persist = Rc::new(TaskPersist::new(&task_dir));
        let mut initial_tasks = initial_tasks;
        if let Some(store) = load_local_task_store(&task_dir) {
            *persist.state.borrow_mut() = store.apply(&mut initial_tasks);
        }
        let tasks: Rc<RefCell<Vec<Task>>> = Rc::new(RefCell::new(initial_tasks));

        // ── Calendar events ───────────────────────────────────────────────────
        let now = now_secs();
        let calendar_events: Rc<RefCell<Vec<CalendarEvent>>> = {
            let cfg = config.borrow();
            let mut evts = load_ics_events(&cfg.ics_paths, profile_id, now);
            evts.extend(load_ics_from_urls(&cfg.ics_urls, profile_id, now));
            evts.extend(load_caldav_events(&connections.borrow(), profile_id, now));
            Rc::new(RefCell::new(evts))
        };

        // Counter for auto-numbering tasks added via the dialog (no keyboard).
        let new_task_n: Rc<Cell<u32>> = Rc::new(Cell::new(1));

        // ── Note & notebook persistence ───────────────────────────────────────
        let data_dir = note_data_dir();
        let _ = std::fs::create_dir_all(&data_dir);
        calendar_events
            .borrow_mut()
            .extend(load_local_events(&data_dir));

        // User HTR lexicon from vault / tasks / calendar (background, ≤ daily).
        {
            let vault_paths = {
                let indexed = vault_fts_paths.borrow().clone();
                if !indexed.is_empty() {
                    indexed
                } else {
                    config
                        .borrow()
                        .vault_path
                        .clone()
                        .into_iter()
                        .collect::<Vec<_>>()
                }
            };
            start_user_htr_lexicon_refresh(
                &data_dir,
                &config.borrow(),
                &vault_paths,
                &tasks.borrow(),
                &calendar_events.borrow(),
            );
        }

        let (init_notes, init_notebooks, note_seq_init, notebook_seq_init) =
            match load_notes_index(&data_dir) {
                Some(idx) => (
                    idx.notes,
                    idx.notebooks,
                    idx.note_id_seq,
                    idx.notebook_id_seq,
                ),
                None => (
                    vec![AppNote {
                        id: 1,
                        title: "Note 1".to_string(),
                        page_index: 0,
                        created_at: now,
                        updated_at: now,
                        notebook_id: Some(1),
                        paper_template: ephemeris_core::PageTemplate::Blank,
                        anchor: None,
                        profile_id: String::new(),
                        text_content: None,
                        doc_page: 0,
                        doc_page_count: 1,
                        tags: vec![],
                    }],
                    vec![AppNotebook {
                        id: 1,
                        title: "Inbox".to_string(),
                        profile_id: String::new(),
                        parent_id: None,
                    }],
                    2u32,
                    2u32,
                ),
            };

        // Counter for auto-numbering new notes.
        let note_id_seq: Rc<Cell<u32>> = Rc::new(Cell::new(note_seq_init));

        // Counter for auto-numbering new notebooks.
        let notebook_id_seq: Rc<Cell<u32>> = Rc::new(Cell::new(notebook_seq_init));

        // Active folder (0 = root notes level).
        let active_notebook_id: Rc<Cell<u32>> = Rc::new(Cell::new(0));

        // Profile picked via the create-profile picker for the current recording session.
        // Cleared after the recording stops.
        let rec_pending_profile: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));

        // Notebooks and notes (restored from disk or defaults).
        let notebooks: Rc<RefCell<Vec<AppNotebook>>> = Rc::new(RefCell::new(init_notebooks));
        let notes: Rc<RefCell<Vec<AppNote>>> = Rc::new(RefCell::new(init_notes));
        // Deferred vault OCR/PDF (leave-note + 10‑minute batch).
        let vault_export: Rc<VaultExportQueue> = Rc::new(VaultExportQueue::new());
        // Debounced local page .bin / strokes JSON writes.
        let local_page_writes: Rc<LocalPageWriteQueue> =
            Rc::new(LocalPageWriteQueue::new(data_dir.clone()));
        // Idle per-line HTR cache + low-priority background worker (ktg.12).
        let bg_recog: Rc<BackgroundRecogQueue> = Rc::new(BackgroundRecogQueue::new());

        // Restore ink pixels + stroke lists for all known notes (current doc page).
        for note in notes.borrow().iter() {
            if let Some((w, h, pixels)) = load_note_page_n(&data_dir, note.id, note.doc_page) {
                ui.restore_page_pixels(note.page_index, w, h, pixels);
            }
            if let Some(strokes) = load_note_page_strokes(&data_dir, note.id, note.doc_page) {
                // Prefer stroke list as source of truth: rebuild raster to match.
                ui.restore_page_strokes(note.page_index, strokes);
            }
        }
        ui.clear_strokes_dirty();

        // ── Obsidian Canvas (spatial boards) — wired after profile_mgr Rc ──────
        let canvas_session = Rc::new(crate::canvas_app::CanvasSession::load());
        *canvas_session.profile_filter.borrow_mut() = active_profile_filter.borrow().clone();
        let canvas_focus_page = notes
            .borrow()
            .iter()
            .map(|n| n.page_index)
            .max()
            .map(|m| m + 1)
            .unwrap_or(0);

        // ── Microphone detection & recordings ─────────────────────────────────
        let device_has_mic = has_microphone();
        ui.clone_component().set_has_mic(device_has_mic);

        let rec_dir = recording_data_dir();
        let _ = std::fs::create_dir_all(&rec_dir);

        let (init_recordings, rec_seq_init) = match load_recordings_index(&rec_dir) {
            Some(idx) => (idx.recordings, idx.rec_id_seq),
            None => (vec![], 1u32),
        };
        let rec_id_seq: Rc<Cell<u32>> = Rc::new(Cell::new(rec_seq_init));
        let recordings: Rc<RefCell<Vec<AppRecording>>> = Rc::new(RefCell::new(init_recordings));
        let rec_handle: Rc<RefCell<Option<RecordingHandle>>> = Rc::new(RefCell::new(None));
        let transcription_pending: Arc<Mutex<Vec<(u32, String)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let transcription_active = Arc::new(AtomicBool::new(false));
        // FIFO of recording ids waiting for whisper (plus the one currently running).
        let transcription_queue: Arc<Mutex<VecDeque<u32>>> = Arc::new(Mutex::new(VecDeque::new()));
        // Ids queued or in-flight — drives list "Transcribing…" chips.
        let transcription_in_flight: Rc<RefCell<HashSet<u32>>> =
            Rc::new(RefCell::new(HashSet::new()));
        let download_result: Arc<Mutex<Option<Result<(), String>>>> = Arc::new(Mutex::new(None));
        let download_active = Arc::new(AtomicBool::new(false));
        let htr_download_result: Arc<Mutex<Option<Result<(), String>>>> =
            Arc::new(Mutex::new(None));
        let htr_download_active = Arc::new(AtomicBool::new(false));
        let line_htr_download_result: Arc<Mutex<Option<Result<(), String>>>> =
            Arc::new(Mutex::new(None));
        let line_htr_download_active = Arc::new(AtomicBool::new(false));
        // See-text overlay generation — bump on open/close so stale UI updates drop.
        let see_text_gen = Arc::new(AtomicU64::new(0));
        // Completed CalDAV sync batches waiting to merge into the agenda.
        let caldav_pending: Arc<Mutex<Vec<CalendarEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let caldav_active = Arc::new(AtomicBool::new(false));

        // ── Initial UI state ──────────────────────────────────────────────────
        {
            let cfg = config.borrow();
            let page = ui.clone_component();
            page.set_task_engage_lane(cfg.task_engage.engage_lane.as_str().into());
            // Default Manage filter to All so Obsidian tasks without #lane/* still show.
            page.set_task_lane_filter("".into());
            refresh_ui(&ui, &tasks.borrow(), profile_id, &profile_mgr, &cfg);
        }
        push_agenda_ui(&ui, &calendar_events.borrow(), AgendaRange::Day, now);
        ui.set_show_start_page(true);
        ui.set_nav_section(NAV_HOME);

        // ── Profile list ──────────────────────────────────────────────────────
        let profile_mgr = Rc::new(RefCell::new(profile_mgr));

        crate::canvas_app::wire_canvas(
            ui.clone(),
            canvas_session.clone(),
            config.clone(),
            connections.clone(),
            active_profile_filter.clone(),
            profile_mgr.clone(),
            canvas_focus_page,
        );

        // Track which profile IDs have notes_in_all = true (for note filtering).
        let notes_in_all_profiles: Rc<RefCell<HashSet<String>>> = {
            let set: HashSet<String> = profile_mgr
                .borrow()
                .list_profiles()
                .unwrap_or_default()
                .iter()
                .filter(|p| p.notes_in_all)
                .map(|p| p.id.0.to_string())
                .collect();
            Rc::new(RefCell::new(set))
        };

        // Track which profile IDs have recordings_in_all = true (for recording filtering).
        let recordings_in_all_profiles: Rc<RefCell<HashSet<String>>> = {
            let set: HashSet<String> = profile_mgr
                .borrow()
                .list_profiles()
                .unwrap_or_default()
                .iter()
                .filter(|p| p.recordings_in_all)
                .map(|p| p.id.0.to_string())
                .collect();
            Rc::new(RefCell::new(set))
        };

        let push_profiles: Rc<dyn Fn()> = {
            let mgr = profile_mgr.clone();
            let ui2 = ui.clone_component();
            Rc::new(move || {
                let entries: Vec<ephemeris_ui::ProfileEntry> = mgr
                    .borrow()
                    .list_profiles()
                    .unwrap_or_default()
                    .iter()
                    .map(|p| {
                        let (has_custom, custom_icon) = load_custom_profile_icon(&p.icon);
                        ephemeris_ui::ProfileEntry {
                            id: p.id.0.to_string().as_str().into(),
                            name: p.name.as_str().into(),
                            icon: p.icon.as_str().into(),
                            icon_char: profile_icon_char(&p.icon).into(),
                            has_custom_icon: has_custom,
                            custom_icon,
                            notes_in_all: p.notes_in_all,
                            recordings_in_all: p.recordings_in_all,
                        }
                    })
                    .collect();
                ui2.set_profile_list(Rc::new(slint::VecModel::from(entries)).into());
            })
        };
        push_profiles();

        // ── Connection list push helper ───────────────────────────────────────
        let push_connections: Rc<dyn Fn()> = {
            let conns_rc = connections.clone();
            let mgr_rc = profile_mgr.clone();
            let ui2 = ui.clone_component();
            Rc::new(move || {
                let mgr = mgr_rc.borrow();
                let connect_pid = ui2.get_connect_profile_id().to_string();
                let conns = conns_rc.borrow();
                let entries: Vec<ephemeris_ui::ConnectionEntry> = conns
                    .iter()
                    .map(|c| {
                        let pname = resolve_profile_name(&mgr, &c.profile_id);
                        ephemeris_ui::ConnectionEntry {
                            id: c.id.to_string().as_str().into(),
                            kind: c.kind.as_str().into(),
                            display: c.display.as_str().into(),
                            value: c.value.as_str().into(),
                            profile_id: c.profile_id.as_str().into(),
                            profile_name: pname.as_str().into(),
                            allow_in_aggregated: c.allow_in_aggregated,
                        }
                    })
                    .collect();
                let has_vault = conns
                    .iter()
                    .any(|c| c.kind == "vault" && c.profile_id == connect_pid);
                let cal_count = conns
                    .iter()
                    .filter(|c| {
                        (c.kind == "calendar" || c.kind == "caldav") && c.profile_id == connect_pid
                    })
                    .count() as i32;
                drop(conns);
                ui2.set_connection_list(Rc::new(slint::VecModel::from(entries)).into());
                ui2.set_connect_has_vault(has_vault);
                ui2.set_connect_calendar_count(cal_count);
            })
        };
        push_connections();

        // Seed Connect tab onto Work (or first) profile — never "All".
        {
            let ui2 = ui.clone_component();
            ui2.set_connect_profile_id(default_conn_profile.as_str().into());
            push_connections();
        }

        // ── Recording list push helper (profile-filtered) ─────────────────────
        let push_recordings: Rc<dyn Fn()> = {
            let recs_rc = recordings.clone();
            let filter_rc = active_profile_filter.clone();
            let ria_rc = recordings_in_all_profiles.clone();
            let inflight_rc = transcription_in_flight.clone();
            let mgr_rc = profile_mgr.clone();
            let ui2 = ui.clone_component();
            Rc::new(move || {
                let filter = filter_rc.borrow().clone();
                let ria = ria_rc.borrow();
                let inflight = inflight_rc.borrow();
                let search = ui2.get_rec_search_query().to_string();
                let sort = ui2.get_rec_sort().to_string();
                let mut visible: Vec<AppRecording> = recs_rc
                    .borrow()
                    .iter()
                    .filter(|r| {
                        if filter.is_empty() {
                            // All view: unassigned + profiles opted in to aggregator
                            r.profile_id.is_empty() || ria.contains(&r.profile_id)
                        } else {
                            // Specific profile: only exact matches (no legacy leakage)
                            r.profile_id == filter
                        }
                    })
                    .cloned()
                    .collect();
                drop(ria);
                visible = filter_recordings_by_query(&visible, &search);
                sort_recordings(&mut visible, &sort);
                push_recording_list_comp(
                    &ui2,
                    &visible,
                    &inflight,
                    filter.is_empty(),
                    &mgr_rc.borrow(),
                );
            })
        };

        // Refresh folder/note lists for the current `active_notebook_id` (0 = root).
        let push_notebooks: Rc<dyn Fn()> = {
            let nbs_rc = notebooks.clone();
            let notes_rc = notes.clone();
            let filter_rc = active_profile_filter.clone();
            let nia_rc = notes_in_all_profiles.clone();
            let active_nb = active_notebook_id.clone();
            let data_dir_pb = data_dir.clone();
            let mgr_pb = profile_mgr.clone();
            let ui_rc = ui.clone();
            let ui2_pb = ui.clone_component();
            Rc::new(move || {
                let filter = filter_rc.borrow().clone();
                let nia = nia_rc.borrow();
                let parent = active_nb.get();
                let sort_mode = ui2_pb.get_notes_sort_mode().to_string();
                let search_q = ui2_pb.get_notes_search_query().to_string();
                let mgr = mgr_pb.borrow();
                let all =
                    build_notebook_list(&nbs_rc.borrow(), &notes_rc.borrow(), &filter, &nia, &mgr);
                let folders = build_folder_list(
                    &nbs_rc.borrow(),
                    &notes_rc.borrow(),
                    parent,
                    &filter,
                    &nia,
                    &mgr,
                );
                let note_entries = build_note_list(
                    &notes_rc.borrow(),
                    parent,
                    now_secs(),
                    &filter,
                    &nia,
                    &data_dir_pb,
                    &mgr,
                    &sort_mode,
                    &search_q,
                );
                drop(nia);
                drop(mgr);
                ui_rc.set_notebook_list(&all);
                ui_rc.set_folder_list(&folders);
                ui_rc.set_note_list(&note_entries);
                let at_root = parent == 0;
                ui_rc.set_notes_at_root(at_root);
                if at_root {
                    ui_rc.set_active_notebook("0", "Notes");
                } else {
                    let title = nbs_rc
                        .borrow()
                        .iter()
                        .find(|nb| nb.id == parent)
                        .map(|nb| nb.title.clone())
                        .unwrap_or_else(|| format!("Folder {parent}"));
                    ui_rc.set_active_notebook(&parent.to_string(), &title);
                }
            })
        };

        // ── Profile add ───────────────────────────────────────────────────────
        {
            let mgr = profile_mgr.clone();
            let push = push_profiles.clone();
            ui.on_settings_profile_add(move |name, icon| {
                let name = name.to_string();
                let icon = icon.to_string();
                if !name.trim().is_empty() {
                    mgr.borrow()
                        .create_profile_with_icon(name.trim(), icon.trim())
                        .expect("create profile");
                    save_profiles_index(&mgr.borrow());
                    push();
                }
            });
        }

        // ── Profile edit ──────────────────────────────────────────────────────
        {
            let mgr = profile_mgr.clone();
            let push = push_profiles.clone();
            ui.on_settings_profile_edit(move |id, name, icon| {
                let id_str = id.to_string();
                let name = name.to_string();
                let icon = icon.to_string();
                let m = mgr.borrow();
                if !name.trim().is_empty() {
                    let _ = m.rename_profile_by_str(&id_str, name.trim());
                }
                let _ = m.set_profile_icon_by_str(&id_str, icon.trim());
                drop(m);
                save_profiles_index(&mgr.borrow());
                push();
            });
        }

        // ── Profile delete ────────────────────────────────────────────────────
        {
            let mgr = profile_mgr.clone();
            let push = push_profiles.clone();
            let nia = notes_in_all_profiles.clone();
            ui.on_settings_profile_delete(move |id| {
                let id_str = id.to_string();
                let _ = mgr.borrow().delete_profile_by_str(&id_str);
                nia.borrow_mut().remove(&id_str);
                save_profiles_index(&mgr.borrow());
                push();
            });
        }

        // ── Profile notes-in-all toggle ───────────────────────────────────────
        {
            let mgr = profile_mgr.clone();
            let push = push_profiles.clone();
            let nia = notes_in_all_profiles.clone();
            let push_nbs_nia = push_notebooks.clone();
            ui.on_settings_profile_toggle_notes_in_all(move |id| {
                let id_str = id.to_string();
                let m = mgr.borrow();
                let profiles = m.list_profiles().unwrap_or_default();
                if let Some(p) = profiles.iter().find(|p| p.id.0.to_string() == id_str) {
                    let new_val = !p.notes_in_all;
                    let _ = m.set_profile_notes_in_all_by_str(&id_str, new_val);
                    if new_val {
                        nia.borrow_mut().insert(id_str);
                    } else {
                        nia.borrow_mut().remove(&id_str);
                    }
                }
                drop(m);
                save_profiles_index(&mgr.borrow());
                push();
                push_nbs_nia();
            });
        }

        // ── Profile recordings-in-all toggle ─────────────────────────────────
        {
            let mgr = profile_mgr.clone();
            let push = push_profiles.clone();
            let ria = recordings_in_all_profiles.clone();
            let push_recs_ria = push_recordings.clone();
            ui.on_settings_profile_toggle_recordings_in_all(move |id| {
                let id_str = id.to_string();
                let m = mgr.borrow();
                let profiles = m.list_profiles().unwrap_or_default();
                if let Some(p) = profiles.iter().find(|p| p.id.0.to_string() == id_str) {
                    let new_val = !p.recordings_in_all;
                    let _ = m.set_profile_recordings_in_all_by_str(&id_str, new_val);
                    if new_val {
                        ria.borrow_mut().insert(id_str);
                    } else {
                        ria.borrow_mut().remove(&id_str);
                    }
                }
                drop(m);
                save_profiles_index(&mgr.borrow());
                push();
                push_recs_ria();
            });
        }

        let profile_mgr = profile_mgr.borrow().clone();

        // ── Agenda range tabs ─────────────────────────────────────────────────
        {
            let cal2 = calendar_events.clone();
            let ui2 = ui.clone_component();
            let filter_rc = active_profile_filter.clone();
            ui.on_agenda_range_changed(move |range_idx| {
                let range = int_to_range(range_idx);
                let now = now_secs();
                let filter_str = filter_rc.borrow().clone();
                let visible = filter_events_by_profile(&cal2.borrow(), &filter_str);
                push_agenda_comp(&ui2, &visible, range, now);
            });
        }

        // ── Profile switcher ─────────────────────────────────────────────────
        {
            let tasks2 = tasks.clone();
            let cal2 = calendar_events.clone();
            let ui2 = ui.clone_component();
            let filter_rc = active_profile_filter.clone();
            let mgr2 = profile_mgr.clone();
            let cfg2 = config.clone();
            let push_recs_switch = push_recordings.clone();
            let push_nbs_switch = push_notebooks.clone();
            let canvas_switch = canvas_session.clone();
            let ui_canvas = ui.clone();
            ui.on_profile_changed(move |id| {
                let id_str = id.to_string();
                *filter_rc.borrow_mut() = id_str.clone();
                let filter_str = filter_rc.borrow().clone();
                // Task-view chips are local to All; clear when leaving aggregator.
                if !filter_str.is_empty() {
                    ui2.set_task_profile_filter("".into());
                }
                let now = now_secs();
                let filtered_tasks = filter_tasks_by_profile(&tasks2.borrow(), &filter_str);
                refresh_comp(&ui2, &filtered_tasks, work_pid, &mgr2, &cfg2.borrow());
                let range = int_to_range(ui2.get_agenda_range());
                let visible_events = filter_events_by_profile(&cal2.borrow(), &filter_str);
                push_agenda_comp(&ui2, &visible_events, range, now);
                set_active_profile_icon(&ui2, &mgr2, &id_str);
                push_recs_switch();
                push_nbs_switch();
                *canvas_switch.profile_filter.borrow_mut() = filter_str;
                crate::canvas_app::refresh_browser(&ui_canvas, &canvas_switch);
            });
        }

        // ── Hand-drawn checkboxes → tasks (ephemeris-fmt.11) ──────────────────
        // Reconciles a page's checkbox lines into `tasks` (source "note") and
        // refreshes the task view when anything changed. In-memory like all
        // non-vault tasks; rebuilt below from the on-disk caches at startup.
        let sync_note_tasks: SyncNoteTasksFn = {
            let tasks2 = tasks.clone();
            let notes2 = notes.clone();
            let ui2 = ui.clone_component();
            let filter_rc = active_profile_filter.clone();
            let mgr2 = profile_mgr.clone();
            let cfg2 = config.clone();
            let persist2 = persist.clone();
            Rc::new(move |note_id, doc_page, cache| {
                let profile = notes2
                    .borrow()
                    .iter()
                    .find(|n| n.id == note_id)
                    .and_then(|n| uuid::Uuid::parse_str(&n.profile_id).ok())
                    .map(ProfileId)
                    .unwrap_or(work_pid);
                let changed = ephemeris_core::sync_page_tasks(
                    &mut tasks2.borrow_mut(),
                    &mut persist2.state.borrow_mut(),
                    note_id,
                    doc_page,
                    profile,
                    cache,
                );
                if changed {
                    persist2.save(&tasks2.borrow());
                    let filter_str = filter_rc.borrow().clone();
                    let filtered = filter_tasks_by_profile(&tasks2.borrow(), &filter_str);
                    refresh_comp(&ui2, &filtered, work_pid, &mgr2, &cfg2.borrow());
                }
            })
        };
        {
            // Drop restored note tasks whose note no longer exists.
            let live: std::collections::HashSet<u32> =
                notes.borrow().iter().map(|n| n.id).collect();
            tasks.borrow_mut().retain(|t| {
                t.source != ephemeris_core::NOTE_TASK_SOURCE
                    || t.source_ref
                        .as_deref()
                        .and_then(ephemeris_core::parse_note_source_ref)
                        .is_none_or(|(id, _)| live.contains(&id))
            });
            let snapshot: Vec<(u32, usize)> = notes
                .borrow()
                .iter()
                .flat_map(|n| (0..n.doc_page_count.max(1)).map(move |p| (n.id, p)))
                .collect();
            for (note_id, doc_page) in snapshot {
                if let Some(cache) = PageRecognitionCache::load(&data_dir, note_id, doc_page) {
                    sync_note_tasks(note_id, doc_page, &cache);
                }
            }
            persist.save(&tasks.borrow());
        }

        // ── Connection management ─────────────────────────────────────────────

        // Connect-tab profile selection (does not change the app-wide filter)
        {
            let push = push_connections.clone();
            let ui2 = ui.clone_component();
            ui.on_connect_profile_changed(move |id| {
                let id_str = id.to_string();
                // If empty (e.g. no profiles yet), clear status flags.
                if id_str.is_empty() {
                    ui2.set_connect_has_vault(false);
                    ui2.set_connect_calendar_count(0);
                } else {
                    ui2.set_connect_profile_id(id_str.as_str().into());
                }
                push();
            });
        }

        // Remove a connection by index
        {
            let conns_rc = connections.clone();
            let push = push_connections.clone();
            ui.on_connection_remove(move |idx| {
                let mut conns = conns_rc.borrow_mut();
                let i = idx as usize;
                if i < conns.len() {
                    conns.remove(i);
                }
                save_connections(&conns);
                drop(conns);
                push();
            });
        }

        // Cycle profile assignment for a connection
        {
            let conns_rc = connections.clone();
            let push = push_connections.clone();
            let mgr3 = profile_mgr.clone();
            ui.on_connection_cycle_profile(move |idx| {
                let mut conns = conns_rc.borrow_mut();
                let i = idx as usize;
                if i >= conns.len() {
                    return;
                }
                let profiles = mgr3.list_profiles().unwrap_or_default();
                // Cycle: "" → profile[0] → profile[1] → ... → back to ""
                let current = &conns[i].profile_id;
                let next_id = if current.is_empty() {
                    profiles
                        .first()
                        .map(|p| p.id.0.to_string())
                        .unwrap_or_default()
                } else {
                    let pos = profiles.iter().position(|p| p.id.0.to_string() == *current);
                    match pos {
                        Some(p) if p + 1 < profiles.len() => profiles[p + 1].id.0.to_string(),
                        _ => String::new(), // wrap back to "All"
                    }
                };
                conns[i].profile_id = next_id;
                save_connections(&conns);
                drop(conns);
                push();
            });
        }

        // Set profile for a specific connection
        {
            let conns_rc = connections.clone();
            let push = push_connections.clone();
            let vault_ex = vault_export.clone();
            ui.on_connection_set_profile(move |idx, profile_id| {
                let mut conns = conns_rc.borrow_mut();
                let i = idx as usize;
                let mut reassigned_vault = false;
                if i < conns.len() {
                    reassigned_vault = conns[i].kind == "vault";
                    conns[i].profile_id = profile_id.to_string();
                }
                save_connections(&conns);
                drop(conns);
                push();
                // Vault moved onto a (possibly new) profile → backfill its notes.
                if reassigned_vault {
                    vault_ex.mark_full_sync();
                    vault_ex.request_flush_soon();
                }
            });
        }

        // Toggle allow_in_aggregated for a connection
        {
            let conns_rc = connections.clone();
            let push = push_connections.clone();
            ui.on_connection_toggle_aggregated(move |idx| {
                let mut conns = conns_rc.borrow_mut();
                let i = idx as usize;
                if i < conns.len() {
                    conns[i].allow_in_aggregated = !conns[i].allow_in_aggregated;
                }
                save_connections(&conns);
                drop(conns);
                push();
            });
        }

        // ── Ring "Task" → open create-task modal ──────────────────────────────
        {
            let mgr_ring_task = profile_mgr.clone();
            let ui2 = ui.clone_component();
            let tasks2 = tasks.clone();
            let cfg2 = config.clone();
            let filter_rc = active_profile_filter.clone();
            ui.on_ring_task(move || {
                {
                    let cfg = cfg2.borrow();
                    if let Some(ref vault) = cfg.vault_path {
                        schedule_vault_git_pull(&cfg);
                        reload_obsidian_tasks(
                            &mut tasks2.borrow_mut(),
                            vault,
                            &cfg.vault_tasks_inbox,
                            &cfg.vault_export_subdir,
                            work_pid,
                        );
                    }
                }
                ui2.set_task_filter("all".into());
                ui2.set_task_sort("priority".into());
                ui2.set_task_profile_filter("".into());
                ui2.set_task_search_query("".into());
                let filter_str = filter_rc.borrow().clone();
                let filtered = filter_tasks_by_profile(&tasks2.borrow(), &filter_str);
                push_full_task_list_comp(&ui2, &filtered, &mgr_ring_task);
                ui2.set_show_start_page(false);
                ui2.set_show_task_view(true);
                ui2.set_ring_visible(false);
                ui2.set_task_dialog_is_edit(false);
                ui2.set_task_dialog_id("".into());
                ui2.set_task_dialog_title("".into());
                ui2.set_task_dialog_priority(1);
                ui2.set_task_dialog_due("".into());
                ui2.set_task_dialog_tags("".into());
                ui2.set_task_dialog_visible(true);
            });
        }

        // ── Status-bar title tap → start page (home) ─────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_page_title_tapped(move || {
                ui2.set_show_task_view(false);
                ui2.set_show_settings(false);
                ui2.set_show_filebrowser(false);
                ui2.set_show_note_list(false);
                ui2.set_show_notebook_list(false);
                ui2.set_show_calendar(false);
                ui2.set_ring_visible(false);
                ui2.set_nav_section(NAV_HOME);
                ui2.set_show_start_page(true);
            });
        }

        // ── Nav rail: Home ────────────────────────────────────────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let canvas_leave = canvas_session.clone();
            let cfg_leave = config.clone();
            let conns_leave = connections.clone();
            ui.on_nav_overview_tapped(move || {
                crate::canvas_app::leave_canvas_session(
                    &ui_rc,
                    &canvas_leave,
                    &cfg_leave,
                    &conns_leave.borrow(),
                    false,
                );
                ui2.set_show_task_view(false);
                ui2.set_show_settings(false);
                ui2.set_show_filebrowser(false);
                ui2.set_show_note_list(false);
                ui2.set_show_notebook_list(false);
                ui2.set_show_calendar(false);
                ui2.set_show_recording_list(false);
                ui2.set_show_search(false);
                ui2.set_show_md_viewer(false);
                ui2.set_ring_visible(false);
                ui2.set_show_start_page(true);
            });
        }

        // ── Nav rail: Notes → show root notes browser ────────────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let push_nbs_nav = push_notebooks.clone();
            let active_nb_nav = active_notebook_id.clone();
            let notes_nav = notes.clone();
            let data_dir_nav = data_dir.clone();
            let vault_leave = vault_export.clone();
            let writes_leave = local_page_writes.clone();
            let canvas_leave = canvas_session.clone();
            let cfg_leave = config.clone();
            let conns_leave = connections.clone();
            ui.on_nav_notes_tapped(move || {
                // Save + queue vault export before leaving the canvas.
                leave_note_canvas(
                    &ui_rc,
                    &mut notes_nav.borrow_mut(),
                    &data_dir_nav,
                    ui2.get_active_note_id().as_str(),
                    &vault_leave,
                    &writes_leave,
                );
                crate::canvas_app::leave_canvas_session(
                    &ui_rc,
                    &canvas_leave,
                    &cfg_leave,
                    &conns_leave.borrow(),
                    false,
                );
                active_nb_nav.set(0);
                ui2.set_notes_search_query("".into());
                push_nbs_nav();
                ui2.set_show_start_page(false);
                ui2.set_show_task_view(false);
                ui2.set_show_settings(false);
                ui2.set_show_filebrowser(false);
                ui2.set_show_note_list(false);
                ui2.set_show_calendar(false);
                ui2.set_show_recording_list(false);
                ui2.set_show_search(false);
                ui2.set_show_md_viewer(false);
                ui2.set_ring_visible(false);
                ui2.set_show_notebook_list(true);
            });
        }

        // ── Folder list: tap a folder → drill into it ─────────────────────────
        {
            let ui2 = ui.clone_component();
            let active_nb2 = active_notebook_id.clone();
            let push_nbs_tap = push_notebooks.clone();
            ui.on_notebook_tapped(move |id| {
                let id_u32: u32 = id.parse().unwrap_or(0);
                if id_u32 == 0 {
                    return;
                }
                active_nb2.set(id_u32);
                push_nbs_tap();
                ui2.set_show_notebook_list(false);
                ui2.set_show_note_list(true);
            });
        }

        // ── Notes browser: back → parent folder (or root) ─────────────────────
        {
            let ui2 = ui.clone_component();
            let notebooks2 = notebooks.clone();
            let active_nb2 = active_notebook_id.clone();
            let push_nbs_back = push_notebooks.clone();
            ui.on_notebook_back_tapped(move || {
                let cur = active_nb2.get();
                let parent = notebooks2
                    .borrow()
                    .iter()
                    .find(|nb| nb.id == cur)
                    .and_then(|nb| nb.parent_id)
                    .unwrap_or(0);
                active_nb2.set(parent);
                push_nbs_back();
                ui2.set_show_note_list(parent != 0);
                ui2.set_show_notebook_list(parent == 0);
            });
        }

        // ── List / gallery toggle ─────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_notes_view_mode_toggled(move || {
                let cur = ui2.get_notes_view_mode().to_string();
                let next = if cur == "gallery" { "list" } else { "gallery" };
                ui2.set_notes_view_mode(next.into());
            });
        }

        // ── Sort cycle: edited → title → created ──────────────────────────────
        {
            let ui2 = ui.clone_component();
            let push_sort = push_notebooks.clone();
            ui.on_notes_sort_cycled(move || {
                let cur = ui2.get_notes_sort_mode().to_string();
                let next = match cur.as_str() {
                    "edited" => "title",
                    "title" => "created",
                    _ => "edited",
                };
                ui2.set_notes_sort_mode(next.into());
                push_sort();
            });
        }

        // ── Folder search filter ──────────────────────────────────────────────
        {
            let push_search = push_notebooks.clone();
            ui.on_notes_search_changed(move |_q| {
                push_search();
            });
        }

        // ── Delete note ───────────────────────────────────────────────────────
        {
            let notes2 = notes.clone();
            let notebooks2 = notebooks.clone();
            let note_seq2 = note_id_seq.clone();
            let nb_seq2 = notebook_id_seq.clone();
            let data_dir2 = data_dir.clone();
            let cfg2 = config.clone();
            let vault_ex = vault_export.clone();
            let bg_del = bg_recog.clone();
            let push_del = push_notebooks.clone();
            let ui2 = ui.clone_component();
            ui.on_note_delete_tapped(move |id| {
                let id_u32: u32 = id.parse().unwrap_or(0);
                if id_u32 == 0 {
                    return;
                }
                let page_count = notes2
                    .borrow()
                    .iter()
                    .find(|n| n.id == id_u32)
                    .map(|n| n.doc_page_count)
                    .unwrap_or(1);
                notes2.borrow_mut().retain(|n| n.id != id_u32);
                delete_note_page_files(&data_dir2, id_u32, page_count);
                bg_del.drop_note(id_u32, page_count);
                if ui2.get_active_note_id().as_str() == id.as_str() {
                    ui2.set_active_note_id("".into());
                }
                push_del();
                save_notes_index_and_sync_vault(
                    &data_dir2,
                    &notes2.borrow(),
                    &notebooks2.borrow(),
                    note_seq2.get(),
                    nb_seq2.get(),
                    &cfg2.borrow(),
                    &vault_ex,
                );
            });
        }

        // ── Delete folder (confirm when non-empty) ────────────────────────────
        let delete_folder_cascade: Rc<dyn Fn(u32)> = {
            let notes2 = notes.clone();
            let notebooks2 = notebooks.clone();
            let note_seq2 = note_id_seq.clone();
            let nb_seq2 = notebook_id_seq.clone();
            let data_dir2 = data_dir.clone();
            let cfg2 = config.clone();
            let vault_ex = vault_export.clone();
            let bg_del = bg_recog.clone();
            let active_nb2 = active_notebook_id.clone();
            let push_del = push_notebooks.clone();
            let ui2 = ui.clone_component();
            Rc::new(move |id_u32: u32| {
                if id_u32 == 0 {
                    return;
                }
                let parent_of_deleted = notebooks2
                    .borrow()
                    .iter()
                    .find(|nb| nb.id == id_u32)
                    .and_then(|nb| nb.parent_id)
                    .unwrap_or(0);
                let doomed = collect_folder_subtree(&notebooks2.borrow(), id_u32);
                let doomed_notes: Vec<u32> = notes2
                    .borrow()
                    .iter()
                    .filter(|n| n.notebook_id.is_some_and(|fid| doomed.contains(&fid)))
                    .map(|n| n.id)
                    .collect();
                let doomed_counts: Vec<(u32, usize)> = notes2
                    .borrow()
                    .iter()
                    .filter(|n| doomed_notes.contains(&n.id))
                    .map(|n| (n.id, n.doc_page_count))
                    .collect();
                notes2
                    .borrow_mut()
                    .retain(|n| !doomed_notes.contains(&n.id));
                for (nid, count) in doomed_counts {
                    delete_note_page_files(&data_dir2, nid, count);
                    bg_del.drop_note(nid, count);
                }
                notebooks2
                    .borrow_mut()
                    .retain(|nb| !doomed.contains(&nb.id));
                if doomed.contains(&active_nb2.get()) {
                    active_nb2.set(parent_of_deleted);
                    ui2.set_show_note_list(parent_of_deleted != 0);
                    ui2.set_show_notebook_list(parent_of_deleted == 0);
                }
                push_del();
                save_notes_index_and_sync_vault(
                    &data_dir2,
                    &notes2.borrow(),
                    &notebooks2.borrow(),
                    note_seq2.get(),
                    nb_seq2.get(),
                    &cfg2.borrow(),
                    &vault_ex,
                );
            })
        };

        {
            let notes2 = notes.clone();
            let notebooks2 = notebooks.clone();
            let ui_rc = ui.clone();
            let do_delete = delete_folder_cascade.clone();
            ui.on_notebook_delete_tapped(move |id| {
                let id_u32: u32 = id.parse().unwrap_or(0);
                if id_u32 == 0 {
                    return;
                }
                let nbs = notebooks2.borrow();
                let title = nbs
                    .iter()
                    .find(|nb| nb.id == id_u32)
                    .map(|nb| nb.title.clone())
                    .unwrap_or_else(|| "Folder".to_string());
                let doomed = collect_folder_subtree(&nbs, id_u32);
                let note_count = notes2
                    .borrow()
                    .iter()
                    .filter(|n| n.notebook_id.is_some_and(|fid| doomed.contains(&fid)))
                    .count();
                let folder_count = doomed.len().saturating_sub(1);
                drop(nbs);
                if note_count == 0 && folder_count == 0 {
                    do_delete(id_u32);
                    return;
                }
                let mut parts = Vec::new();
                if note_count > 0 {
                    parts.push(format!(
                        "{note_count} note{}",
                        if note_count == 1 { "" } else { "s" }
                    ));
                }
                if folder_count > 0 {
                    parts.push(format!(
                        "{folder_count} subfolder{}",
                        if folder_count == 1 { "" } else { "s" }
                    ));
                }
                let message = format!(
                    "This will permanently delete {} and all contents.",
                    parts.join(" and ")
                );
                ui_rc.show_notebook_delete_confirm(&id, &title, &message);
            });
        }

        {
            let do_delete = delete_folder_cascade.clone();
            let ui_rc = ui.clone();
            ui.on_notebook_delete_confirm(move |id| {
                ui_rc.hide_notebook_delete_confirm();
                let id_u32: u32 = id.parse().unwrap_or(0);
                do_delete(id_u32);
            });
            let ui_cancel = ui.clone();
            ui.on_notebook_delete_cancel(move || {
                ui_cancel.hide_notebook_delete_confirm();
            });
        }

        // ── Notebook list: "+ New Notebook" ──────────────────────────────────
        {
            let ui2 = ui.clone_component();
            let filter_nb_new = active_profile_filter.clone();
            ui.on_notebook_new_tapped(move || {
                let pf = filter_nb_new.borrow().clone();
                if pf.is_empty() {
                    // All mode — ask which profile first
                    ui2.set_create_picker_intent("notebook".into());
                    ui2.set_show_create_profile_picker(true);
                } else {
                    ui2.invoke_create_profile_picked("notebook".into(), pf.as_str().into());
                }
            });
        }

        // ── Per-note anchor: tracks anchor of the currently open note ────────
        let current_note_anchor: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));

        // ── Note list: tap a note → open its canvas page ──────────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let notes2 = notes.clone();
            let anchor_rc = current_note_anchor.clone();
            let data_leave = data_dir.clone();
            let vault_leave = vault_export.clone();
            let writes_leave = local_page_writes.clone();
            ui.on_note_tapped(move |id| {
                // Flush previous open note (if any) before switching.
                let prev = ui2.get_active_note_id().to_string();
                if !prev.is_empty() && prev != id.as_str() {
                    leave_note_canvas(
                        &ui_rc,
                        &mut notes2.borrow_mut(),
                        &data_leave,
                        prev.as_str(),
                        &vault_leave,
                        &writes_leave,
                    );
                }
                let notes = notes2.borrow();
                if let Some(note) = notes.iter().find(|n| n.id.to_string() == id) {
                    let page_idx = note.page_index;
                    let note_id = note.id.to_string();
                    let note_title = note.title.clone();
                    let template = note.paper_template;
                    let anchor = note.anchor.clone().unwrap_or_default();
                    let doc_page = note.doc_page;
                    let doc_page_count = note.doc_page_count.max(1);
                    drop(notes);
                    let paper_str = match template {
                        ephemeris_core::PageTemplate::Lines => "lined",
                        ephemeris_core::PageTemplate::Dot => "dotted",
                        ephemeris_core::PageTemplate::Grid => "squared",
                        ephemeris_core::PageTemplate::Blank => "blank",
                    };
                    let anchor_label = if let Some(ds) = anchor.strip_prefix("cal:") {
                        NaiveDate::parse_from_str(ds, "%Y-%m-%d")
                            .map(|d| format_day_anchor_label(d.year(), d.month(), d.day()))
                            .unwrap_or_default()
                    } else {
                        String::new()
                    };
                    *anchor_rc.borrow_mut() = anchor;
                    ui_rc.set_note_anchor_label(&anchor_label);
                    ui2.set_show_note_list(false);
                    ui2.set_show_notebook_list(false);
                    ui2.set_active_note_id(note_id.as_str().into());
                    ui2.set_active_paper(paper_str.into());
                    ui2.set_drawing_menu_open(false);
                    ui_rc.set_page_title(&note_title);
                    ui_rc.set_current_page_template(template);
                    ui_rc.navigate_to_page(page_idx);
                    ui_rc.set_page_index((doc_page + 1) as i32, doc_page_count as i32);
                    push_note_text_for_active(&ui_rc, &notes2.borrow(), &note_id);
                }
            });
        }

        // ── See-text toggle (cached text instantly; pending lines marked) ─────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            let gen_counter = see_text_gen.clone();
            let notes_prior = notes.clone();
            let bg = bg_recog.clone();
            let data_dir_st = data_dir.clone();
            ui.on_note_see_text_toggled(move |show| {
                if show {
                    let gen = gen_counter.fetch_add(1, Ordering::AcqRel) + 1;
                    let _ = gen; // generation still bumps so close discards stale UI pushes
                    let active_id = ui2.get_active_note_id().to_string();
                    let note_id = active_id.parse::<u32>().ok();
                    let doc_page = note_id.and_then(|id| {
                        notes_prior
                            .borrow()
                            .iter()
                            .find(|n| n.id == id)
                            .map(|n| n.doc_page)
                    });
                    let version = recognition_model_version_for(&cfg2.borrow());
                    let strokes = ui_rc.get_page_strokes();
                    let ruling = note_ruling_pitch(&ui_rc);
                    let rows = if let (Some(nid), Some(page)) = (note_id, doc_page) {
                        let mut cache = bg.get_or_load(&data_dir_st, nid, page, &version);
                        cache.sync_strokes(&strokes, ruling, &version);
                        let pending = cache.has_pending();
                        let stored = notes_prior
                            .borrow()
                            .iter()
                            .find(|n| n.id == nid)
                            .and_then(|n| n.text_content.clone());
                        let rows = see_text_rows(&cache, stored.as_deref());
                        bg.store(nid, page, cache);
                        if pending {
                            // Kick low-priority worker immediately (still blocked while drawing).
                            bg.try_spawn(
                                true,
                                &cfg2.borrow(),
                                &data_dir_st,
                                nid,
                                page,
                                strokes,
                                ruling,
                            );
                        }
                        rows
                    } else {
                        Vec::new()
                    };
                    ui_rc.set_see_text_rows(&rows);
                } else {
                    // Discard late See-text overlay updates from a closed view.
                    gen_counter.fetch_add(1, Ordering::AcqRel);
                }
                // Force a clear refresh so ink disappears under see-text (or
                // reappears when returning to ink) without eink ghosting.
                ui_rc.request_screen_change();
            });
        }

        // ── See-text line editor: save / reset / keep / accept ───────────────
        {
            let ctx = SeeTextEditCtx {
                ui: ui.clone(),
                bg: bg_recog.clone(),
                data_dir: data_dir.clone(),
                notes: notes.clone(),
                notebooks: notebooks.clone(),
                note_id_seq: note_id_seq.clone(),
                notebook_id_seq: notebook_id_seq.clone(),
                config: config.clone(),
                vault_export: vault_export.clone(),
            };
            let c = ctx.clone();
            ui.on_see_text_line_save(move |idx, text, kind, number, indent, hl, ul| {
                c.apply(idx, |cache, i| {
                    use ephemeris_core::see_text::{
                        apply_line_edit, block_from_kind, line_edit_state,
                    };
                    let checked = line_edit_state(cache, i)
                        .is_some_and(|e| e.block == ephemeris_core::BlockKind::Checkbox(true));
                    apply_line_edit(
                        cache,
                        i,
                        &ephemeris_core::see_text::LineEdit {
                            text,
                            block: block_from_kind(kind, number, checked),
                            indent: indent.clamp(0, 3) as u8,
                            highlight: hl,
                            underline: ul,
                        },
                    );
                });
            });
            let c = ctx.clone();
            ui.on_see_text_line_reset(move |idx| {
                c.apply(idx, |cache, i| cache.clear_line_correction(i));
            });
            let c = ctx.clone();
            ui.on_see_text_line_keep(move |idx| {
                c.apply(idx, |cache, i| cache.keep_correction(i));
            });
            let c = ctx;
            ui.on_see_text_line_accept(move |idx| {
                c.apply(idx, |cache, i| cache.accept_recognised(i));
            });
        }

        // ── Note list: "+ Note" → create in active folder (0 = root) ──────────
        {
            let ui2 = ui.clone_component();
            let notebooks2 = notebooks.clone();
            let active_nb2 = active_notebook_id.clone();
            let filter_nn = active_profile_filter.clone();
            ui.on_note_new_tapped(move || {
                let nb_id = active_nb2.get();
                let folder_id = if nb_id == 0 { None } else { Some(nb_id) };
                // Note inherits its folder's profile, else active profile filter.
                let pf = if let Some(fid) = folder_id {
                    notebooks2
                        .borrow()
                        .iter()
                        .find(|nb| nb.id == fid)
                        .map(|nb| nb.profile_id.clone())
                        .unwrap_or_else(|| filter_nn.borrow().clone())
                } else {
                    filter_nn.borrow().clone()
                };
                begin_create_with_profile(&ui2, "note", &pf);
            });
        }

        // ── Folder quick-add: "+" icon → new note in that folder + canvas ─────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let notes2 = notes.clone();
            let notebooks2 = notebooks.clone();
            let seq2 = note_id_seq.clone();
            let nb_seq2 = notebook_id_seq.clone();
            let active_nb2 = active_notebook_id.clone();
            let data_dir2 = data_dir.clone();
            let cfg2 = config.clone();
            let vault_ex = vault_export.clone();
            let push_qn = push_notebooks.clone();
            ui.on_notebook_quick_new_note(move |nb_id| {
                let nb_id_u32: u32 = nb_id.parse().unwrap_or(0);
                let new_id = seq2.get();
                seq2.set(new_id + 1);
                let folder_id = if nb_id_u32 == 0 {
                    None
                } else {
                    Some(nb_id_u32)
                };
                let pf = notebooks2
                    .borrow()
                    .iter()
                    .find(|nb| nb.id == nb_id_u32)
                    .map(|nb| nb.profile_id.clone())
                    .unwrap_or_default();
                let title = next_note_title(&cfg2.borrow(), new_id);
                {
                    let mut ns = notes2.borrow_mut();
                    let idx = ns.len();
                    ns.push(AppNote {
                        id: new_id,
                        title: title.clone(),
                        page_index: idx,
                        created_at: now_secs(),
                        updated_at: now_secs(),
                        notebook_id: folder_id,
                        paper_template: ephemeris_core::PageTemplate::Blank,
                        anchor: None,
                        profile_id: pf,
                        text_content: None,
                        doc_page: 0,
                        doc_page_count: 1,
                        tags: vec![],
                    });
                }
                active_nb2.set(nb_id_u32);
                push_qn();
                ui_rc.set_note_anchor_label("");
                ui_rc.push_new_page();
                ui_rc.set_page_title(&title);
                ui_rc.set_page_index(1, 1);
                ui2.set_active_paper("blank".into());
                ui2.set_show_note_list(false);
                ui2.set_show_notebook_list(false);
                ui2.set_drawing_menu_open(false);
                ui2.set_active_note_id(new_id.to_string().as_str().into());
                ui_rc.set_see_text_rows(&[]);
                ui_rc.set_note_see_text(false);
                save_notes_index_and_sync_vault(
                    &data_dir2,
                    &notes2.borrow(),
                    &notebooks2.borrow(),
                    seq2.get(),
                    nb_seq2.get(),
                    &cfg2.borrow(),
                    &vault_ex,
                );
            });
        }

        // ── Note rename: confirm ──────────────────────────────────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let notes2 = notes.clone();
            let notebooks2 = notebooks.clone();
            let note_seq2 = note_id_seq.clone();
            let nb_seq2 = notebook_id_seq.clone();
            let data_dir2 = data_dir.clone();
            let cfg2 = config.clone();
            let vault_ex = vault_export.clone();
            let push_rn = push_notebooks.clone();
            ui.on_note_rename_confirm(move |id, new_title| {
                let new_title = new_title.trim().to_string();
                if new_title.is_empty() {
                    ui2.set_note_rename_visible(false);
                    return;
                }
                {
                    let mut ns = notes2.borrow_mut();
                    if let Some(note) = ns.iter_mut().find(|n| n.id.to_string() == id) {
                        note.title = new_title.clone();
                        note.updated_at = now_secs();
                    }
                }
                // Keep canvas title bar in sync when renaming the currently open note.
                if ui2.get_active_note_id() == id {
                    ui_rc.set_page_title(&new_title);
                }
                push_rn();
                ui2.set_note_rename_visible(false);
                save_notes_index_and_sync_vault(
                    &data_dir2,
                    &notes2.borrow(),
                    &notebooks2.borrow(),
                    note_seq2.get(),
                    nb_seq2.get(),
                    &cfg2.borrow(),
                    &vault_ex,
                );
            });
        }

        // ── Note tags sheet ───────────────────────────────────────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let notes2 = notes.clone();
            let cfg2 = config.clone();
            ui.on_note_tags_open(move || {
                let id = ui2.get_active_note_id().to_string();
                let tags = notes2
                    .borrow()
                    .iter()
                    .find(|n| n.id.to_string() == id)
                    .map(|n| n.tags.join(" "))
                    .unwrap_or_default();
                let suggestions = cfg2.borrow().note_suggested_tags.join(" ");
                ui_rc.set_note_tags_id(&id);
                ui_rc.set_note_tags_value(&tags);
                ui_rc.set_note_tags_suggestions(&suggestions);
                ui2.set_note_tags_draft("".into());
                ui_rc.set_note_tags_visible(true);
            });
        }
        {
            let ui_rc = ui.clone();
            ui.on_note_tags_cancel(move || {
                ui_rc.set_note_tags_visible(false);
            });
        }
        {
            let ui_rc = ui.clone();
            ui.on_note_tags_add_suggestion(move |draft| {
                let mut current = parse_note_tag_list(&ui_rc.get_note_tags_value());
                let add = if draft.trim().is_empty() {
                    // "+ tag" chip: add first curated suggestion not already present.
                    parse_note_tag_list(&ui_rc.get_note_tags_suggestions())
                        .into_iter()
                        .find(|s| !current.iter().any(|c| c.eq_ignore_ascii_case(s)))
                } else {
                    normalize_note_tag(&draft)
                };
                if let Some(tag) = add {
                    if !current.iter().any(|c| c.eq_ignore_ascii_case(&tag)) {
                        current.push(tag);
                    }
                }
                ui_rc.set_note_tags_value(&current.join(" "));
            });
        }
        {
            let ui_rc = ui.clone();
            let notes2 = notes.clone();
            let notebooks2 = notebooks.clone();
            let data_dir2 = data_dir.clone();
            let note_seq2 = note_id_seq.clone();
            let nb_seq2 = notebook_id_seq.clone();
            let cfg2 = config.clone();
            let vault_ex = vault_export.clone();
            let push_rn = push_notebooks.clone();
            ui.on_note_tags_confirm(move |id, tags_raw| {
                let tags = parse_note_tag_list(&tags_raw);
                {
                    let mut ns = notes2.borrow_mut();
                    if let Some(note) = ns.iter_mut().find(|n| n.id.to_string() == id) {
                        note.tags = tags;
                        note.updated_at = now_secs();
                    }
                }
                ui_rc.set_note_tags_visible(false);
                push_rn();
                save_notes_index_and_sync_vault(
                    &data_dir2,
                    &notes2.borrow(),
                    &notebooks2.borrow(),
                    note_seq2.get(),
                    nb_seq2.get(),
                    &cfg2.borrow(),
                    &vault_ex,
                );
                // Mark for leave-style vault export so tags hit the MD soon.
                if let Ok(nid) = id.parse::<u32>() {
                    vault_ex.mark(nid);
                    vault_ex.request_flush_soon();
                    ui_rc.request_screen_change();
                }
            });
        }

        // ── Note move: confirm ────────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            let notes2 = notes.clone();
            let notebooks2 = notebooks.clone();
            let note_seq2 = note_id_seq.clone();
            let nb_seq2 = notebook_id_seq.clone();
            let data_dir2 = data_dir.clone();
            let cfg2 = config.clone();
            let vault_ex = vault_export.clone();
            let push_mv = push_notebooks.clone();
            ui.on_note_move_confirm(move |note_id, notebook_id| {
                let nb_id: u32 = notebook_id.parse().unwrap_or(0);
                let folder_id = if nb_id == 0 { None } else { Some(nb_id) };
                {
                    let mut ns = notes2.borrow_mut();
                    if let Some(note) = ns.iter_mut().find(|n| n.id.to_string() == note_id) {
                        note.notebook_id = folder_id;
                    }
                }
                push_mv();
                ui2.set_note_move_visible(false);
                save_notes_index_and_sync_vault(
                    &data_dir2,
                    &notes2.borrow(),
                    &notebooks2.borrow(),
                    note_seq2.get(),
                    nb_seq2.get(),
                    &cfg2.borrow(),
                    &vault_ex,
                );
            });
        }

        // ── Note move: cancel ─────────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_note_move_cancel(move || {
                ui2.set_note_move_visible(false);
            });
        }

        // ── Note rename: cancel ───────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_note_rename_cancel(move || {
                ui2.set_note_rename_visible(false);
            });
        }

        // ── Notebook rename / new-folder: confirm ─────────────────────────────
        {
            let ui2 = ui.clone_component();
            let notebooks2 = notebooks.clone();
            let notes2 = notes.clone();
            let note_seq2 = note_id_seq.clone();
            let nb_seq2 = notebook_id_seq.clone();
            let data_dir2 = data_dir.clone();
            let cfg2 = config.clone();
            let vault_ex = vault_export.clone();
            let push_nbs_ren = push_notebooks.clone();
            let active_nb_ren = active_notebook_id.clone();
            ui.on_notebook_rename_confirm(move |id, new_title| {
                let is_create = ui2.get_notebook_rename_is_create();
                let title = new_title.trim().to_string();
                if is_create {
                    let new_id = nb_seq2.get();
                    nb_seq2.set(new_id + 1);
                    let parent = active_nb_ren.get();
                    let parent_id = if parent == 0 { None } else { Some(parent) };
                    let profile_id = ui2.get_notebook_create_profile_id().to_string();
                    let title = if title.is_empty() {
                        format!("Folder {new_id}")
                    } else {
                        title
                    };
                    notebooks2.borrow_mut().push(AppNotebook {
                        id: new_id,
                        title,
                        profile_id,
                        parent_id,
                    });
                    push_nbs_ren();
                    let at_root = parent == 0;
                    ui2.set_show_notebook_list(at_root);
                    ui2.set_show_note_list(!at_root);
                    ui2.set_notebook_rename_visible(false);
                    ui2.set_notebook_rename_is_create(false);
                    ui2.set_notebook_create_profile_id("".into());
                    save_notes_index_and_sync_vault(
                        &data_dir2,
                        &notes2.borrow(),
                        &notebooks2.borrow(),
                        note_seq2.get(),
                        nb_seq2.get(),
                        &cfg2.borrow(),
                        &vault_ex,
                    );
                    return;
                }
                let id_u32: u32 = id.parse().unwrap_or(0);
                if !title.is_empty() {
                    if let Some(nb) = notebooks2.borrow_mut().iter_mut().find(|n| n.id == id_u32) {
                        nb.title = title;
                    }
                }
                push_nbs_ren();
                ui2.set_notebook_rename_visible(false);
                save_notes_index_and_sync_vault(
                    &data_dir2,
                    &notes2.borrow(),
                    &notebooks2.borrow(),
                    note_seq2.get(),
                    nb_seq2.get(),
                    &cfg2.borrow(),
                    &vault_ex,
                );
            });
        }

        // ── Notebook rename / new-folder: cancel ──────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_notebook_rename_cancel(move || {
                ui2.set_notebook_rename_visible(false);
                ui2.set_notebook_rename_is_create(false);
                ui2.set_notebook_create_profile_id("".into());
            });
        }

        // ── Calendar state ────────────────────────────────────────────────────
        let today = Utc::now();
        let cal_year: Rc<Cell<i32>> = Rc::new(Cell::new(today.year()));
        let cal_month: Rc<Cell<u32>> = Rc::new(Cell::new(today.month()));
        let cal_day: Rc<Cell<u32>> = Rc::new(Cell::new(today.day()));
        let cal_sub_view: Rc<Cell<i32>> = Rc::new(Cell::new(2)); // start with month view

        // Shared calendar refresh (ICS/CalDAV + notes/recordings on creation date).
        let refresh_cal: Rc<dyn Fn(i32, u32, u32, i32)> = {
            let ui_rc = ui.clone();
            let calendar_events2 = calendar_events.clone();
            let notes2 = notes.clone();
            let recordings2 = recordings.clone();
            let filter_rc = active_profile_filter.clone();
            let nia = notes_in_all_profiles.clone();
            let ria = recordings_in_all_profiles.clone();
            Rc::new(move |y, m, d, sv| {
                let filter = filter_rc.borrow().clone();
                let visible = visible_cal_events(
                    &calendar_events2.borrow(),
                    &notes2.borrow(),
                    &recordings2.borrow(),
                    &filter,
                    &nia.borrow(),
                    &ria.borrow(),
                );
                refresh_calendar(&ui_rc, &visible, y, m, d, sv);
            })
        };

        // Initial calendar data (month view)
        {
            refresh_cal(today.year(), today.month(), today.day(), 2);
        }

        // ── Nav rail: Calendar ────────────────────────────────────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let cal_year2 = cal_year.clone();
            let cal_month2 = cal_month.clone();
            let cal_day2 = cal_day.clone();
            let cal_sub_view2 = cal_sub_view.clone();
            let refresh = refresh_cal.clone();
            let notes_leave = notes.clone();
            let data_leave = data_dir.clone();
            let vault_leave = vault_export.clone();
            let writes_leave = local_page_writes.clone();
            let canvas_leave = canvas_session.clone();
            let cfg_leave = config.clone();
            let conns_leave = connections.clone();
            ui.on_nav_calendar_tapped(move || {
                leave_note_canvas(
                    &ui_rc,
                    &mut notes_leave.borrow_mut(),
                    &data_leave,
                    ui2.get_active_note_id().as_str(),
                    &vault_leave,
                    &writes_leave,
                );
                crate::canvas_app::leave_canvas_session(
                    &ui_rc,
                    &canvas_leave,
                    &cfg_leave,
                    &conns_leave.borrow(),
                    false,
                );
                let sv = cal_sub_view2.get();
                refresh(cal_year2.get(), cal_month2.get(), cal_day2.get(), sv);
                ui2.set_show_start_page(false);
                ui2.set_show_task_view(false);
                ui2.set_show_settings(false);
                ui2.set_show_filebrowser(false);
                ui2.set_show_note_list(false);
                ui2.set_show_notebook_list(false);
                ui2.set_show_recording_list(false);
                ui2.set_show_search(false);
                ui2.set_show_md_viewer(false);
                ui2.set_ring_visible(false);
                ui2.set_show_calendar(true);
            });
        }

        // ── Calendar sub-view change ──────────────────────────────────────────
        {
            let ui_rc = ui.clone();
            let cal_year2 = cal_year.clone();
            let cal_month2 = cal_month.clone();
            let cal_day2 = cal_day.clone();
            let cal_sub_view2 = cal_sub_view.clone();
            let notes2 = notes.clone();
            let refresh = refresh_cal.clone();
            ui.on_cal_sub_view_changed(move |v| {
                cal_sub_view2.set(v);
                ui_rc.set_cal_sub_view(v);
                refresh(cal_year2.get(), cal_month2.get(), cal_day2.get(), v);
                if v == 0 {
                    refresh_day_note_ui(
                        &ui_rc,
                        &notes2.borrow(),
                        cal_year2.get(),
                        cal_month2.get(),
                        cal_day2.get(),
                    );
                }
            });
        }

        // ── Calendar prev/next ────────────────────────────────────────────────
        {
            let cal_year2 = cal_year.clone();
            let cal_month2 = cal_month.clone();
            let cal_day2 = cal_day.clone();
            let cal_sub_view2 = cal_sub_view.clone();
            let refresh = refresh_cal.clone();
            ui.on_cal_prev(move || {
                let (y, m, d) = cal_navigate_prev(
                    cal_year2.get(),
                    cal_month2.get(),
                    cal_day2.get(),
                    cal_sub_view2.get(),
                );
                cal_year2.set(y);
                cal_month2.set(m);
                cal_day2.set(d);
                refresh(y, m, d, cal_sub_view2.get());
            });
        }
        {
            let cal_year2 = cal_year.clone();
            let cal_month2 = cal_month.clone();
            let cal_day2 = cal_day.clone();
            let cal_sub_view2 = cal_sub_view.clone();
            let refresh = refresh_cal.clone();
            ui.on_cal_next(move || {
                let (y, m, d) = cal_navigate_next(
                    cal_year2.get(),
                    cal_month2.get(),
                    cal_day2.get(),
                    cal_sub_view2.get(),
                );
                cal_year2.set(y);
                cal_month2.set(m);
                cal_day2.set(d);
                refresh(y, m, d, cal_sub_view2.get());
            });
        }

        // ── Calendar today button ─────────────────────────────────────────────
        {
            let ui_rc = ui.clone();
            let cal_year2 = cal_year.clone();
            let cal_month2 = cal_month.clone();
            let cal_day2 = cal_day.clone();
            let cal_sub_view2 = cal_sub_view.clone();
            let notes2 = notes.clone();
            let refresh = refresh_cal.clone();
            ui.on_cal_today(move || {
                let now = Utc::now();
                let (y, m, d) = (now.year(), now.month(), now.day());
                cal_year2.set(y);
                cal_month2.set(m);
                cal_day2.set(d);
                let sv = cal_sub_view2.get();
                refresh(y, m, d, sv);
                if sv == 0 {
                    refresh_day_note_ui(&ui_rc, &notes2.borrow(), y, m, d);
                }
            });
        }

        // ── Calendar day tap (from month/year → day view) ─────────────────────
        {
            let ui_rc = ui.clone();
            let cal_year2 = cal_year.clone();
            let cal_month2 = cal_month.clone();
            let cal_day2 = cal_day.clone();
            let cal_sub_view2 = cal_sub_view.clone();
            let notes2 = notes.clone();
            let refresh = refresh_cal.clone();
            ui.on_cal_day_tapped(move |y, m, d| {
                cal_year2.set(y);
                cal_month2.set(m as u32);
                cal_day2.set(d as u32);
                let sv = cal_sub_view2.get();
                // If in year/month view, jump to month/day view
                let new_sv = if sv == 3 { 2 } else { 0 };
                cal_sub_view2.set(new_sv);
                ui_rc.set_cal_sub_view(new_sv);
                refresh(y, m as u32, d as u32, new_sv);
                if new_sv == 0 {
                    refresh_day_note_ui(&ui_rc, &notes2.borrow(), y, m as u32, d as u32);
                }
            });
        }

        // ── Calendar: open/create day note ───────────────────────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let notes2 = notes.clone();
            let notebooks2 = notebooks.clone();
            let seq2 = note_id_seq.clone();
            let nb_seq2 = notebook_id_seq.clone();
            let data_dir2 = data_dir.clone();
            let cfg2 = config.clone();
            let vault_ex = vault_export.clone();
            let cal_year2 = cal_year.clone();
            let cal_month2 = cal_month.clone();
            let cal_day2 = cal_day.clone();
            let anchor_rc = current_note_anchor.clone();
            let filter_dn = active_profile_filter.clone();
            ui.on_cal_open_day_note(move |y, m, d| {
                let anchor_key = format!("cal:{:04}-{:02}-{:02}", y, m as u32, d as u32);
                let pf = filter_dn.borrow().clone();
                let (note_id, page_idx, note_title) = {
                    let ns = notes2.borrow();
                    if let Some(note) = ns
                        .iter()
                        .find(|n| n.anchor.as_deref() == Some(anchor_key.as_str()))
                    {
                        (note.id, note.page_index, note.title.clone())
                    } else {
                        drop(ns);
                        // Create new day note in the Inbox notebook
                        let new_id = seq2.get();
                        seq2.set(new_id + 1);
                        let date =
                            NaiveDate::from_ymd_opt(y, m as u32, d as u32).unwrap_or_default();
                        let title = format!(
                            "{} {}, {}",
                            month_short(date.month()),
                            date.day(),
                            date.year()
                        );
                        let idx = notes2.borrow().len();
                        notes2.borrow_mut().push(AppNote {
                            id: new_id,
                            title: title.clone(),
                            page_index: idx,
                            created_at: now_secs(),
                            updated_at: now_secs(),
                            notebook_id: Some(1),
                            paper_template: ephemeris_core::PageTemplate::Blank,
                            anchor: Some(anchor_key.clone()),
                            profile_id: pf,
                            text_content: None,
                            doc_page: 0,
                            doc_page_count: 1,
                            tags: vec![],
                        });
                        ui_rc.push_new_page();
                        ui_rc.set_page_index(1, 1);
                        save_notes_index_and_sync_vault(
                            &data_dir2,
                            &notes2.borrow(),
                            &notebooks2.borrow(),
                            seq2.get(),
                            nb_seq2.get(),
                            &cfg2.borrow(),
                            &vault_ex,
                        );
                        let ns = notes2.borrow();
                        let note = ns.iter().find(|n| n.id == new_id).unwrap();
                        (note.id, note.page_index, title)
                    }
                };
                // Navigate to the note canvas
                *anchor_rc.borrow_mut() = anchor_key;
                let anchor_label = format_day_anchor_label(y, m as u32, d as u32);
                ui_rc.set_note_anchor_label(&anchor_label);
                ui_rc.set_page_title(&note_title);
                ui_rc.set_current_page_template(ephemeris_core::PageTemplate::Blank);
                ui_rc.navigate_to_page(page_idx);
                if let Some(note) = notes2.borrow().iter().find(|n| n.id == note_id) {
                    set_note_doc_page_chrome(&ui_rc, note);
                } else {
                    ui_rc.set_page_index(1, 1);
                }
                ui2.set_active_paper("blank".into());
                ui2.set_show_calendar(false);
                ui2.set_show_note_list(false);
                ui2.set_show_notebook_list(false);
                ui2.set_active_note_id(note_id.to_string().as_str().into());
                // Remember which day we came from
                cal_year2.set(y);
                cal_month2.set(m as u32);
                cal_day2.set(d as u32);
            });
        }

        // ── Note anchor chip tapped → back to calendar day view ──────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let cal_year2 = cal_year.clone();
            let cal_month2 = cal_month.clone();
            let cal_day2 = cal_day.clone();
            let cal_sub_view2 = cal_sub_view.clone();
            let notes2 = notes.clone();
            let anchor_rc = current_note_anchor.clone();
            let refresh_cal_anchor = refresh_cal.clone();
            let data_leave = data_dir.clone();
            let vault_leave = vault_export.clone();
            let writes_leave = local_page_writes.clone();
            ui.on_note_anchor_tapped(move || {
                let anchor = anchor_rc.borrow().clone();
                // Parse "cal:YYYY-MM-DD"
                if let Some(date_str) = anchor.strip_prefix("cal:") {
                    if let Ok(date) = NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
                        let (y, m, d) = (date.year(), date.month(), date.day());
                        cal_year2.set(y);
                        cal_month2.set(m);
                        cal_day2.set(d);
                        cal_sub_view2.set(0);
                        ui_rc.set_cal_sub_view(0);
                        refresh_cal_anchor(y, m, d, 0);
                        refresh_day_note_ui(&ui_rc, &notes2.borrow(), y, m, d);
                        leave_note_canvas(
                            &ui_rc,
                            &mut notes2.borrow_mut(),
                            &data_leave,
                            ui2.get_active_note_id().as_str(),
                            &vault_leave,
                            &writes_leave,
                        );
                        ui2.set_show_calendar(true);
                        ui2.set_show_note_list(false);
                        ui2.set_show_notebook_list(false);
                        ui2.set_nav_section(NAV_CAL);
                    }
                }
            });
        }

        // ── Start-page notes button → root notes browser ─────────────────────
        {
            let ui2 = ui.clone_component();
            let push_nbs_start = push_notebooks.clone();
            let active_nb_start = active_notebook_id.clone();
            ui.on_start_page_notes(move || {
                active_nb_start.set(0);
                push_nbs_start();
                ui2.set_show_start_page(false);
                ui2.set_nav_section(NAV_NOTES);
                ui2.set_show_note_list(false);
                ui2.set_show_notebook_list(true);
            });
        }

        // ── Ring "Note" → create a new note immediately and open canvas ───────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let notebooks2 = notebooks.clone();
            let active_nb2 = active_notebook_id.clone();
            let filter_ring = active_profile_filter.clone();
            ui.on_ring_note(move || {
                ui_rc.save_current_page();
                let nb_id = active_nb2.get();
                let folder_id = if nb_id == 0 { None } else { Some(nb_id) };
                let pf = if let Some(fid) = folder_id {
                    notebooks2
                        .borrow()
                        .iter()
                        .find(|nb| nb.id == fid)
                        .map(|nb| nb.profile_id.clone())
                        .unwrap_or_else(|| filter_ring.borrow().clone())
                } else {
                    filter_ring.borrow().clone()
                };
                if pf.is_empty() {
                    // All mode — ask which profile first (same as notebook/rec).
                    ui2.set_create_picker_intent("ring-note".into());
                    ui2.set_show_create_profile_picker(true);
                } else {
                    ui2.invoke_create_profile_picked("ring-note".into(), pf.as_str().into());
                }
            });
        }

        // ── Ring "Event" → open create-event modal ────────────────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let cal_year2 = cal_year.clone();
            let cal_month2 = cal_month.clone();
            let cal_day2 = cal_day.clone();
            let refresh = refresh_cal.clone();
            ui.on_ring_calendar(move || {
                let y = cal_year2.get();
                let m = cal_month2.get();
                let d = cal_day2.get();
                refresh(y, m, d, 0);
                ui_rc.set_cal_sub_view(0);
                ui2.set_show_start_page(false);
                ui2.set_show_task_view(false);
                ui2.set_show_settings(false);
                ui2.set_show_filebrowser(false);
                ui2.set_show_note_list(false);
                ui2.set_show_notebook_list(false);
                ui2.set_show_calendar(true);
                ui2.set_ring_visible(false);
                let date = format!("{:04}-{:02}-{:02}", y, m, d);
                ui_rc.open_event_dialog("", &date, "09:00", "10:00", false);
            });
        }

        // ── Ring "Audio" → start recording (or navigate to rec list if no mic) ──
        {
            let ui2 = ui.clone_component();
            let rec_handle2 = rec_handle.clone();
            let push_recs_audio = push_recordings.clone();
            let filter_ra = active_profile_filter.clone();
            ui.on_ring_audio(move || {
                ui2.set_ring_visible(false);
                if !ui2.get_has_mic() {
                    return;
                }
                if rec_handle2.borrow().is_some() {
                    // Already recording — navigate to rec list to show status
                    ui2.set_show_start_page(false);
                    push_recs_audio();
                    ui2.set_show_recording_list(true);
                    ui2.set_nav_section(NAV_REC);
                    return;
                }
                let pf = filter_ra.borrow().clone();
                if pf.is_empty() {
                    ui2.set_create_picker_intent("rec".into());
                    ui2.set_show_create_profile_picker(true);
                } else {
                    ui2.invoke_create_profile_picked("rec".into(), pf.as_str().into());
                }
            });
        }

        // ── Nav rail: Recordings ──────────────────────────────────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let push_recs_nav = push_recordings.clone();
            let notes_leave = notes.clone();
            let data_leave = data_dir.clone();
            let vault_leave = vault_export.clone();
            let writes_leave = local_page_writes.clone();
            let canvas_leave = canvas_session.clone();
            let cfg_leave = config.clone();
            let conns_leave = connections.clone();
            ui.on_nav_rec_tapped(move || {
                leave_note_canvas(
                    &ui_rc,
                    &mut notes_leave.borrow_mut(),
                    &data_leave,
                    ui2.get_active_note_id().as_str(),
                    &vault_leave,
                    &writes_leave,
                );
                crate::canvas_app::leave_canvas_session(
                    &ui_rc,
                    &canvas_leave,
                    &cfg_leave,
                    &conns_leave.borrow(),
                    false,
                );
                push_recs_nav();
                ui2.set_show_start_page(false);
                ui2.set_show_task_view(false);
                ui2.set_show_settings(false);
                ui2.set_show_filebrowser(false);
                ui2.set_show_note_list(false);
                ui2.set_show_notebook_list(false);
                ui2.set_show_calendar(false);
                ui2.set_show_search(false);
                ui2.set_show_md_viewer(false);
                ui2.set_ring_visible(false);
                ui2.set_show_recording_list(true);
            });
        }

        // ── Recording list: "+ New" → start new recording ────────────────────
        {
            let ui2 = ui.clone_component();
            let rec_handle2 = rec_handle.clone();
            let filter_rn = active_profile_filter.clone();
            ui.on_rec_new_tapped(move || {
                if rec_handle2.borrow().is_some() {
                    return;
                }
                let pf = filter_rn.borrow().clone();
                if pf.is_empty() {
                    ui2.set_create_picker_intent("rec".into());
                    ui2.set_show_create_profile_picker(true);
                } else {
                    ui2.invoke_create_profile_picked("rec".into(), pf.as_str().into());
                }
            });
        }

        // ── Recording: pause ─────────────────────────────────────────────────
        {
            let rec_handle2 = rec_handle.clone();
            ui.on_rec_pause_tapped(move || {
                if let Some(ref h) = *rec_handle2.borrow() {
                    h.paused.store(true, Ordering::Relaxed);
                }
            });
        }

        // ── Recording: resume ────────────────────────────────────────────────
        {
            let rec_handle2 = rec_handle.clone();
            ui.on_rec_resume_tapped(move || {
                if let Some(ref h) = *rec_handle2.borrow() {
                    h.paused.store(false, Ordering::Relaxed);
                }
            });
        }

        // ── Recording: stop & save ────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            let rec_handle2 = rec_handle.clone();
            let recordings2 = recordings.clone();
            let rec_id_seq2 = rec_id_seq.clone();
            let rec_dir2 = rec_dir.clone();
            let filter_stop = active_profile_filter.clone();
            let rec_pf_stop = rec_pending_profile.clone();
            let push_recs_stop = push_recordings.clone();
            let txn_queue_stop = transcription_queue.clone();
            let txn_active_stop = transcription_active.clone();
            let txn_pending_stop = transcription_pending.clone();
            let txn_inflight_stop = transcription_in_flight.clone();
            ui.on_rec_stop_tapped(move || {
                let mut h_opt = rec_handle2.borrow_mut();
                if let Some(mut handle) = h_opt.take() {
                    let frames = handle.frame_count.load(Ordering::Relaxed);
                    let duration_secs = (frames / handle.sample_rate as u64) as u32;
                    let rec_id = handle.rec_id;
                    let _ = handle.writer_tx.send(WriterCmd::Finalize);
                    if let Some(thread) = handle.writer_thread.take() {
                        let _path = thread.join().ok().flatten();
                    }
                    drop(handle);
                    let filename = format!("rec_{rec_id}.wav");
                    // Use the profile picked at record-start time; fall back to active filter.
                    let pending_pf = rec_pf_stop.borrow().clone();
                    let profile_id = if pending_pf.is_empty() {
                        filter_stop.borrow().clone()
                    } else {
                        *rec_pf_stop.borrow_mut() = String::new();
                        pending_pf
                    };
                    let new_rec = AppRecording {
                        id: rec_id,
                        title: format!("Recording {rec_id}"),
                        duration_secs,
                        created_at: now_secs(),
                        audio_filename: filename,
                        transcription: None,
                        profile_id,
                    };
                    recordings2.borrow_mut().push(new_rec);
                    save_recordings_index(&rec_dir2, &recordings2.borrow(), rec_id_seq2.get());
                    push_recs_stop();
                    ui2.set_rec_state(0);
                    ui2.set_rec_duration_label("0:00".into());
                    // Auto-transcribe in the background when whisper is installed.
                    if whisper_ready() {
                        enqueue_transcription(
                            rec_id,
                            &txn_queue_stop,
                            &txn_inflight_stop,
                            &txn_active_stop,
                            &txn_pending_stop,
                            &recordings2,
                            &rec_dir2,
                            &ui2.get_whisper_lang(),
                            &push_recs_stop,
                        );
                    }
                }
            });
        }

        // ── Recording: discard ────────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            let rec_handle2 = rec_handle.clone();
            ui.on_rec_discard_tapped(move || {
                let mut h_opt = rec_handle2.borrow_mut();
                if let Some(mut handle) = h_opt.take() {
                    let _ = handle.writer_tx.send(WriterCmd::Discard);
                    if let Some(thread) = handle.writer_thread.take() {
                        let _ = thread.join();
                    }
                    drop(handle);
                    ui2.set_rec_state(0);
                    ui2.set_rec_duration_label("0:00".into());
                }
            });
        }

        // ── Recording: play ───────────────────────────────────────────────────
        {
            let recordings2 = recordings.clone();
            let rec_dir2 = rec_dir.clone();
            ui.on_rec_play_tapped(move |id| {
                let id_u32: u32 = id.parse().unwrap_or(0);
                let recs = recordings2.borrow();
                if let Some(rec) = recs.iter().find(|r| r.id == id_u32) {
                    let path = rec_dir2.join(&rec.audio_filename);
                    drop(recs);
                    play_audio(&path);
                }
            });
        }

        // ── Recording: open detail (primary row action) ───────────────────────
        {
            let open_rec_detail: Rc<dyn Fn(String)> = {
                let ui2 = ui.clone_component();
                let recordings2 = recordings.clone();
                let inflight = transcription_in_flight.clone();
                Rc::new(move |id: String| {
                    let id_u32: u32 = id.parse().unwrap_or(0);
                    let recs = recordings2.borrow();
                    let rec = recs.iter().find(|r| r.id == id_u32).cloned();
                    drop(recs);
                    let Some(rec) = rec else { return };

                    ui2.set_rec_active_id(rec.id.to_string().as_str().into());
                    ui2.set_rec_active_title(rec.title.as_str().into());
                    if inflight.borrow().contains(&id_u32) {
                        ui2.set_rec_transcription_text("Transcribing…".into());
                    } else if let Some(text) = &rec.transcription {
                        ui2.set_rec_transcription_text(text.as_str().into());
                    } else {
                        ui2.set_rec_transcription_text("".into());
                    }
                    ui2.set_rec_transcription_visible(true);
                })
            };
            {
                let open = open_rec_detail.clone();
                ui.on_rec_open_tapped(move |id| open(id));
            }
            {
                let open = open_rec_detail.clone();
                ui.on_rec_transcribe_tapped(move |id| open(id));
            }
        }

        // ── Recording: delete ─────────────────────────────────────────────────
        {
            let recordings2 = recordings.clone();
            let rec_id_seq2 = rec_id_seq.clone();
            let rec_dir2 = rec_dir.clone();
            let push_recs_del = push_recordings.clone();
            ui.on_rec_delete_tapped(move |id| {
                let id_u32: u32 = id.parse().unwrap_or(0);
                let filename = {
                    let recs = recordings2.borrow();
                    recs.iter()
                        .find(|r| r.id == id_u32)
                        .map(|r| r.audio_filename.clone())
                };
                recordings2.borrow_mut().retain(|r| r.id != id_u32);
                if let Some(fname) = filename {
                    let _ = std::fs::remove_file(rec_dir2.join(&fname));
                }
                save_recordings_index(&rec_dir2, &recordings2.borrow(), rec_id_seq2.get());
                push_recs_del();
            });
        }

        // ── Recording: rename confirm ─────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            let recordings2 = recordings.clone();
            let rec_id_seq2 = rec_id_seq.clone();
            let rec_dir2 = rec_dir.clone();
            let push_recs_ren = push_recordings.clone();
            ui.on_rec_rename_confirm(move |id, new_title| {
                let new_title = new_title.trim().to_string();
                if new_title.is_empty() {
                    ui2.set_rec_rename_visible(false);
                    return;
                }
                let id_u32: u32 = id.parse().unwrap_or(0);
                let mut recs = recordings2.borrow_mut();
                if let Some(rec) = recs.iter_mut().find(|r| r.id == id_u32) {
                    rec.title = new_title.clone();
                }
                drop(recs);
                save_recordings_index(&rec_dir2, &recordings2.borrow(), rec_id_seq2.get());
                push_recs_ren();
                ui2.set_rec_rename_visible(false);
                if ui2.get_rec_active_id() == id {
                    ui2.set_rec_active_title(new_title.as_str().into());
                }
            });
        }

        // ── Recording: rename cancel ──────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_rec_rename_cancel(move || {
                ui2.set_rec_rename_visible(false);
            });
        }

        // ── Recording: transcription dialog close (no-op kept for compat) ────
        {
            let ui2 = ui.clone_component();
            ui.on_rec_transcription_close(move || {
                ui2.set_rec_transcription_visible(false);
            });
        }

        // ── Recording: transcription save & close ─────────────────────────────
        {
            let ui2 = ui.clone_component();
            let recordings2 = recordings.clone();
            let rec_id_seq2 = rec_id_seq.clone();
            let rec_dir2 = rec_dir.clone();
            let push_recs_txn = push_recordings.clone();
            ui.on_rec_transcription_save(move |text| {
                let active_id: u32 = ui2.get_rec_active_id().parse().unwrap_or(0);
                let new_title = ui2.get_rec_active_title().trim().to_string();
                let mut recs = recordings2.borrow_mut();
                if let Some(rec) = recs.iter_mut().find(|r| r.id == active_id) {
                    rec.transcription = Some(text.to_string());
                    if !new_title.is_empty() {
                        rec.title = new_title;
                    }
                }
                save_recordings_index(&rec_dir2, &recs, rec_id_seq2.get());
                drop(recs);
                push_recs_txn();
                ui2.set_rec_transcription_visible(false);
            });
        }

        // ── Recording: retranscribe ───────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            let recordings2 = recordings.clone();
            let rec_id_seq2 = rec_id_seq.clone();
            let rec_dir2 = rec_dir.clone();
            let txn_pending2 = transcription_pending.clone();
            let txn_active2 = transcription_active.clone();
            let txn_queue2 = transcription_queue.clone();
            let txn_inflight2 = transcription_in_flight.clone();
            let push_recs_rtx = push_recordings.clone();
            ui.on_rec_transcription_retranscribe(move || {
                let active_id: u32 = ui2.get_rec_active_id().parse().unwrap_or(0);
                if active_id == 0 {
                    return;
                }

                {
                    let mut recs = recordings2.borrow_mut();
                    let Some(rec) = recs.iter_mut().find(|r| r.id == active_id) else {
                        return;
                    };
                    rec.transcription = None;
                    drop(recs);
                    save_recordings_index(&rec_dir2, &recordings2.borrow(), rec_id_seq2.get());
                }

                ui2.set_rec_transcription_text("Transcribing…".into());
                enqueue_transcription(
                    active_id,
                    &txn_queue2,
                    &txn_inflight2,
                    &txn_active2,
                    &txn_pending2,
                    &recordings2,
                    &rec_dir2,
                    &ui2.get_whisper_lang(),
                    &push_recs_rtx,
                );
            });
        }

        // ── Ring pen tools ────────────────────────────────────────────────────
        // ── Ring pen tools ────────────────────────────────────────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            ui.on_ring_pen(move || {
                ui_rc.set_tool(Tool::Pen);
                ui2.set_active_tool("pen".into());
                ui2.set_ring_visible(false);
            });
        }
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            ui.on_ring_highlighter(move || {
                ui_rc.set_tool(Tool::Highlighter);
                ui2.set_active_tool("highlighter".into());
                ui2.set_ring_visible(false);
            });
        }
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            ui.on_ring_eraser(move || {
                ui_rc.set_tool(Tool::Eraser);
                ui2.set_active_tool("eraser".into());
                ui2.set_ring_visible(false);
            });
        }
        // Ring clear is wired with pending_ink_cmd (ephemeris-e1b) later.

        // ── Burger menu tool selection → ink engine ───────────────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            ui.on_tool_selected(move |tool| {
                match tool.as_str() {
                    "pen" => {
                        ui_rc.set_tool(Tool::Pen);
                        ui2.set_active_tool("pen".into());
                    }
                    "highlighter" => {
                        ui_rc.set_tool(Tool::Highlighter);
                        ui2.set_active_tool("highlighter".into());
                    }
                    "eraser" => {
                        ui_rc.set_tool(Tool::Eraser);
                        ui2.set_active_tool("eraser".into());
                    }
                    "move" => {
                        // Move/lasso is handled in the app pointer path — keep
                        // the ink engine on Pen so a later tool switch is clean.
                        ui_rc.set_tool(Tool::Pen);
                        ui2.set_active_tool("move".into());
                    }
                    _ => {}
                }
            });
        }
        // ── Burger Thin / Med / Thick → pen base_width ────────────────────────
        {
            let ui_rc = ui.clone();
            ui.on_pen_width_selected(move |label| {
                ui_rc.set_pen_width(&label);
            });
        }

        // ── Start-page "All Tasks" button → task view ─────────────────────────
        {
            let mgr_tasks_1 = profile_mgr.clone();
            let ui2 = ui.clone_component();
            let tasks2 = tasks.clone();
            let cfg2 = config.clone();
            let filter_rc = active_profile_filter.clone();
            ui.on_start_page_tasks(move || {
                // Refresh vault tasks so external Obsidian edits show up.
                {
                    let cfg = cfg2.borrow();
                    if let Some(ref vault) = cfg.vault_path {
                        schedule_vault_git_pull(&cfg);
                        reload_obsidian_tasks(
                            &mut tasks2.borrow_mut(),
                            vault,
                            &cfg.vault_tasks_inbox,
                            &cfg.vault_export_subdir,
                            work_pid,
                        );
                    }
                }
                // Reset task-view filters when entering the page (active = hide closed).
                ui2.set_task_filter("active".into());
                ui2.set_task_sort("priority".into());
                ui2.set_task_lane_filter("".into());
                ui2.set_task_profile_filter("".into());
                ui2.set_task_search_query("".into());
                let filter_str = filter_rc.borrow().clone();
                let filtered = filter_tasks_by_profile(&tasks2.borrow(), &filter_str);
                push_full_task_list_comp(&ui2, &filtered, &mgr_tasks_1);
                ui2.set_show_start_page(false);
                ui2.set_show_recording_list(false);
                ui2.set_show_task_view(true);
            });
        }

        // ── Nav rail "Tasks" → task view ─────────────────────────────────────
        {
            let mgr_tasks_2 = profile_mgr.clone();
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let tasks2 = tasks.clone();
            let cfg2 = config.clone();
            let filter_rc = active_profile_filter.clone();
            let notes_leave = notes.clone();
            let data_leave = data_dir.clone();
            let vault_leave = vault_export.clone();
            let writes_leave = local_page_writes.clone();
            let canvas_leave = canvas_session.clone();
            let cfg_leave = config.clone();
            let conns_leave = connections.clone();
            ui.on_nav_tasks_tapped(move || {
                leave_note_canvas(
                    &ui_rc,
                    &mut notes_leave.borrow_mut(),
                    &data_leave,
                    ui2.get_active_note_id().as_str(),
                    &vault_leave,
                    &writes_leave,
                );
                crate::canvas_app::leave_canvas_session(
                    &ui_rc,
                    &canvas_leave,
                    &cfg_leave,
                    &conns_leave.borrow(),
                    false,
                );
                {
                    let cfg = cfg2.borrow();
                    if let Some(ref vault) = cfg.vault_path {
                        schedule_vault_git_pull(&cfg);
                        reload_obsidian_tasks(
                            &mut tasks2.borrow_mut(),
                            vault,
                            &cfg.vault_tasks_inbox,
                            &cfg.vault_export_subdir,
                            work_pid,
                        );
                    }
                }
                ui2.set_task_filter("active".into());
                ui2.set_task_sort("priority".into());
                ui2.set_task_lane_filter("".into());
                ui2.set_task_profile_filter("".into());
                ui2.set_task_search_query("".into());
                let filter_str = filter_rc.borrow().clone();
                let filtered = filter_tasks_by_profile(&tasks2.borrow(), &filter_str);
                push_full_task_list_comp(&ui2, &filtered, &mgr_tasks_2);
                ui2.set_show_start_page(false);
                ui2.set_show_note_list(false);
                ui2.set_show_notebook_list(false);
                ui2.set_show_calendar(false);
                ui2.set_show_settings(false);
                ui2.set_show_filebrowser(false);
                ui2.set_show_recording_list(false);
                ui2.set_show_search(false);
                ui2.set_show_md_viewer(false);
                ui2.set_ring_visible(false);
                ui2.set_show_task_view(true);
                ui2.set_nav_section(NAV_TASKS);
            });
        }

        // ── "Back" in task view → start page ─────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_task_back(move || {
                ui2.set_show_task_view(false);
                ui2.set_show_notebook_list(false);
                ui2.set_show_search(false);
                ui2.set_show_md_viewer(false);
                ui2.set_nav_section(NAV_HOME);
                ui2.set_show_start_page(true);
            });
        }

        // ── Toggle task done ──────────────────────────────────────────────────
        {
            let mgr_refresh_1 = profile_mgr.clone();
            let tasks2 = tasks.clone();
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            let filter_rc = active_profile_filter.clone();
            let persist2 = persist.clone();
            ui.on_task_complete_toggled(move |id| {
                let mut dirty: Option<Task> = None;
                {
                    let mut ts = tasks2.borrow_mut();
                    if let Some(t) = ts.iter_mut().find(|t| t.id.0.to_string() == id) {
                        t.set_done(!t.done);
                        persist2.mark_edited(t);
                        if t.source == "obsidian" {
                            dirty = Some(t.clone());
                        }
                    }
                }
                if let Some(ref t) = dirty {
                    write_back_obsidian_task(&cfg2.borrow(), t);
                }
                persist2.save(&tasks2.borrow());
                let filter_str = filter_rc.borrow().clone();
                let filtered = filter_tasks_by_profile(&tasks2.borrow(), &filter_str);
                refresh_comp(&ui2, &filtered, work_pid, &mgr_refresh_1, &cfg2.borrow());
            });
        }

        // When true, md-viewer Back returns to Tasks instead of Search.
        let md_viewer_return_tasks: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        // Wiki-link navigation stack (vault-relative paths); Back pops, else exits.
        let md_viewer_stack: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

        // ── Edit requested → populate + show dialog ───────────────────────────
        {
            let tasks2 = tasks.clone();
            let ui2 = ui.clone_component();
            ui.on_task_edit_requested(move |id| {
                let ts = tasks2.borrow();
                if let Some(t) = ts.iter().find(|t| t.id.0.to_string() == id) {
                    ui2.set_task_dialog_is_edit(true);
                    ui2.set_task_dialog_id(t.id.0.to_string().as_str().into());
                    ui2.set_task_dialog_title(t.title.as_str().into());
                    ui2.set_task_dialog_priority(priority_to_int(t.priority));
                    ui2.set_task_dialog_due(format_due_ymd(t.due).as_str().into());
                    let free_tags: Vec<_> = t
                        .tags
                        .iter()
                        .filter(|tag| {
                            let t = tag.trim_start_matches('#');
                            !t.starts_with("lane/")
                                && !t.starts_with("ctx/")
                                && !t.starts_with("context/")
                                && !t.starts_with("energy/")
                        })
                        .cloned()
                        .collect();
                    ui2.set_task_dialog_tags(free_tags.join(" ").as_str().into());
                    ui2.set_task_dialog_lane(resolve_lane(t, "next").as_str().into());
                    let ctxs: Vec<_> = t.contexts.iter().map(|c| c.name.clone()).collect();
                    ui2.set_task_dialog_contexts(ctxs.join(" ").as_str().into());
                    ui2.set_task_dialog_waiting(
                        t.waiting_for.clone().unwrap_or_default().as_str().into(),
                    );
                    ui2.set_task_dialog_has_source_note(
                        t.source_ref.as_ref().is_some_and(|r| !r.is_empty()),
                    );
                    ui2.set_task_dialog_visible(true);
                }
            });
        }

        // ── View Obsidian source note for a task (md-viewer modal) ────────────
        {
            let tasks2 = tasks.clone();
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            let conns2 = connections.clone();
            let ret = md_viewer_return_tasks.clone();
            let stack = md_viewer_stack.clone();
            ui.on_task_view_source_note(move |id| {
                let ts = tasks2.borrow();
                let Some(t) = ts.iter().find(|t| t.id.0.to_string() == id) else {
                    return;
                };
                let Some(source_ref) = t.source_ref.as_deref().filter(|r| !r.is_empty()) else {
                    tracing::warn!("Task {id} has no source_ref");
                    return;
                };
                let rel = match source_ref.rsplit_once(':') {
                    Some((path, _)) => path,
                    None => source_ref,
                };
                let Some(vault) = resolve_vault_path(&cfg2.borrow(), &conns2.borrow()) else {
                    tracing::warn!("Cannot open task source note: no vault path");
                    return;
                };
                match VaultFtsIndex::read_vault_file(&vault, rel) {
                    Ok(md) => {
                        stack.borrow_mut().clear();
                        stack.borrow_mut().push(rel.to_string());
                        show_md_viewer_content(&ui_rc, rel, &md);
                        ui2.set_task_dialog_visible(false);
                        ui2.set_show_search(false);
                        ret.set(true);
                        ui2.set_show_md_viewer(true);
                    }
                    Err(e) => tracing::warn!("Failed to open task source note {rel}: {e}"),
                }
            });
        }

        // ── Add confirmed ─────────────────────────────────────────────────────
        {
            let mgr_refresh_2 = profile_mgr.clone();
            let tasks2 = tasks.clone();
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            let filter_rc = active_profile_filter.clone();
            let counter = new_task_n.clone();
            let persist2 = persist.clone();
            ui.on_task_confirm_add(move |title, priority, due, tags, lane, contexts, waiting| {
                let title = title.trim().to_string();
                let title = if title.is_empty() {
                    let n = counter.get();
                    counter.set(n + 1);
                    format!("New Task {n}")
                } else {
                    title
                };
                let filter_str = filter_rc.borrow().clone();
                // Prefer global profile; in All, use the task-view chip filter if set.
                let chip = ui2.get_task_profile_filter().to_string();
                let target = if !filter_str.is_empty() {
                    filter_str.clone()
                } else if !chip.is_empty() {
                    chip
                } else {
                    String::new()
                };
                let task_profile = if target == personal_pid.0.to_string() {
                    personal_pid
                } else {
                    work_pid
                };
                let capture_lane = {
                    let cfg = cfg2.borrow();
                    if lane.trim().is_empty() {
                        cfg.task_engage.capture_lane.clone()
                    } else {
                        lane.trim().to_string()
                    }
                };
                let created_id = {
                    let mut ts = tasks2.borrow_mut();
                    let mut t = Task::new(title.clone(), task_profile);
                    t.priority = priority_from_int(priority);
                    t.due = parse_due_ymd(&due);
                    t.tags = parse_tag_list(&tags);
                    apply_task_dialog_fields(&mut t, &capture_lane, &contexts, &waiting);
                    let id = t.id;
                    // When a vault is connected, create into Obsidian inbox.
                    if cfg2.borrow().vault_path.is_some() {
                        t.source = "obsidian".to_string();
                        if !create_obsidian_task(&mut ts, &cfg2.borrow(), &t, task_profile) {
                            // Fall back to local in-memory task if write failed.
                            t.source = "local".to_string();
                            ts.push(t);
                            Some(id)
                        } else {
                            // Obsidian rewrite may change id; prefer title match at end.
                            ts.iter()
                                .rev()
                                .find(|x| x.title == title && x.source == "obsidian")
                                .map(|x| x.id)
                                .or(Some(id))
                        }
                    } else {
                        ts.push(t);
                        Some(id)
                    }
                };
                persist2.save(&tasks2.borrow());
                ui2.set_task_dialog_visible(false);
                let filtered = filter_tasks_by_profile(&tasks2.borrow(), &filter_str);
                refresh_comp(&ui2, &filtered, work_pid, &mgr_refresh_2, &cfg2.borrow());
                if cfg2.borrow().task_engage.clarify_after_capture {
                    if let Some(id) = created_id {
                        if let Some(t) = tasks2.borrow().iter().find(|t| t.id == id) {
                            ui2.set_clarify_task_id(id.0.to_string().as_str().into());
                            ui2.set_clarify_title(t.title.as_str().into());
                            ui2.set_clarify_lane(resolve_lane(t, "next").as_str().into());
                            let ctxs: Vec<_> = t.contexts.iter().map(|c| c.name.clone()).collect();
                            ui2.set_clarify_contexts(ctxs.join(" ").as_str().into());
                            ui2.set_clarify_waiting(
                                t.waiting_for.clone().unwrap_or_default().as_str().into(),
                            );
                            ui2.set_clarify_visible(true);
                        }
                    }
                }
            });
        }

        // ── Edit confirmed — title / priority / due / tags / lane / context
        {
            let mgr_refresh_3 = profile_mgr.clone();
            let tasks2 = tasks.clone();
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            let filter_rc = active_profile_filter.clone();
            let persist2 = persist.clone();
            ui.on_task_confirm_edit(
                move |id, title, priority, due, tags, lane, contexts, waiting| {
                    let title = title.trim().to_string();
                    let mut dirty: Option<Task> = None;
                    {
                        let mut ts = tasks2.borrow_mut();
                        if let Some(t) = ts.iter_mut().find(|t| t.id.0.to_string() == id) {
                            if !title.is_empty() {
                                t.title = title;
                            }
                            t.priority = priority_from_int(priority);
                            t.due = parse_due_ymd(&due);
                            t.tags = parse_tag_list(&tags);
                            apply_task_dialog_fields(t, lane.trim(), &contexts, &waiting);
                            persist2.mark_edited(t);
                            if t.source == "obsidian" {
                                dirty = Some(t.clone());
                            }
                        }
                    }
                    if let Some(ref t) = dirty {
                        write_back_obsidian_task(&cfg2.borrow(), t);
                    }
                    persist2.save(&tasks2.borrow());
                    ui2.set_task_dialog_visible(false);
                    let filter_str = filter_rc.borrow().clone();
                    let filtered = filter_tasks_by_profile(&tasks2.borrow(), &filter_str);
                    refresh_comp(&ui2, &filtered, work_pid, &mgr_refresh_3, &cfg2.borrow());
                },
            );
        }

        // ── Dialog cancel ─────────────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_task_dialog_cancel(move || {
                ui2.set_task_dialog_visible(false);
            });
        }

        // ── Event create dialog ───────────────────────────────────────────────
        {
            let ui_rc = ui.clone();
            let calendar_events2 = calendar_events.clone();
            let cal_year2 = cal_year.clone();
            let cal_month2 = cal_month.clone();
            let cal_day2 = cal_day.clone();
            let cal_sub_view2 = cal_sub_view.clone();
            let filter_rc = active_profile_filter.clone();
            let data_dir2 = data_dir.clone();
            let refresh = refresh_cal.clone();
            let new_event_n: Rc<Cell<u32>> = Rc::new(Cell::new(1));
            ui.on_event_confirm_add(move |title, date, start, end, all_day| {
                let title = title.trim().to_string();
                let title = if title.is_empty() {
                    let n = new_event_n.get();
                    new_event_n.set(n + 1);
                    format!("New Event {n}")
                } else {
                    title
                };
                let date_str = date.to_string();
                let date =
                    NaiveDate::parse_from_str(date_str.trim(), "%Y-%m-%d").unwrap_or_else(|_| {
                        NaiveDate::from_ymd_opt(cal_year2.get(), cal_month2.get(), cal_day2.get())
                            .unwrap_or_else(|| Utc::now().date_naive())
                    });
                let (start_ts, end_ts) = if all_day {
                    let start = date
                        .and_hms_opt(0, 0, 0)
                        .map(|dt| dt.and_utc().timestamp() as u64)
                        .unwrap_or(0);
                    (start, start + 86399)
                } else {
                    let (sh, sm) = parse_hm(&start).unwrap_or((9, 0));
                    let (eh, em) = parse_hm(&end).unwrap_or((10, 0));
                    let start = date
                        .and_hms_opt(sh, sm, 0)
                        .map(|dt| dt.and_utc().timestamp() as u64)
                        .unwrap_or(0);
                    let mut end = date
                        .and_hms_opt(eh, em, 0)
                        .map(|dt| dt.and_utc().timestamp() as u64)
                        .unwrap_or(start + 3600);
                    if end <= start {
                        end = start + 3600;
                    }
                    (start, end)
                };
                let filter_str = filter_rc.borrow().clone();
                let pid = parse_profile_id(&filter_str).unwrap_or(work_pid);
                let mut ev = CalendarEvent::new(
                    format!("local-{}", uuid::Uuid::new_v4()),
                    start_ts,
                    end_ts,
                    title,
                    pid,
                );
                ev.source = "local".to_string();
                {
                    let mut evts = calendar_events2.borrow_mut();
                    evts.push(ev);
                    save_local_events(&data_dir2, &evts);
                }
                cal_year2.set(date.year());
                cal_month2.set(date.month());
                cal_day2.set(date.day());
                ui_rc.close_event_dialog();
                refresh(date.year(), date.month(), date.day(), cal_sub_view2.get());
            });
        }
        {
            let ui_rc = ui.clone();
            ui.on_event_dialog_cancel(move || {
                ui_rc.close_event_dialog();
            });
        }

        // ── Calendar item tap (note / recording) ──────────────────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let notes2 = notes.clone();
            let push_recs = push_recordings.clone();
            let open_rec_detail = {
                // Reuse open path: show detail overlay for recording.
                let ui2 = ui.clone_component();
                let recordings2 = recordings.clone();
                let inflight = transcription_in_flight.clone();
                Rc::new(move |id: String| {
                    let id_u32: u32 = id.parse().unwrap_or(0);
                    let recs = recordings2.borrow();
                    let rec = recs.iter().find(|r| r.id == id_u32).cloned();
                    drop(recs);
                    let Some(rec) = rec else { return };
                    ui2.set_rec_active_id(rec.id.to_string().as_str().into());
                    ui2.set_rec_active_title(rec.title.as_str().into());
                    if inflight.borrow().contains(&id_u32) {
                        ui2.set_rec_transcription_text("Transcribing…".into());
                    } else if let Some(text) = &rec.transcription {
                        ui2.set_rec_transcription_text(text.as_str().into());
                    } else {
                        ui2.set_rec_transcription_text("".into());
                    }
                    ui2.set_rec_transcription_visible(true);
                })
            };
            let anchor_rc = current_note_anchor.clone();
            ui.on_cal_item_tapped(move |kind, id| {
                let kind = kind.to_string();
                let id = id.to_string();
                if kind == "note" {
                    let notes = notes2.borrow();
                    if let Some(note) = notes.iter().find(|n| n.id.to_string() == id) {
                        let page_idx = note.page_index;
                        let note_id = note.id.to_string();
                        let note_title = note.title.clone();
                        let template = note.paper_template;
                        let anchor = note.anchor.clone().unwrap_or_default();
                        let doc_page = note.doc_page;
                        let doc_page_count = note.doc_page_count.max(1);
                        drop(notes);
                        let paper_str = match template {
                            ephemeris_core::PageTemplate::Lines => "lined",
                            ephemeris_core::PageTemplate::Dot => "dotted",
                            ephemeris_core::PageTemplate::Grid => "squared",
                            ephemeris_core::PageTemplate::Blank => "blank",
                        };
                        let anchor_label = if let Some(ds) = anchor.strip_prefix("cal:") {
                            NaiveDate::parse_from_str(ds, "%Y-%m-%d")
                                .map(|d| format_day_anchor_label(d.year(), d.month(), d.day()))
                                .unwrap_or_default()
                        } else {
                            String::new()
                        };
                        *anchor_rc.borrow_mut() = anchor;
                        ui_rc.set_note_anchor_label(&anchor_label);
                        ui2.set_show_calendar(false);
                        ui2.set_show_note_list(false);
                        ui2.set_show_notebook_list(false);
                        ui2.set_show_recording_list(false);
                        ui2.set_active_note_id(note_id.as_str().into());
                        ui2.set_active_paper(paper_str.into());
                        ui2.set_drawing_menu_open(false);
                        ui_rc.set_page_title(&note_title);
                        ui_rc.set_current_page_template(template);
                        ui_rc.navigate_to_page(page_idx);
                        ui_rc.set_page_index((doc_page + 1) as i32, doc_page_count as i32);
                        push_note_text_for_active(&ui_rc, &notes2.borrow(), &note_id);
                    }
                } else if kind == "recording" {
                    ui2.set_show_calendar(false);
                    ui2.set_show_start_page(false);
                    ui2.set_show_task_view(false);
                    ui2.set_show_note_list(false);
                    ui2.set_show_notebook_list(false);
                    push_recs();
                    ui2.set_show_recording_list(true);
                    ui2.set_nav_section(NAV_REC);
                    open_rec_detail(id);
                }
            });
        }

        // ── Recording search / sort → rebuild list ───────────────────────────
        {
            let push_recs = push_recordings.clone();
            ui.on_rec_search_changed(move |_q| {
                push_recs();
            });
        }
        {
            let push_recs = push_recordings.clone();
            ui.on_rec_sort_changed(move |_s| {
                push_recs();
            });
        }

        // ── Task filter / sort / profile-chip / search → rebuild list ─────────
        {
            let mgr_tasklist_1 = profile_mgr.clone();
            let ui2 = ui.clone_component();
            let tasks2 = tasks.clone();
            ui.on_task_filter_changed(move |_filter| {
                push_full_task_list_comp(&ui2, &tasks2.borrow(), &mgr_tasklist_1);
            });
        }
        {
            let mgr_tasklist_2 = profile_mgr.clone();
            let ui2 = ui.clone_component();
            let tasks2 = tasks.clone();
            ui.on_task_sort_changed(move |_sort| {
                push_full_task_list_comp(&ui2, &tasks2.borrow(), &mgr_tasklist_2);
            });
        }
        {
            let mgr_refresh_4 = profile_mgr.clone();
            let ui2 = ui.clone_component();
            let tasks2 = tasks.clone();
            let cfg2 = config.clone();
            let filter_rc = active_profile_filter.clone();
            ui.on_task_profile_filter_changed(move |_pid| {
                let filter_str = filter_rc.borrow().clone();
                let filtered = filter_tasks_by_profile(&tasks2.borrow(), &filter_str);
                refresh_comp(&ui2, &filtered, work_pid, &mgr_refresh_4, &cfg2.borrow());
            });
        }
        {
            let mgr_tasklist_3 = profile_mgr.clone();
            let ui2 = ui.clone_component();
            let tasks2 = tasks.clone();
            ui.on_task_search_changed(move |q| {
                ui2.set_task_search_query(q.as_str().into());
                push_full_task_list_comp(&ui2, &tasks2.borrow(), &mgr_tasklist_3);
            });
        }
        {
            let mgr_lane = profile_mgr.clone();
            let ui2 = ui.clone_component();
            let tasks2 = tasks.clone();
            let cfg2 = config.clone();
            let filter_rc = active_profile_filter.clone();
            ui.on_task_lane_changed(move |lane| {
                ui2.set_task_lane_filter(lane.as_str().into());
                let filter_str = filter_rc.borrow().clone();
                let filtered = filter_tasks_by_profile(&tasks2.borrow(), &filter_str);
                refresh_comp(&ui2, &filtered, work_pid, &mgr_lane, &cfg2.borrow());
            });
        }
        {
            let mgr_ctx = profile_mgr.clone();
            let ui2 = ui.clone_component();
            let tasks2 = tasks.clone();
            let cfg2 = config.clone();
            let filter_rc = active_profile_filter.clone();
            ui.on_task_context_filter_changed(move |ctx| {
                ui2.set_task_context_filter(ctx.as_str().into());
                let filter_str = filter_rc.borrow().clone();
                let filtered = filter_tasks_by_profile(&tasks2.borrow(), &filter_str);
                refresh_comp(&ui2, &filtered, work_pid, &mgr_ctx, &cfg2.borrow());
            });
        }

        // ── Task priority cycle ───────────────────────────────────────────────
        {
            let mgr_prio = profile_mgr.clone();
            let ui2 = ui.clone_component();
            let tasks2 = tasks.clone();
            let cfg2 = config.clone();
            let filter_rc = active_profile_filter.clone();
            let persist2 = persist.clone();
            ui.on_task_priority_cycle(move |id| {
                let mut dirty: Option<Task> = None;
                {
                    let mut ts = tasks2.borrow_mut();
                    if let Some(t) = ts.iter_mut().find(|t| t.id.0.to_string() == id) {
                        t.priority = match t.priority {
                            TaskPriority::Urgent => TaskPriority::High,
                            TaskPriority::High => TaskPriority::Medium,
                            TaskPriority::Medium => TaskPriority::Low,
                            TaskPriority::Low => TaskPriority::Urgent,
                        };
                        persist2.mark_edited(t);
                        if t.source == "obsidian" {
                            dirty = Some(t.clone());
                        }
                    }
                }
                if let Some(ref t) = dirty {
                    write_back_obsidian_task(&cfg2.borrow(), t);
                }
                persist2.save(&tasks2.borrow());
                let filter_str = filter_rc.borrow().clone();
                let profile_filtered = filter_tasks_by_profile(&tasks2.borrow(), &filter_str);
                refresh_comp(&ui2, &profile_filtered, work_pid, &mgr_prio, &cfg2.borrow());
            });
        }

        // ── Settings › Tasks lane editors ─────────────────────────────────────
        {
            let page = ui.clone_component();
            let ui2 = ui.clone_component();
            page.on_settings_lane_label_changed(move |id, label| {
                mutate_settings_lane(&ui2, id.as_ref(), |row| {
                    row.label = label.as_str().into();
                });
            });
        }
        {
            let page = ui.clone_component();
            let ui2 = ui.clone_component();
            page.on_settings_lane_visible_toggled(move |id| {
                mutate_settings_lane(&ui2, id.as_ref(), |row| {
                    row.visible = !row.visible;
                });
            });
        }
        {
            let page = ui.clone_component();
            let ui2 = ui.clone_component();
            page.on_settings_lane_wip_cycle(move |id| {
                mutate_settings_lane(&ui2, id.as_ref(), |row| {
                    row.wip_limit = match row.wip_limit {
                        0 => 3,
                        3 => 5,
                        5 => 8,
                        _ => 0,
                    };
                });
            });
        }

        // ── Clarify after capture ─────────────────────────────────────────────
        {
            let mgr_clarify = profile_mgr.clone();
            let tasks2 = tasks.clone();
            let page = ui.clone_component();
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            let filter_rc = active_profile_filter.clone();
            let persist2 = persist.clone();
            page.on_clarify_save(move |id, lane, contexts, waiting| {
                let id = id.to_string();
                let lane = lane.to_string();
                let contexts = contexts.to_string();
                let waiting = waiting.to_string();
                let mut dirty: Option<Task> = None;
                {
                    let mut ts = tasks2.borrow_mut();
                    if let Some(t) = ts.iter_mut().find(|t| t.id.0.to_string() == id) {
                        apply_task_dialog_fields(t, lane.trim(), &contexts, &waiting);
                        persist2.mark_edited(t);
                        if t.source == "obsidian" {
                            dirty = Some(t.clone());
                        }
                    }
                }
                if let Some(ref t) = dirty {
                    write_back_obsidian_task(&cfg2.borrow(), t);
                }
                persist2.save(&tasks2.borrow());
                ui2.set_clarify_visible(false);
                let filter_str = filter_rc.borrow().clone();
                let filtered = filter_tasks_by_profile(&tasks2.borrow(), &filter_str);
                refresh_comp(&ui2, &filtered, work_pid, &mgr_clarify, &cfg2.borrow());
            });
        }
        {
            let page = ui.clone_component();
            let ui2 = ui.clone_component();
            page.on_clarify_skip(move || {
                ui2.set_clarify_visible(false);
            });
        }

        // ── Weekly review ─────────────────────────────────────────────────────
        let review_queue_ids: Rc<RefCell<Vec<TaskId>>> = Rc::new(RefCell::new(Vec::new()));
        let review_index: Rc<Cell<usize>> = Rc::new(Cell::new(0));
        {
            let mgr_rev = profile_mgr.clone();
            let tasks2 = tasks.clone();
            let page = ui.clone_component();
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            let filter_rc = active_profile_filter.clone();
            let q_ids = review_queue_ids.clone();
            let q_idx = review_index.clone();
            page.on_review_start(move || {
                let filter_str = filter_rc.borrow().clone();
                let filtered = filter_tasks_by_profile(&tasks2.borrow(), &filter_str);
                let cfg = cfg2.borrow();
                let queue = review_queue(
                    &filtered,
                    &cfg.task_engage.review_lanes,
                    now_secs(),
                    cfg.task_engage.staleness_days,
                );
                let ids: Vec<TaskId> = queue.iter().map(|t| t.id).collect();
                if ids.is_empty() {
                    ui2.set_review_title("Nothing to review".into());
                    ui2.set_review_meta("Waiting and Someday are clear.".into());
                    ui2.set_review_progress("0/0".into());
                    ui2.set_review_stale(false);
                    ui2.set_review_task_id("".into());
                    ui2.set_review_lane_label("".into());
                    ui2.set_review_visible(true);
                    *q_ids.borrow_mut() = Vec::new();
                    q_idx.set(0);
                    return;
                }
                *q_ids.borrow_mut() = ids;
                q_idx.set(0);
                show_review_card(&ui2, &filtered, &q_ids.borrow(), 0, &cfg);
                let _ = mgr_rev;
            });
        }
        {
            let mgr_rev = profile_mgr.clone();
            let tasks2 = tasks.clone();
            let page = ui.clone_component();
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            let filter_rc = active_profile_filter.clone();
            let q_ids = review_queue_ids.clone();
            let q_idx = review_index.clone();
            let persist2 = persist.clone();
            page.on_review_confirm(move || {
                let id_str = ui2.get_review_task_id().to_string();
                if !id_str.is_empty() {
                    let mut dirty: Option<Task> = None;
                    {
                        let mut ts = tasks2.borrow_mut();
                        if let Some(t) = ts.iter_mut().find(|t| t.id.0.to_string() == id_str) {
                            mark_reviewed(t, now_secs());
                            persist2.mark_edited(t);
                            if t.source == "obsidian" {
                                dirty = Some(t.clone());
                            }
                        }
                    }
                    if let Some(ref t) = dirty {
                        write_back_obsidian_task(&cfg2.borrow(), t);
                    }
                    persist2.save(&tasks2.borrow());
                }
                let next = q_idx.get() + 1;
                q_idx.set(next);
                let filter_str = filter_rc.borrow().clone();
                let filtered = filter_tasks_by_profile(&tasks2.borrow(), &filter_str);
                if next >= q_ids.borrow().len() {
                    ui2.set_review_visible(false);
                    refresh_comp(&ui2, &filtered, work_pid, &mgr_rev, &cfg2.borrow());
                } else {
                    show_review_card(&ui2, &filtered, &q_ids.borrow(), next, &cfg2.borrow());
                }
            });
        }
        {
            let tasks2 = tasks.clone();
            let page = ui.clone_component();
            let ui2 = ui.clone_component();
            page.on_review_edit(move || {
                let id = ui2.get_review_task_id().to_string();
                ui2.set_review_visible(false);
                if let Some(t) = tasks2.borrow().iter().find(|t| t.id.0.to_string() == id) {
                    ui2.set_task_dialog_is_edit(true);
                    ui2.set_task_dialog_id(t.id.0.to_string().as_str().into());
                    ui2.set_task_dialog_title(t.title.as_str().into());
                    ui2.set_task_dialog_priority(priority_to_int(t.priority));
                    ui2.set_task_dialog_due(format_due_ymd(t.due).as_str().into());
                    ui2.set_task_dialog_lane(resolve_lane(t, "next").as_str().into());
                    let ctxs: Vec<_> = t.contexts.iter().map(|c| c.name.clone()).collect();
                    ui2.set_task_dialog_contexts(ctxs.join(" ").as_str().into());
                    ui2.set_task_dialog_waiting(
                        t.waiting_for.clone().unwrap_or_default().as_str().into(),
                    );
                    ui2.set_task_dialog_tags("".into());
                    ui2.set_task_dialog_visible(true);
                }
            });
        }
        {
            let mgr_rev = profile_mgr.clone();
            let tasks2 = tasks.clone();
            let page = ui.clone_component();
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            let filter_rc = active_profile_filter.clone();
            let q_ids = review_queue_ids.clone();
            let q_idx = review_index.clone();
            let persist2 = persist.clone();
            page.on_review_done(move || {
                let id_str = ui2.get_review_task_id().to_string();
                if !id_str.is_empty() {
                    let mut dirty: Option<Task> = None;
                    {
                        let mut ts = tasks2.borrow_mut();
                        if let Some(t) = ts.iter_mut().find(|t| t.id.0.to_string() == id_str) {
                            t.set_done(true);
                            persist2.mark_edited(t);
                            if t.source == "obsidian" {
                                dirty = Some(t.clone());
                            }
                        }
                    }
                    if let Some(ref t) = dirty {
                        write_back_obsidian_task(&cfg2.borrow(), t);
                    }
                    persist2.save(&tasks2.borrow());
                }
                let next = q_idx.get() + 1;
                q_idx.set(next);
                let filter_str = filter_rc.borrow().clone();
                let filtered = filter_tasks_by_profile(&tasks2.borrow(), &filter_str);
                if next >= q_ids.borrow().len() {
                    ui2.set_review_visible(false);
                    refresh_comp(&ui2, &filtered, work_pid, &mgr_rev, &cfg2.borrow());
                } else {
                    show_review_card(&ui2, &filtered, &q_ids.borrow(), next, &cfg2.borrow());
                }
            });
        }
        {
            let mgr_rev = profile_mgr.clone();
            let tasks2 = tasks.clone();
            let page = ui.clone_component();
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            let filter_rc = active_profile_filter.clone();
            let q_ids = review_queue_ids.clone();
            let q_idx = review_index.clone();
            page.on_review_skip(move || {
                let next = q_idx.get() + 1;
                q_idx.set(next);
                let filter_str = filter_rc.borrow().clone();
                let filtered = filter_tasks_by_profile(&tasks2.borrow(), &filter_str);
                if next >= q_ids.borrow().len() {
                    ui2.set_review_visible(false);
                    refresh_comp(&ui2, &filtered, work_pid, &mgr_rev, &cfg2.borrow());
                } else {
                    show_review_card(&ui2, &filtered, &q_ids.borrow(), next, &cfg2.borrow());
                }
            });
        }
        {
            let mgr_rev = profile_mgr.clone();
            let tasks2 = tasks.clone();
            let page = ui.clone_component();
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            let filter_rc = active_profile_filter.clone();
            page.on_review_close(move || {
                ui2.set_review_visible(false);
                let filter_str = filter_rc.borrow().clone();
                let filtered = filter_tasks_by_profile(&tasks2.borrow(), &filter_str);
                refresh_comp(&ui2, &filtered, work_pid, &mgr_rev, &cfg2.borrow());
            });
        }

        // Shared modifier state: Ctrl held = ring mode, plain click = draw.
        // On the real eink device this maps to PenButton; Ctrl is the desktop stand-in.
        let ctrl_held: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let shift_held: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        // Note ink undo/redo stacks (ephemeris-e1b), keyed by (note_id, doc_page).
        let ink_history: Rc<RefCell<InkHistoryMap>> =
            Rc::new(RefCell::new(InkHistoryMap::new(DEFAULT_INK_HISTORY_DEPTH)));
        let pending_ink_cmd: Rc<Cell<Option<PendingInkCmd>>> = Rc::new(Cell::new(None));

        // ── Drawing menu Clear / Undo / Redo + ring clear (ephemeris-e1b) ─────
        {
            let ui2 = ui.clone_component();
            let pending = pending_ink_cmd.clone();
            ui.on_ring_clear(move || {
                pending.set(Some(PendingInkCmd::Clear));
                ui2.set_ring_visible(false);
            });
        }
        {
            let pending = pending_ink_cmd.clone();
            ui.on_clear_page(move || {
                pending.set(Some(PendingInkCmd::Clear));
            });
        }
        {
            let pending = pending_ink_cmd.clone();
            ui.on_ink_undo(move || {
                pending.set(Some(PendingInkCmd::Undo));
            });
        }
        {
            let pending = pending_ink_cmd.clone();
            ui.on_ink_redo(move || {
                pending.set(Some(PendingInkCmd::Redo));
            });
        }

        // True if the canvas's own TouchArea received the most recent pointer-down.
        // Because Slint only routes PointerPressed to the topmost element at the click
        // position, this stays false when interactive chrome (burger, ring, nav rail)
        // is clicked — letting us block spurious ink strokes on those elements.
        let canvas_touch_down: Rc<Cell<bool>> = Rc::new(Cell::new(false));

        // Finger/mouse page-swipe gestures (pen drawing must not flip pages).
        let page_swipe_allowed: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        // Set by swipe handlers so pointer-up discards the drag stroke instead of inking it.
        let page_swipe_handled: Rc<Cell<bool>> = Rc::new(Cell::new(false));

        // ── Canvas tap → show action ring (only when Ctrl held on desktop) ────
        {
            let ui2 = ui.clone_component();
            let ctrl = ctrl_held.clone();
            let touch_flag = canvas_touch_down.clone();
            ui.on_canvas_touch(move |x, y| {
                touch_flag.set(true);
                if ctrl.get() && !ui2.get_ring_visible() {
                    ui2.set_ring_cx(x);
                    ui2.set_ring_cy(y);
                    ui2.set_ring_show_audio(true);
                    ui2.set_ring_visible(true);
                }
            });
        }

        // ── In-note page flip (finger/mouse only; Slint edge-gates the start) ─
        // Book-style: right border → left (swipe-left) = next/create;
        // left border → right (swipe-right) = previous when it exists.
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let notes2 = notes.clone();
            let notebooks2 = notebooks.clone();
            let note_seq2 = note_id_seq.clone();
            let nb_seq2 = notebook_id_seq.clone();
            let data_dir2 = data_dir.clone();
            let vault_ex = vault_export.clone();
            let swipe_ok = page_swipe_allowed.clone();
            let swipe_done = page_swipe_handled.clone();
            ui.on_swipe_left(move || {
                if !swipe_ok.get() || !ui_rc.is_canvas_active() {
                    return;
                }
                let active = ui2.get_active_note_id().to_string();
                let Ok(note_id) = active.parse::<u32>() else {
                    return;
                };
                let mut ns = notes2.borrow_mut();
                let Some(note) = ns.iter_mut().find(|n| n.id == note_id) else {
                    return;
                };
                let target = note.doc_page + 1;
                switch_note_doc_page(&ui_rc, &data_dir2, note, target, true);
                swipe_done.set(true);
                drop(ns);
                // Index only — never OCR the whole library on a page flip.
                save_notes_index(
                    &data_dir2,
                    &notes2.borrow(),
                    &notebooks2.borrow(),
                    note_seq2.get(),
                    nb_seq2.get(),
                );
                vault_ex.mark(note_id);
            });
        }
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let notes2 = notes.clone();
            let notebooks2 = notebooks.clone();
            let note_seq2 = note_id_seq.clone();
            let nb_seq2 = notebook_id_seq.clone();
            let data_dir2 = data_dir.clone();
            let vault_ex = vault_export.clone();
            let swipe_ok = page_swipe_allowed.clone();
            let swipe_done = page_swipe_handled.clone();
            ui.on_swipe_right(move || {
                if !swipe_ok.get() || !ui_rc.is_canvas_active() {
                    return;
                }
                let active = ui2.get_active_note_id().to_string();
                let Ok(note_id) = active.parse::<u32>() else {
                    return;
                };
                let mut ns = notes2.borrow_mut();
                let Some(note) = ns.iter_mut().find(|n| n.id == note_id) else {
                    return;
                };
                if note.doc_page == 0 {
                    return;
                }
                let target = note.doc_page - 1;
                switch_note_doc_page(&ui_rc, &data_dir2, note, target, false);
                swipe_done.set(true);
                drop(ns);
                save_notes_index(
                    &data_dir2,
                    &notes2.borrow(),
                    &notebooks2.borrow(),
                    note_seq2.get(),
                    nb_seq2.get(),
                );
                vault_ex.mark(note_id);
            });
        }

        // ── Page indicator buttons (burger menu; same flip rules as edge swipe) ─
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let notes2 = notes.clone();
            let notebooks2 = notebooks.clone();
            let note_seq2 = note_id_seq.clone();
            let nb_seq2 = notebook_id_seq.clone();
            let data_dir2 = data_dir.clone();
            let vault_ex = vault_export.clone();
            ui.on_page_next_tapped(move || {
                // Menu-open disables `is_canvas_active`; indicator is only shown then.
                if !ui_rc.is_note_canvas_page() {
                    return;
                }
                let active = ui2.get_active_note_id().to_string();
                let Ok(note_id) = active.parse::<u32>() else {
                    return;
                };
                let mut ns = notes2.borrow_mut();
                let Some(note) = ns.iter_mut().find(|n| n.id == note_id) else {
                    return;
                };
                let target = note.doc_page + 1;
                switch_note_doc_page(&ui_rc, &data_dir2, note, target, true);
                drop(ns);
                save_notes_index(
                    &data_dir2,
                    &notes2.borrow(),
                    &notebooks2.borrow(),
                    note_seq2.get(),
                    nb_seq2.get(),
                );
                vault_ex.mark(note_id);
            });
        }
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let notes2 = notes.clone();
            let notebooks2 = notebooks.clone();
            let note_seq2 = note_id_seq.clone();
            let nb_seq2 = notebook_id_seq.clone();
            let data_dir2 = data_dir.clone();
            let vault_ex = vault_export.clone();
            ui.on_page_prev_tapped(move || {
                if !ui_rc.is_note_canvas_page() {
                    return;
                }
                let active = ui2.get_active_note_id().to_string();
                let Ok(note_id) = active.parse::<u32>() else {
                    return;
                };
                let mut ns = notes2.borrow_mut();
                let Some(note) = ns.iter_mut().find(|n| n.id == note_id) else {
                    return;
                };
                if note.doc_page == 0 {
                    return;
                }
                let target = note.doc_page - 1;
                switch_note_doc_page(&ui_rc, &data_dir2, note, target, false);
                drop(ns);
                save_notes_index(
                    &data_dir2,
                    &notes2.borrow(),
                    &notebooks2.borrow(),
                    note_seq2.get(),
                    nb_seq2.get(),
                );
                vault_ex.mark(note_id);
            });
        }

        // ── Ring dismiss ──────────────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_ring_dismissed(move || {
                ui2.set_ring_visible(false);
                ui2.set_drawing_menu_open(false);
            });
        }

        // ── Drawing ring paper type selection ─────────────────────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let notes2 = notes.clone();
            ui.on_paper_type_tapped(move |paper| {
                use ephemeris_core::PageTemplate;
                let template = match paper.as_str() {
                    "lined" => PageTemplate::Lines,
                    "dotted" => PageTemplate::Dot,
                    "squared" => PageTemplate::Grid,
                    _ => PageTemplate::Blank,
                };
                ui_rc.set_current_page_template(template);
                // Persist the chosen template on the active note so it's restored on re-open.
                let active_id = ui2.get_active_note_id().to_string();
                let mut ns = notes2.borrow_mut();
                if let Some(note) = ns.iter_mut().find(|n| n.id.to_string() == active_id) {
                    note.paper_template = template;
                }
            });
        }

        // ── Rename note from canvas (burger dropdown) ─────────────────────────
        {
            let ui2 = ui.clone_component();
            let notes2 = notes.clone();
            ui.on_rename_canvas_note(move || {
                let active_id = ui2.get_active_note_id().to_string();
                let notes = notes2.borrow();
                if let Some(note) = notes.iter().find(|n| n.id.to_string() == active_id) {
                    let title = note.title.clone();
                    drop(notes);
                    ui2.set_note_rename_id(active_id.as_str().into());
                    ui2.set_note_rename_title(title.as_str().into());
                    ui2.set_note_rename_visible(true);
                }
            });
        }

        // ── Settings → open ───────────────────────────────────────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            let push_so = push_connections.clone();
            let default_pid = default_conn_profile.clone();
            let canvas_leave = canvas_session.clone();
            let cfg_leave = config.clone();
            let conns_leave = connections.clone();
            ui.on_settings_open(move || {
                crate::canvas_app::leave_canvas_session(
                    &ui_rc,
                    &canvas_leave,
                    &cfg_leave,
                    &conns_leave.borrow(),
                    false,
                );
                // Ensure Connect has a concrete profile selected (never "All").
                let connect_pid = {
                    let current = ui2.get_connect_profile_id().to_string();
                    if !current.is_empty() {
                        current
                    } else {
                        let active = ui2.get_active_profile_id().to_string();
                        if !active.is_empty() {
                            active
                        } else {
                            default_pid.clone()
                        }
                    }
                };
                ui2.set_connect_profile_id(connect_pid.as_str().into());
                push_so();
                let cfg = cfg2.borrow();
                // Populate settings view from current config.
                let theme_idx = if cfg.theme == "dark" { 1 } else { 0 };
                ui2.set_settings_theme_idx(theme_idx);
                ui2.set_settings_pen_only(cfg.pen_only_drawing);
                ui2.set_settings_smart_shapes(cfg.note_smart_shapes);
                ui2.set_settings_note_title_pattern(cfg.note_title_pattern.as_str().into());
                ui_rc.set_settings_note_suggested_tags(&cfg.note_suggested_tags.join(", "));
                let vault = cfg
                    .vault_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();
                ui2.set_settings_vault_path(vault.as_str().into());
                ui2.set_connect_setup_export_subdir(
                    cfg.vault_export_subdir
                        .to_string_lossy()
                        .to_string()
                        .as_str()
                        .into(),
                );
                ui2.set_connect_setup_git_enabled(cfg.vault_git_enabled);
                let git_detected = cfg
                    .vault_path
                    .as_ref()
                    .map(|p| ephemeris_core::is_git_repo(p))
                    .unwrap_or(false);
                ui2.set_connect_setup_git_detected(git_detected);

                let sources: Vec<ephemeris_ui::CalSource> = cfg
                    .ics_paths
                    .iter()
                    .map(|p| {
                        let display = p
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| p.to_string_lossy().to_string());
                        ephemeris_ui::CalSource {
                            display: display.as_str().into(),
                            value: p.to_string_lossy().to_string().as_str().into(),
                        }
                    })
                    .chain(cfg.ics_urls.iter().map(|u| {
                        let display = u
                            .strip_prefix("webcal://")
                            .or_else(|| u.strip_prefix("https://"))
                            .unwrap_or(u.as_str())
                            .chars()
                            .take(40)
                            .collect::<String>();
                        ephemeris_ui::CalSource {
                            display: display.as_str().into(),
                            value: u.as_str().into(),
                        }
                    }))
                    .collect();
                ui2.set_settings_cal_sources(Rc::new(slint::VecModel::from(sources)).into());
                ui2.set_settings_new_url("".into());
                ui2.set_whisper_model_installed(find_whisper_model().is_some());
                ui2.set_htr_model_installed(find_htr_model_installed(&cfg));
                ui2.set_line_htr_model_installed(find_line_htr_model_installed(&cfg));
                ui2.set_settings_handwriting_lang(cfg.handwriting_language.as_str().into());
                ui2.set_settings_ocr_enabled(cfg.ocr_enabled);
                push_settings_task_lanes(&ui2, &cfg);
                ui2.set_settings_staleness_days(cfg.task_engage.staleness_days as i32);
                ui2.set_settings_context_presets(
                    cfg.task_engage.context_presets.join(" ").as_str().into(),
                );
                ui2.set_settings_engage_lane(cfg.task_engage.engage_lane.as_str().into());
                ui2.set_settings_capture_lane(cfg.task_engage.capture_lane.as_str().into());
                ui2.set_settings_clarify_after_capture(cfg.task_engage.clarify_after_capture);
                drop(cfg);
                ui2.set_show_start_page(false);
                ui2.set_show_task_view(false);
                ui2.set_show_note_list(false);
                ui2.set_show_notebook_list(false);
                ui2.set_show_recording_list(false);
                ui2.set_show_calendar(false);
                ui2.set_show_search(false);
                ui2.set_show_md_viewer(false);
                ui2.set_show_settings(true);
            });
        }

        // ── Search open ───────────────────────────────────────────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            let fts2 = vault_fts.clone();
            let fts_paths2 = vault_fts_paths.clone();
            let conns2 = connections.clone();
            let filter_rc = active_profile_filter.clone();
            let notes_leave = notes.clone();
            let data_leave = data_dir.clone();
            let vault_leave = vault_export.clone();
            let writes_leave = local_page_writes.clone();
            let canvas_leave = canvas_session.clone();
            let cfg_leave = config.clone();
            let conns_leave = connections.clone();
            ui.on_search_open(move || {
                // Persist ink + queue vault export, then hide the canvas so
                // strokes cannot bleed through search via min() compositing.
                leave_note_canvas(
                    &ui_rc,
                    &mut notes_leave.borrow_mut(),
                    &data_leave,
                    ui2.get_active_note_id().as_str(),
                    &vault_leave,
                    &writes_leave,
                );
                crate::canvas_app::leave_canvas_session(
                    &ui_rc,
                    &canvas_leave,
                    &cfg_leave,
                    &conns_leave.borrow(),
                    false,
                );
                // Show search immediately — do not block the transition on a
                // full vault walk when the FTS index is already warm.
                ui2.set_show_start_page(false);
                ui2.set_show_task_view(false);
                ui2.set_show_note_list(false);
                ui2.set_show_notebook_list(false);
                ui2.set_show_recording_list(false);
                ui2.set_show_calendar(false);
                ui2.set_show_settings(false);
                ui2.set_show_profile_switcher(false);
                ui2.set_ring_visible(false);
                ui2.set_show_md_viewer(false);
                ui2.set_show_search(true);
                ui_rc.set_search_results(&[]);
                ui_rc.set_search_query("");

                let filter = filter_rc.borrow().clone();
                let vaults = vaults_for_search(&filter, &cfg2.borrow(), &conns2.borrow());
                let status = if vaults.is_empty() {
                    let _ = fts2.borrow_mut().clear();
                    fts_paths2.borrow_mut().clear();
                    let vault_conn_count = conns2
                        .borrow()
                        .iter()
                        .filter(|c| c.kind == "vault")
                        .count();
                    if vault_conn_count == 0 {
                        tracing::info!("Search open: no vault connections at all");
                        "No Obsidian vault connected — add one under Settings › Connect."
                            .to_string()
                    } else if filter.is_empty() {
                        tracing::info!(
                            "Search open: {vault_conn_count} vault(s) exist but none allow All profiles"
                        );
                        "Vault(s) connected, but none enabled for All profiles. Switch to Work or enable “in All”."
                            .to_string()
                    } else {
                        tracing::info!(
                            "Search open: no vault for profile filter {filter:?} ({vault_conn_count} vault conn(s) total)"
                        );
                        "No vault for this profile — connect one under Settings › Connect (Work)."
                            .to_string()
                    }
                } else {
                    // Keep legacy config path in sync with the first searchable vault.
                    cfg2.borrow_mut().vault_path = Some(vaults[0].clone());
                    schedule_vault_git_pull(&cfg2.borrow());
                    let names: Vec<String> = vaults
                        .iter()
                        .map(|p| {
                            p.file_name()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_else(|| p.display().to_string())
                        })
                        .collect();
                    let cached = *fts_paths2.borrow() == vaults
                        && fts2.borrow().len().unwrap_or(0) > 0;
                    let exclude = cfg2.borrow().vault_export_subdir.clone();
                    if cached {
                        let n = fts2.borrow().len().unwrap_or(0);
                        tracing::info!(
                            "Search open: reusing FTS index ({n} notes, {} vault(s))",
                            vaults.len()
                        );
                        format!("Ready — {n} vault notes from {}.", names.join(", "))
                    } else {
                        let refs: Vec<&Path> = vaults.iter().map(PathBuf::as_path).collect();
                        match fts2
                            .borrow_mut()
                            .reindex_vaults(&refs, Some(exclude.as_path()))
                        {
                            Ok(n) => {
                                *fts_paths2.borrow_mut() = vaults.clone();
                                tracing::info!(
                                    "Search open: indexed {n} notes from {} vault(s): {:?}",
                                    vaults.len(),
                                    names
                                );
                                if n == 0 {
                                    format!(
                                        "Vault folder has no .md files: {}",
                                        vaults
                                            .iter()
                                            .map(|p| p.display().to_string())
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    )
                                } else {
                                    format!(
                                        "Indexed {n} vault notes from {}.",
                                        names.join(", ")
                                    )
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Vault FTS reindex on search open failed: {e}");
                                format!("Vault index failed: {e}")
                            }
                        }
                    }
                };
                ui_rc.set_search_status(&status);
                ui_rc.request_screen_change();
            });
        }

        // ── Search back ───────────────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_search_back(move || {
                ui2.set_show_search(false);
                ui2.set_show_md_viewer(false);
                ui2.set_nav_section(NAV_HOME);
                ui2.set_show_start_page(true);
            });
        }

        // ── Search query / submit ─────────────────────────────────────────────
        {
            let ui_rc = ui.clone();
            let cfg2 = config.clone();
            let fts2 = vault_fts.clone();
            let notes2 = notes.clone();
            let tasks2 = tasks.clone();
            let cal2 = calendar_events.clone();
            ui.on_search_submit(move |query| {
                let query = query.trim().to_string();
                ui_rc.set_search_query(&query);
                tracing::info!("Search submit: {:?}", query);
                let hits = run_unified_search(
                    &query,
                    &cfg2.borrow(),
                    &fts2.borrow(),
                    &notes2.borrow(),
                    &tasks2.borrow(),
                    &cal2.borrow(),
                );
                tracing::info!("Search hits: {}", hits.len());
                let rows: Vec<(String, String, String, String, String)> = hits
                    .into_iter()
                    .map(|h| {
                        (
                            h.id,
                            h.source,
                            h.title,
                            h.snippet,
                            h.path.unwrap_or_default(),
                        )
                    })
                    .collect();
                ui_rc.set_search_results(&rows);
            });
        }
        {
            // Live search while typing (also keeps the bound property in sync).
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            let fts2 = vault_fts.clone();
            let notes2 = notes.clone();
            let tasks2 = tasks.clone();
            let cal2 = calendar_events.clone();
            ui.on_search_query_changed(move |q| {
                let query = q.trim().to_string();
                ui2.set_search_query(query.as_str().into());
                if query.is_empty() {
                    ui_rc.set_search_results(&[]);
                    return;
                }
                let hits = run_unified_search(
                    &query,
                    &cfg2.borrow(),
                    &fts2.borrow(),
                    &notes2.borrow(),
                    &tasks2.borrow(),
                    &cal2.borrow(),
                );
                let rows: Vec<(String, String, String, String, String)> = hits
                    .into_iter()
                    .map(|h| {
                        (
                            h.id,
                            h.source,
                            h.title,
                            h.snippet,
                            h.path.unwrap_or_default(),
                        )
                    })
                    .collect();
                ui_rc.set_search_results(&rows);
            });
        }

        // ── Search result open ────────────────────────────────────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            let conns2 = connections.clone();
            let notes2 = notes.clone();
            let filter_rc = active_profile_filter.clone();
            let ret = md_viewer_return_tasks.clone();
            let stack = md_viewer_stack.clone();
            ui.on_search_result_tapped(move |id, source| {
                match source.as_str() {
                    "vault" => {
                        let Some(vault) = resolve_vault_path(&cfg2.borrow(), &conns2.borrow())
                        else {
                            tracing::warn!("Cannot open vault hit: no vault path");
                            return;
                        };
                        match VaultFtsIndex::read_vault_file(&vault, &id) {
                            Ok(md) => {
                                stack.borrow_mut().clear();
                                stack.borrow_mut().push(id.clone());
                                show_md_viewer_content(&ui_rc, &id, &md);
                                ui2.set_show_search(false);
                                ret.set(false);
                                ui2.set_show_md_viewer(true);
                            }
                            Err(e) => tracing::warn!("Failed to open vault note {id}: {e}"),
                        }
                    }
                    "local_note" => {
                        let notes = notes2.borrow();
                        if let Some(note) = notes.iter().find(|n| n.id.to_string() == id) {
                            let page_idx = note.page_index;
                            let title = note.title.clone();
                            let template = note.paper_template;
                            let text = note.text_content.clone().unwrap_or_default();
                            drop(notes);
                            ui2.set_show_search(false);
                            ui2.set_show_md_viewer(false);
                            ui2.set_show_note_list(false);
                            ui2.set_show_notebook_list(false);
                            ui2.set_active_note_id(id.as_str().into());
                            ui_rc.set_page_title(&title);
                            ui_rc.set_current_page_template(template);
                            ui_rc.set_see_text_rows(&ephemeris_core::see_text::rows_from_markdown(
                                &text,
                            ));
                            ui_rc.set_note_see_text(false);
                            ui_rc.navigate_to_page(page_idx);
                        }
                    }
                    "task" => {
                        ui2.set_show_search(false);
                        ui2.set_show_md_viewer(false);
                        ui2.set_task_filter("all".into());
                        ui2.set_task_sort("priority".into());
                        let filter_str = filter_rc.borrow().clone();
                        // Task view is refreshed by existing nav handler patterns.
                        let _ = filter_str;
                        ui2.set_show_task_view(true);
                        ui2.set_nav_section(NAV_TASKS);
                    }
                    "calendar" => {
                        ui2.set_show_search(false);
                        ui2.set_show_md_viewer(false);
                        ui2.set_show_calendar(true);
                        ui2.set_nav_section(NAV_CAL);
                    }
                    _ => {}
                }
            });
        }

        // ── MD viewer back → pop wiki stack, else search/Tasks ────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let ret = md_viewer_return_tasks.clone();
            let stack = md_viewer_stack.clone();
            let cfg2 = config.clone();
            let conns2 = connections.clone();
            ui.on_md_viewer_back(move || {
                let prev = {
                    let mut st = stack.borrow_mut();
                    if st.len() > 1 {
                        st.pop();
                        st.last().cloned()
                    } else {
                        st.clear();
                        None
                    }
                };
                if let Some(rel) = prev {
                    if let Some(vault) = resolve_vault_path(&cfg2.borrow(), &conns2.borrow()) {
                        if let Ok(md) = VaultFtsIndex::read_vault_file(&vault, &rel) {
                            show_md_viewer_content(&ui_rc, &rel, &md);
                            return;
                        }
                    }
                }
                ui2.set_show_md_viewer(false);
                if ret.get() {
                    ret.set(false);
                    ui2.set_show_task_view(true);
                    ui2.set_nav_section(NAV_TASKS);
                } else {
                    ui2.set_show_search(true);
                }
            });
        }

        // ── MD viewer wiki / vault link follow ────────────────────────────────
        {
            let ui_rc = ui.clone();
            let cfg2 = config.clone();
            let conns2 = connections.clone();
            let stack = md_viewer_stack.clone();
            ui.on_md_viewer_link_tapped(move |target| {
                if target.starts_with("http://") || target.starts_with("https://") {
                    tracing::info!("External link (not opened on device): {target}");
                    return;
                }
                let Some(vault) = resolve_vault_path(&cfg2.borrow(), &conns2.borrow()) else {
                    tracing::warn!("Cannot follow link: no vault path");
                    return;
                };
                let exclude = cfg2.borrow().vault_export_subdir.clone();
                let Some(rel) = resolve_wiki_target(&vault, &target, Some(exclude.as_path()))
                else {
                    tracing::warn!("Wiki target not found: {target}");
                    return;
                };
                match VaultFtsIndex::read_vault_file(&vault, &rel) {
                    Ok(md) => {
                        stack.borrow_mut().push(rel.clone());
                        show_md_viewer_content(&ui_rc, &rel, &md);
                    }
                    Err(e) => tracing::warn!("Failed to open linked note {rel}: {e}"),
                }
            });
        }

        // ── Settings → back ───────────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_settings_back(move || {
                ui2.set_show_settings(false);
                ui2.set_show_search(false);
                ui2.set_show_md_viewer(false);
                ui2.set_nav_section(NAV_HOME);
                ui2.set_show_start_page(true);
            });
        }

        // ── Settings → clear vault ────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            let conns2 = connections.clone();
            let fts2 = vault_fts.clone();
            let fts_paths2 = vault_fts_paths.clone();
            let push_v = push_connections.clone();
            ui.on_settings_clear_vault(move || {
                ui2.set_settings_vault_path("".into());
                {
                    let mut conns = conns2.borrow_mut();
                    conns.retain(|c| c.kind != "vault");
                    save_connections(&conns);
                }
                push_v();
                cfg2.borrow_mut().vault_path = None;
                if let Err(e) = cfg2.borrow().save() {
                    tracing::warn!("Config save after clear vault failed: {e}");
                }
                if let Err(e) = fts2.borrow_mut().clear() {
                    tracing::warn!("FTS clear after vault remove failed: {e}");
                }
                fts_paths2.borrow_mut().clear();
            });
        }

        // ── Settings → browse vault ───────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_settings_browse_vault(move || {
                let home = home_dir();
                ui2.set_fb_mode(0);
                ui2.set_fb_vault_mode(true);
                ui2.set_fb_current_path(home.to_string_lossy().to_string().as_str().into());
                let entries = read_dir_entries(&home, 0);
                ui2.set_fb_entries(Rc::new(slint::VecModel::from(entries_to_fb(&entries))).into());
                ui2.set_show_settings(false);
                ui2.set_show_filebrowser(true);
            });
        }

        // ── Settings → browse ICS ─────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_settings_browse_ics(move || {
                let home = home_dir();
                ui2.set_fb_mode(1);
                ui2.set_fb_vault_mode(false);
                ui2.set_fb_current_path(home.to_string_lossy().to_string().as_str().into());
                let entries = read_dir_entries(&home, 1);
                ui2.set_fb_entries(Rc::new(slint::VecModel::from(entries_to_fb(&entries))).into());
                ui2.set_show_settings(false);
                ui2.set_show_filebrowser(true);
            });
        }

        // ── Settings → browse custom profile icon ─────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_settings_browse_profile_icon(move || {
                let home = home_dir();
                ui2.set_fb_mode(2);
                ui2.set_fb_vault_mode(false);
                ui2.set_fb_current_path(home.to_string_lossy().to_string().as_str().into());
                let entries = read_dir_entries(&home, 2);
                ui2.set_fb_entries(Rc::new(slint::VecModel::from(entries_to_fb(&entries))).into());
                ui2.set_profile_edit_dialog_visible(false);
                ui2.set_show_settings(false);
                ui2.set_show_filebrowser(true);
            });
        }

        // ── Settings → add CalDAV ─────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            let conns_cd = connections.clone();
            let push_cd = push_connections.clone();
            let default_pid = default_conn_profile.clone();
            let pending_cd = caldav_pending.clone();
            let active_cd = caldav_active.clone();
            ui.on_settings_add_caldav(move |url, user, pass| {
                let url = url.trim().to_string();
                let user = user.trim().to_string();
                let pass = pass.to_string();
                if url.is_empty() || user.is_empty() {
                    return;
                }
                let display = {
                    let host = url
                        .strip_prefix("https://")
                        .or_else(|| url.strip_prefix("http://"))
                        .unwrap_or(url.as_str())
                        .chars()
                        .take(36)
                        .collect::<String>();
                    format!("CalDAV {host}")
                };
                let mut profile_id = ui2.get_connect_profile_id().to_string();
                if profile_id.is_empty() {
                    profile_id = default_pid.clone();
                }
                let pid = parse_profile_id(&profile_id).unwrap_or(work_pid);
                let mut conns = conns_cd.borrow_mut();
                let new_id = next_conn_id(&conns);
                conns.push(AppConnection {
                    id: new_id,
                    kind: "caldav".to_string(),
                    display: display.clone(),
                    value: url.clone(),
                    profile_id,
                    allow_in_aggregated: true,
                    username: user.clone(),
                    password: pass.clone(),
                });
                save_connections(&conns);
                drop(conns);
                push_cd();

                // Sync in background; results drain in about_to_wait.
                if active_cd.swap(true, Ordering::AcqRel) {
                    tracing::info!("CalDAV sync already running; connection saved for next reload");
                    return;
                }
                let mailbox = pending_cd.clone();
                let active_flag = active_cd.clone();
                std::thread::spawn(move || {
                    let (start, end) = ephemeris_core::default_sync_window();
                    let cfg = ephemeris_core::CalDavConfig::new(url, user, pass);
                    match ephemeris_core::CalDavClient::new(cfg)
                        .and_then(|c| c.sync_all(pid, start, end))
                    {
                        Ok((cals, events)) => {
                            tracing::info!(
                                "CalDAV sync: {} calendar(s), {} event(s)",
                                cals.len(),
                                events.len()
                            );
                            if let Ok(mut pending) = mailbox.lock() {
                                pending.extend(events);
                            }
                        }
                        Err(e) => tracing::warn!("CalDAV sync failed: {e}"),
                    }
                    active_flag.store(false, Ordering::Release);
                });
            });
        }

        // ── Settings → add URL ────────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            let conns_url = connections.clone();
            let push_url = push_connections.clone();
            let default_pid = default_conn_profile.clone();
            ui.on_settings_add_url(move |url| {
                let url = url.trim().to_string();
                if url.is_empty() {
                    return;
                }
                let display = url
                    .strip_prefix("webcal://")
                    .or_else(|| url.strip_prefix("https://"))
                    .unwrap_or(url.as_str())
                    .chars()
                    .take(40)
                    .collect::<String>();
                let mut profile_id = ui2.get_connect_profile_id().to_string();
                if profile_id.is_empty() {
                    profile_id = default_pid.clone();
                }
                // Add to connections for the Connect-tab profile
                let mut conns = conns_url.borrow_mut();
                let new_id = next_conn_id(&conns);
                conns.push(AppConnection {
                    id: new_id,
                    kind: "calendar".to_string(),
                    display: display.clone(),
                    value: url.clone(),
                    profile_id,
                    allow_in_aggregated: true,
                    username: String::new(),
                    password: String::new(),
                });
                drop(conns);
                push_url();
                // Also update legacy cal-sources
                let model = ui2.get_settings_cal_sources();
                let mut items: Vec<ephemeris_ui::CalSource> = (0..model.row_count())
                    .filter_map(|i| model.row_data(i))
                    .collect();
                items.push(ephemeris_ui::CalSource {
                    display: display.as_str().into(),
                    value: url.as_str().into(),
                });
                ui2.set_settings_cal_sources(Rc::new(slint::VecModel::from(items)).into());
                ui2.set_settings_new_url("".into());
            });
        }

        // ── Settings → remove source ──────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_settings_remove_source(move |idx| {
                let model = ui2.get_settings_cal_sources();
                let mut items: Vec<ephemeris_ui::CalSource> = (0..model.row_count())
                    .filter_map(|i| model.row_data(i))
                    .collect();
                if (idx as usize) < items.len() {
                    items.remove(idx as usize);
                }
                ui2.set_settings_cal_sources(Rc::new(slint::VecModel::from(items)).into());
            });
        }

        // ── Settings → save ───────────────────────────────────────────────────
        {
            let mgr_settings = profile_mgr.clone();
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            let tasks2 = tasks.clone();
            let cal2 = calendar_events.clone();
            let fts2 = vault_fts.clone();
            let fts_paths2 = vault_fts_paths.clone();
            let filter_rc = active_profile_filter.clone();
            let conns_ss = connections.clone();
            let data_dir_ss = data_dir.clone();
            ui.on_settings_save(move || {
                let theme_idx = ui2.get_settings_theme_idx();
                let theme = if theme_idx == 1 { "dark" } else { "light" };
                let pen_only = ui2.get_settings_pen_only();
                let smart_shapes = ui2.get_settings_smart_shapes();
                let note_title_pattern = {
                    let s = ui2.get_settings_note_title_pattern();
                    let trimmed = s.trim();
                    if trimmed.is_empty() {
                        "yyyy-mm-dd Note {n}".to_string()
                    } else {
                        trimmed.to_string()
                    }
                };

                // Derive vault and ICS sources from the connections list.
                let conns = conns_ss.borrow();
                let vault_str = conns
                    .iter()
                    .find(|c| c.kind == "vault")
                    .map(|c| c.value.clone())
                    .unwrap_or_default();
                let cal_values: Vec<String> = conns
                    .iter()
                    .filter(|c| c.kind == "calendar")
                    .map(|c| c.value.clone())
                    .collect();
                let caldav_conns: Vec<AppConnection> = conns
                    .iter()
                    .filter(|c| c.kind == "caldav")
                    .cloned()
                    .collect();
                drop(conns);

                let mut ics_urls = Vec::new();
                let mut ics_paths = Vec::new();
                for val in &cal_values {
                    if val.starts_with("http") || val.starts_with("webcal") {
                        ics_urls.push(val.clone());
                    } else {
                        ics_paths.push(PathBuf::from(val));
                    }
                }
                let export_subdir = {
                    let s = ui2.get_connect_setup_export_subdir().to_string();
                    let trimmed = s.trim();
                    if trimmed.is_empty() {
                        PathBuf::from(ephemeris_core::DEFAULT_EXPORT_SUBDIR)
                    } else {
                        PathBuf::from(trimmed.trim_start_matches('/'))
                    }
                };
                let git_enabled = ui2.get_connect_setup_git_enabled();

                let mut new_cfg = cfg2.borrow().clone();
                new_cfg.theme = theme.to_string();
                new_cfg.pen_only_drawing = pen_only;
                new_cfg.note_smart_shapes = smart_shapes;
                new_cfg.note_title_pattern = note_title_pattern;
                new_cfg.note_suggested_tags =
                    parse_note_tag_list(&ui2.get_settings_note_suggested_tags());
                new_cfg.ocr_enabled = ui2.get_settings_ocr_enabled();
                if let Some(lang) = ephemeris_core::HandwritingLanguage::parse(
                    ui2.get_settings_handwriting_lang().as_str(),
                ) {
                    new_cfg.handwriting_language = lang;
                }
                new_cfg.vault_path = if vault_str.is_empty() {
                    None
                } else {
                    Some(expand_user_path(&vault_str))
                };
                new_cfg.vault_export_subdir = export_subdir;
                new_cfg.vault_git_enabled = git_enabled;
                new_cfg.ics_urls = ics_urls;
                new_cfg.ics_paths = ics_paths;

                // Tasks / GTD settings
                apply_settings_task_fields_from_ui(&ui2, &mut new_cfg);

                if let Err(e) = new_cfg.save() {
                    tracing::warn!("Config save failed: {e}");
                }
                save_connections(&conns_ss.borrow());

                // Apply theme immediately.
                apply_theme_to_comp(&ui2, theme);

                // Reload tasks (vault tasks use work_pid; sample tasks span both profiles).
                schedule_vault_git_pull(&new_cfg);
                let new_tasks = if let Some(ref vault) = new_cfg.vault_path {
                    load_vault_tasks_with_inbox(
                        vault,
                        &new_cfg.vault_tasks_inbox,
                        &new_cfg.vault_export_subdir,
                        work_pid,
                    )
                    .unwrap_or_else(|_| sample_tasks_split(work_pid, personal_pid))
                } else {
                    sample_tasks_split(work_pid, personal_pid)
                };
                // Keep app-local and note tasks across the provider reload.
                let kept: Vec<Task> = tasks2
                    .borrow()
                    .iter()
                    .filter(|t| t.source == "local" || t.source == ephemeris_core::NOTE_TASK_SOURCE)
                    .cloned()
                    .collect();
                let mut new_tasks = new_tasks;
                new_tasks.retain(|t| t.source != "local");
                new_tasks.extend(kept);
                *tasks2.borrow_mut() = new_tasks;

                // Reload calendar events (ICS + CalDAV), keep local quick-add events.
                let now = now_secs();
                let mut evts = load_ics_events(&new_cfg.ics_paths, work_pid, now);
                evts.extend(load_ics_from_urls(&new_cfg.ics_urls, work_pid, now));
                evts.extend(load_caldav_events(&caldav_conns, work_pid, now));
                evts.extend(load_local_events(&data_dir_ss));
                *cal2.borrow_mut() = evts;

                // Persist new config into the shared handle.
                *cfg2.borrow_mut() = new_cfg.clone();

                // Rebuild vault search index for the new vault path.
                if let Some(ref vault) = new_cfg.vault_path {
                    match fts2
                        .borrow_mut()
                        .reindex_vault(vault, Some(new_cfg.vault_export_subdir.as_path()))
                    {
                        Ok(n) => {
                            tracing::info!("Reindexed {n} vault notes after settings save");
                            *fts_paths2.borrow_mut() = vec![vault.clone()];
                        }
                        Err(e) => tracing::warn!("Vault FTS reindex after save failed: {e}"),
                    }
                } else {
                    if let Err(e) = fts2.borrow_mut().clear() {
                        tracing::warn!("FTS clear after settings save failed: {e}");
                    }
                    fts_paths2.borrow_mut().clear();
                }

                // Refresh UI with active profile filter.
                let filter_str = filter_rc.borrow().clone();
                let filtered_tasks = filter_tasks_by_profile(&tasks2.borrow(), &filter_str);
                let visible_events = filter_events_by_profile(&cal2.borrow(), &filter_str);
                refresh_comp(
                    &ui2,
                    &filtered_tasks,
                    work_pid,
                    &mgr_settings,
                    &cfg2.borrow(),
                );
                push_agenda_comp(&ui2, &visible_events, AgendaRange::Day, now);

                ui2.set_show_settings(false);
                ui2.set_show_start_page(true);
            });
        }

        // ── Settings → whisper download ───────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            let dl_result2 = download_result.clone();
            let dl_active2 = download_active.clone();
            ui.on_settings_whisper_download(move || {
                if dl_active2.swap(true, Ordering::AcqRel) {
                    return; // already downloading
                }
                ui2.set_whisper_downloading(true);
                let mailbox = dl_result2.clone();
                let active_flag = dl_active2.clone();
                std::thread::spawn(move || {
                    let result = download_whisper_model();
                    if let Ok(mut guard) = mailbox.lock() {
                        *guard = Some(result);
                    }
                    active_flag.store(false, Ordering::Release);
                });
            });
        }

        // ── Settings → whisper delete ─────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_settings_whisper_delete(move || {
                if let Some(path) = find_whisper_model() {
                    let _ = std::fs::remove_file(&path);
                }
                ui2.set_whisper_model_installed(false);
            });
        }

        // ── Settings → HTR model download ─────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            let dl_result2 = htr_download_result.clone();
            let dl_active2 = htr_download_active.clone();
            ui.on_settings_htr_download(move || {
                if dl_active2.swap(true, Ordering::AcqRel) {
                    return;
                }
                ui2.set_htr_downloading(true);
                let mailbox = dl_result2.clone();
                let active_flag = dl_active2.clone();
                let cfg = cfg2.borrow().clone();
                std::thread::spawn(move || {
                    let result = download_htr_model(&cfg);
                    if let Ok(mut guard) = mailbox.lock() {
                        *guard = Some(result);
                    }
                    active_flag.store(false, Ordering::Release);
                });
            });
        }

        // ── Settings → HTR model delete ───────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            ui.on_settings_htr_delete(move || {
                delete_htr_model(&cfg2.borrow());
                ui2.set_htr_model_installed(false);
            });
        }

        // ── Settings → line-image HTR download (DE·HTR v2) ────────────────────
        {
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            let dl_result2 = line_htr_download_result.clone();
            let dl_active2 = line_htr_download_active.clone();
            ui.on_settings_line_htr_download(move || {
                if dl_active2.swap(true, Ordering::AcqRel) {
                    return;
                }
                ui2.set_line_htr_downloading(true);
                let mailbox = dl_result2.clone();
                let active_flag = dl_active2.clone();
                let cfg = cfg2.borrow().clone();
                std::thread::spawn(move || {
                    let result = download_line_htr_model(&cfg);
                    if let Ok(mut guard) = mailbox.lock() {
                        *guard = Some(result);
                    }
                    active_flag.store(false, Ordering::Release);
                });
            });
        }
        {
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            ui.on_settings_line_htr_delete(move || {
                delete_line_htr_model(&cfg2.borrow());
                ui2.set_line_htr_model_installed(false);
            });
        }

        // ── File browser → navigate ───────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_fb_navigate(move |path| {
                let p = PathBuf::from(path.as_str());
                let mode = ui2.get_fb_mode();
                ui2.set_fb_current_path(p.to_string_lossy().to_string().as_str().into());
                let entries = read_dir_entries(&p, mode);
                ui2.set_fb_entries(Rc::new(slint::VecModel::from(entries_to_fb(&entries))).into());
            });
        }

        // ── File browser → select vault ───────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            let conns_v = connections.clone();
            let push_v = push_connections.clone();
            let default_pid = default_conn_profile.clone();
            let cfg2 = config.clone();
            let fts2 = vault_fts.clone();
            let fts_paths2 = vault_fts_paths.clone();
            let tasks2 = tasks.clone();
            let vault_ex = vault_export.clone();
            ui.on_fb_select_vault(move |path| {
                let path_str = path.to_string();
                let vault_path = expand_user_path(&path_str);
                let display = vault_path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| path_str.clone());
                let mut profile_id = ui2.get_connect_profile_id().to_string();
                if profile_id.is_empty() {
                    profile_id = default_pid.clone();
                }
                // Replace vault for this profile only (other profiles keep theirs)
                let mut conns = conns_v.borrow_mut();
                conns.retain(|c| !(c.kind == "vault" && c.profile_id == profile_id));
                let new_id = next_conn_id(&conns);
                conns.push(AppConnection {
                    id: new_id,
                    kind: "vault".to_string(),
                    display,
                    value: vault_path.to_string_lossy().to_string(),
                    profile_id: profile_id.clone(),
                    allow_in_aggregated: true,
                    username: String::new(),
                    password: String::new(),
                });
                save_connections(&conns);
                drop(conns);
                push_v();

                let export_subdir = {
                    let s = ui2.get_connect_setup_export_subdir().to_string();
                    let trimmed = s.trim();
                    if trimmed.is_empty() {
                        PathBuf::from(ephemeris_core::DEFAULT_EXPORT_SUBDIR)
                    } else {
                        PathBuf::from(trimmed.trim_start_matches('/'))
                    }
                };
                let is_git = ephemeris_core::is_git_repo(&vault_path);

                // Apply immediately — don't wait for Settings → Save, otherwise
                // search/tasks keep using a stale or empty vault_path.
                {
                    let mut cfg = cfg2.borrow_mut();
                    cfg.vault_path = Some(vault_path.clone());
                    cfg.vault_export_subdir = export_subdir.clone();
                    // Git stays off until the user opts in (offer dialog below).
                    if !is_git {
                        cfg.vault_git_enabled = false;
                        ui2.set_connect_setup_git_enabled(false);
                    }
                    if let Err(e) = cfg.save() {
                        tracing::warn!("Config save after vault select failed: {e}");
                    }
                }
                schedule_vault_git_pull(&cfg2.borrow());
                match fts2
                    .borrow_mut()
                    .reindex_vault(&vault_path, Some(export_subdir.as_path()))
                {
                    Ok(n) => {
                        tracing::info!(
                            "Indexed {n} vault notes from {} after select",
                            vault_path.display()
                        );
                        *fts_paths2.borrow_mut() = vec![vault_path.clone()];
                    }
                    Err(e) => tracing::warn!("Vault FTS reindex after select failed: {e}"),
                }
                reload_obsidian_tasks(
                    &mut tasks2.borrow_mut(),
                    &vault_path,
                    &cfg2.borrow().vault_tasks_inbox,
                    &export_subdir,
                    work_pid,
                );
                schedule_vault_git_push(&cfg2.borrow());

                // Newly attached export target: backfill this profile's
                // historical notes (and any other profiles that already have
                // vaults) on the next idle flush — not on the UI thread.
                vault_ex.mark_full_sync();
                vault_ex.request_flush_soon();
                tracing::info!(
                    "Queued historical note export for profile {profile_id} → {}",
                    vault_path.display()
                );

                ui2.set_settings_vault_path(vault_path.to_string_lossy().as_ref().into());
                ui2.set_connect_setup_export_subdir(
                    export_subdir.to_string_lossy().to_string().as_str().into(),
                );
                ui2.set_connect_setup_git_detected(is_git);
                ui2.set_show_filebrowser(false);
                ui2.set_show_settings(true);
                ui2.set_settings_section(2);
                if is_git {
                    ui2.set_connect_setup_kind("vault-git".into());
                    ui2.set_connect_setup_visible(true);
                }
            });
        }

        // ── Vault setup confirm (export path / git options) ───────────────────
        {
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            let fts2 = vault_fts.clone();
            let fts_paths2 = vault_fts_paths.clone();
            let tasks2 = tasks.clone();
            ui.on_connect_vault_setup_confirm(move || {
                let export_subdir = {
                    let s = ui2.get_connect_setup_export_subdir().to_string();
                    let trimmed = s.trim();
                    if trimmed.is_empty() {
                        PathBuf::from(ephemeris_core::DEFAULT_EXPORT_SUBDIR)
                    } else {
                        PathBuf::from(trimmed.trim_start_matches('/'))
                    }
                };
                let git_enabled = ui2.get_connect_setup_git_enabled();
                {
                    let mut cfg = cfg2.borrow_mut();
                    cfg.vault_export_subdir = export_subdir.clone();
                    cfg.vault_git_enabled = git_enabled;
                    if let Err(e) = cfg.save() {
                        tracing::warn!("Config save after vault setup confirm failed: {e}");
                    }
                }
                let cfg = cfg2.borrow().clone();
                schedule_vault_git_pull(&cfg);
                if let Some(ref vault) = cfg.vault_path {
                    match fts2
                        .borrow_mut()
                        .reindex_vault(vault, Some(export_subdir.as_path()))
                    {
                        Ok(n) => {
                            *fts_paths2.borrow_mut() = vec![vault.clone()];
                            tracing::info!("Reindexed {n} vault notes after setup confirm");
                        }
                        Err(e) => tracing::warn!("Vault FTS reindex after setup confirm: {e}"),
                    }
                    reload_obsidian_tasks(
                        &mut tasks2.borrow_mut(),
                        vault,
                        &cfg.vault_tasks_inbox,
                        &export_subdir,
                        work_pid,
                    );
                    schedule_vault_git_push(&cfg);
                }
            });
        }

        // ── File browser → add ICS file ────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            let conns_i = connections.clone();
            let push_i = push_connections.clone();
            let default_pid = default_conn_profile.clone();
            ui.on_fb_add_ics(move |path| {
                let path_str = path.to_string();
                let p = PathBuf::from(&path_str);
                let display = p
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| path_str.clone());
                let mut profile_id = ui2.get_connect_profile_id().to_string();
                if profile_id.is_empty() {
                    profile_id = default_pid.clone();
                }
                let mut conns = conns_i.borrow_mut();
                let new_id = next_conn_id(&conns);
                conns.push(AppConnection {
                    id: new_id,
                    kind: "calendar".to_string(),
                    display: display.clone(),
                    value: path_str.clone(),
                    profile_id,
                    allow_in_aggregated: true,
                    username: String::new(),
                    password: String::new(),
                });
                drop(conns);
                push_i();
                // Also update legacy cal-sources list for backward compat
                let model = ui2.get_settings_cal_sources();
                let mut items: Vec<ephemeris_ui::CalSource> = (0..model.row_count())
                    .filter_map(|i| model.row_data(i))
                    .collect();
                items.push(ephemeris_ui::CalSource {
                    display: display.as_str().into(),
                    value: path_str.as_str().into(),
                });
                ui2.set_settings_cal_sources(Rc::new(slint::VecModel::from(items)).into());
                ui2.set_show_filebrowser(false);
                ui2.set_show_settings(true);
                ui2.set_settings_section(2);
            });
        }

        // ── File browser → select profile icon ────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_fb_select_profile_icon(move |path| {
                let path_str = path.to_string();
                let icon_key = format!("custom:{path_str}");
                match load_image_file(std::path::Path::new(&path_str)) {
                    Some(img) => {
                        ui2.set_profile_edit_icon(icon_key.as_str().into());
                        ui2.set_profile_edit_has_custom(true);
                        ui2.set_profile_edit_custom_image(img);
                    }
                    None => {
                        tracing::warn!("Failed to load profile icon {path_str}");
                    }
                }
                ui2.set_show_filebrowser(false);
                ui2.set_show_settings(true);
                ui2.set_settings_section(1);
                ui2.set_profile_edit_dialog_visible(true);
            });
        }

        // ── File browser → cancel ─────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_fb_cancel(move || {
                let mode = ui2.get_fb_mode();
                ui2.set_show_filebrowser(false);
                ui2.set_show_settings(true);
                if mode == 2 {
                    ui2.set_settings_section(1);
                    ui2.set_profile_edit_dialog_visible(true);
                } else {
                    ui2.set_settings_section(2); // return to Connect
                }
            });
        }

        // ── Create-in-profile picker: dispatch ────────────────────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let rec_handle2 = rec_handle.clone();
            let rec_id_seq2 = rec_id_seq.clone();
            let rec_dir2 = rec_dir.clone();
            let rec_pf_cp = rec_pending_profile.clone();
            let notes2 = notes.clone();
            let notebooks2 = notebooks.clone();
            let seq2 = note_id_seq.clone();
            let nb_seq2 = notebook_id_seq.clone();
            let active_nb2 = active_notebook_id.clone();
            let data_dir2 = data_dir.clone();
            let cfg2 = config.clone();
            let vault_ex = vault_export.clone();
            let anchor_rc = current_note_anchor.clone();
            let canvas_create_session = canvas_session.clone();
            ui.on_create_profile_picked(move |intent, profile_id| {
                let profile_id = profile_id.to_string();
                match intent.as_str() {
                    "notebook" => {
                        // Prompt for folder name before creating.
                        ui2.set_notebook_create_profile_id(profile_id.as_str().into());
                        ui2.set_notebook_rename_is_create(true);
                        ui2.set_notebook_rename_id("".into());
                        ui2.set_notebook_rename_title("".into());
                        ui2.set_notebook_rename_visible(true);
                    }
                    "rec" => {
                        if rec_handle2.borrow().is_some() {
                            return;
                        }
                        let new_id = rec_id_seq2.get();
                        rec_id_seq2.set(new_id + 1);
                        if let Some(handle) = start_recording(&rec_dir2, new_id) {
                            *rec_pf_cp.borrow_mut() = profile_id;
                            *rec_handle2.borrow_mut() = Some(handle);
                            ui2.set_rec_state(1);
                            ui2.set_rec_duration_label("0:00".into());
                        } else {
                            tracing::warn!("Failed to start recording via picker");
                        }
                    }
                    "note" | "ring-note" => {
                        // All is aggregator-only — never create a note without a real profile.
                        if profile_id.is_empty() {
                            tracing::warn!(
                                "Ignoring note create with empty profile (All is not a target)"
                            );
                            return;
                        }
                        let from_ring = intent == "ring-note";
                        let new_id = seq2.get();
                        seq2.set(new_id + 1);
                        let nb_id = active_nb2.get();
                        let folder_id = if nb_id == 0 { None } else { Some(nb_id) };
                        let title = next_note_title(&cfg2.borrow(), new_id);
                        {
                            let mut ns = notes2.borrow_mut();
                            let idx = ns.len();
                            ns.push(AppNote {
                                id: new_id,
                                title: title.clone(),
                                page_index: idx,
                                created_at: now_secs(),
                                updated_at: now_secs(),
                                notebook_id: folder_id,
                                paper_template: ephemeris_core::PageTemplate::Blank,
                                anchor: None,
                                profile_id,
                                text_content: None,
                                doc_page: 0,
                                doc_page_count: 1,
                        tags: vec![],
                            });
                        }
                        *anchor_rc.borrow_mut() = String::new();
                        ui_rc.set_note_anchor_label("");
                        ui_rc.push_new_page();
                        ui_rc.set_page_title(&title);
                        ui_rc.set_page_index(1, 1);
                        ui2.set_active_paper("blank".into());
                        if from_ring {
                            ui2.set_show_start_page(false);
                            ui2.set_show_task_view(false);
                            ui2.set_show_settings(false);
                            ui2.set_show_filebrowser(false);
                            ui2.set_show_calendar(false);
                            ui2.set_show_recording_list(false);
                            ui2.set_show_search(false);
                            ui2.set_show_md_viewer(false);
                            ui2.set_drawing_menu_open(false);
                            ui2.set_ring_visible(false);
                            ui2.set_nav_section(NAV_NOTES);
                        }
                        ui2.set_show_note_list(false);
                        ui2.set_show_notebook_list(false);
                        ui2.set_active_note_id(new_id.to_string().as_str().into());
                        ui_rc.set_see_text_rows(&[]);
                        ui_rc.set_note_see_text(false);
                        save_notes_index_and_sync_vault(
                            &data_dir2,
                            &notes2.borrow(),
                            &notebooks2.borrow(),
                            seq2.get(),
                            nb_seq2.get(),
                            &cfg2.borrow(),
                            &vault_ex,
                        );
                    }
                    "canvas" => {
                        if profile_id.is_empty() {
                            tracing::warn!(
                                "Ignoring canvas create with empty profile (All is not a target)"
                            );
                            return;
                        }
                        crate::canvas_app::create_canvas_with_profile(
                            &ui_rc,
                            &canvas_create_session,
                            &profile_id,
                        );
                    }
                    "canvas-folder" => {
                        if profile_id.is_empty() {
                            tracing::warn!(
                                "Ignoring canvas folder create with empty profile (All is not a target)"
                            );
                            return;
                        }
                        crate::canvas_app::create_canvas_folder_with_profile(
                            &ui_rc,
                            &canvas_create_session,
                            &profile_id,
                        );
                    }
                    _ => {}
                }
            });
            let ui2_cancel = ui.clone_component();
            ui.on_create_profile_pick_cancel(move || {
                ui2_cancel.set_show_create_profile_picker(false);
            });
        }

        #[cfg(target_os = "linux")]
        let (pen_source, pen_target) = {
            let proxy = event_loop.create_proxy();
            let wake: ephemeris_pal::input::PenWake = Arc::new(move || {
                let _ = proxy.send_event(AppUserEvent::PenReady);
            });
            let target = PenTarget::new(win_w, win_h);
            let source = match EvdevPenSource::spawn(Arc::clone(&target), Some(wake)) {
                Some(src) => {
                    tracing::info!("evdev pen source active");
                    Some(src)
                }
                None => {
                    tracing::info!("no evdev pen device found; finger/mouse only");
                    None
                }
            };
            (source, Some(target))
        };
        #[cfg(target_os = "linux")]
        let ink_recorder = ephemeris_pal::input::InkRecorder::from_env();

        let mut handler = WinitHandler {
            _rt: self.rt,
            ui,
            display: None,
            cursor_pos: (0.0, 0.0),
            scale: 1.0,
            ctrl_held,
            shift_held,
            ink_history,
            pending_ink_cmd,
            canvas_touch_down,
            page_swipe_allowed,
            page_swipe_handled,
            pointer_source: PointerSource::Mouse,
            is_drawing: false,
            redraw_pending: false,
            text_entry_active: false,
            ink_trace: InkLatencyTrace::new(),
            pen_ink_skip_slint: false,
            last_ink_pressure: 0.5,
            shape_hold: None,
            select_gesture: None,
            select_preview_dirty: false,
            undo_ui_page: None,
            data_dir,
            notes: notes.clone(),
            notebooks: notebooks.clone(),
            note_id_seq: note_id_seq.clone(),
            notebook_id_seq: notebook_id_seq.clone(),
            config: config.clone(),
            connections: connections.clone(),
            vault_export: vault_export.clone(),
            local_page_writes: local_page_writes.clone(),
            rec_handle,
            recordings: recordings.clone(),
            rec_id_seq: rec_id_seq.clone(),
            rec_dir: rec_dir.clone(),
            transcription_pending,
            transcription_active,
            transcription_queue,
            transcription_in_flight,
            download_result,
            download_active,
            htr_download_result,
            htr_download_active,
            line_htr_download_result,
            line_htr_download_active,
            bg_recog: bg_recog.clone(),
            caldav_pending,
            caldav_active,
            calendar_events: calendar_events.clone(),
            active_profile_filter: active_profile_filter.clone(),
            push_recordings,
            sync_note_tasks,
            #[cfg(target_os = "linux")]
            pen_source,
            #[cfg(target_os = "linux")]
            ink_recorder,
            #[cfg(target_os = "linux")]
            pen_target,
            chrome_peeking: false,
            chrome_dismiss_armed: false,
            chrome_pointer_held: false,
            chrome_swallow: false,
            chrome_was_covering: false,
            kiosk_settle_until: None,
            chrome_swipe_start: None,
            last_kiosk_check: Instant::now(),
            canvas_session: canvas_session.clone(),
            board_fingers: HashMap::new(),
            board_pinch_last_dist: None,
            board_pinching: false,
        };

        event_loop.run_app(&mut handler)?;
        Ok(())
    }
}

/// Design UI size: `EPHEMERIS_SIZE=WxH`, else ~½ PineNote portrait so chrome
/// upscales to ~1.75× on a 1404×1872 panel (icons/text readable on e-ink).
///
/// Do **not** default to panel-native 1404×1872 — that makes chrome tiny.
/// Ink is still rasterised at panel resolution: the UI upscales chrome into a
/// physical background cache and presents ink 1:1 (see `ephemeris-ui`).
fn probe_window_size() -> (u32, u32) {
    if let Ok(raw) = std::env::var("EPHEMERIS_SIZE") {
        if let Some((w, h)) = raw.split_once('x').or_else(|| raw.split_once('X')) {
            if let (Ok(w), Ok(h)) = (w.trim().parse::<u32>(), h.trim().parse::<u32>()) {
                if w >= 320 && h >= 320 {
                    return (w, h);
                }
            }
        }
        tracing::warn!("Ignoring invalid EPHEMERIS_SIZE={raw:?} (expected e.g. 800x1067)");
    }
    // Keep PineNote portrait aspect (1404∶1872 ≈ 3∶4).
    (800, 1067)
}

// ── Winit ApplicationHandler ──────────────────────────────────────────────────

fn board_finger_distance(fingers: &HashMap<u64, (f32, f32)>) -> Option<f32> {
    let mut iter = fingers.values();
    let (x0, y0) = *iter.next()?;
    let (x1, y1) = *iter.next()?;
    Some(((x1 - x0).hypot(y1 - y0)).max(0.0))
}

fn board_finger_midpoint(fingers: &HashMap<u64, (f32, f32)>) -> (f32, f32) {
    let mut iter = fingers.values();
    let Some(&(x0, y0)) = iter.next() else {
        return (0.0, 0.0);
    };
    let Some(&(x1, y1)) = iter.next() else {
        return (x0, y0);
    };
    ((x0 + x1) * 0.5, (y0 + y1) * 0.5)
}

/// Tracks stillness at the end of a pen stroke for hold-to-snap smart shapes.
struct ShapeHoldState {
    quiet_since: Instant,
    last_x: f32,
    last_y: f32,
    snapped: bool,
}

/// Move-tool gesture state (lasso select + drag). Never feeds the ink engine.
enum SelectGesture {
    /// Drawing a freehand loop (canvas-relative points).
    Lassoing {
        points: Vec<(f32, f32)>,
        preview_damage: ephemeris_core::geom::Rect,
    },
    /// Strokes selected; waiting for drag or a new lasso.
    Selected {
        ids: Vec<ephemeris_core::StrokeId>,
        /// Union bbox of the selection in canvas coords (for hit-testing).
        bbox: ephemeris_core::geom::Rect,
        /// Damage of the dashed outline drawn around the selection.
        outline: ephemeris_core::geom::Rect,
    },
    /// Dragging a prior selection; originals hidden on committed layer.
    Dragging {
        ids: Vec<ephemeris_core::StrokeId>,
        /// Snapshot of strokes at drag start (pre-translate).
        originals: Vec<ephemeris_core::Stroke>,
        origin_x: f32,
        origin_y: f32,
        /// Region cleared on the committed layer when the drag began.
        hide_damage: ephemeris_core::geom::Rect,
        preview_damage: ephemeris_core::geom::Rect,
        dx: f32,
        dy: f32,
    },
}

/// Distinguishes pen digitiser, finger touch, and desktop mouse for ink gating
/// and page-swipe eligibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PointerSource {
    /// EMR / evdev digitiser (Linux). Unused on desktop builds without a pen device.
    #[allow(dead_code)]
    Pen,
    Finger,
    Mouse,
}

/// `(note_id, doc_page, cache)` → reconcile handwritten checkboxes into tasks.
type SyncNoteTasksFn = Rc<dyn Fn(u32, usize, &PageRecognitionCache)>;

struct WinitHandler {
    _rt: Runtime,
    ui: Rc<EphemerisUi>,
    display: Option<DesktopWindow>,
    cursor_pos: (f32, f32),
    scale: f64,
    /// True while the left Ctrl key is held (simulates the pen side-button).
    ctrl_held: Rc<Cell<bool>>,
    /// Shift held (Ctrl+Shift+Z = redo).
    shift_held: Rc<Cell<bool>>,
    /// Per-page ink undo/redo (ephemeris-e1b).
    ink_history: Rc<RefCell<InkHistoryMap>>,
    /// Drawing-menu / shortcut command waiting to run on the UI thread.
    pending_ink_cmd: Rc<Cell<Option<PendingInkCmd>>>,
    /// Set by the canvas TouchArea's pointer-event(down) callback; cleared
    /// before each dispatch so we know whether the canvas (vs. chrome like the
    /// burger button) actually received the press.
    canvas_touch_down: Rc<Cell<bool>>,
    /// True when the active gesture may flip note pages (finger/mouse, not pen).
    page_swipe_allowed: Rc<Cell<bool>>,
    /// Set when a page swipe consumed the gesture — skip committing ink.
    page_swipe_handled: Rc<Cell<bool>>,
    /// Source of the current pointer gesture.
    pointer_source: PointerSource,
    /// True while a draw stroke is in progress (mouse button down, no Ctrl).
    is_drawing: bool,
    /// Coalesce pen samples → one softbuffer present per event-loop turn.
    /// Wayland `buffer_mut` blocks until the back buffer is released; per-sample
    /// `request_redraw` wedges the UI thread on PineNote (force-quit dialog).
    redraw_pending: bool,
    /// Platform text input currently enabled (mirrors the UI's
    /// `text-entry-active`; drives the GNOME on-screen keyboard).
    text_entry_active: bool,
    /// End-to-end sample→present timings when `EPHEMERIS_INK_TRACE=1`.
    ink_trace: InkLatencyTrace,
    /// Skip Slint press/release for the current pen ink stroke (canvas fast path).
    pen_ink_skip_slint: bool,
    /// Last contact pressure while drawing — PenUp often reports 0 and would
    /// otherwise collapse stroke tip width via Catmull-Rom.
    last_ink_pressure: f32,
    /// Hold-still tracker for smart-shape snap (None when feature off / idle).
    shape_hold: Option<ShapeHoldState>,
    /// Lasso / drag state while the Move tool is active.
    select_gesture: Option<SelectGesture>,
    /// Lasso / drag moved since the preview was last drawn.
    select_preview_dirty: bool,
    /// Note page the undo/redo affordances were last synced for.
    undo_ui_page: Option<(u32, usize)>,
    /// Directory where per-note pixel data is saved.
    data_dir: PathBuf,
    /// Shared note list — used to map page_idx → note_id for persistence.
    notes: Rc<RefCell<Vec<AppNote>>>,
    notebooks: Rc<RefCell<Vec<AppNotebook>>>,
    note_id_seq: Rc<Cell<u32>>,
    notebook_id_seq: Rc<Cell<u32>>,
    config: Rc<RefCell<Config>>,
    /// Per-profile vault/calendar connections (export targets).
    connections: Rc<RefCell<Vec<AppConnection>>>,
    /// Vault OCR/PDF deferred to leave-note / 10‑minute batch.
    vault_export: Rc<VaultExportQueue>,
    /// Debounced async local page raster/stroke writes.
    local_page_writes: Rc<LocalPageWriteQueue>,
    /// Active recording handle (Some while recording is in progress).
    rec_handle: Rc<RefCell<Option<RecordingHandle>>>,
    /// Recordings list — needed so about_to_wait can persist transcription results.
    recordings: Rc<RefCell<Vec<AppRecording>>>,
    rec_id_seq: Rc<Cell<u32>>,
    rec_dir: PathBuf,
    /// Completed transcriptions waiting to be written to disk and shown in the UI.
    transcription_pending: Arc<Mutex<Vec<(u32, String)>>>,
    /// True while a whisper subprocess is running (prevents double-dispatch).
    transcription_active: Arc<AtomicBool>,
    /// FIFO of recording ids waiting for (or between) whisper jobs.
    transcription_queue: Arc<Mutex<VecDeque<u32>>>,
    /// Ids queued or in-flight for list "Transcribing…" status.
    transcription_in_flight: Rc<RefCell<HashSet<u32>>>,
    /// Result of a model download: Ok(()) on success, Err(msg) on failure.
    download_result: Arc<Mutex<Option<Result<(), String>>>>,
    /// True while a model download thread is running.
    download_active: Arc<AtomicBool>,
    /// Result of an HTR model download.
    htr_download_result: Arc<Mutex<Option<Result<(), String>>>>,
    /// True while an HTR model download thread is running.
    htr_download_active: Arc<AtomicBool>,
    /// Result of a line-image HTR model download.
    line_htr_download_result: Arc<Mutex<Option<Result<(), String>>>>,
    /// True while a line-image HTR download thread is running.
    line_htr_download_active: Arc<AtomicBool>,
    /// Idle per-line recognition cache + low-priority worker (ktg.12).
    bg_recog: Rc<BackgroundRecogQueue>,
    /// CalDAV events finished syncing off-thread.
    caldav_pending: Arc<Mutex<Vec<CalendarEvent>>>,
    /// True while a CalDAV sync thread is running.
    caldav_active: Arc<AtomicBool>,
    /// Shared calendar events — merged with CalDAV sync results.
    calendar_events: Rc<RefCell<Vec<CalendarEvent>>>,
    active_profile_filter: Rc<RefCell<String>>,
    /// Profile-filtered recording list refresh — called after transcription completes.
    push_recordings: Rc<dyn Fn()>,
    /// Reconcile a page's handwritten checkboxes into tasks (ephemeris-fmt.11).
    sync_note_tasks: SyncNoteTasksFn,
    /// Linux: separate digitiser (PineNote pen) via `/dev/input` — not wl_touch.
    #[cfg(target_os = "linux")]
    pen_source: Option<EvdevPenSource>,
    /// `EPHEMERIS_INK_RECORD=/path.jsonl` — background JSONL pen capture.
    #[cfg(target_os = "linux")]
    ink_recorder: Option<ephemeris_pal::input::InkRecorder>,
    /// Shared with the evdev worker so resize / peek can refresh mapping.
    #[cfg(target_os = "linux")]
    pen_target: Option<Arc<PenTarget>>,
    /// Exclusive-fullscreen peek: GNOME bar visible until the next tap.
    chrome_peeking: bool,
    /// Armed after the revealing pointer lifts (or immediately if none held).
    chrome_dismiss_armed: bool,
    chrome_pointer_held: bool,
    /// Swallow the dismiss tap so it does not draw / open menus.
    chrome_swallow: bool,
    /// Was covering output last check (detect shell-driven un-fullscreen).
    chrome_was_covering: bool,
    /// Startup grace: geometry churn while the compositor settles fullscreen
    /// re-applies kiosk instead of entering peek (peek swallows the next tap).
    kiosk_settle_until: Option<Instant>,
    /// Top-edge swipe tracking: start Y in UI logical px.
    chrome_swipe_start: Option<f32>,
    /// Throttle for the kiosk geometry watchdog.
    last_kiosk_check: Instant,
    /// Obsidian Canvas session — pinch zoom when the board is open.
    canvas_session: Rc<crate::canvas_app::CanvasSession>,
    /// Active finger contacts on the board (`touch.id` → UI logical pos).
    board_fingers: HashMap<u64, (f32, f32)>,
    /// Last inter-finger distance while pinching (UI px).
    board_pinch_last_dist: Option<f32>,
    /// Two-finger pinch in progress — suppress single-pointer pan/tap.
    board_pinching: bool,
}

impl WinitHandler {
    /// Two-finger pinch on the Canvas board overview (UI logical coords).
    fn handle_board_touch(
        &mut self,
        id: u64,
        phase: winit::event::TouchPhase,
        lx: f32,
        ly: f32,
        pressure: f32,
    ) {
        use winit::event::TouchPhase;
        match phase {
            TouchPhase::Started => {
                self.board_fingers.insert(id, (lx, ly));
                if self.board_fingers.len() >= 2 {
                    if !self.board_pinching {
                        // End any single-finger pan/tap before zooming.
                        self.handle_pointer_release(lx, ly, 0);
                        crate::canvas_app::cancel_board_pointer_gesture(&self.canvas_session);
                        self.board_pinching = true;
                    }
                    self.board_pinch_last_dist = board_finger_distance(&self.board_fingers);
                } else {
                    self.pointer_source = PointerSource::Finger;
                    self.page_swipe_allowed.set(true);
                    self.handle_pointer_press(lx, ly, pressure, 0);
                }
            }
            TouchPhase::Moved => {
                if self.board_fingers.contains_key(&id) {
                    self.board_fingers.insert(id, (lx, ly));
                }
                if self.board_pinching && self.board_fingers.len() >= 2 {
                    if let Some(dist) = board_finger_distance(&self.board_fingers) {
                        if let Some(last) = self.board_pinch_last_dist {
                            if last > 2.0 && dist > 2.0 {
                                let factor = dist / last;
                                let (mx, my) = board_finger_midpoint(&self.board_fingers);
                                crate::canvas_app::apply_board_zoom_factor(
                                    &self.ui,
                                    &self.canvas_session,
                                    factor,
                                    mx,
                                    my,
                                );
                                self.request_redraw_coalesced();
                            }
                        }
                        self.board_pinch_last_dist = Some(dist);
                    }
                } else if !self.board_pinching {
                    self.pointer_source = PointerSource::Finger;
                    self.handle_pointer_move(lx, ly, pressure, 0);
                }
            }
            TouchPhase::Ended | TouchPhase::Cancelled => {
                self.board_fingers.remove(&id);
                if self.board_pinching {
                    if self.board_fingers.len() < 2 {
                        self.board_pinching = false;
                        self.board_pinch_last_dist = None;
                        self.canvas_session.save_doc();
                        crate::canvas_app::refresh_board_now(&self.ui, &self.canvas_session);
                        self.request_redraw_coalesced();
                        // Remaining finger must lift before pan resumes.
                    }
                } else {
                    self.handle_pointer_release(lx, ly, 0);
                }
                if self.board_fingers.is_empty() {
                    self.board_pinching = false;
                    self.board_pinch_last_dist = None;
                }
            }
        }
    }

    /// Map a physical window position into UI logical coordinates.
    ///
    /// Normalises against the current physical window size so maximized /
    /// letterboxed softbuffer present still lines up with Slint hit-testing.
    fn map_to_ui(&self, physical: winit::dpi::PhysicalPosition<f64>) -> (f32, f32) {
        let (ui_w, ui_h) = self.ui.size();
        if let Some(d) = &self.display {
            let (pw, ph) = d.physical_size();
            let x = (physical.x / pw as f64) * ui_w as f64;
            let y = (physical.y / ph as f64) * ui_h as f64;
            (x as f32, y as f32)
        } else {
            (
                (physical.x / self.scale) as f32,
                (physical.y / self.scale) as f32,
            )
        }
    }

    fn sync_pen_geometry(&self) {
        #[cfg(target_os = "linux")]
        if let (Some(target), Some(d)) = (&self.pen_target, &self.display) {
            let (ui_w, ui_h) = self.ui.size();
            target.set_ui(ui_w, ui_h);
            let (win_w, win_h, ox, oy) = d.logical_geometry();
            target.set_window(win_w, win_h, ox, oy);
        }
    }

    fn begin_chrome_peek(&mut self) {
        if !kiosk_enabled() || self.chrome_peeking {
            return;
        }
        self.chrome_peeking = true;
        // Wait for the revealing swipe to lift so a synthesized mouse-press
        // for the same finger cannot hide the bar immediately.
        self.chrome_dismiss_armed = !self.chrome_pointer_held;
        if let Some(d) = &self.display {
            d.show_panel();
            d.focus();
        }
        self.sync_pen_geometry();
        tracing::info!("chrome peek: left exclusive fullscreen (panel visible)");
    }

    fn end_chrome_peek(&mut self) {
        if !self.chrome_peeking {
            return;
        }
        self.chrome_peeking = false;
        self.chrome_dismiss_armed = false;
        self.chrome_was_covering = false;
        if let Some(d) = &self.display {
            d.enter_kiosk();
            d.focus();
        }
        self.sync_pen_geometry();
        tracing::info!("chrome peek: restored exclusive fullscreen");
    }

    /// Re-apply kiosk / detect shell-driven un-fullscreen. Never re-fullscreen
    /// while peeking (that would make the panel only flash).
    fn on_kiosk_geometry(&mut self) {
        if !kiosk_enabled() {
            self.sync_pen_geometry();
            return;
        }
        let Some(d) = &self.display else {
            return;
        };
        if self.chrome_peeking {
            self.sync_pen_geometry();
            return;
        }
        if d.covers_output() {
            self.chrome_was_covering = true;
            self.sync_pen_geometry();
            return;
        }
        let settling = self.kiosk_settle_until.is_some_and(|t| Instant::now() < t);
        if self.chrome_was_covering && settling {
            tracing::info!("kiosk: transient un-fullscreen during startup, re-entering");
        } else if self.chrome_was_covering {
            // Shell (or compositor) shrunk us off exclusive fullscreen — enter peek.
            self.begin_chrome_peek();
            return;
        }
        d.enter_kiosk();
        self.sync_pen_geometry();
    }

    fn handle_pointer_move(&mut self, lx: f32, ly: f32, pressure: f32, t_us: u64) {
        use ephemeris_ui::STATUS_BAR_H;
        self.cursor_pos = (lx, ly);
        if self.chrome_swallow {
            return;
        }
        if let Some(start_y) = self.chrome_swipe_start {
            if matches!(self.pointer_source, PointerSource::Pen) {
                // Pen must never complete a panel-reveal gesture.
                self.chrome_swipe_start = None;
            } else if !self.chrome_peeking && kiosk_enabled() && ly - start_y >= CHROME_SWIPE_MIN_DY
            {
                self.chrome_swipe_start = None;
                self.begin_chrome_peek();
                return;
            }
        }
        // While inking, skip Slint pointer dispatch — it dirties UI regions and
        // upgrades Fast ink presents to Partial (and costs CPU every sample).
        if self.is_drawing {
            if !self.ui.is_canvas_active() {
                self.is_drawing = false;
            }
        } else {
            self.ui.dispatch_pointer_moved(lx, ly);
        }
        // Move-tool lasso / drag tracking (independent of is_drawing).
        if self.select_gesture.is_some() && self.is_move_tool() {
            self.handle_select_move(lx, ly);
            return;
        }

        if self.is_drawing {
            if pressure > 0.05 {
                self.last_ink_pressure = pressure;
            }
            // After a smart-shape snap, ignore further moves so the engine tip
            // stays put until PenUp commits the fitted stroke.
            if self.shape_hold.as_ref().is_some_and(|h| h.snapped) {
                self.request_redraw_coalesced();
                return;
            }
            if let Some(h) = self.shape_hold.as_mut() {
                let far = (lx - h.last_x).abs() > SHAPE_HOLD_SLOP
                    || (ly - h.last_y).abs() > SHAPE_HOLD_SLOP;
                if far {
                    h.quiet_since = Instant::now();
                    h.last_x = lx;
                    h.last_y = ly;
                }
            }
            let cy = ly - STATUS_BAR_H as f32;
            if cy >= 0.0 {
                self.ink_trace.on_drained(t_us);
                self.ui.feed_input(
                    &InputEvent::PenMove(PenSample {
                        x: lx,
                        y: cy,
                        pressure,
                        tilt: 0.0,
                        in_range: true,
                        t_us,
                    }),
                    t_ms_from_us(t_us),
                );
                self.ink_trace.on_feed_done(t_us);
                // Do not persist mid-stroke / mid-erase — sync disk I/O on every
                // sample freezes PineNote. Persist on pointer release only.
            }
            self.request_redraw_coalesced();
        } else {
            // Idle moves over a static note canvas: skip redraw wake (eink).
            // Overlays / chrome swipe / peek still need Slint frames.
            // Stylus cursor is cleared by DesktopWindow::poll_tablet_cursor
            // (tablet_v2); wl_pointer hide stays on focus/startup.
            let need = self.chrome_peeking
                || self.chrome_swipe_start.is_some()
                || !self.ui.is_canvas_active()
                || self.ui.has_ink_damage();
            if need {
                self.request_redraw_coalesced();
            }
        }
    }

    /// Ask for at most one redraw until `RedrawRequested` clears the flag.
    fn request_redraw_coalesced(&mut self) {
        if self.redraw_pending {
            return;
        }
        self.redraw_pending = true;
        if let Some(d) = &self.display {
            d.request_redraw();
        }
    }

    fn handle_pointer_press(&mut self, lx: f32, ly: f32, pressure: f32, t_us: u64) {
        use ephemeris_ui::{STATUS_BAR_H, TOOLBAR_H};
        let (_, ui_h) = self.ui.size();
        self.cursor_pos = (lx, ly);
        self.chrome_pointer_held = true;
        self.page_swipe_handled.set(false);
        self.pen_ink_skip_slint = false;

        if self.chrome_peeking && self.chrome_dismiss_armed {
            self.end_chrome_peek();
            self.chrome_swallow = true;
            self.chrome_swipe_start = None;
            if let Some(d) = &self.display {
                d.request_redraw();
            }
            return;
        }

        // Finger/mouse only — pen near the top edge must not pull the Wayland
        // panel (Mutter overview / GNOME top bar) while writing.
        if !self.chrome_peeking
            && kiosk_enabled()
            && ly <= CHROME_SWIPE_EDGE_Y
            && !matches!(self.pointer_source, PointerSource::Pen)
        {
            self.chrome_swipe_start = Some(ly);
        } else {
            self.chrome_swipe_start = None;
        }

        // Bring the window forward only when unfocused — focus_window() every
        // pen-down is a compositor activation stall on PineNote.
        if let Some(d) = &self.display {
            d.focus_if_unfocused();
        }

        let pen_only = self.config.borrow().pen_only_drawing;
        let allow_ink = match self.pointer_source {
            PointerSource::Pen | PointerSource::Mouse => true,
            PointerSource::Finger => !pen_only,
        };

        if !self.is_move_tool() && self.select_gesture.is_some() {
            self.cancel_select_gesture();
        }

        // Move / lasso tool: select+drag path — never feed the ink engine.
        if self.is_move_tool()
            && allow_ink
            && !self.ctrl_held.get()
            && self.ui.hits_inkable_canvas(lx, ly)
        {
            self.pen_ink_skip_slint = matches!(self.pointer_source, PointerSource::Pen);
            self.canvas_touch_down.set(true);
            self.handle_select_press(lx, ly);
            self.request_redraw_coalesced();
            return;
        }

        // Pen on the active ink plane: skip Slint press dispatch (dirties UI /
        // can upgrade Fast→Partial). Geometry gate replaces canvas TouchArea.
        let pen_fast = matches!(self.pointer_source, PointerSource::Pen)
            && allow_ink
            && !self.ctrl_held.get()
            && self.ui.hits_inkable_canvas(lx, ly);
        if pen_fast {
            self.pen_ink_skip_slint = true;
            self.canvas_touch_down.set(true);
            self.is_drawing = true;
            self.arm_shape_hold(lx, ly);
            self.bg_recog.on_pen_down();
            self.last_ink_pressure = pressure.max(0.05);
            let cy = ly - STATUS_BAR_H as f32;
            self.ink_trace.on_drained(t_us);
            self.ui.feed_input(
                &InputEvent::PenDown(PenSample {
                    x: lx,
                    y: cy,
                    pressure,
                    tilt: 0.0,
                    in_range: true,
                    t_us,
                }),
                t_ms_from_us(t_us),
            );
            self.ink_trace.on_feed_done(t_us);
            self.request_redraw_coalesced();
            return;
        }

        // Reset canvas-touch gate before dispatching so we can detect whether
        // the canvas TouchArea (vs. chrome) got this press.
        self.canvas_touch_down.set(false);
        let canvas_was_active = self.ui.is_canvas_active();
        self.ui.dispatch_pointer_pressed(lx, ly);
        let in_canvas = ly > STATUS_BAR_H as f32 && ly < ui_h as f32 - TOOLBAR_H as f32;
        if in_canvas
            && allow_ink
            && !self.ctrl_held.get()
            && canvas_was_active
            && self.ui.is_canvas_active()
            && self.canvas_touch_down.get()
        {
            self.is_drawing = true;
            self.arm_shape_hold(lx, ly);
            self.bg_recog.on_pen_down();
            self.last_ink_pressure = pressure.max(0.05);
            let cy = ly - STATUS_BAR_H as f32;
            self.ink_trace.on_drained(t_us);
            self.ui.feed_input(
                &InputEvent::PenDown(PenSample {
                    x: lx,
                    y: cy,
                    pressure,
                    tilt: 0.0,
                    in_range: true,
                    t_us,
                }),
                t_ms_from_us(t_us),
            );
            self.ink_trace.on_feed_done(t_us);
        }
        self.request_redraw_coalesced();
    }

    /// Start (or clear) hold-to-snap tracking for the new stroke.
    fn arm_shape_hold(&mut self, lx: f32, ly: f32) {
        if self.config.borrow().note_smart_shapes {
            self.shape_hold = Some(ShapeHoldState {
                quiet_since: Instant::now(),
                last_x: lx,
                last_y: ly,
                snapped: false,
            });
        } else {
            self.shape_hold = None;
        }
    }

    fn is_move_tool(&self) -> bool {
        self.ui.get_active_tool() == "move"
    }

    /// Abandon lasso/drag; restore any strokes hidden for a move preview.
    fn cancel_select_gesture(&mut self) {
        match self.select_gesture.take() {
            Some(SelectGesture::Dragging { hide_damage, .. }) => {
                self.ui.restore_hidden_strokes(&hide_damage);
                self.ui.clear_select_preview();
            }
            Some(_) => self.ui.clear_select_preview(),
            None => {}
        }
    }

    /// Forget the selection after a page switch without touching the new
    /// page's committed raster (hidden-stroke restore is page-specific).
    fn drop_select_gesture(&mut self) {
        if self.select_gesture.take().is_some() {
            self.ui.clear_select_preview();
            self.request_redraw_coalesced();
        }
    }

    fn handle_select_press(&mut self, lx: f32, ly: f32) {
        use ephemeris_ui::STATUS_BAR_H;
        let cy = ly - STATUS_BAR_H as f32;
        if cy < 0.0 {
            return;
        }

        if let Some(SelectGesture::Selected { ids, bbox, outline }) = self.select_gesture.take() {
            let hit = lx >= bbox.min_x - SELECT_HIT_PAD
                && lx <= bbox.max_x + SELECT_HIT_PAD
                && cy >= bbox.min_y - SELECT_HIT_PAD
                && cy <= bbox.max_y + SELECT_HIT_PAD;
            if hit {
                let originals: Vec<_> = self
                    .ui
                    .get_page_strokes()
                    .into_iter()
                    .filter(|s| ids.contains(&s.id))
                    .collect();
                let hide_damage = self.ui.hide_strokes_for_move(&ids);
                // Passing the outline as prior damage erases it on screen.
                let preview_damage = self.ui.set_move_preview(&originals, outline);
                self.select_gesture = Some(SelectGesture::Dragging {
                    ids,
                    originals,
                    origin_x: lx,
                    origin_y: cy,
                    hide_damage,
                    preview_damage,
                    dx: 0.0,
                    dy: 0.0,
                });
                return;
            }
            self.ui.clear_select_preview();
        } else if matches!(
            self.select_gesture,
            Some(SelectGesture::Dragging { .. }) | Some(SelectGesture::Lassoing { .. })
        ) {
            self.cancel_select_gesture();
        }

        let preview_damage = self
            .ui
            .set_lasso_preview(&[(lx, cy)], ephemeris_core::geom::Rect::empty());
        self.select_gesture = Some(SelectGesture::Lassoing {
            points: vec![(lx, cy)],
            preview_damage,
        });
    }

    /// Record the latest lasso point / drag offset. Rendering is deferred to
    /// [`Self::flush_select_preview`] so a burst of pen samples costs one
    /// raster pass instead of one per sample (PineNote froze otherwise).
    fn handle_select_move(&mut self, lx: f32, ly: f32) {
        use ephemeris_ui::STATUS_BAR_H;
        let cy = ly - STATUS_BAR_H as f32;
        match self.select_gesture.as_mut() {
            Some(SelectGesture::Lassoing { points, .. }) => {
                points.push((lx, cy));
            }
            Some(SelectGesture::Dragging {
                origin_x,
                origin_y,
                dx,
                dy,
                ..
            }) => {
                *dx = lx - *origin_x;
                *dy = cy - *origin_y;
            }
            _ => return,
        }
        self.select_preview_dirty = true;
        self.request_redraw_coalesced();
    }

    /// Redraw the lasso / move preview once for all moves since the last call.
    fn flush_select_preview(&mut self) {
        if !std::mem::take(&mut self.select_preview_dirty) {
            return;
        }
        match self.select_gesture.as_mut() {
            Some(SelectGesture::Lassoing {
                points,
                preview_damage,
            }) => {
                *preview_damage = self.ui.set_lasso_preview(points, *preview_damage);
            }
            Some(SelectGesture::Dragging {
                originals,
                preview_damage,
                dx,
                dy,
                ..
            }) => {
                let preview: Vec<_> = originals
                    .iter()
                    .map(|s| {
                        let mut c = s.clone();
                        ephemeris_core::ink::translate_stroke(&mut c, *dx, *dy);
                        c
                    })
                    .collect();
                *preview_damage = self.ui.set_move_preview(&preview, *preview_damage);
            }
            _ => {}
        }
    }

    fn handle_select_release(&mut self, lx: f32, ly: f32) {
        use ephemeris_ui::STATUS_BAR_H;
        let cy = ly - STATUS_BAR_H as f32;
        match self.select_gesture.take() {
            Some(SelectGesture::Lassoing { points, .. }) => {
                let path_len: f32 = points
                    .windows(2)
                    .map(|w| {
                        let dx = w[1].0 - w[0].0;
                        let dy = w[1].1 - w[0].1;
                        (dx * dx + dy * dy).sqrt()
                    })
                    .sum();
                self.ui.clear_select_preview();
                if path_len < LASSO_MIN_PATH || points.len() < 3 {
                    return;
                }
                let ids = self.ui.strokes_inside_lasso(&points);
                if ids.is_empty() {
                    return;
                }
                self.enter_selected(ids);
            }
            Some(SelectGesture::Dragging {
                ids,
                hide_damage,
                origin_x,
                origin_y,
                ..
            }) => {
                // Prefer release point if the last move was coalesced away.
                let dx = lx - origin_x;
                let dy = cy - origin_y;
                self.ui.clear_select_preview();
                if dx.abs() > 0.5 || dy.abs() > 0.5 {
                    self.ui.translate_strokes(&ids, dx, dy);
                    self.record_ink_edit(InkEdit::Translate {
                        ids: ids.clone(),
                        dx,
                        dy,
                    });
                    // Persist + sync_strokes / See-text / note_tasks; arm idle HWR.
                    // Do not call on_pen_down during the select gesture.
                    self.persist_note_page_and_schedule_export();
                    self.bg_recog.on_pen_up();
                    self.sync_undo_redo_ui();
                } else {
                    self.ui.restore_hidden_strokes(&hide_damage);
                }
                self.enter_selected(ids);
            }
            Some(other) => {
                // Selected without a drag: keep it.
                self.select_gesture = Some(other);
            }
            None => {}
        }
    }

    /// Enter the Selected state and outline the selection on the live layer.
    fn enter_selected(&mut self, ids: Vec<ephemeris_core::StrokeId>) {
        let bbox = Self::bbox_for_ids(&self.ui.get_page_strokes(), &ids);
        let outline = self
            .ui
            .set_selection_outline(bbox, ephemeris_core::geom::Rect::empty());
        self.select_gesture = Some(SelectGesture::Selected { ids, bbox, outline });
    }

    fn bbox_for_ids(
        strokes: &[ephemeris_core::Stroke],
        ids: &[ephemeris_core::StrokeId],
    ) -> ephemeris_core::geom::Rect {
        let mut bbox = ephemeris_core::geom::Rect::empty();
        for s in strokes {
            if ids.contains(&s.id) {
                bbox = bbox.union(&s.approx_bbox());
            }
        }
        bbox
    }

    fn current_note_page(&self) -> Option<(u32, usize)> {
        let page_idx = self.ui.current_page_idx();
        self.notes
            .borrow()
            .iter()
            .find(|n| n.page_index == page_idx)
            .map(|n| (n.id, n.doc_page))
    }

    /// Record strokes the eraser removed during a gesture that ended without
    /// a normal PenUp (page swipe / clear), so they stay undoable and do not
    /// leak into the next stroke's history entry.
    fn record_pending_erase(&self) {
        let erased = self.ui.take_eraser_removed();
        if !erased.is_empty() {
            self.record_ink_edit(InkEdit::Remove(erased));
        }
    }

    fn record_ink_edit(&self, edit: InkEdit) {
        let Some((note_id, doc_page)) = self.current_note_page() else {
            return;
        };
        self.ink_history.borrow_mut().push(note_id, doc_page, edit);
    }

    fn sync_undo_redo_ui(&self) {
        if let Some((note_id, doc_page)) = self.current_note_page() {
            let h = self.ink_history.borrow();
            self.ui.set_can_undo(h.can_undo(note_id, doc_page));
            self.ui.set_can_redo(h.can_redo(note_id, doc_page));
        } else {
            self.ui.set_can_undo(false);
            self.ui.set_can_redo(false);
        }
    }

    /// Apply a pending Undo / Redo / Clear from the drawing menu or shortcut.
    fn process_pending_ink_cmd(&mut self) {
        let Some(cmd) = self.pending_ink_cmd.take() else {
            return;
        };
        // Undo/redo mid-gesture is dropped (not deferred): replaying it after
        // PenUp would immediately revert the stroke the user just drew.
        let mid_gesture = self.is_drawing
            || matches!(
                self.select_gesture,
                Some(SelectGesture::Lassoing { .. }) | Some(SelectGesture::Dragging { .. })
            );
        if mid_gesture && !matches!(cmd, PendingInkCmd::Clear) {
            return;
        }
        if self.is_drawing {
            self.is_drawing = false;
            self.shape_hold = None;
            self.bg_recog.set_drawing(false);
            self.ui.discard_in_progress_stroke();
            self.record_pending_erase();
        }
        // An idle selection would go stale once strokes change underneath it.
        self.cancel_select_gesture();
        match cmd {
            PendingInkCmd::Undo => self.perform_ink_undo(),
            PendingInkCmd::Redo => self.perform_ink_redo(),
            PendingInkCmd::Clear => self.perform_ink_clear(),
        }
    }

    fn perform_ink_undo(&mut self) {
        let Some((note_id, doc_page)) = self.current_note_page() else {
            return;
        };
        let Some(op) = self.ink_history.borrow_mut().undo(note_id, doc_page) else {
            return;
        };
        self.bg_recog.on_pen_down(); // cancel in-flight HWR
        if self.ui.apply_ink_op(&op) {
            self.persist_note_page_and_schedule_export();
            self.bg_recog.on_pen_up();
        }
        self.sync_undo_redo_ui();
        self.request_redraw_coalesced();
    }

    fn perform_ink_redo(&mut self) {
        let Some((note_id, doc_page)) = self.current_note_page() else {
            return;
        };
        let Some(op) = self.ink_history.borrow_mut().redo(note_id, doc_page) else {
            return;
        };
        self.bg_recog.on_pen_down();
        if self.ui.apply_ink_op(&op) {
            self.persist_note_page_and_schedule_export();
            self.bg_recog.on_pen_up();
        }
        self.sync_undo_redo_ui();
        self.request_redraw_coalesced();
    }

    fn perform_ink_clear(&mut self) {
        self.cancel_select_gesture();
        let before = self.ui.get_page_strokes();
        if before.is_empty() {
            return;
        }
        self.record_ink_edit(InkEdit::Clear { before });
        self.ui.clear_ink();
        self.persist_note_page_and_schedule_export();
        self.bg_recog.on_pen_up();
        self.sync_undo_redo_ui();
        self.request_redraw_coalesced();
    }

    /// Fire smart-shape snap once the tip has been still for [`SHAPE_HOLD`].
    fn poll_shape_hold(&mut self) -> bool {
        if !self.is_drawing {
            return false;
        }
        let Some(h) = self.shape_hold.as_ref() else {
            return false;
        };
        if h.snapped || h.quiet_since.elapsed() < SHAPE_HOLD {
            return false;
        }
        if self.ui.try_snap_in_progress_shape() {
            if let Some(h) = self.shape_hold.as_mut() {
                h.snapped = true;
            }
            true
        } else {
            // Fit failed — wait another hold period before retrying.
            if let Some(h) = self.shape_hold.as_mut() {
                h.quiet_since = Instant::now();
            }
            false
        }
    }

    /// Persist raster + strokes when the live stroke list changed (erase / clear).
    fn persist_if_strokes_dirty(&mut self) {
        if !self.ui.take_strokes_dirty() {
            return;
        }
        self.persist_note_page_and_schedule_export();
    }

    /// Queue debounced local page files; mark the note for deferred vault OCR/PDF.
    fn persist_note_page_and_schedule_export(&mut self) {
        let _stall = StallWatch::new("persist_note_page");
        let page_idx = self.ui.current_page_idx();
        let note_info = self
            .notes
            .borrow()
            .iter()
            .find(|n| n.page_index == page_idx)
            .map(|n| (n.id, n.doc_page));
        let Some((note_id, doc_page)) = note_info else {
            return;
        };
        // Arc snapshot for the writer; then re-unique the live layer so the next
        // stroke does not COW the full panel mid-draw.
        let pixels = self.ui.get_committed_pixels_arc();
        let (w, h) = (pixels.width, pixels.height);
        let strokes = self.ui.get_page_strokes();
        self.local_page_writes.queue(PendingPageWrite {
            note_id,
            doc_page,
            width: w,
            height: h,
            pixels,
            strokes: strokes.clone(),
        });
        self.ui.ensure_committed_unique();

        // Incremental recognition cache: invalidate only lines whose stroke ids
        // changed — do not wipe the whole page's text_content.
        let version = recognition_model_version_for(&self.config.borrow());
        let ruling = note_ruling_pitch(&self.ui);
        let mut cache = self
            .bg_recog
            .get_or_load(&self.data_dir, note_id, doc_page, &version);
        cache.sync_strokes(&strokes, ruling, &version);
        if let Err(e) = cache.save(&self.data_dir, note_id, doc_page) {
            tracing::warn!("Failed to save recognition cache for note {note_id}: {e}");
        }
        let completed = cache.completed_markdown();
        let rows = see_text_rows(&cache, Some(&completed));
        let see_text_open = self.ui.clone_component().get_note_see_text();
        (self.sync_note_tasks)(note_id, doc_page, &cache);
        self.bg_recog.store(note_id, doc_page, cache);
        if let Some(note) = self.notes.borrow_mut().iter_mut().find(|n| n.id == note_id) {
            note.updated_at = now_secs();
            note.text_content = if completed.is_empty() {
                None
            } else {
                Some(completed)
            };
        }
        // Update See-text overlay in place when open (pending markers); otherwise
        // leave Slint alone mid-ink to avoid UI damage.
        if see_text_open {
            self.ui.set_see_text_rows(&rows);
        }
        // Vault export waits for leave-note or the 10‑minute batch — never per stroke.
        self.vault_export.mark(note_id);
    }

    fn handle_pointer_release(&mut self, lx: f32, ly: f32, t_us: u64) {
        use ephemeris_ui::STATUS_BAR_H;
        self.cursor_pos = (lx, ly);
        self.chrome_pointer_held = false;
        self.chrome_swipe_start = None;

        if self.chrome_swallow {
            self.chrome_swallow = false;
            if let Some(d) = &self.display {
                d.request_redraw();
            }
            return;
        }

        if self.chrome_peeking {
            // Arm dismiss after the revealing swipe lifts.
            self.chrome_dismiss_armed = true;
        }

        let skip_slint = self.pen_ink_skip_slint;
        self.pen_ink_skip_slint = false;

        if !skip_slint {
            // Ensure Slint TouchArea.mouse-x/y reflect the release point before up.
            // On PineNote, Touch Ended often arrives without a final Moved at the
            // lift coords — without this, page-swipe delta stays under threshold.
            self.ui.dispatch_pointer_moved(lx, ly);
            // Dispatch release so Slint swipe callbacks can set `page_swipe_handled`
            // before we decide whether to commit ink.
            self.ui.dispatch_pointer_released(lx, ly);
        }
        if !skip_slint && self.page_swipe_handled.get() {
            if self.is_drawing {
                self.is_drawing = false;
                self.shape_hold = None;
                self.bg_recog.set_drawing(false);
                self.ui.discard_in_progress_stroke();
                self.record_pending_erase();
                self.sync_undo_redo_ui();
            }
            if self.select_gesture.is_some() {
                self.cancel_select_gesture();
            }
            self.page_swipe_handled.set(false);
            if let Some(d) = &self.display {
                d.request_redraw();
            }
            return;
        }
        if self.select_gesture.is_some() && self.is_move_tool() {
            self.handle_select_release(lx, ly);
            if let Some(d) = &self.display {
                d.request_redraw();
            }
            return;
        }
        if self.is_drawing {
            self.is_drawing = false;
            self.shape_hold = None;
            let cy = ly - STATUS_BAR_H as f32;
            self.ink_trace.on_drained(t_us);
            let update = self.ui.feed_input(
                &InputEvent::PenUp(PenSample {
                    x: lx,
                    y: cy,
                    // Hold last contact pressure — 0.0 collapses tip width.
                    pressure: self.last_ink_pressure,
                    tilt: 0.0,
                    in_range: true,
                    t_us,
                }),
                t_ms_from_us(t_us),
            );
            self.ink_trace.on_feed_done(t_us);
            // Record undo history (pen add or eraser batch) before persist.
            let erased = self.ui.take_eraser_removed();
            if !erased.is_empty() {
                self.record_ink_edit(InkEdit::Remove(erased));
            } else if let InkUpdate::Finished { stroke, .. } = &update {
                // Look up the stored stroke: a smart-shape snap replaces the
                // engine's freehand points, and redo must restore the shape.
                if stroke.tool != Tool::Eraser {
                    if let Some(stored) = self.ui.page_stroke(stroke.id) {
                        self.record_ink_edit(InkEdit::Add(vec![stored]));
                    }
                }
            }
            // Persist after pen commit / erase gesture end. Keep local disk
            // writes on the UI thread (fast) but never run vault OCR/PDF here.
            if matches!(update, InkUpdate::Finished { .. }) || self.ui.take_strokes_dirty() {
                self.persist_note_page_and_schedule_export();
            }
            // Arm idle background recognition (≥ 1.5 s after last PenUp).
            self.bg_recog.on_pen_up();
            self.sync_undo_redo_ui();
        } else {
            // Clear / erase that finished without an active draw still needs save.
            self.persist_if_strokes_dirty();
        }
        if let Some(d) = &self.display {
            d.request_redraw();
        }
    }

    /// Drain Linux evdev pen samples into the same pointer/ink paths as touch.
    #[cfg(target_os = "linux")]
    fn drain_pen_events(&mut self) {
        let Some(src) = &self.pen_source else {
            return;
        };
        let events: Vec<InputEvent> = src.try_iter().collect();
        for ev in events {
            // Off-thread JSONL capture (EPHEMERIS_INK_RECORD); try_send only.
            if let Some(rec) = &self.ink_recorder {
                rec.record(&ev);
            }
            match ev {
                InputEvent::PenDown(s) => {
                    self.pointer_source = PointerSource::Pen;
                    self.page_swipe_allowed.set(false);
                    self.handle_pointer_press(s.x, s.y, s.pressure, s.t_us);
                }
                InputEvent::PenMove(s) => {
                    self.pointer_source = PointerSource::Pen;
                    self.handle_pointer_move(s.x, s.y, s.pressure, s.t_us);
                }
                InputEvent::PenUp(s) => {
                    self.pointer_source = PointerSource::Pen;
                    if s.pressure > 0.05 {
                        self.last_ink_pressure = s.pressure;
                    }
                    self.handle_pointer_release(s.x, s.y, s.t_us);
                }
                InputEvent::PenButton { pressed } => {
                    // Side button ≈ Ctrl (opens tool ring / modifier paths).
                    self.ctrl_held.set(pressed);
                }
                InputEvent::Hover { x, y, .. } => {
                    self.cursor_pos = (x, y);
                    self.ui.dispatch_pointer_moved(x, y);
                }
                _ => {}
            }
        }
    }
}

impl ApplicationHandler<AppUserEvent> for WinitHandler {
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: AppUserEvent) {
        let _stall = StallWatch::new("user_event");
        match event {
            AppUserEvent::PenReady => {
                #[cfg(target_os = "linux")]
                self.drain_pen_events();
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let _stall = StallWatch::new("about_to_wait");
        self.process_pending_ink_cmd();
        // Page / note switches change which history the drawing menu reflects
        // and leave any selection pointing at the previous page's strokes.
        let note_page = self.current_note_page();
        if note_page != self.undo_ui_page {
            self.undo_ui_page = note_page;
            self.sync_undo_redo_ui();
            self.drop_select_gesture();
        }
        // Switching away from the Move tool drops the selection outline now,
        // not on the next pen-down.
        if self.select_gesture.is_some() && !self.is_move_tool() {
            self.cancel_select_gesture();
            self.request_redraw_coalesced();
        }
        #[cfg(target_os = "linux")]
        self.drain_pen_events();
        self.flush_select_preview();
        // Hide Mutter tablet-tool cursor (zwp_tablet_tool_v2.set_cursor on
        // winit's wl_display). Must run on the same connection as the window.
        if let Some(d) = &mut self.display {
            d.poll_tablet_cursor();
        }
        // Kiosk watchdog: re-cover the output unless we're peeking the panel.
        // Skip while drawing — enter_kiosk/resize resets softbuffer age→0 and
        // forces a full-panel rewrite mid-stroke (PineNote force-quit).
        if kiosk_enabled()
            && !self.is_drawing
            && self.last_kiosk_check.elapsed() >= Duration::from_millis(250)
        {
            self.last_kiosk_check = Instant::now();
            self.on_kiosk_geometry();
        }
        // --- Drain whisper download result ---
        if let Ok(mut guard) = self.download_result.lock() {
            if let Some(result) = guard.take() {
                drop(guard);
                match result {
                    Ok(()) => {
                        self.ui.set_whisper_downloading(false);
                        self.ui.set_whisper_model_installed(true);
                    }
                    Err(msg) => {
                        tracing::error!("Whisper download failed: {msg}");
                        self.ui.set_whisper_downloading(false);
                        self.ui.set_whisper_model_installed(false);
                    }
                }
                if let Some(d) = &self.display {
                    d.request_redraw();
                }
            }
        }

        // --- Drain HTR download result ---
        if let Ok(mut guard) = self.htr_download_result.lock() {
            if let Some(result) = guard.take() {
                drop(guard);
                match result {
                    Ok(()) => {
                        self.ui.set_htr_downloading(false);
                        self.ui.set_htr_model_installed(true);
                    }
                    Err(msg) => {
                        tracing::error!("HTR download failed: {msg}");
                        self.ui.set_htr_downloading(false);
                        self.ui.set_htr_model_installed(find_htr_model_installed(
                            &self.config.borrow(),
                        ));
                    }
                }
                if let Some(d) = &self.display {
                    d.request_redraw();
                }
            }
        }

        // --- Drain line-image HTR download result ---
        if let Ok(mut guard) = self.line_htr_download_result.lock() {
            if let Some(result) = guard.take() {
                drop(guard);
                match result {
                    Ok(()) => {
                        self.ui.set_line_htr_downloading(false);
                        self.ui.set_line_htr_model_installed(true);
                    }
                    Err(msg) => {
                        tracing::error!("Line HTR download failed: {msg}");
                        self.ui.set_line_htr_downloading(false);
                        self.ui
                            .set_line_htr_model_installed(find_line_htr_model_installed(
                                &self.config.borrow(),
                            ));
                    }
                }
                if let Some(d) = &self.display {
                    d.request_redraw();
                }
            }
        }

        // --- Debounced local page .bin / strokes JSON ---
        self.local_page_writes.try_flush_async(false);

        // --- On-screen keyboard follows the UI's text-entry dialogs ---
        let text_entry = self.ui.text_entry_active();
        if text_entry != self.text_entry_active {
            self.text_entry_active = text_entry;
            if let Some(d) = &self.display {
                d.set_text_entry(text_entry);
            }
        }

        // --- Canvas: long-press fires while the finger is still down ---
        if crate::canvas_app::poll_board_long_press(&self.ui, &self.canvas_session) {
            self.request_redraw_coalesced();
        }

        // --- Notes: hold-still smart-shape snap while the tip is still down ---
        if self.poll_shape_hold() {
            self.request_redraw_coalesced();
        }

        // --- Canvas: finished handwriting → text conversions ---
        if crate::canvas_app::poll_card_recognition(&self.ui, &self.canvas_session) {
            self.request_redraw_coalesced();
        }

        // --- Deferred vault OCR/PDF (leave-note or 10‑minute batch) ---
        self.vault_export.try_flush(
            &self.config.borrow(),
            &self.notes.borrow(),
            &self.notebooks.borrow(),
            &self.data_dir,
            &self.connections.borrow(),
            &self.local_page_writes,
        );

        // --- Idle background per-line recognition (ktg.12) ---
        if self.bg_recog.may_spawn(false) {
            let page_idx = self.ui.current_page_idx();
            let note_info = self
                .notes
                .borrow()
                .iter()
                .find(|n| n.page_index == page_idx)
                .map(|n| (n.id, n.doc_page));
            if let Some((note_id, doc_page)) = note_info {
                let strokes = self.ui.get_page_strokes();
                let ruling = note_ruling_pitch(&self.ui);
                self.bg_recog.try_spawn(
                    false,
                    &self.config.borrow(),
                    &self.data_dir,
                    note_id,
                    doc_page,
                    strokes,
                    ruling,
                );
            }
        }
        if let Ok(mut guard) = self.bg_recog.result.lock() {
            if let Some(mut job) = guard.take() {
                drop(guard);
                // See-text edits saved while the job ran must not be clobbered.
                if let Some(current) = self.bg_recog.current(job.note_id, job.doc_page) {
                    if job.cache.adopt_corrections_from(&current) {
                        if let Err(e) = job.cache.save(&self.data_dir, job.note_id, job.doc_page) {
                            tracing::warn!(
                                "Failed to save recognition cache for note {}: {e}",
                                job.note_id
                            );
                        }
                    }
                }
                let completed = job.cache.completed_markdown();
                let rows = see_text_rows(&job.cache, Some(&completed));
                self.bg_recog
                    .store(job.note_id, job.doc_page, job.cache.clone());
                let mut did_persist = false;
                {
                    let mut ns = self.notes.borrow_mut();
                    if let Some(note) = ns.iter_mut().find(|n| n.id == job.note_id) {
                        // Only update text when this job matches the note's current page.
                        if note.doc_page == job.doc_page {
                            note.text_content = if completed.is_empty() {
                                None
                            } else {
                                Some(completed)
                            };
                            note.updated_at = now_secs();
                            did_persist = true;
                        }
                    }
                }
                if did_persist {
                    (self.sync_note_tasks)(job.note_id, job.doc_page, &job.cache);
                    save_notes_index_and_sync_vault(
                        &self.data_dir,
                        &self.notes.borrow(),
                        &self.notebooks.borrow(),
                        self.note_id_seq.get(),
                        self.notebook_id_seq.get(),
                        &self.config.borrow(),
                        &self.vault_export,
                    );
                }
                let see_text_open = self.ui.clone_component().get_note_see_text();
                let active_id = self
                    .ui
                    .clone_component()
                    .get_active_note_id()
                    .to_string()
                    .parse::<u32>()
                    .ok();
                if see_text_open && active_id == Some(job.note_id) {
                    self.ui.set_see_text_rows(&rows);
                    self.ui.request_screen_change();
                    if let Some(d) = &self.display {
                        d.request_redraw();
                    }
                }
                // If pending lines remain and idle is armed, another spawn may follow.
                if job.cache.has_pending() && !self.bg_recog.idle.borrow().is_drawing() {
                    self.bg_recog.on_pen_up();
                }
            }
        }

        // --- Drain completed transcriptions ---
        if let Ok(mut pending) = self.transcription_pending.lock() {
            if !pending.is_empty() {
                let drained: Vec<(u32, String)> = pending.drain(..).collect();
                drop(pending);
                let mut recs = self.recordings.borrow_mut();
                let mut updated = false;
                let ui2 = self.ui.clone_component();
                let active_open: u32 = ui2.get_rec_active_id().parse().unwrap_or(0);
                for (id, text) in drained {
                    self.transcription_in_flight.borrow_mut().remove(&id);
                    if let Some(rec) = recs.iter_mut().find(|r| r.id == id) {
                        rec.transcription = Some(text.clone());
                        updated = true;
                        if id == active_open && ui2.get_rec_transcription_visible() {
                            ui2.set_rec_transcription_text(slint::SharedString::from(
                                text.as_str(),
                            ));
                        }
                    }
                }
                if updated {
                    save_recordings_index(&self.rec_dir, &recs, self.rec_id_seq.get());
                    drop(recs);
                    (self.push_recordings)();
                    if let Some(d) = &self.display {
                        d.request_redraw();
                    }
                } else {
                    drop(recs);
                }
                // Kick the next queued job now that the worker slot is free.
                let lang = ui2.get_whisper_lang();
                pump_transcription_queue(
                    &self.transcription_queue,
                    &self.transcription_active,
                    &self.transcription_pending,
                    &self.recordings,
                    &self.rec_dir,
                    &lang,
                    &self.transcription_in_flight,
                    &self.push_recordings,
                );
            }
        }

        // --- Poll recording duration ---
        let h_opt = self.rec_handle.borrow();
        if let Some(ref h) = *h_opt {
            let frames = h.frame_count.load(Ordering::Relaxed);
            let secs = frames / h.sample_rate as u64;
            let label = format_rec_duration(secs);
            let state = if h.paused.load(Ordering::Relaxed) {
                2i32
            } else {
                1i32
            };
            drop(h_opt);
            self.ui.set_rec_duration_label(&label);
            self.ui.set_rec_state(state);
            if let Some(d) = &self.display {
                d.request_redraw();
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(500),
            ));
        } else if self.transcription_active.load(Ordering::Relaxed)
            || self.download_active.load(Ordering::Relaxed)
            || self.htr_download_active.load(Ordering::Relaxed)
            || self.line_htr_download_active.load(Ordering::Relaxed)
            || self.bg_recog.active.load(Ordering::Relaxed)
            || self.bg_recog.has_pending_work()
            || self.caldav_active.load(Ordering::Relaxed)
            || self.vault_export.active.load(Ordering::Relaxed)
            || self.local_page_writes.active.load(Ordering::Relaxed)
        {
            // Keep waking while whisper / model download / bg recog / CalDAV /
            // vault export / local page-write workers run.
            drop(h_opt);
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(500),
            ));
        } else if self.local_page_writes.has_pending() {
            drop(h_opt);
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + LOCAL_PAGE_WRITE_DEBOUNCE,
            ));
        } else if self.vault_export.has_pending() {
            // Wake for the 10‑minute vault batch (or a leave-note flush_soon).
            drop(h_opt);
            let elapsed = self.vault_export.last_flush_at.get().elapsed();
            let remaining = VAULT_EXPORT_BATCH_INTERVAL.saturating_sub(elapsed);
            let wake = if self.vault_export.flush_soon.get() {
                Duration::from_millis(50)
            } else {
                remaining
                    .min(Duration::from_secs(60))
                    .max(Duration::from_millis(500))
            };
            event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + wake));
        }
        // Canvas long-press deadline / card recognition worker.
        // Every branch above re-arms a future deadline when it still needs one,
        // so a deadline already in the past here is stale.
        let now = Instant::now();
        let current = match event_loop.control_flow() {
            ControlFlow::WaitUntil(t) if t > now => Some(t),
            ControlFlow::WaitUntil(_) | ControlFlow::Wait => None,
            ControlFlow::Poll => Some(now),
        };
        let shape_wake = self.shape_hold.as_ref().and_then(|h| {
            if self.is_drawing && !h.snapped {
                Some(h.quiet_since + SHAPE_HOLD)
            } else {
                None
            }
        });
        let canvas_wake = crate::canvas_app::next_wake(&self.canvas_session);
        let earliest = match (shape_wake, canvas_wake) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        };
        match (earliest, current) {
            (Some(wake), Some(t)) if wake < t => {
                event_loop.set_control_flow(ControlFlow::WaitUntil(wake));
            }
            (Some(wake), None) => event_loop.set_control_flow(ControlFlow::WaitUntil(wake)),
            (None, None) => event_loop.set_control_flow(ControlFlow::Wait),
            _ => {}
        }

        // --- Drain completed CalDAV syncs ---
        if let Ok(mut pending) = self.caldav_pending.lock() {
            if !pending.is_empty() {
                let drained: Vec<CalendarEvent> = pending.drain(..).collect();
                drop(pending);
                {
                    let mut evts = self.calendar_events.borrow_mut();
                    // Replace prior events from the same CalDAV calendar URLs.
                    for ev in &drained {
                        let src = ev.source.clone();
                        if src.starts_with("caldav:") {
                            evts.retain(|e| e.source != src);
                        }
                    }
                    evts.extend(drained);
                }
                let filter = self.active_profile_filter.borrow().clone();
                let visible = filter_events_by_profile(&self.calendar_events.borrow(), &filter);
                push_agenda_comp(
                    &self.ui.clone_component(),
                    &visible,
                    AgendaRange::Day,
                    now_secs(),
                );
                if let Some(d) = &self.display {
                    d.request_redraw();
                }
            }
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let (w, h) = self.ui.size();
        match DesktopWindow::new(event_loop, w, h) {
            Ok(d) => {
                self.scale = d.scale_factor();
                if kiosk_enabled() {
                    d.enter_kiosk();
                }
                d.request_redraw();
                self.display = Some(d);
                self.chrome_was_covering = false;
                self.kiosk_settle_until = Some(Instant::now() + KIOSK_SETTLE);
                self.on_kiosk_geometry();
            }
            Err(e) => {
                tracing::error!("DesktopWindow creation failed: {e}");
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let _stall = StallWatch::new("window_event");
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::Resized(_) => {
                if let Some(d) = &mut self.display {
                    let _ = d.resize_surface();
                    d.request_redraw();
                }
                self.on_kiosk_geometry();
            }

            WindowEvent::Moved(_) => {
                self.on_kiosk_geometry();
            }

            WindowEvent::Focused(focused) => {
                if focused {
                    if let Some(d) = &self.display {
                        d.hide_cursor();
                    }
                }
                self.on_kiosk_geometry();
            }

            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale = scale_factor;
                if let Some(d) = &mut self.display {
                    let _ = d.resize_surface();
                    d.request_redraw();
                }
                self.on_kiosk_geometry();
            }

            // Track Ctrl/Shift so canvas-touch opens ring and Ctrl+Z/Y undo/redo.
            WindowEvent::ModifiersChanged(modifiers) => {
                use winit::keyboard::ModifiersState;
                let state = modifiers.state();
                self.ctrl_held.set(state.contains(ModifiersState::CONTROL));
                self.shift_held.set(state.contains(ModifiersState::SHIFT));
            }

            WindowEvent::CursorMoved { position, .. } => {
                let (lx, ly) = self.map_to_ui(position);
                self.handle_pointer_move(lx, ly, 0.6, 0);
            }

            WindowEvent::MouseInput {
                state,
                button: winit::event::MouseButton::Left,
                ..
            } => {
                let (lx, ly) = self.cursor_pos;
                match state {
                    winit::event::ElementState::Pressed => {
                        self.pointer_source = PointerSource::Mouse;
                        self.page_swipe_allowed.set(true);
                        self.handle_pointer_press(lx, ly, 0.8, 0);
                    }
                    winit::event::ElementState::Released => {
                        self.handle_pointer_release(lx, ly, 0);
                    }
                }
            }

            // PineNote / Wayland: finger contacts arrive as Touch. The EMR pen
            // is handled separately via evdev on Linux (`drain_pen_events`).
            // On the Canvas board, two fingers → pinch zoom (Slint only sees one
            // pointer, so we recognize the gesture here).
            WindowEvent::Touch(touch) => {
                use winit::event::TouchPhase;
                let (lx, ly) = self.map_to_ui(touch.location);
                let pressure = touch
                    .force
                    .map(|f| f.normalized() as f32)
                    .unwrap_or(0.8)
                    .clamp(0.05, 1.0);
                let board_open = {
                    let page = self.ui.clone_component();
                    page.get_show_canvas_board() && !page.get_canvas_focus_active()
                };
                if board_open {
                    self.handle_board_touch(touch.id, touch.phase, lx, ly, pressure);
                } else {
                    self.board_fingers.clear();
                    self.board_pinch_last_dist = None;
                    self.board_pinching = false;
                    match touch.phase {
                        TouchPhase::Started => {
                            self.pointer_source = PointerSource::Finger;
                            self.page_swipe_allowed.set(true);
                            self.handle_pointer_press(lx, ly, pressure, 0);
                        }
                        TouchPhase::Moved => {
                            self.pointer_source = PointerSource::Finger;
                            self.handle_pointer_move(lx, ly, pressure, 0);
                        }
                        TouchPhase::Ended | TouchPhase::Cancelled => {
                            self.handle_pointer_release(lx, ly, 0);
                        }
                    }
                }
            }

            // macOS trackpad pinch (same continuous scale as finger pinch).
            WindowEvent::PinchGesture { delta, phase, .. } => {
                use winit::event::TouchPhase;
                if !delta.is_finite() {
                    return;
                }
                let page = self.ui.clone_component();
                if !page.get_show_canvas_board() || page.get_canvas_focus_active() {
                    return;
                }
                let factor = (1.0 + delta) as f32;
                if factor > 0.0 {
                    let (uw, uh) = self.ui.size();
                    let (cx, cy) = self.cursor_pos;
                    let (sx, sy) = if cx > 1.0 || cy > 1.0 {
                        (cx, cy)
                    } else {
                        (uw as f32 * 0.5, uh as f32 * 0.5)
                    };
                    crate::canvas_app::apply_board_zoom_factor(
                        &self.ui,
                        &self.canvas_session,
                        factor,
                        sx,
                        sy,
                    );
                    self.request_redraw_coalesced();
                }
                if matches!(phase, TouchPhase::Ended | TouchPhase::Cancelled) {
                    self.canvas_session.save_doc();
                    crate::canvas_app::refresh_board_now(&self.ui, &self.canvas_session);
                    self.request_redraw_coalesced();
                }
            }

            WindowEvent::Ime(winit::event::Ime::Commit(text)) => {
                // On-screen keyboard text: one key press/release per char.
                for c in text.chars() {
                    let key = slint::SharedString::from(c.to_string().as_str());
                    self.ui.dispatch_key_pressed(key.clone());
                    self.ui.dispatch_key_released(key);
                }
                if let Some(d) = &self.display {
                    d.request_redraw();
                }
            }

            WindowEvent::KeyboardInput { event, .. } => {
                use winit::keyboard::{Key, NamedKey};
                // Desktop undo/redo (ephemeris-e1b): Ctrl+Z / Ctrl+Shift+Z / Ctrl+Y.
                // Text fields keep their own Ctrl+Z; only the note page owns ink undo.
                if event.state == winit::event::ElementState::Pressed
                    && self.ctrl_held.get()
                    && !self.ui.text_entry_active()
                    && self.ui.is_note_canvas_page()
                {
                    if let Key::Character(s) = &event.logical_key {
                        let lower = s.to_lowercase();
                        if lower == "z" {
                            if self.shift_held.get() {
                                self.pending_ink_cmd.set(Some(PendingInkCmd::Redo));
                            } else {
                                self.pending_ink_cmd.set(Some(PendingInkCmd::Undo));
                            }
                            self.process_pending_ink_cmd();
                            if let Some(d) = &self.display {
                                d.request_redraw();
                            }
                            return;
                        }
                        if lower == "y" {
                            self.pending_ink_cmd.set(Some(PendingInkCmd::Redo));
                            self.process_pending_ink_cmd();
                            if let Some(d) = &self.display {
                                d.request_redraw();
                            }
                            return;
                        }
                    }
                }
                let text: slint::SharedString = match &event.logical_key {
                    Key::Named(NamedKey::Backspace) => slint::platform::Key::Backspace.into(),
                    Key::Named(NamedKey::Delete) => slint::platform::Key::Delete.into(),
                    Key::Named(NamedKey::ArrowLeft) => slint::platform::Key::LeftArrow.into(),
                    Key::Named(NamedKey::ArrowRight) => slint::platform::Key::RightArrow.into(),
                    Key::Named(NamedKey::ArrowUp) => slint::platform::Key::UpArrow.into(),
                    Key::Named(NamedKey::ArrowDown) => slint::platform::Key::DownArrow.into(),
                    Key::Named(NamedKey::Enter) => slint::platform::Key::Return.into(),
                    Key::Named(NamedKey::Escape) => slint::platform::Key::Escape.into(),
                    Key::Named(NamedKey::Home) => slint::platform::Key::Home.into(),
                    Key::Named(NamedKey::End) => slint::platform::Key::End.into(),
                    Key::Named(NamedKey::Tab) => slint::platform::Key::Tab.into(),
                    Key::Character(s) => slint::SharedString::from(s.as_str()),
                    _ => return,
                };
                match event.state {
                    winit::event::ElementState::Pressed => {
                        self.ui.dispatch_key_pressed(text);
                    }
                    winit::event::ElementState::Released => {
                        self.ui.dispatch_key_released(text);
                    }
                }
                if let Some(d) = &self.display {
                    d.request_redraw();
                }
            }

            WindowEvent::RedrawRequested => {
                self.redraw_pending = false;
                if let Some(display) = &mut self.display {
                    let t0 = Instant::now();
                    if let Err(e) = self.ui.render_frame(display) {
                        tracing::error!("render_frame failed: {e}");
                    }
                    self.ink_trace.on_present(t0.elapsed().as_micros() as u64);
                }
            }
            _ => {}
        }
    }
}

// ── Profiles persistence ──────────────────────────────────────────────────────

#[derive(serde::Serialize, serde::Deserialize)]
struct ProfilesIndex {
    profiles: Vec<ephemeris_core::Profile>,
}

fn profiles_data_dir() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".local").join("share")
        });
    base.join("ephemeris")
}

fn profiles_index_path() -> PathBuf {
    profiles_data_dir().join("profiles.json")
}

fn save_profiles_index(mgr: &ProfileManager) {
    let profiles = mgr.list_profiles().unwrap_or_default();
    let index = ProfilesIndex { profiles };
    match serde_json::to_string(&index) {
        Ok(json) => {
            let path = profiles_index_path();
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::write(&path, json) {
                tracing::warn!("Failed to save profiles index: {e}");
            }
        }
        Err(e) => tracing::warn!("Failed to serialize profiles index: {e}"),
    }
}

fn load_profiles_index() -> Option<ProfilesIndex> {
    let json = std::fs::read_to_string(profiles_index_path()).ok()?;
    serde_json::from_str(&json).ok()
}

// ── Note + Notebook data models ───────────────────────────────────────────────

/// In-memory notebook / folder record.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct AppNotebook {
    id: u32,
    title: String,
    /// Profile this notebook belongs to; "" = legacy (visible in all profiles).
    #[serde(default)]
    profile_id: String,
    /// Parent folder id; `None` = root-level folder.
    #[serde(default)]
    parent_id: Option<u32>,
}

/// In-memory note record, parallel to a PageBook page at `page_index`.
///
/// `page_index` is the note's slot in the global [`EphemerisUi`] page book (one
/// slot per note).  Within a note, `doc_page` / `doc_page_count` track multi-page
/// documents; rasters are stored as `page_{id}_p{n}.bin` (page 0 also accepts
/// legacy `page_{id}.bin`).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct AppNote {
    id: u32,
    title: String,
    page_index: usize,
    created_at: u64,
    /// Last edit time (ink save / rename). Falls back to `created_at` when 0.
    #[serde(default)]
    updated_at: u64,
    /// Parent folder; `None` = root-level (unfiled) note.
    #[serde(default)]
    notebook_id: Option<u32>,
    paper_template: ephemeris_core::PageTemplate,
    /// "cal:YYYY-MM-DD" when this note is anchored to a calendar day.
    #[serde(default)]
    anchor: Option<String>,
    /// Profile this note belongs to; "" = legacy (visible in all profiles).
    #[serde(default)]
    profile_id: String,
    /// OCR / transcribed machine-readable text for see-text view + vault export.
    #[serde(default)]
    text_content: Option<String>,
    /// Zero-based page within this note's document (swipe navigation).
    #[serde(default)]
    doc_page: usize,
    /// Number of pages in this note's document (at least 1).
    #[serde(default = "default_doc_page_count")]
    doc_page_count: usize,
    /// Freeform Obsidian-style tags (exported as YAML `tags:` frontmatter).
    #[serde(default)]
    tags: Vec<String>,
}

fn default_doc_page_count() -> usize {
    1
}

fn note_updated_at(n: &AppNote) -> u64 {
    if n.updated_at > 0 {
        n.updated_at
    } else {
        n.created_at
    }
}

/// An external connection (vault path, calendar file/URL) with per-profile assignment.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct AppConnection {
    pub(crate) id: u32,
    /// `"vault"` | `"calendar"` (ICS) | `"caldav"` (authenticated CalDAV).
    pub(crate) kind: String,
    pub(crate) display: String,
    pub(crate) value: String,
    pub(crate) profile_id: String,
    allow_in_aggregated: bool,
    /// CalDAV username (empty for ICS/vault).
    #[serde(default)]
    username: String,
    /// CalDAV password / app token (empty for ICS/vault). Stored locally with connections.
    #[serde(default)]
    password: String,
}

/// Persisted snapshot of all notes and notebooks (saved to notes_index.json).
#[derive(serde::Serialize, serde::Deserialize)]
struct NotesIndex {
    note_id_seq: u32,
    notebook_id_seq: u32,
    notebooks: Vec<AppNotebook>,
    notes: Vec<AppNote>,
}

// ── Recording data models ─────────────────────────────────────────────────────

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct AppRecording {
    id: u32,
    title: String,
    duration_secs: u32,
    created_at: u64,
    audio_filename: String,
    transcription: Option<String>,
    /// Profile this recording belongs to; "" = visible in all profiles.
    #[serde(default)]
    profile_id: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RecordingsIndex {
    rec_id_seq: u32,
    recordings: Vec<AppRecording>,
}

enum WriterCmd {
    SamplesF32(Vec<f32>),
    Finalize,
    Discard,
}

struct RecordingHandle {
    _stream: cpal::Stream,
    writer_tx: std::sync::mpsc::Sender<WriterCmd>,
    writer_thread: Option<std::thread::JoinHandle<Option<PathBuf>>>,
    frame_count: Arc<AtomicU64>,
    sample_rate: u32,
    #[allow(dead_code)]
    channels: u16,
    paused: Arc<AtomicBool>,
    rec_id: u32,
}

// ── Recording helpers ─────────────────────────────────────────────────────────

fn has_microphone() -> bool {
    use cpal::traits::HostTrait;
    cpal::default_host().default_input_device().is_some()
}

fn recording_data_dir() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".local").join("share")
        });
    base.join("ephemeris").join("recordings")
}

fn recordings_index_path(dir: &Path) -> PathBuf {
    dir.join("recordings_index.json")
}

fn save_recordings_index(dir: &Path, recordings: &[AppRecording], seq: u32) {
    let index = RecordingsIndex {
        rec_id_seq: seq,
        recordings: recordings.to_vec(),
    };
    match serde_json::to_string(&index) {
        Ok(json) => {
            if let Err(e) = std::fs::write(recordings_index_path(dir), json) {
                tracing::warn!("Failed to save recordings index: {e}");
            }
        }
        Err(e) => tracing::warn!("Failed to serialize recordings index: {e}"),
    }
}

fn load_recordings_index(dir: &Path) -> Option<RecordingsIndex> {
    let json = std::fs::read_to_string(recordings_index_path(dir)).ok()?;
    serde_json::from_str(&json).ok()
}

fn build_recording_list(
    recordings: &[AppRecording],
    transcribing: &HashSet<u32>,
    show_badges: bool,
    mgr: &ephemeris_core::profile_manager::ProfileManager,
) -> Vec<ephemeris_ui::RecordingEntry> {
    recordings
        .iter()
        .map(|r| {
            let (show_profile, profile_icon_char, has_custom_profile_icon, custom_profile_icon) =
                profile_badge_for(&r.profile_id, show_badges, mgr);
            let preview = r
                .transcription
                .as_deref()
                .map(transcript_preview)
                .unwrap_or_default();
            ephemeris_ui::RecordingEntry {
                id: r.id.to_string().as_str().into(),
                title: r.title.as_str().into(),
                duration_label: format_rec_duration(r.duration_secs as u64).as_str().into(),
                date_label: format_rec_date(r.created_at, now_secs()).as_str().into(),
                has_transcription: r.transcription.is_some(),
                transcript_preview: preview.as_str().into(),
                is_transcribing: transcribing.contains(&r.id),
                show_profile,
                profile_icon_char: profile_icon_char.as_str().into(),
                has_custom_profile_icon,
                custom_profile_icon,
            }
        })
        .collect()
}

/// Collapse whitespace and ellipsize for the recordings list preview line.
fn transcript_preview(text: &str) -> String {
    const MAX_CHARS: usize = 110;
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return String::new();
    }
    let count = collapsed.chars().count();
    if count <= MAX_CHARS {
        collapsed
    } else {
        let truncated: String = collapsed
            .chars()
            .take(MAX_CHARS.saturating_sub(1))
            .collect();
        format!("{truncated}…")
    }
}

fn filter_recordings_by_query(recordings: &[AppRecording], query: &str) -> Vec<AppRecording> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return recordings.to_vec();
    }
    recordings
        .iter()
        .filter(|r| {
            r.title.to_lowercase().contains(&q)
                || r.transcription
                    .as_deref()
                    .map(|t| t.to_lowercase().contains(&q))
                    .unwrap_or(false)
        })
        .cloned()
        .collect()
}

fn sort_recordings(recordings: &mut [AppRecording], sort: &str) {
    match sort {
        "title" => recordings.sort_by(|a, b| {
            a.title
                .to_lowercase()
                .cmp(&b.title.to_lowercase())
                .then(b.created_at.cmp(&a.created_at))
        }),
        "duration" => recordings.sort_by(|a, b| {
            b.duration_secs
                .cmp(&a.duration_secs)
                .then(b.created_at.cmp(&a.created_at))
        }),
        // Newest first
        _ => recordings.sort_by_key(|r| std::cmp::Reverse(r.created_at)),
    }
}

fn push_recording_list_comp(
    comp: &ephemeris_ui::EphemerisPage,
    recordings: &[AppRecording],
    transcribing: &HashSet<u32>,
    show_badges: bool,
    mgr: &ephemeris_core::profile_manager::ProfileManager,
) {
    comp.set_recording_list(
        Rc::new(slint::VecModel::from(build_recording_list(
            recordings,
            transcribing,
            show_badges,
            mgr,
        )))
        .into(),
    );
}

fn whisper_ready() -> bool {
    find_whisper_binary().is_some() && find_whisper_model().is_some()
}

/// Queue a recording for background whisper and kick the worker if idle.
#[allow(clippy::too_many_arguments)]
fn enqueue_transcription(
    rec_id: u32,
    queue: &Arc<Mutex<VecDeque<u32>>>,
    in_flight: &Rc<RefCell<HashSet<u32>>>,
    active: &Arc<AtomicBool>,
    mailbox: &Arc<Mutex<Vec<(u32, String)>>>,
    recordings: &Rc<RefCell<Vec<AppRecording>>>,
    rec_dir: &Path,
    lang: &str,
    push_recordings: &Rc<dyn Fn()>,
) {
    in_flight.borrow_mut().insert(rec_id);
    if let Ok(mut q) = queue.lock() {
        if !q.iter().any(|&id| id == rec_id) {
            q.push_back(rec_id);
        }
    }
    push_recordings();
    pump_transcription_queue(
        queue,
        active,
        mailbox,
        recordings,
        rec_dir,
        lang,
        in_flight,
        push_recordings,
    );
}

/// Start the next queued whisper job if none is running.
#[allow(clippy::too_many_arguments)]
fn pump_transcription_queue(
    queue: &Arc<Mutex<VecDeque<u32>>>,
    active: &Arc<AtomicBool>,
    mailbox: &Arc<Mutex<Vec<(u32, String)>>>,
    recordings: &Rc<RefCell<Vec<AppRecording>>>,
    rec_dir: &Path,
    lang: &str,
    in_flight: &Rc<RefCell<HashSet<u32>>>,
    push_recordings: &Rc<dyn Fn()>,
) {
    if active.swap(true, Ordering::AcqRel) {
        return;
    }
    let next_id = queue.lock().ok().and_then(|mut q| q.pop_front());
    let Some(rec_id) = next_id else {
        active.store(false, Ordering::Release);
        return;
    };
    let wav_path = {
        let recs = recordings.borrow();
        let Some(rec) = recs.iter().find(|r| r.id == rec_id) else {
            active.store(false, Ordering::Release);
            in_flight.borrow_mut().remove(&rec_id);
            push_recordings();
            // Try the next queued id.
            pump_transcription_queue(
                queue,
                active,
                mailbox,
                recordings,
                rec_dir,
                lang,
                in_flight,
                push_recordings,
            );
            return;
        };
        rec_dir.join(&rec.audio_filename)
    };
    let lang = lang.to_string();
    let mailbox = mailbox.clone();
    let active_flag = active.clone();
    std::thread::spawn(move || {
        let text = run_whisper(&wav_path, &lang);
        if let Ok(mut pending) = mailbox.lock() {
            pending.push((rec_id, text));
        }
        active_flag.store(false, Ordering::Release);
    });
}

fn format_rec_duration(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn format_rec_date(created_at: u64, now: u64) -> String {
    use chrono::{DateTime, Datelike, Local, Timelike, Utc};
    let dt = DateTime::<Utc>::from_timestamp(created_at as i64, 0)
        .map(|u| u.with_timezone(&Local))
        .unwrap_or_else(|| Utc::now().with_timezone(&Local));
    let now_dt = DateTime::<Utc>::from_timestamp(now as i64, 0)
        .map(|u| u.with_timezone(&Local))
        .unwrap_or_else(|| Utc::now().with_timezone(&Local));
    let age_days = now.saturating_sub(created_at) / 86_400;
    let time = format!("{:02}:{:02}", dt.hour(), dt.minute());
    if age_days == 0 && dt.day() == now_dt.day() {
        time
    } else if age_days < 2 && dt.day() != now_dt.day() {
        format!("Yesterday {time}")
    } else if dt.year() == now_dt.year() {
        format!("{} {} {time}", dt.day(), month_short(dt.month()))
    } else {
        format!(
            "{} {} {}",
            dt.day(),
            month_short(dt.month()),
            dt.year() % 100
        )
    }
}

fn month_short(m: u32) -> &'static str {
    match m {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        _ => "Dec",
    }
}

fn start_recording(rec_dir: &Path, rec_id: u32) -> Option<RecordingHandle> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let device = host.default_input_device()?;
    let supported_config = device.default_input_config().ok()?;
    // cpal 0.17+: SampleRate is a plain u32 (was a newtype with `.0`).
    let sample_rate = supported_config.sample_rate();
    let channels = supported_config.channels();
    let stream_config: cpal::StreamConfig = supported_config.into();

    let (tx, rx) = std::sync::mpsc::channel::<WriterCmd>();
    let frame_count = Arc::new(AtomicU64::new(0));
    let paused = Arc::new(AtomicBool::new(false));

    let wav_path = rec_dir.join(format!("rec_{rec_id}.wav"));
    let wav_path2 = wav_path.clone();
    let channels_w = channels;
    let sr_w = sample_rate;

    let writer_thread = std::thread::spawn(move || -> Option<PathBuf> {
        let spec = hound::WavSpec {
            channels: channels_w,
            sample_rate: sr_w,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(&wav_path2, spec).ok()?;
        loop {
            match rx.recv() {
                Ok(WriterCmd::SamplesF32(samples)) => {
                    for s in samples {
                        let _ = writer.write_sample(s);
                    }
                }
                Ok(WriterCmd::Finalize) => {
                    let _ = writer.finalize();
                    return Some(wav_path2);
                }
                Ok(WriterCmd::Discard) => {
                    drop(writer);
                    let _ = std::fs::remove_file(&wav_path2);
                    return None;
                }
                Err(_) => {
                    let _ = writer.finalize();
                    return Some(wav_path2);
                }
            }
        }
    });

    let fc2 = frame_count.clone();
    let pa2 = paused.clone();
    let tx2 = tx.clone();
    let ch_usize = channels as usize;

    // cpal 0.18: StreamError → Error; StreamConfig passed by value (Copy).
    let err_fn = |e: cpal::Error| tracing::warn!("cpal input error: {e}");

    let stream = match supported_config.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            stream_config,
            move |data: &[f32], _| {
                if pa2.load(Ordering::Relaxed) {
                    return;
                }
                fc2.fetch_add((data.len() / ch_usize) as u64, Ordering::Relaxed);
                let _ = tx2.send(WriterCmd::SamplesF32(data.to_vec()));
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::I16 => {
            let fc3 = fc2;
            let pa3 = pa2;
            let tx3 = tx2;
            device.build_input_stream(
                stream_config,
                move |data: &[i16], _| {
                    if pa3.load(Ordering::Relaxed) {
                        return;
                    }
                    fc3.fetch_add((data.len() / ch_usize) as u64, Ordering::Relaxed);
                    let samples: Vec<f32> =
                        data.iter().map(|&s| s as f32 / i16::MAX as f32).collect();
                    let _ = tx3.send(WriterCmd::SamplesF32(samples));
                },
                err_fn,
                None,
            )
        }
        _ => {
            tracing::warn!("Unsupported cpal sample format; attempting F32 fallback");
            device.build_input_stream(
                stream_config,
                move |data: &[f32], _| {
                    if pa2.load(Ordering::Relaxed) {
                        return;
                    }
                    fc2.fetch_add((data.len() / ch_usize) as u64, Ordering::Relaxed);
                    let _ = tx2.send(WriterCmd::SamplesF32(data.to_vec()));
                },
                err_fn,
                None,
            )
        }
    };

    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("Failed to build input stream: {e}");
            return None;
        }
    };
    if let Err(e) = stream.play() {
        tracing::warn!("Failed to start recording stream: {e}");
        return None;
    }

    Some(RecordingHandle {
        _stream: stream,
        writer_tx: tx,
        writer_thread: Some(writer_thread),
        frame_count,
        sample_rate,
        channels,
        paused,
        rec_id,
    })
}

fn play_audio(path: &Path) {
    let path = path.to_owned();
    std::thread::spawn(move || {
        #[cfg(target_os = "macos")]
        let _ = std::process::Command::new("afplay").arg(&path).status();
        #[cfg(not(target_os = "macos"))]
        let _ = std::process::Command::new("aplay").arg(&path).status();
    });
}

fn default_htr_model_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(home).join(".local").join("share"));
    base.join("ephemeris").join("htr")
}

fn resolve_htr_model_dir(config: &ephemeris_core::Config) -> PathBuf {
    config
        .htr_model_dir
        .clone()
        .unwrap_or_else(default_htr_model_dir)
}

/// Process-wide lexicon shared by See-text / bg recog / vault export cascades.
static SHARED_HTR_LEXICON: OnceLock<SharedHtrLexicon> = OnceLock::new();

pub(crate) fn build_cascading_transcriber(config: &ephemeris_core::Config) -> CascadingTranscriber {
    let htr =
        StrokeHtrTranscriber::from_parts(config.htr_enabled, Some(resolve_htr_model_dir(config)));
    let line_htr = LineImageHtrTranscriber::from_parts(
        config.line_htr_enabled,
        Some(resolve_line_htr_model_dir(config)),
    );
    let ocr = RasterOcrTranscriber::from_parts(
        config.ocr_enabled,
        config.ocr_languages.clone(),
        config.ocr_tessdata.clone(),
    );
    let mut cascade = CascadingTranscriber::with_line_htr(htr, line_htr, ocr)
        .with_language(config.handwriting_language);
    if let Some(shared) = SHARED_HTR_LEXICON.get() {
        cascade = cascade.with_shared_lexicon(Arc::clone(shared));
    }
    cascade
}

/// Load cached merged lexicon and spawn a background vault/task/calendar refresh.
fn start_user_htr_lexicon_refresh(
    data_dir: &Path,
    config: &Config,
    vault_paths: &[PathBuf],
    tasks: &[Task],
    events: &[CalendarEvent],
) {
    let shared: SharedHtrLexicon = Arc::new(RwLock::new(load_merged_htr_lexicon(Some(data_dir))));
    let _ = SHARED_HTR_LEXICON.set(Arc::clone(&shared));
    let sources = UserLexiconSources {
        vault_paths: vault_paths.to_vec(),
        exclude_subdir: Some(config.vault_export_subdir.clone()),
        task_titles: tasks.iter().map(|t| t.title.clone()).collect(),
        event_titles: events.iter().map(|e| e.title.clone()).collect(),
    };
    spawn_user_lexicon_refresh(data_dir.to_path_buf(), sources, Some(shared));
}

fn find_htr_model_installed(config: &ephemeris_core::Config) -> bool {
    let dir = resolve_htr_model_dir(config);
    dir.join("model.onnx").is_file() || dir.join("model.rten").is_file()
}

fn default_line_htr_model_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(home).join(".local").join("share"));
    base.join("ephemeris").join("line-htr")
}

fn resolve_line_htr_model_dir(config: &ephemeris_core::Config) -> PathBuf {
    config
        .line_htr_model_dir
        .clone()
        .unwrap_or_else(default_line_htr_model_dir)
}

fn find_line_htr_model_installed(config: &ephemeris_core::Config) -> bool {
    let dir = resolve_line_htr_model_dir(config);
    dir.join("model.onnx").is_file() || dir.join("model.rten").is_file()
}

/// Download DE·HTR v2 int8 ONNX (~10.4 MB) from Hugging Face + write charset/meta.
fn download_line_htr_model(config: &ephemeris_core::Config) -> Result<(), String> {
    const MODEL_URL: &str = "https://huggingface.co/naeyn/de-htr-web-v2/resolve/main/model.onnx";
    let dest_dir = resolve_line_htr_model_dir(config);
    std::fs::create_dir_all(&dest_dir).map_err(|e| format!("mkdir failed: {e}"))?;
    let model_path = dest_dir.join("model.onnx");
    let mut resp =
        reqwest::blocking::get(MODEL_URL).map_err(|e| format!("download failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "HTTP {} fetching line HTR model from Hugging Face",
            resp.status()
        ));
    }
    {
        let mut f =
            std::fs::File::create(&model_path).map_err(|e| format!("create model failed: {e}"))?;
        std::io::copy(&mut resp, &mut f).map_err(|e| format!("write failed: {e}"))?;
    }
    // charset.txt — one char per line for load_line_htr_meta
    let alphabet = ephemeris_core::DE_HTR_V2_ALPHABET;
    let mut charset = String::new();
    for ch in alphabet.chars() {
        charset.push(ch);
        charset.push('\n');
    }
    std::fs::write(dest_dir.join("charset.txt"), charset)
        .map_err(|e| format!("write charset failed: {e}"))?;
    let meta = serde_json::json!({
        "blank_index": 0,
        "alphabet": alphabet,
        "input_name": "image",
        "output_name": "logits",
        "image_height": 64,
        "max_width": 1024,
        "polarity": "dark_on_light",
        "normalize": "[0,1]",
        "white_value": 1.0,
        "mean_std": false,
        "source": "naeyn/de-htr-web-v2",
        "license": "Apache-2.0",
        "notes": "German-focused modern handwriting CTC; Latin+digits also covered. PineNote: ~0.2–1s/line on A55 typical. Feed gray/255 dark-on-light; no ImageNet mean/std (v2 norms inside model).",
    });
    std::fs::write(
        dest_dir.join("meta.json"),
        serde_json::to_string_pretty(&meta).unwrap_or_else(|_| "{}".into()),
    )
    .map_err(|e| format!("write meta failed: {e}"))?;
    if find_line_htr_model_installed(config) {
        Ok(())
    } else {
        Err(format!(
            "download finished but model.onnx missing in {}",
            dest_dir.display()
        ))
    }
}

fn delete_line_htr_model(config: &ephemeris_core::Config) {
    let dir = resolve_line_htr_model_dir(config);
    for name in ["model.onnx", "model.rten", "charset.txt", "meta.json"] {
        let _ = std::fs::remove_file(dir.join(name));
    }
}

fn download_htr_model(config: &ephemeris_core::Config) -> Result<(), String> {
    // Packaged zip from scripts/export-htr-model.sh / GitHub Release asset.
    const URL: &str =
        "https://github.com/steffen030/ephemeris/releases/download/htr-model-v1/ephemeris-htr-v1.zip";
    let dest_dir = resolve_htr_model_dir(config);
    std::fs::create_dir_all(&dest_dir).map_err(|e| format!("mkdir failed: {e}"))?;
    let zip_path = dest_dir.join("ephemeris-htr-v1.zip");
    let mut resp = reqwest::blocking::get(URL).map_err(|e| format!("download failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "HTTP {} — build locally with scripts/export-htr-model.sh and copy model.onnx + charset.txt to {}",
            resp.status(),
            dest_dir.display()
        ));
    }
    {
        let mut f =
            std::fs::File::create(&zip_path).map_err(|e| format!("create zip failed: {e}"))?;
        std::io::copy(&mut resp, &mut f).map_err(|e| format!("write failed: {e}"))?;
    }
    // Prefer `unzip` if present; otherwise instruct the user.
    let status = std::process::Command::new("unzip")
        .args(["-o", "-d"])
        .arg(&dest_dir)
        .arg(&zip_path)
        .status();
    match status {
        Ok(s) if s.success() => {
            let _ = std::fs::remove_file(&zip_path);
            if find_htr_model_installed(config) {
                Ok(())
            } else {
                Err(format!(
                    "unzip succeeded but model.onnx missing in {}",
                    dest_dir.display()
                ))
            }
        }
        _ => Err(format!(
            "Downloaded zip to {} — install unzip or extract model.onnx + charset.txt manually",
            zip_path.display()
        )),
    }
}

fn delete_htr_model(config: &ephemeris_core::Config) {
    let dir = resolve_htr_model_dir(config);
    for name in ["model.onnx", "model.rten", "charset.txt", "dict.txt"] {
        let _ = std::fs::remove_file(dir.join(name));
    }
}

fn find_whisper_binary() -> Option<String> {
    // Prefer the apt-bundled wrapper (sets LD_LIBRARY_PATH for shipped .so).
    let bundled = [
        "/usr/bin/whisper-cli",
        "/usr/lib/ephemeris/whisper/whisper-cli",
    ];
    for path in &bundled {
        if Path::new(path).is_file() {
            return Some((*path).to_string());
        }
    }
    // "whisper" without a suffix is the Python/OpenAI package — exclude it.
    for cmd in &["whisper-cli", "whisper-cpp", "main"] {
        if std::process::Command::new(cmd)
            .arg("--help")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
        {
            return Some(cmd.to_string());
        }
    }
    None
}

fn find_whisper_model() -> Option<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let candidates = [
        PathBuf::from(&home).join(".local/share/ephemeris/whisper/ggml-base.bin"),
        PathBuf::from("/opt/homebrew/share/whisper-cpp/models/ggml-base.bin"),
        PathBuf::from("/usr/local/share/whisper-cpp/models/ggml-base.bin"),
        PathBuf::from("/usr/share/whisper-cpp/models/ggml-base.bin"),
        PathBuf::from(&home).join(".cache/whisper/ggml-base.bin"),
    ];
    candidates.into_iter().find(|p| p.exists())
}

fn whisper_model_dest() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".local/share/ephemeris/whisper/ggml-base.bin")
}

fn download_whisper_model() -> Result<(), String> {
    const URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin";
    let dest = whisper_model_dest();
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir failed: {e}"))?;
    }
    let mut resp = reqwest::blocking::get(URL).map_err(|e| format!("download failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let tmp = dest.with_extension("bin.part");
    {
        let mut f = std::fs::File::create(&tmp).map_err(|e| format!("create tmp failed: {e}"))?;
        std::io::copy(&mut resp, &mut f).map_err(|e| format!("write failed: {e}"))?;
    }
    std::fs::rename(&tmp, &dest).map_err(|e| format!("rename failed: {e}"))?;
    Ok(())
}

fn run_whisper(wav_path: &Path, lang: &str) -> String {
    let Some(bin) = find_whisper_binary() else {
        return "whisper-cli not found.\n\
            PineNote/apt: reinstall ephemeris (bundles whisper-cli).\n\
            macOS: brew install whisper-cpp\n\
            or build from https://github.com/ggerganov/whisper.cpp"
            .to_string();
    };
    let Some(model) = find_whisper_model() else {
        return "No whisper model found.\n\
            Go to Settings → Transcription and tap Download."
            .to_string();
    };
    let mut cmd = std::process::Command::new(&bin);
    // Bundled binary (not the /usr/bin wrapper) needs its sibling .so path.
    if bin.contains("/usr/lib/ephemeris/whisper/") {
        let lib_dir = "/usr/lib/ephemeris/whisper";
        let existing = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
        let joined = if existing.is_empty() {
            lib_dir.to_string()
        } else {
            format!("{lib_dir}:{existing}")
        };
        cmd.env("LD_LIBRARY_PATH", joined);
    }
    let output = cmd
        .args(["-m", &model.to_string_lossy(), "-l", lang])
        .arg(wav_path)
        .output();
    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let text = if stdout.trim().is_empty() {
                String::from_utf8_lossy(&out.stderr).to_string()
            } else {
                stdout
            };
            // Strip "[HH:MM:SS.mmm --> HH:MM:SS.mmm]  " timestamp prefixes
            let cleaned: Vec<&str> = text
                .lines()
                .map(|l| {
                    if let Some(pos) = l.rfind(']') {
                        l[pos + 1..].trim()
                    } else {
                        l.trim()
                    }
                })
                .filter(|l| !l.is_empty())
                .collect();
            if cleaned.is_empty() {
                "No speech detected.".to_string()
            } else {
                cleaned.join(" ")
            }
        }
        Err(e) => format!("Failed to run {bin}: {e}"),
    }
}

// ── Notes browser list builders ───────────────────────────────────────────────

fn notebook_visible(
    nb: &AppNotebook,
    profile_filter: &str,
    notes_in_all: &HashSet<String>,
) -> bool {
    if profile_filter.is_empty() {
        nb.profile_id.is_empty() || notes_in_all.contains(&nb.profile_id)
    } else {
        nb.profile_id == profile_filter
    }
}

fn note_visible(n: &AppNote, profile_filter: &str, notes_in_all: &HashSet<String>) -> bool {
    if profile_filter.is_empty() {
        n.profile_id.is_empty() || notes_in_all.contains(&n.profile_id)
    } else {
        n.profile_id == profile_filter
    }
}

fn note_in_folder(n: &AppNote, folder_id: u32) -> bool {
    match n.notebook_id {
        None => folder_id == 0,
        Some(id) => id == folder_id,
    }
}

fn folder_under_parent(nb: &AppNotebook, parent_id: u32) -> bool {
    match nb.parent_id {
        None => parent_id == 0,
        Some(id) => id == parent_id,
    }
}

/// Collect `root_id` and every nested folder id under it.
fn collect_folder_subtree(notebooks: &[AppNotebook], root_id: u32) -> HashSet<u32> {
    let mut doomed = HashSet::from([root_id]);
    let mut grew = true;
    while grew {
        grew = false;
        for nb in notebooks {
            if let Some(pid) = nb.parent_id {
                if doomed.contains(&pid) && doomed.insert(nb.id) {
                    grew = true;
                }
            }
        }
    }
    doomed
}

fn count_notes_in_folder(
    notes: &[AppNote],
    folder_id: u32,
    profile_filter: &str,
    notes_in_all: &HashSet<String>,
) -> i32 {
    notes
        .iter()
        .filter(|n| note_in_folder(n, folder_id))
        .filter(|n| note_visible(n, profile_filter, notes_in_all))
        .count() as i32
}

fn count_child_folders(
    notebooks: &[AppNotebook],
    parent_id: u32,
    profile_filter: &str,
    notes_in_all: &HashSet<String>,
) -> i32 {
    notebooks
        .iter()
        .filter(|nb| folder_under_parent(nb, parent_id))
        .filter(|nb| notebook_visible(nb, profile_filter, notes_in_all))
        .count() as i32
}

/// Empty profile (All aggregator) → show create-profile picker; else create now.
/// Shared by Notes / Canvas / Recordings create taps.
pub(crate) fn begin_create_with_profile(
    page: &ephemeris_ui::EphemerisPage,
    intent: &str,
    profile_id: &str,
) {
    if profile_id.is_empty() {
        page.set_create_picker_intent(intent.into());
        page.set_show_create_profile_picker(true);
    } else {
        page.invoke_create_profile_picked(intent.into(), profile_id.into());
    }
}

/// Shared All-aggregator origin badge for Notes / Canvas / Recordings rows.
pub(crate) fn profile_badge_for(
    profile_id: &str,
    show_in_all: bool,
    mgr: &ephemeris_core::profile_manager::ProfileManager,
) -> (bool, String, bool, slint::Image) {
    if !show_in_all {
        return (false, String::new(), false, slint::Image::default());
    }
    if profile_id.is_empty() {
        return (true, "\u{E2EB}".to_string(), false, slint::Image::default());
    }
    let profiles = mgr.list_profiles().unwrap_or_default();
    if let Some(p) = profiles.iter().find(|p| p.id.0.to_string() == profile_id) {
        let (has_custom, img) = load_custom_profile_icon(&p.icon);
        (
            true,
            profile_icon_char(&p.icon).to_string(),
            has_custom,
            img,
        )
    } else {
        (true, "\u{E491}".to_string(), false, slint::Image::default())
    }
}

/// Flat list of all notebooks (for move dialog), with path-like titles.
fn build_notebook_list(
    notebooks: &[AppNotebook],
    notes: &[AppNote],
    profile_filter: &str,
    notes_in_all: &HashSet<String>,
    mgr: &ephemeris_core::profile_manager::ProfileManager,
) -> Vec<ephemeris_ui::FolderListItem> {
    let show_badges = profile_filter.is_empty();
    notebooks
        .iter()
        .filter(|nb| notebook_visible(nb, profile_filter, notes_in_all))
        .map(|nb| {
            let count = count_notes_in_folder(notes, nb.id, profile_filter, notes_in_all);
            let folders = count_child_folders(notebooks, nb.id, profile_filter, notes_in_all);
            let title = notebook_display_path(notebooks, nb.id);
            let (show_profile, profile_icon_char, has_custom_profile_icon, custom_profile_icon) =
                profile_badge_for(&nb.profile_id, show_badges, mgr);
            ephemeris_ui::FolderListItem {
                id: nb.id.to_string(),
                title,
                note_count: count,
                folder_count: folders,
                show_profile,
                profile_icon_char,
                has_custom_profile_icon,
                custom_profile_icon,
            }
        })
        .collect()
}

/// Child folders under `parent_id` (0 = root).
fn build_folder_list(
    notebooks: &[AppNotebook],
    notes: &[AppNote],
    parent_id: u32,
    profile_filter: &str,
    notes_in_all: &HashSet<String>,
    mgr: &ephemeris_core::profile_manager::ProfileManager,
) -> Vec<ephemeris_ui::FolderListItem> {
    let show_badges = profile_filter.is_empty();
    notebooks
        .iter()
        .filter(|nb| folder_under_parent(nb, parent_id))
        .filter(|nb| notebook_visible(nb, profile_filter, notes_in_all))
        .map(|nb| {
            let count = count_notes_in_folder(notes, nb.id, profile_filter, notes_in_all);
            let folders = count_child_folders(notebooks, nb.id, profile_filter, notes_in_all);
            let (show_profile, profile_icon_char, has_custom_profile_icon, custom_profile_icon) =
                profile_badge_for(&nb.profile_id, show_badges, mgr);
            ephemeris_ui::FolderListItem {
                id: nb.id.to_string(),
                title: nb.title.clone(),
                note_count: count,
                folder_count: folders,
                show_profile,
                profile_icon_char,
                has_custom_profile_icon,
                custom_profile_icon,
            }
        })
        .collect()
}

/// Notes in a folder (`0` = root / unfiled).
#[allow(clippy::too_many_arguments)]
fn build_note_list(
    notes: &[AppNote],
    notebook_id: u32,
    now: u64,
    profile_filter: &str,
    notes_in_all: &HashSet<String>,
    data_dir: &Path,
    mgr: &ephemeris_core::profile_manager::ProfileManager,
    sort_mode: &str,
    search_q: &str,
) -> Vec<ephemeris_ui::NoteListItem> {
    let show_badges = profile_filter.is_empty();
    let q = search_q.trim().to_lowercase();
    let mut items: Vec<&AppNote> = notes
        .iter()
        .filter(|n| note_in_folder(n, notebook_id))
        .filter(|n| note_visible(n, profile_filter, notes_in_all))
        .filter(|n| {
            if q.is_empty() {
                return true;
            }
            let title_hit = n.title.to_lowercase().contains(&q);
            let text_hit = n
                .text_content
                .as_deref()
                .map(|t| strip_inline_markdown(t).to_lowercase().contains(&q))
                .unwrap_or(false);
            title_hit || text_hit
        })
        .collect();

    match sort_mode {
        "title" => items.sort_by(|a, b| {
            a.title
                .to_lowercase()
                .cmp(&b.title.to_lowercase())
                .then_with(|| note_updated_at(b).cmp(&note_updated_at(a)))
        }),
        "created" => items.sort_by_key(|a| std::cmp::Reverse(a.created_at)),
        _ => items.sort_by_key(|a| std::cmp::Reverse(note_updated_at(a))),
    }

    items
        .into_iter()
        .map(|n| {
            let (has_preview, preview) = note_preview_image(data_dir, n.id);
            let (show_profile, profile_icon_char, has_custom_profile_icon, custom_profile_icon) =
                profile_badge_for(&n.profile_id, show_badges, mgr);
            let stamp = if sort_mode == "created" {
                n.created_at
            } else {
                note_updated_at(n)
            };
            let tags_label = if n.tags.is_empty() {
                String::new()
            } else {
                n.tags
                    .iter()
                    .map(|t| format!("#{t}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            ephemeris_ui::NoteListItem {
                id: n.id.to_string(),
                title: n.title.clone(),
                date_label: format_note_date(stamp, now),
                tags_label,
                has_preview,
                preview,
                show_profile,
                profile_icon_char,
                has_custom_profile_icon,
                custom_profile_icon,
            }
        })
        .collect()
}

fn notebook_display_path(notebooks: &[AppNotebook], id: u32) -> String {
    let mut parts = Vec::new();
    let mut cur = Some(id);
    let mut guard = 0;
    while let Some(cid) = cur {
        guard += 1;
        if guard > 32 {
            break;
        }
        if let Some(nb) = notebooks.iter().find(|n| n.id == cid) {
            parts.push(nb.title.clone());
            cur = nb.parent_id;
        } else {
            break;
        }
    }
    parts.reverse();
    if parts.is_empty() {
        format!("Folder {id}")
    } else {
        parts.join(" / ")
    }
}

/// Display title + sanitized path segments for vault note export.
///
/// Segments are nested under `{export_subdir}/Notes/` (see
/// [`ephemeris_core::notebook_export_dir`]). Unfiled notes use an empty
/// segment list so the file lands directly in `Notes/`.
fn notebook_export_parts(
    notebooks: &[AppNotebook],
    notebook_id: Option<u32>,
) -> (String, Vec<String>) {
    match notebook_id {
        None => ("Notes".to_string(), vec![]),
        Some(id) => {
            let mut parts = Vec::new();
            let mut cur = Some(id);
            let mut guard = 0;
            while let Some(cid) = cur {
                guard += 1;
                if guard > 32 {
                    break;
                }
                if let Some(nb) = notebooks.iter().find(|n| n.id == cid) {
                    parts.push(nb.title.clone());
                    cur = nb.parent_id;
                } else {
                    break;
                }
            }
            parts.reverse();
            if parts.is_empty() {
                (format!("Folder {id}"), vec![format!("Folder_{id}")])
            } else {
                let title = parts.join(" / ");
                let segs = parts
                    .iter()
                    .map(|p| ephemeris_core::vault_export::sanitize_component(p))
                    .collect();
                (title, segs)
            }
        }
    }
}

/// Downsample a gray8 page into a small RGBA preview for the notes gallery.
/// Portrait thumbs match the note frame (~3∶4).
fn note_preview_image(data_dir: &Path, note_id: u32) -> (bool, slint::Image) {
    let Some((w, h, gray)) = load_note_page(data_dir, note_id) else {
        return (false, slint::Image::default());
    };
    note_preview_from_gray(w, h, &gray)
}

pub(crate) fn note_preview_from_gray(w: u32, h: u32, gray: &[u8]) -> (bool, slint::Image) {
    if w == 0 || h == 0 || gray.len() < (w * h) as usize {
        return (false, slint::Image::default());
    }

    // Crop to ink content so empty margins don't dominate the thumb.
    const INK_THRESH: u8 = 240;
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0u32, 0u32);
    let mut found = false;
    for y in 0..h {
        let row = (y * w) as usize;
        for x in 0..w {
            if gray[row + x as usize] < INK_THRESH {
                found = true;
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x + 1);
                y1 = y1.max(y + 1);
            }
        }
    }
    let (sx0, sy0, sw, sh) = if found && x1 > x0 && y1 > y0 {
        // Pad ~4% so strokes aren't clipped at the edge.
        let pad_x = ((x1 - x0) / 25).clamp(4, 24);
        let pad_y = ((y1 - y0) / 25).clamp(4, 24);
        let x0 = x0.saturating_sub(pad_x);
        let y0 = y0.saturating_sub(pad_y);
        let x1 = (x1 + pad_x).min(w);
        let y1 = (y1 + pad_y).min(h);
        (x0, y0, x1 - x0, y1 - y0)
    } else {
        (0, 0, w, h)
    };

    // Match gallery tile width (~220–240); keep crop aspect.
    const TARGET_W: u32 = 240;
    let (tw, th) = if sh * TARGET_W >= sw {
        let tw = TARGET_W;
        let th = (tw as u64 * sh as u64 / sw as u64).clamp(1, 320) as u32;
        (tw, th)
    } else {
        let th = TARGET_W;
        let tw = (th as u64 * sw as u64 / sh as u64).clamp(1, 320) as u32;
        (tw, th)
    };

    let mut rgba = vec![255u8; (tw * th * 4) as usize];
    for ty in 0..th {
        let y_a = sy0 + ty * sh / th;
        let y_b = sy0 + ((ty + 1) * sh / th).min(sh);
        let y_b = y_b.max(y_a + 1);
        for tx in 0..tw {
            let x_a = sx0 + tx * sw / tw;
            let x_b = sx0 + ((tx + 1) * sw / tw).min(sw);
            let x_b = x_b.max(x_a + 1);
            // Ink-preserving reduce: darkest pixel in the source block.
            let mut darkest = 255u8;
            for y in y_a..y_b {
                let row = (y * w) as usize;
                for x in x_a..x_b {
                    let g = gray[row + x as usize];
                    if g < darkest {
                        darkest = g;
                    }
                }
            }
            // Mild contrast: push mid-grays toward ink for e-ink readability.
            let g = if darkest > 250 {
                255
            } else if darkest < 40 {
                0
            } else {
                // Stretch [40,250] → [0,255] then darken slightly.
                let stretched = ((darkest.saturating_sub(40) as u32) * 255 / 210) as u8;
                stretched.saturating_mul(9) / 10
            };
            let i = ((ty * tw + tx) * 4) as usize;
            rgba[i] = g;
            rgba[i + 1] = g;
            rgba[i + 2] = g;
            rgba[i + 3] = 255;
        }
    }
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&rgba, tw, th);
    (true, slint::Image::from_rgba8(buffer))
}

/// Leave the note canvas: persist ink locally and request a soon vault export.
/// Push title/body/blocks into the Obsidian markdown viewer.
fn show_md_viewer_content(ui: &EphemerisUi, rel_path: &str, md: &str) {
    let title = Path::new(rel_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Note")
        .to_string();
    let body = markdown_to_plain(md);
    let blocks = markdown_to_blocks(md);
    ui.set_md_viewer(&title, &body, rel_path);
    ui.set_md_viewer_blocks(&blocks);
}

fn leave_note_canvas(
    ui: &EphemerisUi,
    notes: &mut [AppNote],
    data_dir: &Path,
    active_note_id: &str,
    vault_export: &VaultExportQueue,
    local_writes: &LocalPageWriteQueue,
) {
    ui.save_current_page();
    // Durability: drain debounced writer before leaving the canvas.
    local_writes.flush_sync();
    flush_active_note_pixels(ui, notes, data_dir, active_note_id);
    if let Ok(id) = active_note_id.parse::<u32>() {
        vault_export.mark(id);
        vault_export.request_flush_soon();
        // Wake the event loop so `about_to_wait` runs `try_flush` promptly
        // instead of waiting for the next user input / 10‑minute batch.
        ui.request_screen_change();
    }
}

/// Flush the live canvas of `active_note_id` to disk so gallery previews stay fresh.
fn flush_active_note_pixels(
    ui: &EphemerisUi,
    notes: &mut [AppNote],
    data_dir: &Path,
    active_note_id: &str,
) {
    flush_active_note_ink(ui, notes, data_dir, active_note_id);
}

fn format_note_date(created_at: u64, now: u64) -> String {
    format_rec_date(created_at, now)
}

// ── Note pixel persistence ────────────────────────────────────────────────────

/// Returns `~/.local/share/ephemeris/notes/` (XDG_DATA_HOME respected if set).
fn note_data_dir() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".local").join("share")
        });
    base.join("ephemeris").join("notes")
}

const PAGE_BIN_MAGIC: &[u8; 4] = b"EPH1";

fn note_page_path(data_dir: &Path, note_id: u32, doc_page: usize) -> PathBuf {
    data_dir.join(format!("page_{note_id}_p{doc_page}.bin"))
}

fn note_page_legacy_path(data_dir: &Path, note_id: u32) -> PathBuf {
    data_dir.join(format!("page_{note_id}.bin"))
}

fn note_strokes_path(data_dir: &Path, note_id: u32, doc_page: usize) -> PathBuf {
    data_dir.join(format!("strokes_{note_id}_p{doc_page}.json"))
}

fn save_note_page_pixels(
    data_dir: &Path,
    note_id: u32,
    doc_page: usize,
    width: u32,
    height: u32,
    pixels: &[u8],
) {
    let path = note_page_path(data_dir, note_id, doc_page);
    let mut buf = Vec::with_capacity(12 + pixels.len());
    buf.extend_from_slice(PAGE_BIN_MAGIC);
    buf.extend_from_slice(&width.to_le_bytes());
    buf.extend_from_slice(&height.to_le_bytes());
    buf.extend_from_slice(pixels);
    if let Err(e) = std::fs::write(&path, &buf) {
        tracing::warn!("Failed to save note {note_id} page {doc_page}: {e}");
    }
    // Keep legacy single-file name in sync for page 0 (previews / older tools).
    if doc_page == 0 {
        let legacy = note_page_legacy_path(data_dir, note_id);
        if let Err(e) = std::fs::write(&legacy, &buf) {
            tracing::warn!("Failed to save legacy note page {note_id}: {e}");
        }
    }
}

fn save_note_page_strokes(
    data_dir: &Path,
    note_id: u32,
    doc_page: usize,
    strokes: &[ephemeris_core::Stroke],
) {
    let page = ephemeris_core::store::StrokePage {
        strokes: strokes.to_vec(),
    };
    match serde_json::to_string(&page) {
        Ok(json) => {
            if let Err(e) = std::fs::write(note_strokes_path(data_dir, note_id, doc_page), json) {
                tracing::warn!("Failed to save strokes for note {note_id} page {doc_page}: {e}");
            }
        }
        Err(e) => tracing::warn!("Failed to serialize strokes for note {note_id}: {e}"),
    }
}

fn load_note_page_strokes(
    data_dir: &Path,
    note_id: u32,
    doc_page: usize,
) -> Option<Vec<ephemeris_core::Stroke>> {
    let path = note_strokes_path(data_dir, note_id, doc_page);
    let json = std::fs::read_to_string(&path).ok()?;
    let page: ephemeris_core::store::StrokePage = serde_json::from_str(&json).ok()?;
    Some(page.strokes)
}

/// Persist raster + stroke list for the active note's current doc page.
fn flush_active_note_ink(
    ui: &EphemerisUi,
    notes: &mut [AppNote],
    data_dir: &Path,
    active_note_id: &str,
) {
    if active_note_id.is_empty() {
        return;
    }
    let Ok(id) = active_note_id.parse::<u32>() else {
        return;
    };
    let Some(note) = notes.iter_mut().find(|n| n.id == id) else {
        return;
    };
    let pixels = ui.get_committed_pixels_arc();
    let (w, h) = (pixels.width, pixels.height);
    if pixels.data.len() < (w * h) as usize {
        return;
    }
    save_note_page_pixels(data_dir, note.id, note.doc_page, w, h, &pixels.data);
    save_note_page_strokes(data_dir, note.id, note.doc_page, &ui.get_page_strokes());
    note.updated_at = now_secs();
}

/// Logical canvas size strokes are recorded in (design UI size minus chrome).
fn design_canvas_size() -> (u32, u32) {
    let (w, h) = probe_window_size();
    (
        w,
        h.saturating_sub(ephemeris_ui::STATUS_BAR_H + ephemeris_ui::TOOLBAR_H),
    )
}

/// Page raster at logical canvas resolution for HTR/OCR.
///
/// Page bins are stored at panel resolution, but recognisers are tuned on
/// logical-size ink and stroke-HTR spans live in logical coordinates, so the
/// cascade always sees one coordinate space.
fn transcription_raster(
    raster: &ephemeris_pal::display::PixelBuf,
    logical: (u32, u32),
) -> ephemeris_pal::display::PixelBuf {
    if raster.width <= logical.0 || logical.0 == 0 || logical.1 == 0 {
        return raster.clone();
    }
    ephemeris_ui::compose::resample_gray(raster, logical.0, logical.1)
}

/// Map transcription spans from the `from` raster size onto `to` (PDF page).
fn scale_spans(
    mut spans: Vec<ephemeris_core::TextSpan>,
    from: (u32, u32),
    to: (u32, u32),
) -> Vec<ephemeris_core::TextSpan> {
    if from == to || from.0 == 0 || from.1 == 0 {
        return spans;
    }
    let sx = to.0 as f32 / from.0 as f32;
    let sy = to.1 as f32 / from.1 as f32;
    for s in &mut spans {
        s.x *= sx;
        s.y *= sy;
        s.width *= sx;
        s.height *= sy;
    }
    spans
}

/// Scale stroke geometry from logical ink space onto the page-bin raster.
fn scale_strokes_to_page(strokes: &mut [ephemeris_core::Stroke], from: (u32, u32), to: (u32, u32)) {
    if from == to || from.0 == 0 || from.1 == 0 {
        return;
    }
    let sx = to.0 as f32 / from.0 as f32;
    let sy = to.1 as f32 / from.1 as f32;
    let s_avg = (sx + sy) * 0.5;
    for stroke in strokes {
        stroke.base_width *= s_avg;
        for p in &mut stroke.points {
            p.x *= sx;
            p.y *= sy;
        }
    }
}

fn read_page_bin(path: &Path) -> Option<(u32, u32, Vec<u8>)> {
    let data = std::fs::read(path).ok()?;
    if data.len() >= 12 && &data[0..4] == PAGE_BIN_MAGIC {
        let width = u32::from_le_bytes(data[4..8].try_into().ok()?);
        let height = u32::from_le_bytes(data[8..12].try_into().ok()?);
        return Some((width, height, data[12..].to_vec()));
    }
    // Legacy raw gray buffer — assume 800-wide canvas.
    let width = 800u32;
    if data.len() % width as usize == 0 {
        let height = (data.len() / width as usize) as u32;
        Some((width, height, data))
    } else {
        None
    }
}

/// Load document page `doc_page` for a note. Page 0 falls back to legacy
/// `page_{id}.bin` when the paged file is missing.
fn load_note_page_n(data_dir: &Path, note_id: u32, doc_page: usize) -> Option<(u32, u32, Vec<u8>)> {
    let primary = note_page_path(data_dir, note_id, doc_page);
    if let Some(loaded) = read_page_bin(&primary) {
        return Some(loaded);
    }
    if doc_page == 0 {
        read_page_bin(&note_page_legacy_path(data_dir, note_id))
    } else {
        None
    }
}

/// Load a note page raster (document page 0). Returns `(width, height, gray8)`.
fn load_note_page(data_dir: &Path, note_id: u32) -> Option<(u32, u32, Vec<u8>)> {
    load_note_page_n(data_dir, note_id, 0)
}

fn delete_note_page_files(data_dir: &Path, note_id: u32, doc_page_count: usize) {
    let _ = std::fs::remove_file(note_page_legacy_path(data_dir, note_id));
    for p in 0..doc_page_count.max(1) {
        let _ = std::fs::remove_file(note_page_path(data_dir, note_id, p));
        let _ = std::fs::remove_file(note_strokes_path(data_dir, note_id, p));
        PageRecognitionCache::delete(data_dir, note_id, p);
    }
}

fn set_note_doc_page_chrome(ui: &EphemerisUi, note: &AppNote) {
    let count = note.doc_page_count.max(1);
    let idx = (note.doc_page + 1).min(count);
    ui.set_page_index(idx as i32, count as i32);
}

/// Persist the live canvas into `note`'s current doc page, then load `target`
/// into the note's PageBook slot (creating a blank page when missing).
fn switch_note_doc_page(
    ui: &EphemerisUi,
    data_dir: &Path,
    note: &mut AppNote,
    target: usize,
    create_if_missing: bool,
) {
    let pixels = ui.get_committed_pixels_arc();
    save_note_page_pixels(
        data_dir,
        note.id,
        note.doc_page,
        pixels.width,
        pixels.height,
        &pixels.data,
    );
    save_note_page_strokes(data_dir, note.id, note.doc_page, &ui.get_page_strokes());

    let count = note.doc_page_count.max(1);
    let target = if create_if_missing && target >= count {
        note.doc_page_count = target + 1;
        target
    } else {
        target.min(count.saturating_sub(1))
    };
    note.doc_page = target;
    note.updated_at = now_secs();

    if let Some((pw, ph, data)) = load_note_page_n(data_dir, note.id, target) {
        ui.restore_page_pixels(note.page_index, pw, ph, data);
        if let Some(strokes) = load_note_page_strokes(data_dir, note.id, target) {
            ui.restore_page_strokes(note.page_index, strokes);
        } else {
            // Legacy raster-only page: keep pixels, empty stroke list.
            ui.restore_page_strokes(note.page_index, Vec::new());
        }
        ui.clear_strokes_dirty();
    } else {
        ui.clear_ink();
    }
    ui.discard_in_progress_stroke();
    set_note_doc_page_chrome(ui, note);
}

/// Persists local + handwritten-checkbox tasks (`tasks_local.json`, ephemeris-rjz).
struct TaskPersist {
    path: PathBuf,
    /// Note-task ink_seen / user_edited bookkeeping (see `NoteTaskState`).
    state: RefCell<ephemeris_core::NoteTaskState>,
    /// Last JSON written, so unchanged state costs no disk write.
    last: RefCell<String>,
}

impl TaskPersist {
    fn new(data_dir: &Path) -> Self {
        Self {
            path: local_tasks_path(data_dir),
            state: RefCell::default(),
            last: RefCell::default(),
        }
    }

    /// Note tasks the user ticked/edited must survive erasing their ink.
    fn mark_edited(&self, task: &Task) {
        if task.source == ephemeris_core::NOTE_TASK_SOURCE {
            self.state.borrow_mut().user_edited.insert(task.id);
        }
    }

    /// Write the store if it differs from the last write (atomic rename).
    fn save(&self, tasks: &[Task]) {
        let json =
            ephemeris_core::LocalTaskStore::from_tasks(tasks, &self.state.borrow()).to_json();
        if json.is_empty() || json == *self.last.borrow() {
            return;
        }
        let tmp = self.path.with_extension("json.tmp");
        let res = std::fs::write(&tmp, &json).and_then(|_| std::fs::rename(&tmp, &self.path));
        match res {
            Ok(()) => *self.last.borrow_mut() = json,
            Err(e) => tracing::warn!("Failed to save local tasks: {e}"),
        }
    }
}

fn local_tasks_path(data_dir: &Path) -> PathBuf {
    data_dir.join("tasks_local.json")
}

fn load_local_task_store(data_dir: &Path) -> Option<ephemeris_core::LocalTaskStore> {
    let json = std::fs::read_to_string(local_tasks_path(data_dir)).ok()?;
    ephemeris_core::LocalTaskStore::from_json(&json)
}

fn notes_index_path(data_dir: &Path) -> PathBuf {
    data_dir.join("notes_index.json")
}

fn save_notes_index(
    data_dir: &Path,
    notes: &[AppNote],
    notebooks: &[AppNotebook],
    note_id_seq: u32,
    notebook_id_seq: u32,
) {
    let index = NotesIndex {
        note_id_seq,
        notebook_id_seq,
        notebooks: notebooks.to_vec(),
        notes: notes.to_vec(),
    };
    match serde_json::to_string(&index) {
        Ok(json) => {
            if let Err(e) = std::fs::write(notes_index_path(data_dir), json) {
                tracing::warn!("Failed to save notes index: {e}");
            }
        }
        Err(e) => tracing::warn!("Failed to serialize notes index: {e}"),
    }
}

/// Persist notes index; defer vault OCR/PDF to leave-note / 10‑minute batch.
fn save_notes_index_and_sync_vault(
    data_dir: &Path,
    notes: &[AppNote],
    notebooks: &[AppNotebook],
    note_id_seq: u32,
    notebook_id_seq: u32,
    config: &Config,
    vault_export: &VaultExportQueue,
) {
    save_notes_index(data_dir, notes, notebooks, note_id_seq, notebook_id_seq);
    // Structural / metadata change: one deferred full library pass (same
    // cascade as before) — never block the UI thread with OCR/PDF. Notes
    // without a per-profile vault are skipped at flush time. The legacy
    // `vault_path` flag is a cheap "any export configured" hint; flush uses
    // per-profile connections as the source of truth.
    if config.vault_path.is_some() {
        vault_export.mark_full_sync();
    }
}

/// Profile id that owns a note for vault export (note → notebook fallback).
fn note_export_profile_id(note: &AppNote, notebooks: &[AppNotebook]) -> String {
    if !note.profile_id.is_empty() {
        return note.profile_id.clone();
    }
    if let Some(nb_id) = note.notebook_id {
        if let Some(nb) = notebooks.iter().find(|n| n.id == nb_id) {
            if !nb.profile_id.is_empty() {
                return nb.profile_id.clone();
            }
        }
    }
    String::new()
}

/// Absolute vault directory for `profile_id`, if that profile has a vault connection.
fn vault_path_for_profile(connections: &[AppConnection], profile_id: &str) -> Option<PathBuf> {
    if profile_id.is_empty() {
        return None;
    }
    connections
        .iter()
        .find(|c| c.kind == "vault" && c.profile_id == profile_id)
        .map(|c| expand_user_path(&c.value))
        .filter(|p| p.is_dir())
}

/// Export one note's PDF + markdown without kicking git (batch caller pushes once).
///
/// Returns the vault path written when export ran; `None` when skipped (no
/// note, or no vault connection for the note's profile).
fn export_single_note_to_vault_no_git(
    config: &Config,
    notes: &[AppNote],
    notebooks: &[AppNotebook],
    data_dir: &Path,
    connections: &[AppConnection],
    note_id: u32,
) -> Option<PathBuf> {
    let note = notes.iter().find(|n| n.id == note_id)?;
    let profile_id = note_export_profile_id(note, notebooks);
    let vault = vault_path_for_profile(connections, &profile_id)?;
    let (notebook_title, notebook_segments) = notebook_export_parts(notebooks, note.notebook_id);
    let export = build_vault_note_export(
        config,
        &vault,
        data_dir,
        note,
        &notebook_title,
        &notebook_segments,
    );
    if let Err(e) =
        ephemeris_core::export_note_markdown(&vault, &config.vault_export_subdir, &export)
    {
        tracing::warn!("Vault markdown export for note {note_id} failed: {e}");
    }
    Some(vault)
}

fn build_vault_note_export(
    config: &Config,
    vault: &Path,
    data_dir: &Path,
    note: &AppNote,
    notebook_title: &str,
    notebook_segments: &[String],
) -> VaultNoteExport {
    let export_root = &config.vault_export_subdir;
    let note_safe = ephemeris_core::vault_export::sanitize_component(&note.title);
    let note_dir = ephemeris_core::notebook_export_dir(vault, export_root, notebook_segments);
    // Keep notebook folders markdown-first: PDF + hash live under `_assets/`.
    ephemeris_core::migrate_legacy_note_pdf(&note_dir, &note_safe);
    let pdf_abs =
        ephemeris_core::note_pdf_export_path(vault, export_root, notebook_segments, &note_safe);

    let mut pdf_ok = false;
    // Prefer freshly recognised text over a cached See-text snapshot so export
    // tracks newly added ink. Use the note's current document page (not always 0).
    let mut recognised_text: Option<String> = None;
    let mut recognised_markdown: Option<String> = None;
    if let Some((w, h, gray)) = load_note_page_n(data_dir, note.id, note.doc_page) {
        let raster = ephemeris_pal::display::PixelBuf {
            width: w,
            height: h,
            stride: w,
            data: gray,
        };
        let input = transcription_raster(&raster, design_canvas_size());
        let page = RasterPage {
            width: w,
            height: h,
            gray8: raster.data,
        };
        let strokes = load_note_page_strokes(data_dir, note.id, note.doc_page).unwrap_or_default();
        let cascade = build_cascading_transcriber(config);
        // Ink pages go through the per-line recognition cache so the export keeps
        // line formatting and only re-recognises changed lines; raster-only pages
        // fall back to a fresh page transcription.
        let spans = if strokes.is_empty() {
            cascade
                .transcribe_page(&[], input.width, input.height, &input.data)
                .unwrap_or_else(|e| {
                    tracing::debug!("Transcription for note {} skipped: {e}", note.id);
                    Vec::new()
                })
        } else {
            let cache = recognise_page_cache(config, data_dir, note, &strokes, &cascade);
            recognised_markdown = Some(cache.completed_markdown()).filter(|m| !m.is_empty());
            cache.to_spans()
        };
        let fresh = RasterOcrTranscriber::spans_to_text(&spans);
        if !fresh.is_empty() {
            recognised_text = Some(fresh);
        }
        let searchable_text = recognised_text
            .clone()
            .or_else(|| note.text_content.as_ref().map(|t| strip_inline_markdown(t)));
        // Yellow highlighter in PDF: pass Highlighter strokes so export can
        // multiply-blend classic marker yellow over the gray8 canvas (mono
        // e-ink UI stays gray; colour PDF / colour e-ink viewers show yellow).
        // Strokes live in logical ink space; scale onto the page-bin raster.
        let mut highlight_strokes: Vec<_> = strokes
            .iter()
            .filter(|s| s.tool == Tool::Highlighter)
            .cloned()
            .collect();
        scale_strokes_to_page(&mut highlight_strokes, (input.width, input.height), (w, h));
        let opts = PdfExportOptions {
            title: note.title.clone(),
            searchable_text,
            page_spans: if spans.is_empty() {
                vec![]
            } else {
                vec![scale_spans(spans, (input.width, input.height), (w, h))]
            },
            page_highlight_strokes: if highlight_strokes.is_empty() {
                vec![]
            } else {
                vec![highlight_strokes]
            },
        };
        match export_raster_pages_to_pdf(&[page], &pdf_abs, &opts) {
            Ok(0) => {
                pdf_ok = true;
                tracing::debug!(
                    "PDF export for note {} skipped (content hash current): {}",
                    note.id,
                    pdf_abs.display()
                );
            }
            Ok(_) => pdf_ok = true,
            Err(e) => tracing::warn!("PDF export for note {} failed: {e}", note.id),
        }
    }

    let pdf_vault_rel = if pdf_ok || pdf_abs.exists() {
        Some(ephemeris_core::note_pdf_export_rel(
            export_root,
            notebook_segments,
            &note_safe,
        ))
    } else {
        None
    };

    VaultNoteExport {
        notebook_title: notebook_title.to_string(),
        notebook_segments: notebook_segments.to_vec(),
        note_title: note.title.clone(),
        note_id: note.id,
        created_at: note.created_at,
        text_content: recognised_markdown
            .or(recognised_text)
            .or_else(|| note.text_content.clone()),
        pdf_vault_rel,
        tags: note.tags.clone(),
    }
}

/// Bring a page's recognition cache up to date with `strokes` (recognising only
/// changed lines) and persist it.
fn recognise_page_cache(
    config: &Config,
    data_dir: &Path,
    note: &AppNote,
    strokes: &[ephemeris_core::Stroke],
    cascade: &CascadingTranscriber,
) -> PageRecognitionCache {
    let version = recognition_model_version_for(config);
    let ruling = template_ruling_pitch(note.paper_template);
    let mut cache = PageRecognitionCache::load(data_dir, note.id, note.doc_page)
        .unwrap_or_else(|| PageRecognitionCache::new(&version));
    cache.sync_strokes(strokes, ruling, &version);
    if cache.has_pending() {
        let _ = cache.recognise_pending(strokes, ruling, &|| false, |line, idx| {
            cascade.transcribe_line(line, idx)
        });
        if let Err(e) = cache.save(data_dir, note.id, note.doc_page) {
            tracing::warn!("Failed to save recognition cache for note {}: {e}", note.id);
        }
    }
    cache
}

fn sync_notes_export_to_vault(
    config: &Config,
    notes: &[AppNote],
    notebooks: &[AppNotebook],
    data_dir: &Path,
    connections: &[AppConnection],
) {
    let export_root = &config.vault_export_subdir;
    // Group notes by their profile's vault so one profile never writes into
    // another profile's Obsidian folder. Profiles without a vault are skipped.
    let mut by_vault: Vec<(PathBuf, Vec<VaultNoteExport>)> = Vec::new();
    let mut skipped = 0usize;
    for n in notes {
        let profile_id = note_export_profile_id(n, notebooks);
        let Some(vault) = vault_path_for_profile(connections, &profile_id) else {
            skipped += 1;
            continue;
        };
        let (notebook_title, notebook_segments) = notebook_export_parts(notebooks, n.notebook_id);
        let export = build_vault_note_export(
            config,
            &vault,
            data_dir,
            n,
            &notebook_title,
            &notebook_segments,
        );
        if let Some((_, bucket)) = by_vault.iter_mut().find(|(p, _)| p == &vault) {
            bucket.push(export);
        } else {
            by_vault.push((vault, vec![export]));
        }
    }

    let mut total = 0usize;
    for (vault, payload) in &by_vault {
        match sync_notes_to_vault(vault, export_root, payload) {
            Ok(n) => {
                total += n;
                tracing::debug!(
                    "Synced {n} notes to vault export under {}/{}",
                    vault.display(),
                    export_root.display()
                );
            }
            Err(e) => tracing::warn!("Vault note export sync failed for {}: {e}", vault.display()),
        }
        schedule_vault_git_push_for(config, vault);
    }
    if skipped > 0 {
        tracing::debug!("Skipped {skipped} note(s) with no per-profile vault connection");
    }
    if total == 0 && by_vault.is_empty() {
        tracing::debug!("No notes exported (no matching profile vaults)");
    }
}

/// Shared handles for the See-text line editor callbacks.
#[derive(Clone)]
struct SeeTextEditCtx {
    ui: Rc<EphemerisUi>,
    bg: Rc<BackgroundRecogQueue>,
    data_dir: PathBuf,
    notes: Rc<RefCell<Vec<AppNote>>>,
    notebooks: Rc<RefCell<Vec<AppNotebook>>>,
    note_id_seq: Rc<Cell<u32>>,
    notebook_id_seq: Rc<Cell<u32>>,
    config: Rc<RefCell<Config>>,
    vault_export: Rc<VaultExportQueue>,
}

impl SeeTextEditCtx {
    /// Apply `edit` to cache line `idx` of the active note page, then persist
    /// the cache, refresh `note.text_content`, the notes index and the vault
    /// export (same path as a finished background recognition), and re-push
    /// the See-text rows.
    fn apply(
        &self,
        idx: i32,
        edit: impl FnOnce(&mut ephemeris_core::recognition_cache::PageRecognitionCache, usize),
    ) {
        let Ok(idx) = usize::try_from(idx) else {
            return;
        };
        let Ok(note_id) = self
            .ui
            .clone_component()
            .get_active_note_id()
            .to_string()
            .parse::<u32>()
        else {
            return;
        };
        let Some(doc_page) = self
            .notes
            .borrow()
            .iter()
            .find(|n| n.id == note_id)
            .map(|n| n.doc_page)
        else {
            return;
        };
        let version = recognition_model_version_for(&self.config.borrow());
        let mut cache = self
            .bg
            .get_or_load(&self.data_dir, note_id, doc_page, &version);
        if idx >= cache.lines.len() {
            return;
        }
        edit(&mut cache, idx);
        if let Err(e) = cache.save(&self.data_dir, note_id, doc_page) {
            tracing::warn!("Failed to save recognition cache for note {note_id}: {e}");
        }
        let completed = cache.completed_markdown();
        let rows = see_text_rows(&cache, Some(&completed));
        self.bg.store(note_id, doc_page, cache);
        if let Some(note) = self.notes.borrow_mut().iter_mut().find(|n| n.id == note_id) {
            note.text_content = if completed.is_empty() {
                None
            } else {
                Some(completed)
            };
            note.updated_at = now_secs();
        }
        save_notes_index_and_sync_vault(
            &self.data_dir,
            &self.notes.borrow(),
            &self.notebooks.borrow(),
            self.note_id_seq.get(),
            self.notebook_id_seq.get(),
            &self.config.borrow(),
            &self.vault_export,
        );
        self.vault_export.mark(note_id);
        self.ui.set_see_text_rows(&rows);
        self.ui.request_screen_change();
    }
}

/// See-text rows: from the live cache lines, else from stored markdown.
fn see_text_rows(
    cache: &ephemeris_core::recognition_cache::PageRecognitionCache,
    stored_markdown: Option<&str>,
) -> Vec<ephemeris_core::see_text::SeeTextRow> {
    if cache.lines.is_empty() {
        ephemeris_core::see_text::rows_from_markdown(stored_markdown.unwrap_or_default())
    } else {
        ephemeris_core::see_text::rows_from_cache(cache)
    }
}

/// Push see-text content for the active note into the UI.
fn push_note_text_for_active(ui: &ephemeris_ui::EphemerisUi, notes: &[AppNote], active_id: &str) {
    let text = notes
        .iter()
        .find(|n| n.id.to_string() == active_id)
        .and_then(|n| n.text_content.clone())
        .unwrap_or_default();
    ui.set_see_text_rows(&ephemeris_core::see_text::rows_from_markdown(&text));
    ui.set_note_see_text(false);
}

fn load_notes_index(data_dir: &Path) -> Option<NotesIndex> {
    let json = std::fs::read_to_string(notes_index_path(data_dir)).ok()?;
    serde_json::from_str(&json).ok()
}

// ── Calendar helpers ──────────────────────────────────────────────────────────

fn days_in_month(year: i32, month: u32) -> u32 {
    let next_month = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)
    };
    next_month
        .and_then(|d| d.pred_opt())
        .map(|d| d.day())
        .unwrap_or(30)
}

fn month_name(month: u32) -> &'static str {
    match month {
        1 => "January",
        2 => "February",
        3 => "March",
        4 => "April",
        5 => "May",
        6 => "June",
        7 => "July",
        8 => "August",
        9 => "September",
        10 => "October",
        11 => "November",
        12 => "December",
        _ => "Unknown",
    }
}

fn weekday_short(wd: chrono::Weekday) -> &'static str {
    match wd {
        chrono::Weekday::Mon => "Mon",
        chrono::Weekday::Tue => "Tue",
        chrono::Weekday::Wed => "Wed",
        chrono::Weekday::Thu => "Thu",
        chrono::Weekday::Fri => "Fri",
        chrono::Weekday::Sat => "Sat",
        chrono::Weekday::Sun => "Sun",
    }
}

/// Unix timestamp → NaiveDateTime (UTC).
fn ts_to_naive(ts: u64) -> NaiveDateTime {
    DateTime::from_timestamp(ts as i64, 0)
        .map(|dt| dt.naive_utc())
        .unwrap_or_default()
}

/// Build month-grid data: `(day, has_events, is_today, in_month)` for 42 cells.
fn cal_month_days(
    year: i32,
    month: u32,
    events: &[CalendarEvent],
) -> Vec<(i32, bool, bool, bool, String)> {
    let today = Utc::now().date_naive();
    let first = NaiveDate::from_ymd_opt(year, month, 1).unwrap_or(today);
    // Monday-based weekday offset (Mon=0)
    let start_offset = first.weekday().num_days_from_monday() as i32;
    let total_days = days_in_month(year, month) as i32;

    let month_start_ts = first
        .and_hms_opt(0, 0, 0)
        .and_then(|dt| dt.and_utc().timestamp().try_into().ok())
        .unwrap_or(0u64);
    let month_end_ts = month_start_ts + total_days as u64 * 86400;

    // Map day number → first event title for that day
    let mut event_day_titles = std::collections::HashMap::<u32, String>::new();
    let mut sorted_events: Vec<_> = events
        .iter()
        .filter(|e| e.start >= month_start_ts && e.start < month_end_ts)
        .collect();
    sorted_events.sort_by_key(|e| e.start);
    for e in sorted_events {
        let day_num = ((e.start - month_start_ts) / 86400) as u32 + 1;
        event_day_titles
            .entry(day_num)
            .or_insert_with(|| e.title.clone());
    }

    let mut result = Vec::with_capacity(42);
    for _ in 0..start_offset {
        result.push((0, false, false, false, String::new()));
    }
    for d in 1..=total_days {
        let date = NaiveDate::from_ymd_opt(year, month, d as u32).unwrap_or(today);
        let title = event_day_titles
            .get(&(d as u32))
            .cloned()
            .unwrap_or_default();
        result.push((d, !title.is_empty(), date == today, true, title));
    }
    while result.len() < 42 {
        result.push((0, false, false, false, String::new()));
    }
    result.truncate(42);
    result
}

/// UI calendar row: `(title, time_label, all_day, col, item_kind, item_id)`.
type CalUiEvent = (String, String, bool, i32, String, String);

fn cal_item_meta(e: &CalendarEvent) -> (String, String) {
    match e.source.as_str() {
        "note" => (
            "note".to_string(),
            e.uid.strip_prefix("note:").unwrap_or("").to_string(),
        ),
        "recording" => (
            "recording".to_string(),
            e.uid.strip_prefix("recording:").unwrap_or("").to_string(),
        ),
        _ => ("event".to_string(), String::new()),
    }
}

/// Build day events: `(title, time_label, all_day, col=0, kind, id)`.
fn cal_day_events(year: i32, month: u32, day: u32, events: &[CalendarEvent]) -> Vec<CalUiEvent> {
    let date = NaiveDate::from_ymd_opt(year, month, day).unwrap_or_default();
    let day_start = date
        .and_hms_opt(0, 0, 0)
        .and_then(|dt| dt.and_utc().timestamp().try_into().ok())
        .unwrap_or(0u64);
    let day_end = day_start + 86400;

    let mut result: Vec<_> = events
        .iter()
        .filter(|e| e.start < day_end && e.end > day_start)
        .map(|e| {
            let all_day = e.end.saturating_sub(e.start) >= 86399;
            let label = if all_day {
                match e.source.as_str() {
                    "note" => "Note".to_string(),
                    "recording" => "Recording".to_string(),
                    _ => "All day".to_string(),
                }
            } else {
                let start_dt = ts_to_naive(e.start);
                let end_dt = ts_to_naive(e.end);
                format!(
                    "{:02}:{:02}–{:02}:{:02}",
                    start_dt.hour(),
                    start_dt.minute(),
                    end_dt.hour(),
                    end_dt.minute()
                )
            };
            let (kind, id) = cal_item_meta(e);
            (e.title.clone(), label, all_day, 0i32, kind, id)
        })
        .collect();
    result.sort_by(|a, b| a.2.cmp(&b.2).reverse().then(a.1.cmp(&b.1)));
    result
}

/// Build week events: col = day-of-week offset from Monday (0=Mon).
fn cal_week_events(
    year: i32,
    month: u32,
    day: u32,
    events: &[CalendarEvent],
) -> (Vec<String>, Vec<CalUiEvent>) {
    let date = NaiveDate::from_ymd_opt(year, month, day).unwrap_or_default();
    let mon_offset = date.weekday().num_days_from_monday() as i64;
    let monday = date - chrono::Duration::days(mon_offset);

    let week_start_ts: u64 = monday
        .and_hms_opt(0, 0, 0)
        .and_then(|dt| dt.and_utc().timestamp().try_into().ok())
        .unwrap_or(0);
    let week_end_ts = week_start_ts + 7 * 86400;

    let labels: Vec<String> = (0..7)
        .map(|i| {
            let d = monday + chrono::Duration::days(i);
            format!("{}\n{}", weekday_short(d.weekday()), d.day())
        })
        .collect();

    let mut result: Vec<_> = events
        .iter()
        .filter(|e| e.start < week_end_ts && e.end > week_start_ts)
        .map(|e| {
            let col = ((e.start.saturating_sub(week_start_ts)) / 86400).min(6) as i32;
            let all_day = e.end.saturating_sub(e.start) >= 86399;
            let label = if all_day {
                match e.source.as_str() {
                    "note" => "Note".to_string(),
                    "recording" => "Rec".to_string(),
                    _ => "All day".to_string(),
                }
            } else {
                let dt = ts_to_naive(e.start);
                format!("{:02}:{:02}", dt.hour(), dt.minute())
            };
            let (kind, id) = cal_item_meta(e);
            (e.title.clone(), label, all_day, col, kind, id)
        })
        .collect();
    result.sort_by(|a, b| {
        a.3.cmp(&b.3)
            .then(a.2.cmp(&b.2).reverse())
            .then(a.1.cmp(&b.1))
    });
    (labels, result)
}

fn utc_day_start(ts: u64) -> u64 {
    let dt = DateTime::<Utc>::from_timestamp(ts as i64, 0).unwrap_or_else(Utc::now);
    dt.date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|ndt| ndt.and_utc().timestamp().try_into().ok())
        .unwrap_or(ts)
}

fn recording_visible(
    r: &AppRecording,
    profile_filter: &str,
    recordings_in_all: &HashSet<String>,
) -> bool {
    if profile_filter.is_empty() {
        r.profile_id.is_empty() || recordings_in_all.contains(&r.profile_id)
    } else {
        r.profile_id == profile_filter
    }
}

/// Append notes/recordings as synthetic all-day calendar entries (creation date).
fn append_notes_recordings_as_events(
    events: &mut Vec<CalendarEvent>,
    notes: &[AppNote],
    recordings: &[AppRecording],
    profile_filter: &str,
    notes_in_all: &HashSet<String>,
    recordings_in_all: &HashSet<String>,
) {
    let fallback_pid = ProfileId(uuid::Uuid::nil());
    for n in notes
        .iter()
        .filter(|n| note_visible(n, profile_filter, notes_in_all))
    {
        let start = utc_day_start(n.created_at);
        events.push(CalendarEvent {
            id: EventId::new(),
            uid: format!("note:{}", n.id),
            start,
            end: start + 86399,
            title: n.title.clone(),
            location: None,
            source: "note".to_string(),
            profile_id: parse_profile_id(&n.profile_id).unwrap_or(fallback_pid),
        });
    }
    for r in recordings
        .iter()
        .filter(|r| recording_visible(r, profile_filter, recordings_in_all))
    {
        let start = utc_day_start(r.created_at);
        events.push(CalendarEvent {
            id: EventId::new(),
            uid: format!("recording:{}", r.id),
            start,
            end: start + 86399,
            title: r.title.clone(),
            location: None,
            source: "recording".to_string(),
            profile_id: parse_profile_id(&r.profile_id).unwrap_or(fallback_pid),
        });
    }
}

fn visible_cal_events(
    events: &[CalendarEvent],
    notes: &[AppNote],
    recordings: &[AppRecording],
    profile_filter: &str,
    notes_in_all: &HashSet<String>,
    recordings_in_all: &HashSet<String>,
) -> Vec<CalendarEvent> {
    let mut visible = filter_events_by_profile(events, profile_filter);
    append_notes_recordings_as_events(
        &mut visible,
        notes,
        recordings,
        profile_filter,
        notes_in_all,
        recordings_in_all,
    );
    visible
}

fn local_events_path(data_dir: &Path) -> PathBuf {
    data_dir.join("local_events.json")
}

fn load_local_events(data_dir: &Path) -> Vec<CalendarEvent> {
    let Ok(json) = std::fs::read_to_string(local_events_path(data_dir)) else {
        return Vec::new();
    };
    serde_json::from_str(&json).unwrap_or_default()
}

fn save_local_events(data_dir: &Path, events: &[CalendarEvent]) {
    let local: Vec<&CalendarEvent> = events.iter().filter(|e| e.source == "local").collect();
    if let Ok(json) = serde_json::to_string_pretty(&local) {
        let _ = std::fs::write(local_events_path(data_dir), json);
    }
}

fn parse_hm(s: &str) -> Option<(u32, u32)> {
    let mut parts = s.trim().split(':');
    let h: u32 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some((h, m))
}

/// Year view: `(name, short, event_count, month_num)` for months 1..=12.
fn cal_year_months(year: i32, events: &[CalendarEvent]) -> Vec<(String, String, i32, i32)> {
    (1u32..=12)
        .map(|m| {
            let m_start = NaiveDate::from_ymd_opt(year, m, 1)
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .and_then(|dt| dt.and_utc().timestamp().try_into().ok())
                .unwrap_or(0u64);
            let m_end = m_start + days_in_month(year, m) as u64 * 86400;
            let count = events
                .iter()
                .filter(|e| e.start >= m_start && e.start < m_end)
                .count() as i32;
            (
                month_name(m).to_string(),
                month_short(m).to_string(),
                count,
                m as i32,
            )
        })
        .collect()
}

/// Push all calendar data for the given date and sub-view to the UI.
fn refresh_calendar(
    ui: &ephemeris_ui::EphemerisUi,
    events: &[CalendarEvent],
    year: i32,
    month: u32,
    day: u32,
    sub_view: i32,
) {
    ui.set_cal_year(year);
    ui.set_cal_month(month as i32);
    ui.set_cal_day(day as i32);
    match sub_view {
        0 => {
            let label = format!("{} {}, {}", month_short(month), day, year);
            ui.set_cal_header_label(&label);
            let evts = cal_day_events(year, month, day, events);
            ui.set_cal_events(&evts);
        }
        1 => {
            let date = NaiveDate::from_ymd_opt(year, month, day).unwrap_or_default();
            let mon_off = date.weekday().num_days_from_monday() as i64;
            let monday = date - chrono::Duration::days(mon_off);
            let sunday = monday + chrono::Duration::days(6);
            let label = if monday.month() == sunday.month() {
                format!(
                    "{} {}-{}, {}",
                    month_short(monday.month()),
                    monday.day(),
                    sunday.day(),
                    monday.year()
                )
            } else {
                format!(
                    "{} {}–{} {}",
                    month_short(monday.month()),
                    monday.day(),
                    month_short(sunday.month()),
                    sunday.day()
                )
            };
            ui.set_cal_header_label(&label);
            let (labels, evts) = cal_week_events(year, month, day, events);
            ui.set_cal_week_day_labels(&labels);
            ui.set_cal_events(&evts);
        }
        2 => {
            let label = format!("{} {}", month_name(month), year);
            ui.set_cal_header_label(&label);
            let days = cal_month_days(year, month, events);
            ui.set_cal_month_days(&days);
        }
        3 => {
            ui.set_cal_header_label(&year.to_string());
            let months = cal_year_months(year, events);
            ui.set_cal_year_months(&months);
        }
        _ => {}
    }
}

/// Format a short date label for the anchor chip, e.g. "Sep 23".
fn format_day_anchor_label(_year: i32, month: u32, day: u32) -> String {
    format!("{} {}", month_short(month), day)
}

/// Set day-note UI properties for the calendar day view.
fn refresh_day_note_ui(
    ui: &ephemeris_ui::EphemerisUi,
    notes: &[AppNote],
    year: i32,
    month: u32,
    day: u32,
) {
    let anchor_key = format!("cal:{:04}-{:02}-{:02}", year, month, day);
    if let Some(note) = notes
        .iter()
        .find(|n| n.anchor.as_deref() == Some(anchor_key.as_str()))
    {
        ui.set_cal_day_note_exists(true);
        ui.set_cal_day_note_id(&note.id.to_string());
    } else {
        ui.set_cal_day_note_exists(false);
        ui.set_cal_day_note_id("");
    }
}

fn cal_navigate_prev(year: i32, month: u32, day: u32, sub_view: i32) -> (i32, u32, u32) {
    match sub_view {
        0 => {
            let d = NaiveDate::from_ymd_opt(year, month, day)
                .and_then(|d| d.pred_opt())
                .unwrap_or_default();
            (d.year(), d.month(), d.day())
        }
        1 => {
            let d = NaiveDate::from_ymd_opt(year, month, day)
                .map(|d| d - chrono::Duration::weeks(1))
                .unwrap_or_default();
            (d.year(), d.month(), d.day())
        }
        2 => {
            if month == 1 {
                (year - 1, 12, 1)
            } else {
                (year, month - 1, 1)
            }
        }
        3 => (year - 1, month, day),
        _ => (year, month, day),
    }
}

fn cal_navigate_next(year: i32, month: u32, day: u32, sub_view: i32) -> (i32, u32, u32) {
    match sub_view {
        0 => {
            let d = NaiveDate::from_ymd_opt(year, month, day)
                .and_then(|d| d.succ_opt())
                .unwrap_or_default();
            (d.year(), d.month(), d.day())
        }
        1 => {
            let d = NaiveDate::from_ymd_opt(year, month, day)
                .map(|d| d + chrono::Duration::weeks(1))
                .unwrap_or_default();
            (d.year(), d.month(), d.day())
        }
        2 => {
            if month == 12 {
                (year + 1, 1, 1)
            } else {
                (year, month + 1, 1)
            }
        }
        3 => (year + 1, month, day),
        _ => (year, month, day),
    }
}

// ── Data loading helpers ──────────────────────────────────────────────────────

/// Expand `~` / `$HOME` prefixes so vault paths from the UI resolve on disk.
fn expand_user_path(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return PathBuf::new();
    }
    if trimmed == "~" {
        return home_dir();
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    if let Some(rest) = trimmed.strip_prefix("$HOME/") {
        return home_dir().join(rest);
    }
    PathBuf::from(trimmed)
}

fn connections_path() -> PathBuf {
    profiles_data_dir().join("connections.json")
}

fn save_connections(conns: &[AppConnection]) {
    match serde_json::to_string_pretty(conns) {
        Ok(json) => {
            let path = connections_path();
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::write(&path, json) {
                tracing::warn!("Failed to save connections: {e}");
            }
        }
        Err(e) => tracing::warn!("Failed to serialize connections: {e}"),
    }
}

fn load_connections() -> Option<Vec<AppConnection>> {
    let json = std::fs::read_to_string(connections_path()).ok()?;
    serde_json::from_str(&json).ok()
}

/// Vault directories visible for the current profile filter.
///
/// - Specific profile: vaults assigned to that profile id.
/// - All (`filter` empty): vaults with `allow_in_aggregated`, else any vault.
/// - Falls back to legacy `config.vault_path` when connections are empty.
fn vaults_for_search(filter: &str, config: &Config, connections: &[AppConnection]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let vault_conns: Vec<&AppConnection> =
        connections.iter().filter(|c| c.kind == "vault").collect();

    let selected: Vec<&AppConnection> = if filter.is_empty() {
        let agg: Vec<_> = vault_conns
            .iter()
            .copied()
            .filter(|c| c.allow_in_aggregated)
            .collect();
        if agg.is_empty() {
            vault_conns
        } else {
            agg
        }
    } else {
        vault_conns
            .iter()
            .copied()
            .filter(|c| c.profile_id == filter)
            .collect()
    };

    for c in selected {
        let expanded = expand_user_path(&c.value);
        if expanded.is_dir() {
            if !out.iter().any(|p| p == &expanded) {
                out.push(expanded);
            }
        } else {
            tracing::warn!(
                "Vault connection {:?} ({}) is not a directory",
                c.value,
                c.display
            );
        }
    }

    if out.is_empty() {
        if let Some(ref p) = config.vault_path {
            let expanded = expand_user_path(&p.to_string_lossy());
            if expanded.is_dir() {
                out.push(expanded);
            }
        }
    }
    out
}

/// First searchable vault path (for opening hits / legacy single-vault APIs).
fn resolve_vault_path(config: &Config, connections: &[AppConnection]) -> Option<PathBuf> {
    vaults_for_search("", config, connections)
        .into_iter()
        .next()
        .or_else(|| {
            // Prefer any vault connection even if not aggregated.
            connections
                .iter()
                .filter(|c| c.kind == "vault")
                .map(|c| expand_user_path(&c.value))
                .find(|p| p.is_dir())
        })
}

/// Run unified search across vault FTS, local notes, tasks, and calendar.
fn run_unified_search(
    query: &str,
    config: &Config,
    fts: &VaultFtsIndex,
    notes: &[AppNote],
    tasks: &[Task],
    events: &[CalendarEvent],
) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    // Search the FTS index whenever it has documents — path may live only on
    // connections if config.vault_path was never persisted.
    let _vault_configured = config.vault_path.is_some();
    match fts.len() {
        Ok(0) => {
            if _vault_configured {
                tracing::warn!(
                    "Vault configured but FTS index is empty — re-open Search to reindex"
                );
            }
        }
        Ok(n) => {
            tracing::debug!("Vault FTS has {n} docs; querying {query:?}");
            match fts.search(query, 40) {
                Ok(mut vault_hits) => {
                    tracing::info!("Vault search: {} hit(s) for {query:?}", vault_hits.len());
                    hits.append(&mut vault_hits);
                }
                Err(e) => tracing::warn!("Vault FTS search failed: {e}"),
            }
        }
        Err(e) => tracing::warn!("Vault FTS len failed: {e}"),
    }
    let local_iter = notes
        .iter()
        .map(|n| (n.id.to_string(), n.title.clone(), n.text_content.as_deref()));
    hits.extend(search_local_notes(local_iter, query, 20));
    let task_iter = tasks
        .iter()
        .map(|t| (t.id.0.to_string(), t.title.clone(), t.done));
    hits.extend(search_tasks(task_iter, query, 20));
    let event_iter = events.iter().map(|e| (e.id.0.to_string(), e.title.clone()));
    hits.extend(search_events(event_iter, query, 20));
    hits
}

fn load_vault_tasks_with_inbox(
    vault_path: &std::path::Path,
    inbox_rel: &std::path::Path,
    export_subdir: &std::path::Path,
    profile_id: ProfileId,
) -> ephemeris_core::Result<Vec<Task>> {
    let extractor = MarkdownTaskExtractor::with_inbox(vault_path, profile_id, inbox_rel)
        .with_exclude_export_subdir(export_subdir.to_path_buf());
    TaskProvider::list(&extractor, profile_id)
}

/// Replace in-memory Obsidian tasks with a fresh read from the vault.
fn reload_obsidian_tasks(
    tasks: &mut Vec<Task>,
    vault_path: &std::path::Path,
    inbox_rel: &std::path::Path,
    export_subdir: &std::path::Path,
    profile_id: ProfileId,
) {
    match load_vault_tasks_with_inbox(vault_path, inbox_rel, export_subdir, profile_id) {
        Ok(vault_tasks) => {
            tasks.retain(|t| t.source != "obsidian");
            tasks.extend(vault_tasks);
        }
        Err(e) => tracing::warn!("Vault task reload failed: {e}"),
    }
}

/// Persist an Obsidian-sourced task back to its markdown checkbox line.
fn write_back_obsidian_task(config: &Config, task: &Task) {
    let Some(vault) = config.vault_path.as_ref() else {
        return;
    };
    schedule_vault_git_pull(config);
    let extractor =
        MarkdownTaskExtractor::with_inbox(vault, task.profile_id, &config.vault_tasks_inbox)
            .with_exclude_export_subdir(config.vault_export_subdir.clone());
    if let Err(e) = TaskProvider::update(&extractor, task) {
        tracing::warn!("Obsidian task write-back failed for '{}': {e}", task.title);
    }
    schedule_vault_git_push(config);
}

/// Append a new task into the vault inbox and refresh in-memory Obsidian tasks.
fn create_obsidian_task(
    tasks: &mut Vec<Task>,
    config: &Config,
    task: &Task,
    profile_id: ProfileId,
) -> bool {
    let Some(vault) = config.vault_path.as_ref() else {
        return false;
    };
    schedule_vault_git_pull(config);
    let extractor = MarkdownTaskExtractor::with_inbox(vault, profile_id, &config.vault_tasks_inbox)
        .with_exclude_export_subdir(config.vault_export_subdir.clone());
    match TaskProvider::create(&extractor, task) {
        Ok(()) => {
            reload_obsidian_tasks(
                tasks,
                vault,
                &config.vault_tasks_inbox,
                &config.vault_export_subdir,
                profile_id,
            );
            schedule_vault_git_push(config);
            true
        }
        Err(e) => {
            tracing::warn!("Obsidian task create failed: {e}");
            false
        }
    }
}

/// Background `git pull` when vault git sync is enabled.
/// Conflicts under `vault_export_subdir` resolve with Ephemeris (local) winning.
fn schedule_vault_git_pull(config: &Config) {
    if !config.vault_git_enabled {
        return;
    }
    let Some(vault) = config.vault_path.clone() else {
        return;
    };
    if !ephemeris_core::is_git_repo(&vault) {
        return;
    }
    let export_subdir = config.vault_export_subdir.clone();
    std::thread::spawn(move || {
        match ephemeris_core::vault_git_pull(&vault, Some(export_subdir.as_path())) {
            Ok(()) => tracing::debug!("Vault git pull ok: {}", vault.display()),
            Err(e) => tracing::warn!("Vault git pull failed: {e}"),
        }
    });
}

/// Debounced background `git add/commit/push` for the legacy/active vault path.
fn schedule_vault_git_push(config: &Config) {
    if let Some(ref vault) = config.vault_path {
        schedule_vault_git_push_for(config, vault);
    }
}

/// Debounced background `git add/commit/push` after writes to a specific vault.
pub(crate) fn schedule_vault_git_push_for(config: &Config, vault: &Path) {
    if !config.vault_git_enabled {
        return;
    }
    if !ephemeris_core::is_git_repo(vault) {
        return;
    }
    if VAULT_GIT_PUSH_PENDING.swap(true, Ordering::AcqRel) {
        // Coalesce: run once more after the in-flight debounce finishes so a
        // push scheduled during the sleep window is not dropped.
        VAULT_GIT_PUSH_AGAIN.store(true, Ordering::Release);
        return;
    }
    let vault = vault.to_path_buf();
    let export_subdir = config.vault_export_subdir.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(3));
            VAULT_GIT_PUSH_AGAIN.store(false, Ordering::Release);
            match ephemeris_core::vault_git_commit_and_push(
                &vault,
                "ephemeris: sync notes and tasks",
                &export_subdir,
            ) {
                Ok(()) => tracing::debug!("Vault git commit/push ok: {}", vault.display()),
                Err(e) => tracing::warn!("Vault git commit/push failed: {e}"),
            }
            if !VAULT_GIT_PUSH_AGAIN.swap(false, Ordering::AcqRel) {
                VAULT_GIT_PUSH_PENDING.store(false, Ordering::Release);
                // Lost race: another schedule set AGAIN after we cleared it.
                if VAULT_GIT_PUSH_AGAIN.swap(false, Ordering::AcqRel)
                    && VAULT_GIT_PUSH_PENDING
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                        .is_ok()
                {
                    continue;
                }
                break;
            }
        }
    });
}

/// Parse every `.ics` file in `paths` and return all events in a 90-day window
/// starting from yesterday (so ongoing multi-day events are included).
fn load_ics_events(
    paths: &[std::path::PathBuf],
    profile_id: ProfileId,
    now: u64,
) -> Vec<CalendarEvent> {
    let window_start = now.saturating_sub(86_400);
    let window_end = now.saturating_add(90 * 86_400);
    let mut all = Vec::new();
    for path in paths {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                match IcsReader::parse_events(&text, profile_id, window_start, window_end) {
                    Ok(events) => {
                        tracing::info!("Loaded {} events from {}", events.len(), path.display());
                        all.extend(events);
                    }
                    Err(e) => tracing::warn!("ICS parse failed for {}: {e}", path.display()),
                }
            }
            Err(e) => tracing::warn!("ICS read failed for {}: {e}", path.display()),
        }
    }
    all
}

fn parse_profile_id(s: &str) -> Option<ProfileId> {
    uuid::Uuid::parse_str(s).ok().map(ProfileId)
}

/// Sync all CalDAV connections (blocking). Used on startup / settings save.
fn load_caldav_events(
    conns: &[AppConnection],
    default_pid: ProfileId,
    now: u64,
) -> Vec<CalendarEvent> {
    let window_start = now.saturating_sub(30 * 86_400);
    let window_end = now.saturating_add(180 * 86_400);
    let mut all = Vec::new();
    for c in conns.iter().filter(|c| c.kind == "caldav") {
        let pid = parse_profile_id(&c.profile_id).unwrap_or(default_pid);
        let cfg = ephemeris_core::CalDavConfig::new(&c.value, &c.username, &c.password);
        match ephemeris_core::CalDavClient::new(cfg)
            .and_then(|client| client.sync_all(pid, window_start, window_end))
        {
            Ok((cals, events)) => {
                tracing::info!(
                    "CalDAV {}: {} calendar(s), {} event(s)",
                    c.display,
                    cals.len(),
                    events.len()
                );
                all.extend(events);
            }
            Err(e) => tracing::warn!("CalDAV load failed for {}: {e}", c.display),
        }
    }
    all
}

/// Fetch remote ICS/webcal feeds and return all events in a 90-day window.
///
/// `webcal://` is rewritten to `https://` before fetching.
/// Requires the `ics` feature on `ephemeris-core`.
fn load_ics_from_urls(urls: &[String], profile_id: ProfileId, now: u64) -> Vec<CalendarEvent> {
    let window_start = now.saturating_sub(86_400);
    let window_end = now.saturating_add(90 * 86_400);
    let mut all = Vec::new();
    for raw_url in urls {
        let url = if raw_url.starts_with("webcals://") {
            raw_url.replacen("webcals://", "https://", 1)
        } else if raw_url.starts_with("webcal://") {
            raw_url.replacen("webcal://", "https://", 1)
        } else {
            raw_url.clone()
        };
        match IcsReader::fetch_feed(&url, profile_id, window_start, window_end) {
            Ok(events) => {
                tracing::info!("Fetched {} events from {}", events.len(), raw_url);
                all.extend(events);
            }
            Err(e) => tracing::warn!("ICS fetch failed for {raw_url}: {e}"),
        }
    }
    all
}

/// Resolve the user's home directory (falls back to `/` if `$HOME` is unset).
fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// File browser listing mode.
/// - 0: vault folders only
/// - 1: directories + `.ics` files
/// - 2: directories + image files (png/jpg/jpeg/webp)
fn read_dir_entries(dir: &Path, mode: i32) -> Vec<(String, String, bool, bool)> {
    let mut result: Vec<(String, String, bool, bool)> = Vec::new();

    // Parent entry (skip when we're already at the filesystem root).
    if let Some(parent) = dir.parent() {
        if parent != dir {
            result.push((
                "..".to_string(),
                parent.to_string_lossy().to_string(),
                true,
                false,
            ));
        }
    }

    let mut dirs: Vec<(String, String, bool)> = Vec::new();
    let mut files: Vec<(String, String)> = Vec::new();

    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue; // skip hidden entries
            }
            if path.is_dir() {
                let is_vault = path.join(".obsidian").is_dir();
                dirs.push((name, path.to_string_lossy().to_string(), is_vault));
            } else if mode == 1 && name.to_lowercase().ends_with(".ics") {
                files.push((name, path.to_string_lossy().to_string()));
            } else if mode == 2 {
                let lower = name.to_lowercase();
                if lower.ends_with(".png")
                    || lower.ends_with(".jpg")
                    || lower.ends_with(".jpeg")
                    || lower.ends_with(".webp")
                {
                    files.push((name, path.to_string_lossy().to_string()));
                }
            }
        }
    }

    dirs.sort_by(|a, b| a.0.cmp(&b.0));
    files.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, path, is_vault) in dirs {
        result.push((name, path, true, is_vault));
    }
    for (name, path) in files {
        result.push((name, path, false, false));
    }
    result
}

/// Convert `read_dir_entries` tuples to the `FileBrowserEntry` Slint struct.
fn entries_to_fb(entries: &[(String, String, bool, bool)]) -> Vec<ephemeris_ui::FileBrowserEntry> {
    entries
        .iter()
        .map(
            |(name, full_path, is_dir, is_vault)| ephemeris_ui::FileBrowserEntry {
                name: name.as_str().into(),
                full_path: full_path.as_str().into(),
                is_dir: *is_dir,
                is_vault: *is_vault,
            },
        )
        .collect()
}

/// Apply a theme name directly to a cloned `EphemerisPage` component,
/// bypassing the `EphemerisUi` wrapper (used from settings save callback).
fn apply_theme_to_comp(comp: &ephemeris_ui::EphemerisPage, theme: &str) {
    let t = ephemeris_ui::EinkTheme::get(comp);
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

// ── Agenda helpers ────────────────────────────────────────────────────────────

fn int_to_range(i: i32) -> AgendaRange {
    match i {
        1 => AgendaRange::Week,
        2 => AgendaRange::Month,
        _ => AgendaRange::Day,
    }
}

/// Push filtered agenda entries to the `EphemerisUi` wrapper.
fn push_agenda_ui(ui: &EphemerisUi, events: &[CalendarEvent], range: AgendaRange, now: u64) {
    let filtered = filter_agenda(events, range, now);
    let entries = to_agenda_entries(&filtered, range, now);
    let tuples: Vec<(String, String, String)> = entries
        .iter()
        .map(|e| {
            (
                e.title.clone(),
                e.time_label.clone(),
                e.location_label.clone(),
            )
        })
        .collect();
    ui.set_agenda_list(&tuples);
}

/// Push filtered agenda entries directly to a cloned `EphemerisPage` component.
/// Used from callbacks that already hold a `clone_strong()` of the component.
fn push_agenda_comp(
    comp: &ephemeris_ui::EphemerisPage,
    events: &[CalendarEvent],
    range: AgendaRange,
    now: u64,
) {
    let filtered = filter_agenda(events, range, now);
    let entries = to_agenda_entries(&filtered, range, now);
    comp.set_agenda_list(
        Rc::new(slint::VecModel::from(
            entries
                .iter()
                .map(|e| ephemeris_ui::AgendaEntry {
                    title: e.title.as_str().into(),
                    time_label: e.time_label.as_str().into(),
                    location_label: e.location_label.as_str().into(),
                })
                .collect::<Vec<_>>(),
        ))
        .into(),
    );
}

// ── Task display helpers ──────────────────────────────────────────────────────

/// Max ranked open tasks shown on the home page (keeps agenda visible).
const HOME_TASK_LIMIT: usize = 5;

/// Refresh both the start-page Engage list and the full-task-list in the UI.
fn refresh_ui(
    ui: &EphemerisUi,
    tasks: &[Task],
    profile_id: ProfileId,
    mgr: &ephemeris_core::profile_manager::ProfileManager,
    config: &Config,
) {
    refresh_comp(&ui.clone_component(), tasks, profile_id, mgr, config);
}

/// Same as `refresh_ui` but operates on a cloned `EphemerisPage` component.
fn refresh_comp(
    comp: &ephemeris_ui::EphemerisPage,
    tasks: &[Task],
    profile_id: ProfileId,
    mgr: &ephemeris_core::profile_manager::ProfileManager,
    config: &Config,
) {
    let now = now_secs();

    // In All-profiles mode, home Engage chips filter without switching profile.
    let chip = if comp.get_active_profile_id().is_empty() {
        comp.get_task_profile_filter().to_string()
    } else {
        String::new()
    };
    let chip_scoped;
    let home_tasks: &[Task] = if chip.is_empty() {
        tasks
    } else {
        chip_scoped = filter_tasks_by_profile(tasks, &chip);
        &chip_scoped
    };

    let engage_lane = {
        let from_ui = comp.get_task_engage_lane().to_string();
        if from_ui.is_empty() {
            config.task_engage.engage_lane.clone()
        } else {
            from_ui
        }
    };
    let ctx_filter = comp.get_task_context_filter().to_string();
    // Now + Next: unlaned Obsidian tasks resolve to Next and must still appear.
    let mut filter = EngageFilter::start_page(engage_lane.clone());
    if !ctx_filter.is_empty() {
        filter.context_names.push(ctx_filter);
    }
    let ranked = list_engage(home_tasks, &filter, now);

    let now_count = count_lane(home_tasks, &engage_lane);
    let wip = config
        .task_lanes
        .iter()
        .find(|l| l.id == engage_lane)
        .and_then(|l| l.wip_limit);
    let wip_msg = match wip {
        Some(limit) if now_count as u32 > limit => {
            format!("Now WIP soft limit: {now_count}/{limit}")
        }
        _ => String::new(),
    };
    comp.set_task_wip_warning(wip_msg.as_str().into());
    comp.set_task_engage_lane(engage_lane.as_str().into());

    let show_badges = comp.get_active_profile_id().is_empty();
    let task_entries: Vec<ephemeris_ui::TaskEntry> = ranked
        .iter()
        .take(HOME_TASK_LIMIT)
        .map(|t| {
            let title = strip_inline_markdown(&t.title);
            let (dl, ov) = format_due(t.due, now);
            let (show_profile, profile_icon_char, has_custom_profile_icon, custom_profile_icon) =
                profile_badge_for(&t.profile_id.0.to_string(), show_badges, mgr);
            ephemeris_ui::TaskEntry {
                id: t.id.0.to_string().as_str().into(),
                title: title.as_str().into(),
                priority_label: priority_label(t.priority).as_str().into(),
                due_label: dl.as_str().into(),
                overdue: ov,
                show_profile,
                profile_icon_char: profile_icon_char.as_str().into(),
                has_custom_profile_icon,
                custom_profile_icon,
            }
        })
        .collect();
    comp.set_task_list(Rc::new(slint::VecModel::from(task_entries)).into());
    push_task_lanes_comp(comp, tasks, config);
    push_full_task_list_comp(comp, tasks, mgr);
    let _ = profile_id;
}

fn push_task_lanes_comp(comp: &ephemeris_ui::EphemerisPage, tasks: &[Task], config: &Config) {
    let entries: Vec<ephemeris_ui::TaskLaneEntry> = config
        .task_lanes
        .iter()
        .filter(|l| l.visible)
        .map(|l| ephemeris_ui::TaskLaneEntry {
            id: l.id.as_str().into(),
            label: l.label.as_str().into(),
            count: count_lane(tasks, &l.id) as i32,
        })
        .collect();
    comp.set_task_lane_list(Rc::new(slint::VecModel::from(entries)).into());
}

fn apply_task_dialog_fields(task: &mut Task, lane: &str, contexts: &str, waiting: &str) {
    let lane = if lane.is_empty() { "next" } else { lane };
    set_lane_tag(task, lane);
    task.contexts = parse_tag_list(contexts)
        .into_iter()
        .map(|name| {
            let lower = name.to_ascii_lowercase();
            let kind = match lower.as_str() {
                "phone" | "computer" | "email" | "laptop" | "online" => TaskContextKind::Tool,
                "home" | "office" | "errand" | "anywhere" | "town" | "shop" => {
                    TaskContextKind::Place
                }
                _ => TaskContextKind::Custom,
            };
            TaskContext::new(kind, lower)
        })
        .collect();
    let w = waiting.trim();
    task.waiting_for = if w.is_empty() {
        None
    } else {
        Some(w.to_string())
    };
    if task.waiting_for.is_some() && lane == "next" {
        // Keep explicit waiting lane if user picked Waiting; else infer.
    }
    sync_tags_from_structured(task);
}

fn push_settings_task_lanes(comp: &ephemeris_ui::EphemerisPage, cfg: &Config) {
    let entries: Vec<ephemeris_ui::SettingsLaneEntry> = cfg
        .task_lanes
        .iter()
        .map(|l| ephemeris_ui::SettingsLaneEntry {
            id: l.id.as_str().into(),
            label: l.label.as_str().into(),
            visible: l.visible,
            wip_limit: l.wip_limit.unwrap_or(0) as i32,
        })
        .collect();
    comp.set_settings_task_lanes(Rc::new(slint::VecModel::from(entries)).into());
}

fn apply_settings_task_fields_from_ui(comp: &ephemeris_ui::EphemerisPage, cfg: &mut Config) {
    let model = comp.get_settings_task_lanes();
    for i in 0..model.row_count() {
        if let Some(row) = model.row_data(i) {
            let id = row.id.to_string();
            if let Some(lane) = cfg.task_lanes.iter_mut().find(|l| l.id == id) {
                let label = row.label.to_string();
                if !label.trim().is_empty() {
                    lane.label = label;
                }
                lane.visible = row.visible;
                lane.wip_limit = if row.wip_limit <= 0 {
                    None
                } else {
                    Some(row.wip_limit as u32)
                };
            }
        }
    }
    let days = comp.get_settings_staleness_days();
    if days > 0 {
        cfg.task_engage.staleness_days = days as u32;
    }
    let presets = comp.get_settings_context_presets().to_string();
    cfg.task_engage.context_presets = parse_tag_list(&presets);
    let engage = comp.get_settings_engage_lane().to_string();
    if !engage.is_empty() {
        cfg.task_engage.engage_lane = engage;
    }
    let capture = comp.get_settings_capture_lane().to_string();
    if !capture.is_empty() {
        cfg.task_engage.capture_lane = capture;
    }
    cfg.task_engage.clarify_after_capture = comp.get_settings_clarify_after_capture();
}

fn mutate_settings_lane(
    comp: &ephemeris_ui::EphemerisPage,
    id: &str,
    mut f: impl FnMut(&mut ephemeris_ui::SettingsLaneEntry),
) {
    let model = comp.get_settings_task_lanes();
    let mut rows: Vec<_> = (0..model.row_count())
        .filter_map(|i| model.row_data(i))
        .collect();
    if let Some(row) = rows.iter_mut().find(|r| r.id.as_str() == id) {
        f(row);
    }
    comp.set_settings_task_lanes(Rc::new(slint::VecModel::from(rows)).into());
}

fn show_review_card(
    comp: &ephemeris_ui::EphemerisPage,
    tasks: &[Task],
    queue: &[TaskId],
    index: usize,
    cfg: &Config,
) {
    if queue.is_empty() || index >= queue.len() {
        comp.set_review_visible(false);
        return;
    }
    let id = queue[index];
    let Some(t) = tasks.iter().find(|t| t.id == id) else {
        comp.set_review_visible(false);
        return;
    };
    let now = now_secs();
    let lane = resolve_lane(t, "next");
    let lane_label = cfg
        .task_lanes
        .iter()
        .find(|l| l.id == lane)
        .map(|l| l.label.clone())
        .unwrap_or(lane.clone());
    let stale = is_stale(t, now, cfg.task_engage.staleness_days);
    let mut meta = Vec::new();
    if let Some(ref w) = t.waiting_for {
        meta.push(format!("Waiting: {w}"));
    }
    if !t.contexts.is_empty() {
        meta.push(format!(
            "Ctx: {}",
            t.contexts
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    comp.set_review_task_id(id.0.to_string().as_str().into());
    comp.set_review_title(strip_inline_markdown(&t.title).as_str().into());
    comp.set_review_lane_label(lane_label.as_str().into());
    comp.set_review_meta(meta.join(" · ").as_str().into());
    comp.set_review_stale(stale);
    comp.set_review_progress(format!("{}/{}", index + 1, queue.len()).as_str().into());
    comp.set_review_visible(true);
}

/// Push the task-view list using current UI filter/sort/profile-chip/search.
fn push_full_task_list_comp(
    comp: &ephemeris_ui::EphemerisPage,
    all_tasks: &[Task],
    mgr: &ephemeris_core::profile_manager::ProfileManager,
) {
    let global_profile = comp.get_active_profile_id().to_string();
    let chip = if global_profile.is_empty() {
        comp.get_task_profile_filter().to_string()
    } else {
        String::new()
    };
    let search = comp.get_task_search_query().to_string();
    let filter = comp.get_task_filter().to_string();
    let sort = comp.get_task_sort().to_string();
    let lane_filter = comp.get_task_lane_filter().to_string();
    let now = now_secs();

    let mut scoped = filter_tasks_by_profile(all_tasks, &global_profile);
    if !chip.is_empty() {
        scoped = filter_tasks_by_profile(&scoped, &chip);
    }
    scoped = filter_tasks_by_query(&scoped, &search);
    if !lane_filter.is_empty() {
        scoped.retain(|t| resolve_lane(t, "next") == lane_filter);
    }

    let show_badges = global_profile.is_empty();
    let entries = build_full_task_list(&scoped, now, &filter, &sort);
    comp.set_full_task_list(
        Rc::new(slint::VecModel::from(
            entries
                .iter()
                .map(
                    |(id, title, pl, pi, dl, ov, done, profile_id, lane, has_source)| {
                        let (show_profile, profile_icon_char, has_custom, custom_icon) =
                            profile_badge_for(profile_id, show_badges, mgr);
                        ephemeris_ui::TaskViewEntry {
                            id: id.as_str().into(),
                            title: title.as_str().into(),
                            priority_label: pl.as_str().into(),
                            priority_int: *pi,
                            due_label: dl.as_str().into(),
                            overdue: *ov,
                            done: *done,
                            lane_label: lane.as_str().into(),
                            has_source_note: *has_source,
                            show_profile,
                            profile_icon_char: profile_icon_char.as_str().into(),
                            has_custom_profile_icon: has_custom,
                            custom_profile_icon: custom_icon,
                        }
                    },
                )
                .collect::<Vec<_>>(),
        ))
        .into(),
    );
}

/// Case-insensitive match on title and tags.
fn filter_tasks_by_query(tasks: &[Task], query: &str) -> Vec<Task> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return tasks.to_vec();
    }
    tasks
        .iter()
        .filter(|t| {
            let title = strip_inline_markdown(&t.title).to_lowercase();
            if title.contains(&q) {
                return true;
            }
            t.tags.iter().any(|tag| tag.to_lowercase().contains(&q))
                || t.contexts
                    .iter()
                    .any(|c| c.name.to_lowercase().contains(&q))
                || t.waiting_for
                    .as_deref()
                    .map(|w| w.to_lowercase().contains(&q))
                    .unwrap_or(false)
                || t.project
                    .as_deref()
                    .map(|p| p.to_lowercase().contains(&q))
                    .unwrap_or(false)
                || t.description
                    .as_deref()
                    .map(|d| d.to_lowercase().contains(&q))
                    .unwrap_or(false)
        })
        .cloned()
        .collect()
}

/// Completed tasks older than this are hidden from the "all" filter.
const CLOSED_TASK_RETENTION_SECS: u64 = 14 * 86_400;

/// Slint task-list row: id, title, priority label, priority int, due, overdue, done, profile, lane, has_source_note.
type TaskListRow = (
    String,
    String,
    String,
    i32,
    String,
    bool,
    bool,
    String,
    String,
    bool,
);

/// True when `ts` falls on the same local calendar day as `now`.
fn same_local_day(ts: u64, now: u64) -> bool {
    use chrono::{DateTime, Local, Utc};
    let a = DateTime::<Utc>::from_timestamp(ts as i64, 0).map(|u| u.with_timezone(&Local));
    let b = DateTime::<Utc>::from_timestamp(now as i64, 0).map(|u| u.with_timezone(&Local));
    match (a, b) {
        (Some(a), Some(b)) => a.date_naive() == b.date_naive(),
        _ => false,
    }
}

fn build_full_task_list(tasks: &[Task], now: u64, filter: &str, sort: &str) -> Vec<TaskListRow> {
    let mut v: Vec<_> = tasks
        .iter()
        .filter(|t| match filter {
            // Active keeps today's completions scratched-out (Zeigarnik closure)
            // until local midnight; older done items leave the working list.
            "active" => {
                if !t.done {
                    true
                } else {
                    t.completed_at
                        .map(|ts| same_local_day(ts, now))
                        .unwrap_or(false)
                }
            }
            "done" => t.done,
            // "all": hide long-closed items so the list stays usable
            _ => {
                if !t.done {
                    return true;
                }
                let closed_at = t.completed_at.unwrap_or(0);
                closed_at == 0 || now.saturating_sub(closed_at) <= CLOSED_TASK_RETENTION_SECS
            }
        })
        .map(|t| {
            let pi = priority_to_int(t.priority);
            let pl = priority_label(t.priority);
            let (dl, ov) = format_due(t.due, now);
            let has_source = t.source_ref.as_ref().is_some_and(|r| !r.is_empty());
            (
                t.id.0.to_string(),
                strip_inline_markdown(&t.title),
                pl,
                pi,
                dl,
                ov,
                t.done,
                t.profile_id.0.to_string(),
                resolve_lane(t, "next"),
                has_source,
            )
        })
        .collect();
    match sort {
        "due" => v.sort_by(|a, b| {
            // open before done, then tasks with no due date sort last
            a.6.cmp(&b.6).then({
                let due_a = if a.4.is_empty() { u64::MAX } else { 0 };
                let due_b = if b.4.is_empty() { u64::MAX } else { 0 };
                due_a.cmp(&due_b).then(a.5.cmp(&b.5))
            })
        }),
        "title" => v.sort_by(|a, b| {
            a.6.cmp(&b.6)
                .then(a.1.to_lowercase().cmp(&b.1.to_lowercase()))
        }),
        "added" => {
            // preserve original order but keep open tasks above today's done
            v.sort_by_key(|a| a.6)
        }
        _ => {
            // priority, with open tasks before closed when mixed
            v.sort_by(|a, b| a.6.cmp(&b.6).then(a.3.cmp(&b.3)))
        }
    }
    v
}

fn priority_label(p: TaskPriority) -> String {
    match p {
        TaskPriority::Urgent => "U".to_string(),
        TaskPriority::High => "H".to_string(),
        TaskPriority::Medium => "M".to_string(),
        TaskPriority::Low => "L".to_string(),
    }
}

fn priority_from_int(p: i32) -> TaskPriority {
    match p {
        0 => TaskPriority::Urgent,
        1 => TaskPriority::High,
        3 => TaskPriority::Low,
        _ => TaskPriority::Medium,
    }
}

fn priority_to_int(p: TaskPriority) -> i32 {
    match p {
        TaskPriority::Urgent => 0,
        TaskPriority::High => 1,
        TaskPriority::Medium => 2,
        TaskPriority::Low => 3,
    }
}

/// Next auto-title for a new note (`Config.note_title_pattern`, `{n}` = counter).
fn next_note_title(config: &Config, n: u32) -> String {
    format_note_title(&config.note_title_pattern, n, Local::now().date_naive())
}

/// Normalize a freeform note tag for Obsidian (`#` stripped, spaces → `-`).
fn normalize_note_tag(raw: &str) -> Option<String> {
    let t = raw
        .trim()
        .trim_start_matches('#')
        .replace(' ', "-")
        .trim_matches(|c: char| c == ',' || c.is_whitespace())
        .to_string();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

/// Parse space/comma-separated note tags into a deduped list.
fn parse_note_tag_list(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in raw.split(|c: char| c.is_whitespace() || c == ',') {
        if let Some(tag) = normalize_note_tag(part) {
            if !out.iter().any(|t: &String| t.eq_ignore_ascii_case(&tag)) {
                out.push(tag);
            }
        }
    }
    out
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn now_ms() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u32
}

/// Ink-engine timestamp (ms) for a pointer sample.
///
/// `t_us == 0` (mouse / touch / synthetic) falls back to [`now_ms`]. Otherwise
/// the kernel timestamp is mapped onto the `now_ms` clock via a process-static
/// epoch pinned at the first stamped sample, so real pen inter-sample spacing
/// is preserved (burst-delivered evdev packets no longer collapse onto one
/// drain-time `now_ms()`).
fn t_ms_from_us(t_us: u64) -> u32 {
    if t_us == 0 {
        return now_ms();
    }
    static EPOCH: OnceLock<(u64, u32)> = OnceLock::new();
    let (epoch_us, epoch_ms) = *EPOCH.get_or_init(|| (t_us, now_ms()));
    epoch_ms.wrapping_add((t_us.saturating_sub(epoch_us) / 1000) as u32)
}

fn sample_tasks_split(work: ProfileId, personal: ProfileId) -> Vec<Task> {
    const DAY: u64 = 86_400;
    let now = now_secs();
    let mut tasks = Vec::new();

    let mut t = Task::new("Review quarterly OKRs", work);
    t.priority = TaskPriority::High;
    t.due = Some(now + DAY);
    set_lane_tag(&mut t, "now");
    t.contexts
        .push(TaskContext::new(TaskContextKind::Tool, "computer"));
    tasks.push(t);

    let mut t = Task::new("Fix login redirect bug", work);
    t.priority = TaskPriority::Urgent;
    t.due = Some(now.saturating_sub(DAY));
    set_lane_tag(&mut t, "now");
    t.contexts
        .push(TaskContext::new(TaskContextKind::Tool, "computer"));
    tasks.push(t);

    let mut t = Task::new("Write architecture doc", work);
    t.priority = TaskPriority::Medium;
    t.due = Some(now + 3 * DAY);
    set_lane_tag(&mut t, "next");
    tasks.push(t);

    let mut t = Task::new("Book dentist appointment", personal);
    t.priority = TaskPriority::Medium;
    set_lane_tag(&mut t, "now");
    t.contexts
        .push(TaskContext::new(TaskContextKind::Tool, "phone"));
    tasks.push(t);

    let mut t = Task::new("Plan weekend trip", personal);
    t.priority = TaskPriority::Low;
    set_lane_tag(&mut t, "later");
    t.due = Some(now + 7 * DAY);
    tasks.push(t);

    tasks
}

fn filter_tasks_by_profile(tasks: &[Task], profile_id: &str) -> Vec<Task> {
    if profile_id.is_empty() {
        tasks.to_vec()
    } else {
        tasks
            .iter()
            .filter(|t| t.profile_id.0.to_string() == profile_id)
            .cloned()
            .collect()
    }
}

fn filter_events_by_profile(events: &[CalendarEvent], profile_id: &str) -> Vec<CalendarEvent> {
    if profile_id.is_empty() {
        events.to_vec()
    } else {
        events
            .iter()
            .filter(|e| e.profile_id.0.to_string() == profile_id)
            .cloned()
            .collect()
    }
}

/// Map a profile icon key to its Flutter Material Icons Unicode character.
///
/// The bundled font uses Flutter's cmap (not Google's raw Material Icons codes).
fn profile_icon_char(icon: &str) -> &'static str {
    if icon.starts_with("custom:") {
        return "\u{E491}"; // person fallback; UI shows the image instead
    }
    match icon {
        "work" => "\u{E11C}",              // business_center
        "home" => "\u{E318}",              // home
        "private" => "\u{E3AE}",           // lock
        "person" => "\u{E491}",            // person
        "kids" => "\u{E160}",              // child_care
        "family" => "\u{E257}",            // family_restroom
        "party" => "\u{E149}",             // celebration
        "pets" => "\u{E4A1}",              // pets
        "fitness" => "\u{E28D}",           // fitness_center
        "run" => "\u{E1DC}",               // directions_run (legacy)
        "music" => "\u{E415}",             // music_note
        "cafe" => "\u{E38D}",              // local_cafe
        "food" => "\u{E390}",              // local_restaurant
        "travel" | "flight" => "\u{E297}", // flight
        "beach" => "\u{E0D6}",             // beach_access
        "creative" => "\u{E46B}",          // palette
        "code" => "\u{E176}",              // code
        "night" => "\u{E42E}",             // nightlight
        "group" => "\u{E2EB}",             // group
        "school" => "\u{E3DD}",            // menu_book
        "heart" => "\u{E25B}",             // favorite
        _ => "\u{E491}",                   // person
    }
}

fn load_image_file(path: &std::path::Path) -> Option<slint::Image> {
    let dyn_img = image::open(path).ok()?;
    let rgba = dyn_img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let buffer =
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba.as_raw(), w, h);
    Some(slint::Image::from_rgba8(buffer))
}

fn load_custom_profile_icon(icon: &str) -> (bool, slint::Image) {
    if let Some(path) = icon.strip_prefix("custom:") {
        match load_image_file(std::path::Path::new(path)) {
            Some(img) => (true, img),
            None => (false, slint::Image::default()),
        }
    } else {
        (false, slint::Image::default())
    }
}

/// Resolve the icon char and display name for the active profile and set them on the UI component.
fn set_active_profile_icon(
    ui: &ephemeris_ui::EphemerisPage,
    mgr: &ephemeris_core::profile_manager::ProfileManager,
    profile_id_str: &str,
) {
    if profile_id_str.is_empty() {
        ui.set_active_profile_icon_char("\u{E2EB}".into()); // group for "All"
        ui.set_active_profile_has_custom(false);
        ui.set_active_profile_custom_icon(slint::Image::default());
        ui.set_active_profile_name("".into());
    } else {
        let profiles = mgr.list_profiles().unwrap_or_default();
        if let Some(p) = profiles
            .iter()
            .find(|p| p.id.0.to_string() == profile_id_str)
        {
            let (has_custom, img) = load_custom_profile_icon(&p.icon);
            ui.set_active_profile_icon_char(profile_icon_char(&p.icon).into());
            ui.set_active_profile_has_custom(has_custom);
            ui.set_active_profile_custom_icon(img);
            ui.set_active_profile_name(p.name.clone().into());
        } else {
            ui.set_active_profile_icon_char("\u{E491}".into());
            ui.set_active_profile_has_custom(false);
            ui.set_active_profile_custom_icon(slint::Image::default());
            ui.set_active_profile_name("".into());
        }
    }
}

/// Get the resolved profile name for a given profile_id string.
fn resolve_profile_name(
    mgr: &ephemeris_core::profile_manager::ProfileManager,
    profile_id_str: &str,
) -> String {
    if profile_id_str.is_empty() {
        return String::new();
    }
    mgr.list_profiles()
        .unwrap_or_default()
        .iter()
        .find(|p| p.id.0.to_string() == profile_id_str)
        .map(|p| p.name.clone())
        .unwrap_or_default()
}

/// Compute the next ID for a connection list.
fn next_conn_id(conns: &[AppConnection]) -> u32 {
    conns.iter().map(|c| c.id).max().unwrap_or(0) + 1
}

/// Format a `due` Unix timestamp relative to `now_secs`.
fn format_due(due: Option<u64>, now_secs: u64) -> (String, bool) {
    const DAY: u64 = 86_400;
    match due {
        None => (String::new(), false),
        Some(d) => {
            if d <= now_secs {
                ("overdue".to_string(), true)
            } else {
                let days = (d - now_secs).div_ceil(DAY);
                if days == 0 {
                    ("today".to_string(), false)
                } else if days == 1 {
                    ("tmrw".to_string(), false)
                } else {
                    (format!("{}d", days), false)
                }
            }
        }
    }
}

/// Format due as `YYYY-MM-DD` for the edit dialog (empty if unset).
fn format_due_ymd(due: Option<u64>) -> String {
    match due {
        Some(ts) => DateTime::<Utc>::from_timestamp(ts as i64, 0)
            .map(|dt| dt.format("%Y-%m-%d").to_string())
            .unwrap_or_default(),
        None => String::new(),
    }
}

/// Parse `YYYY-MM-DD` (or empty) into a Unix timestamp at UTC midnight.
fn parse_due_ymd(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let date = NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
    let dt = date.and_hms_opt(0, 0, 0)?;
    Some(dt.and_utc().timestamp() as u64)
}

/// Split a space/comma-separated tag string; strips leading `#`.
fn parse_tag_list(s: &str) -> Vec<String> {
    s.split(|c: char| c.is_whitespace() || c == ',')
        .map(|t| t.trim().trim_start_matches('#'))
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_creation() {
        let app = App::new().expect("should create app");
        assert!(!app.version.is_empty());
    }

    #[test]
    fn app_run() {
        let app = App::new().expect("should create app");
        app.run().expect("should run without error");
    }

    #[test]
    fn app_receives_ready_event() {
        let app = App::new().expect("should create app");
        let evt = app.try_recv_event();
        assert_eq!(evt, Some(Event::Ready));
    }

    #[test]
    fn app_ping_pong_communication() {
        let app = App::new().expect("should create app");
        let _ = app.try_recv_event();
        app.send_command(Command::Ping).expect("should send ping");
        std::thread::sleep(std::time::Duration::from_millis(100));
        let evt = app.try_recv_event();
        assert_eq!(evt, Some(Event::Pong));
    }

    #[test]
    fn load_vault_tasks_missing_dir_returns_empty() {
        let profile = ProfileId::new();
        // Non-existent path returns Ok(empty) because the recursive walker
        // skips directories that don't exist.
        let result = load_vault_tasks_with_inbox(
            std::path::Path::new("/tmp/no-such-vault-xyz"),
            std::path::Path::new(ephemeris_core::DEFAULT_TASKS_INBOX),
            std::path::Path::new(ephemeris_core::DEFAULT_EXPORT_SUBDIR),
            profile,
        );
        // Either Ok(empty) or Err — both are acceptable; just must not panic.
        if let Ok(tasks) = result {
            assert!(tasks.is_empty());
        }
    }

    #[test]
    fn load_ics_events_missing_file_returns_empty() {
        let profile = ProfileId::new();
        let paths = vec![std::path::PathBuf::from("/tmp/no-such-calendar.ics")];
        let events = load_ics_events(&paths, profile, now_secs());
        assert!(events.is_empty(), "missing ICS file must yield empty list");
    }

    #[test]
    fn load_ics_events_parses_valid_file() {
        use std::io::Write;
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("test.ics");
        // Event at 1970-01-01T12:00:00Z = Unix 43200.
        // Pass now=0 so window = [0, 90*86400]; 43200 is well inside it.
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
                   BEGIN:VEVENT\r\nUID:evt1\r\nSUMMARY:Test Event\r\n\
                   DTSTART:19700101T120000Z\r\nDTEND:19700101T130000Z\r\n\
                   END:VEVENT\r\nEND:VCALENDAR\r\n";
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(ics.as_bytes()).unwrap();

        let profile = ProfileId::new();
        let events = load_ics_events(&[path], profile, 0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].title, "Test Event");
    }

    #[test]
    fn int_to_range_mapping() {
        assert_eq!(int_to_range(0), AgendaRange::Day);
        assert_eq!(int_to_range(1), AgendaRange::Week);
        assert_eq!(int_to_range(2), AgendaRange::Month);
        assert_eq!(int_to_range(99), AgendaRange::Day); // unknown → Day
    }

    #[test]
    fn new_task_auto_numbering() {
        let counter = Rc::new(Cell::new(1u32));
        let make_title = |raw: &str| -> String {
            let t = raw.trim().to_string();
            if t.is_empty() {
                let n = counter.get();
                counter.set(n + 1);
                format!("New Task {n}")
            } else {
                t
            }
        };
        assert_eq!(make_title(""), "New Task 1");
        assert_eq!(make_title("   "), "New Task 2");
        assert_eq!(make_title("My custom task"), "My custom task");
        assert_eq!(make_title("New Task"), "New Task");
    }

    #[test]
    fn parse_due_and_tags_for_dialog() {
        assert_eq!(parse_due_ymd(""), None);
        assert_eq!(parse_due_ymd("2025-06-01"), Some(1_748_736_000));
        assert_eq!(format_due_ymd(Some(1_748_736_000)), "2025-06-01");
        assert_eq!(
            parse_tag_list("#work home, errand"),
            vec!["work", "home", "errand"]
        );
    }

    #[test]
    fn filter_tasks_by_query_matches_title_and_tags() {
        let pid = ProfileId::new();
        let mut a = Task::new("Buy **milk**", pid);
        a.tags = vec!["grocery".into()];
        let mut b = Task::new("Write docs", pid);
        b.tags = vec!["work".into()];
        let tasks = vec![a, b];
        let hits = filter_tasks_by_query(&tasks, "milk");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].title.contains("milk"));
        let by_tag = filter_tasks_by_query(&tasks, "work");
        assert_eq!(by_tag.len(), 1);
        assert_eq!(by_tag[0].title, "Write docs");
        assert_eq!(filter_tasks_by_query(&tasks, "").len(), 2);
    }

    #[test]
    fn active_filter_keeps_completed_today_scratched() {
        use chrono::{Duration, Local, TimeZone};
        let pid = ProfileId::new();
        let today = Local::now().date_naive();
        let noon_today = today.and_hms_opt(12, 0, 0).expect("noon");
        let noon_yesterday = (today - Duration::days(1))
            .and_hms_opt(12, 0, 0)
            .expect("noon");
        let now = Local
            .from_local_datetime(&noon_today)
            .single()
            .expect("local")
            .timestamp() as u64;
        let yesterday_ts = Local
            .from_local_datetime(&noon_yesterday)
            .single()
            .expect("local")
            .timestamp() as u64;

        let open = Task::new("Still open", pid);
        let mut done_today = Task::new("Done today", pid);
        done_today.set_done(true);
        done_today.completed_at = Some(now - 3600);
        let mut done_yesterday = Task::new("Done yesterday", pid);
        done_yesterday.set_done(true);
        done_yesterday.completed_at = Some(yesterday_ts);

        let tasks = vec![open, done_today, done_yesterday];
        let rows = build_full_task_list(&tasks, now, "active", "priority");
        let titles: Vec<_> = rows.iter().map(|r| r.1.as_str()).collect();
        assert!(titles.contains(&"Still open"));
        assert!(titles.contains(&"Done today"));
        assert!(!titles.contains(&"Done yesterday"));
        // Open tasks sort before scratched-out done
        assert_eq!(rows[0].1, "Still open");
        assert!(rows.iter().any(|r| r.1 == "Done today" && r.6));
    }

    #[test]
    fn page_bin_round_trips_physical_size_and_legacy_logical_loads() {
        let dir = std::env::temp_dir().join(format!("eph-ktg4-bin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (pw, ph) = ephemeris_pal::display::PINENOTE_PX;
        let phys = vec![7u8; (pw * ph) as usize];
        save_note_page_pixels(&dir, 9, 0, pw, ph, &phys);
        let (w, h, data) = load_note_page_n(&dir, 9, 0).expect("physical bin loads");
        assert_eq!((w, h), (pw, ph));
        assert_eq!(data, phys);

        // Pre-ktg.4 logical-size bin (EPH1 header) still loads with its own size.
        save_note_page_pixels(&dir, 9, 1, 800, 1067, &vec![255u8; 800 * 1067]);
        let (w, h, _) = load_note_page_n(&dir, 9, 1).expect("logical bin loads");
        assert_eq!((w, h), (800, 1067));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transcription_runs_at_logical_size_and_spans_scale_to_pdf_page() {
        let (pw, ph) = ephemeris_pal::display::PINENOTE_PX;
        let raster = ephemeris_pal::display::PixelBuf::new(pw, ph);
        let input = transcription_raster(&raster, (800, 1067));
        assert_eq!((input.width, input.height), (800, 1067));

        let spans = vec![ephemeris_core::TextSpan::new(
            "ago", 100.0, 200.0, 40.0, 20.0,
        )];
        let scaled = scale_spans(spans, (800, 1067), (pw, ph));
        assert!((scaled[0].x - 175.5).abs() < 0.01);
        assert!((scaled[0].width - 70.2).abs() < 0.01);
        assert!((scaled[0].y - 200.0 * ph as f32 / 1067.0).abs() < 0.01);
    }

    #[test]
    fn transcript_preview_collapses_and_ellipsizes() {
        assert_eq!(transcript_preview("  hello   world  "), "hello world");
        let long = "word ".repeat(40);
        let preview = transcript_preview(&long);
        assert!(preview.ends_with('…'));
        assert!(preview.chars().count() <= 110);
    }

    #[test]
    fn filter_and_sort_recordings() {
        let a = AppRecording {
            id: 1,
            title: "Alpha meeting".into(),
            duration_secs: 30,
            created_at: 100,
            audio_filename: "a.wav".into(),
            transcription: Some("talk about milk".into()),
            profile_id: String::new(),
        };
        let b = AppRecording {
            id: 2,
            title: "Beta notes".into(),
            duration_secs: 90,
            created_at: 200,
            audio_filename: "b.wav".into(),
            transcription: None,
            profile_id: String::new(),
        };
        let all = vec![a, b];
        let milk = filter_recordings_by_query(&all, "milk");
        assert_eq!(milk.len(), 1);
        assert_eq!(milk[0].id, 1);

        let mut by_date = all.clone();
        sort_recordings(&mut by_date, "date");
        assert_eq!(by_date[0].id, 2);

        let mut by_len = all.clone();
        sort_recordings(&mut by_len, "duration");
        assert_eq!(by_len[0].id, 2);
    }
}
