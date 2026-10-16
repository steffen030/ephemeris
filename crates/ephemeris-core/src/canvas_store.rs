//! Filesystem persistence for Ephemeris Canvas (parallel to Notes).
//!
//! Layout under `~/.local/share/ephemeris/canvas/`:
//! ```text
//! canvas_index.json
//! c_{id}.json                 # nodes, edges, viewport
//! cards/c_{id}/{node_id}/
//!   strokes.json
//!   page.bin
//!   text.json
//! ```

use crate::json_canvas::{
    new_canvas_id, CanvasEdge, CanvasNode, FileNode, GroupNode, JsonCanvas, LinkNode, TextNode,
};
use crate::recognition_cache::PageRecognitionCache;
use crate::store::StrokePage;
use crate::AppError;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Nested folder (like a notebook).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppCanvasFolder {
    pub id: u32,
    pub title: String,
    #[serde(default)]
    pub profile_id: String,
    #[serde(default)]
    pub parent_id: Option<u32>,
}

/// Canvas metadata row in the index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppCanvasMeta {
    pub id: u32,
    pub title: String,
    #[serde(default)]
    pub folder_id: Option<u32>,
    #[serde(default)]
    pub profile_id: String,
    pub created_at: u64,
    #[serde(default)]
    pub updated_at: u64,
}

/// Persisted index of all canvases and folders.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CanvasIndex {
    #[serde(default)]
    pub canvas_id_seq: u32,
    #[serde(default)]
    pub folder_id_seq: u32,
    #[serde(default)]
    pub folders: Vec<AppCanvasFolder>,
    #[serde(default)]
    pub canvases: Vec<AppCanvasMeta>,
}

/// Zoom presets for the board chrome (Fit / 100% / Focus).
/// Continuous pinch updates [`CanvasView::scale`] while leaving the preset at
/// `OneHundred` (or converting from Fit on first pinch).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CanvasZoomPreset {
    Fit,
    #[default]
    OneHundred,
    FocusSelected,
}

fn default_view_scale() -> f32 {
    1.0
}

/// Saved board viewport.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanvasView {
    /// World-space top-left of the viewport (board coords).
    pub x: f32,
    pub y: f32,
    #[serde(default)]
    pub zoom: CanvasZoomPreset,
    /// Screen pixels per board unit when `zoom` is not `Fit`.
    /// Updated by pinch / trackpad; presets reset it (100% → 1.0).
    #[serde(default = "default_view_scale")]
    pub scale: f32,
}

impl Default for CanvasView {
    fn default() -> Self {
        Self {
            x: -40.0,
            y: -40.0,
            zoom: CanvasZoomPreset::OneHundred,
            scale: 1.0,
        }
    }
}

/// On-disk body for one canvas (nodes/edges + Ephemeris view).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CanvasDoc {
    #[serde(default)]
    pub nodes: Vec<CanvasNode>,
    #[serde(default)]
    pub edges: Vec<CanvasEdge>,
    #[serde(default)]
    pub view: CanvasView,
    /// Text cards shown as their recognised (HWR) text instead of the ink.
    /// Ephemeris-only: the ink stays on disk so the card can revert.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub typed_cards: Vec<String>,
}

impl CanvasDoc {
    /// Strip Ephemeris-only view for Obsidian export.
    pub fn to_json_canvas(&self) -> JsonCanvas {
        JsonCanvas {
            nodes: self.nodes.clone(),
            edges: self.edges.clone(),
        }
    }

    pub fn from_json_canvas(jc: JsonCanvas) -> Self {
        Self {
            nodes: jc.nodes,
            edges: jc.edges,
            view: CanvasView::default(),
            typed_cards: Vec::new(),
        }
    }

    pub fn find_node(&self, id: &str) -> Option<&CanvasNode> {
        self.nodes.iter().find(|n| n.id() == id)
    }

    pub fn find_node_mut(&mut self, id: &str) -> Option<&mut CanvasNode> {
        self.nodes.iter_mut().find(|n| n.id() == id)
    }

    /// True when text card `id` shows its recognised text, not the ink.
    pub fn is_typed(&self, id: &str) -> bool {
        self.typed_cards.iter().any(|t| t == id)
    }

    /// Show text card `id` as recognised text (`true`) or as ink (`false`).
    pub fn set_typed(&mut self, id: &str, typed: bool) {
        self.typed_cards.retain(|t| t != id);
        if typed {
            self.typed_cards.push(id.to_string());
        }
    }

    pub fn remove_node(&mut self, id: &str) {
        self.typed_cards.retain(|t| t != id);
        self.nodes.retain(|n| n.id() != id);
        self.edges.retain(|e| e.from_node != id && e.to_node != id);
    }

    pub fn add_text_card(&mut self, x: i32, y: i32) -> String {
        let id = new_canvas_id();
        self.nodes.push(CanvasNode::Text(TextNode {
            id: id.clone(),
            x,
            y,
            width: 280,
            height: 160,
            text: String::new(),
            color: None,
        }));
        id
    }

    pub fn add_file_card(&mut self, x: i32, y: i32, file: impl Into<String>) -> String {
        let id = new_canvas_id();
        self.nodes.push(CanvasNode::File(FileNode {
            id: id.clone(),
            x,
            y,
            width: 260,
            height: 100,
            file: file.into(),
            subpath: None,
            color: None,
        }));
        id
    }

    pub fn add_link_card(&mut self, x: i32, y: i32, url: impl Into<String>) -> String {
        let id = new_canvas_id();
        self.nodes.push(CanvasNode::Link(LinkNode {
            id: id.clone(),
            x,
            y,
            width: 260,
            height: 80,
            url: url.into(),
            color: None,
        }));
        id
    }

    pub fn add_group(&mut self, x: i32, y: i32, label: impl Into<String>) -> String {
        let id = new_canvas_id();
        self.nodes.push(CanvasNode::Group(GroupNode {
            id: id.clone(),
            x,
            y,
            width: 480,
            height: 320,
            label: Some(label.into()),
            background: None,
            background_style: None,
            color: None,
        }));
        id
    }

    /// Nodes fully inside group `group_id` (Obsidian moves these with the group).
    pub fn group_members(&self, group_id: &str) -> Vec<String> {
        let Some(g) = self.find_node(group_id) else {
            return Vec::new();
        };
        if !matches!(g, CanvasNode::Group(_)) {
            return Vec::new();
        }
        self.nodes
            .iter()
            .filter(|n| n.id() != group_id && g.contains(n))
            .map(|n| n.id().to_string())
            .collect()
    }

    /// New group framing `ids` with `pad` board px of margin. Inserted at the
    /// bottom of the z-order so it stays behind its members.
    pub fn add_group_around(&mut self, ids: &[String], pad: i32, label: &str) -> Option<String> {
        let mut bounds: Option<(i32, i32, i32, i32)> = None;
        for n in self
            .nodes
            .iter()
            .filter(|n| ids.iter().any(|id| id == n.id()))
        {
            let (x1, y1) = (n.x() + n.width(), n.y() + n.height());
            bounds = Some(match bounds {
                None => (n.x(), n.y(), x1, y1),
                Some((a, b, c, d)) => (a.min(n.x()), b.min(n.y()), c.max(x1), d.max(y1)),
            });
        }
        let (x0, y0, x1, y1) = bounds?;
        let id = new_canvas_id();
        self.nodes.insert(
            0,
            CanvasNode::Group(GroupNode {
                id: id.clone(),
                x: x0 - pad,
                y: y0 - pad,
                width: x1 - x0 + 2 * pad,
                height: y1 - y0 + 2 * pad,
                label: Some(label.to_string()),
                background: None,
                background_style: None,
                color: None,
            }),
        );
        Some(id)
    }
}

/// `~/.local/share/ephemeris/canvas/` (respects `XDG_DATA_HOME`).
pub fn canvas_data_dir() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".local").join("share")
        });
    base.join("ephemeris").join("canvas")
}

pub fn canvas_index_path(data_dir: &Path) -> PathBuf {
    data_dir.join("canvas_index.json")
}

pub fn canvas_doc_path(data_dir: &Path, canvas_id: u32) -> PathBuf {
    data_dir.join(format!("c_{canvas_id}.json"))
}

pub fn card_dir(data_dir: &Path, canvas_id: u32, node_id: &str) -> PathBuf {
    data_dir
        .join("cards")
        .join(format!("c_{canvas_id}"))
        .join(node_id)
}

pub fn card_strokes_path(data_dir: &Path, canvas_id: u32, node_id: &str) -> PathBuf {
    card_dir(data_dir, canvas_id, node_id).join("strokes.json")
}

pub fn card_page_path(data_dir: &Path, canvas_id: u32, node_id: &str) -> PathBuf {
    card_dir(data_dir, canvas_id, node_id).join("page.bin")
}

pub fn card_text_path(data_dir: &Path, canvas_id: u32, node_id: &str) -> PathBuf {
    card_dir(data_dir, canvas_id, node_id).join("text.json")
}

pub fn load_canvas_index(data_dir: &Path) -> Option<CanvasIndex> {
    let json = fs::read_to_string(canvas_index_path(data_dir)).ok()?;
    serde_json::from_str(&json).ok()
}

pub fn save_canvas_index(data_dir: &Path, index: &CanvasIndex) -> crate::Result<()> {
    fs::create_dir_all(data_dir).map_err(AppError::Io)?;
    let json = serde_json::to_string_pretty(index)
        .map_err(|e| AppError::Storage(format!("serialize canvas index: {e}")))?;
    let path = canvas_index_path(data_dir);
    atomic_write_bytes(&path, json.as_bytes())?;
    Ok(())
}

pub fn load_canvas_doc(data_dir: &Path, canvas_id: u32) -> CanvasDoc {
    let path = canvas_doc_path(data_dir, canvas_id);
    fs::read_to_string(&path)
        .ok()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default()
}

pub fn save_canvas_doc(data_dir: &Path, canvas_id: u32, doc: &CanvasDoc) -> crate::Result<()> {
    fs::create_dir_all(data_dir).map_err(AppError::Io)?;
    let json = serde_json::to_string_pretty(doc)
        .map_err(|e| AppError::Storage(format!("serialize canvas doc: {e}")))?;
    atomic_write_bytes(&canvas_doc_path(data_dir, canvas_id), json.as_bytes())?;
    Ok(())
}

pub fn delete_canvas_files(data_dir: &Path, canvas_id: u32) {
    let _ = fs::remove_file(canvas_doc_path(data_dir, canvas_id));
    let cards = data_dir.join("cards").join(format!("c_{canvas_id}"));
    let _ = fs::remove_dir_all(cards);
}

pub fn load_card_strokes(data_dir: &Path, canvas_id: u32, node_id: &str) -> StrokePage {
    let path = card_strokes_path(data_dir, canvas_id, node_id);
    fs::read_to_string(&path)
        .ok()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default()
}

pub fn save_card_strokes(
    data_dir: &Path,
    canvas_id: u32,
    node_id: &str,
    page: &StrokePage,
) -> crate::Result<()> {
    let dir = card_dir(data_dir, canvas_id, node_id);
    fs::create_dir_all(&dir).map_err(AppError::Io)?;
    let json = serde_json::to_string_pretty(page)
        .map_err(|e| AppError::Storage(format!("serialize card strokes: {e}")))?;
    atomic_write_bytes(
        &card_strokes_path(data_dir, canvas_id, node_id),
        json.as_bytes(),
    )?;
    Ok(())
}

pub fn load_card_text(
    data_dir: &Path,
    canvas_id: u32,
    node_id: &str,
) -> Option<PageRecognitionCache> {
    let path = card_text_path(data_dir, canvas_id, node_id);
    let json = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&json).ok()
}

pub fn save_card_text(
    data_dir: &Path,
    canvas_id: u32,
    node_id: &str,
    cache: &PageRecognitionCache,
) -> crate::Result<()> {
    let dir = card_dir(data_dir, canvas_id, node_id);
    fs::create_dir_all(&dir).map_err(AppError::Io)?;
    let json = serde_json::to_string_pretty(cache)
        .map_err(|e| AppError::Storage(format!("serialize card text: {e}")))?;
    atomic_write_bytes(
        &card_text_path(data_dir, canvas_id, node_id),
        json.as_bytes(),
    )?;
    Ok(())
}

const PAGE_BIN_MAGIC: &[u8; 4] = b"EPH1";

pub fn save_card_page_pixels(
    data_dir: &Path,
    canvas_id: u32,
    node_id: &str,
    width: u32,
    height: u32,
    pixels: &[u8],
) -> crate::Result<()> {
    let dir = card_dir(data_dir, canvas_id, node_id);
    fs::create_dir_all(&dir).map_err(AppError::Io)?;
    let mut buf = Vec::with_capacity(12 + pixels.len());
    buf.extend_from_slice(PAGE_BIN_MAGIC);
    buf.extend_from_slice(&width.to_le_bytes());
    buf.extend_from_slice(&height.to_le_bytes());
    buf.extend_from_slice(pixels);
    atomic_write_bytes(&card_page_path(data_dir, canvas_id, node_id), &buf)?;
    Ok(())
}

pub fn load_card_page_pixels(
    data_dir: &Path,
    canvas_id: u32,
    node_id: &str,
) -> Option<(u32, u32, Vec<u8>)> {
    let buf = fs::read(card_page_path(data_dir, canvas_id, node_id)).ok()?;
    if buf.len() < 12 || &buf[0..4] != PAGE_BIN_MAGIC {
        return None;
    }
    let width = u32::from_le_bytes(buf[4..8].try_into().ok()?);
    let height = u32::from_le_bytes(buf[8..12].try_into().ok()?);
    Some((width, height, buf[12..].to_vec()))
}

fn atomic_write_bytes(path: &Path, content: &[u8]) -> crate::Result<()> {
    let tmp = path.with_extension("ephemeris-tmp");
    fs::write(&tmp, content).map_err(AppError::Io)?;
    fs::rename(&tmp, path).map_err(AppError::Io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn group_around_frames_members_and_moves_with_them() {
        let mut doc = CanvasDoc::default();
        let a = doc.add_text_card(100, 100);
        let b = doc.add_text_card(500, 300);
        let outside = doc.add_text_card(2000, 2000);
        let g = doc
            .add_group_around(&[a.clone(), b.clone()], 40, "Ideas")
            .unwrap();
        let n = doc.find_node(&g).unwrap();
        assert_eq!((n.x(), n.y()), (60, 60));
        assert_eq!((n.width(), n.height()), (280 + 400 + 80, 160 + 200 + 80));
        assert!(matches!(doc.nodes[0], CanvasNode::Group(_)));
        let mut members = doc.group_members(&g);
        members.sort();
        let mut want = vec![a, b];
        want.sort();
        assert_eq!(members, want);
        assert!(!doc.group_members(&g).contains(&outside));
        assert!(doc.add_group_around(&[], 40, "x").is_none());
    }

    #[test]
    fn index_and_doc_round_trip() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut index = CanvasIndex {
            canvas_id_seq: 1,
            ..Default::default()
        };
        index.canvases.push(AppCanvasMeta {
            id: 1,
            title: "Map".into(),
            folder_id: None,
            profile_id: "p1".into(),
            created_at: 1,
            updated_at: 2,
        });
        save_canvas_index(dir, &index).unwrap();
        let loaded = load_canvas_index(dir).unwrap();
        assert_eq!(loaded.canvases[0].title, "Map");

        let mut doc = CanvasDoc::default();
        let nid = doc.add_text_card(10, 20);
        save_canvas_doc(dir, 1, &doc).unwrap();
        let again = load_canvas_doc(dir, 1);
        assert_eq!(again.nodes.len(), 1);
        assert_eq!(again.nodes[0].id(), nid);
    }

    #[test]
    fn card_sidecars() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let page = StrokePage::default();
        save_card_strokes(dir, 1, "abc", &page).unwrap();
        assert!(card_strokes_path(dir, 1, "abc").exists());
        let cache = PageRecognitionCache::new("test");
        save_card_text(dir, 1, "abc", &cache).unwrap();
        assert!(load_card_text(dir, 1, "abc").is_some());
        save_card_page_pixels(dir, 1, "abc", 2, 2, &[255, 255, 255, 255]).unwrap();
        let (w, h, px) = load_card_page_pixels(dir, 1, "abc").unwrap();
        assert_eq!((w, h, px.len()), (2, 2, 4));
    }

    #[test]
    fn typed_cards_persist_and_stay_out_of_export() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut doc = CanvasDoc::default();
        let a = doc.add_text_card(0, 0);
        let b = doc.add_text_card(300, 0);
        doc.set_typed(&a, true);
        doc.set_typed(&a, true);
        assert_eq!(doc.typed_cards, vec![a.clone()]);
        save_canvas_doc(dir, 1, &doc).unwrap();
        let again = load_canvas_doc(dir, 1);
        assert!(again.is_typed(&a) && !again.is_typed(&b));
        // Ephemeris-only: Obsidian's JSON Canvas never sees it.
        let json = again.to_json_canvas().to_json_pretty().unwrap();
        assert!(!json.contains("typed_cards"));

        let mut doc = again;
        doc.set_typed(&a, false);
        assert!(!doc.is_typed(&a));
        doc.set_typed(&b, true);
        doc.remove_node(&b);
        assert!(doc.typed_cards.is_empty());
    }
}
