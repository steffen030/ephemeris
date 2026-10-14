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
pub mod ink_layout;
pub mod model;
pub mod refresh;
#[cfg(feature = "sqlite")]
pub mod sqlite;
pub mod store;
pub mod text_format;

pub use action::{Action, ActionMap, ActionRouter, CanvasPoint, DefaultActionMap, RouterOutput};
pub use config::{
    format_note_title, Config, HandwritingLanguage, ResolvedHandwritingLanguage, TaskEngageConfig,
    TaskLaneConfig,
};
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
pub use text_format::{lines_to_markdown, BlockKind, InlineKind, InlineMark, LineFormat};

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
pub mod htr_eval;
pub mod layout_detect;
pub mod line_htr;
pub mod markdown_tasks;
pub mod note_search;
pub mod note_tasks;
pub mod obsidian;
pub mod pdf_export;
pub mod prioritizer;
pub mod profile_manager;
pub mod recognition_cache;
pub mod stroke_htr;
pub mod stroke_persistence;
pub mod stroke_render;
pub mod task_cache;
pub mod task_context;
pub mod task_provider;
pub mod transcriber;
pub mod user_lexicon;
pub mod vault_export;
pub mod vault_git;
pub use agenda::{filter_agenda, to_agenda_entries, AgendaEntry, AgendaRange};
#[cfg(feature = "sqlite")]
pub use aggregation::AggregationService;
#[cfg(feature = "caldav")]
pub use caldav::{default_sync_window, CalDavCalendar, CalDavClient, CalDavConfig};
pub use calendar_cache::CalendarCache;
pub use ink_layout::{classify_mark_strokes, split_page_ink, MarkKind, MarkStroke, PageInk};
pub use layout_detect::{detect_line_formats, DetectedLine, LayoutLine};
pub use line_htr::{
    prepare_line_htr_input, split_ink_line_bands, ImageLineBand, LineHtrInput, DE_HTR_V2_ALPHABET,
    LINE_HTR_HEIGHT, LINE_HTR_MAX_WIDTH,
};
pub use markdown_tasks::{MarkdownTaskExtractor, DEFAULT_TASKS_INBOX};
pub use note_search::{
    list_markdown_files, markdown_to_blocks, markdown_to_plain, search_events, search_local_notes,
    search_tasks, strip_inline_markdown, MdBlock, MdBlockKind, SearchHit, VaultFtsIndex,
};
pub use note_tasks::{
    checkbox_tasks_for_page, note_task_id, note_task_source_ref, parse_note_source_ref,
    reconcile_note_tasks, sync_page_tasks, NoteTaskChanges, NOTE_TASK_SOURCE,
};
pub use pdf_export::{
    export_raster_pages_to_pdf, export_stroke_pages_to_pdf, pdf_export_is_current,
    raster_export_hash, PdfExportOptions, RasterPage,
};
pub use prioritizer::{rank_tasks, task_score};
pub use recognition_cache::{
    line_stroke_ids_hash, lower_thread_nice_priority, recognition_model_version,
    stroke_line_ids_hash, BgRecogIdle, CachedLine, PageRecognitionCache, RecogCacheStats,
    BG_RECOG_IDLE_DELAY, PENDING_LINE_MARKER, RECOGNITION_CACHE_FORMAT,
};
pub use stroke_htr::armrest;
pub use stroke_htr::{
    bundled_htr_lexicon_text, ctc_decode_with_lm, ctc_greedy_decode, ctc_prefix_beam_decode,
    decode_charset, default_htr_language_context, default_line_gap, featurize_line,
    featurize_line_pieces, group_strokes_into_lines, group_strokes_into_lines_with_ruling,
    is_structured_htr_token, merged_htr_lexicon, split_stroke_line_at_largest_x_gap,
    tokenize_htr_text, CharBigramLm, CtcBeamConfig, CtcFrame, HtrLexicon, StrokeLine,
    CARBUNE_POINTS_PER_UNIT, GENERIC_LEXICON_WEIGHT, LINED_TEMPLATE_PITCH,
    MAX_STROKE_HTR_TIMESTEPS, STRUCTURED_TOKEN_WEIGHT, USER_LEXICON_WEIGHT,
};
pub use stroke_render::{
    prepare_line_htr_from_strokes, render_stroke_line, render_strokes_page, StrokeRenderParams,
    HTR_LINE_STROKE_WIDTH_PX, OCR_STROKE_WIDTH_PX, OCR_TARGET_LINE_HEIGHT_PX,
};
pub use task_cache::TaskCache;
#[cfg(feature = "sqlite")]
pub use task_provider::LocalTaskProvider;
pub use task_provider::{ProviderCapabilities, TaskProvider};
pub use transcriber::{
    calibrated_cascade_score, mean_confidence, parse_tesseract_tsv, pick_best_line_candidate,
    select_line_cascade, CascadeBackend, CascadingTranscriber, FakeTranscriber,
    LineImageHtrTranscriber, NoOpTranscriber, RasterOcrTranscriber, SharedHtrLexicon,
    StrokeHtrFeatures, StrokeHtrTranscriber, TextSpan, Transcriber, CASCADE_ACCEPT_THRESHOLD,
    CASCADE_LEXICON_WEIGHT,
};
pub use user_lexicon::{
    accumulate_token_frequencies, build_and_save_user_lexicon, build_user_lexicon_tokens,
    load_merged_htr_lexicon, load_user_lexicon_text, refresh_user_lexicon_if_stale,
    save_user_lexicon, spawn_user_lexicon_refresh, user_lexicon_needs_refresh, user_lexicon_path,
    UserLexiconSources, USER_LEXICON_FILENAME, USER_LEXICON_MAX_TOKENS,
    USER_LEXICON_REFRESH_INTERVAL,
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
