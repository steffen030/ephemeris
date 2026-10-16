//! Obsidian-compatible Canvas: browser, board overview, focus ink, vault export.

use ephemeris_core::profile_manager::ProfileManager;
use ephemeris_core::{
    canvas_data_dir, crop_ink_preview, delete_canvas_files, export_canvas_json, gray8_to_png,
    hit_port, hit_resize_handle, hit_side, hit_test, load_canvas_doc, load_canvas_index,
    load_card_page_pixels, load_card_strokes, load_card_text, new_canvas_id, render_board,
    save_canvas_doc, save_canvas_index, save_card_page_pixels, save_card_strokes, save_card_text,
    snap_grid, view_for_fit, view_for_focus, zoom_view_at, AppCanvasFolder, AppCanvasMeta,
    BoardDrawOpts, CanvasDoc, CanvasEdge, CanvasIndex, CanvasNode, CanvasZoomPreset, Config,
    EdgeEnd, NodeSide, PageRecognitionCache, StrokePage, TextNode, VaultCanvasExport, Viewport,
    DEFAULT_EXPORT_SUBDIR, PORT_HIT_RADIUS, RECOGNITION_CACHE_FORMAT,
};
use ephemeris_ui::{
    CanvasCardOverlay, CanvasFolderListItem, CanvasListItem, EphemerisUi, FolderListItem,
};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::app::{
    begin_create_with_profile, build_cascading_transcriber, note_preview_from_gray,
    profile_badge_for, recognition_model_version_for, schedule_vault_git_push_for, AppConnection,
};

/// Left nav-rail section for Canvas (`nav-section` in main.slint).
const NAV_HOME: i32 = 0;
const NAV_CANVAS: i32 = 3;

const GRID: i32 = 20;
const CONNECT_MARGIN: f32 = 48.0;
const LONG_PRESS: Duration = Duration::from_millis(480);
/// Finger contacts on PineNote wobble well past 14px during a hold.
const LONG_PRESS_SLOP: f32 = 28.0;
/// Min interval between board re-renders while a drag / pinch is in flight.
/// Each render is a full-panel raster + e-ink refresh; rendering every touch
/// sample stalls the PineNote UI thread until GNOME offers force-quit.
const BOARD_GESTURE_FRAME: Duration = Duration::from_millis(120);

/// A finger / mouse press on the board, until it is released.
#[derive(Clone)]
pub(crate) struct BoardPress {
    t0: Instant,
    x: f32,
    y: f32,
    /// Card / group under the press point.
    hit: Option<String>,
    /// Left the long-press slop: the press is a drag (move or pan).
    moved: bool,
    /// The long-press action already ran for this press.
    long_pressed: bool,
}

/// Worker → UI mailbox: `(node_id, completed markdown)` per card.
type CardTexts = Arc<Mutex<Option<Vec<(String, String)>>>>;

/// Background handwriting → text run for some cards of one canvas.
struct CardRecogJob {
    canvas_id: u32,
    /// Cards waiting for text (shown as "Reading handwriting…").
    ids: Vec<String>,
    /// Filled once the worker is done.
    result: CardTexts,
}

/// Poll interval while a [`CardRecogJob`] runs.
const RECOG_POLL: Duration = Duration::from_millis(300);

/// Move-tool drag: screen start point + each dragged node's original position.
type MoveOrigin = ((f32, f32), Vec<(String, i32, i32)>);

/// Resize-handle drag: node id, screen start point, original `(w, h)`.
type ResizeDrag = (String, (f32, f32), (i32, i32));

/// Smallest card / group after a resize (board px).
const MIN_NODE_W: i32 = 80;
const MIN_NODE_H: i32 = 40;
/// Margin around cards when wrapping a selection in a group (board px).
const GROUP_PAD: i32 = 40;

/// Ink thumbnails keyed by node id: `(w, h, gray8)`.
type InkPreviews = HashMap<String, (u32, u32, Vec<u8>)>;

/// Browser thumbnails keyed by canvas id: `(updated_at, has_preview, image)`.
type BrowserThumbs = HashMap<u32, (u64, bool, slint::Image)>;

/// Board raster size for browser thumbnails (portrait, like the board).
const THUMB_W: u32 = 300;
const THUMB_H: u32 = 400;

pub(crate) struct CanvasSession {
    pub data_dir: PathBuf,
    pub index: RefCell<CanvasIndex>,
    pub active_folder_id: Cell<u32>,
    pub active_canvas_id: Cell<u32>,
    pub doc: RefCell<CanvasDoc>,
    pub selected_ids: RefCell<Vec<String>>,
    pub multi_select: Cell<bool>,
    pub port_drag: RefCell<Option<(String, NodeSide)>>,
    pub focus_node_id: RefCell<Option<String>>,
    pub focus_page_index: Cell<usize>,
    pub pan_last: Cell<Option<(f32, f32)>>,
    pub last_tap: RefCell<Option<(u64, f32, f32, String)>>,
    pub press: RefCell<Option<BoardPress>>,
    pub long_press_consumed: Cell<bool>,
    pub move_dragging: Cell<bool>,
    pub move_origin: RefCell<Option<MoveOrigin>>,
    pub resize_drag: RefCell<Option<ResizeDrag>>,
    /// Board point (snapped) where the Add popover was opened by long-press;
    /// `None` = menu Add → staggered spawn.
    add_anchor: Cell<Option<(i32, i32)>>,
    recog_job: RefCell<Option<CardRecogJob>>,
    /// Config snapshot source for the recognition worker.
    config: RefCell<Option<Rc<RefCell<Config>>>>,
    /// Card ink thumbnails for `ink_previews_canvas` (loaded once, not per frame).
    ink_previews: RefCell<InkPreviews>,
    ink_previews_canvas: Cell<Option<u32>>,
    last_board_render: Cell<Option<Instant>>,
    browser_thumbs: RefCell<BrowserThumbs>,
    pub sort_mode: RefCell<String>,
    pub view_mode: RefCell<String>,
    pub profile_filter: Rc<RefCell<String>>,
    pub profile_mgr: RefCell<Option<Rc<RefCell<ProfileManager>>>>,
    /// Board refresh + browser push callbacks set by [`wire_canvas`].
    board_refresh: RefCell<Option<Rc<dyn Fn()>>>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn format_canvas_date(ts: u64, now: u64) -> String {
    use chrono::{DateTime, Datelike, Local, Timelike, Utc};
    if ts == 0 {
        return String::new();
    }
    let dt = DateTime::<Utc>::from_timestamp(ts as i64, 0)
        .map(|u| u.with_timezone(&Local))
        .unwrap_or_else(|| Utc::now().with_timezone(&Local));
    let now_dt = DateTime::<Utc>::from_timestamp(now as i64, 0)
        .map(|u| u.with_timezone(&Local))
        .unwrap_or_else(|| Utc::now().with_timezone(&Local));
    let age_days = now.saturating_sub(ts) / 86_400;
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

impl CanvasSession {
    pub(crate) fn load() -> Self {
        let data_dir = canvas_data_dir();
        let _ = std::fs::create_dir_all(&data_dir);
        let index = load_canvas_index(&data_dir).unwrap_or_default();
        Self {
            data_dir,
            index: RefCell::new(index),
            active_folder_id: Cell::new(0),
            active_canvas_id: Cell::new(0),
            doc: RefCell::new(CanvasDoc::default()),
            selected_ids: RefCell::new(Vec::new()),
            multi_select: Cell::new(false),
            port_drag: RefCell::new(None),
            focus_node_id: RefCell::new(None),
            focus_page_index: Cell::new(0),
            pan_last: Cell::new(None),
            last_tap: RefCell::new(None),
            press: RefCell::new(None),
            long_press_consumed: Cell::new(false),
            move_dragging: Cell::new(false),
            move_origin: RefCell::new(None),
            resize_drag: RefCell::new(None),
            add_anchor: Cell::new(None),
            recog_job: RefCell::new(None),
            config: RefCell::new(None),
            ink_previews: RefCell::new(HashMap::new()),
            ink_previews_canvas: Cell::new(None),
            last_board_render: Cell::new(None),
            browser_thumbs: RefCell::new(HashMap::new()),
            sort_mode: RefCell::new("edited".into()),
            view_mode: RefCell::new("list".into()),
            profile_filter: Rc::new(RefCell::new(String::new())),
            profile_mgr: RefCell::new(None),
            board_refresh: RefCell::new(None),
        }
    }

    /// Profile for a new canvas/folder: parent folder's id, else active filter.
    fn create_target_profile(&self) -> String {
        let fid = self.active_folder_id.get();
        if fid != 0 {
            if let Some(p) = self
                .index
                .borrow()
                .folders
                .iter()
                .find(|f| f.id == fid)
                .map(|f| f.profile_id.clone())
            {
                if !p.is_empty() {
                    return p;
                }
            }
        }
        self.profile_filter.borrow().clone()
    }

    fn save_index(&self) {
        if let Err(e) = save_canvas_index(&self.data_dir, &self.index.borrow()) {
            tracing::warn!("save canvas index: {e}");
        }
    }

    pub(crate) fn save_doc(&self) {
        let id = self.active_canvas_id.get();
        if id == 0 {
            return;
        }
        if let Err(e) = save_canvas_doc(&self.data_dir, id, &self.doc.borrow()) {
            tracing::warn!("save canvas doc {id}: {e}");
        }
        if let Some(meta) = self
            .index
            .borrow_mut()
            .canvases
            .iter_mut()
            .find(|c| c.id == id)
        {
            meta.updated_at = now_secs();
        }
        self.save_index();
    }

    fn clear_selection(&self) {
        self.selected_ids.borrow_mut().clear();
        self.multi_select.set(false);
    }

    fn set_single_selection(&self, id: Option<String>) {
        let mut sel = self.selected_ids.borrow_mut();
        sel.clear();
        if let Some(id) = id {
            sel.push(id);
        }
        self.multi_select.set(false);
    }

    /// Where a new card goes: the long-press point, else staggered.
    fn spawn_point(&self) -> (i32, i32) {
        if let Some(p) = self.add_anchor.get() {
            return p;
        }
        let n = self.doc.borrow().nodes.len() as i32;
        let view = self.doc.borrow().view.clone();
        (
            snap_grid(view.x as i32 + 80 + n * 24, GRID),
            snap_grid(view.y as i32 + 80 + n * 24, GRID),
        )
    }

    fn sync_selection_ui(&self, ui: &EphemerisUi) {
        let page = ui.clone_component();
        let sel = self.selected_ids.borrow();
        let id = sel.first().cloned().unwrap_or_default();
        page.set_canvas_selected_id(id.as_str().into());
        page.set_canvas_selection_count(sel.len() as i32);
        page.set_canvas_multi_select(self.multi_select.get());
        let doc = self.doc.borrow();
        let (kind, group_label) = if self.multi_select.get() && !sel.is_empty() {
            ("multi", None)
        } else if sel.len() == 1 {
            match doc.find_node(&id) {
                Some(CanvasNode::Group(g)) => ("group", Some(g.label.clone().unwrap_or_default())),
                Some(CanvasNode::Text(_)) => ("text", None),
                Some(CanvasNode::File(_)) => ("file", None),
                Some(CanvasNode::Link(_)) => ("link", None),
                None => ("", None),
            }
        } else {
            ("", None)
        };
        // Only cards with ink can switch between handwriting and typed text.
        let previews = self.ink_previews.borrow();
        let inked = sel
            .iter()
            .filter(|i| previews.contains_key(*i) && !doc.is_typed(i))
            .count();
        let typed = sel
            .iter()
            .filter(|i| previews.contains_key(*i) && doc.is_typed(i))
            .count();
        let busy = self
            .recog_job
            .borrow()
            .as_ref()
            .is_some_and(|j| sel.iter().any(|i| j.ids.contains(i)));
        page.set_canvas_selected_kind(kind.into());
        page.set_canvas_selected_inked(inked as i32);
        page.set_canvas_selected_typed(typed as i32);
        page.set_canvas_hwr_busy(busy);
        page.set_canvas_selected_label(group_label.unwrap_or_default().into());
    }
}

/// Tear down board/browser/focus so other nav sections can show.
pub(crate) fn leave_canvas_session(
    ui: &EphemerisUi,
    session: &CanvasSession,
    config: &Rc<RefCell<Config>>,
    connections: &[AppConnection],
    return_to_browser: bool,
) {
    leave_focus_if_any(ui, session, config);
    let page = ui.clone_component();
    let was_board = page.get_show_canvas_board() || session.active_canvas_id.get() != 0;
    if was_board && session.active_canvas_id.get() != 0 {
        session.save_doc();
        export_active_canvas(session, &config.borrow(), connections);
    }
    page.set_show_canvas_board(false);
    page.set_canvas_focus_active(false);
    page.set_canvas_menu_open(false);
    page.set_canvas_add_visible(false);
    page.set_canvas_file_dialog_visible(false);
    page.set_canvas_link_dialog_visible(false);
    page.set_canvas_rename_visible(false);
    page.set_drawing_menu_open(false);
    *session.port_drag.borrow_mut() = None;
    *session.resize_drag.borrow_mut() = None;
    session.clear_selection();
    session.sync_selection_ui(ui);
    session.pan_last.set(None);
    session.move_dragging.set(false);
    *session.press.borrow_mut() = None;
    session.long_press_consumed.set(false);

    if return_to_browser {
        page.set_show_canvas_folder_list(true);
        page.set_show_canvas_list(session.active_folder_id.get() != 0);
        page.set_nav_section(NAV_CANVAS);
        push_browser(ui, session);
    } else {
        page.set_show_canvas_folder_list(false);
        page.set_show_canvas_list(false);
    }
}

/// Refresh the canvas folder/list browser (e.g. after profile switch).
pub(crate) fn refresh_browser(ui: &EphemerisUi, session: &CanvasSession) {
    push_browser(ui, session);
}

/// Create a canvas under `profile_id` and open it (picker dispatch).
pub(crate) fn create_canvas_with_profile(
    ui: &EphemerisUi,
    session: &CanvasSession,
    profile_id: &str,
) {
    let mut idx = session.index.borrow_mut();
    let id = idx.canvas_id_seq.max(1);
    idx.canvas_id_seq = id + 1;
    let folder = session.active_folder_id.get();
    idx.canvases.push(AppCanvasMeta {
        id,
        title: format!("Canvas {id}"),
        folder_id: if folder == 0 { None } else { Some(folder) },
        profile_id: profile_id.to_string(),
        created_at: now_secs(),
        updated_at: now_secs(),
    });
    drop(idx);
    session.save_index();
    let refresh = session
        .board_refresh
        .borrow()
        .clone()
        .unwrap_or_else(|| Rc::new(|| {}) as Rc<dyn Fn()>);
    open_canvas(ui, session, id, &refresh);
}

/// Create a canvas folder under `profile_id` (picker dispatch).
pub(crate) fn create_canvas_folder_with_profile(
    ui: &EphemerisUi,
    session: &CanvasSession,
    profile_id: &str,
) {
    let mut idx = session.index.borrow_mut();
    let id = idx.folder_id_seq.max(1);
    idx.folder_id_seq = id + 1;
    let parent = session.active_folder_id.get();
    idx.folders.push(AppCanvasFolder {
        id,
        title: format!("Folder {id}"),
        profile_id: profile_id.to_string(),
        parent_id: if parent == 0 { None } else { Some(parent) },
    });
    drop(idx);
    session.save_index();
    push_browser(ui, session);
}

/// Wire Canvas UI. Call once during app setup after notes PageBook slots are known.
pub(crate) fn wire_canvas(
    ui: Rc<EphemerisUi>,
    session: Rc<CanvasSession>,
    config: Rc<RefCell<Config>>,
    connections: Rc<RefCell<Vec<AppConnection>>>,
    active_profile_filter: Rc<RefCell<String>>,
    profile_mgr: Rc<RefCell<ProfileManager>>,
    next_page_index: usize,
) {
    session.focus_page_index.set(next_page_index);
    *session.config.borrow_mut() = Some(config.clone());
    *session.profile_filter.borrow_mut() = active_profile_filter.borrow().clone();
    *session.profile_mgr.borrow_mut() = Some(profile_mgr);
    push_browser(&ui, &session);

    let refresh = {
        let ui = ui.clone();
        let session = session.clone();
        Rc::new(move || refresh_board_image(&ui, &session)) as Rc<dyn Fn()>
    };
    *session.board_refresh.borrow_mut() = Some(refresh.clone());

    let leave_board = {
        let ui = ui.clone();
        let session = session.clone();
        let config = config.clone();
        let connections = connections.clone();
        Rc::new(move || {
            leave_canvas_session(&ui, &session, &config, &connections.borrow(), true);
        }) as Rc<dyn Fn()>
    };

    {
        let ui_rc = ui.clone();
        let session = session.clone();
        let leave = leave_board.clone();
        ui.on_nav_canvas_tapped(move || {
            let page = ui_rc.clone_component();
            if page.get_show_canvas_board() || session.focus_node_id.borrow().is_some() {
                leave();
            }
            session.active_folder_id.set(0);
            page.set_show_start_page(false);
            page.set_show_task_view(false);
            page.set_show_settings(false);
            page.set_show_filebrowser(false);
            page.set_show_note_list(false);
            page.set_show_notebook_list(false);
            page.set_show_calendar(false);
            page.set_show_recording_list(false);
            page.set_show_search(false);
            page.set_show_canvas_board(false);
            page.set_canvas_focus_active(false);
            page.set_nav_section(NAV_CANVAS);
            page.set_show_canvas_list(false);
            page.set_show_canvas_folder_list(true);
            ui_rc.set_canvas_search_query("");
            push_browser(&ui_rc, &session);
        });
    }

    {
        let ui_rc = ui.clone();
        let session = session.clone();
        ui.on_canvas_search_changed(move |_q| push_browser(&ui_rc, &session));
    }

    {
        let ui_rc = ui.clone();
        let session = session.clone();
        ui.on_canvas_folder_quick_new(move |id| {
            let Ok(fid) = id.parse::<u32>() else {
                return;
            };
            let folder_profile = session
                .index
                .borrow()
                .folders
                .iter()
                .find(|f| f.id == fid)
                .map(|f| f.profile_id.clone())
                .unwrap_or_default();
            let profile = if folder_profile.is_empty() {
                session.create_target_profile()
            } else {
                folder_profile
            };
            session.active_folder_id.set(fid);
            if profile.is_empty() {
                // All aggregator + unowned folder: ask which profile, like + Canvas.
                begin_create_with_profile(&ui_rc.clone_component(), "canvas", &profile);
            } else {
                create_canvas_with_profile(&ui_rc, &session, &profile);
            }
        });
    }

    {
        let ui_rc = ui.clone();
        let session = session.clone();
        ui.on_canvas_delete_tapped(move |id| {
            let Ok(cid) = id.parse::<u32>() else {
                return;
            };
            session.index.borrow_mut().canvases.retain(|c| c.id != cid);
            delete_canvas_files(&session.data_dir, cid);
            session.browser_thumbs.borrow_mut().remove(&cid);
            session.save_index();
            push_browser(&ui_rc, &session);
        });
    }

    {
        let ui_rc = ui.clone();
        let session = session.clone();
        ui.on_canvas_folder_delete_tapped(move |id| {
            let Ok(fid) = id.parse::<u32>() else {
                return;
            };
            let idx = session.index.borrow();
            let title = idx
                .folders
                .iter()
                .find(|f| f.id == fid)
                .map(|f| f.title.clone())
                .unwrap_or_else(|| "Folder".to_string());
            let doomed = canvas_folder_subtree(&idx, fid);
            let canvas_count = idx
                .canvases
                .iter()
                .filter(|c| c.folder_id.is_some_and(|f| doomed.contains(&f)))
                .count();
            let folder_count = doomed.len().saturating_sub(1);
            drop(idx);
            if canvas_count == 0 && folder_count == 0 {
                delete_canvas_folder(&session, fid);
                push_browser(&ui_rc, &session);
                return;
            }
            let mut parts = Vec::new();
            if canvas_count > 0 {
                parts.push(format!(
                    "{canvas_count} canvas{}",
                    if canvas_count == 1 { "" } else { "es" }
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
            ui_rc.show_folder_delete_confirm("canvas", &id, &title, &message);
        });
    }

    {
        let ui_rc = ui.clone();
        let session = session.clone();
        ui.on_canvas_folder_delete_confirm(move |id| {
            ui_rc.hide_notebook_delete_confirm();
            if let Ok(fid) = id.parse::<u32>() {
                delete_canvas_folder(&session, fid);
                push_browser(&ui_rc, &session);
            }
        });
    }

    {
        let ui_rc = ui.clone();
        let session = session.clone();
        ui.on_canvas_move_confirm(move |id, folder| {
            ui_rc.clone_component().set_note_move_visible(false);
            let (Ok(cid), Ok(fid)) = (id.parse::<u32>(), folder.parse::<u32>()) else {
                return;
            };
            if let Some(c) = session
                .index
                .borrow_mut()
                .canvases
                .iter_mut()
                .find(|c| c.id == cid)
            {
                c.folder_id = if fid == 0 { None } else { Some(fid) };
            }
            session.save_index();
            push_browser(&ui_rc, &session);
        });
    }

    {
        let ui_rc = ui.clone();
        let session = session.clone();
        ui.on_canvas_folder_tapped(move |id| {
            if let Ok(fid) = id.parse::<u32>() {
                session.active_folder_id.set(fid);
                let page = ui_rc.clone_component();
                page.set_show_canvas_folder_list(true);
                page.set_show_canvas_list(true);
                push_browser(&ui_rc, &session);
            }
        });
    }

    {
        let ui_rc = ui.clone();
        let session = session.clone();
        ui.on_canvas_browser_back_tapped(move || {
            let fid = session.active_folder_id.get();
            let page = ui_rc.clone_component();
            if fid == 0 {
                page.set_show_canvas_folder_list(false);
                page.set_show_canvas_list(false);
                page.set_show_start_page(true);
                page.set_nav_section(NAV_HOME);
                return;
            }
            let parent = session
                .index
                .borrow()
                .folders
                .iter()
                .find(|f| f.id == fid)
                .and_then(|f| f.parent_id)
                .unwrap_or(0);
            session.active_folder_id.set(parent);
            page.set_show_canvas_list(parent != 0);
            page.set_show_canvas_folder_list(true);
            push_browser(&ui_rc, &session);
        });
    }

    {
        let session = session.clone();
        let ui_rc = ui.clone();
        ui.on_canvas_folder_new_tapped(move || {
            let pf = session.create_target_profile();
            begin_create_with_profile(&ui_rc.clone_component(), "canvas-folder", &pf);
        });
    }

    {
        let ui_rc = ui.clone();
        let session = session.clone();
        ui.on_canvas_new_tapped(move || {
            let pf = session.create_target_profile();
            begin_create_with_profile(&ui_rc.clone_component(), "canvas", &pf);
        });
    }

    {
        let ui_rc = ui.clone();
        let session = session.clone();
        let refresh = refresh.clone();
        ui.on_canvas_tapped(move |id| {
            if let Ok(cid) = id.parse::<u32>() {
                open_canvas(&ui_rc, &session, cid, &refresh);
            }
        });
    }

    {
        let session = session.clone();
        let ui_rc = ui.clone();
        ui.on_canvas_sort_toggled(move || {
            let next = match session.sort_mode.borrow().as_str() {
                "edited" => "created",
                "created" => "title",
                _ => "edited",
            };
            *session.sort_mode.borrow_mut() = next.into();
            ui_rc.clone_component().set_canvas_sort_mode(next.into());
            push_browser(&ui_rc, &session);
        });
    }

    {
        let session = session.clone();
        let ui_rc = ui.clone();
        ui.on_canvas_view_mode_toggled(move || {
            let next = if session.view_mode.borrow().as_str() == "gallery" {
                "list"
            } else {
                "gallery"
            };
            *session.view_mode.borrow_mut() = next.into();
            ui_rc.clone_component().set_canvas_view_mode(next.into());
            push_browser(&ui_rc, &session);
        });
    }

    {
        let ui_rc = ui.clone();
        let session = session.clone();
        ui.on_canvas_rename_request(move |kind, id| {
            let page = ui_rc.clone_component();
            let title = match kind.as_str() {
                "folder" => session
                    .index
                    .borrow()
                    .folders
                    .iter()
                    .find(|f| f.id.to_string() == id)
                    .map(|f| f.title.clone())
                    .unwrap_or_default(),
                "group" => match session.doc.borrow().find_node(&id) {
                    Some(CanvasNode::Group(g)) => g.label.clone().unwrap_or_default(),
                    _ => return,
                },
                "board" => {
                    let cid = session.active_canvas_id.get();
                    session
                        .index
                        .borrow()
                        .canvases
                        .iter()
                        .find(|c| c.id == cid)
                        .map(|c| c.title.clone())
                        .unwrap_or_else(|| page.get_canvas_board_title().to_string())
                }
                _ => session
                    .index
                    .borrow()
                    .canvases
                    .iter()
                    .find(|c| c.id.to_string() == id)
                    .map(|c| c.title.clone())
                    .unwrap_or_default(),
            };
            page.set_canvas_rename_kind(kind.into());
            page.set_canvas_rename_id(id.into());
            page.set_canvas_rename_title(title.into());
            page.set_canvas_rename_visible(true);
        });
    }

    {
        let ui_rc = ui.clone();
        let session = session.clone();
        let refresh = refresh.clone();
        ui.on_canvas_rename_confirm(move |kind, id, title| {
            let page = ui_rc.clone_component();
            page.set_canvas_rename_visible(false);
            let title = title.trim().to_string();
            if kind == "group" {
                // Obsidian groups may be unlabeled: empty clears the label.
                let label = (!title.is_empty()).then_some(title);
                if let Some(CanvasNode::Group(g)) = session.doc.borrow_mut().find_node_mut(&id) {
                    g.label = label;
                }
                session.save_doc();
                session.sync_selection_ui(&ui_rc);
                refresh();
                return;
            }
            if title.is_empty() {
                return;
            }
            match kind.as_str() {
                "folder" => {
                    if let Ok(fid) = id.parse::<u32>() {
                        if let Some(f) = session
                            .index
                            .borrow_mut()
                            .folders
                            .iter_mut()
                            .find(|f| f.id == fid)
                        {
                            f.title = title;
                        }
                        session.save_index();
                        push_browser(&ui_rc, &session);
                    }
                }
                "board" => {
                    let cid = session.active_canvas_id.get();
                    if let Some(c) = session
                        .index
                        .borrow_mut()
                        .canvases
                        .iter_mut()
                        .find(|c| c.id == cid)
                    {
                        c.title = title.clone();
                        c.updated_at = now_secs();
                    }
                    session.save_index();
                    page.set_canvas_board_title(title.into());
                    push_browser(&ui_rc, &session);
                }
                _ => {
                    if let Ok(cid) = id.parse::<u32>() {
                        if let Some(c) = session
                            .index
                            .borrow_mut()
                            .canvases
                            .iter_mut()
                            .find(|c| c.id == cid)
                        {
                            c.title = title;
                            c.updated_at = now_secs();
                        }
                        session.save_index();
                        if session.active_canvas_id.get() == cid {
                            page.set_canvas_board_title(
                                session
                                    .index
                                    .borrow()
                                    .canvases
                                    .iter()
                                    .find(|c| c.id == cid)
                                    .map(|c| c.title.clone())
                                    .unwrap_or_default()
                                    .into(),
                            );
                        }
                        push_browser(&ui_rc, &session);
                    }
                }
            }
        });
    }

    {
        let ui_rc = ui.clone();
        ui.on_canvas_rename_cancel(move || {
            ui_rc.clone_component().set_canvas_rename_visible(false);
        });
    }

    {
        let ui_rc = ui.clone();
        let session = session.clone();
        let refresh = refresh.clone();
        let config = config.clone();
        ui.on_canvas_board_pointer(move |x, y, kind, _pointer| {
            let page = ui_rc.clone_component();
            if !page.get_show_canvas_board() || page.get_canvas_focus_active() {
                return;
            }
            let (bw, bh) = ui_rc.canvas_size();
            let vp = {
                let doc = session.doc.borrow();
                Viewport::from_view(&doc.view, bw, bh, &doc)
            };

            match kind.as_str() {
                "down" => {
                    session.long_press_consumed.set(false);
                    let hit = hit_test(&session.doc.borrow(), &vp, x, y);
                    *session.press.borrow_mut() = Some(BoardPress {
                        t0: Instant::now(),
                        x,
                        y,
                        hit,
                        moved: false,
                        long_pressed: false,
                    });

                    // Resize handle / port drag from single selection.
                    if !session.multi_select.get() && session.selected_ids.borrow().len() == 1 {
                        let id = session.selected_ids.borrow()[0].clone();
                        let size = {
                            let doc = session.doc.borrow();
                            hit_resize_handle(&doc, &vp, x, y, &id, PORT_HIT_RADIUS)
                                .then(|| doc.find_node(&id).map(|n| (n.width(), n.height())))
                                .flatten()
                        };
                        if let Some(size) = size {
                            *session.resize_drag.borrow_mut() = Some((id, (x, y), size));
                            *session.press.borrow_mut() = None;
                            return;
                        }
                        if let Some(side) =
                            hit_port(&session.doc.borrow(), &vp, x, y, &id, PORT_HIT_RADIUS)
                        {
                            *session.port_drag.borrow_mut() = Some((id, side));
                            *session.press.borrow_mut() = None;
                        }
                    }
                }
                "move" => {
                    let resize = session.resize_drag.borrow().clone();
                    if let Some((id, (sx, sy), (w0, h0))) = resize {
                        let (bx0, by0) = vp.screen_to_board(sx, sy);
                        let (bx1, by1) = vp.screen_to_board(x, y);
                        let w = snap_grid(w0 + (bx1 - bx0) as i32, GRID).max(MIN_NODE_W);
                        let h = snap_grid(h0 + (by1 - by0) as i32, GRID).max(MIN_NODE_H);
                        if let Some(n) = session.doc.borrow_mut().find_node_mut(&id) {
                            n.set_size(w, h);
                        }
                        refresh_board_throttled(&ui_rc, &session);
                        return;
                    }
                    if session.port_drag.borrow().is_some() {
                        return;
                    }
                    // Bind first: an if-let scrutinee's Ref lives through the body.
                    let press = session.press.borrow().clone();
                    let Some(press) = press else {
                        return;
                    };
                    if !press.moved {
                        let far = (x - press.x).abs() > LONG_PRESS_SLOP
                            || (y - press.y).abs() > LONG_PRESS_SLOP;
                        if !far {
                            return;
                        }
                        if let Some(p) = session.press.borrow_mut().as_mut() {
                            p.moved = true;
                        }
                        // Drag from the press point, so the slop is not lost.
                        match &press.hit {
                            Some(id) => begin_move(&ui_rc, &session, id, (press.x, press.y)),
                            // Long-press on empty board opened Add: no pan.
                            None if press.long_pressed => {}
                            None => session.pan_last.set(Some((press.x, press.y))),
                        }
                    }
                    if session.move_dragging.get() {
                        // Total delta from drag start: snapping per-sample deltas
                        // would round small finger moves back to the old cell.
                        if let Some(((sx, sy), origin)) = session.move_origin.borrow().as_ref() {
                            let (bx0, by0) = vp.screen_to_board(*sx, *sy);
                            let (bx1, by1) = vp.screen_to_board(x, y);
                            let dx = (bx1 - bx0) as i32;
                            let dy = (by1 - by0) as i32;
                            let mut doc = session.doc.borrow_mut();
                            for (id, ox, oy) in origin {
                                if let Some(n) = doc.find_node_mut(id) {
                                    n.set_pos(snap_grid(ox + dx, GRID), snap_grid(oy + dy, GRID));
                                }
                            }
                        }
                        refresh_board_throttled(&ui_rc, &session);
                    } else if let Some((lx, ly)) = session.pan_last.get() {
                        let mut doc = session.doc.borrow_mut();
                        doc.view.x -= (x - lx) / vp.scale.max(0.01);
                        doc.view.y -= (y - ly) / vp.scale.max(0.01);
                        session.pan_last.set(Some((x, y)));
                        drop(doc);
                        refresh_board_throttled(&ui_rc, &session);
                    }
                }
                "up" => {
                    let resized = session.resize_drag.borrow_mut().take();
                    if resized.is_some() {
                        // A short resize press must not also count as a tap.
                        session.long_press_consumed.set(true);
                        session.save_doc();
                        refresh();
                        return;
                    }
                    // Finish port-drag connect.
                    let port_drag = session.port_drag.borrow_mut().take();
                    if let Some((from_id, from_side)) = port_drag {
                        session.long_press_consumed.set(true);
                        let to_hit = hit_test(&session.doc.borrow(), &vp, x, y);
                        if let Some(to_id) = to_hit {
                            if to_id != from_id {
                                let (bx, by) = vp.screen_to_board(x, y);
                                let to_side =
                                    hit_side(&session.doc.borrow(), &to_id, bx, by, CONNECT_MARGIN)
                                        .unwrap_or(NodeSide::Left);
                                let edge = CanvasEdge {
                                    id: new_canvas_id(),
                                    from_node: from_id,
                                    from_side: Some(from_side),
                                    from_end: None,
                                    to_node: to_id,
                                    to_side: Some(to_side),
                                    to_end: Some(EdgeEnd::Arrow),
                                    color: None,
                                    label: None,
                                };
                                session.doc.borrow_mut().edges.push(edge);
                                session.save_doc();
                                refresh();
                            }
                        }
                        return;
                    }

                    let press = session.press.borrow_mut().take();
                    let panned = session.pan_last.take().is_some();
                    let moved = session.move_dragging.replace(false);
                    *session.move_origin.borrow_mut() = None;
                    if press.is_some_and(|p| p.moved) {
                        // A drag never doubles as a tap.
                        session.long_press_consumed.set(true);
                    }
                    if panned || moved {
                        session.save_doc();
                        // Present the final position skipped by throttling.
                        refresh();
                    }
                }
                "tap" => {
                    if session.long_press_consumed.replace(false) {
                        return;
                    }
                    handle_board_tap(&ui_rc, &session, &config, &refresh, x, y);
                }
                _ => {}
            }
        });
    }

    {
        let session = session.clone();
        let refresh = refresh.clone();
        let ui_rc = ui.clone();
        ui.on_canvas_zoom_preset(move |preset| {
            let (bw, bh) = ui_rc.canvas_size();
            let mut doc = session.doc.borrow_mut();
            match preset.as_str() {
                "fit" => doc.view = view_for_fit(&doc, bw, bh),
                "focus" => {
                    if let Some(id) = session.selected_ids.borrow().first().cloned() {
                        doc.view = view_for_focus(&doc, &id, bw, bh);
                    } else {
                        doc.view.zoom = CanvasZoomPreset::OneHundred;
                        doc.view.scale = 1.0;
                    }
                }
                _ => {
                    doc.view.zoom = CanvasZoomPreset::OneHundred;
                    doc.view.scale = 1.0;
                }
            }
            drop(doc);
            session.save_doc();
            refresh();
        });
    }

    {
        let ui_rc = ui.clone();
        let session = session.clone();
        let refresh = refresh.clone();
        ui.on_canvas_add_confirm(move |kind| {
            let page = ui_rc.clone_component();
            page.set_canvas_add_visible(false);
            match kind.as_str() {
                "file" => {
                    page.set_canvas_file_dialog_visible(true);
                    page.set_canvas_file_path("".into());
                    return;
                }
                "link" => {
                    page.set_canvas_link_dialog_visible(true);
                    page.set_canvas_link_url("https://".into());
                    return;
                }
                "group" => {
                    let (x, y) = session.spawn_point();
                    session.add_anchor.set(None);
                    let id = session.doc.borrow_mut().add_group(x, y, "Group");
                    session.set_single_selection(Some(id));
                    session.sync_selection_ui(&ui_rc);
                }
                _ => {
                    let (x, y) = session.spawn_point();
                    session.add_anchor.set(None);
                    let id = session.doc.borrow_mut().add_text_card(x, y);
                    session.set_single_selection(Some(id));
                    session.sync_selection_ui(&ui_rc);
                }
            }
            session.save_doc();
            refresh();
        });
    }

    {
        let ui_rc = ui.clone();
        let session = session.clone();
        let refresh = refresh.clone();
        ui.on_canvas_file_confirm(move || {
            let page = ui_rc.clone_component();
            let path = page.get_canvas_file_path().to_string();
            page.set_canvas_file_dialog_visible(false);
            if path.trim().is_empty() {
                return;
            }
            let (x, y) = session.spawn_point();
            session.add_anchor.set(None);
            let id = session.doc.borrow_mut().add_file_card(x, y, path);
            session.set_single_selection(Some(id));
            session.sync_selection_ui(&ui_rc);
            session.save_doc();
            refresh();
        });
    }

    {
        let ui_rc = ui.clone();
        let session = session.clone();
        let refresh = refresh.clone();
        ui.on_canvas_link_confirm(move || {
            let page = ui_rc.clone_component();
            let url = page.get_canvas_link_url().to_string();
            page.set_canvas_link_dialog_visible(false);
            if url.trim().is_empty() {
                return;
            }
            let (x, y) = session.spawn_point();
            session.add_anchor.set(None);
            let id = session.doc.borrow_mut().add_link_card(x, y, url);
            session.set_single_selection(Some(id));
            session.sync_selection_ui(&ui_rc);
            session.save_doc();
            refresh();
        });
    }

    {
        let ui_rc = ui.clone();
        let session = session.clone();
        let config = config.clone();
        let refresh = refresh.clone();
        ui.on_canvas_focus_done(move || {
            let node = session.focus_node_id.borrow().clone();
            leave_focus_if_any(&ui_rc, &session, &config);
            // A typed card's text follows its ink: re-read what was written.
            if let Some(id) = node.filter(|id| session.doc.borrow().is_typed(id)) {
                start_card_recognition(&ui_rc, &session, vec![id]);
            }
            let page = ui_rc.clone_component();
            page.set_canvas_focus_active(false);
            page.set_show_canvas_board(true);
            page.set_drawing_menu_open(false);
            refresh();
            // Ink previews reload in refresh(): the bar may now offer To text.
            session.sync_selection_ui(&ui_rc);
            ui_rc.request_screen_change();
        });
    }

    {
        let session = session.clone();
        let ui_rc = ui.clone();
        ui.on_canvas_add_open(move || {
            let page = ui_rc.clone_component();
            let (bw, bh) = ui_rc.canvas_size();
            session.add_anchor.set(None);
            page.set_canvas_add_x((bw as f32 - 240.0) / 2.0);
            page.set_canvas_add_y((bh as f32 - 260.0) / 2.0);
            page.set_canvas_add_visible(true);
        });
    }

    {
        let session = session.clone();
        let ui_rc = ui.clone();
        ui.on_canvas_convert_selected(move || {
            let ids: Vec<String> = {
                let sel = session.selected_ids.borrow();
                let previews = session.ink_previews.borrow();
                let doc = session.doc.borrow();
                sel.iter()
                    .filter(|id| previews.contains_key(*id) && !doc.is_typed(id))
                    .cloned()
                    .collect()
            };
            start_card_recognition(&ui_rc, &session, ids);
        });
    }

    {
        let session = session.clone();
        let ui_rc = ui.clone();
        let refresh = refresh.clone();
        ui.on_canvas_revert_selected(move || {
            let sel = session.selected_ids.borrow().clone();
            {
                let mut doc = session.doc.borrow_mut();
                for id in &sel {
                    doc.set_typed(id, false);
                }
            }
            session.save_doc();
            session.sync_selection_ui(&ui_rc);
            refresh();
        });
    }

    {
        let session = session.clone();
        let ui_rc = ui.clone();
        let config = config.clone();
        ui.on_canvas_open_selected(move || {
            let id = session.selected_ids.borrow().first().cloned();
            let is_text = id.as_deref().is_some_and(|id| {
                matches!(
                    session.doc.borrow().find_node(id),
                    Some(CanvasNode::Text(_))
                )
            });
            if let (Some(id), true) = (id, is_text) {
                enter_focus(&ui_rc, &session, &config, &id);
            }
        });
    }

    {
        let session = session.clone();
        let refresh = refresh.clone();
        let ui_rc = ui.clone();
        ui.on_canvas_group_selected(move || {
            let ids = session.selected_ids.borrow().clone();
            let gid = session
                .doc
                .borrow_mut()
                .add_group_around(&ids, GROUP_PAD, "Group");
            let Some(gid) = gid else {
                return;
            };
            session.multi_select.set(false);
            session.set_single_selection(Some(gid.clone()));
            session.sync_selection_ui(&ui_rc);
            session.save_doc();
            refresh();
            // Name it right away; Cancel keeps "Group".
            ui_rc
                .clone_component()
                .invoke_canvas_rename_request("group".into(), gid.as_str().into());
        });
    }

    {
        let session = session.clone();
        let refresh = refresh.clone();
        let ui_rc = ui.clone();
        ui.on_canvas_delete_selected(move || {
            let ids = session
                .selected_ids
                .borrow_mut()
                .drain(..)
                .collect::<Vec<_>>();
            if ids.is_empty() {
                return;
            }
            {
                let mut doc = session.doc.borrow_mut();
                for id in &ids {
                    doc.remove_node(id);
                }
            }
            session.multi_select.set(false);
            session.sync_selection_ui(&ui_rc);
            session.save_doc();
            refresh();
        });
    }

    {
        let session = session.clone();
        let refresh = refresh.clone();
        let ui_rc = ui.clone();
        ui.on_canvas_clear_selection(move || {
            session.clear_selection();
            session.sync_selection_ui(&ui_rc);
            refresh();
        });
    }

    {
        let leave = leave_board.clone();
        ui.on_canvas_leave_board(move || leave());
    }
}

fn open_canvas(ui: &EphemerisUi, session: &CanvasSession, id: u32, refresh: &Rc<dyn Fn()>) {
    session.active_canvas_id.set(id);
    *session.doc.borrow_mut() = load_canvas_doc(&session.data_dir, id);
    session.ink_previews_canvas.set(None);
    session.clear_selection();
    *session.port_drag.borrow_mut() = None;
    *session.resize_drag.borrow_mut() = None;
    *session.focus_node_id.borrow_mut() = None;
    let page = ui.clone_component();
    page.set_show_canvas_folder_list(false);
    page.set_show_canvas_list(false);
    page.set_show_canvas_board(true);
    page.set_canvas_focus_active(false);
    page.set_canvas_menu_open(false);
    page.set_drawing_menu_open(false);
    page.set_canvas_add_visible(false);
    session.sync_selection_ui(ui);
    let title = session
        .index
        .borrow()
        .canvases
        .iter()
        .find(|c| c.id == id)
        .map(|c| c.title.clone())
        .unwrap_or_else(|| format!("Canvas {id}"));
    page.set_canvas_board_title(title.into());
    refresh();
    ui.request_screen_change();
}

/// Start dragging `hit`: the whole selection when it is part of it, else just
/// that card (which becomes the selection). Groups carry the nodes fully
/// inside them (Obsidian).
fn begin_move(ui: &EphemerisUi, session: &CanvasSession, hit: &str, start: (f32, f32)) {
    let in_selection = session.selected_ids.borrow().iter().any(|s| s == hit);
    if !in_selection {
        session.set_single_selection(Some(hit.to_string()));
    }
    let mut ids = session.selected_ids.borrow().clone();
    session.sync_selection_ui(ui);
    let doc = session.doc.borrow();
    for gid in ids.clone() {
        for m in doc.group_members(&gid) {
            if !ids.contains(&m) {
                ids.push(m);
            }
        }
    }
    let origin = ids
        .into_iter()
        .filter_map(|id| {
            let n = doc.find_node(&id)?;
            let (nx, ny) = (n.x(), n.y());
            Some((id, nx, ny))
        })
        .collect();
    drop(doc);
    *session.move_origin.borrow_mut() = Some((start, origin));
    session.move_dragging.set(true);
}

/// Run the long-press action once a still press has been held for
/// [`LONG_PRESS`]: on a card it joins the multi-selection, on empty board it
/// opens the Add popover at that spot. Polled from the event loop (Slint
/// timers do not run on this platform). Returns true when the UI changed.
pub(crate) fn poll_board_long_press(ui: &EphemerisUi, session: &CanvasSession) -> bool {
    let press = session.press.borrow().clone();
    let Some(press) = press else {
        return false;
    };
    if press.moved || press.long_pressed || press.t0.elapsed() < LONG_PRESS {
        return false;
    }
    let page = ui.clone_component();
    if !page.get_show_canvas_board() || page.get_canvas_focus_active() {
        // Board went away mid-press: drop it so next_wake stops firing.
        *session.press.borrow_mut() = None;
        return false;
    }
    if let Some(p) = session.press.borrow_mut().as_mut() {
        p.long_pressed = true;
    }
    // The release that follows must not also count as a tap.
    session.long_press_consumed.set(true);
    match press.hit {
        Some(id) => {
            // Additive: keeps the current selection (a tapped card, or an
            // existing multi-select) and adds this one.
            session.multi_select.set(true);
            let mut sel = session.selected_ids.borrow_mut();
            if !sel.contains(&id) {
                sel.push(id);
            }
            drop(sel);
            session.sync_selection_ui(ui);
            refresh_board_image(ui, session);
        }
        None => {
            let (bw, bh) = ui.canvas_size();
            let (bx, by) = {
                let doc = session.doc.borrow();
                Viewport::from_view(&doc.view, bw, bh, &doc).screen_to_board(press.x, press.y)
            };
            session.add_anchor.set(Some((
                snap_grid(bx as i32, GRID),
                snap_grid(by as i32, GRID),
            )));
            page.set_canvas_menu_open(false);
            page.set_canvas_add_x(press.x + 12.0);
            page.set_canvas_add_y(press.y + 12.0);
            page.set_canvas_add_visible(true);
        }
    }
    true
}

/// Next time the event loop must wake for the canvas (pending long-press or
/// a running handwriting conversion).
pub(crate) fn next_wake(session: &CanvasSession) -> Option<Instant> {
    let press = session
        .press
        .borrow()
        .as_ref()
        .filter(|p| !p.moved && !p.long_pressed)
        .map(|p| p.t0 + LONG_PRESS);
    let recog = session
        .recog_job
        .borrow()
        .as_ref()
        .map(|_| Instant::now() + RECOG_POLL);
    match (press, recog) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Recognise the handwriting of `ids` (text cards of the active canvas) on a
/// worker thread and show them as typed text. One job at a time.
fn start_card_recognition(ui: &EphemerisUi, session: &CanvasSession, ids: Vec<String>) {
    if ids.is_empty() || session.recog_job.borrow().is_some() {
        return;
    }
    let Some(config) = session.config.borrow().as_ref().map(|c| c.borrow().clone()) else {
        return;
    };
    let canvas_id = session.active_canvas_id.get();
    {
        let mut doc = session.doc.borrow_mut();
        for id in &ids {
            doc.set_typed(id, true);
        }
    }
    session.save_doc();
    let result = Arc::new(Mutex::new(None));
    let mailbox = Arc::clone(&result);
    let data_dir = session.data_dir.clone();
    let job_ids = ids.clone();
    std::thread::spawn(move || {
        let version = recognition_model_version_for(&config);
        let cascade = build_cascading_transcriber(&config);
        let mut out = Vec::new();
        for id in job_ids {
            let strokes = load_card_strokes(&data_dir, canvas_id, &id).strokes;
            let mut cache = load_card_text(&data_dir, canvas_id, &id)
                .unwrap_or_else(|| PageRecognitionCache::new(&version));
            cache.sync_strokes(&strokes, None, &version);
            if cache.has_pending() {
                let _ = cache.recognise_pending(&strokes, None, &|| false, |line, idx| {
                    cascade.transcribe_line(line, idx)
                });
            }
            if let Err(e) = save_card_text(&data_dir, canvas_id, &id, &cache) {
                tracing::warn!("save card text {canvas_id}/{id}: {e}");
            }
            out.push((id, cache.completed_markdown()));
        }
        if let Ok(mut guard) = mailbox.lock() {
            *guard = Some(out);
        }
    });
    *session.recog_job.borrow_mut() = Some(CardRecogJob {
        canvas_id,
        ids,
        result,
    });
    session.sync_selection_ui(ui);
    refresh_board_image(ui, session);
}

/// Apply a finished [`CardRecogJob`]. Returns true when the UI changed.
pub(crate) fn poll_card_recognition(ui: &EphemerisUi, session: &CanvasSession) -> bool {
    let done = session
        .recog_job
        .borrow()
        .as_ref()
        .and_then(|j| j.result.lock().ok()?.take());
    let Some(texts) = done else {
        return false;
    };
    let Some(job) = session.recog_job.borrow_mut().take() else {
        return false;
    };
    let apply = |doc: &mut CanvasDoc| {
        for (id, md) in &texts {
            if let Some(CanvasNode::Text(TextNode { text, .. })) = doc.find_node_mut(id) {
                *text = md.clone();
            }
        }
    };
    if job.canvas_id == session.active_canvas_id.get() {
        apply(&mut session.doc.borrow_mut());
        session.save_doc();
        session.sync_selection_ui(ui);
        refresh_board_image(ui, session);
    } else {
        // Board was left while recognising: update the stored document.
        let mut doc = load_canvas_doc(&session.data_dir, job.canvas_id);
        apply(&mut doc);
        if let Err(e) = save_canvas_doc(&session.data_dir, job.canvas_id, &doc) {
            tracing::warn!("save canvas doc {}: {e}", job.canvas_id);
        }
    }
    true
}

/// Cancel pan / move / long-press when a second finger starts a pinch.
pub(crate) fn cancel_board_pointer_gesture(session: &CanvasSession) {
    session.pan_last.set(None);
    session.move_dragging.set(false);
    *session.move_origin.borrow_mut() = None;
    session.long_press_consumed.set(false);
    *session.press.borrow_mut() = None;
    *session.port_drag.borrow_mut() = None;
    *session.resize_drag.borrow_mut() = None;
}

/// Continuous zoom around a screen point (pinch / trackpad). Refreshes the board
/// image at most every [`BOARD_GESTURE_FRAME`]; call [`refresh_board_now`] when
/// the gesture ends.
pub(crate) fn apply_board_zoom_factor(
    ui: &EphemerisUi,
    session: &CanvasSession,
    factor: f32,
    screen_x: f32,
    screen_y: f32,
) {
    if !factor.is_finite() || factor <= 0.0 || (factor - 1.0).abs() < 1e-6 {
        return;
    }
    let (bw, bh) = ui.canvas_size();
    {
        let mut doc = session.doc.borrow_mut();
        let mut view = doc.view.clone();
        zoom_view_at(&mut view, &doc, bw, bh, factor, screen_x, screen_y);
        doc.view = view;
    }
    refresh_board_throttled(ui, session);
}

/// Render the board immediately (end of a throttled pinch / drag).
pub(crate) fn refresh_board_now(ui: &EphemerisUi, session: &CanvasSession) {
    refresh_board_image(ui, session);
}

/// Re-render unless the last board render was under [`BOARD_GESTURE_FRAME`] ago.
fn refresh_board_throttled(ui: &EphemerisUi, session: &CanvasSession) {
    if let Some(t) = session.last_board_render.get() {
        if t.elapsed() < BOARD_GESTURE_FRAME {
            return;
        }
    }
    refresh_board_image(ui, session);
}

/// Load card ink thumbnails once per canvas (disk I/O must stay off the
/// per-frame path — the board re-renders on every drag / pinch frame).
fn ensure_ink_previews(session: &CanvasSession) {
    let canvas_id = session.active_canvas_id.get();
    if session.ink_previews_canvas.get() == Some(canvas_id) {
        return;
    }
    let doc = session.doc.borrow();
    let mut previews = session.ink_previews.borrow_mut();
    previews.clear();
    for n in &doc.nodes {
        if let CanvasNode::Text(t) = n {
            if let Some((w, h, px)) = load_card_page_pixels(&session.data_dir, canvas_id, &t.id) {
                // Cropped to the ink so it scales up to fill the card; None = blank page.
                if let Some(preview) = crop_ink_preview(w, h, &px) {
                    previews.insert(t.id.clone(), preview);
                }
            }
        }
    }
    session.ink_previews_canvas.set(Some(canvas_id));
}

fn refresh_board_image(ui: &EphemerisUi, session: &CanvasSession) {
    ensure_ink_previews(session);
    session.last_board_render.set(Some(Instant::now()));
    let (bw, bh) = ui.canvas_size();
    let doc = session.doc.borrow();
    let vp = Viewport::from_view(&doc.view, bw, bh, &doc);
    let selected = session.selected_ids.borrow().clone();
    let show_ports = !session.multi_select.get() && selected.len() == 1;
    let ink_previews = session.ink_previews.borrow();
    let opts = BoardDrawOpts {
        selected_ids: &selected,
        show_ports,
        ink_previews: &ink_previews,
        text_overlay: true,
        typed_cards: &doc.typed_cards,
    };
    let gray = render_board(&doc, &vp, bw, bh, &opts);
    let reading: Vec<String> = session
        .recog_job
        .borrow()
        .as_ref()
        .map(|j| j.ids.clone())
        .unwrap_or_default();
    let cards = card_overlays(&doc, &vp, (bw, bh), &ink_previews, &reading);
    let empty = doc.nodes.is_empty();
    drop(ink_previews);
    drop(doc);
    ui.set_canvas_board_gray8(bw, bh, &gray);
    ui.set_canvas_text_cards(&cards);
    ui.clone_component().set_canvas_board_empty(empty);
}

/// Text cards whose body the UI draws: typed (recognised) cards and cards
/// without ink. Cards off screen or too small to read are skipped.
fn card_overlays(
    doc: &CanvasDoc,
    vp: &Viewport,
    (bw, bh): (u32, u32),
    ink_previews: &InkPreviews,
    reading: &[String],
) -> Vec<CanvasCardOverlay> {
    // Inset inside the 2–3px card border.
    const INSET: f32 = 3.0;
    let mut out = Vec::new();
    for n in &doc.nodes {
        let CanvasNode::Text(t) = n else {
            continue;
        };
        let typed = doc.is_typed(&t.id);
        if ink_previews.contains_key(&t.id) && !typed {
            continue;
        }
        let (sx, sy) = vp.board_to_screen(t.x as f32, t.y as f32);
        let (sw, sh) = (t.width as f32 * vp.scale, t.height as f32 * vp.scale);
        let off_screen = sx + sw < 0.0 || sy + sh < 0.0 || sx > bw as f32 || sy > bh as f32;
        if off_screen || sw < 24.0 || sh < 16.0 {
            continue;
        }
        let placeholder = if reading.contains(&t.id) {
            "Reading handwriting…"
        } else if typed {
            "No text recognised — tap Show ink to go back"
        } else {
            "Empty"
        };
        out.push(CanvasCardOverlay {
            id: t.id.clone(),
            x: sx + INSET,
            y: sy + INSET,
            w: sw - 2.0 * INSET,
            h: sh - 2.0 * INSET,
            scale: vp.scale,
            markdown: if reading.contains(&t.id) {
                String::new()
            } else {
                t.text.clone()
            },
            placeholder: placeholder.into(),
        });
    }
    out
}

/// `root` plus every nested child folder id.
fn canvas_folder_subtree(idx: &CanvasIndex, root: u32) -> Vec<u32> {
    let mut out = vec![root];
    let mut i = 0;
    while i < out.len() {
        let parent = out[i];
        out.extend(
            idx.folders
                .iter()
                .filter(|f| f.parent_id == Some(parent))
                .map(|f| f.id),
        );
        i += 1;
    }
    out
}

/// Delete a folder, its subfolders and every canvas inside them.
fn delete_canvas_folder(session: &CanvasSession, fid: u32) {
    let mut idx = session.index.borrow_mut();
    let doomed = canvas_folder_subtree(&idx, fid);
    let parent = idx
        .folders
        .iter()
        .find(|f| f.id == fid)
        .and_then(|f| f.parent_id)
        .unwrap_or(0);
    let doomed_canvases: Vec<u32> = idx
        .canvases
        .iter()
        .filter(|c| c.folder_id.is_some_and(|f| doomed.contains(&f)))
        .map(|c| c.id)
        .collect();
    idx.canvases.retain(|c| !doomed_canvases.contains(&c.id));
    idx.folders.retain(|f| !doomed.contains(&f.id));
    drop(idx);
    for cid in &doomed_canvases {
        delete_canvas_files(&session.data_dir, *cid);
        session.browser_thumbs.borrow_mut().remove(cid);
    }
    if doomed.contains(&session.active_folder_id.get()) {
        session.active_folder_id.set(parent);
    }
    session.save_index();
}

/// Board overview thumbnail for the browser, cached until the canvas changes.
fn canvas_thumbnail(session: &CanvasSession, meta: &AppCanvasMeta) -> (bool, slint::Image) {
    let stamp = meta.updated_at.max(meta.created_at);
    if let Some((s, has, img)) = session.browser_thumbs.borrow().get(&meta.id) {
        if *s == stamp {
            return (*has, img.clone());
        }
    }
    let doc = load_canvas_doc(&session.data_dir, meta.id);
    let (has, img) = if doc.nodes.is_empty() {
        (false, slint::Image::default())
    } else {
        let view = view_for_fit(&doc, THUMB_W, THUMB_H);
        let vp = Viewport::from_view(&view, THUMB_W, THUMB_H, &doc);
        let no_ink = HashMap::new();
        let opts = BoardDrawOpts {
            selected_ids: &[],
            show_ports: false,
            ink_previews: &no_ink,
            text_overlay: false,
            typed_cards: &doc.typed_cards,
        };
        let gray = render_board(&doc, &vp, THUMB_W, THUMB_H, &opts);
        note_preview_from_gray(THUMB_W, THUMB_H, &gray)
    };
    session
        .browser_thumbs
        .borrow_mut()
        .insert(meta.id, (stamp, has, img.clone()));
    (has, img)
}

fn push_browser(ui: &EphemerisUi, session: &CanvasSession) {
    let fid = session.active_folder_id.get();
    let profile = session.profile_filter.borrow().clone();
    let sort = session.sort_mode.borrow().clone();
    let view = session.view_mode.borrow().clone();
    let query = ui.get_canvas_search_query().trim().to_lowercase();
    let now = now_secs();
    let show_badges = profile.is_empty();

    // Snapshot badge fields while holding the manager borrow briefly.
    let badge_for = |profile_id: &str| -> (bool, String, bool, slint::Image) {
        let mgr_slot = session.profile_mgr.borrow();
        match mgr_slot.as_ref() {
            Some(rc) => {
                let mgr = rc.borrow();
                profile_badge_for(profile_id, show_badges, &mgr)
            }
            None => (false, String::new(), false, slint::Image::default()),
        }
    };

    let idx = session.index.borrow();

    let title = if fid == 0 {
        "Canvas".to_string()
    } else {
        idx.folders
            .iter()
            .find(|f| f.id == fid)
            .map(|f| f.title.clone())
            .unwrap_or_else(|| "Canvas".into())
    };
    ui.set_canvas_browser_title(&title);
    ui.set_canvas_at_root(fid == 0);
    let page = ui.clone_component();
    page.set_canvas_sort_mode(sort.clone().into());
    page.set_canvas_view_mode(view.into());

    let mut folders: Vec<CanvasFolderListItem> = idx
        .folders
        .iter()
        .filter(|f| f.parent_id.unwrap_or(0) == fid)
        .filter(|f| profile.is_empty() || f.profile_id.is_empty() || f.profile_id == profile)
        .map(|f| {
            let child_folders = idx
                .folders
                .iter()
                .filter(|c| c.parent_id == Some(f.id))
                .count() as i32;
            let child_canvases = idx
                .canvases
                .iter()
                .filter(|c| c.folder_id == Some(f.id))
                .count() as i32;
            let (show_profile, profile_icon_char, has_custom_profile_icon, custom_profile_icon) =
                badge_for(&f.profile_id);
            CanvasFolderListItem {
                id: f.id.to_string(),
                title: f.title.clone(),
                canvas_count: child_canvases,
                folder_count: child_folders,
                show_profile,
                profile_icon_char,
                has_custom_profile_icon,
                custom_profile_icon,
            }
        })
        .collect();
    folders.sort_by_key(|a| a.title.to_lowercase());

    let mut canvases: Vec<(AppCanvasMeta, CanvasListItem)> = idx
        .canvases
        .iter()
        .filter(|c| c.folder_id.unwrap_or(0) == fid)
        .filter(|c| profile.is_empty() || c.profile_id.is_empty() || c.profile_id == profile)
        .filter(|c| query.is_empty() || c.title.to_lowercase().contains(&query))
        .map(|c| {
            let (show_profile, profile_icon_char, has_custom_profile_icon, custom_profile_icon) =
                badge_for(&c.profile_id);
            let (has_preview, preview) = canvas_thumbnail(session, c);
            (
                c.clone(),
                CanvasListItem {
                    id: c.id.to_string(),
                    title: c.title.clone(),
                    date_label: format_canvas_date(c.updated_at.max(c.created_at), now),
                    has_preview,
                    preview,
                    show_profile,
                    profile_icon_char,
                    has_custom_profile_icon,
                    custom_profile_icon,
                },
            )
        })
        .collect();
    match sort.as_str() {
        "title" => canvases.sort_by_key(|a| a.0.title.to_lowercase()),
        "created" => canvases.sort_by_key(|a| std::cmp::Reverse(a.0.created_at)),
        _ => canvases.sort_by_key(|a| std::cmp::Reverse(a.0.updated_at)),
    }
    let canvases: Vec<CanvasListItem> = canvases.into_iter().map(|(_, e)| e).collect();

    // Move dialog: every folder visible under the current profile filter.
    let mut move_targets: Vec<FolderListItem> = idx
        .folders
        .iter()
        .filter(|f| profile.is_empty() || f.profile_id.is_empty() || f.profile_id == profile)
        .map(|f| FolderListItem {
            id: f.id.to_string(),
            title: f.title.clone(),
            note_count: 0,
            folder_count: 0,
            show_profile: false,
            profile_icon_char: String::new(),
            has_custom_profile_icon: false,
            custom_profile_icon: slint::Image::default(),
        })
        .collect();
    move_targets.sort_by_key(|f| f.title.to_lowercase());
    ui.set_canvas_move_folder_list(&move_targets);

    ui.set_canvas_folder_list(&folders);
    ui.set_canvas_list(&canvases);
}

fn handle_board_tap(
    ui: &EphemerisUi,
    session: &CanvasSession,
    config: &Rc<RefCell<Config>>,
    refresh: &Rc<dyn Fn()>,
    x: f32,
    y: f32,
) {
    let (bw, bh) = ui.canvas_size();
    let doc = session.doc.borrow();
    let vp = Viewport::from_view(&doc.view, bw, bh, &doc);
    let hit = hit_test(&doc, &vp, x, y);
    drop(doc);

    let now = now_ms();
    let is_dbl = if let (Some((t, px, py, prev_id)), Some(id)) =
        (session.last_tap.borrow().clone(), hit.as_ref())
    {
        prev_id == *id
            && now.saturating_sub(t) < 450
            && (x - px).abs() < 24.0
            && (y - py).abs() < 24.0
    } else {
        false
    };

    if let Some(id) = hit {
        if session.multi_select.get() {
            let mut sel = session.selected_ids.borrow_mut();
            if let Some(pos) = sel.iter().position(|s| s == &id) {
                sel.remove(pos);
            } else {
                sel.push(id.clone());
            }
            if sel.is_empty() {
                session.multi_select.set(false);
            }
            drop(sel);
            session.sync_selection_ui(ui);
            *session.last_tap.borrow_mut() = Some((now, x, y, id));
            refresh();
            return;
        }

        session.set_single_selection(Some(id.clone()));
        session.sync_selection_ui(ui);
        *session.last_tap.borrow_mut() = Some((now, x, y, id.clone()));
        if is_dbl {
            let (is_text, is_group) = match session.doc.borrow().find_node(&id) {
                Some(CanvasNode::Text(_)) => (true, false),
                Some(CanvasNode::Group(_)) => (false, true),
                _ => (false, false),
            };
            if is_group {
                refresh();
                ui.clone_component()
                    .invoke_canvas_rename_request("group".into(), id.as_str().into());
                return;
            }
            if is_text {
                enter_focus(ui, session, config, &id);
                return;
            }
        }
        refresh();
    } else {
        session.clear_selection();
        session.sync_selection_ui(ui);
        *session.last_tap.borrow_mut() = None;
        refresh();
    }
}

fn enter_focus(
    ui: &EphemerisUi,
    session: &CanvasSession,
    _config: &Rc<RefCell<Config>>,
    node_id: &str,
) {
    *session.focus_node_id.borrow_mut() = Some(node_id.to_string());
    let page_idx = session.focus_page_index.get();
    let canvas_id = session.active_canvas_id.get();
    let strokes = load_card_strokes(&session.data_dir, canvas_id, node_id).strokes;
    ui.restore_page_strokes(page_idx, strokes);
    if let Some((w, h, px)) = load_card_page_pixels(&session.data_dir, canvas_id, node_id) {
        ui.restore_page_pixels(page_idx, w, h, px);
    }
    ui.navigate_to_page(page_idx);
    ui.clear_strokes_dirty();

    if let Some(cache) = load_card_text(&session.data_dir, canvas_id, node_id) {
        ui.set_see_text_rows(&ephemeris_core::see_text::rows_from_cache(&cache));
    }

    let page = ui.clone_component();
    // Keep board visible under the focus modal (dimmed veil in Slint).
    page.set_show_canvas_board(true);
    page.set_canvas_focus_active(true);
    page.set_canvas_menu_open(false);
    page.set_drawing_menu_open(false);
    page.set_active_note_id(format!("canvas:{canvas_id}:{node_id}").into());
    let board_title = page.get_canvas_board_title().to_string();
    page.set_page_title(format!("{board_title} · Text").into());
    ui.request_screen_change();
}

fn leave_focus_if_any(ui: &EphemerisUi, session: &CanvasSession, config: &Rc<RefCell<Config>>) {
    let Some(node_id) = session.focus_node_id.borrow_mut().take() else {
        return;
    };
    let canvas_id = session.active_canvas_id.get();
    ui.save_current_page();
    let strokes = ui.get_page_strokes();
    let _ = save_card_strokes(
        &session.data_dir,
        canvas_id,
        &node_id,
        &StrokePage {
            strokes: strokes.clone(),
        },
    );
    let (iw, ih) = ui.ink_canvas_size();
    let pixels = ui.get_committed_pixels();
    let _ = save_card_page_pixels(&session.data_dir, canvas_id, &node_id, iw, ih, &pixels);
    session.ink_previews_canvas.set(None);

    let version = recognition_model_version_for(&config.borrow());
    let mut cache = load_card_text(&session.data_dir, canvas_id, &node_id)
        .unwrap_or_else(|| PageRecognitionCache::new(&version));
    let _ = cache.sync_strokes(&strokes, None, &version);
    let md = cache.completed_markdown();
    let _ = save_card_text(&session.data_dir, canvas_id, &node_id, &cache);
    if let Some(CanvasNode::Text(TextNode { text, .. })) =
        session.doc.borrow_mut().find_node_mut(&node_id)
    {
        if !md.is_empty() {
            *text = md;
        }
    }
    session.save_doc();
    ui.clone_component().set_note_see_text(false);
    let _ = RECOGNITION_CACHE_FORMAT;
}

fn export_active_canvas(session: &CanvasSession, config: &Config, connections: &[AppConnection]) {
    let canvas_id = session.active_canvas_id.get();
    if canvas_id == 0 {
        return;
    }
    let meta = session
        .index
        .borrow()
        .canvases
        .iter()
        .find(|c| c.id == canvas_id)
        .cloned();
    let Some(meta) = meta else {
        return;
    };
    let profile = meta.profile_id.clone();
    let vault = connections
        .iter()
        .find(|c| c.kind == "vault" && (profile.is_empty() || c.profile_id == profile))
        .map(|c| PathBuf::from(&c.value));
    let Some(vault) = vault else {
        return;
    };
    let export_subdir = if config.vault_export_subdir.is_empty() {
        PathBuf::from(DEFAULT_EXPORT_SUBDIR)
    } else {
        PathBuf::from(&config.vault_export_subdir)
    };

    let (folder_title, segments) = folder_export_parts(&session.index.borrow(), meta.folder_id);
    let doc = session.doc.borrow().to_json_canvas();
    let mut ink_pngs = Vec::new();
    for n in &session.doc.borrow().nodes {
        // Typed cards export as their text only (no ink image beside them).
        if session.doc.borrow().is_typed(n.id()) {
            continue;
        }
        if let CanvasNode::Text(t) = n {
            if let Some((w, h, px)) = load_card_page_pixels(&session.data_dir, canvas_id, &t.id) {
                if let Ok(png) = gray8_to_png(w, h, &px) {
                    ink_pngs.push((t.id.clone(), png));
                }
            }
        }
    }
    let export = VaultCanvasExport {
        folder_title,
        folder_segments: segments,
        canvas_title: meta.title.clone(),
        canvas_id,
        doc,
        ink_pngs,
        include_ink_file_nodes: true,
    };
    let config = config.clone();
    std::thread::spawn(
        move || match export_canvas_json(&vault, &export_subdir, &export) {
            Ok(_) => {
                schedule_vault_git_push_for(&config, &vault);
            }
            Err(e) => tracing::warn!("canvas export failed: {e}"),
        },
    );
}

fn folder_export_parts(index: &CanvasIndex, folder_id: Option<u32>) -> (String, Vec<String>) {
    let Some(mut fid) = folder_id else {
        return ("Canvas".into(), vec![]);
    };
    let mut titles = Vec::new();
    while fid != 0 {
        if let Some(f) = index.folders.iter().find(|f| f.id == fid) {
            titles.push(f.title.clone());
            fid = f.parent_id.unwrap_or(0);
        } else {
            break;
        }
    }
    titles.reverse();
    let display = if titles.is_empty() {
        "Canvas".into()
    } else {
        titles.join(" / ")
    };
    let segs = titles
        .iter()
        .map(|t| ephemeris_core::vault_export::sanitize_component(t))
        .collect();
    (display, segs)
}
