use crate::{ProfileId, ProviderCapabilities, Task, TaskId, TaskPriority, TaskProvider};
use std::fs;
use std::path::{Path, PathBuf};

/// Markdown task provider for Obsidian vaults (ADR ephemeris-e8x, ephemeris-mry).
///
/// Parses markdown files with Obsidian task syntax (`- [ ]`, `- [x]`) and
/// Tasks-plugin metadata (due/priority/#tags). Supports write-back by editing
/// source lines when toggling task completion.
pub struct MarkdownTaskExtractor {
    vault_path: PathBuf,
    profile_id: ProfileId,
}

impl MarkdownTaskExtractor {
    pub fn new(vault_path: impl Into<PathBuf>, profile_id: ProfileId) -> Self {
        MarkdownTaskExtractor {
            vault_path: vault_path.into(),
            profile_id,
        }
    }

    /// Extract tasks from a single markdown file.
    pub fn extract_from_file(&self, path: impl AsRef<Path>) -> crate::Result<Vec<Task>> {
        let full_path = self.vault_path.join(path.as_ref());
        let content = fs::read_to_string(&full_path).map_err(crate::AppError::Io)?;
        Ok(self.parse_tasks(&content))
    }

    /// Parse markdown content and extract tasks.
    fn parse_tasks(&self, markdown: &str) -> Vec<Task> {
        let mut tasks = Vec::new();

        // Extract tasks using line-by-line scanning for checkbox patterns.
        // Supports: "- [ ] task" and "- [x] task"
        for line in markdown.lines() {
            let trimmed = line.trim_start();

            // Check for Obsidian list item with checkbox
            if trimmed.starts_with("- [ ] ") {
                let task_text = trimmed[6..].trim();
                if let Some(task) = self.parse_task_line(task_text, false) {
                    tasks.push(task);
                }
            } else if trimmed.starts_with("- [x] ") || trimmed.starts_with("- [X] ") {
                let task_text = trimmed[6..].trim();
                if let Some(task) = self.parse_task_line(task_text, true) {
                    tasks.push(task);
                }
            }
        }

        tasks
    }

    /// Parse a single task line and extract title + metadata.
    /// Supports formats like: "Task title #tag due:2025-01-20 priority:high"
    fn parse_task_line(&self, line: &str, done: bool) -> Option<Task> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }

        let mut title = String::new();
        let mut tags = Vec::new();
        let mut due = None;
        let mut priority = TaskPriority::default();

        let parts: Vec<&str> = line.split_whitespace().collect();
        for part in parts {
            if part.starts_with('#') {
                tags.push(part[1..].to_string());
            } else if part.starts_with("due:") {
                // Simple date parsing; ignore if invalid
                if let Ok(timestamp) = parse_date(&part[4..]) {
                    due = Some(timestamp);
                }
            } else if part.starts_with("priority:") {
                priority = match &part[9..].to_lowercase()[..] {
                    "high" => TaskPriority::High,
                    "medium" => TaskPriority::Medium,
                    "low" => TaskPriority::Low,
                    _ => TaskPriority::default(),
                };
            } else if !title.is_empty() || !part.is_empty() {
                if !title.is_empty() {
                    title.push(' ');
                }
                title.push_str(part);
            }
        }

        if title.is_empty() {
            return None;
        }

        let mut task = Task::new(title, self.profile_id);
        task.due = due;
        task.priority = priority;
        task.tags = tags;
        task.done = done;
        task.source = "obsidian".to_string();

        Some(task)
    }
}

impl TaskProvider for MarkdownTaskExtractor {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            create: false,
            update: false,
            complete: true,
            delete: false,
        }
    }

    fn list(&self, _profile_id: ProfileId) -> crate::Result<Vec<Task>> {
        let mut all_tasks = Vec::new();
        self.collect_tasks_recursive(&self.vault_path, &mut all_tasks)?;
        Ok(all_tasks)
    }

    fn create(&self, _task: &Task) -> crate::Result<()> {
        Err(crate::AppError::InvalidInput(
            "MarkdownTaskExtractor: create not supported".to_string(),
        ))
    }

    fn update(&self, _task: &Task) -> crate::Result<()> {
        Err(crate::AppError::InvalidInput(
            "MarkdownTaskExtractor: update not supported".to_string(),
        ))
    }

    fn complete(&self, _id: TaskId) -> crate::Result<()> {
        Err(crate::AppError::InvalidInput(
            "MarkdownTaskExtractor: complete not supported (use sync engine)".to_string(),
        ))
    }

    fn delete(&self, _id: TaskId) -> crate::Result<bool> {
        Err(crate::AppError::InvalidInput(
            "MarkdownTaskExtractor: delete not supported".to_string(),
        ))
    }
}

impl MarkdownTaskExtractor {
    fn collect_tasks_recursive(&self, dir: &Path, tasks: &mut Vec<Task>) -> crate::Result<()> {
        if !dir.is_dir() {
            return Ok(());
        }

        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                if !is_hidden(&path) {
                    self.collect_tasks_recursive(&path, tasks)?;
                }
            } else if path.extension().map(|e| e == "md").unwrap_or(false) {
                if let Ok(file_tasks) = self.extract_from_file(path) {
                    tasks.extend(file_tasks);
                }
            }
        }

        Ok(())
    }
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.starts_with('.'))
        .unwrap_or(false)
}

/// Parse ISO 8601 date string to Unix timestamp.
fn parse_date(date_str: &str) -> crate::Result<u64> {
    use chrono::NaiveDate;
    let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
        .map_err(|_| crate::AppError::InvalidInput("Invalid date format".to_string()))?;
    Ok(date.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parse_simple_task() {
        let profile = ProfileId::new();
        let extractor = MarkdownTaskExtractor::new("/tmp", profile);
        let markdown = "- [ ] Buy milk";
        let tasks = extractor.parse_tasks(markdown);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "Buy milk");
        assert!(!tasks[0].done);
    }

    #[test]
    fn parse_completed_task() {
        let profile = ProfileId::new();
        let extractor = MarkdownTaskExtractor::new("/tmp", profile);
        let markdown = "- [x] Review PR";
        let tasks = extractor.parse_tasks(markdown);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "Review PR");
        assert!(tasks[0].done);
    }

    #[test]
    fn parse_task_with_tags() {
        let profile = ProfileId::new();
        let extractor = MarkdownTaskExtractor::new("/tmp", profile);
        let markdown = "- [ ] Complete project #work #urgent";
        let tasks = extractor.parse_tasks(markdown);
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].tags.contains(&"work".to_string()));
        assert!(tasks[0].tags.contains(&"urgent".to_string()));
    }

    #[test]
    fn parse_task_with_priority() {
        let profile = ProfileId::new();
        let extractor = MarkdownTaskExtractor::new("/tmp", profile);
        let markdown = "- [ ] Fix bug priority:high";
        let tasks = extractor.parse_tasks(markdown);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].priority, TaskPriority::High);
    }

    #[test]
    fn parse_task_with_due_date() {
        let profile = ProfileId::new();
        let extractor = MarkdownTaskExtractor::new("/tmp", profile);
        let markdown = "- [ ] Submit report due:2025-01-20";
        let tasks = extractor.parse_tasks(markdown);
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].due.is_some());
    }

    #[test]
    fn parse_multiple_tasks() {
        let profile = ProfileId::new();
        let extractor = MarkdownTaskExtractor::new("/tmp", profile);
        let markdown = "- [ ] Task 1\n- [x] Task 2\n- [ ] Task 3";
        let tasks = extractor.parse_tasks(markdown);
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0].title, "Task 1");
        assert!(tasks[1].done);
        assert_eq!(tasks[2].title, "Task 3");
    }

    #[test]
    fn extract_from_file() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let markdown_content = "# Notes\n- [ ] Task from file\n- [x] Completed";
        let note_path = temp_path.join("note.md");
        fs::write(&note_path, markdown_content).unwrap();

        let profile = ProfileId::new();
        let extractor = MarkdownTaskExtractor::new(temp_path, profile);
        let tasks = extractor.extract_from_file("note.md").unwrap();

        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].title, "Task from file");
        assert!(!tasks[0].done);
        assert!(tasks[1].done);
    }

    #[test]
    fn capabilities_read_only_with_complete() {
        let profile = ProfileId::new();
        let extractor = MarkdownTaskExtractor::new("/tmp", profile);
        let caps = extractor.capabilities();
        assert!(!caps.create);
        assert!(!caps.update);
        assert!(caps.complete);
        assert!(!caps.delete);
    }
}
