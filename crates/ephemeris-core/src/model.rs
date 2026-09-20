//! Ephemeris domain models (ADR ephemeris-cm7, ephemeris-fna.2).
//!
//! Core data types for notes, inking, tasks, calendars, and accounts. All types
//! are pure data: `Serialize`/`Deserialize`, `Send + Sync`, no I/O.
//!
//! ## Model Overview
//!
//! - [`Profile`]: User profile (collection of accounts and settings).
//! - [`Account`]: External account (email, caldav, obsidian, etc.).
//! - [`Note`]: Document container for pages.
//! - [`Page`]: Holds ink strokes; can have a template (blank, grid, dot).
//! - [`Stroke`], [`Point`]: Ink stroke with pressure/tilt (ADR ephemeris-cm7).
//! - [`Task`]: Task from any source (local, CalDAV, Obsidian).
//! - [`CalendarEvent`]: Calendar event from any source (ICS feed, CalDAV).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// Newtype id for a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProfileId(pub Uuid);

impl ProfileId {
    pub fn new() -> Self {
        ProfileId(Uuid::new_v4())
    }
}

impl Default for ProfileId {
    fn default() -> Self {
        ProfileId::new()
    }
}

/// Newtype id for an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccountId(pub Uuid);

impl AccountId {
    pub fn new() -> Self {
        AccountId(Uuid::new_v4())
    }
}

impl Default for AccountId {
    fn default() -> Self {
        AccountId::new()
    }
}

/// Newtype id for a note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NoteId(pub Uuid);

impl NoteId {
    pub fn new() -> Self {
        NoteId(Uuid::new_v4())
    }
}

impl Default for NoteId {
    fn default() -> Self {
        NoteId::new()
    }
}

/// Newtype id for a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PageId(pub Uuid);

impl PageId {
    pub fn new() -> Self {
        PageId(Uuid::new_v4())
    }
}

impl Default for PageId {
    fn default() -> Self {
        PageId::new()
    }
}

/// Newtype id for a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(pub Uuid);

impl TaskId {
    pub fn new() -> Self {
        TaskId(Uuid::new_v4())
    }
}

impl Default for TaskId {
    fn default() -> Self {
        TaskId::new()
    }
}

/// Newtype id for a calendar event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EventId(pub Uuid);

impl EventId {
    pub fn new() -> Self {
        EventId(Uuid::new_v4())
    }
}

impl Default for EventId {
    fn default() -> Self {
        EventId::new()
    }
}

/// Newtype id for a stroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StrokeId(pub Uuid);

impl StrokeId {
    /// Generate a fresh random id.
    pub fn new() -> Self {
        StrokeId(Uuid::new_v4())
    }
}

impl Default for StrokeId {
    fn default() -> Self {
        StrokeId::new()
    }
}

/// A user profile: collection of accounts and workspace state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub id: ProfileId,
    pub name: String,
}

impl Profile {
    pub fn new(name: impl Into<String>) -> Self {
        Profile {
            id: ProfileId::new(),
            name: name.into(),
        }
    }
}

/// External account kinds (email, CalDAV, Obsidian, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccountKind {
    Email,
    CalDAV,
    Obsidian,
    WebDAV,
    Local,
}

/// An external account (email, caldav, obsidian vault, etc.).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    pub profile_id: ProfileId,
    pub kind: AccountKind,
    pub name: String,
    pub config: HashMap<String, String>,
}

impl Account {
    pub fn new(profile_id: ProfileId, kind: AccountKind, name: impl Into<String>) -> Self {
        Account {
            id: AccountId::new(),
            profile_id,
            kind,
            name: name.into(),
            config: HashMap::new(),
        }
    }
}

/// A note: document container for pages (ink canvas).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub id: NoteId,
    pub title: String,
    pub created: u64,
    pub updated: u64,
}

impl Note {
    pub fn new(title: impl Into<String>, now: u64) -> Self {
        Note {
            id: NoteId::new(),
            title: title.into(),
            created: now,
            updated: now,
        }
    }
}

/// Page template hint (blank, lines, grid, dot matrix).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PageTemplate {
    #[default]
    Blank,
    Lines,
    Grid,
    Dot,
}

/// A page: canvas for ink strokes, indexed within a note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    pub id: PageId,
    pub note_id: NoteId,
    pub index: usize,
    pub template: PageTemplate,
}

impl Page {
    pub fn new(note_id: NoteId, index: usize, template: PageTemplate) -> Self {
        Page {
            id: PageId::new(),
            note_id,
            index,
            template,
        }
    }
}

/// Task priority level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum TaskPriority {
    Low = 0,
    #[default]
    Medium = 1,
    High = 2,
}

/// A task from any source (local, CalDAV VTODO, Obsidian checkbox, etc.).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub title: String,
    pub due: Option<u64>,
    pub priority: TaskPriority,
    pub tags: Vec<String>,
    pub source: String,
    pub profile_id: ProfileId,
    pub done: bool,
}

impl Task {
    pub fn new(title: impl Into<String>, profile_id: ProfileId) -> Self {
        Task {
            id: TaskId::new(),
            title: title.into(),
            due: None,
            priority: TaskPriority::default(),
            tags: Vec::new(),
            source: "local".to_string(),
            profile_id,
            done: false,
        }
    }
}

/// A calendar event from any source (ICS feed, CalDAV, etc.).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub id: EventId,
    pub uid: String,
    pub start: u64,
    pub end: u64,
    pub title: String,
    pub location: Option<String>,
    pub source: String,
    pub profile_id: ProfileId,
}

impl CalendarEvent {
    pub fn new(
        uid: impl Into<String>,
        start: u64,
        end: u64,
        title: impl Into<String>,
        profile_id: ProfileId,
    ) -> Self {
        CalendarEvent {
            id: EventId::new(),
            uid: uid.into(),
            start,
            end,
            title: title.into(),
            location: None,
            source: "calendar".to_string(),
            profile_id,
        }
    }
}

/// The drawing tool that produced a stroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Tool {
    /// Standard ink pen.
    #[default]
    Pen,
    /// Highlighter (semi-transparent, wider).
    Highlighter,
    /// Eraser stroke (recorded so erasing is itself undoable/replayable).
    Eraser,
}

/// An sRGB colour with straight alpha, each channel in `[0, 255]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const BLACK: Color = Color {
        r: 0,
        g: 0,
        b: 0,
        a: 255,
    };
    pub const WHITE: Color = Color {
        r: 255,
        g: 255,
        b: 255,
        a: 255,
    };

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Color { r, g, b, a: 255 }
    }
}

impl Default for Color {
    fn default() -> Self {
        Color::BLACK
    }
}

/// One sample along a stroke's centre line.
///
/// * `x`, `y` — position in logical pixels.
/// * `pressure` — normalised `[0.0, 1.0]` tip pressure at this sample.
/// * `tilt` — pen tilt in degrees `[-90.0, 90.0]`.
/// * `t_ms` — timestamp in milliseconds relative to stroke start.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
    pub tilt: f32,
    pub t_ms: u32,
}

impl Point {
    pub fn new(x: f32, y: f32, pressure: f32, tilt: f32, t_ms: u32) -> Self {
        Point {
            x,
            y,
            pressure,
            tilt,
            t_ms,
        }
    }
}

/// A completed (or in-progress) ink stroke.
///
/// The `points` are the *smoothed* centre-line samples that the engine
/// produced from raw input.  `base_width` is the nominal pen width in logical
/// pixels; the per-point rendered width is derived from `base_width` and each
/// point's pressure by the render layer (see `ink::width`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stroke {
    pub id: StrokeId,
    pub tool: Tool,
    pub color: Color,
    /// Nominal stroke width in logical pixels (pressure scales around this).
    pub base_width: f32,
    pub points: Vec<Point>,
}

impl Stroke {
    /// Create an empty stroke with a fresh id.
    pub fn new(tool: Tool, color: Color, base_width: f32) -> Self {
        Stroke {
            id: StrokeId::new(),
            tool,
            color,
            base_width,
            points: Vec::new(),
        }
    }
}

// Compile-time assertion that all core model types are thread-safe.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    // ID types
    assert_send_sync::<ProfileId>();
    assert_send_sync::<AccountId>();
    assert_send_sync::<NoteId>();
    assert_send_sync::<PageId>();
    assert_send_sync::<TaskId>();
    assert_send_sync::<EventId>();
    assert_send_sync::<StrokeId>();
    // Domain types
    assert_send_sync::<Profile>();
    assert_send_sync::<Account>();
    assert_send_sync::<AccountKind>();
    assert_send_sync::<Note>();
    assert_send_sync::<Page>();
    assert_send_sync::<PageTemplate>();
    assert_send_sync::<Task>();
    assert_send_sync::<TaskPriority>();
    assert_send_sync::<CalendarEvent>();
    // Ink types
    assert_send_sync::<Stroke>();
    assert_send_sync::<Point>();
    assert_send_sync::<Tool>();
    assert_send_sync::<Color>();
};

#[cfg(test)]
mod tests {
    use super::*;

    // ID type tests
    #[test]
    fn profile_id_round_trip() {
        let id = ProfileId::new();
        let json = serde_json::to_string(&id).unwrap();
        let back: ProfileId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn account_id_round_trip() {
        let id = AccountId::new();
        let json = serde_json::to_string(&id).unwrap();
        let back: AccountId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn note_id_round_trip() {
        let id = NoteId::new();
        let json = serde_json::to_string(&id).unwrap();
        let back: NoteId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn page_id_round_trip() {
        let id = PageId::new();
        let json = serde_json::to_string(&id).unwrap();
        let back: PageId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn task_id_round_trip() {
        let id = TaskId::new();
        let json = serde_json::to_string(&id).unwrap();
        let back: TaskId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn event_id_round_trip() {
        let id = EventId::new();
        let json = serde_json::to_string(&id).unwrap();
        let back: EventId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    // Domain type tests
    #[test]
    fn profile_serde_round_trip() {
        let profile = Profile::new("Work");
        let json = serde_json::to_string(&profile).unwrap();
        let back: Profile = serde_json::from_str(&json).unwrap();
        assert_eq!(profile, back);
        assert_eq!(profile.name, "Work");
    }

    #[test]
    fn account_serde_round_trip() {
        let profile_id = ProfileId::new();
        let mut account = Account::new(profile_id, AccountKind::CalDAV, "work@example.com");
        account
            .config
            .insert("url".to_string(), "https://example.com/caldav".to_string());

        let json = serde_json::to_string(&account).unwrap();
        let back: Account = serde_json::from_str(&json).unwrap();
        assert_eq!(account, back);
        assert_eq!(back.kind, AccountKind::CalDAV);
    }

    #[test]
    fn note_serde_round_trip() {
        let note = Note::new("My Notes", 1000);
        let json = serde_json::to_string(&note).unwrap();
        let back: Note = serde_json::from_str(&json).unwrap();
        assert_eq!(note, back);
        assert_eq!(back.title, "My Notes");
    }

    #[test]
    fn page_serde_round_trip() {
        let note_id = NoteId::new();
        let page = Page::new(note_id, 0, PageTemplate::Grid);
        let json = serde_json::to_string(&page).unwrap();
        let back: Page = serde_json::from_str(&json).unwrap();
        assert_eq!(page, back);
        assert_eq!(back.template, PageTemplate::Grid);
    }

    #[test]
    fn task_serde_round_trip() {
        let profile_id = ProfileId::new();
        let mut task = Task::new("Write report", profile_id);
        task.priority = TaskPriority::High;
        task.tags.push("urgent".to_string());
        task.due = Some(2000);

        let json = serde_json::to_string(&task).unwrap();
        let back: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(task, back);
        assert_eq!(back.priority, TaskPriority::High);
        assert!(!back.done);
    }

    #[test]
    fn calendar_event_serde_round_trip() {
        let profile_id = ProfileId::new();
        let mut event = CalendarEvent::new("uid-12345", 1000, 2000, "Team Meeting", profile_id);
        event.location = Some("Conference Room A".to_string());

        let json = serde_json::to_string(&event).unwrap();
        let back: CalendarEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
        assert_eq!(back.location, Some("Conference Room A".to_string()));
    }

    #[test]
    fn account_kind_serde_all_variants() {
        for kind in [
            AccountKind::Email,
            AccountKind::CalDAV,
            AccountKind::Obsidian,
            AccountKind::WebDAV,
            AccountKind::Local,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(kind, serde_json::from_str::<AccountKind>(&json).unwrap());
        }
    }

    #[test]
    fn page_template_serde_all_variants() {
        for template in [PageTemplate::Blank, PageTemplate::Grid, PageTemplate::Dot] {
            let json = serde_json::to_string(&template).unwrap();
            assert_eq!(
                template,
                serde_json::from_str::<PageTemplate>(&json).unwrap()
            );
        }
    }

    #[test]
    fn task_priority_serde_all_variants() {
        for priority in [TaskPriority::Low, TaskPriority::Medium, TaskPriority::High] {
            let json = serde_json::to_string(&priority).unwrap();
            assert_eq!(
                priority,
                serde_json::from_str::<TaskPriority>(&json).unwrap()
            );
        }
    }

    // Ink types (existing tests preserved)
    #[test]
    fn stroke_serde_round_trip() {
        let mut s = Stroke::new(Tool::Pen, Color::BLACK, 2.5);
        s.points.push(Point::new(1.0, 2.0, 0.5, 10.0, 0));
        s.points.push(Point::new(3.0, 4.0, 0.7, 12.0, 16));

        let json = serde_json::to_string(&s).unwrap();
        let back: Stroke = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn point_serde_round_trip() {
        let p = Point::new(1.5, -2.5, 0.33, -45.0, 1234);
        let json = serde_json::to_string(&p).unwrap();
        let back: Point = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn tool_and_color_round_trip() {
        for tool in [Tool::Pen, Tool::Highlighter, Tool::Eraser] {
            let json = serde_json::to_string(&tool).unwrap();
            assert_eq!(tool, serde_json::from_str::<Tool>(&json).unwrap());
        }
        let c = Color::rgb(10, 20, 30);
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(c, serde_json::from_str::<Color>(&json).unwrap());
    }
}
