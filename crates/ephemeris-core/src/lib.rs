//! Ephemeris core — platform-agnostic domain models and logic.
//!
//! No UI, no platform. The pure model + ink engine is I/O-free; the optional
//! `fs` feature adds a JSON-on-disk [`store::JsonFileStore`] for desktop dev
//! and persist-and-reload tests until the SQLite backend (ephemeris-fna.3)
//! lands.

pub mod geom;
pub mod ink;
pub mod model;
pub mod store;

pub use ink::{InkConfig, InkEngine, InkUpdate};
pub use model::{
    Account, AccountId, AccountKind, CalendarEvent, Color, EventId, Note, NoteId, Page, PageId,
    PageTemplate, Point, Profile, ProfileId, Stroke, StrokeId, Task, TaskId, TaskPriority, Tool,
};
