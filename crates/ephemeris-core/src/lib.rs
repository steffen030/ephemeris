//! Ephemeris core — platform-agnostic domain models and logic.
//!
//! No UI, no platform. The pure model + ink engine is I/O-free; the optional
//! `fs` feature adds a JSON-on-disk [`store::JsonFileStore`] for desktop dev
//! and persist-and-reload tests until the SQLite backend (ephemeris-fna.3)
//! lands.

pub mod action;
pub mod agenda;
#[cfg(feature = "sqlite")]
pub mod aggregation;
pub mod config;
pub mod error;
pub mod geom;
pub mod ics;
pub mod ink;
pub mod model;
pub mod refresh;
#[cfg(feature = "sqlite")]
pub mod sqlite;
pub mod store;

pub use action::{Action, ActionMap, ActionRouter, CanvasPoint, DefaultActionMap, RouterOutput};
pub use config::Config;
pub use error::{AppError, Result};
pub use ink::{EraserConfig, EraserEngine, EraserUpdate, InkConfig, InkEngine, InkUpdate};
pub use model::{
    Account, AccountId, AccountKind, CalendarEvent, Color, EventId, Note, NoteId, Page, PageId,
    PageTemplate, Point, Profile, ProfileId, Stroke, StrokeId, Task, TaskId, TaskPriority,
    TaskStatus, Tool,
};
pub use refresh::{DamageSource, Present, RefreshConfig, RefreshScheduler};

/// Initialize tracing with environment filter.
pub fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(true)
        .with_thread_ids(true)
        .init();
}
pub mod calendar_cache;
pub mod markdown_tasks;
pub mod note_search;
pub mod obsidian;
pub mod pdf_export;
pub mod prioritizer;
pub mod profile_manager;
pub mod stroke_persistence;
pub mod task_cache;
pub mod task_provider;
pub mod transcriber;
pub mod vault_export;
pub use agenda::{filter_agenda, to_agenda_entries, AgendaEntry, AgendaRange};
#[cfg(feature = "sqlite")]
pub use aggregation::AggregationService;
pub use calendar_cache::CalendarCache;
pub use markdown_tasks::{MarkdownTaskExtractor, DEFAULT_TASKS_INBOX};
pub use note_search::{
    markdown_to_blocks, markdown_to_plain, search_events, search_local_notes, search_tasks,
    strip_inline_markdown, MdBlock, MdBlockKind, SearchHit, VaultFtsIndex,
};
pub use pdf_export::{
    export_raster_pages_to_pdf, export_stroke_pages_to_pdf, pdf_export_is_current,
    raster_export_hash, PdfExportOptions, RasterPage,
};
pub use prioritizer::{rank_tasks, task_score};
pub use task_cache::TaskCache;
#[cfg(feature = "sqlite")]
pub use task_provider::LocalTaskProvider;
pub use task_provider::{ProviderCapabilities, TaskProvider};
pub use transcriber::{
    parse_tesseract_tsv, FakeTranscriber, NoOpTranscriber, RasterOcrTranscriber, TextSpan,
    Transcriber,
};
pub use vault_export::{
    export_note_markdown, sync_notes_to_vault, VaultNoteExport, DEFAULT_EXPORT_SUBDIR,
};
#[cfg(feature = "webdav")]
pub mod webdav;
