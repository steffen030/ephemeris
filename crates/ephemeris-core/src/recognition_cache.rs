//! Per-line handwriting recognition cache (ephemeris-ktg.12).
//!
//! Keys each recognised line by a hash of its stroke ids plus a model-version
//! string. Stroke edits invalidate only lines whose stroke set changed — the
//! rest keep their cached text for See-text, search, and vault export.
//!
//! Background recognition is scheduled by [`BgRecogIdle`]: ≥
//! [`BG_RECOG_IDLE_DELAY`] after the last PenUp, cancelled on PenDown, and
//! never while drawing.

use crate::model::{Stroke, StrokeId};
use crate::stroke_htr::{default_line_gap, group_strokes_into_lines_with_ruling, StrokeLine};
use crate::transcriber::TextSpan;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Idle time after PenUp before background recognition may run.
pub const BG_RECOG_IDLE_DELAY: Duration = Duration::from_millis(1500);

/// Bump when cascade / feature pipeline changes invalidate stored line text.
pub const RECOGNITION_CACHE_FORMAT: &str = "cascade-v2";

/// Placeholder shown in See-text for lines not yet recognised.
pub const PENDING_LINE_MARKER: &str = "…";

/// Stable content hash of a line's stroke ids (order-independent).
pub fn line_stroke_ids_hash(ids: &[StrokeId]) -> u64 {
    let mut sorted: Vec<_> = ids.iter().map(|id| id.0).collect();
    sorted.sort_unstable();
    let mut hasher = DefaultHasher::new();
    for u in sorted {
        u.hash(&mut hasher);
    }
    hasher.finish()
}

/// Hash stroke ids belonging to a [`StrokeLine`].
pub fn stroke_line_ids_hash(line: &StrokeLine) -> u64 {
    let ids: Vec<StrokeId> = line.strokes.iter().map(|s| s.id).collect();
    line_stroke_ids_hash(&ids)
}

/// Build a model-version key from cascade identity + language.
///
/// Changing any component forces a full re-recognition of cached lines.
pub fn recognition_model_version(
    format: &str,
    language: &str,
    stroke_features: &str,
    ocr_enabled: bool,
) -> String {
    format!("{format}|lang={language}|feat={stroke_features}|ocr={ocr_enabled}")
}

/// One cached / pending recognition line for a page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedLine {
    /// Hash of the line's stroke ids (see [`line_stroke_ids_hash`]).
    pub stroke_ids_hash: u64,
    /// Stroke ids that formed this line when recognised (or when marked pending).
    pub stroke_ids: Vec<StrokeId>,
    /// Recognised text when ready; empty while pending.
    #[serde(default)]
    pub text: String,
    /// Model confidence in `[0, 1]`; negative when unknown / pending.
    #[serde(default = "default_pending_confidence")]
    pub confidence: f32,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// True until a recogniser result has been applied for this hash.
    #[serde(default)]
    pub pending: bool,
}

fn default_pending_confidence() -> f32 {
    -1.0
}

impl CachedLine {
    /// Pending placeholder from a live [`StrokeLine`].
    pub fn pending_from_line(line: &StrokeLine) -> Self {
        let stroke_ids: Vec<StrokeId> = line.strokes.iter().map(|s| s.id).collect();
        Self {
            stroke_ids_hash: line_stroke_ids_hash(&stroke_ids),
            stroke_ids,
            text: String::new(),
            confidence: -1.0,
            x: line.x,
            y: line.y,
            width: line.width,
            height: line.height,
            pending: true,
        }
    }

    /// Store a recognised span for this line (clears pending).
    pub fn apply_span(&mut self, span: &TextSpan) {
        self.text = span.text.clone();
        self.confidence = span.confidence;
        self.x = span.x;
        self.y = span.y;
        self.width = span.width;
        self.height = span.height;
        self.pending = false;
    }
}

/// Per-page recognition cache persisted as `text_{id}_p{n}.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PageRecognitionCache {
    /// Cascade / model identity; mismatch drops all lines on sync.
    pub model_version: String,
    /// Reading-order lines for the page.
    pub lines: Vec<CachedLine>,
}

/// Stats from [`PageRecognitionCache::sync_strokes`] / [`PageRecognitionCache::recognise_pending`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecogCacheStats {
    /// Lines whose stroke-id set (or model version) changed → need recognition.
    pub invalidated: usize,
    /// Lines reused from the previous cache.
    pub reused: usize,
    /// Recogniser invocations performed this pass.
    pub recognised: usize,
}

impl PageRecognitionCache {
    pub fn new(model_version: impl Into<String>) -> Self {
        Self {
            model_version: model_version.into(),
            lines: Vec::new(),
        }
    }

    /// Path for persisted page text: `text_{note_id}_p{doc_page}.json`.
    pub fn path(data_dir: &Path, note_id: u32, doc_page: usize) -> PathBuf {
        data_dir.join(format!("text_{note_id}_p{doc_page}.json"))
    }

    pub fn load(data_dir: &Path, note_id: u32, doc_page: usize) -> Option<Self> {
        let path = Self::path(data_dir, note_id, doc_page);
        let json = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&json).ok()
    }

    pub fn save(&self, data_dir: &Path, note_id: u32, doc_page: usize) -> crate::Result<()> {
        let path = Self::path(data_dir, note_id, doc_page);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| crate::AppError::Storage(format!("mkdir for text cache: {e}")))?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| crate::AppError::Storage(format!("serialize text cache: {e}")))?;
        std::fs::write(&path, json)
            .map_err(|e| crate::AppError::Storage(format!("write text cache: {e}")))?;
        Ok(())
    }

    pub fn delete(data_dir: &Path, note_id: u32, doc_page: usize) {
        let _ = std::fs::remove_file(Self::path(data_dir, note_id, doc_page));
    }

    /// Reconcile cache with current strokes: reuse matching line hashes, mark
    /// the rest pending. Does **not** run recognition.
    ///
    /// Returns how many lines were invalidated vs reused.
    pub fn sync_strokes(
        &mut self,
        strokes: &[Stroke],
        ruling_pitch: Option<f32>,
        model_version: &str,
    ) -> RecogCacheStats {
        let gap = default_line_gap(strokes);
        let live = group_strokes_into_lines_with_ruling(strokes, gap, ruling_pitch);
        let version_ok = self.model_version == model_version;
        let mut by_hash: std::collections::HashMap<u64, CachedLine> =
            std::collections::HashMap::new();
        if version_ok {
            for line in self.lines.drain(..) {
                by_hash.insert(line.stroke_ids_hash, line);
            }
        } else {
            self.lines.clear();
        }
        self.model_version = model_version.to_string();

        let mut stats = RecogCacheStats::default();
        let mut next = Vec::with_capacity(live.len());
        for line in &live {
            let hash = stroke_line_ids_hash(line);
            match by_hash.remove(&hash) {
                Some(mut cached) if !cached.pending => {
                    // Keep recognised text; refresh geometry from live grouping.
                    cached.x = line.x;
                    cached.y = line.y;
                    cached.width = line.width;
                    cached.height = line.height;
                    cached.stroke_ids = line.strokes.iter().map(|s| s.id).collect();
                    next.push(cached);
                    stats.reused += 1;
                }
                Some(mut cached) => {
                    cached.x = line.x;
                    cached.y = line.y;
                    cached.width = line.width;
                    cached.height = line.height;
                    cached.stroke_ids = line.strokes.iter().map(|s| s.id).collect();
                    cached.pending = true;
                    next.push(cached);
                    stats.invalidated += 1;
                }
                None => {
                    next.push(CachedLine::pending_from_line(line));
                    stats.invalidated += 1;
                }
            }
        }
        self.lines = next;
        stats
    }

    /// True when at least one line still needs recognition.
    pub fn has_pending(&self) -> bool {
        self.lines.iter().any(|l| l.pending)
    }

    pub fn pending_count(&self) -> usize {
        self.lines.iter().filter(|l| l.pending).count()
    }

    /// Completed lines only (for search / vault / `note.text_content`).
    pub fn completed_text(&self) -> String {
        self.lines
            .iter()
            .filter(|l| !l.pending && !l.text.is_empty())
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// See-text body: recognised lines plus [`PENDING_LINE_MARKER`] for pending.
    pub fn display_text(&self) -> String {
        if self.lines.is_empty() {
            return String::new();
        }
        self.lines
            .iter()
            .map(|l| {
                if l.pending || l.text.is_empty() {
                    PENDING_LINE_MARKER
                } else {
                    l.text.as_str()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Convert completed lines to [`TextSpan`]s (line index = position).
    pub fn to_spans(&self) -> Vec<TextSpan> {
        self.lines
            .iter()
            .enumerate()
            .filter(|(_, l)| !l.pending && !l.text.is_empty())
            .map(|(i, l)| {
                let mut span = TextSpan::new(&l.text, l.x, l.y, l.width, l.height);
                span.confidence = l.confidence;
                span.line = i as i32;
                span
            })
            .collect()
    }

    /// Run `recognise` for every pending line. Stops early when `cancel` is true.
    ///
    /// `recognise` receives the live [`StrokeLine`] rebuilt from `strokes` and
    /// the line index. Counter-friendly for unit tests.
    pub fn recognise_pending<F>(
        &mut self,
        strokes: &[Stroke],
        ruling_pitch: Option<f32>,
        cancel: &dyn Fn() -> bool,
        mut recognise: F,
    ) -> crate::Result<RecogCacheStats>
    where
        F: FnMut(&StrokeLine, i32) -> crate::Result<Option<TextSpan>>,
    {
        let gap = default_line_gap(strokes);
        let live = group_strokes_into_lines_with_ruling(strokes, gap, ruling_pitch);
        let mut stats = RecogCacheStats::default();
        if live.len() != self.lines.len() {
            // Caller should sync first; still attempt by hash.
        }
        for (i, live_line) in live.iter().enumerate() {
            if cancel() {
                break;
            }
            let hash = stroke_line_ids_hash(live_line);
            let Some(slot) = self.lines.iter_mut().find(|l| l.stroke_ids_hash == hash) else {
                continue;
            };
            if !slot.pending {
                stats.reused += 1;
                continue;
            }
            match recognise(live_line, i as i32)? {
                Some(span) => {
                    slot.apply_span(&span);
                    stats.recognised += 1;
                }
                None => {
                    // Empty result: clear pending so we do not spin forever.
                    slot.text.clear();
                    slot.confidence = -1.0;
                    slot.pending = false;
                    stats.recognised += 1;
                }
            }
        }
        Ok(stats)
    }
}

/// Idle / cancel state machine for background recognition (pure, testable).
///
/// Trigger ≥ [`BG_RECOG_IDLE_DELAY`] after last PenUp; cancel/pause on PenDown;
/// never while `is_drawing`.
#[derive(Debug, Clone)]
pub struct BgRecogIdle {
    last_pen_up: Option<Instant>,
    /// Set on PenDown; cleared when a new PenUp arms the idle timer.
    cancelled: bool,
    is_drawing: bool,
    idle_delay: Duration,
}

impl Default for BgRecogIdle {
    fn default() -> Self {
        Self::new(BG_RECOG_IDLE_DELAY)
    }
}

impl BgRecogIdle {
    pub fn new(idle_delay: Duration) -> Self {
        Self {
            last_pen_up: None,
            cancelled: false,
            is_drawing: false,
            idle_delay,
        }
    }

    pub fn on_pen_down(&mut self) {
        self.cancelled = true;
        self.is_drawing = true;
        self.last_pen_up = None;
    }

    pub fn on_pen_up(&mut self, now: Instant) {
        self.is_drawing = false;
        self.cancelled = false;
        self.last_pen_up = Some(now);
    }

    /// Sync drawing flag without treating it as a pen event (e.g. swipe abort).
    pub fn set_drawing(&mut self, drawing: bool) {
        self.is_drawing = drawing;
        if drawing {
            self.cancelled = true;
            self.last_pen_up = None;
        }
    }

    /// True while a run should abort between lines (PenDown / drawing).
    pub fn should_cancel(&self) -> bool {
        self.cancelled || self.is_drawing
    }

    /// True when idle delay has elapsed and recognition may start.
    pub fn should_run(&self, now: Instant) -> bool {
        if self.is_drawing || self.cancelled {
            return false;
        }
        match self.last_pen_up {
            Some(t) => now.duration_since(t) >= self.idle_delay,
            None => false,
        }
    }

    /// Clear the armed timer after a worker has been started (or work finished).
    pub fn clear_arm(&mut self) {
        self.last_pen_up = None;
    }

    pub fn is_drawing(&self) -> bool {
        self.is_drawing
    }

    pub fn idle_delay(&self) -> Duration {
        self.idle_delay
    }
}

/// Lower the calling thread's nice level to 10 on Linux; no-op elsewhere.
pub fn lower_thread_nice_priority() {
    #[cfg(target_os = "linux")]
    {
        // PRIO_PROCESS + who=0 → calling thread under Linux NPTL.
        unsafe {
            libc::setpriority(libc::PRIO_PROCESS, 0, 10);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Color, Point, Tool};

    fn stroke_at(y: f32, id: Option<StrokeId>) -> Stroke {
        let mut s = Stroke::new(Tool::Pen, Color::BLACK, 2.0);
        if let Some(id) = id {
            s.id = id;
        }
        s.points = vec![
            Point::new(10.0, y, 0.5, 0.0, 0),
            Point::new(80.0, y, 0.5, 0.0, 10),
        ];
        s
    }

    fn two_line_strokes() -> (Stroke, Stroke, Stroke, Stroke) {
        // Line A near y=20, line B near y=120 (gap >> default cluster).
        let a1 = stroke_at(20.0, None);
        let a2 = stroke_at(22.0, None);
        let b1 = stroke_at(120.0, None);
        let b2 = stroke_at(122.0, None);
        (a1, a2, b1, b2)
    }

    #[test]
    fn editing_one_line_re_runs_that_line_only() {
        let (a1, a2, b1, b2) = two_line_strokes();
        let strokes = vec![a1.clone(), a2.clone(), b1.clone(), b2.clone()];
        let version = "test-v1";
        let mut cache = PageRecognitionCache::new(version);
        let sync = cache.sync_strokes(&strokes, None, version);
        assert_eq!(sync.invalidated, 2);
        assert_eq!(sync.reused, 0);

        let mut calls = 0usize;
        let stats = cache
            .recognise_pending(&strokes, None, &|| false, |line, idx| {
                calls += 1;
                Ok(Some(TextSpan::new(
                    format!("L{idx}"),
                    line.x,
                    line.y,
                    line.width,
                    line.height,
                )))
            })
            .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(stats.recognised, 2);
        assert!(!cache.has_pending());
        assert_eq!(cache.completed_text(), "L0\nL1");

        // Edit only the first line (replace a stroke id).
        let mut a1_edit = a1;
        a1_edit.id = StrokeId::new();
        let strokes2 = vec![a1_edit, a2, b1, b2];
        let sync2 = cache.sync_strokes(&strokes2, None, version);
        assert_eq!(
            sync2.invalidated, 1,
            "only the edited line should invalidate"
        );
        assert_eq!(sync2.reused, 1, "untouched line must be reused");
        assert_eq!(cache.pending_count(), 1);

        calls = 0;
        let stats2 = cache
            .recognise_pending(&strokes2, None, &|| false, |line, idx| {
                calls += 1;
                Ok(Some(TextSpan::new(
                    format!("L{idx}-new"),
                    line.x,
                    line.y,
                    line.width,
                    line.height,
                )))
            })
            .unwrap();
        assert_eq!(calls, 1, "recogniser must run for the dirty line only");
        assert_eq!(stats2.recognised, 1);
        assert!(!cache.has_pending());
        let text = cache.completed_text();
        assert!(
            text.contains("L0-new") || text.contains("L1-new"),
            "edited line text updated: {text}"
        );
        assert!(
            text.contains("L0") || text.contains("L1"),
            "untouched line text retained: {text}"
        );
    }

    #[test]
    fn model_version_change_invalidates_all() {
        let (a1, a2, b1, b2) = two_line_strokes();
        let strokes = vec![a1, a2, b1, b2];
        let mut cache = PageRecognitionCache::new("v1");
        cache.sync_strokes(&strokes, None, "v1");
        let _ = cache
            .recognise_pending(&strokes, None, &|| false, |line, idx| {
                Ok(Some(TextSpan::new(
                    format!("x{idx}"),
                    line.x,
                    line.y,
                    line.width,
                    line.height,
                )))
            })
            .unwrap();
        let sync = cache.sync_strokes(&strokes, None, "v2");
        assert_eq!(sync.reused, 0);
        assert_eq!(sync.invalidated, 2);
        assert!(cache.has_pending());
    }

    #[test]
    fn display_text_marks_pending_lines() {
        let (a1, a2, b1, b2) = two_line_strokes();
        let strokes = vec![a1, a2, b1, b2];
        let mut cache = PageRecognitionCache::new("v1");
        cache.sync_strokes(&strokes, None, "v1");
        // Recognise first line only.
        let mut first = true;
        let _ = cache
            .recognise_pending(&strokes, None, &|| false, |line, idx| {
                if first {
                    first = false;
                    Ok(Some(TextSpan::new(
                        "hello",
                        line.x,
                        line.y,
                        line.width,
                        line.height,
                    )))
                } else {
                    // Leave second pending by cancelling after first.
                    let _ = idx;
                    Ok(None)
                }
            })
            .unwrap();
        // Force second back to pending for display check.
        if let Some(last) = cache.lines.last_mut() {
            last.pending = true;
            last.text.clear();
        }
        let display = cache.display_text();
        assert!(display.contains("hello"));
        assert!(display.contains(PENDING_LINE_MARKER));
        assert_eq!(cache.completed_text(), "hello");
    }

    #[test]
    fn idle_delay_and_cancel_on_pen_down() {
        let mut idle = BgRecogIdle::new(Duration::from_millis(100));
        let t0 = Instant::now();
        assert!(!idle.should_run(t0));

        idle.on_pen_up(t0);
        assert!(!idle.should_run(t0 + Duration::from_millis(50)));
        assert!(idle.should_run(t0 + Duration::from_millis(100)));
        assert!(idle.should_run(t0 + Duration::from_millis(200)));

        idle.on_pen_down();
        assert!(idle.should_cancel());
        assert!(!idle.should_run(t0 + Duration::from_millis(500)));

        let t1 = t0 + Duration::from_millis(600);
        idle.on_pen_up(t1);
        assert!(!idle.should_cancel());
        assert!(!idle.should_run(t1 + Duration::from_millis(50)));
        assert!(idle.should_run(t1 + Duration::from_millis(100)));
    }

    #[test]
    fn never_runs_while_drawing() {
        let mut idle = BgRecogIdle::new(Duration::from_millis(10));
        let t0 = Instant::now();
        idle.on_pen_up(t0);
        idle.set_drawing(true);
        assert!(!idle.should_run(t0 + Duration::from_secs(5)));
        assert!(idle.should_cancel());
    }

    #[test]
    fn recognise_pending_stops_on_cancel() {
        let (a1, a2, b1, b2) = two_line_strokes();
        let strokes = vec![a1, a2, b1, b2];
        let mut cache = PageRecognitionCache::new("v1");
        cache.sync_strokes(&strokes, None, "v1");
        let mut calls = 0usize;
        let cancel_after_one = std::cell::Cell::new(false);
        let _ = cache
            .recognise_pending(&strokes, None, &|| cancel_after_one.get(), |line, idx| {
                calls += 1;
                cancel_after_one.set(true);
                Ok(Some(TextSpan::new(
                    format!("{idx}"),
                    line.x,
                    line.y,
                    line.width,
                    line.height,
                )))
            })
            .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(cache.pending_count(), 1);
    }

    #[test]
    fn page_cache_round_trips_json() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = PageRecognitionCache::new("v1");
        cache.lines.push(CachedLine {
            stroke_ids_hash: 42,
            stroke_ids: vec![StrokeId::new()],
            text: "hi".into(),
            confidence: 0.8,
            x: 1.0,
            y: 2.0,
            width: 3.0,
            height: 4.0,
            pending: false,
        });
        cache.save(dir.path(), 7, 1).unwrap();
        let loaded = PageRecognitionCache::load(dir.path(), 7, 1).unwrap();
        assert_eq!(loaded.model_version, "v1");
        assert_eq!(loaded.lines.len(), 1);
        assert_eq!(loaded.lines[0].text, "hi");
        assert_eq!(
            PageRecognitionCache::path(dir.path(), 7, 1),
            dir.path().join("text_7_p1.json")
        );
    }
}
