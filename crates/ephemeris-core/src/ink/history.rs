//! Per-page undo/redo for note ink (ephemeris-e1b).
//!
//! Stores compact reverse-capable edits (not a full stroke-list clone on every
//! pen sample). Apply the [`ApplyOp`] returned by [`InkHistory::undo`] /
//! [`InkHistory::redo`] to the live stroke list + raster, then run the normal
//! persist / recognition hub.

use crate::model::{Stroke, StrokeId};

/// Default max undo steps per page (e-ink memory budget).
pub const DEFAULT_INK_HISTORY_DEPTH: usize = 24;

/// A recorded forward edit. Kept on the undo stack; redo reapplies it.
#[derive(Debug, Clone, PartialEq)]
pub enum InkEdit {
    /// One or more strokes were added (pen / highlighter commit).
    Add(Vec<Stroke>),
    /// Strokes were removed (eraser gesture). Full stroke data kept for undo.
    Remove(Vec<Stroke>),
    /// Selection was translated by `(dx, dy)`.
    Translate {
        ids: Vec<StrokeId>,
        dx: f32,
        dy: f32,
    },
    /// Page was cleared (`before` = strokes prior to clear).
    Clear { before: Vec<Stroke> },
}

/// Concrete mutation to apply to the live page.
#[derive(Debug, Clone, PartialEq)]
pub enum ApplyOp {
    Add(Vec<Stroke>),
    Remove(Vec<StrokeId>),
    Translate {
        ids: Vec<StrokeId>,
        dx: f32,
        dy: f32,
    },
    /// Replace the entire stroke list (clear undo / clear redo).
    SetAll(Vec<Stroke>),
}

impl InkEdit {
    fn to_undo_op(&self) -> ApplyOp {
        match self {
            InkEdit::Add(strokes) => ApplyOp::Remove(strokes.iter().map(|s| s.id).collect()),
            InkEdit::Remove(strokes) => ApplyOp::Add(strokes.clone()),
            InkEdit::Translate { ids, dx, dy } => ApplyOp::Translate {
                ids: ids.clone(),
                dx: -dx,
                dy: -dy,
            },
            InkEdit::Clear { before } => ApplyOp::SetAll(before.clone()),
        }
    }

    fn to_redo_op(&self) -> ApplyOp {
        match self {
            InkEdit::Add(strokes) => ApplyOp::Add(strokes.clone()),
            InkEdit::Remove(strokes) => ApplyOp::Remove(strokes.iter().map(|s| s.id).collect()),
            InkEdit::Translate { ids, dx, dy } => ApplyOp::Translate {
                ids: ids.clone(),
                dx: *dx,
                dy: *dy,
            },
            InkEdit::Clear { .. } => ApplyOp::SetAll(Vec::new()),
        }
    }
}

/// Undo/redo stacks for one note page.
#[derive(Debug, Clone)]
pub struct InkHistory {
    undo: Vec<InkEdit>,
    redo: Vec<InkEdit>,
    max_depth: usize,
}

impl Default for InkHistory {
    fn default() -> Self {
        Self::new(DEFAULT_INK_HISTORY_DEPTH)
    }
}

impl InkHistory {
    pub fn new(max_depth: usize) -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            max_depth: max_depth.max(1),
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }

    pub fn redo_len(&self) -> usize {
        self.redo.len()
    }

    /// Record a forward edit. Clears the redo stack (standard editor behaviour).
    pub fn push(&mut self, edit: InkEdit) {
        match &edit {
            InkEdit::Add(s) | InkEdit::Remove(s) if s.is_empty() => return,
            InkEdit::Translate { dx, dy, .. } if *dx == 0.0 && *dy == 0.0 => return,
            _ => {}
        }
        self.redo.clear();
        self.undo.push(edit);
        while self.undo.len() > self.max_depth {
            self.undo.remove(0);
        }
    }

    /// Pop one undo step and return the [`ApplyOp`] that restores prior state.
    pub fn undo(&mut self) -> Option<ApplyOp> {
        let edit = self.undo.pop()?;
        let op = edit.to_undo_op();
        self.redo.push(edit);
        Some(op)
    }

    /// Pop one redo step and return the [`ApplyOp`] that re-applies it.
    pub fn redo(&mut self) -> Option<ApplyOp> {
        let edit = self.redo.pop()?;
        let op = edit.to_redo_op();
        self.undo.push(edit);
        Some(op)
    }

    /// Drop all history (e.g. page discarded).
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
}

/// Map of per-page histories keyed by `(note_id, doc_page)`.
#[derive(Debug, Default, Clone)]
pub struct InkHistoryMap {
    pages: std::collections::HashMap<(u32, usize), InkHistory>,
    max_depth: usize,
}

impl InkHistoryMap {
    pub fn new(max_depth: usize) -> Self {
        Self {
            pages: std::collections::HashMap::new(),
            max_depth: max_depth.max(1),
        }
    }

    fn entry(&mut self, note_id: u32, doc_page: usize) -> &mut InkHistory {
        let depth = self.max_depth;
        self.pages
            .entry((note_id, doc_page))
            .or_insert_with(|| InkHistory::new(depth))
    }

    pub fn push(&mut self, note_id: u32, doc_page: usize, edit: InkEdit) {
        self.entry(note_id, doc_page).push(edit);
    }

    pub fn undo(&mut self, note_id: u32, doc_page: usize) -> Option<ApplyOp> {
        self.entry(note_id, doc_page).undo()
    }

    pub fn redo(&mut self, note_id: u32, doc_page: usize) -> Option<ApplyOp> {
        self.entry(note_id, doc_page).redo()
    }

    pub fn can_undo(&self, note_id: u32, doc_page: usize) -> bool {
        self.pages
            .get(&(note_id, doc_page))
            .is_some_and(|h| h.can_undo())
    }

    pub fn can_redo(&self, note_id: u32, doc_page: usize) -> bool {
        self.pages
            .get(&(note_id, doc_page))
            .is_some_and(|h| h.can_redo())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Color, Point, Tool};

    fn stroke(seed: u8, x: f32) -> Stroke {
        let mut s = Stroke::new(Tool::Pen, Color::BLACK, 3.0);
        s.id = StrokeId(uuid::Uuid::from_bytes([
            seed, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, seed,
        ]));
        s.points = vec![
            Point::new(x, 10.0, 0.5, 0.0, 0),
            Point::new(x + 10.0, 10.0, 0.5, 0.0, 10),
        ];
        s
    }

    #[test]
    fn add_undo_redo() {
        let mut h = InkHistory::new(8);
        let s = stroke(1, 0.0);
        let id = s.id;
        h.push(InkEdit::Add(vec![s.clone()]));
        assert!(h.can_undo());
        match h.undo() {
            Some(ApplyOp::Remove(ids)) => assert_eq!(ids, vec![id]),
            other => panic!("unexpected {other:?}"),
        }
        assert!(h.can_redo());
        match h.redo() {
            Some(ApplyOp::Add(strokes)) => assert_eq!(strokes[0].id, id),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn translate_inverts() {
        let mut h = InkHistory::new(8);
        let id = stroke(2, 0.0).id;
        h.push(InkEdit::Translate {
            ids: vec![id],
            dx: 5.0,
            dy: -3.0,
        });
        match h.undo() {
            Some(ApplyOp::Translate { dx, dy, .. }) => {
                assert!((dx - -5.0).abs() < 1e-5);
                assert!((dy - 3.0).abs() < 1e-5);
            }
            other => panic!("unexpected {other:?}"),
        }
        match h.redo() {
            Some(ApplyOp::Translate { dx, dy, .. }) => {
                assert!((dx - 5.0).abs() < 1e-5);
                assert!((dy - -3.0).abs() < 1e-5);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn clear_restores_snapshot() {
        let mut h = InkHistory::new(8);
        let before = vec![stroke(3, 1.0), stroke(4, 2.0)];
        h.push(InkEdit::Clear {
            before: before.clone(),
        });
        match h.undo() {
            Some(ApplyOp::SetAll(s)) => assert_eq!(s.len(), 2),
            other => panic!("unexpected {other:?}"),
        }
        match h.redo() {
            Some(ApplyOp::SetAll(s)) => assert!(s.is_empty()),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn depth_truncates() {
        let mut h = InkHistory::new(3);
        for i in 0..5 {
            h.push(InkEdit::Add(vec![stroke(i, i as f32)]));
        }
        assert_eq!(h.undo_len(), 3);
    }

    #[test]
    fn push_clears_redo() {
        let mut h = InkHistory::new(8);
        h.push(InkEdit::Add(vec![stroke(1, 0.0)]));
        let _ = h.undo();
        assert!(h.can_redo());
        h.push(InkEdit::Add(vec![stroke(2, 1.0)]));
        assert!(!h.can_redo());
    }

    #[test]
    fn map_is_per_page() {
        let mut m = InkHistoryMap::new(8);
        m.push(1, 0, InkEdit::Add(vec![stroke(1, 0.0)]));
        assert!(m.can_undo(1, 0));
        assert!(!m.can_undo(1, 1));
        assert!(!m.can_undo(2, 0));
    }
}
