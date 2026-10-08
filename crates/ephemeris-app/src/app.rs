use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, Timelike, Utc};
use ephemeris_core::ics::IcsReader;
use ephemeris_core::ink::InkUpdate;
use ephemeris_core::model::Tool;
use ephemeris_core::prioritizer::rank_tasks;
use ephemeris_core::profile_manager::ProfileManager;
use ephemeris_core::{
    export_raster_pages_to_pdf, filter_agenda, markdown_to_blocks, markdown_to_plain,
    search_events, search_local_notes, search_tasks, strip_inline_markdown, sync_notes_to_vault,
    to_agenda_entries, AgendaRange, CalendarEvent, Config, MarkdownTaskExtractor, PdfExportOptions,
    ProfileId, RasterOcrTranscriber, RasterPage, SearchHit, Task, TaskPriority, TaskProvider,
    Transcriber, VaultFtsIndex, VaultNoteExport,
};
use ephemeris_pal::input::{InputEvent, PenSample};
use slint::{Global, Model};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::runtime::Runtime;
use tokio::sync::mpsc::UnboundedSender;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
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
        let event_loop = EventLoop::new()?;
        // Wayland cannot report the primary monitor before the window exists, so
        // size from EPHEMERIS_SIZE=WxH or a PineNote-class default. Touch/pen
        // coordinates are mapped to this UI size from the live window.
        let (win_w, win_h) = probe_window_size();
        tracing::info!("UI size (logical): {win_w}×{win_h}");

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
            match vault_fts.borrow_mut().reindex_vault(vault) {
                Ok(n) => {
                    tracing::info!("Indexed {n} vault notes for search");
                    *vault_fts_paths.borrow_mut() = vec![vault.clone()];
                }
                Err(e) => tracing::warn!("Vault FTS reindex failed: {e}"),
            }
        }
        let connections: Rc<RefCell<Vec<AppConnection>>> = Rc::new(RefCell::new(init_connections));
        save_connections(&connections.borrow());

        // ── Tasks ─────────────────────────────────────────────────────────────
        // Load from Obsidian vault when configured; fall back to demo data.
        let initial_tasks = {
            let cfg = config.borrow();
            if let Some(ref vault) = cfg.vault_path {
                match load_vault_tasks_with_inbox(vault, &cfg.vault_tasks_inbox, profile_id) {
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

        // ── Note & notebook persistence ───────────────────────────────────────
        let data_dir = note_data_dir();
        let _ = std::fs::create_dir_all(&data_dir);

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

        // Restore ink pixels for all known notes.
        for note in notes.borrow().iter() {
            if let Some((_w, _h, pixels)) = load_note_page(&data_dir, note.id) {
                ui.restore_page_pixels(note.page_index, pixels);
            }
        }

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
        let download_result: Arc<Mutex<Option<Result<(), String>>>> = Arc::new(Mutex::new(None));
        let download_active = Arc::new(AtomicBool::new(false));

        // ── Initial UI state ──────────────────────────────────────────────────
        refresh_ui(&ui, &tasks.borrow(), profile_id);
        push_agenda_ui(&ui, &calendar_events.borrow(), AgendaRange::Day, now);
        ui.set_show_start_page(true);
        ui.set_nav_section(0);

        // ── Profile list ──────────────────────────────────────────────────────
        let profile_mgr = Rc::new(RefCell::new(profile_mgr));

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
                    .filter(|c| c.kind == "calendar" && c.profile_id == connect_pid)
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
            let ui2 = ui.clone_component();
            Rc::new(move || {
                let filter = filter_rc.borrow().clone();
                let ria = ria_rc.borrow();
                let visible: Vec<AppRecording> = recs_rc
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
                push_recording_list_comp(&ui2, &visible);
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
            let push_recs_switch = push_recordings.clone();
            let push_nbs_switch = push_notebooks.clone();
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
                refresh_comp(&ui2, &filtered_tasks, work_pid);
                let range = int_to_range(ui2.get_agenda_range());
                let visible_events = filter_events_by_profile(&cal2.borrow(), &filter_str);
                push_agenda_comp(&ui2, &visible_events, range, now);
                set_active_profile_icon(&ui2, &mgr2, &id_str);
                push_recs_switch();
                push_nbs_switch();
            });
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
            ui.on_connection_set_profile(move |idx, profile_id| {
                let mut conns = conns_rc.borrow_mut();
                let i = idx as usize;
                if i < conns.len() {
                    conns[i].profile_id = profile_id.to_string();
                }
                save_connections(&conns);
                drop(conns);
                push();
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

        // ── Ring "Task" → task view ───────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            let tasks2 = tasks.clone();
            let cfg2 = config.clone();
            let filter_rc = active_profile_filter.clone();
            ui.on_ring_task(move || {
                {
                    let cfg = cfg2.borrow();
                    if let Some(ref vault) = cfg.vault_path {
                        reload_obsidian_tasks(
                            &mut tasks2.borrow_mut(),
                            vault,
                            &cfg.vault_tasks_inbox,
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
                push_full_task_list_comp(&ui2, &filtered);
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
                ui2.set_show_notebook_list(false);
                ui2.set_show_calendar(false);
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
            ui.on_nav_notes_tapped(move || {
                // Save current page state before switching views.
                ui_rc.save_current_page();
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
            let push_del = push_notebooks.clone();
            let ui2 = ui.clone_component();
            ui.on_note_delete_tapped(move |id| {
                let id_u32: u32 = id.parse().unwrap_or(0);
                if id_u32 == 0 {
                    return;
                }
                notes2.borrow_mut().retain(|n| n.id != id_u32);
                let page_path = data_dir2.join(format!("page_{id_u32}.bin"));
                let _ = std::fs::remove_file(&page_path);
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
                notes2
                    .borrow_mut()
                    .retain(|n| !doomed_notes.contains(&n.id));
                for nid in &doomed_notes {
                    let _ = std::fs::remove_file(data_dir2.join(format!("page_{nid}.bin")));
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
            ui.on_note_tapped(move |id| {
                let notes = notes2.borrow();
                if let Some(note) = notes.iter().find(|n| n.id.to_string() == id) {
                    let page_idx = note.page_index;
                    let note_id = note.id.to_string();
                    let note_title = note.title.clone();
                    let template = note.paper_template;
                    let anchor = note.anchor.clone().unwrap_or_default();
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
                    push_note_text_for_active(&ui_rc, &notes2.borrow(), &note_id);
                }
            });
        }

        // ── See-text toggle (OCR / transcript overlay) ───────────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let notes2 = notes.clone();
            let notebooks2 = notebooks.clone();
            let seq2 = note_id_seq.clone();
            let nb_seq2 = notebook_id_seq.clone();
            let data_dir2 = data_dir.clone();
            let cfg2 = config.clone();
            ui.on_note_see_text_toggled(move |show| {
                if show {
                    let active_id = ui2.get_active_note_id().to_string();
                    let (w, h) = ui_rc.canvas_size();
                    let pixels = ui_rc.get_committed_pixels();
                    let cfg = cfg2.borrow();
                    let ocr = RasterOcrTranscriber::from_parts(
                        cfg.ocr_enabled,
                        cfg.ocr_languages.clone(),
                        cfg.ocr_tessdata.clone(),
                    );
                    drop(cfg);
                    let text = match ocr.transcribe_raster(w, h, &pixels) {
                        Ok(spans) => RasterOcrTranscriber::spans_to_text(&spans),
                        Err(e) => {
                            tracing::warn!("See-text OCR failed: {e}");
                            String::new()
                        }
                    };
                    ui_rc.set_note_text_content(&text);
                    if !active_id.is_empty() {
                        if let Ok(id) = active_id.parse::<u32>() {
                            let mut ns = notes2.borrow_mut();
                            if let Some(note) = ns.iter_mut().find(|n| n.id == id) {
                                note.text_content = if text.is_empty() { None } else { Some(text) };
                                note.updated_at = now_secs();
                            }
                            drop(ns);
                            save_notes_index_and_sync_vault(
                                &data_dir2,
                                &notes2.borrow(),
                                &notebooks2.borrow(),
                                seq2.get(),
                                nb_seq2.get(),
                                &cfg2.borrow(),
                            );
                        }
                    }
                }
                // Force a clear refresh so ink disappears under see-text (or
                // reappears when returning to ink) without eink ghosting.
                ui_rc.request_screen_change();
            });
        }

        // ── Note list: "+ Note" → create in active folder (0 = root) ──────────
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
            let filter_nn = active_profile_filter.clone();
            let anchor_rc = current_note_anchor.clone();
            ui.on_note_new_tapped(move || {
                let new_id = seq2.get();
                seq2.set(new_id + 1);
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
                {
                    let mut ns = notes2.borrow_mut();
                    let idx = ns.len();
                    ns.push(AppNote {
                        id: new_id,
                        title: format!("Note {}", new_id),
                        page_index: idx,
                        created_at: now_secs(),
                        updated_at: now_secs(),
                        notebook_id: folder_id,
                        paper_template: ephemeris_core::PageTemplate::Blank,
                        anchor: None,
                        profile_id: pf,
                        text_content: None,
                    });
                }
                *anchor_rc.borrow_mut() = String::new();
                ui_rc.set_note_anchor_label("");
                ui_rc.push_new_page();
                ui_rc.set_page_title(&format!("Note {}", new_id));
                ui2.set_active_paper("blank".into());
                ui2.set_show_note_list(false);
                ui2.set_show_notebook_list(false);
                ui2.set_active_note_id(new_id.to_string().as_str().into());
                ui_rc.set_note_text_content("");
                ui_rc.set_note_see_text(false);
                save_notes_index_and_sync_vault(
                    &data_dir2,
                    &notes2.borrow(),
                    &notebooks2.borrow(),
                    seq2.get(),
                    nb_seq2.get(),
                    &cfg2.borrow(),
                );
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
                {
                    let mut ns = notes2.borrow_mut();
                    let idx = ns.len();
                    ns.push(AppNote {
                        id: new_id,
                        title: format!("Note {}", new_id),
                        page_index: idx,
                        created_at: now_secs(),
                        updated_at: now_secs(),
                        notebook_id: folder_id,
                        paper_template: ephemeris_core::PageTemplate::Blank,
                        anchor: None,
                        profile_id: pf,
                        text_content: None,
                    });
                }
                active_nb2.set(nb_id_u32);
                push_qn();
                ui_rc.set_note_anchor_label("");
                ui_rc.push_new_page();
                ui_rc.set_page_title(&format!("Note {}", new_id));
                ui2.set_active_paper("blank".into());
                ui2.set_show_note_list(false);
                ui2.set_show_notebook_list(false);
                ui2.set_drawing_menu_open(false);
                ui2.set_active_note_id(new_id.to_string().as_str().into());
                ui_rc.set_note_text_content("");
                ui_rc.set_note_see_text(false);
                save_notes_index_and_sync_vault(
                    &data_dir2,
                    &notes2.borrow(),
                    &notebooks2.borrow(),
                    seq2.get(),
                    nb_seq2.get(),
                    &cfg2.borrow(),
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
                );
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

        // Initial calendar data (month view)
        {
            let days = cal_month_days(today.year(), today.month(), &calendar_events.borrow());
            ui.set_cal_month_days(&days);
            let label = format!("{} {}", month_name(today.month()), today.year());
            ui.set_cal_header_label(&label);
        }

        // ── Nav rail: Calendar ────────────────────────────────────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let calendar_events2 = calendar_events.clone();
            let cal_year2 = cal_year.clone();
            let cal_month2 = cal_month.clone();
            let cal_day2 = cal_day.clone();
            let cal_sub_view2 = cal_sub_view.clone();
            let filter_rc = active_profile_filter.clone();
            ui.on_nav_calendar_tapped(move || {
                ui_rc.save_current_page();
                let sv = cal_sub_view2.get();
                let filter_str = filter_rc.borrow().clone();
                let visible = filter_events_by_profile(&calendar_events2.borrow(), &filter_str);
                refresh_calendar(
                    &ui_rc,
                    &visible,
                    cal_year2.get(),
                    cal_month2.get(),
                    cal_day2.get(),
                    sv,
                );
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
            let calendar_events2 = calendar_events.clone();
            let cal_year2 = cal_year.clone();
            let cal_month2 = cal_month.clone();
            let cal_day2 = cal_day.clone();
            let cal_sub_view2 = cal_sub_view.clone();
            let notes2 = notes.clone();
            let filter_rc = active_profile_filter.clone();
            ui.on_cal_sub_view_changed(move |v| {
                cal_sub_view2.set(v);
                ui_rc.set_cal_sub_view(v);
                let filter_str = filter_rc.borrow().clone();
                let visible = filter_events_by_profile(&calendar_events2.borrow(), &filter_str);
                refresh_calendar(
                    &ui_rc,
                    &visible,
                    cal_year2.get(),
                    cal_month2.get(),
                    cal_day2.get(),
                    v,
                );
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
            let ui_rc = ui.clone();
            let calendar_events2 = calendar_events.clone();
            let cal_year2 = cal_year.clone();
            let cal_month2 = cal_month.clone();
            let cal_day2 = cal_day.clone();
            let cal_sub_view2 = cal_sub_view.clone();
            let filter_rc = active_profile_filter.clone();
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
                let filter_str = filter_rc.borrow().clone();
                let visible = filter_events_by_profile(&calendar_events2.borrow(), &filter_str);
                refresh_calendar(&ui_rc, &visible, y, m, d, cal_sub_view2.get());
            });
        }
        {
            let ui_rc = ui.clone();
            let calendar_events2 = calendar_events.clone();
            let cal_year2 = cal_year.clone();
            let cal_month2 = cal_month.clone();
            let cal_day2 = cal_day.clone();
            let cal_sub_view2 = cal_sub_view.clone();
            let filter_rc = active_profile_filter.clone();
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
                let filter_str = filter_rc.borrow().clone();
                let visible = filter_events_by_profile(&calendar_events2.borrow(), &filter_str);
                refresh_calendar(&ui_rc, &visible, y, m, d, cal_sub_view2.get());
            });
        }

        // ── Calendar today button ─────────────────────────────────────────────
        {
            let ui_rc = ui.clone();
            let calendar_events2 = calendar_events.clone();
            let cal_year2 = cal_year.clone();
            let cal_month2 = cal_month.clone();
            let cal_day2 = cal_day.clone();
            let cal_sub_view2 = cal_sub_view.clone();
            let notes2 = notes.clone();
            let filter_rc = active_profile_filter.clone();
            ui.on_cal_today(move || {
                let now = Utc::now();
                let (y, m, d) = (now.year(), now.month(), now.day());
                cal_year2.set(y);
                cal_month2.set(m);
                cal_day2.set(d);
                let sv = cal_sub_view2.get();
                let filter_str = filter_rc.borrow().clone();
                let visible = filter_events_by_profile(&calendar_events2.borrow(), &filter_str);
                refresh_calendar(&ui_rc, &visible, y, m, d, sv);
                if sv == 0 {
                    refresh_day_note_ui(&ui_rc, &notes2.borrow(), y, m, d);
                }
            });
        }

        // ── Calendar day tap (from month/year → day view) ─────────────────────
        {
            let ui_rc = ui.clone();
            let calendar_events2 = calendar_events.clone();
            let cal_year2 = cal_year.clone();
            let cal_month2 = cal_month.clone();
            let cal_day2 = cal_day.clone();
            let cal_sub_view2 = cal_sub_view.clone();
            let notes2 = notes.clone();
            let filter_rc = active_profile_filter.clone();
            ui.on_cal_day_tapped(move |y, m, d| {
                cal_year2.set(y);
                cal_month2.set(m as u32);
                cal_day2.set(d as u32);
                let sv = cal_sub_view2.get();
                // If in year/month view, jump to month/day view
                let new_sv = if sv == 3 { 2 } else { 0 };
                cal_sub_view2.set(new_sv);
                ui_rc.set_cal_sub_view(new_sv);
                let filter_str = filter_rc.borrow().clone();
                let visible = filter_events_by_profile(&calendar_events2.borrow(), &filter_str);
                refresh_calendar(&ui_rc, &visible, y, m as u32, d as u32, new_sv);
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
                        });
                        ui_rc.push_new_page();
                        save_notes_index_and_sync_vault(
                            &data_dir2,
                            &notes2.borrow(),
                            &notebooks2.borrow(),
                            seq2.get(),
                            nb_seq2.get(),
                            &cfg2.borrow(),
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
            let calendar_events2 = calendar_events.clone();
            let cal_year2 = cal_year.clone();
            let cal_month2 = cal_month.clone();
            let cal_day2 = cal_day.clone();
            let cal_sub_view2 = cal_sub_view.clone();
            let notes2 = notes.clone();
            let anchor_rc = current_note_anchor.clone();
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
                        refresh_calendar(&ui_rc, &calendar_events2.borrow(), y, m, d, 0);
                        refresh_day_note_ui(&ui_rc, &notes2.borrow(), y, m, d);
                        ui_rc.save_current_page();
                        ui2.set_show_calendar(true);
                        ui2.set_show_note_list(false);
                        ui2.set_show_notebook_list(false);
                        ui2.set_nav_section(3);
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
                ui2.set_nav_section(1);
                ui2.set_show_note_list(false);
                ui2.set_show_notebook_list(true);
            });
        }

        // ── Ring "Note" → create a new note immediately and open canvas ───────
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
            let filter_ring = active_profile_filter.clone();
            let anchor_rc = current_note_anchor.clone();
            ui.on_ring_note(move || {
                ui_rc.save_current_page();
                let new_id = seq2.get();
                seq2.set(new_id + 1);
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
                {
                    let mut ns = notes2.borrow_mut();
                    let idx = ns.len();
                    ns.push(AppNote {
                        id: new_id,
                        title: format!("Note {}", new_id),
                        page_index: idx,
                        created_at: now_secs(),
                        updated_at: now_secs(),
                        notebook_id: folder_id,
                        paper_template: ephemeris_core::PageTemplate::Blank,
                        anchor: None,
                        profile_id: pf,
                        text_content: None,
                    });
                }
                *anchor_rc.borrow_mut() = String::new();
                ui_rc.set_note_anchor_label("");
                ui_rc.push_new_page();
                ui_rc.set_page_title(&format!("Note {}", new_id));
                ui2.set_active_paper("blank".into());
                ui2.set_show_start_page(false);
                ui2.set_show_task_view(false);
                ui2.set_show_settings(false);
                ui2.set_show_filebrowser(false);
                ui2.set_show_calendar(false);
                ui2.set_show_recording_list(false);
                ui2.set_show_search(false);
                ui2.set_show_md_viewer(false);
                ui2.set_show_note_list(false);
                ui2.set_show_notebook_list(false);
                ui2.set_drawing_menu_open(false);
                ui2.set_ring_visible(false);
                ui2.set_nav_section(1);
                ui2.set_active_note_id(new_id.to_string().as_str().into());
                ui_rc.set_note_text_content("");
                ui_rc.set_note_see_text(false);
                save_notes_index_and_sync_vault(
                    &data_dir2,
                    &notes2.borrow(),
                    &notebooks2.borrow(),
                    seq2.get(),
                    nb_seq2.get(),
                    &cfg2.borrow(),
                );
            });
        }

        // ── Ring "Event" → calendar view ─────────────────────────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let calendar_events2 = calendar_events.clone();
            let cal_year2 = cal_year.clone();
            let cal_month2 = cal_month.clone();
            let cal_day2 = cal_day.clone();
            let cal_sub_view2 = cal_sub_view.clone();
            ui.on_ring_calendar(move || {
                let sv = cal_sub_view2.get();
                refresh_calendar(
                    &ui_rc,
                    &calendar_events2.borrow(),
                    cal_year2.get(),
                    cal_month2.get(),
                    cal_day2.get(),
                    sv,
                );
                ui2.set_show_start_page(false);
                ui2.set_show_task_view(false);
                ui2.set_show_settings(false);
                ui2.set_show_filebrowser(false);
                ui2.set_show_note_list(false);
                ui2.set_show_notebook_list(false);
                ui2.set_show_calendar(true);
                ui2.set_ring_visible(false);
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
                    ui2.set_nav_section(3);
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
            ui.on_nav_rec_tapped(move || {
                ui_rc.save_current_page();
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

        // ── Recording: transcribe ─────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            let recordings2 = recordings.clone();
            let rec_dir2 = rec_dir.clone();
            let txn_pending2 = transcription_pending.clone();
            let txn_active2 = transcription_active.clone();
            ui.on_rec_transcribe_tapped(move |id| {
                let id_u32: u32 = id.parse().unwrap_or(0);
                let recs = recordings2.borrow();
                let rec = recs.iter().find(|r| r.id == id_u32).cloned();
                drop(recs);
                let Some(rec) = rec else { return };

                // Track which recording is being shown so Save knows which to update.
                ui2.set_rec_active_id(rec.id.to_string().as_str().into());

                // If we already have a transcription, just display it.
                if let Some(text) = &rec.transcription {
                    ui2.set_rec_transcription_text(text.as_str().into());
                    ui2.set_rec_transcription_visible(true);
                    return;
                }

                // Show "Transcribing…" immediately while the subprocess runs.
                ui2.set_rec_transcription_text("Transcribing…".into());
                ui2.set_rec_transcription_visible(true);

                if txn_active2.swap(true, Ordering::AcqRel) {
                    // Another transcription is already running.
                    ui2.set_rec_transcription_text(
                        "A transcription is already running. Please wait.".into(),
                    );
                    return;
                }

                let wav_path = rec_dir2.join(&rec.audio_filename);
                let lang = ui2.get_whisper_lang();
                let mailbox = txn_pending2.clone();
                let active_flag = txn_active2.clone();
                std::thread::spawn(move || {
                    let text = run_whisper(&wav_path, &lang);
                    if let Ok(mut pending) = mailbox.lock() {
                        pending.push((id_u32, text));
                    }
                    active_flag.store(false, Ordering::Release);
                });
            });
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
                    rec.title = new_title;
                }
                drop(recs);
                save_recordings_index(&rec_dir2, &recordings2.borrow(), rec_id_seq2.get());
                push_recs_ren();
                ui2.set_rec_rename_visible(false);
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
                let mut recs = recordings2.borrow_mut();
                if let Some(rec) = recs.iter_mut().find(|r| r.id == active_id) {
                    rec.transcription = Some(text.to_string());
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
            ui.on_rec_transcription_retranscribe(move || {
                let active_id: u32 = ui2.get_rec_active_id().parse().unwrap_or(0);
                if active_id == 0 {
                    return;
                }

                if txn_active2.swap(true, Ordering::AcqRel) {
                    ui2.set_rec_transcription_text(
                        "A transcription is already running. Please wait.".into(),
                    );
                    return;
                }

                // Clear stored transcription so a fresh result is written back.
                let wav_path = {
                    let mut recs = recordings2.borrow_mut();
                    let Some(rec) = recs.iter_mut().find(|r| r.id == active_id) else {
                        txn_active2.store(false, Ordering::Release);
                        return;
                    };
                    rec.transcription = None;
                    let path = rec_dir2.join(&rec.audio_filename);
                    drop(recs);
                    save_recordings_index(&rec_dir2, &recordings2.borrow(), rec_id_seq2.get());
                    path
                };

                ui2.set_rec_transcription_text("Transcribing…".into());
                let lang = ui2.get_whisper_lang();
                let mailbox = txn_pending2.clone();
                let active_flag = txn_active2.clone();
                std::thread::spawn(move || {
                    let text = run_whisper(&wav_path, &lang);
                    if let Ok(mut pending) = mailbox.lock() {
                        pending.push((active_id, text));
                    }
                    active_flag.store(false, Ordering::Release);
                });
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
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            ui.on_ring_clear(move || {
                ui_rc.clear_ink();
                ui2.set_ring_visible(false);
            });
        }

        // ── Burger menu tool selection → ink engine ───────────────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            ui.on_tool_selected(move |tool| {
                let t = match tool.as_str() {
                    "pen" => Tool::Pen,
                    "highlighter" => Tool::Highlighter,
                    "eraser" => Tool::Eraser,
                    _ => return,
                };
                ui_rc.set_tool(t);
                ui2.set_active_tool(tool.into());
            });
        }

        // ── Start-page "All Tasks" button → task view ─────────────────────────
        {
            let ui2 = ui.clone_component();
            let tasks2 = tasks.clone();
            let cfg2 = config.clone();
            let filter_rc = active_profile_filter.clone();
            ui.on_start_page_tasks(move || {
                // Refresh vault tasks so external Obsidian edits show up.
                {
                    let cfg = cfg2.borrow();
                    if let Some(ref vault) = cfg.vault_path {
                        reload_obsidian_tasks(
                            &mut tasks2.borrow_mut(),
                            vault,
                            &cfg.vault_tasks_inbox,
                            work_pid,
                        );
                    }
                }
                // Reset task-view filters when entering the page (active = hide closed).
                ui2.set_task_filter("active".into());
                ui2.set_task_sort("priority".into());
                ui2.set_task_profile_filter("".into());
                ui2.set_task_search_query("".into());
                let filter_str = filter_rc.borrow().clone();
                let filtered = filter_tasks_by_profile(&tasks2.borrow(), &filter_str);
                push_full_task_list_comp(&ui2, &filtered);
                ui2.set_show_start_page(false);
                ui2.set_show_recording_list(false);
                ui2.set_show_task_view(true);
            });
        }

        // ── Nav rail "Tasks" → task view ─────────────────────────────────────
        {
            let ui_rc = ui.clone();
            let ui2 = ui.clone_component();
            let tasks2 = tasks.clone();
            let cfg2 = config.clone();
            let filter_rc = active_profile_filter.clone();
            ui.on_nav_tasks_tapped(move || {
                ui_rc.save_current_page();
                {
                    let cfg = cfg2.borrow();
                    if let Some(ref vault) = cfg.vault_path {
                        reload_obsidian_tasks(
                            &mut tasks2.borrow_mut(),
                            vault,
                            &cfg.vault_tasks_inbox,
                            work_pid,
                        );
                    }
                }
                ui2.set_task_filter("active".into());
                ui2.set_task_sort("priority".into());
                ui2.set_task_profile_filter("".into());
                ui2.set_task_search_query("".into());
                let filter_str = filter_rc.borrow().clone();
                let filtered = filter_tasks_by_profile(&tasks2.borrow(), &filter_str);
                push_full_task_list_comp(&ui2, &filtered);
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
                ui2.set_nav_section(2);
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
                ui2.set_nav_section(0);
                ui2.set_show_start_page(true);
            });
        }

        // ── Toggle task done ──────────────────────────────────────────────────
        {
            let tasks2 = tasks.clone();
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            let filter_rc = active_profile_filter.clone();
            ui.on_task_complete_toggled(move |id| {
                let mut dirty: Option<Task> = None;
                {
                    let mut ts = tasks2.borrow_mut();
                    if let Some(t) = ts.iter_mut().find(|t| t.id.0.to_string() == id) {
                        t.set_done(!t.done);
                        if t.source == "obsidian" {
                            dirty = Some(t.clone());
                        }
                    }
                }
                if let Some(ref t) = dirty {
                    write_back_obsidian_task(&cfg2.borrow(), t);
                }
                let filter_str = filter_rc.borrow().clone();
                let filtered = filter_tasks_by_profile(&tasks2.borrow(), &filter_str);
                refresh_comp(&ui2, &filtered, work_pid);
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
                    ui2.set_task_dialog_due(format_due_ymd(t.due).as_str().into());
                    ui2.set_task_dialog_tags(t.tags.join(" ").as_str().into());
                    ui2.set_task_dialog_visible(true);
                }
            });
        }

        // ── Add confirmed ─────────────────────────────────────────────────────
        {
            let tasks2 = tasks.clone();
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            let filter_rc = active_profile_filter.clone();
            let counter = new_task_n.clone();
            ui.on_task_confirm_add(move |title, priority, due, tags| {
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
                {
                    let mut ts = tasks2.borrow_mut();
                    let mut t = Task::new(title, task_profile);
                    t.priority = priority_from_int(priority);
                    t.due = parse_due_ymd(&due);
                    t.tags = parse_tag_list(&tags);
                    // When a vault is connected, create into Obsidian inbox.
                    if cfg2.borrow().vault_path.is_some() {
                        t.source = "obsidian".to_string();
                        if !create_obsidian_task(&mut ts, &cfg2.borrow(), &t, task_profile) {
                            // Fall back to local in-memory task if write failed.
                            t.source = "local".to_string();
                            ts.push(t);
                        }
                    } else {
                        ts.push(t);
                    }
                }
                ui2.set_task_dialog_visible(false);
                let filtered = filter_tasks_by_profile(&tasks2.borrow(), &filter_str);
                refresh_comp(&ui2, &filtered, work_pid);
            });
        }

        // ── Edit confirmed — title / priority / due / tags (+ Obsidian write-back)
        {
            let tasks2 = tasks.clone();
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            let filter_rc = active_profile_filter.clone();
            ui.on_task_confirm_edit(move |id, title, priority, due, tags| {
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
                        if t.source == "obsidian" {
                            dirty = Some(t.clone());
                        }
                    }
                }
                if let Some(ref t) = dirty {
                    write_back_obsidian_task(&cfg2.borrow(), t);
                }
                ui2.set_task_dialog_visible(false);
                let filter_str = filter_rc.borrow().clone();
                let filtered = filter_tasks_by_profile(&tasks2.borrow(), &filter_str);
                refresh_comp(&ui2, &filtered, work_pid);
            });
        }

        // ── Dialog cancel ─────────────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_task_dialog_cancel(move || {
                ui2.set_task_dialog_visible(false);
            });
        }

        // ── Task filter / sort / profile-chip / search → rebuild list ─────────
        {
            let ui2 = ui.clone_component();
            let tasks2 = tasks.clone();
            ui.on_task_filter_changed(move |_filter| {
                push_full_task_list_comp(&ui2, &tasks2.borrow());
            });
        }
        {
            let ui2 = ui.clone_component();
            let tasks2 = tasks.clone();
            ui.on_task_sort_changed(move |_sort| {
                push_full_task_list_comp(&ui2, &tasks2.borrow());
            });
        }
        {
            let ui2 = ui.clone_component();
            let tasks2 = tasks.clone();
            let filter_rc = active_profile_filter.clone();
            ui.on_task_profile_filter_changed(move |_pid| {
                let filter_str = filter_rc.borrow().clone();
                let filtered = filter_tasks_by_profile(&tasks2.borrow(), &filter_str);
                refresh_comp(&ui2, &filtered, work_pid);
            });
        }
        {
            let ui2 = ui.clone_component();
            let tasks2 = tasks.clone();
            ui.on_task_search_changed(move |q| {
                ui2.set_task_search_query(q.as_str().into());
                push_full_task_list_comp(&ui2, &tasks2.borrow());
            });
        }

        // ── Task priority cycle ───────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            let tasks2 = tasks.clone();
            let cfg2 = config.clone();
            let filter_rc = active_profile_filter.clone();
            ui.on_task_priority_cycle(move |id| {
                let mut dirty: Option<Task> = None;
                {
                    let mut ts = tasks2.borrow_mut();
                    if let Some(t) = ts.iter_mut().find(|t| t.id.0.to_string() == id) {
                        t.priority = match t.priority {
                            TaskPriority::High => TaskPriority::Medium,
                            TaskPriority::Medium => TaskPriority::Low,
                            TaskPriority::Low => TaskPriority::High,
                        };
                        if t.source == "obsidian" {
                            dirty = Some(t.clone());
                        }
                    }
                }
                if let Some(ref t) = dirty {
                    write_back_obsidian_task(&cfg2.borrow(), t);
                }
                let filter_str = filter_rc.borrow().clone();
                let profile_filtered = filter_tasks_by_profile(&tasks2.borrow(), &filter_str);
                refresh_comp(&ui2, &profile_filtered, work_pid);
            });
        }

        // Shared modifier state: Ctrl held = ring mode, plain click = draw.
        // On the real eink device this maps to PenButton; Ctrl is the desktop stand-in.
        let ctrl_held: Rc<Cell<bool>> = Rc::new(Cell::new(false));

        // True if the canvas's own TouchArea received the most recent pointer-down.
        // Because Slint only routes PointerPressed to the topmost element at the click
        // position, this stays false when interactive chrome (burger, ring, nav rail)
        // is clicked — letting us block spurious ink strokes on those elements.
        let canvas_touch_down: Rc<Cell<bool>> = Rc::new(Cell::new(false));

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
                ui2.set_drawing_menu_open(false);
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
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            let push_so = push_connections.clone();
            let default_pid = default_conn_profile.clone();
            ui.on_settings_open(move || {
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
                ui2.set_settings_cal_sources(Rc::new(slint::VecModel::from(sources)).into());
                ui2.set_settings_new_url("".into());
                ui2.set_whisper_model_installed(find_whisper_model().is_some());
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
            ui.on_search_open(move || {
                // Persist ink, then hide the canvas layer so strokes cannot
                // bleed through the search overlay via min() compositing.
                ui_rc.save_current_page();
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
                    if cached {
                        let n = fts2.borrow().len().unwrap_or(0);
                        tracing::info!(
                            "Search open: reusing FTS index ({n} notes, {} vault(s))",
                            vaults.len()
                        );
                        format!("Ready — {n} vault notes from {}.", names.join(", "))
                    } else {
                        let refs: Vec<&Path> = vaults.iter().map(PathBuf::as_path).collect();
                        match fts2.borrow_mut().reindex_vaults(&refs) {
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
                ui2.set_nav_section(0);
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
                                let title = Path::new(&id)
                                    .file_stem()
                                    .and_then(|s| s.to_str())
                                    .unwrap_or("Note")
                                    .to_string();
                                let body = markdown_to_plain(&md);
                                let blocks = markdown_to_blocks(&md);
                                ui_rc.set_md_viewer(&title, &body, &id);
                                ui_rc.set_md_viewer_blocks(&blocks);
                                ui2.set_show_search(false);
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
                            ui_rc.set_note_text_content(&text);
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
                        ui2.set_nav_section(2);
                    }
                    "calendar" => {
                        ui2.set_show_search(false);
                        ui2.set_show_md_viewer(false);
                        ui2.set_show_calendar(true);
                        ui2.set_nav_section(3);
                    }
                    _ => {}
                }
            });
        }

        // ── MD viewer back → search ───────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_md_viewer_back(move || {
                ui2.set_show_md_viewer(false);
                ui2.set_show_search(true);
            });
        }

        // ── Settings → back ───────────────────────────────────────────────────
        {
            let ui2 = ui.clone_component();
            ui.on_settings_back(move || {
                ui2.set_show_settings(false);
                ui2.set_show_search(false);
                ui2.set_show_md_viewer(false);
                ui2.set_nav_section(0);
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
            let ui2 = ui.clone_component();
            let cfg2 = config.clone();
            let tasks2 = tasks.clone();
            let cal2 = calendar_events.clone();
            let fts2 = vault_fts.clone();
            let fts_paths2 = vault_fts_paths.clone();
            let filter_rc = active_profile_filter.clone();
            let conns_ss = connections.clone();
            ui.on_settings_save(move || {
                let theme_idx = ui2.get_settings_theme_idx();
                let theme = if theme_idx == 1 { "dark" } else { "light" };

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
                let new_cfg = Config {
                    theme: theme.to_string(),
                    vault_tasks_inbox: cfg2.borrow().vault_tasks_inbox.clone(),
                    vault_export_subdir: cfg2.borrow().vault_export_subdir.clone(),
                    ocr_enabled: cfg2.borrow().ocr_enabled,
                    ocr_languages: cfg2.borrow().ocr_languages.clone(),
                    ocr_tessdata: cfg2.borrow().ocr_tessdata.clone(),
                    vault_path: if vault_str.is_empty() {
                        None
                    } else {
                        Some(expand_user_path(&vault_str))
                    },
                    ics_urls,
                    ics_paths,
                    ..Config::default()
                };

                if let Err(e) = new_cfg.save() {
                    tracing::warn!("Config save failed: {e}");
                }
                save_connections(&conns_ss.borrow());

                // Apply theme immediately.
                apply_theme_to_comp(&ui2, theme);

                // Reload tasks (vault tasks use work_pid; sample tasks span both profiles).
                let new_tasks = if let Some(ref vault) = new_cfg.vault_path {
                    load_vault_tasks_with_inbox(vault, &new_cfg.vault_tasks_inbox, work_pid)
                        .unwrap_or_else(|_| sample_tasks_split(work_pid, personal_pid))
                } else {
                    sample_tasks_split(work_pid, personal_pid)
                };
                *tasks2.borrow_mut() = new_tasks;

                // Reload calendar events.
                let now = now_secs();
                let mut evts = load_ics_events(&new_cfg.ics_paths, work_pid, now);
                evts.extend(load_ics_from_urls(&new_cfg.ics_urls, work_pid, now));
                *cal2.borrow_mut() = evts;

                // Persist new config into the shared handle.
                *cfg2.borrow_mut() = new_cfg.clone();

                // Rebuild vault search index for the new vault path.
                if let Some(ref vault) = new_cfg.vault_path {
                    match fts2.borrow_mut().reindex_vault(vault) {
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
                refresh_comp(&ui2, &filtered_tasks, work_pid);
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
                    profile_id,
                    allow_in_aggregated: true,
                });
                save_connections(&conns);
                drop(conns);
                push_v();

                // Apply immediately — don't wait for Settings → Save, otherwise
                // search/tasks keep using a stale or empty vault_path.
                {
                    let mut cfg = cfg2.borrow_mut();
                    cfg.vault_path = Some(vault_path.clone());
                    if let Err(e) = cfg.save() {
                        tracing::warn!("Config save after vault select failed: {e}");
                    }
                }
                match fts2.borrow_mut().reindex_vault(&vault_path) {
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
                    work_pid,
                );

                ui2.set_settings_vault_path(vault_path.to_string_lossy().as_ref().into());
                ui2.set_show_filebrowser(false);
                ui2.set_show_settings(true);
                ui2.set_settings_section(2);
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
            let ui2 = ui.clone_component();
            let rec_handle2 = rec_handle.clone();
            let rec_id_seq2 = rec_id_seq.clone();
            let rec_dir2 = rec_dir.clone();
            let rec_pf_cp = rec_pending_profile.clone();
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
                    _ => {}
                }
            });
            let ui2_cancel = ui.clone_component();
            ui.on_create_profile_pick_cancel(move || {
                ui2_cancel.set_show_create_profile_picker(false);
            });
        }

        let mut handler = WinitHandler {
            _rt: self.rt,
            ui,
            display: None,
            cursor_pos: (0.0, 0.0),
            scale: 1.0,
            ctrl_held,
            canvas_touch_down,
            is_drawing: false,
            data_dir,
            notes: notes.clone(),
            notebooks: notebooks.clone(),
            config: config.clone(),
            rec_handle,
            recordings: recordings.clone(),
            rec_id_seq: rec_id_seq.clone(),
            rec_dir: rec_dir.clone(),
            transcription_pending,
            transcription_active,
            download_result,
            download_active,
            push_recordings,
        };

        event_loop.run_app(&mut handler)?;
        Ok(())
    }
}

/// Logical UI size: `EPHEMERIS_SIZE=WxH`, else PineNote portrait (1404×1872).
fn probe_window_size() -> (u32, u32) {
    if let Ok(raw) = std::env::var("EPHEMERIS_SIZE") {
        if let Some((w, h)) = raw.split_once('x').or_else(|| raw.split_once('X')) {
            if let (Ok(w), Ok(h)) = (w.trim().parse::<u32>(), h.trim().parse::<u32>()) {
                if w >= 320 && h >= 320 {
                    return (w, h);
                }
            }
        }
        tracing::warn!("Ignoring invalid EPHEMERIS_SIZE={raw:?} (expected e.g. 1404x1872)");
    }
    // PineNote panel is 1872×1404; portrait is the usual handheld orientation.
    (1404, 1872)
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
    /// Set by the canvas TouchArea's pointer-event(down) callback; cleared
    /// before each dispatch so we know whether the canvas (vs. chrome like the
    /// burger button) actually received the press.
    canvas_touch_down: Rc<Cell<bool>>,
    /// True while a draw stroke is in progress (mouse button down, no Ctrl).
    is_drawing: bool,
    /// Directory where per-note pixel data is saved.
    data_dir: PathBuf,
    /// Shared note list — used to map page_idx → note_id for persistence.
    notes: Rc<RefCell<Vec<AppNote>>>,
    notebooks: Rc<RefCell<Vec<AppNotebook>>>,
    config: Rc<RefCell<Config>>,
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
    /// Result of a model download: Ok(()) on success, Err(msg) on failure.
    download_result: Arc<Mutex<Option<Result<(), String>>>>,
    /// True while a model download thread is running.
    download_active: Arc<AtomicBool>,
    /// Profile-filtered recording list refresh — called after transcription completes.
    push_recordings: Rc<dyn Fn()>,
}

impl WinitHandler {
    /// Map a physical window position into UI logical coordinates.
    ///
    /// Normalises against the current physical window size so fullscreen /
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

    fn handle_pointer_move(&mut self, lx: f32, ly: f32, pressure: f32) {
        use ephemeris_ui::STATUS_BAR_H;
        self.cursor_pos = (lx, ly);
        self.ui.dispatch_pointer_moved(lx, ly);
        if self.is_drawing && !self.ui.is_canvas_active() {
            self.is_drawing = false;
        }
        if self.is_drawing {
            let cy = ly - STATUS_BAR_H as f32;
            if cy >= 0.0 {
                self.ui.feed_input(
                    &InputEvent::PenMove(PenSample {
                        x: lx,
                        y: cy,
                        pressure,
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

    fn handle_pointer_press(&mut self, lx: f32, ly: f32, pressure: f32) {
        use ephemeris_ui::{STATUS_BAR_H, TOOLBAR_H};
        let (_, ui_h) = self.ui.size();
        self.cursor_pos = (lx, ly);
        // Reset canvas-touch gate before dispatching so we can detect whether
        // the canvas TouchArea (vs. chrome) got this press.
        self.canvas_touch_down.set(false);
        let canvas_was_active = self.ui.is_canvas_active();
        self.ui.dispatch_pointer_pressed(lx, ly);
        let in_canvas = ly > STATUS_BAR_H as f32 && ly < ui_h as f32 - TOOLBAR_H as f32;
        if in_canvas
            && !self.ctrl_held.get()
            && canvas_was_active
            && self.ui.is_canvas_active()
            && self.canvas_touch_down.get()
        {
            self.is_drawing = true;
            let cy = ly - STATUS_BAR_H as f32;
            self.ui.feed_input(
                &InputEvent::PenDown(PenSample {
                    x: lx,
                    y: cy,
                    pressure,
                    tilt: 0.0,
                    in_range: true,
                }),
                now_ms(),
            );
        }
        if let Some(d) = &self.display {
            d.request_redraw();
        }
    }

    fn handle_pointer_release(&mut self, lx: f32, ly: f32) {
        use ephemeris_ui::STATUS_BAR_H;
        self.cursor_pos = (lx, ly);
        self.ui.dispatch_pointer_released(lx, ly);
        if self.is_drawing {
            self.is_drawing = false;
            let cy = ly - STATUS_BAR_H as f32;
            let update = self.ui.feed_input(
                &InputEvent::PenUp(PenSample {
                    x: lx,
                    y: cy,
                    pressure: 0.0,
                    tilt: 0.0,
                    in_range: true,
                }),
                now_ms(),
            );
            if matches!(update, InkUpdate::Finished { .. }) {
                let page_idx = self.ui.current_page_idx();
                let note_id = self
                    .notes
                    .borrow()
                    .iter()
                    .find(|n| n.page_index == page_idx)
                    .map(|n| n.id);
                if let Some(note_id) = note_id {
                    let pixels = self.ui.get_committed_pixels();
                    let (w, h) = self.ui.canvas_size();
                    save_note_page(&self.data_dir, note_id, w, h, &pixels);
                    if let Some(note) = self.notes.borrow_mut().iter_mut().find(|n| n.id == note_id)
                    {
                        note.updated_at = now_secs();
                    }
                    export_single_note_to_vault(
                        &self.config.borrow(),
                        &self.notes.borrow(),
                        &self.notebooks.borrow(),
                        &self.data_dir,
                        note_id,
                    );
                }
            }
        }
        if let Some(d) = &self.display {
            d.request_redraw();
        }
    }
}

impl ApplicationHandler for WinitHandler {
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
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

        // --- Drain completed transcriptions ---
        if let Ok(mut pending) = self.transcription_pending.lock() {
            if !pending.is_empty() {
                let drained: Vec<(u32, String)> = pending.drain(..).collect();
                drop(pending);
                let mut recs = self.recordings.borrow_mut();
                let mut updated = false;
                for (id, text) in drained {
                    if let Some(rec) = recs.iter_mut().find(|r| r.id == id) {
                        rec.transcription = Some(text.clone());
                        updated = true;
                        // Update the visible transcription dialog if it's still open.
                        self.ui
                            .clone_component()
                            .set_rec_transcription_text(slint::SharedString::from(text.as_str()));
                    }
                }
                if updated {
                    save_recordings_index(&self.rec_dir, &recs, self.rec_id_seq.get());
                    drop(recs);
                    (self.push_recordings)();
                    if let Some(d) = &self.display {
                        d.request_redraw();
                    }
                }
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
        {
            // Keep waking while a whisper subprocess or model download is running.
            drop(h_opt);
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(500),
            ));
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let (w, h) = self.ui.size();
        match DesktopWindow::new(event_loop, w, h) {
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
                let (lx, ly) = self.map_to_ui(position);
                self.handle_pointer_move(lx, ly, 0.6);
            }

            WindowEvent::MouseInput {
                state,
                button: winit::event::MouseButton::Left,
                ..
            } => {
                let (lx, ly) = self.cursor_pos;
                match state {
                    winit::event::ElementState::Pressed => {
                        self.handle_pointer_press(lx, ly, 0.8);
                    }
                    winit::event::ElementState::Released => {
                        self.handle_pointer_release(lx, ly);
                    }
                }
            }

            // PineNote / Wayland: stylus and finger contacts arrive as Touch,
            // not MouseInput. Without this the UI never sees pen or touch.
            WindowEvent::Touch(touch) => {
                use winit::event::TouchPhase;
                let (lx, ly) = self.map_to_ui(touch.location);
                let pressure = touch
                    .force
                    .map(|f| f.normalized() as f32)
                    .unwrap_or(0.8)
                    .clamp(0.05, 1.0);
                match touch.phase {
                    TouchPhase::Started => self.handle_pointer_press(lx, ly, pressure),
                    TouchPhase::Moved => self.handle_pointer_move(lx, ly, pressure),
                    TouchPhase::Ended | TouchPhase::Cancelled => {
                        self.handle_pointer_release(lx, ly);
                    }
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
struct AppConnection {
    id: u32,
    kind: String, // "vault" | "calendar"
    display: String,
    value: String,
    profile_id: String,
    allow_in_aggregated: bool,
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

fn build_recording_list(recordings: &[AppRecording]) -> Vec<ephemeris_ui::RecordingEntry> {
    recordings
        .iter()
        .map(|r| ephemeris_ui::RecordingEntry {
            id: r.id.to_string().as_str().into(),
            title: r.title.as_str().into(),
            duration_label: format_rec_duration(r.duration_secs as u64).as_str().into(),
            date_label: format_rec_date(r.created_at, now_secs()).as_str().into(),
            has_transcription: r.transcription.is_some(),
        })
        .collect()
}

fn push_recording_list_comp(comp: &ephemeris_ui::EphemerisPage, recordings: &[AppRecording]) {
    comp.set_recording_list(
        Rc::new(slint::VecModel::from(build_recording_list(recordings))).into(),
    );
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
    let sample_rate = supported_config.sample_rate().0;
    let channels = supported_config.channels();
    let stream_config: cpal::StreamConfig = supported_config.clone().into();

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

    let err_fn = |e: cpal::StreamError| tracing::warn!("cpal input error: {e}");

    let stream = match supported_config.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &stream_config,
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
                &stream_config,
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
                &stream_config,
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

fn find_whisper_binary() -> Option<String> {
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
            Install: brew install whisper-cpp (macOS)\n\
            or build from https://github.com/ggerganov/whisper.cpp"
            .to_string();
    };
    let Some(model) = find_whisper_model() else {
        return "No whisper model found.\n\
            Go to Settings → Transcription and tap Download."
            .to_string();
    };
    let output = std::process::Command::new(&bin)
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

fn profile_badge_for(
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
                .map(|t| t.to_lowercase().contains(&q))
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
            ephemeris_ui::NoteListItem {
                id: n.id.to_string(),
                title: n.title.clone(),
                date_label: format_note_date(stamp, now),
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

fn notebook_export_title(notebooks: &[AppNotebook], notebook_id: Option<u32>) -> String {
    match notebook_id {
        None => "Root".to_string(),
        Some(id) => notebook_display_path(notebooks, id),
    }
}

/// Downsample a gray8 page into a small RGBA preview for the notes gallery.
fn note_preview_image(data_dir: &Path, note_id: u32) -> (bool, slint::Image) {
    const TW: u32 = 120;
    const TH: u32 = 90;
    let Some((w, h, gray)) = load_note_page(data_dir, note_id) else {
        return (false, slint::Image::default());
    };
    if w == 0 || h == 0 || gray.len() < (w * h) as usize {
        return (false, slint::Image::default());
    }
    let mut rgba = vec![0u8; (TW * TH * 4) as usize];
    for ty in 0..TH {
        for tx in 0..TW {
            let sx = tx * w / TW;
            let sy = ty * h / TH;
            let g = gray[(sy * w + sx) as usize];
            let i = ((ty * TW + tx) * 4) as usize;
            rgba[i] = g;
            rgba[i + 1] = g;
            rgba[i + 2] = g;
            rgba[i + 3] = 255;
        }
    }
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&rgba, TW, TH);
    (true, slint::Image::from_rgba8(buffer))
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

fn save_note_page(data_dir: &Path, note_id: u32, width: u32, height: u32, pixels: &[u8]) {
    let path = data_dir.join(format!("page_{note_id}.bin"));
    let mut buf = Vec::with_capacity(12 + pixels.len());
    buf.extend_from_slice(PAGE_BIN_MAGIC);
    buf.extend_from_slice(&width.to_le_bytes());
    buf.extend_from_slice(&height.to_le_bytes());
    buf.extend_from_slice(pixels);
    if let Err(e) = std::fs::write(&path, buf) {
        tracing::warn!("Failed to save note page {note_id}: {e}");
    }
}

/// Load a note page raster. Returns `(width, height, gray8)`.
/// Legacy files without a header are treated as width=800 when divisible.
fn load_note_page(data_dir: &Path, note_id: u32) -> Option<(u32, u32, Vec<u8>)> {
    let path = data_dir.join(format!("page_{note_id}.bin"));
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

/// Persist notes index and mirror markdown sidecars into the configured vault.
fn save_notes_index_and_sync_vault(
    data_dir: &Path,
    notes: &[AppNote],
    notebooks: &[AppNotebook],
    note_id_seq: u32,
    notebook_id_seq: u32,
    config: &Config,
) {
    save_notes_index(data_dir, notes, notebooks, note_id_seq, notebook_id_seq);
    sync_notes_export_to_vault(config, notes, notebooks, data_dir);
}

/// Export one note's PDF + markdown sidecar into the vault (used on stroke end).
fn export_single_note_to_vault(
    config: &Config,
    notes: &[AppNote],
    notebooks: &[AppNotebook],
    data_dir: &Path,
    note_id: u32,
) {
    let Some(note) = notes.iter().find(|n| n.id == note_id) else {
        return;
    };
    let Some(vault) = config.vault_path.as_ref() else {
        return;
    };
    let notebook_title = notebook_export_title(notebooks, note.notebook_id);
    let export = build_vault_note_export(config, vault, data_dir, note, &notebook_title);
    if let Err(e) =
        ephemeris_core::export_note_markdown(vault, &config.vault_export_subdir, &export)
    {
        tracing::warn!("Vault markdown export for note {note_id} failed: {e}");
    }
}

fn build_vault_note_export(
    config: &Config,
    vault: &Path,
    data_dir: &Path,
    note: &AppNote,
    notebook_title: &str,
) -> VaultNoteExport {
    let export_root = &config.vault_export_subdir;
    let nb_safe = ephemeris_core::vault_export::sanitize_component(notebook_title);
    let note_safe = ephemeris_core::vault_export::sanitize_component(&note.title);
    let pdf_abs = vault
        .join(export_root)
        .join(&nb_safe)
        .join(format!("{note_safe}.pdf"));

    let mut pdf_ok = false;
    if let Some((w, h, gray)) = load_note_page(data_dir, note.id) {
        let page = RasterPage {
            width: w,
            height: h,
            gray8: gray,
        };
        let ocr = RasterOcrTranscriber::from_parts(
            config.ocr_enabled,
            config.ocr_languages.clone(),
            config.ocr_tessdata.clone(),
        );
        let spans = ocr
            .transcribe_raster(w, h, &page.gray8)
            .unwrap_or_else(|e| {
                tracing::debug!("OCR for note {} skipped: {e}", note.id);
                Vec::new()
            });
        let searchable_text = note.text_content.clone().or_else(|| {
            let t = RasterOcrTranscriber::spans_to_text(&spans);
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        });
        let opts = PdfExportOptions {
            title: note.title.clone(),
            searchable_text,
            page_spans: if spans.is_empty() {
                vec![]
            } else {
                vec![spans]
            },
        };
        match export_raster_pages_to_pdf(&[page], &pdf_abs, &opts) {
            Ok(_) => pdf_ok = true,
            Err(e) => tracing::warn!("PDF export for note {} failed: {e}", note.id),
        }
    }

    let pdf_vault_rel = if pdf_ok || pdf_abs.exists() {
        Some(
            export_root
                .join(&nb_safe)
                .join(format!("{note_safe}.pdf"))
                .to_string_lossy()
                .replace('\\', "/"),
        )
    } else {
        None
    };

    VaultNoteExport {
        notebook_title: notebook_title.to_string(),
        note_title: note.title.clone(),
        note_id: note.id,
        created_at: note.created_at,
        text_content: note.text_content.clone(),
        pdf_vault_rel,
    }
}

fn sync_notes_export_to_vault(
    config: &Config,
    notes: &[AppNote],
    notebooks: &[AppNotebook],
    data_dir: &Path,
) {
    let Some(vault) = config.vault_path.as_ref() else {
        return;
    };
    let export_root = &config.vault_export_subdir;
    let payload: Vec<VaultNoteExport> = notes
        .iter()
        .map(|n| {
            let notebook_title = notebook_export_title(notebooks, n.notebook_id);
            build_vault_note_export(config, vault, data_dir, n, &notebook_title)
        })
        .collect();

    match sync_notes_to_vault(vault, export_root, &payload) {
        Ok(n) => tracing::debug!(
            "Synced {n} notes to vault export under {}",
            export_root.display()
        ),
        Err(e) => tracing::warn!("Vault note export sync failed: {e}"),
    }
}

/// Push see-text content for the active note into the UI.
fn push_note_text_for_active(ui: &ephemeris_ui::EphemerisUi, notes: &[AppNote], active_id: &str) {
    let text = notes
        .iter()
        .find(|n| n.id.to_string() == active_id)
        .and_then(|n| n.text_content.clone())
        .unwrap_or_default();
    ui.set_note_text_content(&text);
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

/// Build day events: `(title, time_label, all_day, col=0)`.
fn cal_day_events(
    year: i32,
    month: u32,
    day: u32,
    events: &[CalendarEvent],
) -> Vec<(String, String, bool, i32)> {
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
                "All day".to_string()
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
            (e.title.clone(), label, all_day, 0i32)
        })
        .collect();
    result.sort_by(|a, b| a.2.cmp(&b.2).reverse().then(a.1.cmp(&b.1)));
    result
}

/// Build week events: col = day-of-week offset from Monday (0=Mon).
type CalWeekEvent = (String, String, bool, i32);

fn cal_week_events(
    year: i32,
    month: u32,
    day: u32,
    events: &[CalendarEvent],
) -> (Vec<String>, Vec<CalWeekEvent>) {
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
                "All day".to_string()
            } else {
                let dt = ts_to_naive(e.start);
                format!("{:02}:{:02}", dt.hour(), dt.minute())
            };
            (e.title.clone(), label, all_day, col)
        })
        .collect();
    result.sort_by(|a, b| {
        a.3.cmp(&b.3)
            .then(a.2.cmp(&b.2).reverse())
            .then(a.1.cmp(&b.1))
    });
    (labels, result)
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
    profile_id: ProfileId,
) -> ephemeris_core::Result<Vec<Task>> {
    let extractor = MarkdownTaskExtractor::with_inbox(vault_path, profile_id, inbox_rel);
    TaskProvider::list(&extractor, profile_id)
}

/// Replace in-memory Obsidian tasks with a fresh read from the vault.
fn reload_obsidian_tasks(
    tasks: &mut Vec<Task>,
    vault_path: &std::path::Path,
    inbox_rel: &std::path::Path,
    profile_id: ProfileId,
) {
    match load_vault_tasks_with_inbox(vault_path, inbox_rel, profile_id) {
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
    let extractor =
        MarkdownTaskExtractor::with_inbox(vault, task.profile_id, &config.vault_tasks_inbox);
    if let Err(e) = TaskProvider::update(&extractor, task) {
        tracing::warn!("Obsidian task write-back failed for '{}': {e}", task.title);
    }
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
    let extractor = MarkdownTaskExtractor::with_inbox(vault, profile_id, &config.vault_tasks_inbox);
    match TaskProvider::create(&extractor, task) {
        Ok(()) => {
            reload_obsidian_tasks(tasks, vault, &config.vault_tasks_inbox, profile_id);
            true
        }
        Err(e) => {
            tracing::warn!("Obsidian task create failed: {e}");
            false
        }
    }
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

/// Refresh both the start-page ranked list and the full-task-list in the UI.
fn refresh_ui(ui: &EphemerisUi, tasks: &[Task], profile_id: ProfileId) {
    refresh_comp(&ui.clone_component(), tasks, profile_id);
}

/// Same as `refresh_ui` but operates on a cloned `EphemerisPage` component.
fn refresh_comp(comp: &ephemeris_ui::EphemerisPage, tasks: &[Task], profile_id: ProfileId) {
    let now = now_secs();

    // In All-profiles mode, home Tasks Ahead chips filter without switching profile.
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

    let ranked = rank_tasks(home_tasks, now);
    let task_entries: Vec<ephemeris_ui::TaskEntry> = ranked
        .iter()
        .take(HOME_TASK_LIMIT)
        .map(|t| {
            let title = strip_inline_markdown(&t.title);
            let (dl, ov) = format_due(t.due, now);
            ephemeris_ui::TaskEntry {
                id: t.id.0.to_string().as_str().into(),
                title: title.as_str().into(),
                priority_label: priority_label(t.priority).as_str().into(),
                due_label: dl.as_str().into(),
                overdue: ov,
            }
        })
        .collect();
    comp.set_task_list(Rc::new(slint::VecModel::from(task_entries)).into());
    push_full_task_list_comp(comp, tasks);
    let _ = profile_id;
}

/// Push the task-view list using current UI filter/sort/profile-chip/search.
fn push_full_task_list_comp(comp: &ephemeris_ui::EphemerisPage, all_tasks: &[Task]) {
    let global_profile = comp.get_active_profile_id().to_string();
    let chip = if global_profile.is_empty() {
        comp.get_task_profile_filter().to_string()
    } else {
        String::new()
    };
    let search = comp.get_task_search_query().to_string();
    let filter = comp.get_task_filter().to_string();
    let sort = comp.get_task_sort().to_string();
    let now = now_secs();

    let mut scoped = filter_tasks_by_profile(all_tasks, &global_profile);
    if !chip.is_empty() {
        scoped = filter_tasks_by_profile(&scoped, &chip);
    }
    scoped = filter_tasks_by_query(&scoped, &search);

    let entries = build_full_task_list(&scoped, now, &filter, &sort);
    comp.set_full_task_list(
        Rc::new(slint::VecModel::from(
            entries
                .iter()
                .map(
                    |(id, title, pl, pi, dl, ov, done)| ephemeris_ui::TaskViewEntry {
                        id: id.as_str().into(),
                        title: title.as_str().into(),
                        priority_label: pl.as_str().into(),
                        priority_int: *pi,
                        due_label: dl.as_str().into(),
                        overdue: *ov,
                        done: *done,
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

fn build_full_task_list(
    tasks: &[Task],
    now: u64,
    filter: &str,
    sort: &str,
) -> Vec<(String, String, String, i32, String, bool, bool)> {
    let mut v: Vec<_> = tasks
        .iter()
        .filter(|t| match filter {
            "active" => !t.done,
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
            (
                t.id.0.to_string(),
                strip_inline_markdown(&t.title),
                pl,
                pi,
                dl,
                ov,
                t.done,
            )
        })
        .collect();
    match sort {
        "due" => v.sort_by(|a, b| {
            // tasks with no due date sort last
            let due_a = if a.4.is_empty() { u64::MAX } else { 0 };
            let due_b = if b.4.is_empty() { u64::MAX } else { 0 };
            due_a.cmp(&due_b).then(a.5.cmp(&b.5))
        }),
        "title" => v.sort_by_key(|a| a.1.to_lowercase()),
        "added" => {} // preserve original order
        _ => {
            // priority, with open tasks before closed when mixed ("all")
            v.sort_by(|a, b| a.6.cmp(&b.6).then(a.3.cmp(&b.3)))
        }
    }
    v
}

fn priority_label(p: TaskPriority) -> String {
    match p {
        TaskPriority::High => "P0".to_string(),
        TaskPriority::Medium => "P1".to_string(),
        TaskPriority::Low => "P2".to_string(),
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

fn sample_tasks_split(work: ProfileId, personal: ProfileId) -> Vec<Task> {
    const DAY: u64 = 86_400;
    let now = now_secs();
    let mut tasks = Vec::new();

    let mut t = Task::new("Review quarterly OKRs", work);
    t.priority = TaskPriority::High;
    t.due = Some(now + DAY);
    tasks.push(t);

    let mut t = Task::new("Fix login redirect bug", work);
    t.priority = TaskPriority::High;
    t.due = Some(now.saturating_sub(DAY));
    tasks.push(t);

    let mut t = Task::new("Write architecture doc", work);
    t.priority = TaskPriority::Medium;
    t.due = Some(now + 3 * DAY);
    tasks.push(t);

    let mut t = Task::new("Book dentist appointment", personal);
    t.priority = TaskPriority::Medium;
    tasks.push(t);

    let mut t = Task::new("Plan weekend trip", personal);
    t.priority = TaskPriority::Low;
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
}
