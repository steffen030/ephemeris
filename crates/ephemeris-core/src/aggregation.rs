//! Holistic cross-profile aggregation service (ADR ephemeris-uv4).
//!
//! Provides read service that unions calendar events and tasks across all
//! profiles while allowing single-profile filtering. The "holistic overview"
//! pseudo-profile aggregates all data; filtering by ProfileId returns the subset.

#[cfg(feature = "sqlite")]
use crate::sqlite::SqliteStore;
use crate::{CalendarEvent, ProfileId, Task};

/// Cross-profile aggregation service for tasks and calendar events.
#[cfg(feature = "sqlite")]
pub struct AggregationService {
    store: SqliteStore,
}

#[cfg(feature = "sqlite")]
impl AggregationService {
    pub fn new(store: SqliteStore) -> Self {
        AggregationService { store }
    }

    /// Get all tasks across all profiles (holistic union).
    pub fn list_all_tasks(&self) -> crate::Result<Vec<Task>> {
        self.store.list_all_tasks()
    }

    /// Get all tasks for a specific profile.
    pub fn list_tasks_for_profile(&self, profile_id: ProfileId) -> crate::Result<Vec<Task>> {
        self.store.list_tasks_for_profile(profile_id)
    }

    /// Get all calendar events across all profiles (holistic union).
    pub fn list_all_events(&self) -> crate::Result<Vec<CalendarEvent>> {
        self.store.list_all_events()
    }

    /// Get all calendar events for a specific profile.
    pub fn list_events_for_profile(
        &self,
        profile_id: ProfileId,
    ) -> crate::Result<Vec<CalendarEvent>> {
        self.store.list_events_for_profile(profile_id)
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use crate::model::{Profile, Task};

    #[test]
    fn list_all_tasks_empty() {
        let store = SqliteStore::open_in_memory().unwrap();
        let service = AggregationService::new(store);

        let tasks = service.list_all_tasks().unwrap();
        assert_eq!(tasks.len(), 0);
    }

    #[test]
    fn list_all_tasks_single_profile() {
        let store = SqliteStore::open_in_memory().unwrap();
        let profile = Profile::new("Work");
        store.upsert_profile(&profile).unwrap();

        let task = Task::new("Review PR", profile.id);
        store.upsert_task(&task).unwrap();

        let service = AggregationService::new(store);
        let tasks = service.list_all_tasks().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "Review PR");
    }

    #[test]
    fn list_all_tasks_multiple_profiles() {
        let store = SqliteStore::open_in_memory().unwrap();
        let p1 = Profile::new("Work");
        let p2 = Profile::new("Personal");
        store.upsert_profile(&p1).unwrap();
        store.upsert_profile(&p2).unwrap();

        let t1 = Task::new("Work task", p1.id);
        let t2 = Task::new("Personal task", p2.id);
        store.upsert_task(&t1).unwrap();
        store.upsert_task(&t2).unwrap();

        let service = AggregationService::new(store);
        let tasks = service.list_all_tasks().unwrap();
        assert_eq!(tasks.len(), 2);
    }

    #[test]
    fn list_tasks_for_profile_filters_correctly() {
        let store = SqliteStore::open_in_memory().unwrap();
        let p1 = Profile::new("Work");
        let p2 = Profile::new("Personal");
        store.upsert_profile(&p1).unwrap();
        store.upsert_profile(&p2).unwrap();

        let t1 = Task::new("Work task 1", p1.id);
        let t2 = Task::new("Work task 2", p1.id);
        let t3 = Task::new("Personal task", p2.id);
        store.upsert_task(&t1).unwrap();
        store.upsert_task(&t2).unwrap();
        store.upsert_task(&t3).unwrap();

        let service = AggregationService::new(store);
        let work_tasks = service.list_tasks_for_profile(p1.id).unwrap();
        let personal_tasks = service.list_tasks_for_profile(p2.id).unwrap();

        assert_eq!(work_tasks.len(), 2);
        assert_eq!(personal_tasks.len(), 1);
        assert_eq!(work_tasks[0].profile_id, p1.id);
        assert_eq!(personal_tasks[0].profile_id, p2.id);
    }

    #[test]
    fn list_all_events_empty() {
        let store = SqliteStore::open_in_memory().unwrap();
        let service = AggregationService::new(store);

        let events = service.list_all_events().unwrap();
        assert_eq!(events.len(), 0);
    }

    #[test]
    fn list_all_events_single_profile() {
        let store = SqliteStore::open_in_memory().unwrap();
        let profile = Profile::new("Work");
        store.upsert_profile(&profile).unwrap();

        let event = CalendarEvent::new("uid-1", 1000, 2000, "Team Meeting", profile.id);
        store.upsert_event(&event).unwrap();

        let service = AggregationService::new(store);
        let events = service.list_all_events().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].title, "Team Meeting");
    }

    #[test]
    fn list_all_events_multiple_profiles() {
        let store = SqliteStore::open_in_memory().unwrap();
        let p1 = Profile::new("Work");
        let p2 = Profile::new("Personal");
        store.upsert_profile(&p1).unwrap();
        store.upsert_profile(&p2).unwrap();

        let e1 = CalendarEvent::new("uid-1", 1000, 2000, "Work meeting", p1.id);
        let e2 = CalendarEvent::new("uid-2", 3000, 4000, "Personal event", p2.id);
        store.upsert_event(&e1).unwrap();
        store.upsert_event(&e2).unwrap();

        let service = AggregationService::new(store);
        let events = service.list_all_events().unwrap();
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn list_events_for_profile_filters_correctly() {
        let store = SqliteStore::open_in_memory().unwrap();
        let p1 = Profile::new("Work");
        let p2 = Profile::new("Personal");
        store.upsert_profile(&p1).unwrap();
        store.upsert_profile(&p2).unwrap();

        let e1 = CalendarEvent::new("uid-1", 1000, 2000, "Work meeting", p1.id);
        let e2 = CalendarEvent::new("uid-2", 3000, 4000, "Another work event", p1.id);
        let e3 = CalendarEvent::new("uid-3", 5000, 6000, "Personal event", p2.id);
        store.upsert_event(&e1).unwrap();
        store.upsert_event(&e2).unwrap();
        store.upsert_event(&e3).unwrap();

        let service = AggregationService::new(store);
        let work_events = service.list_events_for_profile(p1.id).unwrap();
        let personal_events = service.list_events_for_profile(p2.id).unwrap();

        assert_eq!(work_events.len(), 2);
        assert_eq!(personal_events.len(), 1);
        assert_eq!(work_events[0].profile_id, p1.id);
        assert_eq!(personal_events[0].profile_id, p2.id);
    }
}
