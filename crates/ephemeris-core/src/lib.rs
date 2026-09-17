//! Ephemeris core — platform-agnostic domain models and logic.
//!
//! No UI, no platform. The pure model + ink engine is I/O-free; the optional
//! `fs` feature adds a JSON-on-disk [`store::JsonFileStore`] for desktop dev
//! and persist-and-reload tests until the SQLite backend (ephemeris-fna.3)
//! lands.

pub mod action;
pub mod config;
pub mod error;
pub mod geom;
pub mod ics;
pub mod ink;
pub mod model;
pub mod sqlite;
pub mod store;

pub use config::Config;
pub use error::{AppError, Result};
pub use ink::{InkConfig, InkEngine, InkUpdate};
pub use model::{
    Account, AccountId, AccountKind, CalendarEvent, Color, EventId, Note, NoteId, Page, PageId,
    PageTemplate, Point, Profile, ProfileId, Stroke, StrokeId, Task, TaskId, TaskPriority, Tool,
};

/// Initialize tracing with environment filter.
pub fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(true)
        .with_thread_ids(true)
        .init();
}
pub mod webdav;
pub mod task_provider;
