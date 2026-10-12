//! Unified GTD context parsing and horizon-lane resolution (ADR ephemeris-4if.8).
//!
//! Separates **horizon** (when: Now/Next/Later/…) from **context** (where/how:
//! place, tool, person, energy, duration) and maps Obsidian tags / Todoist labels
//! into the unified [`Task`] model.

use crate::config::TaskLaneConfig;
use crate::model::{
    EnergyLevel, Task, TaskContext, TaskContextKind, TaskId, TaskPriority,
};

/// Tag prefix that encodes a horizon lane (`#lane/now`).
pub const LANE_TAG_PREFIX: &str = "lane/";

/// Tag prefixes that encode a typed place/tool context (`#ctx/home`, `#context/office`).
pub const CTX_TAG_PREFIXES: &[&str] = &["ctx/", "context/"];

/// Known tool-like `@context` / label names.
const TOOL_NAMES: &[&str] = &["phone", "computer", "email", "laptop", "online"];
/// Known place-like names.
const PLACE_NAMES: &[&str] = &["home", "office", "errand", "anywhere", "town", "shop"];
/// Known person-agenda prefix.
const PERSON_PREFIXES: &[&str] = &["agenda/", "person/", "with/"];

/// Result of splitting freeform tags into lane, typed contexts, and leftover tags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedTags {
    pub lane_id: Option<String>,
    pub contexts: Vec<TaskContext>,
    pub tags: Vec<String>,
    pub energy: Option<EnergyLevel>,
}

/// Parse Obsidian-style tags and `@mentions` into lane + typed contexts.
///
/// - `#lane/<id>` → lane
/// - `#ctx/<name>` / `#context/<name>` → Place (or Tool if name matches tools)
/// - `@phone` / `@home` → Tool / Place
/// - `#energy/high` → EnergyLevel + Energy context
/// - remaining `#tags` stay as freeform tags (without the `#`)
pub fn parse_tags_and_mentions<'a>(
    tags: impl IntoIterator<Item = &'a str>,
    title_tokens: impl IntoIterator<Item = &'a str>,
) -> ParsedTags {
    let mut out = ParsedTags::default();

    for raw in tags {
        let tag = raw.trim().trim_start_matches('#');
        if tag.is_empty() {
            continue;
        }
        if let Some(lane) = tag.strip_prefix(LANE_TAG_PREFIX) {
            if !lane.is_empty() {
                out.lane_id = Some(lane.to_ascii_lowercase());
            }
            continue;
        }
        if let Some(name) = strip_ctx_prefix(tag) {
            out.contexts.push(context_for_name(name));
            continue;
        }
        if let Some(level) = tag.strip_prefix("energy/") {
            if let Some(e) = parse_energy(level) {
                out.energy = Some(e);
                out.contexts
                    .push(TaskContext::new(TaskContextKind::Energy, level.to_ascii_lowercase()));
            }
            continue;
        }
        out.tags.push(tag.to_string());
    }

    for tok in title_tokens {
        let t = tok.trim();
        if let Some(name) = t.strip_prefix('@') {
            if name.is_empty() {
                continue;
            }
            // Avoid double-adding if already present as tag form
            let ctx = context_for_at(name);
            if !out
                .contexts
                .iter()
                .any(|c| c.name.eq_ignore_ascii_case(&ctx.name) && c.kind == ctx.kind)
            {
                out.contexts.push(ctx);
            }
        }
    }

    out
}

/// Apply [`ParsedTags`] onto a task (contexts, energy, lane from tags; merge leftover tags).
pub fn apply_parsed_tags(task: &mut Task, parsed: ParsedTags) {
    if let Some(lane) = parsed.lane_id {
        task.lane_id = Some(lane);
    }
    if !parsed.contexts.is_empty() {
        task.contexts = parsed.contexts;
    }
    if parsed.energy.is_some() {
        task.energy = parsed.energy;
    }
    task.tags = parsed.tags;
}

/// Resolve effective lane: explicit `task.lane_id` → `#lane/` in tags → waiting_for → default.
pub fn resolve_lane(task: &Task, default_lane: &str) -> String {
    if let Some(ref id) = task.lane_id {
        if !id.is_empty() {
            return id.clone();
        }
    }
    for tag in &task.tags {
        if let Some(lane) = tag.trim_start_matches('#').strip_prefix(LANE_TAG_PREFIX) {
            if !lane.is_empty() {
                return lane.to_ascii_lowercase();
            }
        }
    }
    if task.waiting_for.as_ref().is_some_and(|w| !w.is_empty()) {
        return "waiting".into();
    }
    default_lane.to_string()
}

/// Ensure `task.tags` contains exactly one `#lane/<id>` (without `#` in the Vec)
/// matching `lane_id`, removing other `lane/*` entries.
pub fn set_lane_tag(task: &mut Task, lane_id: &str) {
    task.tags
        .retain(|t| !t.trim_start_matches('#').starts_with(LANE_TAG_PREFIX));
    if !lane_id.is_empty() {
        task.tags.push(format!("{LANE_TAG_PREFIX}{lane_id}"));
    }
    task.lane_id = Some(lane_id.to_string());
}

/// Sync freeform tags + context chips into Obsidian-writable tag list (no `#` prefix).
/// Preserves non-lane/non-ctx tags; writes `#lane/` and `#ctx/` for structured fields.
pub fn sync_tags_from_structured(task: &mut Task) {
    let mut keep: Vec<String> = task
        .tags
        .iter()
        .filter(|t| {
            let t = t.trim_start_matches('#');
            !t.starts_with(LANE_TAG_PREFIX)
                && !CTX_TAG_PREFIXES.iter().any(|p| t.starts_with(p))
                && !t.starts_with("energy/")
        })
        .cloned()
        .collect();

    if let Some(ref lane) = task.lane_id {
        if !lane.is_empty() {
            keep.push(format!("{LANE_TAG_PREFIX}{lane}"));
        }
    }
    for ctx in &task.contexts {
        match ctx.kind {
            TaskContextKind::Place | TaskContextKind::Tool | TaskContextKind::Custom => {
                keep.push(format!("ctx/{}", ctx.name));
            }
            TaskContextKind::Person => {
                keep.push(format!("ctx/{}", ctx.name));
            }
            TaskContextKind::Energy => {
                keep.push(format!("energy/{}", ctx.name));
            }
            TaskContextKind::Duration => {}
        }
    }
    if let Some(e) = task.energy {
        let name = energy_name(e);
        if !keep.iter().any(|t| t == &format!("energy/{name}")) {
            keep.push(format!("energy/{name}"));
        }
    }
    task.tags = keep;
}

/// Map Obsidian / Todoist priority words and numbers into [`TaskPriority`].
pub fn parse_priority_word(s: &str) -> TaskPriority {
    match s.to_lowercase().as_str() {
        "highest" | "urgent" | "p4" | "4" => TaskPriority::Urgent,
        "high" | "p3" | "3" => TaskPriority::High,
        "lowest" | "low" | "p1" | "1" => TaskPriority::Low,
        "medium" | "normal" | "p2" | "2" => TaskPriority::Medium,
        _ => TaskPriority::Medium,
    }
}

/// Filters for the Engage (Start) list.
#[derive(Debug, Clone, Default)]
pub struct EngageFilter {
    /// Primary horizon lane (typically `now`) — always included.
    pub lane_id: String,
    /// Extra lanes treated as actionable (default: `next`). Unlaned tasks resolve here.
    pub also_lanes: Vec<String>,
    /// If non-empty, task must have at least one matching context name (case-insensitive).
    pub context_names: Vec<String>,
    /// Max duration in minutes; tasks without duration pass.
    pub max_duration_minutes: Option<u32>,
    /// Required energy; tasks without energy pass when filter is set.
    pub energy: Option<EnergyLevel>,
}

impl EngageFilter {
    /// Start-page Engage: Now + Next (legacy/unlaned Obsidian tasks land in Next).
    pub fn start_page(engage_lane: impl Into<String>) -> Self {
        EngageFilter {
            lane_id: engage_lane.into(),
            also_lanes: vec!["next".into()],
            ..Default::default()
        }
    }

    fn lane_allowed(&self, lane: &str) -> bool {
        lane == self.lane_id || self.also_lanes.iter().any(|l| l == lane)
    }
}

/// Filter open actionable tasks (Now + Next by default), then rank with [`crate::rank_tasks`].
///
/// Excludes Later / Waiting / Someday so Engage stays a short “what can I do”
/// list. Context / duration / energy chips further narrow the list.
pub fn list_engage<'a>(
    tasks: &'a [Task],
    filter: &EngageFilter,
    now_secs: u64,
) -> Vec<&'a Task> {
    let candidates: Vec<Task> = tasks
        .iter()
        .filter(|t| !t.done && !matches!(t.status, crate::TaskStatus::Cancelled))
        .filter(|t| filter.lane_allowed(&resolve_lane(t, "next")))
        .filter(|t| context_match(t, &filter.context_names))
        .filter(|t| duration_match(t, filter.max_duration_minutes))
        .filter(|t| energy_match(t, filter.energy))
        .cloned()
        .collect();

    // rank_tasks needs owned slice; re-borrow from original by id
    let ranked = crate::rank_tasks(&candidates, now_secs);
    let ids: Vec<TaskId> = ranked.iter().map(|t| t.id).collect();
    ids.into_iter()
        .filter_map(|id| tasks.iter().find(|t| t.id == id))
        .collect()
}

/// Count open tasks in a lane (for WIP badge).
pub fn count_lane(tasks: &[Task], lane_id: &str) -> usize {
    tasks
        .iter()
        .filter(|t| !t.done && resolve_lane(t, "next") == lane_id)
        .count()
}

fn context_match(task: &Task, names: &[String]) -> bool {
    if names.is_empty() {
        return true;
    }
    names.iter().any(|want| {
        task.contexts
            .iter()
            .any(|c| c.name.eq_ignore_ascii_case(want))
            || task
                .tags
                .iter()
                .any(|t| t.eq_ignore_ascii_case(want))
    })
}

fn duration_match(task: &Task, max: Option<u32>) -> bool {
    match (max, task.duration_minutes) {
        (None, _) => true,
        (Some(_), None) => true,
        (Some(m), Some(d)) => d <= m,
    }
}

fn energy_match(task: &Task, want: Option<EnergyLevel>) -> bool {
    match (want, task.energy) {
        (None, _) => true,
        (Some(_), None) => true,
        (Some(w), Some(e)) => w == e,
    }
}

fn strip_ctx_prefix(tag: &str) -> Option<&str> {
    for p in CTX_TAG_PREFIXES {
        if let Some(rest) = tag.strip_prefix(p) {
            if !rest.is_empty() {
                return Some(rest);
            }
        }
    }
    None
}

fn context_for_name(name: &str) -> TaskContext {
    let lower = name.to_ascii_lowercase();
    for p in PERSON_PREFIXES {
        if let Some(rest) = lower.strip_prefix(p) {
            return TaskContext::new(TaskContextKind::Person, rest);
        }
    }
    if TOOL_NAMES.contains(&lower.as_str()) {
        TaskContext::new(TaskContextKind::Tool, lower)
    } else if PLACE_NAMES.contains(&lower.as_str()) {
        TaskContext::new(TaskContextKind::Place, lower)
    } else {
        TaskContext::new(TaskContextKind::Custom, lower)
    }
}

fn context_for_at(name: &str) -> TaskContext {
    for p in PERSON_PREFIXES {
        if let Some(rest) = name.to_ascii_lowercase().strip_prefix(p) {
            return TaskContext::new(TaskContextKind::Person, rest);
        }
    }
    // bare @Name → Person if capitalized-looking, else tool/place heuristics
    let lower = name.to_ascii_lowercase();
    if TOOL_NAMES.contains(&lower.as_str()) {
        TaskContext::new(TaskContextKind::Tool, lower)
    } else if PLACE_NAMES.contains(&lower.as_str()) {
        TaskContext::new(TaskContextKind::Place, lower)
    } else {
        TaskContext::new(TaskContextKind::Person, name.to_string())
    }
}

fn parse_energy(s: &str) -> Option<EnergyLevel> {
    match s.to_lowercase().as_str() {
        "low" => Some(EnergyLevel::Low),
        "medium" | "med" => Some(EnergyLevel::Medium),
        "high" => Some(EnergyLevel::High),
        _ => None,
    }
}

fn energy_name(e: EnergyLevel) -> &'static str {
    match e {
        EnergyLevel::Low => "low",
        EnergyLevel::Medium => "medium",
        EnergyLevel::High => "high",
    }
}

/// Infer project name from a vault-relative path (first path segment / notebook).
pub fn project_from_vault_path(rel: &str) -> Option<String> {
    let rel = rel.trim_start_matches('/').trim_start_matches('\\');
    if rel.is_empty() {
        return None;
    }
    let first = rel.split(['/', '\\']).next()?;
    if first.is_empty() || first.eq_ignore_ascii_case("ephemeris") {
        // Prefer second segment under Ephemeris/ export tree
        let mut parts = rel.split(['/', '\\']);
        let _ = parts.next()?;
        return parts.next().map(|s| s.trim_end_matches(".md").to_string());
    }
    Some(first.trim_end_matches(".md").to_string())
}

// ── Todoist mapping (for future TaskProvider) ─────────────────────────────────

/// Todoist REST priority 1–4 → unified [`TaskPriority`] (p4=urgent … p1=low).
pub fn todoist_priority(p: u8) -> TaskPriority {
    match p {
        4 => TaskPriority::Urgent,
        3 => TaskPriority::High,
        2 => TaskPriority::Medium,
        _ => TaskPriority::Low,
    }
}

/// Match a Todoist section name to a configured lane id (case-insensitive label or id).
///
/// Returns `None` when no lane matches — caller should keep the SQLite overlay.
pub fn todoist_section_to_lane(section: &str, lanes: &[TaskLaneConfig]) -> Option<String> {
    let s = section.trim();
    if s.is_empty() {
        return None;
    }
    let lower = s.to_ascii_lowercase();
    lanes
        .iter()
        .find(|l| l.id.eq_ignore_ascii_case(&lower) || l.label.eq_ignore_ascii_case(s))
        .map(|l| l.id.clone())
}

/// Apply Todoist labels into contexts + leftover tags (same rules as Obsidian tags).
pub fn apply_todoist_labels(task: &mut Task, labels: &[String]) {
    let refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
    let parsed = parse_tags_and_mentions(refs, []);
    apply_parsed_tags(task, parsed);
}

/// Fields commonly returned by the Todoist REST task object.
#[derive(Debug, Clone, Default)]
pub struct TodoistTaskFields<'a> {
    pub content: &'a str,
    pub description: Option<&'a str>,
    pub project_name: Option<&'a str>,
    pub section_name: Option<&'a str>,
    pub labels: &'a [String],
    /// Todoist priority 1–4.
    pub priority: u8,
    pub due_unix: Option<u64>,
    pub duration_minutes: Option<u32>,
    pub url: Option<&'a str>,
    pub parent_id: Option<TaskId>,
}

/// Fill a [`Task`] from Todoist fields + lane config (section → lane when matched).
pub fn apply_todoist_fields(
    task: &mut Task,
    fields: &TodoistTaskFields<'_>,
    lanes: &[TaskLaneConfig],
) {
    task.title = fields.content.to_string();
    task.description = fields.description.map(|s| s.to_string());
    task.project = fields.project_name.map(|s| s.to_string());
    task.section = fields.section_name.map(|s| s.to_string());
    task.priority = todoist_priority(fields.priority);
    task.due = fields.due_unix;
    task.duration_minutes = fields.duration_minutes;
    task.url = fields.url.map(|s| s.to_string());
    task.parent_id = fields.parent_id;
    task.source = "todoist".to_string();
    apply_todoist_labels(task, fields.labels);
    if let Some(section) = fields.section_name {
        if let Some(lane) = todoist_section_to_lane(section, lanes) {
            set_lane_tag(task, &lane);
        }
    }
}

// ── Weekly review / staleness ─────────────────────────────────────────────────

/// Anchor timestamp for staleness: `reviewed_at` → `created_at` → 0.
pub fn review_anchor(task: &Task) -> u64 {
    task.reviewed_at
        .or(task.created_at)
        .unwrap_or(0)
}

/// True when an open task in a review lane has not been confirmed within `staleness_days`.
pub fn is_stale(task: &Task, now_secs: u64, staleness_days: u32) -> bool {
    if task.done || matches!(task.status, crate::TaskStatus::Cancelled) {
        return false;
    }
    let threshold = u64::from(staleness_days).saturating_mul(86_400);
    if threshold == 0 {
        return false;
    }
    let anchor = review_anchor(task);
    if anchor == 0 {
        return true;
    }
    now_secs.saturating_sub(anchor) >= threshold
}

/// Mark a task as reviewed (resets staleness timer).
pub fn mark_reviewed(task: &mut Task, now_secs: u64) {
    task.reviewed_at = Some(now_secs);
}

/// Build a weekly-review queue: open tasks in `review_lanes`, stale first, then by lane order.
pub fn review_queue<'a>(
    tasks: &'a [Task],
    review_lanes: &[String],
    now_secs: u64,
    staleness_days: u32,
) -> Vec<&'a Task> {
    let mut out: Vec<&Task> = tasks
        .iter()
        .filter(|t| !t.done && !matches!(t.status, crate::TaskStatus::Cancelled))
        .filter(|t| {
            let lane = resolve_lane(t, "next");
            review_lanes.iter().any(|l| l == &lane)
        })
        .collect();

    out.sort_by(|a, b| {
        let sa = is_stale(a, now_secs, staleness_days);
        let sb = is_stale(b, now_secs, staleness_days);
        sb.cmp(&sa) // stale first
            .then_with(|| {
                let la = resolve_lane(a, "next");
                let lb = resolve_lane(b, "next");
                let ia = review_lanes.iter().position(|l| l == &la).unwrap_or(999);
                let ib = review_lanes.iter().position(|l| l == &lb).unwrap_or(999);
                ia.cmp(&ib)
            })
            .then_with(|| review_anchor(a).cmp(&review_anchor(b)))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProfileId, Task};

    #[test]
    fn parse_lane_and_ctx_tags() {
        let p = parse_tags_and_mentions(["lane/now", "ctx/home", "work"], []);
        assert_eq!(p.lane_id.as_deref(), Some("now"));
        assert_eq!(p.contexts.len(), 1);
        assert_eq!(p.contexts[0].kind, TaskContextKind::Place);
        assert_eq!(p.contexts[0].name, "home");
        assert_eq!(p.tags, vec!["work".to_string()]);
    }

    #[test]
    fn parse_at_mentions() {
        let p = parse_tags_and_mentions([], ["Call", "@phone", "about", "tax"]);
        assert_eq!(p.contexts.len(), 1);
        assert_eq!(p.contexts[0].kind, TaskContextKind::Tool);
        assert_eq!(p.contexts[0].name, "phone");
    }

    #[test]
    fn resolve_lane_prefers_explicit() {
        let mut t = Task::new("x", ProfileId::new());
        t.lane_id = Some("later".into());
        t.tags.push("lane/now".into());
        assert_eq!(resolve_lane(&t, "next"), "later");
    }

    #[test]
    fn resolve_waiting_from_waiting_for() {
        let mut t = Task::new("x", ProfileId::new());
        t.waiting_for = Some("Sam".into());
        assert_eq!(resolve_lane(&t, "next"), "waiting");
    }

    #[test]
    fn set_lane_tag_replaces() {
        let mut t = Task::new("x", ProfileId::new());
        t.tags = vec!["lane/now".into(), "work".into()];
        set_lane_tag(&mut t, "later");
        assert!(t.tags.iter().any(|x| x == "lane/later"));
        assert!(!t.tags.iter().any(|x| x == "lane/now"));
        assert!(t.tags.iter().any(|x| x == "work"));
        assert_eq!(t.lane_id.as_deref(), Some("later"));
    }

    #[test]
    fn list_engage_includes_now_and_next() {
        let pid = ProfileId::new();
        let mut a = Task::new("now task", pid);
        a.lane_id = Some("now".into());
        a.priority = TaskPriority::High;
        let mut b = Task::new("next task", pid);
        b.lane_id = Some("next".into());
        let mut c = Task::new("later task", pid);
        c.lane_id = Some("later".into());
        let mut d = Task::new("unlaned obsidian", pid); // resolves to next
        let tasks = vec![a, b, c, d];
        let f = EngageFilter::start_page("now");
        let got = list_engage(&tasks, &f, 1_750_000_000);
        let titles: Vec<_> = got.iter().map(|t| t.title.as_str()).collect();
        assert!(titles.contains(&"now task"));
        assert!(titles.contains(&"next task"));
        assert!(titles.contains(&"unlaned obsidian"));
        assert!(!titles.contains(&"later task"));
    }

    #[test]
    fn priority_word_urgent() {
        assert_eq!(parse_priority_word("highest"), TaskPriority::Urgent);
        assert_eq!(parse_priority_word("p4"), TaskPriority::Urgent);
        assert_eq!(parse_priority_word("high"), TaskPriority::High);
        assert_eq!(parse_priority_word("p1"), TaskPriority::Low);
    }

    #[test]
    fn todoist_priority_and_section_lane() {
        assert_eq!(todoist_priority(4), TaskPriority::Urgent);
        assert_eq!(todoist_priority(1), TaskPriority::Low);
        let lanes = crate::config::Config::default().task_lanes;
        assert_eq!(
            todoist_section_to_lane("Now", &lanes).as_deref(),
            Some("now")
        );
        assert_eq!(todoist_section_to_lane("Inbox", &lanes), None);
    }

    #[test]
    fn apply_todoist_fields_maps_labels_and_section() {
        let pid = ProfileId::new();
        let mut t = Task::new("", pid);
        let labels = vec!["ctx/home".into(), "work".into()];
        let lanes = crate::config::Config::default().task_lanes;
        apply_todoist_fields(
            &mut t,
            &TodoistTaskFields {
                content: "Buy milk",
                section_name: Some("Waiting"),
                labels: &labels,
                priority: 3,
                duration_minutes: Some(15),
                ..Default::default()
            },
            &lanes,
        );
        assert_eq!(t.title, "Buy milk");
        assert_eq!(t.priority, TaskPriority::High);
        assert_eq!(t.lane_id.as_deref(), Some("waiting"));
        assert_eq!(t.duration_minutes, Some(15));
        assert!(t.contexts.iter().any(|c| c.name == "home"));
        assert!(t.tags.iter().any(|x| x == "work"));
    }

    #[test]
    fn review_queue_stale_first() {
        let pid = ProfileId::new();
        let now = 1_750_000_000u64;
        let mut stale = Task::new("stale waiting", pid);
        set_lane_tag(&mut stale, "waiting");
        stale.created_at = Some(now - 30 * 86_400);
        let mut fresh = Task::new("fresh someday", pid);
        set_lane_tag(&mut fresh, "someday");
        fresh.reviewed_at = Some(now - 86_400);
        let tasks = vec![fresh, stale];
        let q = review_queue(&tasks, &["waiting".into(), "someday".into()], now, 14);
        assert_eq!(q[0].title, "stale waiting");
        assert!(is_stale(q[0], now, 14));
        assert!(!is_stale(q[1], now, 14));
    }
}
