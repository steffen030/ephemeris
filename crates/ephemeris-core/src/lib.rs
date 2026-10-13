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
pub use config::{format_note_title, Config, TaskEngageConfig, TaskLaneConfig};
pub use error::{AppError, Result};
pub use ink::{EraserConfig, EraserEngine, EraserUpdate, InkConfig, InkEngine, InkUpdate};
pub use model::{
    Account, AccountId, AccountKind, CalendarEvent, Color, EnergyLevel, EventId, Note, NoteId,
    Page, PageId, PageTemplate, Point, Profile, ProfileId, Stroke, StrokeId, Task, TaskContext,
    TaskContextKind, TaskId, TaskPriority, TaskStatus, Tool,
};
pub use refresh::{DamageSource, Present, RefreshConfig, RefreshScheduler};
pub use task_context::{
    apply_parsed_tags, apply_todoist_fields, apply_todoist_labels, count_lane, is_stale,
    list_engage, mark_reviewed, parse_tags_and_mentions, project_from_vault_path, resolve_lane,
    review_queue, set_lane_tag, sync_tags_from_structured, todoist_priority,
    todoist_section_to_lane, EngageFilter, TodoistTaskFields, LANE_TAG_PREFIX,
};

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
#[cfg(feature = "caldav")]
pub mod caldav;
pub mod calendar_cache;
pub mod line_htr;
pub mod markdown_tasks;
pub mod note_search;
pub mod obsidian;
pub mod pdf_export;
pub mod prioritizer;
pub mod profile_manager;
pub mod stroke_htr;
pub mod stroke_persistence;
pub mod task_cache;
pub mod task_context;
pub mod task_provider;
pub mod transcriber;
pub mod vault_export;
pub mod vault_git;
pub use agenda::{filter_agenda, to_agenda_entries, AgendaEntry, AgendaRange};
#[cfg(feature = "sqlite")]
pub use aggregation::AggregationService;
#[cfg(feature = "caldav")]
pub use caldav::{default_sync_window, CalDavCalendar, CalDavClient, CalDavConfig};
pub use calendar_cache::CalendarCache;
pub use line_htr::{
    prepare_line_htr_input, split_ink_line_bands, ImageLineBand, LineHtrInput, DE_HTR_V2_ALPHABET,
    LINE_HTR_HEIGHT, LINE_HTR_MAX_WIDTH,
};
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
pub use stroke_htr::{
    ctc_decode_with_lm, ctc_greedy_decode, ctc_prefix_beam_decode, decode_charset,
    default_htr_language_context, default_line_gap, featurize_line, group_strokes_into_lines,
    CharBigramLm, CtcBeamConfig, CtcFrame, HtrLexicon, StrokeLine,
};
pub use task_cache::TaskCache;
#[cfg(feature = "sqlite")]
pub use task_provider::LocalTaskProvider;
pub use task_provider::{ProviderCapabilities, TaskProvider};
pub use transcriber::{
    mean_confidence, parse_tesseract_tsv, CascadingTranscriber, FakeTranscriber,
    LineImageHtrTranscriber, NoOpTranscriber, RasterOcrTranscriber, StrokeHtrTranscriber, TextSpan,
    Transcriber,
};
pub use vault_export::{
    export_note_markdown, notebook_export_dir, notebook_export_rel, path_under_export_subdir,
    sync_notes_to_vault, VaultNoteExport, DEFAULT_EXPORT_SUBDIR, NOTES_EXPORT_FOLDER,
};
pub use vault_git::{
    commit_and_push as vault_git_commit_and_push, is_git_repo, pull as vault_git_pull,
};
#[cfg(feature = "webdav")]
pub mod webdav;
