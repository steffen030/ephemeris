//! Obsidian-compatible Canvas: browser, board overview, focus ink, vault export.

use ephemeris_core::profile_manager::ProfileManager;
use ephemeris_core::{
    canvas_data_dir, export_canvas_json, gray8_to_png, hit_port, hit_side, hit_test,
    load_canvas_doc, load_canvas_index, load_card_page_pixels, load_card_strokes, load_card_text,
    new_canvas_id, render_board, save_canvas_doc, save_canvas_index, save_card_page_pixels,
    save_card_strokes, save_card_text, snap_grid, view_for_fit, view_for_focus, zoom_view_at,
    AppCanvasFolder, AppCanvasMeta, BoardDrawOpts, CanvasDoc, CanvasEdge, CanvasIndex, CanvasNode,
    CanvasZoomPreset, Config, EdgeEnd, NodeSide, PageRecognitionCache, StrokePage, TextNode,
    VaultCanvasExport, Viewport, DEFAULT_EXPORT_SUBDIR, PORT_HIT_RADIUS, RECOGNITION_CACHE_FORMAT,
};
use ephemeris_ui::{CanvasFolderListItem, CanvasListItem, EphemerisUi};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::app::{
    begin_create_with_profile, profile_badge_for, recognition_model_version_for, AppConnection,
};

const GRID: i32 = 20;
const CONNECT_MARGIN: f32 = 48.0;
const LONG_PRESS_MS: u64 = 480;
const LONG_PRESS_SLOP: f32 = 14.0;

/// `(t_ms, x, y, hit_node_id)` for long-press / drag start.
type BoardPressStart = (u64, f32, f32, Option<String>);

pub(crate) struct CanvasSession {
    pub data_dir: PathBuf,
    pub index: RefCell<CanvasIndex>,
    pub active_folder_id: Cell<u32>,
    pub active_canvas_id: Cell<u32>,
    pub doc: RefCell<CanvasDoc>,
    pub selected_ids: RefCell<Vec<String>>,
    pub multi_select: Cell<bool>,
    pub connect_from: RefCell<Option<(String, NodeSide)>>,
    pub port_drag: RefCell<Option<(String, NodeSide)>>,
    pub focus_node_id: RefCell<Option<String>>,
    pub focus_page_index: Cell<usize>,
    pub pan_last: Cell<Option<(f32, f32)>>,
    pub last_tap: RefCell<Option<(u64, f32, f32, String)>>,
    pub press_start: RefCell<Option<BoardPressStart>>,
    pub long_press_consumed: Cell<bool>,
    pub move_dragging: Cell<bool>,
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
            connect_from: RefCell::new(None),
            port_drag: RefCell::new(None),
            focus_node_id: RefCell::new(None),
            focus_page_index: Cell::new(0),
            pan_last: Cell::new(None),
            last_tap: RefCell::new(None),
            press_start: RefCell::new(None),
            long_press_consumed: Cell::new(false),
            move_dragging: Cell::new(false),
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

    fn spawn_offset(&self) -> (i32, i32) {
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
        page.set_canvas_selected_id(id.into());
        page.set_canvas_selection_count(sel.len() as i32);
        page.set_canvas_multi_select(self.multi_select.get());
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
    page.set_canvas_connect_mode(false);
    page.set_canvas_tool("select".into());
    page.set_canvas_add_visible(false);
    page.set_canvas_file_dialog_visible(false);
    page.set_canvas_link_dialog_visible(false);
    page.set_canvas_rename_visible(false);
    page.set_drawing_menu_open(false);
    *session.connect_from.borrow_mut() = None;
    *session.port_drag.borrow_mut() = None;
    session.clear_selection();
    session.sync_selection_ui(ui);
    session.pan_last.set(None);
    session.move_dragging.set(false);
    *session.press_start.borrow_mut() = None;
    session.long_press_consumed.set(false);

    if return_to_browser {
        page.set_show_canvas_folder_list(true);
        page.set_show_canvas_list(session.active_folder_id.get() != 0);
        page.set_nav_section(2);
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
            page.set_nav_section(2);
            page.set_show_canvas_list(false);
            page.set_show_canvas_folder_list(true);
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
                page.set_nav_section(0);
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
        ui.on_canvas_rename_confirm(move |kind, id, title| {
            let page = ui_rc.clone_component();
            page.set_canvas_rename_visible(false);
            let title = title.trim().to_string();
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
        ui.on_canvas_board_pointer(move |x, y, kind, pointer| {
            let kind = kind.as_str();
            let pointer = pointer.as_str();
            let page = ui_rc.clone_component();
            if !page.get_show_canvas_board() || page.get_canvas_focus_active() {
                return;
            }
            let connect = page.get_canvas_connect_mode();
            let move_tool = page.get_canvas_tool() == "move";
            let (bw, bh) = ui_rc.canvas_size();
            let vp = {
                let doc = session.doc.borrow();
                Viewport::from_view(&doc.view, bw, bh, &doc)
            };

            match kind {
                "down" => {
                    session.long_press_consumed.set(false);
                    let hit = hit_test(&session.doc.borrow(), &vp, x, y);
                    *session.press_start.borrow_mut() = Some((now_ms(), x, y, hit.clone()));

                    // Port drag from single selection.
                    if !session.multi_select.get() && session.selected_ids.borrow().len() == 1 {
                        let id = session.selected_ids.borrow()[0].clone();
                        if let Some(side) =
                            hit_port(&session.doc.borrow(), &vp, x, y, &id, PORT_HIT_RADIUS)
                        {
                            *session.port_drag.borrow_mut() = Some((id, side));
                            session.pan_last.set(None);
                            return;
                        }
                    }

                    if move_tool {
                        if let Some(id) = hit {
                            let mut sel = session.selected_ids.borrow_mut();
                            if !sel.contains(&id) {
                                sel.clear();
                                sel.push(id);
                                session.multi_select.set(false);
                            }
                            drop(sel);
                            session.sync_selection_ui(&ui_rc);
                            session.move_dragging.set(true);
                            session.pan_last.set(Some((x, y)));
                        }
                        // Empty space in Move: do not pan.
                    } else if !connect {
                        // Select tool: pan only on empty space with finger/mouse.
                        if hit.is_none() && (pointer == "finger" || pointer == "mouse") {
                            session.pan_last.set(Some((x, y)));
                        }
                    }
                }
                "move" => {
                    if session.port_drag.borrow().is_some() {
                        return;
                    }
                    if let Some((t0, px, py, _)) = session.press_start.borrow().clone() {
                        if (x - px).abs() > LONG_PRESS_SLOP || (y - py).abs() > LONG_PRESS_SLOP {
                            *session.press_start.borrow_mut() = Some((t0, px, py, None));
                        }
                    }
                    if session.move_dragging.get() {
                        if let Some((lx, ly)) = session.pan_last.get() {
                            let (bx0, by0) = vp.screen_to_board(lx, ly);
                            let (bx1, by1) = vp.screen_to_board(x, y);
                            let dx = (bx1 - bx0) as i32;
                            let dy = (by1 - by0) as i32;
                            let ids = session.selected_ids.borrow().clone();
                            let mut doc = session.doc.borrow_mut();
                            for id in &ids {
                                if let Some(n) = doc.find_node_mut(id) {
                                    n.set_pos(
                                        snap_grid(n.x() + dx, GRID),
                                        snap_grid(n.y() + dy, GRID),
                                    );
                                }
                            }
                        }
                        session.pan_last.set(Some((x, y)));
                        refresh();
                    } else if let Some((lx, ly)) = session.pan_last.get() {
                        if pointer == "finger" || pointer == "mouse" {
                            let mut doc = session.doc.borrow_mut();
                            doc.view.x -= (x - lx) / vp.scale.max(0.01);
                            doc.view.y -= (y - ly) / vp.scale.max(0.01);
                            session.pan_last.set(Some((x, y)));
                            drop(doc);
                            refresh();
                        }
                    }
                }
                "up" => {
                    // Finish port-drag connect.
                    if let Some((from_id, from_side)) = session.port_drag.borrow_mut().take() {
                        if let Some(to_id) = hit_test(&session.doc.borrow(), &vp, x, y) {
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
                        session.pan_last.set(None);
                        session.move_dragging.set(false);
                        *session.press_start.borrow_mut() = None;
                        return;
                    }

                    // Long-press → multi-select.
                    if let Some((t0, px, py, Some(id))) = session.press_start.borrow().clone() {
                        let held = now_ms().saturating_sub(t0);
                        let still =
                            (x - px).abs() <= LONG_PRESS_SLOP && (y - py).abs() <= LONG_PRESS_SLOP;
                        if held >= LONG_PRESS_MS && still {
                            session.multi_select.set(true);
                            let mut sel = session.selected_ids.borrow_mut();
                            if !sel.contains(&id) {
                                sel.clear();
                                sel.push(id);
                            }
                            drop(sel);
                            session.sync_selection_ui(&ui_rc);
                            session.long_press_consumed.set(true);
                            refresh();
                        }
                    }
                    *session.press_start.borrow_mut() = None;
                    session.pan_last.set(None);
                    session.move_dragging.set(false);
                    session.save_doc();
                }
                "tap" => {
                    if session.long_press_consumed.replace(false) {
                        return;
                    }
                    handle_board_tap(&ui_rc, &session, &config, &refresh, x, y, connect);
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
            let (x, y) = session.spawn_offset();
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
                    let id = session.doc.borrow_mut().add_group(x, y, "Group");
                    session.set_single_selection(Some(id));
                    session.sync_selection_ui(&ui_rc);
                }
                _ => {
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
            let (x, y) = session.spawn_offset();
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
            let (x, y) = session.spawn_offset();
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
            leave_focus_if_any(&ui_rc, &session, &config);
            let page = ui_rc.clone_component();
            page.set_canvas_focus_active(false);
            page.set_show_canvas_board(true);
            page.set_drawing_menu_open(false);
            refresh();
            ui_rc.request_screen_change();
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
    session.clear_selection();
    *session.connect_from.borrow_mut() = None;
    *session.port_drag.borrow_mut() = None;
    *session.focus_node_id.borrow_mut() = None;
    let page = ui.clone_component();
    page.set_show_canvas_folder_list(false);
    page.set_show_canvas_list(false);
    page.set_show_canvas_board(true);
    page.set_canvas_focus_active(false);
    page.set_canvas_menu_open(false);
    page.set_canvas_connect_mode(false);
    page.set_canvas_tool("select".into());
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

/// Cancel pan / move / long-press when a second finger starts a pinch.
pub(crate) fn cancel_board_pointer_gesture(session: &CanvasSession) {
    session.pan_last.set(None);
    session.move_dragging.set(false);
    session.long_press_consumed.set(false);
    *session.press_start.borrow_mut() = None;
    *session.port_drag.borrow_mut() = None;
}

/// Continuous zoom around a screen point (pinch / trackpad). Refreshes the board image.
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
    if let Some(refresh) = session.board_refresh.borrow().clone() {
        refresh();
    }
}

fn refresh_board_image(ui: &EphemerisUi, session: &CanvasSession) {
    let (bw, bh) = ui.canvas_size();
    let doc = session.doc.borrow();
    let vp = Viewport::from_view(&doc.view, bw, bh, &doc);
    let selected = session.selected_ids.borrow().clone();
    let connect = session
        .connect_from
        .borrow()
        .as_ref()
        .map(|(id, _)| id.clone());
    let show_ports = !session.multi_select.get() && selected.len() == 1;
    let canvas_id = session.active_canvas_id.get();
    let mut ink_previews: HashMap<String, (u32, u32, Vec<u8>)> = HashMap::new();
    for n in &doc.nodes {
        if let CanvasNode::Text(t) = n {
            if let Some((w, h, px)) = load_card_page_pixels(&session.data_dir, canvas_id, &t.id) {
                // Skip blank pages (all near-white).
                if px.iter().any(|&p| p < 220) {
                    ink_previews.insert(t.id.clone(), (w, h, px));
                }
            }
        }
    }
    let opts = BoardDrawOpts {
        selected_ids: &selected,
        connect_from: connect.as_deref(),
        show_ports,
        ink_previews: &ink_previews,
    };
    let gray = render_board(&doc, &vp, bw, bh, &opts);
    drop(doc);
    ui.set_canvas_board_gray8(bw, bh, &gray);
}

fn push_browser(ui: &EphemerisUi, session: &CanvasSession) {
    let fid = session.active_folder_id.get();
    let profile = session.profile_filter.borrow().clone();
    let sort = session.sort_mode.borrow().clone();
    let view = session.view_mode.borrow().clone();
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
        .map(|c| {
            let (show_profile, profile_icon_char, has_custom_profile_icon, custom_profile_icon) =
                badge_for(&c.profile_id);
            (
                c.clone(),
                CanvasListItem {
                    id: c.id.to_string(),
                    title: c.title.clone(),
                    date_label: format_canvas_date(c.updated_at.max(c.created_at), now),
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
    connect: bool,
) {
    let (bw, bh) = ui.canvas_size();
    let doc = session.doc.borrow();
    let vp = Viewport::from_view(&doc.view, bw, bh, &doc);
    let hit = hit_test(&doc, &vp, x, y);
    drop(doc);

    if connect {
        if let Some(id) = hit {
            let (bx, by) = {
                let doc = session.doc.borrow();
                let vp = Viewport::from_view(&doc.view, bw, bh, &doc);
                vp.screen_to_board(x, y)
            };
            let side = hit_side(&session.doc.borrow(), &id, bx, by, CONNECT_MARGIN)
                .unwrap_or(NodeSide::Right);
            let mut from = session.connect_from.borrow_mut();
            if let Some((from_id, from_side)) = from.take() {
                if from_id != id {
                    let edge = CanvasEdge {
                        id: new_canvas_id(),
                        from_node: from_id,
                        from_side: Some(from_side),
                        from_end: None,
                        to_node: id,
                        to_side: Some(side),
                        to_end: Some(EdgeEnd::Arrow),
                        color: None,
                        label: None,
                    };
                    session.doc.borrow_mut().edges.push(edge);
                    session.save_doc();
                }
            } else {
                *from = Some((id, side));
            }
            refresh();
        }
        return;
    }

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
            let is_text = matches!(
                session.doc.borrow().find_node(&id),
                Some(CanvasNode::Text(_))
            );
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
        *session.connect_from.borrow_mut() = None;
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
    std::thread::spawn(move || {
        if let Err(e) = export_canvas_json(&vault, &export_subdir, &export) {
            tracing::warn!("canvas export failed: {e}");
        }
    });
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
