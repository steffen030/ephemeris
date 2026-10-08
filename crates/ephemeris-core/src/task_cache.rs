use crate::{ProfileId, Task, TaskId};
use std::collections::HashMap;

/// Task cache and aggregator (ADR ephemeris-e8x, ephemeris-fy6).
///
/// Aggregates tasks from all enabled TaskProviders into one list,
/// tagged with source + profile, supporting cross-source dedup and filtering.
pub struct TaskCache {
    // Key: TaskId; Value: Task
    tasks: HashMap<TaskId, Task>,
    // All task IDs indexed by profile
    by_profile: HashMap<ProfileId, Vec<TaskId>>,
}

impl TaskCache {
    pub fn new() -> Self {
        TaskCache {
            tasks: HashMap::new(),
            by_profile: HashMap::new(),
        }
    }

    /// Add or replace a task.
    pub fn upsert(&mut self, task: Task) {
        let task_id = task.id;
        let profile_id = task.profile_id;

        // Track profile index
        self.by_profile.entry(profile_id).or_default();

        // If new task, track its ID in profile index
        if !self.tasks.contains_key(&task_id) {
            if let Some(tasks) = self.by_profile.get_mut(&profile_id) {
                tasks.push(task_id);
            }
        }

        self.tasks.insert(task_id, task);
    }

    /// Load all tasks from a source (replaces previous tasks from that source).
    pub fn load_from_source(&mut self, source: &str, profile_id: ProfileId, tasks: Vec<Task>) {
        // Remove old tasks from this source/profile
        let ids_to_remove: Vec<_> = self
            .tasks
            .iter()
            .filter(|(_, t)| t.source == source && t.profile_id == profile_id)
            .map(|(_, t)| t.id)
            .collect();
        for id in ids_to_remove {
            self.tasks.remove(&id);
            if let Some(profile_tasks) = self.by_profile.get_mut(&profile_id) {
                profile_tasks.retain(|&tid| tid != id);
            }
        }

        // Add new tasks
        for task in tasks {
            self.upsert(task);
        }
    }

    /// Get all tasks across all profiles.
    pub fn list_all(&self) -> Vec<Task> {
        let mut tasks: Vec<_> = self.tasks.values().cloned().collect();
        // Sort by priority (High > Medium > Low) then by creation order
        tasks.sort_by(|a, b| {
            use crate::TaskPriority::*;
            let a_pri = match a.priority {
                High => 0,
                Medium => 1,
                Low => 2,
            };
            let b_pri = match b.priority {
                High => 0,
                Medium => 1,
                Low => 2,
            };
            a_pri.cmp(&b_pri)
        });
        tasks
    }

    /// Get all tasks for a profile.
    pub fn list_for_profile(&self, profile_id: ProfileId) -> Vec<Task> {
        let all = self.list_all();
        all.into_iter()
            .filter(|t| t.profile_id == profile_id)
            .collect()
    }

    /// Get all open (not done) tasks.
    pub fn list_open(&self) -> Vec<Task> {
        let mut tasks: Vec<_> = self.tasks.values().filter(|t| !t.done).cloned().collect();
        tasks.sort_by(|a, b| {
            use crate::TaskPriority::*;
            let a_pri = match a.priority {
                High => 0,
                Medium => 1,
                Low => 2,
            };
            let b_pri = match b.priority {
                High => 0,
                Medium => 1,
                Low => 2,
            };
            a_pri.cmp(&b_pri)
        });
        tasks
    }

    /// Get all open tasks for a profile.
    pub fn list_open_for_profile(&self, profile_id: ProfileId) -> Vec<Task> {
        self.list_open()
            .into_iter()
            .filter(|t| t.profile_id == profile_id)
            .collect()
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub fn get(&self, id: TaskId) -> Option<&Task> {
        self.tasks.get(&id)
    }

    pub fn get_mut(&mut self, id: TaskId) -> Option<&mut Task> {
        self.tasks.get_mut(&id)
    }
}

impl Default for TaskCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TaskPriority;

    fn make_task(title: &str, priority: TaskPriority, profile_id: ProfileId) -> Task {
        let mut task = Task::new(title, profile_id);
        task.priority = priority;
        task
    }

    #[test]
    fn cache_creation() {
        let cache = TaskCache::new();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn upsert_single_task() {
        let mut cache = TaskCache::new();
        let profile = ProfileId::new();
        let task = make_task("Review PR", TaskPriority::High, profile);

        cache.upsert(task);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn list_all_tasks() {
        let mut cache = TaskCache::new();
        let profile = ProfileId::new();

        cache.upsert(make_task("Low priority", TaskPriority::Low, profile));
        cache.upsert(make_task("High priority", TaskPriority::High, profile));
        cache.upsert(make_task("Medium priority", TaskPriority::Medium, profile));

        let tasks = cache.list_all();
        assert_eq!(tasks.len(), 3);
        // Should be sorted by priority: High > Medium > Low
        assert_eq!(tasks[0].priority, TaskPriority::High);
        assert_eq!(tasks[1].priority, TaskPriority::Medium);
        assert_eq!(tasks[2].priority, TaskPriority::Low);
    }

    #[test]
    fn list_for_profile() {
        let mut cache = TaskCache::new();
        let p1 = ProfileId::new();
        let p2 = ProfileId::new();

        cache.upsert(make_task("Work task", TaskPriority::High, p1));
        cache.upsert(make_task("Personal task", TaskPriority::Medium, p2));

        let p1_tasks = cache.list_for_profile(p1);
        let p2_tasks = cache.list_for_profile(p2);

        assert_eq!(p1_tasks.len(), 1);
        assert_eq!(p2_tasks.len(), 1);
        assert_eq!(p1_tasks[0].title, "Work task");
        assert_eq!(p2_tasks[0].title, "Personal task");
    }

    #[test]
    fn list_open_tasks() {
        let mut cache = TaskCache::new();
        let profile = ProfileId::new();

        cache.upsert(make_task("Open task 1", TaskPriority::High, profile));
        let mut done_task = make_task("Done task", TaskPriority::High, profile);
        done_task.done = true;
        cache.upsert(done_task);
        cache.upsert(make_task("Open task 2", TaskPriority::Medium, profile));

        let open = cache.list_open();
        assert_eq!(open.len(), 2);
        assert!(!open.iter().any(|t| t.done));
    }

    #[test]
    fn load_from_source() {
        let mut cache = TaskCache::new();
        let profile = ProfileId::new();

        let mut t1 = make_task("Local task 1", TaskPriority::Medium, profile);
        t1.source = "local".to_string();
        cache.upsert(t1);

        let mut t2 = make_task("Local task 2", TaskPriority::Medium, profile);
        t2.source = "local".to_string();
        cache.upsert(t2);

        assert_eq!(cache.len(), 2);

        // Replace all local tasks with a new set
        let mut new_task = make_task("Updated local task", TaskPriority::High, profile);
        new_task.source = "local".to_string();
        cache.load_from_source("local", profile, vec![new_task]);

        assert_eq!(cache.len(), 1);
        assert_eq!(cache.list_all()[0].title, "Updated local task");
    }

    #[test]
    fn load_from_multiple_sources() {
        let mut cache = TaskCache::new();
        let profile = ProfileId::new();

        // Local source
        let mut local_task = make_task("Local task", TaskPriority::High, profile);
        local_task.source = "local".to_string();

        // Obsidian source
        let mut obsidian_task = make_task("Obsidian task", TaskPriority::Medium, profile);
        obsidian_task.source = "obsidian".to_string();

        cache.load_from_source("local", profile, vec![local_task]);
        cache.load_from_source("obsidian", profile, vec![obsidian_task]);

        assert_eq!(cache.len(), 2);
        let all = cache.list_all();
        assert!(all
            .iter()
            .any(|t| t.source == "local" && t.title == "Local task"));
        assert!(all
            .iter()
            .any(|t| t.source == "obsidian" && t.title == "Obsidian task"));
    }

    #[test]
    fn get_and_get_mut() {
        let mut cache = TaskCache::new();
        let profile = ProfileId::new();
        let task = make_task("Test task", TaskPriority::Medium, profile);
        let task_id = task.id;

        cache.upsert(task);

        let fetched = cache.get(task_id).unwrap();
        assert_eq!(fetched.title, "Test task");
        assert!(!fetched.done);

        // Modify task
        if let Some(t) = cache.get_mut(task_id) {
            t.done = true;
        }

        let updated = cache.get(task_id).unwrap();
        assert!(updated.done);
    }
}
