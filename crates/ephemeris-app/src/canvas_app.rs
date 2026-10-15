//! Obsidian-compatible Canvas: browser, board overview, focus ink, vault export.

use ephemeris_core::{
    canvas_data_dir, export_canvas_json, gray8_to_png, hit_side, hit_test, load_canvas_doc,
    load_canvas_index, load_card_page_pixels, load_card_strokes, load_card_text, new_canvas_id,
    render_board, save_canvas_doc, save_canvas_index, save_card_page_pixels, save_card_strokes,
    save_card_text, snap_grid, view_for_fit, view_for_focus, AppCanvasFolder, AppCanvasMeta,
    CanvasDoc, CanvasEdge, CanvasIndex, CanvasNode, CanvasZoomPreset, Config, EdgeEnd, NodeSide,
    PageRecognitionCache, StrokePage, TextNode, VaultCanvasExport, Viewport, DEFAULT_EXPORT_SUBDIR,
    RECOGNITION_CACHE_FORMAT,
};
use ephemeris_ui::{CanvasFolderListItem, CanvasListItem, EphemerisUi};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::app::{recognition_model_version_for, AppConnection};

const GRID: i32 = 20;
const CONNECT_MARGIN: f32 = 48.0;

pub(crate) struct CanvasSession {
    pub data_dir: PathBuf,
    pub index: RefCell<CanvasIndex>,
    pub active_folder_id: Cell<u32>,
    pub active_canvas_id: Cell<u32>,
    pub doc: RefCell<CanvasDoc>,
    pub selected_id: RefCell<Option<String>>,
    pub connect_from: RefCell<Option<(String, NodeSide)>>,
    pub focus_node_id: RefCell<Option<String>>,
    pub focus_page_index: Cell<usize>,
    pub pan_last: Cell<Option<(f32, f32)>>,
    pub last_tap: RefCell<Option<(u64, f32, f32, String)>>,
    pub move_dragging: Cell<bool>,
    pub profile_filter: Rc<RefCell<String>>,
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
            selected_id: RefCell::new(None),
            connect_from: RefCell::new(None),
            focus_node_id: RefCell::new(None),
            focus_page_index: Cell::new(0),
            pan_last: Cell::new(None),
            last_tap: RefCell::new(None),
            move_dragging: Cell::new(false),
            profile_filter: Rc::new(RefCell::new(String::new())),
        }
    }

    fn save_index(&self) {
        if let Err(e) = save_canvas_index(&self.data_dir, &self.index.borrow()) {
            tracing::warn!("save canvas index: {e}");
        }
    }

    fn save_doc(&self) {
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
}

/// Wire Canvas UI. Call once during app setup after notes PageBook slots are known.
pub(crate) fn wire_canvas(
    ui: Rc<EphemerisUi>,
    session: Rc<CanvasSession>,
    config: Rc<RefCell<Config>>,
    connections: Rc<RefCell<Vec<AppConnection>>>,
    active_profile_filter: Rc<RefCell<String>>,
    next_page_index: usize,
) {
    session.focus_page_index.set(next_page_index);
    *session.profile_filter.borrow_mut() = active_profile_filter.borrow().clone();
    push_browser(&ui, &session);

    let refresh = {
        let ui = ui.clone();
        let session = session.clone();
        Rc::new(move || refresh_board_image(&ui, &session)) as Rc<dyn Fn()>
    };

    let leave_board = {
        let ui = ui.clone();
        let session = session.clone();
        let config = config.clone();
        let connections = connections.clone();
        Rc::new(move || {
            leave_focus_if_any(&ui, &session, &config);
            session.save_doc();
            export_active_canvas(&session, &config.borrow(), &connections.borrow());
            let page = ui.clone_component();
            page.set_show_canvas_board(false);
            page.set_canvas_menu_open(false);
            page.set_canvas_connect_mode(false);
            page.set_canvas_focus_active(false);
            page.set_show_canvas_folder_list(true);
            page.set_show_canvas_list(session.active_folder_id.get() != 0);
            page.set_nav_section(2);
            push_browser(&ui, &session);
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
        ui.on_start_page_canvas_tapped(move || {
            session.active_folder_id.set(0);
            let page = ui_rc.clone_component();
            page.set_show_start_page(false);
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
        let profile = active_profile_filter.clone();
        ui.on_canvas_folder_new_tapped(move || {
            let mut idx = session.index.borrow_mut();
            let id = idx.folder_id_seq.max(1);
            idx.folder_id_seq = id + 1;
            let parent = session.active_folder_id.get();
            idx.folders.push(AppCanvasFolder {
                id,
                title: format!("Folder {id}"),
                profile_id: profile.borrow().clone(),
                parent_id: if parent == 0 { None } else { Some(parent) },
            });
            drop(idx);
            session.save_index();
            push_browser(&ui_rc, &session);
        });
    }

    {
        let ui_rc = ui.clone();
        let session = session.clone();
        let refresh = refresh.clone();
        let profile = active_profile_filter.clone();
        ui.on_canvas_new_tapped(move || {
            let mut idx = session.index.borrow_mut();
            let id = idx.canvas_id_seq.max(1);
            idx.canvas_id_seq = id + 1;
            let folder = session.active_folder_id.get();
            idx.canvases.push(AppCanvasMeta {
                id,
                title: format!("Canvas {id}"),
                folder_id: if folder == 0 { None } else { Some(folder) },
                profile_id: profile.borrow().clone(),
                created_at: now_secs(),
                updated_at: now_secs(),
            });
            drop(idx);
            session.save_index();
            open_canvas(&ui_rc, &session, id, &refresh);
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
                    if pointer == "finger" || (pointer == "mouse" && !move_tool && !connect) {
                        session.pan_last.set(Some((x, y)));
                    }
                    if move_tool {
                        if let Some(id) = hit_test(&session.doc.borrow(), &vp, x, y) {
                            *session.selected_id.borrow_mut() = Some(id.clone());
                            session.move_dragging.set(true);
                            page.set_canvas_selected_id(id.into());
                            session.pan_last.set(Some((x, y)));
                        }
                    }
                }
                "move" => {
                    if session.move_dragging.get() {
                        if let Some(id) = session.selected_id.borrow().clone() {
                            if let Some((lx, ly)) = session.pan_last.get() {
                                let (bx0, by0) = vp.screen_to_board(lx, ly);
                                let (bx1, by1) = vp.screen_to_board(x, y);
                                let dx = (bx1 - bx0) as i32;
                                let dy = (by1 - by0) as i32;
                                if let Some(n) = session.doc.borrow_mut().find_node_mut(&id) {
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
                    session.pan_last.set(None);
                    session.move_dragging.set(false);
                    session.save_doc();
                }
                "tap" => {
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
                    if let Some(id) = session.selected_id.borrow().clone() {
                        doc.view = view_for_focus(&doc, &id, bw, bh);
                    } else {
                        doc.view.zoom = CanvasZoomPreset::OneHundred;
                    }
                }
                _ => doc.view.zoom = CanvasZoomPreset::OneHundred,
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
            let view = session.doc.borrow().view.clone();
            let x = snap_grid(view.x as i32 + 80, GRID);
            let y = snap_grid(view.y as i32 + 80, GRID);
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
                    *session.selected_id.borrow_mut() = Some(id.clone());
                    page.set_canvas_selected_id(id.into());
                }
                _ => {
                    let id = session.doc.borrow_mut().add_text_card(x, y);
                    *session.selected_id.borrow_mut() = Some(id.clone());
                    page.set_canvas_selected_id(id.into());
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
            let view = session.doc.borrow().view.clone();
            let id = session.doc.borrow_mut().add_file_card(
                snap_grid(view.x as i32 + 80, GRID),
                snap_grid(view.y as i32 + 80, GRID),
                path,
            );
            *session.selected_id.borrow_mut() = Some(id);
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
            let view = session.doc.borrow().view.clone();
            let id = session.doc.borrow_mut().add_link_card(
                snap_grid(view.x as i32 + 80, GRID),
                snap_grid(view.y as i32 + 80, GRID),
                url,
            );
            *session.selected_id.borrow_mut() = Some(id);
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
        });
    }

    {
        let session = session.clone();
        let refresh = refresh.clone();
        let ui_rc = ui.clone();
        ui.on_canvas_delete_selected(move || {
            if let Some(id) = session.selected_id.borrow_mut().take() {
                session.doc.borrow_mut().remove_node(&id);
                ui_rc.clone_component().set_canvas_selected_id("".into());
                session.save_doc();
                refresh();
            }
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
    *session.selected_id.borrow_mut() = None;
    *session.connect_from.borrow_mut() = None;
    *session.focus_node_id.borrow_mut() = None;
    let page = ui.clone_component();
    page.set_show_canvas_folder_list(false);
    page.set_show_canvas_list(false);
    page.set_show_canvas_board(true);
    page.set_canvas_focus_active(false);
    page.set_canvas_menu_open(false);
    page.set_canvas_connect_mode(false);
    page.set_canvas_tool("select".into());
    page.set_canvas_selected_id("".into());
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

fn refresh_board_image(ui: &EphemerisUi, session: &CanvasSession) {
    let (bw, bh) = ui.canvas_size();
    let doc = session.doc.borrow();
    let vp = Viewport::from_view(&doc.view, bw, bh, &doc);
    let selected = session.selected_id.borrow().clone();
    let connect = session
        .connect_from
        .borrow()
        .as_ref()
        .map(|(id, _)| id.clone());
    let gray = render_board(&doc, &vp, bw, bh, selected.as_deref(), connect.as_deref());
    drop(doc);
    ui.set_canvas_board_gray8(bw, bh, &gray);
}

fn push_browser(ui: &EphemerisUi, session: &CanvasSession) {
    let fid = session.active_folder_id.get();
    let profile = session.profile_filter.borrow().clone();
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

    let folders: Vec<CanvasFolderListItem> = idx
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
            CanvasFolderListItem {
                id: f.id.to_string(),
                title: f.title.clone(),
                canvas_count: child_canvases,
                folder_count: child_folders,
            }
        })
        .collect();

    let canvases: Vec<CanvasListItem> = idx
        .canvases
        .iter()
        .filter(|c| c.folder_id.unwrap_or(0) == fid)
        .filter(|c| profile.is_empty() || c.profile_id.is_empty() || c.profile_id == profile)
        .map(|c| CanvasListItem {
            id: c.id.to_string(),
            title: c.title.clone(),
            date_label: if c.updated_at > 0 {
                format!("{}", c.updated_at / 86400)
            } else {
                String::new()
            },
        })
        .collect();

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
            if let Some((from_id, from_side)) = from.clone() {
                if from_id != id {
                    session.doc.borrow_mut().edges.push(CanvasEdge {
                        id: new_canvas_id(),
                        from_node: from_id,
                        from_side: Some(from_side),
                        from_end: Some(EdgeEnd::None),
                        to_node: id,
                        to_side: Some(side),
                        to_end: Some(EdgeEnd::Arrow),
                        color: None,
                        label: None,
                    });
                    *from = None;
                    ui.clone_component().set_canvas_connect_mode(false);
                    session.save_doc();
                    refresh();
                }
            } else {
                *from = Some((id, side));
                refresh();
            }
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
        *session.selected_id.borrow_mut() = Some(id.clone());
        ui.clone_component()
            .set_canvas_selected_id(id.clone().into());
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
        *session.selected_id.borrow_mut() = None;
        ui.clone_component().set_canvas_selected_id("".into());
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
    page.set_show_canvas_board(false);
    page.set_canvas_focus_active(true);
    page.set_canvas_menu_open(false);
    page.set_drawing_menu_open(false);
    page.set_active_note_id(format!("canvas:{canvas_id}:{node_id}").into());
    page.set_page_title("Text card".into());
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
