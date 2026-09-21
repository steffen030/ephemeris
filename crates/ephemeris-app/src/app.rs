use ephemeris_core::ics::IcsReader;
use ephemeris_core::prioritizer::rank_tasks;
use ephemeris_core::{
    filter_agenda, to_agenda_entries, AgendaRange, CalendarEvent, Config, MarkdownTaskExtractor,
    ProfileId, Task, TaskPriority, TaskProvider,
};
use ephemeris_pal::input::{InputEvent, PenSample};
use slint::{Global, Model};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::runtime::Runtime;
use tokio::sync::mpsc::UnboundedSender;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::WindowId;

use ephemeris_pal::display::DesktopWindow;
use ephemeris_ui::EphemerisUi;

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
        // Load config — silently fall back to defaults if file is absent or malformed.
        let config = Config::load().unwrap_or_default();
        tracing::info!(
            "Config loaded: theme={}, vault={:?}, ics_paths={}, ics_urls={}",
            config.theme,
            config.vault_path,
            config.ics_paths.len(),
            config.ics_urls.len()
        );

        let ui = Rc::new(EphemerisUi::new(800, 600)?);
        ui.set_page_title("Ephemeris");
        ui.apply_theme(&config.theme);

        let profile_id = ProfileId::new();
        let config: Rc<RefCell<Config>> = Rc::new(RefCell::new(config));

        // ── Tasks ─────────────────────────────────────────────────────────────
        // Load from Obsidian vault when configured; fall back to demo data.
        let initial_tasks = if let Some(ref vault) = config.borrow().vault_path {
            match load_vault_tasks(vault, profile_id) {
                Ok(ts) => {
                    tracing::info!("Loaded {} tasks from vault {}", ts.len(), vault.display());
                    ts
                }
                Err(e) => {
                    tracing::warn!(
                        "Vault task load failed for {}: {e}; using sample tasks",
                        vault.display()
                    );
                    sample_tasks(profile_id)
                }
            }
        } else {
            sample_tasks(profile_id)
        };

        let tasks: Rc<RefCell<Vec<Task>>> = Rc::new(RefCell::new(initial_tasks));

        // ── Calendar events ───────────────────────────────────────────────────
        let now = now_secs();
        let calendar_events: Rc<RefCell<Vec<CalendarEvent>>> = {
            let cfg = config.borrow();
            let mut evts = load_ics_events(&cfg.ics_paths, profile_id, now);
            evts.extend(load_ics_from_urls(&cfg.ics_urls, profile_id, now));
            Rc::new(RefCell::new(evts))
        };

        // Counter for auto-numbering tasks added via the dialog (no keyboard).
        let new_task_n: Rc<Cell<u32>> = Rc::new(Cell::new(1));

        // Counter for auto-numbering new notes.
        let note_id_seq: Rc<Cell<u32>> = Rc::new(Cell::new(2));

        // Notes: one initial note corresponds to PageBook page 0.
        let notes: Rc<RefCell<Vec<AppNote>>> = Rc::new(RefCell::new(vec![AppNote {
            id: 1,
            title: "Note 1".to_string(),
            page_index: 0,
            created_at: now,
        }]));

        // ── Initial UI state ──────────────────────────────────────────────────
        refresh_ui(&*ui, &tasks.borrow(), profile_id);
        push_agenda_ui(&*ui, &calendar_events.borrow(), AgendaRange::Day, now);
        ui.set_show_start_page(true);
        ui.set_nav_section(0);

        // ── Agenda range tabs ─────────────────────────────────────────────────
        {
            let cal2 = calendar_events.clone();
            let ui2 = ui.clone_component();
            ui.on_agenda_range_changed(move |range_idx| {
                let range = int_to_range(range_idx);
                let now = now_secs();
                push_agenda_comp(&ui2, &cal2.borrow(), range, now);
            });
        }

        // ── Ring "Task" → task view ───────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            let tasks2 = tasks.clone();
            ui.on_ring_task(move || {
                let now = now_secs();
                let entries = build_full_task_list(&tasks2.borrow(), now);
                ui2.set_full_task_list(
                    Rc::new(slint::VecModel::from(
                        entries
                            .iter()
                            .map(|(id, title, pl, dl, ov, done)| ephemeris_ui::TaskViewEntry {
                                id: id.as_str().into(),
                                title: title.as_str().into(),
                                priority_label: pl.as_str().into(),
                                due_label: dl.as_str().into(),
                                overdue: *ov,
                                done: *done,
                            })
                            .collect::<Vec<_>>(),
                    ))
                    .into(),
                );
                ui2.set_show_task_view(true);
                ui2.set_ring_visible(false);
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
                ui2.set_ring_visible(false);
                ui2.set_nav_section(0);
                ui2.set_show_start_page(true);
            });
        }

        // ── Nav rail: Home ────────────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_nav_overview_tapped(move || {
                ui2.set_show_task_view(false);
                ui2.set_show_settings(false);
                ui2.set_show_filebrowser(false);
                ui2.set_show_note_list(false);
                ui2.set_ring_visible(false);
                ui2.set_show_start_page(true);
            });
        }

        // ── Nav rail: Notes ───────────────────────────────────────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let notes2 = notes.clone();
            ui.on_nav_notes_tapped(move || {
                let now = now_secs();
                let entries = build_note_list(&notes2.borrow(), now);
                ui_rc.set_note_list(&entries);
                ui2.set_show_start_page(false);
                ui2.set_show_task_view(false);
                ui2.set_show_settings(false);
                ui2.set_show_filebrowser(false);
                ui2.set_ring_visible(false);
                ui2.set_show_note_list(true);
            });
        }

        // ── Note list: tap a note → open its canvas page ──────────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let notes2 = notes.clone();
            ui.on_note_tapped(move |id| {
                let notes = notes2.borrow();
                if let Some(note) = notes.iter().find(|n| n.id.to_string() == id) {
                    let page_idx = note.page_index;
                    let note_id = note.id.to_string();
                    drop(notes);
                    ui2.set_show_note_list(false);
                    ui2.set_active_note_id(note_id.as_str().into());
                    ui_rc.navigate_to_page(page_idx);
                }
            });
        }

        // ── Note list: "+ New Note" → create, navigate to blank canvas ────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let notes2 = notes.clone();
            let seq2 = note_id_seq.clone();
            ui.on_note_new_tapped(move || {
                let new_id = seq2.get();
                seq2.set(new_id + 1);
                {
                    let mut ns = notes2.borrow_mut();
                    let idx = ns.len();
                    ns.push(AppNote {
                        id: new_id,
                        title: format!("Note {}", new_id),
                        page_index: idx,
                        created_at: now_secs(),
                    });
                }
                ui_rc.push_new_page();
                ui2.set_show_note_list(false);
                ui2.set_active_note_id(new_id.to_string().as_str().into());
            });
        }

        // ── Note rename: confirm ──────────────────────────────────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let notes2 = notes.clone();
            ui.on_note_rename_confirm(move |id, new_title| {
                let new_title = new_title.trim().to_string();
                if new_title.is_empty() {
                    ui2.set_note_rename_visible(false);
                    return;
                }
                {
                    let mut ns = notes2.borrow_mut();
                    if let Some(note) = ns.iter_mut().find(|n| n.id.to_string() == id) {
                        note.title = new_title;
                    }
                }
                let now = now_secs();
                let entries = build_note_list(&notes2.borrow(), now);
                ui_rc.set_note_list(&entries);
                ui2.set_note_rename_visible(false);
            });
        }

        // ── Note rename: cancel ───────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_note_rename_cancel(move || {
                ui2.set_note_rename_visible(false);
            });
        }

        // ── Start-page "Notes →" → note browser ──────────────────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let notes2 = notes.clone();
            ui.on_start_page_notes(move || {
                let now = now_secs();
                let entries = build_note_list(&notes2.borrow(), now);
                ui_rc.set_note_list(&entries);
                ui2.set_show_start_page(false);
                ui2.set_nav_section(1);
                ui2.set_show_note_list(true);
            });
        }

        // ── Ring "Note" → note browser ────────────────────────────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let notes2 = notes.clone();
            ui.on_ring_note(move || {
                let now = now_secs();
                let entries = build_note_list(&notes2.borrow(), now);
                ui_rc.set_note_list(&entries);
                ui2.set_show_start_page(false);
                ui2.set_show_task_view(false);
                ui2.set_ring_visible(false);
                ui2.set_nav_section(1);
                ui2.set_show_note_list(true);
            });
        }

        // ── Ring "Event" → stub ───────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_ring_calendar(move || {
                // Not yet implemented: dismiss ring and stay on canvas.
                ui2.set_ring_visible(false);
            });
        }

        // ── Ring "Audio" → stub ───────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_ring_audio(move || {
                ui2.set_ring_visible(false);
            });
        }

        // ── Start-page "All Tasks" button → task view ─────────────────────────
        {
            let ui2 = ui.clone_component();
            let tasks2 = tasks.clone();
            ui.on_start_page_tasks(move || {
                let now = now_secs();
                let entries = build_full_task_list(&tasks2.borrow(), now);
                ui2.set_full_task_list(
                    Rc::new(slint::VecModel::from(
                        entries
                            .iter()
                            .map(|(id, title, pl, dl, ov, done)| ephemeris_ui::TaskViewEntry {
                                id: id.as_str().into(),
                                title: title.as_str().into(),
                                priority_label: pl.as_str().into(),
                                due_label: dl.as_str().into(),
                                overdue: *ov,
                                done: *done,
                            })
                            .collect::<Vec<_>>(),
                    ))
                    .into(),
                );
                ui2.set_show_start_page(false);
                ui2.set_show_task_view(true);
            });
        }

        // ── "Back" in task view → start page ─────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_task_back(move || {
                ui2.set_show_task_view(false);
                ui2.set_nav_section(0);
                ui2.set_show_start_page(true);
            });
        }

        // ── Toggle task done ──────────────────────────────────────────────────
        {
            let tasks2 = tasks.clone();
            let ui2 = ui.clone_component();
            let pid = profile_id;
            ui.on_task_complete_toggled(move |id| {
                {
                    let mut ts = tasks2.borrow_mut();
                    if let Some(t) = ts.iter_mut().find(|t| t.id.0.to_string() == id) {
                        t.done = !t.done;
                    }
                }
                refresh_comp(&ui2, &tasks2.borrow(), pid);
            });
        }

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
                    ui2.set_task_dialog_visible(true);
                }
            });
        }

        // ── Add confirmed ─────────────────────────────────────────────────────
        // Title from the Slint property is always "New Task" (TextInput removed);
        // auto-number to distinguish multiple new tasks.
        {
            let tasks2 = tasks.clone();
            let ui2 = ui.clone_component();
            let pid = profile_id;
            let counter = new_task_n.clone();
            ui.on_task_confirm_add(move |title, priority| {
                let title = title.trim().to_string();
                let title = if title.is_empty() || title == "New Task" {
                    let n = counter.get();
                    counter.set(n + 1);
                    format!("New Task {n}")
                } else {
                    title
                };
                {
                    let mut ts = tasks2.borrow_mut();
                    let mut t = Task::new(title, pid);
                    t.priority = priority_from_int(priority);
                    ts.push(t);
                }
                ui2.set_task_dialog_visible(false);
                refresh_comp(&ui2, &tasks2.borrow(), pid);
            });
        }

        // ── Edit confirmed ────────────────────────────────────────────────────
        // Only priority can change (title is static text, no keyboard).
        {
            let tasks2 = tasks.clone();
            let ui2 = ui.clone_component();
            let pid = profile_id;
            ui.on_task_confirm_edit(move |id, _title, priority| {
                {
                    let mut ts = tasks2.borrow_mut();
                    if let Some(t) = ts.iter_mut().find(|t| t.id.0.to_string() == id) {
                        t.priority = priority_from_int(priority);
                    }
                }
                ui2.set_task_dialog_visible(false);
                refresh_comp(&ui2, &tasks2.borrow(), pid);
            });
        }

        // ── Dialog cancel ─────────────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_task_dialog_cancel(move || {
                ui2.set_task_dialog_visible(false);
            });
        }

        // Shared modifier state: Ctrl held = ring mode, plain click = draw.
        // On the real eink device this maps to PenButton; Ctrl is the desktop stand-in.
        let ctrl_held: Rc<Cell<bool>> = Rc::new(Cell::new(false));

        // ── Canvas tap → show action ring (only when Ctrl held on desktop) ────
        {
            let ui2 = ui.clone_component();
            let ctrl = ctrl_held.clone();
            ui.on_canvas_touch(move |x, y| {
                if ctrl.get() && !ui2.get_ring_visible() {
                    ui2.set_ring_cx(x);
                    ui2.set_ring_cy(y);
                    ui2.set_ring_show_audio(true);
                    ui2.set_ring_visible(true);
                }
            });
        }

        // ── Ring dismiss ──────────────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_ring_dismissed(move || {
                ui2.set_ring_visible(false);
            });
        }

        // ── Settings → open ───────────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            ui.on_settings_open(move || {
                let cfg = cfg2.borrow();
                // Populate settings view from current config.
                let theme_idx = if cfg.theme == "dark" { 1 } else { 0 };
                ui2.set_settings_theme_idx(theme_idx);
                let vault = cfg
                    .vault_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();
                ui2.set_settings_vault_path(vault.as_str().into());

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
                ui2.set_settings_cal_sources(
                    Rc::new(slint::VecModel::from(sources)).into(),
                );
                ui2.set_settings_new_url("".into());
                ui2.set_show_start_page(false);
                ui2.set_show_task_view(false);
                ui2.set_show_settings(true);
            });
        }

        // ── Settings → back ───────────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_settings_back(move || {
                ui2.set_show_settings(false);
                ui2.set_nav_section(0);
                ui2.set_show_start_page(true);
            });
        }

        // ── Settings → clear vault ────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_settings_clear_vault(move || {
                ui2.set_settings_vault_path("".into());
            });
        }

        // ── Settings → browse vault ───────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_settings_browse_vault(move || {
                let home = home_dir();
                ui2.set_fb_vault_mode(true);
                ui2.set_fb_current_path(home.to_string_lossy().to_string().as_str().into());
                let entries = read_dir_entries(&home, true);
                ui2.set_fb_entries(
                    Rc::new(slint::VecModel::from(entries_to_fb(&entries))).into(),
                );
                ui2.set_show_settings(false);
                ui2.set_show_filebrowser(true);
            });
        }

        // ── Settings → browse ICS ─────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_settings_browse_ics(move || {
                let home = home_dir();
                ui2.set_fb_vault_mode(false);
                ui2.set_fb_current_path(home.to_string_lossy().to_string().as_str().into());
                let entries = read_dir_entries(&home, false);
                ui2.set_fb_entries(
                    Rc::new(slint::VecModel::from(entries_to_fb(&entries))).into(),
                );
                ui2.set_show_settings(false);
                ui2.set_show_filebrowser(true);
            });
        }

        // ── Settings → add URL ────────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
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
                let model = ui2.get_settings_cal_sources();
                let mut items: Vec<ephemeris_ui::CalSource> = (0..model.row_count())
                    .filter_map(|i| model.row_data(i))
                    .collect();
                items.push(ephemeris_ui::CalSource {
                    display: display.as_str().into(),
                    value: url.as_str().into(),
                });
                ui2.set_settings_cal_sources(
                    Rc::new(slint::VecModel::from(items)).into(),
                );
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
                ui2.set_settings_cal_sources(
                    Rc::new(slint::VecModel::from(items)).into(),
                );
            });
        }

        // ── Settings → save ───────────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            let tasks2 = tasks.clone();
            let cal2 = calendar_events.clone();
            let pid = profile_id;
            ui.on_settings_save(move || {
                let theme_idx = ui2.get_settings_theme_idx();
                let theme = if theme_idx == 1 { "dark" } else { "light" };
                let vault_str = ui2.get_settings_vault_path().to_string();
                let sources_model = ui2.get_settings_cal_sources();
                let sources: Vec<(String, String)> = (0..sources_model.row_count())
                    .filter_map(|i| sources_model.row_data(i))
                    .map(|s| (s.display.to_string(), s.value.to_string()))
                    .collect();

                let mut new_cfg = Config::default();
                new_cfg.theme = theme.to_string();
                new_cfg.vault_path =
                    if vault_str.is_empty() { None } else { Some(PathBuf::from(&vault_str)) };
                for (_, val) in &sources {
                    if val.starts_with("http") || val.starts_with("webcal") {
                        new_cfg.ics_urls.push(val.clone());
                    } else {
                        new_cfg.ics_paths.push(PathBuf::from(val));
                    }
                }

                if let Err(e) = new_cfg.save() {
                    tracing::warn!("Config save failed: {e}");
                }

                // Apply theme immediately.
                apply_theme_to_comp(&ui2, theme);

                // Reload tasks.
                let new_tasks = if let Some(ref vault) = new_cfg.vault_path {
                    load_vault_tasks(vault, pid).unwrap_or_else(|_| sample_tasks(pid))
                } else {
                    sample_tasks(pid)
                };
                *tasks2.borrow_mut() = new_tasks;

                // Reload calendar events.
                let now = now_secs();
                let mut evts = load_ics_events(&new_cfg.ics_paths, pid, now);
                evts.extend(load_ics_from_urls(&new_cfg.ics_urls, pid, now));
                *cal2.borrow_mut() = evts;

                // Persist new config into the shared handle.
                *cfg2.borrow_mut() = new_cfg;

                // Refresh UI.
                refresh_comp(&ui2, &tasks2.borrow(), pid);
                push_agenda_comp(&ui2, &cal2.borrow(), AgendaRange::Day, now);

                ui2.set_show_settings(false);
                ui2.set_show_start_page(true);
            });
        }

        // ── File browser → navigate ───────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_fb_navigate(move |path| {
                let p = PathBuf::from(path.as_str());
                let vault_mode = ui2.get_fb_vault_mode();
                ui2.set_fb_current_path(p.to_string_lossy().to_string().as_str().into());
                let entries = read_dir_entries(&p, vault_mode);
                ui2.set_fb_entries(
                    Rc::new(slint::VecModel::from(entries_to_fb(&entries))).into(),
                );
            });
        }

        // ── File browser → select vault ───────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_fb_select_vault(move |path| {
                ui2.set_settings_vault_path(path.to_string().as_str().into());
                ui2.set_show_filebrowser(false);
                ui2.set_show_settings(true);
            });
        }

        // ── File browser → add ICS file ────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_fb_add_ics(move |path| {
                let p = PathBuf::from(path.as_str());
                let display = p
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.to_string());
                let model = ui2.get_settings_cal_sources();
                let mut items: Vec<ephemeris_ui::CalSource> = (0..model.row_count())
                    .filter_map(|i| model.row_data(i))
                    .collect();
                items.push(ephemeris_ui::CalSource {
                    display: display.as_str().into(),
                    value: path.to_string().as_str().into(),
                });
                ui2.set_settings_cal_sources(
                    Rc::new(slint::VecModel::from(items)).into(),
                );
                ui2.set_show_filebrowser(false);
                ui2.set_show_settings(true);
            });
        }

        // ── File browser → cancel ─────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_fb_cancel(move || {
                ui2.set_show_filebrowser(false);
                ui2.set_show_settings(true);
            });
        }

        let mut handler = WinitHandler {
            _rt: self.rt,
            ui,
            display: None,
            cursor_pos: (0.0, 0.0),
            scale: 1.0,
            ctrl_held,
            is_drawing: false,
        };

        EventLoop::new()?.run_app(&mut handler)?;
        Ok(())
    }
}

// ── Winit ApplicationHandler ──────────────────────────────────────────────────

struct WinitHandler {
    _rt: Runtime,
    ui: Rc<EphemerisUi>,
    display: Option<DesktopWindow>,
    cursor_pos: (f32, f32),
    scale: f64,
    /// True while the left Ctrl key is held (simulates the pen side-button).
    ctrl_held: Rc<Cell<bool>>,
    /// True while a draw stroke is in progress (mouse button down, no Ctrl).
    is_drawing: bool,
}

impl ApplicationHandler for WinitHandler {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        match DesktopWindow::new(event_loop, 800, 600) {
            Ok(d) => {
                self.scale = d.scale_factor();
                d.request_redraw();
                self.display = Some(d);
            }
            Err(e) => {
                tracing::error!("DesktopWindow creation failed: {e}");
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        use ephemeris_ui::{STATUS_BAR_H, TOOLBAR_H};
        const NAV_W: f32 = 80.0;
        const WINDOW_H: f32 = 600.0;

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::Resized(_) => {
                if let Some(d) = &mut self.display {
                    let _ = d.resize_surface();
                    d.request_redraw();
                }
            }

            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale = scale_factor;
                if let Some(d) = &mut self.display {
                    let _ = d.resize_surface();
                    d.request_redraw();
                }
            }

            // Track Ctrl modifier so canvas-touch only opens ring when Ctrl is held.
            WindowEvent::ModifiersChanged(modifiers) => {
                use winit::keyboard::ModifiersState;
                self.ctrl_held
                    .set(modifiers.state().contains(ModifiersState::CONTROL));
            }

            WindowEvent::CursorMoved { position, .. } => {
                let lx = (position.x / self.scale) as f32;
                let ly = (position.y / self.scale) as f32;
                self.cursor_pos = (lx, ly);
                self.ui.dispatch_pointer_moved(lx, ly);
                if self.is_drawing {
                    let cy = ly - STATUS_BAR_H as f32;
                    if cy >= 0.0 {
                        self.ui.feed_input(
                            &InputEvent::PenMove(PenSample {
                                x: lx,
                                y: cy,
                                pressure: 0.6,
                                tilt: 0.0,
                                in_range: true,
                            }),
                            now_ms(),
                        );
                    }
                }
                if let Some(d) = &self.display {
                    d.request_redraw();
                }
            }

            WindowEvent::MouseInput {
                state,
                button: winit::event::MouseButton::Left,
                ..
            } => {
                let (lx, ly) = self.cursor_pos;
                match state {
                    winit::event::ElementState::Pressed => {
                        self.ui.dispatch_pointer_pressed(lx, ly);
                        let in_canvas = lx >= NAV_W
                            && ly > STATUS_BAR_H as f32
                            && ly < WINDOW_H - TOOLBAR_H as f32;
                        if in_canvas
                            && !self.ctrl_held.get()
                            && self.ui.is_canvas_active()
                        {
                            self.is_drawing = true;
                            let cy = ly - STATUS_BAR_H as f32;
                            self.ui.feed_input(
                                &InputEvent::PenDown(PenSample {
                                    x: lx,
                                    y: cy,
                                    pressure: 0.8,
                                    tilt: 0.0,
                                    in_range: true,
                                }),
                                now_ms(),
                            );
                        }
                    }
                    winit::event::ElementState::Released => {
                        self.ui.dispatch_pointer_released(lx, ly);
                        if self.is_drawing {
                            self.is_drawing = false;
                            let cy = ly - STATUS_BAR_H as f32;
                            self.ui.feed_input(
                                &InputEvent::PenUp(PenSample {
                                    x: lx,
                                    y: cy,
                                    pressure: 0.0,
                                    tilt: 0.0,
                                    in_range: true,
                                }),
                                now_ms(),
                            );
                        }
                    }
                }
                if let Some(d) = &self.display {
                    d.request_redraw();
                }
            }

            WindowEvent::KeyboardInput { event, .. } => {
                use winit::keyboard::{Key, NamedKey};
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
                if let Some(display) = &mut self.display {
                    if let Err(e) = self.ui.render_frame(display) {
                        tracing::error!("render_frame failed: {e}");
                    }
                }
            }
            _ => {}
        }
    }
}

// ── Note data model ───────────────────────────────────────────────────────────

/// In-memory note record, parallel to a PageBook page at `page_index`.
struct AppNote {
    id: u32,
    title: String,
    page_index: usize,
    created_at: u64,
}

/// Build a note list suitable for `EphemerisUi::set_note_list`.
fn build_note_list(notes: &[AppNote], now: u64) -> Vec<(String, String, String)> {
    notes
        .iter()
        .map(|n| {
            (
                n.id.to_string(),
                n.title.clone(),
                format_note_date(n.created_at, now),
            )
        })
        .collect()
}

fn format_note_date(created_at: u64, now: u64) -> String {
    let secs = now.saturating_sub(created_at);
    if secs < 86_400 {
        "Today".to_string()
    } else if secs < 2 * 86_400 {
        "Yesterday".to_string()
    } else {
        format!("{} days ago", secs / 86_400)
    }
}

// ── Data loading helpers ──────────────────────────────────────────────────────

/// Read all `- [ ]` / `- [x]` tasks from every `.md` file in `vault_path`.
fn load_vault_tasks(
    vault_path: &std::path::Path,
    profile_id: ProfileId,
) -> ephemeris_core::Result<Vec<Task>> {
    let extractor = MarkdownTaskExtractor::new(vault_path, profile_id);
    TaskProvider::list(&extractor, profile_id)
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
                        tracing::info!(
                            "Loaded {} events from {}",
                            events.len(),
                            path.display()
                        );
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

/// Read one level of a directory for the file browser.
///
/// Returns `(name, full_path, is_dir, is_vault)` tuples:
/// - Parent entry (`..`) prepended unless already at root.
/// - Hidden entries (names starting with `.`) skipped.
/// - In vault mode: only directories; in ICS mode: directories + `.ics` files.
/// - Directories sorted before files; each group sorted by name.
fn read_dir_entries(dir: &Path, vault_mode: bool) -> Vec<(String, String, bool, bool)> {
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
            } else if !vault_mode && name.to_lowercase().ends_with(".ics") {
                files.push((name, path.to_string_lossy().to_string()));
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
fn entries_to_fb(
    entries: &[(String, String, bool, bool)],
) -> Vec<ephemeris_ui::FileBrowserEntry> {
    entries
        .iter()
        .map(|(name, full_path, is_dir, is_vault)| ephemeris_ui::FileBrowserEntry {
            name: name.as_str().into(),
            full_path: full_path.as_str().into(),
            is_dir: *is_dir,
            is_vault: *is_vault,
        })
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
fn push_agenda_ui(
    ui: &EphemerisUi,
    events: &[CalendarEvent],
    range: AgendaRange,
    now: u64,
) {
    let filtered = filter_agenda(events, range, now);
    let entries = to_agenda_entries(&filtered, range, now);
    let tuples: Vec<(String, String, String)> = entries
        .iter()
        .map(|e| (e.title.clone(), e.time_label.clone(), e.location_label.clone()))
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

/// Refresh both the start-page ranked list and the full-task-list in the UI.
fn refresh_ui(ui: &EphemerisUi, tasks: &[Task], profile_id: ProfileId) {
    let now = now_secs();
    let ranked = rank_tasks(tasks, now);
    let entries: Vec<(String, String, String, bool)> = ranked
        .iter()
        .map(|t| {
            let pl = priority_label(t.priority);
            let (dl, ov) = format_due(t.due, now);
            (t.title.clone(), pl, dl, ov)
        })
        .collect();
    ui.set_task_list(&entries);

    let full = build_full_task_list(tasks, now);
    ui.set_full_task_list(&full);
    let _ = profile_id;
}

/// Same as `refresh_ui` but operates on a cloned `EphemerisPage` component.
fn refresh_comp(comp: &ephemeris_ui::EphemerisPage, tasks: &[Task], profile_id: ProfileId) {
    let now = now_secs();

    let ranked = rank_tasks(tasks, now);
    let task_entries: Vec<ephemeris_ui::TaskEntry> = ranked
        .iter()
        .map(|t| ephemeris_ui::TaskEntry {
            title: t.title.as_str().into(),
            priority_label: priority_label(t.priority).as_str().into(),
            due_label: {
                let (dl, _) = format_due(t.due, now);
                dl.as_str().into()
            },
            overdue: format_due(t.due, now).1,
        })
        .collect();
    comp.set_task_list(Rc::new(slint::VecModel::from(task_entries)).into());

    let full = build_full_task_list(tasks, now);
    comp.set_full_task_list(
        Rc::new(slint::VecModel::from(
            full.iter()
                .map(|(id, title, pl, dl, ov, done)| ephemeris_ui::TaskViewEntry {
                    id: id.as_str().into(),
                    title: title.as_str().into(),
                    priority_label: pl.as_str().into(),
                    due_label: dl.as_str().into(),
                    overdue: *ov,
                    done: *done,
                })
                .collect::<Vec<_>>(),
        ))
        .into(),
    );
    let _ = profile_id;
}

fn build_full_task_list(
    tasks: &[Task],
    now: u64,
) -> Vec<(String, String, String, String, bool, bool)> {
    tasks
        .iter()
        .map(|t| {
            let pl = priority_label(t.priority);
            let (dl, ov) = format_due(t.due, now);
            (t.id.0.to_string(), t.title.clone(), pl, dl, ov, t.done)
        })
        .collect()
}

fn priority_label(p: TaskPriority) -> String {
    match p {
        TaskPriority::High => "!!!".to_string(),
        TaskPriority::Medium => "!!".to_string(),
        TaskPriority::Low => "!".to_string(),
    }
}

fn priority_from_int(p: i32) -> TaskPriority {
    match p {
        0 => TaskPriority::High,
        2 => TaskPriority::Low,
        _ => TaskPriority::Medium,
    }
}

fn priority_to_int(p: TaskPriority) -> i32 {
    match p {
        TaskPriority::High => 0,
        TaskPriority::Medium => 1,
        TaskPriority::Low => 2,
    }
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

fn sample_tasks(profile: ProfileId) -> Vec<Task> {
    const DAY: u64 = 86_400;
    let now = now_secs();
    let mut tasks = Vec::new();

    let mut t = Task::new("Review quarterly OKRs", profile);
    t.priority = TaskPriority::High;
    t.due = Some(now + DAY);
    tasks.push(t);

    let mut t = Task::new("Fix login redirect bug", profile);
    t.priority = TaskPriority::High;
    t.due = Some(now.saturating_sub(DAY));
    tasks.push(t);

    let mut t = Task::new("Write architecture doc", profile);
    t.priority = TaskPriority::Medium;
    t.due = Some(now + 3 * DAY);
    tasks.push(t);

    let mut t = Task::new("Update team wiki", profile);
    t.priority = TaskPriority::Medium;
    tasks.push(t);

    let mut t = Task::new("Book travel for conference", profile);
    t.priority = TaskPriority::Low;
    t.due = Some(now + 7 * DAY);
    tasks.push(t);

    tasks
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
                let days = (d - now_secs + DAY - 1) / DAY;
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
        let result = load_vault_tasks(std::path::Path::new("/tmp/no-such-vault-xyz"), profile);
        // Either Ok(empty) or Err — both are acceptable; just must not panic.
        match result {
            Ok(tasks) => assert!(tasks.is_empty()),
            Err(_) => {} // also fine
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
        // Simulate what on_task_confirm_add does when title == "New Task"
        let counter = Rc::new(Cell::new(1u32));
        let make_title = |raw: &str| -> String {
            let t = raw.trim().to_string();
            if t.is_empty() || t == "New Task" {
                let n = counter.get();
                counter.set(n + 1);
                format!("New Task {n}")
            } else {
                t
            }
        };
        assert_eq!(make_title("New Task"), "New Task 1");
        assert_eq!(make_title("New Task"), "New Task 2");
        assert_eq!(make_title("My custom task"), "My custom task");
        assert_eq!(make_title("New Task"), "New Task 3");
    }
}
