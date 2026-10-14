//! Hand-drawn checkboxes → Ephemeris tasks (ephemeris-fmt.11).
//!
//! Tasks are derived straight from a page's [`PageRecognitionCache`], not from
//! the exported vault file (the export subdir stays excluded from vault task
//! scanning, so there is no duplicate and no write-back into generated files).
//!
//! # Rules
//! * **Id**: UUID v5 over `(note_id, doc_page, anchor stroke id)` where the anchor
//!   is the smallest stroke id of the line. Adding words or strokes to the line
//!   keeps the anchor (and the task); only erasing the anchor stroke itself
//!   changes the id. Re-recognition never changes strokes, so it never duplicates.
//! * **Source**: `source = "note"`, `source_ref = "note:{id}:p{page}:{anchor}"`.
//! * **Title**: [`CachedLine::effective_text`] (user corrections included).
//! * **Done**: taken from the ink on creation. Afterwards ink wins only when the
//!   ink state itself changed since the last sync (`ink_seen`); an app tick on an
//!   unchanged ink state is kept. The app never alters ink.
//! * **Erase**: a task whose checkbox line is gone from the page is deleted, but
//!   only while no line of the page is pending (a pending line looks "missing").
//!   Exception: a task in [`NoteTaskState::user_edited`] (ticked or edited in the
//!   app) is kept as an orphan, so erasing the ink never destroys user work.
//!   Uncommitted, unedited note tasks are still deleted on erase.
//! * **Persistence**: tasks plus [`NoteTaskState`] are saved by
//!   [`crate::local_task_store`], so app ticks and `ink_seen` survive restarts.

use crate::local_task_store::NoteTaskState;
use crate::model::{ProfileId, StrokeId, Task, TaskId};
use crate::recognition_cache::{CachedLine, PageRecognitionCache};
use crate::text_format::BlockKind;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// `Task::source` value for tasks derived from handwritten checkboxes.
pub const NOTE_TASK_SOURCE: &str = "note";

/// Namespace for stable handwritten-checkbox task ids.
const NOTE_TASK_NS: Uuid = Uuid::from_bytes([
    0x65, 0x70, 0x68, 0x65, 0x6d, 0x65, 0x72, 0x69, // "ephemeri"
    0x73, 0x2d, 0x6e, 0x6f, 0x74, 0x65, 0x74, 0x6b, // "s-notetk"
]);

/// Smallest stroke id of a line: stable while strokes are only added.
fn anchor_stroke(line: &CachedLine) -> Option<StrokeId> {
    line.stroke_ids.iter().copied().min_by_key(|s| s.0)
}

/// Stable task id for a checkbox line.
pub fn note_task_id(note_id: u32, doc_page: usize, anchor: StrokeId) -> TaskId {
    let key = format!("{note_id}\0{doc_page}\0{}", anchor.0);
    TaskId(Uuid::new_v5(&NOTE_TASK_NS, key.as_bytes()))
}

/// `source_ref` pointing back at note / page / line anchor.
pub fn note_task_source_ref(note_id: u32, doc_page: usize, anchor: StrokeId) -> String {
    format!("note:{note_id}:p{doc_page}:{}", anchor.0)
}

/// Parse `(note_id, doc_page)` out of a [`note_task_source_ref`] string.
pub fn parse_note_source_ref(source_ref: &str) -> Option<(u32, usize)> {
    let rest = source_ref.strip_prefix("note:")?;
    let mut parts = rest.split(':');
    let note_id = parts.next()?.parse().ok()?;
    let page = parts.next()?.strip_prefix('p')?.parse().ok()?;
    Some((note_id, page))
}

/// Desired tasks for a page: one per completed, non-empty checkbox line.
/// `done` is the ink state.
pub fn checkbox_tasks_for_page(
    note_id: u32,
    doc_page: usize,
    profile: ProfileId,
    cache: &PageRecognitionCache,
) -> Vec<Task> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for line in &cache.lines {
        let BlockKind::Checkbox(checked) = line.effective_format().block else {
            continue;
        };
        let title = line.effective_text().trim();
        if title.is_empty() || (line.pending && !line.has_correction()) {
            continue;
        }
        let Some(anchor) = anchor_stroke(line) else {
            continue;
        };
        let id = note_task_id(note_id, doc_page, anchor);
        if !seen.insert(id) {
            continue;
        }
        let mut t = Task::new(title, profile);
        t.id = id;
        t.source = NOTE_TASK_SOURCE.to_string();
        t.source_ref = Some(note_task_source_ref(note_id, doc_page, anchor));
        t.set_done(checked);
        out.push(t);
    }
    out
}

/// Task mutations needed to match a page's checkboxes.
#[derive(Debug, Default, PartialEq)]
pub struct NoteTaskChanges {
    pub creates: Vec<Task>,
    pub updates: Vec<Task>,
    pub deletes: Vec<TaskId>,
}

impl NoteTaskChanges {
    pub fn is_empty(&self) -> bool {
        self.creates.is_empty() && self.updates.is_empty() && self.deletes.is_empty()
    }
}

/// Diff `desired` (from [`checkbox_tasks_for_page`]) against `existing` tasks.
///
/// `state.ink_seen` maps task id → last-seen ink tick state and is updated in
/// place; `state.user_edited` tasks are never deleted. Only tasks of this note page are considered for update / delete.
pub fn reconcile_note_tasks(
    note_id: u32,
    doc_page: usize,
    desired: &[Task],
    existing: &[Task],
    state: &mut NoteTaskState,
    allow_delete: bool,
) -> NoteTaskChanges {
    let mut changes = NoteTaskChanges::default();
    let mine: HashMap<TaskId, &Task> = existing
        .iter()
        .filter(|t| {
            t.source == NOTE_TASK_SOURCE
                && t.source_ref.as_deref().and_then(parse_note_source_ref)
                    == Some((note_id, doc_page))
        })
        .map(|t| (t.id, t))
        .collect();

    for want in desired {
        let ink = want.done;
        let Some(have) = mine.get(&want.id) else {
            state.ink_seen.insert(want.id, ink);
            changes.creates.push(want.clone());
            continue;
        };
        let mut next = (*have).clone();
        if next.title != want.title {
            next.title = want.title.clone();
        }
        next.profile_id = want.profile_id;
        if state.ink_seen.get(&want.id) != Some(&ink) {
            next.set_done(ink);
        }
        state.ink_seen.insert(want.id, ink);
        if next != **have {
            changes.updates.push(next);
        }
    }
    if allow_delete {
        let wanted: HashSet<TaskId> = desired.iter().map(|t| t.id).collect();
        for id in mine.keys() {
            if !wanted.contains(id) && !state.user_edited.contains(id) {
                state.ink_seen.remove(id);
                changes.deletes.push(*id);
            }
        }
        changes.deletes.sort_by_key(|id| id.0);
    }
    changes
}

/// Reconcile one page cache into `tasks`; returns true when `tasks` changed.
pub fn sync_page_tasks(
    tasks: &mut Vec<Task>,
    state: &mut NoteTaskState,
    note_id: u32,
    doc_page: usize,
    profile: ProfileId,
    cache: &PageRecognitionCache,
) -> bool {
    let desired = checkbox_tasks_for_page(note_id, doc_page, profile, cache);
    let changes = reconcile_note_tasks(
        note_id,
        doc_page,
        &desired,
        tasks,
        state,
        !cache.has_pending(),
    );
    if changes.is_empty() {
        return false;
    }
    tasks.retain(|t| !changes.deletes.contains(&t.id));
    for u in changes.updates {
        if let Some(slot) = tasks.iter_mut().find(|t| t.id == u.id) {
            *slot = u;
        }
    }
    tasks.extend(changes.creates);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text_format::LineFormat;

    fn sid(n: u128) -> StrokeId {
        StrokeId(Uuid::from_u128(n))
    }

    fn line(ids: &[u128], text: &str, block: BlockKind) -> CachedLine {
        let stroke_ids: Vec<StrokeId> = ids.iter().map(|&n| sid(n)).collect();
        CachedLine {
            stroke_ids_hash: crate::recognition_cache::line_stroke_ids_hash(&stroke_ids),
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
                block,
                ..LineFormat::default()
            },
            user_text: None,
            user_format: None,
            user_stale: false,
        }
    }

    fn cache(lines: Vec<CachedLine>) -> PageRecognitionCache {
        PageRecognitionCache {
            model_version: "t".into(),
            lines,
        }
    }

    fn two_boxes() -> PageRecognitionCache {
        cache(vec![
            line(&[1, 2], "buy milk", BlockKind::Checkbox(false)),
            line(&[3], "plain", BlockKind::Paragraph),
            line(&[5, 4], "call bob", BlockKind::Checkbox(true)),
        ])
    }

    fn sync(tasks: &mut Vec<Task>, seen: &mut NoteTaskState, c: &PageRecognitionCache) {
        sync_page_tasks(tasks, seen, 7, 0, ProfileId(Uuid::nil()), c);
    }

    #[test]
    fn two_checkboxes_yield_two_tasks_with_done_state() {
        let p = ProfileId(Uuid::nil());
        let ts = checkbox_tasks_for_page(7, 0, p, &two_boxes());
        assert_eq!(ts.len(), 2);
        assert_eq!((ts[0].title.as_str(), ts[0].done), ("buy milk", false));
        assert_eq!((ts[1].title.as_str(), ts[1].done), ("call bob", true));
        assert_eq!(ts[0].source, NOTE_TASK_SOURCE);
        assert_eq!(
            parse_note_source_ref(ts[1].source_ref.as_deref().unwrap()),
            Some((7, 0))
        );
    }

    #[test]
    fn rerun_does_not_duplicate() {
        let (mut tasks, mut seen) = (Vec::new(), NoteTaskState::default());
        sync(&mut tasks, &mut seen, &two_boxes());
        assert!(!sync_page_tasks(
            &mut tasks,
            &mut seen,
            7,
            0,
            ProfileId(Uuid::nil()),
            &two_boxes()
        ));
        assert_eq!(tasks.len(), 2);
    }

    #[test]
    fn id_stable_when_strokes_added_but_distinct_per_page_and_note() {
        let a = note_task_id(7, 0, sid(1));
        assert_eq!(a, note_task_id(7, 0, sid(1)));
        assert_ne!(a, note_task_id(7, 1, sid(1)));
        assert_ne!(a, note_task_id(8, 0, sid(1)));
        let mut c = two_boxes();
        c.lines[0] = line(&[1, 2, 9], "buy milk now", BlockKind::Checkbox(false));
        let (mut tasks, mut seen) = (Vec::new(), NoteTaskState::default());
        sync(&mut tasks, &mut seen, &two_boxes());
        sync(&mut tasks, &mut seen, &c);
        assert_eq!(tasks.len(), 2);
        assert!(tasks.iter().any(|t| t.title == "buy milk now"));
    }

    #[test]
    fn erased_checkbox_is_deleted_unless_pending() {
        let (mut tasks, mut seen) = (Vec::new(), NoteTaskState::default());
        sync(&mut tasks, &mut seen, &two_boxes());
        let mut c = two_boxes();
        c.lines.remove(0);
        let mut pending = c.clone();
        pending.lines.push(CachedLine {
            pending: true,
            text: String::new(),
            ..line(&[20], "", BlockKind::Paragraph)
        });
        sync(&mut tasks, &mut seen, &pending);
        assert_eq!(tasks.len(), 2, "pending page defers deletes");
        sync(&mut tasks, &mut seen, &c);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "call bob");
    }

    #[test]
    fn user_edited_task_is_kept_on_erase() {
        let (mut tasks, mut seen) = (Vec::new(), NoteTaskState::default());
        sync(&mut tasks, &mut seen, &two_boxes());
        let kept = tasks[0].id;
        seen.user_edited.insert(kept);
        let mut c = two_boxes();
        c.lines.drain(0..3);
        sync(&mut tasks, &mut seen, &c);
        assert_eq!(tasks.len(), 1, "unedited call bob deleted, edited kept");
        assert_eq!(tasks[0].id, kept);
    }

    #[test]
    fn text_correction_updates_title() {
        let (mut tasks, mut seen) = (Vec::new(), NoteTaskState::default());
        sync(&mut tasks, &mut seen, &two_boxes());
        let mut c = two_boxes();
        c.lines[0].user_text = Some("buy oat milk".into());
        sync(&mut tasks, &mut seen, &c);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].title, "buy oat milk");
    }

    #[test]
    fn app_tick_survives_unchanged_ink_but_ink_change_wins() {
        let (mut tasks, mut seen) = (Vec::new(), NoteTaskState::default());
        sync(&mut tasks, &mut seen, &two_boxes());
        tasks[0].set_done(true); // ticked in the app
        sync(&mut tasks, &mut seen, &two_boxes());
        assert!(tasks[0].done, "unchanged ink must not revert the app tick");
        let mut c = two_boxes();
        c.lines[0].format.block = BlockKind::Checkbox(true);
        sync(&mut tasks, &mut seen, &c);
        assert!(tasks[0].done);
        c.lines[0].format.block = BlockKind::Checkbox(false);
        sync(&mut tasks, &mut seen, &c);
        assert!(!tasks[0].done, "ink untick wins");
    }

    #[test]
    fn other_pages_and_sources_untouched() {
        let (mut tasks, mut seen) = (Vec::new(), NoteTaskState::default());
        sync(&mut tasks, &mut seen, &two_boxes());
        let other = Task::new("keep", ProfileId(Uuid::nil()));
        tasks.push(other.clone());
        sync_page_tasks(
            &mut tasks,
            &mut seen,
            7,
            1,
            ProfileId(Uuid::nil()),
            &cache(vec![]),
        );
        assert_eq!(tasks.len(), 3);
        assert!(tasks.contains(&other));
    }
}
