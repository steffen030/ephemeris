//! Persistence for completed strokes.
//!
//! ADR ephemeris-cm7 stores strokes per page as a compressed blob plus an
//! index table (SQLite; see ephemeris-fna.3).  That storage backend is a
//! separate task.  Here we define the platform-agnostic [`StrokeStore`] trait
//! that the ink engine persists to, plus two concrete implementations:
//!
//! * [`MemoryStore`] — in-memory, for tests and the desktop mock.
//! * [`JsonFileStore`] — a simple JSON-on-disk store (feature-gated `fs`),
//!   used to satisfy the "strokes persist and reload" acceptance criterion
//!   until the SQLite backend lands.
//!
//! The serialization used by both is plain serde, so any future backend can
//! reuse the exact on-wire shape.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model::{Stroke, StrokeId};

/// Errors from a [`StrokeStore`].
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("stroke not found: {0:?}")]
    NotFound(StrokeId),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("i/o error: {0}")]
    Io(String),
}

/// Abstract persistence for strokes on a single page.
///
/// Implementations must preserve insertion order: [`StrokeStore::load_all`]
/// returns strokes in the order they were appended, matching the z-order in
/// which they should be re-rendered.
pub trait StrokeStore {
    /// Append (or replace, by id) a completed stroke.
    fn save(&mut self, stroke: &Stroke) -> Result<(), StoreError>;

    /// Load a single stroke by id.
    fn load(&self, id: StrokeId) -> Result<Stroke, StoreError>;

    /// Load every stroke in insertion (z) order.
    fn load_all(&self) -> Result<Vec<Stroke>, StoreError>;

    /// Remove a stroke by id (e.g. after erasing). Returns whether it existed.
    fn remove(&mut self, id: StrokeId) -> Result<bool, StoreError>;
}

/// Serializable snapshot of a page's strokes — the shared on-wire shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StrokePage {
    pub strokes: Vec<Stroke>,
}

// ── In-memory store ─────────────────────────────────────────────────────────

/// A simple in-memory [`StrokeStore`], preserving insertion order.
#[derive(Debug, Default)]
pub struct MemoryStore {
    strokes: Vec<Stroke>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of stored strokes.
    pub fn len(&self) -> usize {
        self.strokes.len()
    }

    /// `true` if no strokes are stored.
    pub fn is_empty(&self) -> bool {
        self.strokes.is_empty()
    }

    /// Serialize the whole page to a JSON string.
    pub fn to_json(&self) -> Result<String, StoreError> {
        let page = StrokePage { strokes: self.strokes.clone() };
        Ok(serde_json::to_string(&page)?)
    }

    /// Reconstruct a store from a JSON string produced by [`Self::to_json`].
    pub fn from_json(json: &str) -> Result<Self, StoreError> {
        let page: StrokePage = serde_json::from_str(json)?;
        Ok(Self { strokes: page.strokes })
    }
}

impl StrokeStore for MemoryStore {
    fn save(&mut self, stroke: &Stroke) -> Result<(), StoreError> {
        if let Some(slot) = self.strokes.iter_mut().find(|s| s.id == stroke.id) {
            *slot = stroke.clone();
        } else {
            self.strokes.push(stroke.clone());
        }
        Ok(())
    }

    fn load(&self, id: StrokeId) -> Result<Stroke, StoreError> {
        self.strokes
            .iter()
            .find(|s| s.id == id)
            .cloned()
            .ok_or(StoreError::NotFound(id))
    }

    fn load_all(&self) -> Result<Vec<Stroke>, StoreError> {
        Ok(self.strokes.clone())
    }

    fn remove(&mut self, id: StrokeId) -> Result<bool, StoreError> {
        let before = self.strokes.len();
        self.strokes.retain(|s| s.id != id);
        Ok(self.strokes.len() != before)
    }
}

// ── JSON-on-disk store ──────────────────────────────────────────────────────

/// A [`StrokeStore`] backed by a single JSON file on disk.
///
/// Every mutation rewrites the file. This is intentionally simple (not the
/// production SQLite backend) but satisfies persist-and-reload requirements and
/// is handy for desktop dev.  Available with the default `fs` feature.
#[cfg(feature = "fs")]
pub struct JsonFileStore {
    path: std::path::PathBuf,
    mem: MemoryStore,
}

#[cfg(feature = "fs")]
impl JsonFileStore {
    /// Open (or create) a store at `path`. If the file exists it is loaded.
    pub fn open(path: impl Into<std::path::PathBuf>) -> Result<Self, StoreError> {
        let path = path.into();
        let mem = if path.exists() {
            let text = std::fs::read_to_string(&path).map_err(|e| StoreError::Io(e.to_string()))?;
            if text.trim().is_empty() {
                MemoryStore::new()
            } else {
                MemoryStore::from_json(&text)?
            }
        } else {
            MemoryStore::new()
        };
        Ok(Self { path, mem })
    }

    fn flush(&self) -> Result<(), StoreError> {
        let json = self.mem.to_json()?;
        std::fs::write(&self.path, json).map_err(|e| StoreError::Io(e.to_string()))
    }
}

#[cfg(feature = "fs")]
impl StrokeStore for JsonFileStore {
    fn save(&mut self, stroke: &Stroke) -> Result<(), StoreError> {
        self.mem.save(stroke)?;
        self.flush()
    }

    fn load(&self, id: StrokeId) -> Result<Stroke, StoreError> {
        self.mem.load(id)
    }

    fn load_all(&self) -> Result<Vec<Stroke>, StoreError> {
        self.mem.load_all()
    }

    fn remove(&mut self, id: StrokeId) -> Result<bool, StoreError> {
        let existed = self.mem.remove(id)?;
        self.flush()?;
        Ok(existed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Color, Point, Tool};

    fn sample_stroke() -> Stroke {
        let mut s = Stroke::new(Tool::Pen, Color::BLACK, 2.0);
        s.points.push(Point::new(0.0, 0.0, 0.5, 0.0, 0));
        s.points.push(Point::new(5.0, 5.0, 0.7, 0.0, 8));
        s
    }

    #[test]
    fn memory_store_save_and_load() {
        let mut store = MemoryStore::new();
        let s = sample_stroke();
        store.save(&s).unwrap();
        assert_eq!(store.load(s.id).unwrap(), s);
        assert_eq!(store.load_all().unwrap(), vec![s]);
    }

    #[test]
    fn memory_store_preserves_order() {
        let mut store = MemoryStore::new();
        let a = sample_stroke();
        let b = sample_stroke();
        store.save(&a).unwrap();
        store.save(&b).unwrap();
        let all = store.load_all().unwrap();
        assert_eq!(all[0].id, a.id);
        assert_eq!(all[1].id, b.id);
    }

    #[test]
    fn memory_store_replace_by_id_keeps_position() {
        let mut store = MemoryStore::new();
        let a = sample_stroke();
        let b = sample_stroke();
        store.save(&a).unwrap();
        store.save(&b).unwrap();

        let mut a2 = a.clone();
        a2.base_width = 9.0;
        store.save(&a2).unwrap();

        let all = store.load_all().unwrap();
        assert_eq!(all.len(), 2, "replacing by id must not add a new stroke");
        assert_eq!(all[0].base_width, 9.0);
        assert_eq!(all[0].id, a.id);
    }

    #[test]
    fn memory_store_remove() {
        let mut store = MemoryStore::new();
        let s = sample_stroke();
        store.save(&s).unwrap();
        assert!(store.remove(s.id).unwrap());
        assert!(!store.remove(s.id).unwrap());
        assert!(store.load(s.id).is_err());
    }

    #[test]
    fn json_round_trip_reloads_identically() {
        let mut store = MemoryStore::new();
        let s = sample_stroke();
        store.save(&s).unwrap();

        let json = store.to_json().unwrap();
        let reloaded = MemoryStore::from_json(&json).unwrap();
        assert_eq!(reloaded.load_all().unwrap(), vec![s]);
    }

    #[cfg(feature = "fs")]
    #[test]
    fn json_file_store_persists_and_reloads() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ephemeris-strokes-test-{}.json", uuid::Uuid::new_v4()));

        let s = sample_stroke();
        {
            let mut store = JsonFileStore::open(&path).unwrap();
            store.save(&s).unwrap();
        } // dropped: file written

        // Reopen from scratch and confirm the stroke reloads identically.
        let store2 = JsonFileStore::open(&path).unwrap();
        assert_eq!(store2.load(s.id).unwrap(), s);
        assert_eq!(store2.load_all().unwrap(), vec![s]);

        let _ = std::fs::remove_file(&path);
    }
}
