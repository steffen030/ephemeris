use crate::task_context::{
    apply_parsed_tags, parse_priority_word, parse_tags_and_mentions, project_from_vault_path,
    sync_tags_from_structured,
};
use crate::{
    ProfileId, ProviderCapabilities, Task, TaskId, TaskPriority, TaskProvider, TaskStatus,
};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Namespace for stable Obsidian task ids (UUID v5 over path + line).
const OBSIDIAN_TASK_NS: Uuid = Uuid::from_bytes([
    0x65, 0x70, 0x68, 0x65, 0x6d, 0x65, 0x72, 0x69, // "ephemeri"
    0x73, 0x2d, 0x6f, 0x62, 0x73, 0x69, 0x64, 0x6e, // "s-obsidn"
]);

/// Default vault-relative inbox for newly created tasks.
pub const DEFAULT_TASKS_INBOX: &str = "Ephemeris/Tasks.md";

/// Markdown task provider for Obsidian vaults (ADR ephemeris-e8x, ephemeris-mry).
///
/// Parses markdown checkbox tasks and Tasks-plugin metadata:
/// `due:` / `📅`, `scheduled:` / `⏳`, `start:` / `🛫`, `priority:` / ⏫🔼🔽⏬,
/// `#tags`, `#lane/<id>`, `#ctx/<name>`, `@mentions`, `🔁` recurrence,
/// `🆔`/`⛔` dependencies, nearest heading → section, path → project,
/// and status marks `[ ]` `/` `x` `-`.
pub struct MarkdownTaskExtractor {
    vault_path: PathBuf,
    profile_id: ProfileId,
    /// Vault-relative path where [`TaskProvider::create`] appends new tasks.
    inbox_rel: PathBuf,
    /// When set, notebook export folders under this subdir are not walked for
    /// tasks; the inbox file is still read even if it lives under the subdir.
    exclude_export_subdir: Option<PathBuf>,
}

impl MarkdownTaskExtractor {
    pub fn new(vault_path: impl Into<PathBuf>, profile_id: ProfileId) -> Self {
        Self::with_inbox(vault_path, profile_id, DEFAULT_TASKS_INBOX)
    }

    pub fn with_inbox(
        vault_path: impl Into<PathBuf>,
        profile_id: ProfileId,
        inbox_rel: impl Into<PathBuf>,
    ) -> Self {
        MarkdownTaskExtractor {
            vault_path: vault_path.into(),
            profile_id,
            inbox_rel: inbox_rel.into(),
            exclude_export_subdir: None,
        }
    }

    /// Skip exported note folders under `subdir` when listing tasks.
    pub fn with_exclude_export_subdir(mut self, subdir: impl Into<PathBuf>) -> Self {
        self.exclude_export_subdir = Some(subdir.into());
        self
    }

    /// Extract tasks from a single markdown file (vault-relative or absolute).
    pub fn extract_from_file(&self, path: impl AsRef<Path>) -> crate::Result<Vec<Task>> {
        let full_path = resolve_vault_path(&self.vault_path, path.as_ref());
        let content = fs::read_to_string(&full_path).map_err(crate::AppError::Io)?;
        let rel = relative_vault_path(&self.vault_path, &full_path);
        Ok(self.parse_tasks(&content, Some(&rel)))
    }

    /// Parse markdown content and extract tasks.
    fn parse_tasks(&self, markdown: &str, rel_path: Option<&str>) -> Vec<Task> {
        let mut tasks = Vec::new();
        let mut current_heading: Option<String> = None;
        let project = rel_path.and_then(project_from_vault_path);

        for (idx, line) in markdown.lines().enumerate() {
            let line_no = idx + 1;
            let trimmed = line.trim_start();

            if let Some(heading) = parse_markdown_heading(trimmed) {
                current_heading = Some(heading);
                continue;
            }

            let (status, task_text) = match parse_checkbox_prefix(trimmed) {
                Some(v) => v,
                None => continue,
            };

            if let Some(mut task) = self.parse_task_line(task_text, status) {
                if let Some(rel) = rel_path {
                    let source_ref = format!("{rel}:{line_no}");
                    task.id = stable_task_id(rel, line_no);
                    task.source_ref = Some(source_ref);
                }
                if task.project.is_none() {
                    task.project = project.clone();
                }
                if task.section.is_none() {
                    task.section = current_heading.clone();
                }
                tasks.push(task);
            }
        }

        tasks
    }

    /// Parse a single task line body (after the checkbox) into a [`Task`].
    fn parse_task_line(&self, line: &str, status: TaskStatus) -> Option<Task> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }

        let mut title_parts = Vec::new();
        let mut raw_tags = Vec::new();
        let mut due = None;
        let mut scheduled = None;
        let mut start = None;
        let mut completed_at = None;
        let mut created_at = None;
        let mut cancelled_at = None;
        let mut recurrence = None;
        let mut priority = TaskPriority::default();
        let mut depends_on = Vec::new();
        let mut task_id_token: Option<String> = None;

        let parts: Vec<&str> = line.split_whitespace().collect();
        let mut i = 0;
        while i < parts.len() {
            let part = parts[i];

            if part.starts_with('#') && part.len() > 1 {
                raw_tags.push(part[1..].to_string());
                i += 1;
                continue;
            }

            // Textual key:value metadata
            if let Some((key, val)) = part.split_once(':') {
                let key = key.to_lowercase();
                match key.as_str() {
                    "due" => {
                        if let Ok(ts) = parse_date(val) {
                            due = Some(ts);
                        }
                        i += 1;
                        continue;
                    }
                    "scheduled" | "sched" => {
                        if let Ok(ts) = parse_date(val) {
                            scheduled = Some(ts);
                        }
                        i += 1;
                        continue;
                    }
                    "start" => {
                        if let Ok(ts) = parse_date(val) {
                            start = Some(ts);
                        }
                        i += 1;
                        continue;
                    }
                    "completed" | "done" => {
                        if let Ok(ts) = parse_date(val) {
                            completed_at = Some(ts);
                        }
                        i += 1;
                        continue;
                    }
                    "created" => {
                        if let Ok(ts) = parse_date(val) {
                            created_at = Some(ts);
                        }
                        i += 1;
                        continue;
                    }
                    "cancelled" | "canceled" => {
                        if let Ok(ts) = parse_date(val) {
                            cancelled_at = Some(ts);
                        }
                        i += 1;
                        continue;
                    }
                    "priority" | "prio" => {
                        priority = parse_priority_word(val);
                        i += 1;
                        continue;
                    }
                    "repeat" | "recurrence" => {
                        recurrence = Some(val.to_string());
                        i += 1;
                        continue;
                    }
                    "id" => {
                        task_id_token = Some(val.to_string());
                        i += 1;
                        continue;
                    }
                    "dependsOn" | "dependson" | "depends" => {
                        for id in val.split(',') {
                            let id = id.trim();
                            if !id.is_empty() {
                                depends_on.push(id.to_string());
                            }
                        }
                        i += 1;
                        continue;
                    }
                    "waiting" | "waiting_for" | "waiting-for" => {
                        // handled after title assembly via raw token; keep as title word
                    }
                    _ => {}
                }
            }

            // Obsidian Tasks emoji metadata: emoji then date/rule as next token
            if let Some(kind) = emoji_meta_kind(part) {
                if let EmojiMeta::Priority(p) = kind {
                    priority = p;
                    i += 1;
                    continue;
                }
                if let EmojiMeta::Id = kind {
                    if let Some(next) = parts.get(i + 1) {
                        task_id_token = Some((*next).to_string());
                        i += 2;
                        continue;
                    }
                }
                if let EmojiMeta::Depends = kind {
                    if let Some(next) = parts.get(i + 1) {
                        for id in next.split(',') {
                            let id = id.trim();
                            if !id.is_empty() {
                                depends_on.push(id.to_string());
                            }
                        }
                        i += 2;
                        continue;
                    }
                }
                if let Some(next) = parts.get(i + 1) {
                    match kind {
                        EmojiMeta::Due => {
                            if let Ok(ts) = parse_date(next) {
                                due = Some(ts);
                            }
                        }
                        EmojiMeta::Scheduled => {
                            if let Ok(ts) = parse_date(next) {
                                scheduled = Some(ts);
                            }
                        }
                        EmojiMeta::Start => {
                            if let Ok(ts) = parse_date(next) {
                                start = Some(ts);
                            }
                        }
                        EmojiMeta::Completed => {
                            if let Ok(ts) = parse_date(next) {
                                completed_at = Some(ts);
                            }
                        }
                        EmojiMeta::Created => {
                            if let Ok(ts) = parse_date(next) {
                                created_at = Some(ts);
                            }
                        }
                        EmojiMeta::Cancelled => {
                            if let Ok(ts) = parse_date(next) {
                                cancelled_at = Some(ts);
                            }
                        }
                        EmojiMeta::Recurrence => {
                            // Collect remaining recurrence words until next emoji/tag/key
                            let mut rule = String::new();
                            i += 1;
                            while i < parts.len() {
                                let p = parts[i];
                                if p.starts_with('#')
                                    || emoji_meta_kind(p).is_some()
                                    || p.contains(':')
                                {
                                    i -= 1;
                                    break;
                                }
                                if !rule.is_empty() {
                                    rule.push(' ');
                                }
                                rule.push_str(p);
                                i += 1;
                            }
                            recurrence = Some(rule);
                            i += 1;
                            continue;
                        }
                        EmojiMeta::Priority(_) | EmojiMeta::Id | EmojiMeta::Depends => {}
                    }
                    i += 2;
                    continue;
                }
            }

            title_parts.push(part.to_string());
            i += 1;
        }

        let title = title_parts.join(" ");
        if title.is_empty() {
            return None;
        }

        let done = matches!(status, TaskStatus::Done);
        let mut task = Task::new(title.clone(), self.profile_id);
        task.due = due;
        task.scheduled = scheduled;
        task.start = start;
        task.completed_at = completed_at;
        task.created_at = created_at;
        task.cancelled_at = cancelled_at;
        task.recurrence = recurrence;
        task.priority = priority;
        task.depends_on = depends_on;
        // Opaque Obsidian id kept in url-ish slot only if we need round-trip later;
        // store as description note when present and no description yet.
        let _ = task_id_token;
        task.done = done;
        task.status = status;
        task.source = "obsidian".to_string();

        let title_refs: Vec<&str> = title_parts.iter().map(|s| s.as_str()).collect();
        let tag_refs: Vec<&str> = raw_tags.iter().map(|s| s.as_str()).collect();
        let parsed = parse_tags_and_mentions(tag_refs, title_refs);
        apply_parsed_tags(&mut task, parsed);

        // Strip @mentions from displayed title (contexts already captured).
        let clean_title: Vec<&str> = title_parts
            .iter()
            .map(|s| s.as_str())
            .filter(|s| !s.starts_with('@'))
            .collect();
        if !clean_title.is_empty() {
            task.title = clean_title.join(" ");
        }

        Some(task)
    }

    fn collect_tasks_recursive(&self, dir: &Path, tasks: &mut Vec<Task>) -> crate::Result<()> {
        if !dir.is_dir() {
            return Ok(());
        }

        // Inside the export tree: only read the configured inbox; skip note mirrors.
        if let Some(ref ex) = self.exclude_export_subdir {
            if crate::path_under_export_subdir(&self.vault_path, dir, ex) {
                let inbox_abs = self.vault_path.join(&self.inbox_rel);
                if inbox_abs.is_file()
                    && crate::path_under_export_subdir(&self.vault_path, &inbox_abs, ex)
                {
                    if let Ok(file_tasks) = self.extract_from_file(&inbox_abs) {
                        tasks.extend(file_tasks);
                    }
                }
                return Ok(());
            }
        }

        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                if is_hidden(&path) {
                    continue;
                }
                self.collect_tasks_recursive(&path, tasks)?;
            } else if path.extension().map(|e| e == "md").unwrap_or(false) {
                if let Ok(file_tasks) = self.extract_from_file(&path) {
                    tasks.extend(file_tasks);
                }
            }
        }

        Ok(())
    }

    fn find_task(&self, id: TaskId) -> crate::Result<Task> {
        self.list(self.profile_id)?
            .into_iter()
            .find(|t| t.id == id)
            .ok_or_else(|| crate::AppError::NotFound(format!("obsidian task {id:?}")))
    }

    fn rewrite_task_line(&self, task: &Task) -> crate::Result<()> {
        let source_ref = task.source_ref.as_deref().ok_or_else(|| {
            crate::AppError::InvalidInput("obsidian task missing source_ref".into())
        })?;
        let (rel, line_no) = parse_source_ref(source_ref)?;
        let full_path = self.vault_path.join(&rel);
        let content = fs::read_to_string(&full_path).map_err(crate::AppError::Io)?;
        let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
        let idx = line_no
            .checked_sub(1)
            .ok_or_else(|| crate::AppError::InvalidInput("invalid source_ref line".into()))?;
        if idx >= lines.len() {
            return Err(crate::AppError::NotFound(format!(
                "line {line_no} gone in {}",
                rel.display()
            )));
        }

        let old = &lines[idx];
        let indent: String = old.chars().take_while(|c| c.is_whitespace()).collect();
        if parse_checkbox_prefix(old.trim_start()).is_none() {
            return Err(crate::AppError::InvalidInput(format!(
                "{}:{line_no} is not a checkbox task line",
                rel.display()
            )));
        }
        lines[idx] = format!("{indent}{}", format_checkbox_line(task));

        let mut out = lines.join("\n");
        if content.ends_with('\n') {
            out.push('\n');
        }
        atomic_write(&full_path, &out)
    }
}

impl TaskProvider for MarkdownTaskExtractor {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            create: true,
            update: true,
            complete: true,
            delete: false,
        }
    }

    fn list(&self, _profile_id: ProfileId) -> crate::Result<Vec<Task>> {
        let mut all_tasks = Vec::new();
        self.collect_tasks_recursive(&self.vault_path, &mut all_tasks)?;
        Ok(all_tasks)
    }

    fn create(&self, task: &Task) -> crate::Result<()> {
        let full_path = self.vault_path.join(&self.inbox_rel);
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).map_err(crate::AppError::Io)?;
        }

        let line = format_checkbox_line(task);
        let mut content = if full_path.exists() {
            fs::read_to_string(&full_path).map_err(crate::AppError::Io)?
        } else {
            String::new()
        };
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(&line);
        content.push('\n');
        atomic_write(&full_path, &content)
    }

    fn update(&self, task: &Task) -> crate::Result<()> {
        if task.source != "obsidian" {
            return Err(crate::AppError::InvalidInput(
                "MarkdownTaskExtractor: update only for obsidian tasks".into(),
            ));
        }
        self.rewrite_task_line(task)
    }

    fn complete(&self, id: TaskId) -> crate::Result<()> {
        let mut task = self.find_task(id)?;
        task.set_done(true);
        self.rewrite_task_line(&task)
    }

    fn delete(&self, _id: TaskId) -> crate::Result<bool> {
        Err(crate::AppError::InvalidInput(
            "MarkdownTaskExtractor: delete not supported".to_string(),
        ))
    }
}

fn stable_task_id(rel: &str, line_no: usize) -> TaskId {
    let key = format!("{rel}\0{line_no}");
    TaskId(Uuid::new_v5(&OBSIDIAN_TASK_NS, key.as_bytes()))
}

fn parse_source_ref(source_ref: &str) -> crate::Result<(PathBuf, usize)> {
    let (path, line) = source_ref
        .rsplit_once(':')
        .ok_or_else(|| crate::AppError::InvalidInput(format!("bad source_ref: {source_ref}")))?;
    let line_no: usize = line
        .parse()
        .map_err(|_| crate::AppError::InvalidInput(format!("bad source_ref line: {source_ref}")))?;
    Ok((PathBuf::from(path), line_no))
}

fn parse_checkbox_prefix(trimmed: &str) -> Option<(TaskStatus, &str)> {
    let patterns: &[(&str, TaskStatus)] = &[
        ("- [ ] ", TaskStatus::Todo),
        ("- [x] ", TaskStatus::Done),
        ("- [X] ", TaskStatus::Done),
        ("- [/] ", TaskStatus::InProgress),
        ("- [-] ", TaskStatus::Cancelled),
    ];
    for (prefix, status) in patterns {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return Some((*status, rest.trim()));
        }
    }
    None
}

fn format_checkbox_line(task: &Task) -> String {
    let mut task = task.clone();
    sync_tags_from_structured(&mut task);

    let mark = match task.status {
        TaskStatus::Todo => " ",
        TaskStatus::InProgress => "/",
        TaskStatus::Done => "x",
        TaskStatus::Cancelled => "-",
    };
    // Prefer explicit status; fall back to done flag.
    let mark = if task.done && matches!(task.status, TaskStatus::Todo) {
        "x"
    } else {
        mark
    };

    let mut body = task.title.clone();
    for ctx in &task.contexts {
        if matches!(
            ctx.kind,
            crate::TaskContextKind::Tool | crate::TaskContextKind::Person
        ) {
            body.push_str(" @");
            body.push_str(&ctx.name);
        }
    }
    for tag in &task.tags {
        body.push_str(" #");
        body.push_str(tag);
    }
    if let Some(due) = task.due.and_then(format_due) {
        body.push_str(" 📅 ");
        body.push_str(&due);
    }
    if let Some(sched) = task.scheduled.and_then(format_due) {
        body.push_str(" ⏳ ");
        body.push_str(&sched);
    }
    if let Some(start) = task.start.and_then(format_due) {
        body.push_str(" 🛫 ");
        body.push_str(&start);
    }
    if let Some(created) = task.created_at.and_then(format_due) {
        body.push_str(" ➕ ");
        body.push_str(&created);
    }
    if let Some(done_at) = task.completed_at.and_then(format_due) {
        body.push_str(" ✅ ");
        body.push_str(&done_at);
    }
    if let Some(cancelled) = task.cancelled_at.and_then(format_due) {
        body.push_str(" ❌ ");
        body.push_str(&cancelled);
    }
    if let Some(ref rule) = task.recurrence {
        if !rule.is_empty() {
            body.push_str(" 🔁 ");
            body.push_str(rule);
        }
    }
    if !task.depends_on.is_empty() {
        body.push_str(" ⛔ ");
        body.push_str(&task.depends_on.join(","));
    }
    match task.priority {
        TaskPriority::Urgent => body.push_str(" ⏫"),
        TaskPriority::High => body.push_str(" 🔼"),
        TaskPriority::Low => body.push_str(" 🔽"),
        TaskPriority::Medium => {}
    }
    format!("- [{mark}] {body}")
}

fn format_due(ts: u64) -> Option<String> {
    use chrono::{DateTime, Utc};
    let dt = DateTime::<Utc>::from_timestamp(ts as i64, 0)?;
    Some(dt.format("%Y-%m-%d").to_string())
}

enum EmojiMeta {
    Due,
    Scheduled,
    Start,
    Completed,
    Created,
    Cancelled,
    Recurrence,
    Priority(TaskPriority),
    Id,
    Depends,
}

fn emoji_meta_kind(token: &str) -> Option<EmojiMeta> {
    match token {
        "📅" | "🗓" => Some(EmojiMeta::Due),
        "⏳" => Some(EmojiMeta::Scheduled),
        "🛫" => Some(EmojiMeta::Start),
        "✅" => Some(EmojiMeta::Completed),
        "➕" => Some(EmojiMeta::Created),
        "❌" => Some(EmojiMeta::Cancelled),
        "🔁" => Some(EmojiMeta::Recurrence),
        "⏫" => Some(EmojiMeta::Priority(TaskPriority::Urgent)),
        "🔼" => Some(EmojiMeta::Priority(TaskPriority::High)),
        "🔽" => Some(EmojiMeta::Priority(TaskPriority::Low)),
        "⏬" => Some(EmojiMeta::Priority(TaskPriority::Low)),
        "🆔" => Some(EmojiMeta::Id),
        "⛔" => Some(EmojiMeta::Depends),
        _ => None,
    }
}

fn parse_markdown_heading(trimmed: &str) -> Option<String> {
    let bytes = trimmed.as_bytes();
    if bytes.is_empty() || bytes[0] != b'#' {
        return None;
    }
    let mut i = 0;
    while i < bytes.len() && bytes[i] == b'#' {
        i += 1;
    }
    if i == 0 || i > 6 {
        return None;
    }
    if i < bytes.len() && bytes[i] != b' ' {
        return None;
    }
    let title = trimmed[i..].trim();
    if title.is_empty() {
        return None;
    }
    Some(title.to_string())
}

fn resolve_vault_path(vault: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        vault.join(path)
    }
}

fn relative_vault_path(vault: &Path, full: &Path) -> String {
    full.strip_prefix(vault)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| full.to_string_lossy().replace('\\', "/"))
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with('.'))
        .unwrap_or(false)
}

fn parse_date(s: &str) -> Result<u64, ()> {
    use chrono::{NaiveDate, TimeZone, Utc};
    let date = NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| ())?;
    let dt = date.and_hms_opt(0, 0, 0).ok_or(())?;
    Ok(Utc.from_utc_datetime(&dt).timestamp() as u64)
}

fn atomic_write(path: &Path, content: &str) -> crate::Result<()> {
    let tmp = path.with_extension("md.tmp");
    fs::write(&tmp, content).map_err(crate::AppError::Io)?;
    fs::rename(&tmp, path).map_err(crate::AppError::Io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TaskProvider;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn exported_note_checkboxes_are_not_scanned() {
        let tmp = TempDir::new().unwrap();
        let export = tmp.path().join("Ephemeris/Notes");
        fs::create_dir_all(&export).unwrap();
        fs::write(export.join("Note.md"), "- [ ] handwritten\n").unwrap();
        fs::write(tmp.path().join("Real.md"), "- [ ] typed\n").unwrap();
        let ext = MarkdownTaskExtractor::new(tmp.path(), ProfileId::new())
            .with_exclude_export_subdir("Ephemeris");
        let titles: Vec<String> = ext
            .list(ProfileId::new())
            .unwrap()
            .into_iter()
            .map(|t| t.title)
            .collect();
        assert_eq!(titles, vec!["typed".to_string()]);
    }

    #[test]
    fn parse_simple_task() {
        let ext = MarkdownTaskExtractor::new("/tmp", ProfileId::new());
        let tasks = ext.parse_tasks("- [ ] Buy milk #errands", Some("a.md"));
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "Buy milk");
        assert!(!tasks[0].done);
        assert_eq!(tasks[0].tags, vec!["errands".to_string()]);
    }

    #[test]
    fn parse_completed_task() {
        let ext = MarkdownTaskExtractor::new("/tmp", ProfileId::new());
        let tasks = ext.parse_tasks("- [x] Review PR", Some("a.md"));
        assert_eq!(tasks[0].title, "Review PR");
        assert!(tasks[0].done);
        assert_eq!(tasks[0].status, TaskStatus::Done);
    }

    #[test]
    fn parse_emoji_due_and_priority() {
        let ext = MarkdownTaskExtractor::new("/tmp", ProfileId::new());
        let tasks = ext.parse_tasks("- [ ] Ship feature 📅 2025-06-01 🔼 #work", Some("a.md"));
        assert_eq!(tasks[0].title, "Ship feature");
        assert!(tasks[0].due.is_some());
        assert_eq!(tasks[0].priority, TaskPriority::High);
        assert_eq!(tasks[0].tags, vec!["work".to_string()]);
    }

    #[test]
    fn parse_lane_ctx_and_heading_section() {
        let ext = MarkdownTaskExtractor::new("/tmp", ProfileId::new());
        let md = "## Inbox\n- [ ] Call bank @phone #lane/now #ctx/home\n";
        let tasks = ext.parse_tasks(md, Some("Work/note.md"));
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].section.as_deref(), Some("Inbox"));
        assert_eq!(tasks[0].project.as_deref(), Some("Work"));
        assert_eq!(tasks[0].lane_id.as_deref(), Some("now"));
        assert!(tasks[0]
            .contexts
            .iter()
            .any(|c| c.name == "phone" || c.name == "home"));
        assert!(!tasks[0].title.contains('@'));
    }

    #[test]
    fn parse_urgent_priority_emoji() {
        let ext = MarkdownTaskExtractor::new("/tmp", ProfileId::new());
        let tasks = ext.parse_tasks("- [ ] Critical ⏫", Some("a.md"));
        assert_eq!(tasks[0].priority, TaskPriority::Urgent);
    }

    #[test]
    fn parse_in_progress_and_cancelled() {
        let ext = MarkdownTaskExtractor::new("/tmp", ProfileId::new());
        let tasks = ext.parse_tasks("- [/] Doing\n- [-] Dropped", Some("a.md"));
        assert_eq!(tasks[0].status, TaskStatus::InProgress);
        assert_eq!(tasks[1].status, TaskStatus::Cancelled);
    }

    #[test]
    fn parse_task_with_due_date() {
        let ext = MarkdownTaskExtractor::new("/tmp", ProfileId::new());
        let tasks = ext.parse_tasks("- [ ] Call due:2025-01-20", Some("a.md"));
        assert!(tasks[0].due.is_some());
    }

    #[test]
    fn parse_task_with_priority() {
        let ext = MarkdownTaskExtractor::new("/tmp", ProfileId::new());
        let tasks = ext.parse_tasks("- [ ] Urgent priority:high", Some("a.md"));
        assert_eq!(tasks[0].priority, TaskPriority::High);
    }

    #[test]
    fn parse_task_with_tags() {
        let ext = MarkdownTaskExtractor::new("/tmp", ProfileId::new());
        let tasks = ext.parse_tasks("- [ ] Task 1\n- [ ] Task 2 #a\n- [ ] Task 3", Some("a.md"));
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0].title, "Task 1");
        assert_eq!(tasks[2].title, "Task 3");
    }

    #[test]
    fn parse_multiple_tasks() {
        let ext = MarkdownTaskExtractor::new("/tmp", ProfileId::new());
        let tasks = ext.parse_tasks("- [ ] A\n- [x] B\n- [ ] C", Some("a.md"));
        assert_eq!(tasks.len(), 3);
    }

    #[test]
    fn stable_ids_are_deterministic() {
        let ext = MarkdownTaskExtractor::new("/tmp", ProfileId::new());
        let a = ext.parse_tasks("- [ ] Same", Some("note.md"));
        let b = ext.parse_tasks("- [ ] Same", Some("note.md"));
        assert_eq!(a[0].id, b[0].id);
    }

    #[test]
    fn extract_from_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("t.md");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "- [ ] Task from file").unwrap();
        let ext = MarkdownTaskExtractor::new(tmp.path(), ProfileId::new());
        let tasks = ext.extract_from_file("t.md").unwrap();
        assert_eq!(tasks[0].title, "Task from file");
    }

    #[test]
    fn create_appends_to_inbox() {
        let tmp = TempDir::new().unwrap();
        let pid = ProfileId::new();
        let ext = MarkdownTaskExtractor::with_inbox(tmp.path(), pid, "Inbox.md");
        let mut task = Task::new("New one", pid);
        task.source = "obsidian".to_string();
        TaskProvider::create(&ext, &task).unwrap();
        let content = fs::read_to_string(tmp.path().join("Inbox.md")).unwrap();
        assert!(content.contains("- [ ] New one"));
    }

    #[test]
    fn complete_toggles_checkbox_in_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("t.md");
        fs::write(&path, "- [ ] Buy milk\n").unwrap();
        let pid = ProfileId::new();
        let ext = MarkdownTaskExtractor::new(tmp.path(), pid);
        let tasks = ext.extract_from_file("t.md").unwrap();
        let milk = tasks.iter().find(|t| t.title == "Buy milk").unwrap();
        TaskProvider::complete(&ext, milk.id).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("- [x] Buy milk"));
    }

    #[test]
    fn update_can_uncomplete_and_change_priority() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("t.md");
        fs::write(&path, "- [x] Done thing priority:low\n").unwrap();
        let pid = ProfileId::new();
        let ext = MarkdownTaskExtractor::new(tmp.path(), pid);
        let mut tasks = ext.extract_from_file("t.md").unwrap();
        let mut t = tasks.remove(0);
        t.set_done(false);
        t.priority = TaskPriority::High;
        t.title = "Renamed".into();
        TaskProvider::update(&ext, &t).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("- [ ] Renamed"));
        assert!(content.contains("🔼"));
    }

    #[test]
    fn capabilities_full_write_except_delete() {
        let ext = MarkdownTaskExtractor::new("/tmp", ProfileId::new());
        let caps = TaskProvider::capabilities(&ext);
        assert!(caps.create && caps.update && caps.complete && !caps.delete);
    }

    #[test]
    fn format_round_trips_emoji_fields() {
        let pid = ProfileId::new();
        let mut t = Task::new("Ship", pid);
        t.source = "obsidian".to_string();
        t.due = Some(parse_date("2025-06-01").unwrap());
        t.scheduled = Some(parse_date("2025-05-01").unwrap());
        t.priority = TaskPriority::High;
        t.tags.push("work".into());
        t.recurrence = Some("every week".into());
        let line = format_checkbox_line(&t);
        let ext = MarkdownTaskExtractor::new("/tmp", pid);
        let parsed = ext.parse_task_line(line.strip_prefix("- [ ] ").unwrap(), TaskStatus::Todo);
        let p = parsed.unwrap();
        assert_eq!(p.title, "Ship");
        assert_eq!(p.due, t.due);
        assert_eq!(p.scheduled, t.scheduled);
        assert_eq!(p.priority, TaskPriority::High);
        assert_eq!(p.recurrence.as_deref(), Some("every week"));
    }
}
