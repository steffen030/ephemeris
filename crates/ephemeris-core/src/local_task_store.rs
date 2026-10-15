//! Persistence model for app-local tasks and handwritten-checkbox tasks
//! (ephemeris-rjz).
//!
//! Obsidian / Todoist / CalDAV tasks are reloaded from their providers and are
//! never stored here. Two kinds of tasks have no other home and would be lost
//! on restart, so the app saves them as one JSON document (`tasks_local.json`):
//! * **local** tasks (`source == "local"`): created in the app;
//! * **note** tasks (`source == "note"`, see [`crate::note_tasks`]): rebuilt from
//!   recognition caches at startup, but their done state / edits and the
//!   last-seen ink state would otherwise revert to the ink.
//!
//! This module is pure (de)serialisation and merge logic; file I/O lives in the
//! app.

use crate::model::{Task, TaskId};
use crate::note_tasks::NOTE_TASK_SOURCE;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// `Task::source` value for tasks created inside the app.
pub const LOCAL_TASK_SOURCE: &str = "local";

/// Current on-disk format version.
pub const LOCAL_TASK_STORE_VERSION: u32 = 1;

/// Per-note-task bookkeeping kept next to the tasks themselves.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NoteTaskState {
    /// Last-seen ink tick state per note task. Ink only overrides the stored
    /// done state when it differs from this value.
    pub ink_seen: HashMap<TaskId, bool>,
    /// Note tasks the user ticked or edited in the app; these survive erasing
    /// their checkbox ink.
    pub user_edited: HashSet<TaskId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct InkSeenEntry {
    id: TaskId,
    ink: bool,
}

/// Serialised form of the local / note task state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LocalTaskStore {
    pub version: u32,
    /// Tasks with `source == "local"`.
    #[serde(default)]
    pub local: Vec<Task>,
    /// Tasks with `source == "note"` (full snapshot incl. app ticks / edits).
    #[serde(default)]
    pub note: Vec<Task>,
    #[serde(default)]
    user_edited: Vec<TaskId>,
    #[serde(default)]
    ink_seen: Vec<InkSeenEntry>,
}

impl LocalTaskStore {
    /// Snapshot the persistable part of `tasks`. Output order is deterministic
    /// (sorted by id for the state lists) so unchanged state serialises equal.
    pub fn from_tasks(tasks: &[Task], state: &NoteTaskState) -> Self {
        let local = tasks
            .iter()
            .filter(|t| t.source == LOCAL_TASK_SOURCE)
            .cloned()
            .collect();
        let note: Vec<Task> = tasks
            .iter()
            .filter(|t| t.source == NOTE_TASK_SOURCE)
            .cloned()
            .collect();
        let live: HashSet<TaskId> = note.iter().map(|t| t.id).collect();
        let mut user_edited: Vec<TaskId> = state
            .user_edited
            .iter()
            .copied()
            .filter(|id| live.contains(id))
            .collect();
        user_edited.sort_by_key(|id| id.0);
        let mut ink_seen: Vec<InkSeenEntry> = state
            .ink_seen
            .iter()
            .filter(|(id, _)| live.contains(id))
            .map(|(&id, &ink)| InkSeenEntry { id, ink })
            .collect();
        ink_seen.sort_by_key(|e| e.id.0);
        Self {
            version: LOCAL_TASK_STORE_VERSION,
            local,
            note,
            user_edited,
            ink_seen,
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// Parse a store; `None` for malformed or newer-format files (caller treats
    /// that as "no store").
    pub fn from_json(json: &str) -> Option<Self> {
        let s: Self = serde_json::from_str(json).ok()?;
        (s.version <= LOCAL_TASK_STORE_VERSION).then_some(s)
    }

    /// Merge into freshly loaded `tasks`: provider tasks stay, any local / note
    /// tasks already present (e.g. sample data) are replaced by the stored ones.
    /// Returns the restored note-task state for the startup sync.
    pub fn apply(self, tasks: &mut Vec<Task>) -> NoteTaskState {
        tasks.retain(|t| t.source != LOCAL_TASK_SOURCE && t.source != NOTE_TASK_SOURCE);
        tasks.extend(self.local);
        tasks.extend(self.note);
        NoteTaskState {
            ink_seen: self.ink_seen.into_iter().map(|e| (e.id, e.ink)).collect(),
            user_edited: self.user_edited.into_iter().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProfileId;
    use crate::note_tasks::{note_task_id, sync_page_tasks};
    use crate::recognition_cache::{line_stroke_ids_hash, CachedLine, PageRecognitionCache};
    use crate::text_format::{BlockKind, LineFormat};
    use crate::StrokeId;
    use uuid::Uuid;

    fn pid() -> ProfileId {
        ProfileId(Uuid::nil())
    }

    fn sid(n: u128) -> StrokeId {
        StrokeId(Uuid::from_u128(n))
    }

    fn box_line(id: u128, text: &str, checked: bool) -> CachedLine {
        let stroke_ids = vec![sid(id)];
        CachedLine {
            stroke_ids_hash: line_stroke_ids_hash(&stroke_ids),
            stroke_ids,
            text: text.into(),
            raw_text: None,
            confidence: 0.9,
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
            pending: false,
            format: LineFormat {
                block: BlockKind::Checkbox(checked),
                ..LineFormat::default()
            },
            user_text: None,
            user_format: None,
            user_stale: false,
        }
    }

    fn page(lines: Vec<CachedLine>) -> PageRecognitionCache {
        PageRecognitionCache {
            model_version: "t".into(),
            lines,
        }
    }

    fn sync(tasks: &mut Vec<Task>, st: &mut NoteTaskState, c: &PageRecognitionCache) {
        sync_page_tasks(tasks, st, 7, 0, pid(), c);
    }

    /// Simulate a restart: serialise, parse, apply onto provider-only tasks.
    fn restart(
        tasks: &[Task],
        st: &NoteTaskState,
        provider: Vec<Task>,
    ) -> (Vec<Task>, NoteTaskState) {
        let json = LocalTaskStore::from_tasks(tasks, st).to_json();
        let store = LocalTaskStore::from_json(&json).expect("parses");
        let mut fresh = provider;
        let st2 = store.apply(&mut fresh);
        (fresh, st2)
    }

    #[test]
    fn round_trip_local_and_provider_tasks() {
        let local = Task::new("mine", pid());
        let mut obs = Task::new("vault", pid());
        obs.source = "obsidian".into();
        let tasks = vec![local.clone(), obs.clone()];
        let (back, _) = restart(&tasks, &NoteTaskState::default(), vec![obs.clone()]);
        assert_eq!(back.len(), 2);
        assert!(back.contains(&local) && back.contains(&obs));
        let json = LocalTaskStore::from_tasks(&tasks, &NoteTaskState::default()).to_json();
        assert!(!json.contains("vault"), "provider tasks are not stored");
    }

    #[test]
    fn apply_replaces_sample_local_tasks() {
        let stored = Task::new("real", pid());
        let json =
            LocalTaskStore::from_tasks(std::slice::from_ref(&stored), &NoteTaskState::default())
                .to_json();
        let mut fresh = vec![Task::new("sample", pid())];
        LocalTaskStore::from_json(&json).unwrap().apply(&mut fresh);
        assert_eq!(fresh, vec![stored]);
    }

    #[test]
    fn app_tick_survives_restart_unless_ink_changed() {
        let c = page(vec![box_line(1, "buy milk", false)]);
        let (mut tasks, mut st) = (Vec::new(), NoteTaskState::default());
        sync(&mut tasks, &mut st, &c);
        tasks[0].set_done(true); // ticked in the app
        st.user_edited.insert(tasks[0].id);

        let (mut tasks, mut st) = restart(&tasks, &st, Vec::new());
        assert!(tasks[0].done);
        sync(&mut tasks, &mut st, &c); // startup rebuild from the same ink
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].done, "unchanged ink must not revert the app tick");

        let (mut tasks, mut st) = restart(&tasks, &st, Vec::new());
        sync(
            &mut tasks,
            &mut st,
            &page(vec![box_line(1, "buy milk", true)]),
        );
        assert!(tasks[0].done);
        let (mut tasks, mut st) = restart(&tasks, &st, Vec::new());
        sync(
            &mut tasks,
            &mut st,
            &page(vec![box_line(1, "buy milk", false)]),
        );
        assert!(!tasks[0].done, "ink untick after restart wins");
    }

    #[test]
    fn without_persisted_ink_seen_restart_would_revert() {
        // Guards the reason ink_seen is stored at all.
        let c = page(vec![box_line(1, "x", false)]);
        let (mut tasks, mut st) = (Vec::new(), NoteTaskState::default());
        sync(&mut tasks, &mut st, &c);
        tasks[0].set_done(true);
        let mut fresh = tasks.clone();
        let mut empty = NoteTaskState::default();
        sync(&mut fresh, &mut empty, &c);
        assert!(!fresh[0].done);
    }

    #[test]
    fn user_edited_note_task_survives_erase_but_plain_one_does_not() {
        let c = page(vec![
            box_line(1, "edited", false),
            box_line(2, "untouched", false),
        ]);
        let (mut tasks, mut st) = (Vec::new(), NoteTaskState::default());
        sync(&mut tasks, &mut st, &c);
        let edited = note_task_id(7, 0, sid(1));
        tasks
            .iter_mut()
            .find(|t| t.id == edited)
            .unwrap()
            .set_done(true);
        st.user_edited.insert(edited);

        let (mut tasks, mut st) = restart(&tasks, &st, Vec::new());
        sync(&mut tasks, &mut st, &page(vec![]));
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, edited);
        assert!(tasks[0].done);
        // And still there after another restart + sync.
        let (mut tasks, mut st) = restart(&tasks, &st, Vec::new());
        sync(&mut tasks, &mut st, &page(vec![]));
        assert_eq!(tasks.len(), 1);
    }

    #[test]
    fn state_for_vanished_tasks_is_pruned() {
        let mut st = NoteTaskState::default();
        let gone = TaskId::new();
        st.ink_seen.insert(gone, true);
        st.user_edited.insert(gone);
        let store = LocalTaskStore::from_tasks(&[], &st);
        assert_eq!(store.apply(&mut Vec::new()), NoteTaskState::default());
    }

    #[test]
    fn serialisation_is_deterministic() {
        let c = page(vec![box_line(1, "a", true), box_line(2, "b", false)]);
        let (mut tasks, mut st) = (Vec::new(), NoteTaskState::default());
        sync(&mut tasks, &mut st, &c);
        let a = LocalTaskStore::from_tasks(&tasks, &st).to_json();
        let b = LocalTaskStore::from_tasks(&tasks, &st).to_json();
        assert_eq!(a, b);
    }

    #[test]
    fn missing_old_or_garbage_file_is_none() {
        assert!(LocalTaskStore::from_json("").is_none());
        assert!(LocalTaskStore::from_json("not json").is_none());
        assert!(LocalTaskStore::from_json(r#"{"version":99}"#).is_none());
        let minimal = LocalTaskStore::from_json(r#"{"version":1}"#).unwrap();
        let mut tasks = vec![Task::new("sample", pid())];
        let st = minimal.apply(&mut tasks);
        assert!(tasks.is_empty());
        assert_eq!(st, NoteTaskState::default());
    }
}
