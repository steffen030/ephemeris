use crate::{ProfileId, Task, TaskId};

/// Capability flags for a task provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProviderCapabilities {
    pub create: bool,
    pub update: bool,
    pub complete: bool,
    pub delete: bool,
}

impl ProviderCapabilities {
    pub const FULL_WRITE: Self = Self {
        create: true,
        update: true,
        complete: true,
        delete: true,
    };
    pub const READ_ONLY: Self = Self {
        create: false,
        update: false,
        complete: false,
        delete: false,
    };
}

/// Task provider trait per ADR ephemeris-e8x.
pub trait TaskProvider {
    fn capabilities(&self) -> ProviderCapabilities;
    fn list(&self, profile_id: ProfileId) -> crate::Result<Vec<Task>>;
    fn create(&self, task: &Task) -> crate::Result<()>;
    fn update(&self, task: &Task) -> crate::Result<()>;
    fn complete(&self, id: TaskId) -> crate::Result<()>;
    fn delete(&self, id: TaskId) -> crate::Result<bool>;
}

/// Local task provider backed by SQLite (ADR ephemeris-e8x, ephemeris-pir).
#[cfg(feature = "sqlite")]
pub struct LocalTaskProvider {
    store: crate::sqlite::SqliteStore,
}

#[cfg(feature = "sqlite")]
impl Default for LocalTaskProvider {
    fn default() -> Self {
        LocalTaskProvider::open_in_memory().expect("in-memory store must open")
    }
}

#[cfg(feature = "sqlite")]
impl LocalTaskProvider {
    pub fn new(store: crate::sqlite::SqliteStore) -> Self {
        LocalTaskProvider { store }
    }

    pub fn open_in_memory() -> crate::Result<Self> {
        Ok(LocalTaskProvider {
            store: crate::sqlite::SqliteStore::open_in_memory()?,
        })
    }

    pub fn open(path: impl AsRef<std::path::Path>) -> crate::Result<Self> {
        Ok(LocalTaskProvider {
            store: crate::sqlite::SqliteStore::open(path)?,
        })
    }
}

#[cfg(feature = "sqlite")]
impl TaskProvider for LocalTaskProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::FULL_WRITE
    }

    fn list(&self, profile_id: ProfileId) -> crate::Result<Vec<Task>> {
        self.store.list_tasks_for_profile(profile_id)
    }

    fn create(&self, task: &Task) -> crate::Result<()> {
        self.store.upsert_task(task)
    }

    fn update(&self, task: &Task) -> crate::Result<()> {
        self.store.upsert_task(task)
    }

    fn complete(&self, id: TaskId) -> crate::Result<()> {
        match self.store.get_task(id)? {
            None => Err(crate::AppError::NotFound(format!("task {id:?}"))),
            Some(mut t) => {
                t.done = true;
                self.store.upsert_task(&t)
            }
        }
    }

    fn delete(&self, id: TaskId) -> crate::Result<bool> {
        self.store.delete_task(id)
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use crate::model::{Profile, ProfileId, Task, TaskId, TaskPriority};

    fn setup() -> (LocalTaskProvider, ProfileId) {
        let provider = LocalTaskProvider::open_in_memory().expect("in-memory store");
        let profile = Profile::new("Test");
        provider
            .store
            .upsert_profile(&profile)
            .expect("upsert profile");
        (provider, profile.id)
    }

    #[test]
    fn capabilities_report_full_write() {
        let (provider, _) = setup();
        assert_eq!(provider.capabilities(), ProviderCapabilities::FULL_WRITE);
    }

    #[test]
    fn create_and_list() {
        let (provider, pid) = setup();
        let task = Task::new("Buy milk", pid);
        provider.create(&task).unwrap();

        let tasks = provider.list(pid).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "Buy milk");
        assert!(!tasks[0].done);
    }

    #[test]
    fn update_task() {
        let (provider, pid) = setup();
        let mut task = Task::new("Draft report", pid);
        provider.create(&task).unwrap();

        task.priority = TaskPriority::High;
        task.tags = vec!["work".to_string()];
        provider.update(&task).unwrap();

        let tasks = provider.list(pid).unwrap();
        assert_eq!(tasks[0].priority, TaskPriority::High);
        assert_eq!(tasks[0].tags, vec!["work"]);
    }

    #[test]
    fn complete_marks_done() {
        let (provider, pid) = setup();
        let task = Task::new("Exercise", pid);
        provider.create(&task).unwrap();

        provider.complete(task.id).unwrap();

        let tasks = provider.list(pid).unwrap();
        assert!(tasks[0].done);
    }

    #[test]
    fn complete_missing_task_returns_err() {
        let (provider, _) = setup();
        let missing = TaskId::new();
        assert!(provider.complete(missing).is_err());
    }

    #[test]
    fn delete_task() {
        let (provider, pid) = setup();
        let task = Task::new("Temporary", pid);
        provider.create(&task).unwrap();

        assert!(provider.delete(task.id).unwrap());
        assert!(
            !provider.delete(task.id).unwrap(),
            "second delete returns false"
        );
        assert!(provider.list(pid).unwrap().is_empty());
    }

    #[test]
    fn list_filters_by_profile() {
        let provider = LocalTaskProvider::open_in_memory().unwrap();
        let p1 = Profile::new("Profile 1");
        let p2 = Profile::new("Profile 2");
        provider.store.upsert_profile(&p1).unwrap();
        provider.store.upsert_profile(&p2).unwrap();

        provider.create(&Task::new("P1 task", p1.id)).unwrap();
        provider.create(&Task::new("P2 task", p2.id)).unwrap();

        let p1_tasks = provider.list(p1.id).unwrap();
        let p2_tasks = provider.list(p2.id).unwrap();
        assert_eq!(p1_tasks.len(), 1);
        assert_eq!(p1_tasks[0].title, "P1 task");
        assert_eq!(p2_tasks.len(), 1);
        assert_eq!(p2_tasks[0].title, "P2 task");
    }

    #[test]
    fn default_impl_works() {
        let provider = LocalTaskProvider::default();
        assert_eq!(provider.capabilities(), ProviderCapabilities::FULL_WRITE);
    }
}
