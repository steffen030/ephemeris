use crate::Task;

/// Task provider trait for aggregating tasks from multiple sources.
pub trait TaskProvider {
    fn list_tasks(&self) -> crate::Result<Vec<Task>>;
}

/// Local task provider backed by SQLite.
pub struct LocalTaskProvider;

impl LocalTaskProvider {
    pub fn new() -> Self {
        LocalTaskProvider
    }
}

impl TaskProvider for LocalTaskProvider {
    fn list_tasks(&self) -> crate::Result<Vec<Task>> {
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_provider_list_tasks() {
        let provider = LocalTaskProvider::new();
        let tasks = provider.list_tasks().expect("should list");
        assert_eq!(tasks.len(), 0);
    }
}
