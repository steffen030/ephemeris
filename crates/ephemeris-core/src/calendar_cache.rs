use crate::{CalendarEvent, EventId, ProfileId};
use std::collections::HashMap;

/// Calendar cache with cross-source merge and dedup (ADR ephemeris-op3, ephemeris-fy6).
///
/// Aggregates events from all enabled calendar sources (ICS feeds, CalDAV) into
/// one queryable cache, deduping by UID + recurrence-id and tagging with source/profile.
pub struct CalendarCache {
    // Key: (uid, recurrence-id); Value: CalendarEvent
    events_by_uid: HashMap<(String, Option<u64>), CalendarEvent>,
    // All events indexed by profile
    by_profile: HashMap<ProfileId, Vec<EventId>>,
}

impl CalendarCache {
    pub fn new() -> Self {
        CalendarCache {
            events_by_uid: HashMap::new(),
            by_profile: HashMap::new(),
        }
    }

    /// Add or replace an event; uses UID + recurrence-id for dedup.
    pub fn upsert(&mut self, event: CalendarEvent) {
        let key = (event.uid.clone(), Self::parse_recurrence_id(&event));
        let profile_id = event.profile_id;

        // Track profile index
        self.by_profile.entry(profile_id).or_default();

        // If new event, track its ID in profile index
        if !self.events_by_uid.contains_key(&key) {
            if let Some(events) = self.by_profile.get_mut(&profile_id) {
                events.push(event.id);
            }
        }

        self.events_by_uid.insert(key, event);
    }

    /// Load all events from a source (replaces previous events from that source).
    pub fn load_from_source(
        &mut self,
        source: &str,
        profile_id: ProfileId,
        events: Vec<CalendarEvent>,
    ) {
        // Remove old events from this source/profile
        let keys_to_remove: Vec<_> = self
            .events_by_uid
            .iter()
            .filter(|(_, e)| e.source == source && e.profile_id == profile_id)
            .map(|(k, _)| k.clone())
            .collect();
        for key in keys_to_remove {
            self.events_by_uid.remove(&key);
        }

        // Add new events
        for event in events {
            self.upsert(event);
        }
    }

    /// Get all events across all profiles.
    pub fn list_all(&self) -> Vec<CalendarEvent> {
        let mut events: Vec<_> = self.events_by_uid.values().cloned().collect();
        events.sort_by_key(|e| e.start);
        events
    }

    /// Get all events for a profile.
    pub fn list_for_profile(&self, profile_id: ProfileId) -> Vec<CalendarEvent> {
        let all = self.list_all();
        all.into_iter()
            .filter(|e| e.profile_id == profile_id)
            .collect()
    }

    /// Get events in a time range (Unix timestamps).
    pub fn list_in_range(&self, start: u64, end: u64) -> Vec<CalendarEvent> {
        let mut events: Vec<_> = self
            .events_by_uid
            .values()
            .filter(|e| e.start < end && e.end > start)
            .cloned()
            .collect();
        events.sort_by_key(|e| e.start);
        events
    }

    /// Get events for a profile in a time range.
    pub fn list_for_profile_in_range(
        &self,
        profile_id: ProfileId,
        start: u64,
        end: u64,
    ) -> Vec<CalendarEvent> {
        self.list_in_range(start, end)
            .into_iter()
            .filter(|e| e.profile_id == profile_id)
            .collect()
    }

    pub fn len(&self) -> usize {
        self.events_by_uid.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events_by_uid.is_empty()
    }

    /// Extract recurrence-id from uid if present (format: uid@recurrence-id).
    /// This is a simplified model; real CalDAV uses RECURRENCE-ID in the event.
    fn parse_recurrence_id(_event: &CalendarEvent) -> Option<u64> {
        // For now, we don't parse recurrence-id; the dedup is by UID alone.
        // In a full implementation, this would extract recurrence-id from event metadata.
        None
    }
}

impl Default for CalendarCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_event(uid: &str, title: &str, start: u64, profile_id: ProfileId) -> CalendarEvent {
        CalendarEvent::new(uid, start, start + 3600, title, profile_id)
    }

    #[test]
    fn cache_creation() {
        let cache = CalendarCache::new();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn upsert_single_event() {
        let mut cache = CalendarCache::new();
        let profile = ProfileId::new();
        let event = make_event("e1", "Meeting", 1000, profile);

        cache.upsert(event);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn dedup_by_uid() {
        let mut cache = CalendarCache::new();
        let profile = ProfileId::new();
        let e1 = make_event("uid-123", "Team meeting", 1000, profile);
        let mut e2 = make_event("uid-123", "Updated meeting", 1500, profile);
        e2.id = e1.id; // Same event ID

        cache.upsert(e1);
        assert_eq!(cache.len(), 1);

        cache.upsert(e2);
        assert_eq!(cache.len(), 1); // Should still be 1 due to dedup

        let all = cache.list_all();
        assert_eq!(all[0].title, "Updated meeting");
        assert_eq!(all[0].start, 1500);
    }

    #[test]
    fn list_all_events() {
        let mut cache = CalendarCache::new();
        let profile = ProfileId::new();
        cache.upsert(make_event("e1", "Event 1", 1000, profile));
        cache.upsert(make_event("e2", "Event 2", 2000, profile));
        cache.upsert(make_event("e3", "Event 3", 1500, profile));

        let events = cache.list_all();
        assert_eq!(events.len(), 3);
        // Should be sorted by start time
        assert_eq!(events[0].start, 1000);
        assert_eq!(events[1].start, 1500);
        assert_eq!(events[2].start, 2000);
    }

    #[test]
    fn list_for_profile() {
        let mut cache = CalendarCache::new();
        let p1 = ProfileId::new();
        let p2 = ProfileId::new();

        cache.upsert(make_event("e1", "Work meeting", 1000, p1));
        cache.upsert(make_event("e2", "Personal event", 2000, p2));

        let p1_events = cache.list_for_profile(p1);
        let p2_events = cache.list_for_profile(p2);

        assert_eq!(p1_events.len(), 1);
        assert_eq!(p2_events.len(), 1);
        assert_eq!(p1_events[0].title, "Work meeting");
        assert_eq!(p2_events[0].title, "Personal event");
    }

    #[test]
    fn list_in_range() {
        let mut cache = CalendarCache::new();
        let profile = ProfileId::new();

        cache.upsert(make_event("e1", "Event 1", 1000, profile)); // 1000-4600
        cache.upsert(make_event("e2", "Event 2", 2000, profile)); // 2000-5600
        cache.upsert(make_event("e3", "Event 3", 3000, profile)); // 3000-6600

        // Query range [1500, 2500): e1 and e2 overlap, e3 does not
        let events = cache.list_in_range(1500, 2500);
        assert_eq!(events.len(), 2);
        let titles: Vec<_> = events.iter().map(|e| &e.title[..]).collect();
        assert!(titles.contains(&"Event 1"));
        assert!(titles.contains(&"Event 2"));
    }

    #[test]
    fn load_from_source() {
        let mut cache = CalendarCache::new();
        let profile = ProfileId::new();

        let mut e1 = make_event("e1", "ICS event", 1000, profile);
        e1.source = "ics".to_string();
        cache.upsert(e1);

        let mut e2 = make_event("e2", "ICS event 2", 2000, profile);
        e2.source = "ics".to_string();
        cache.upsert(e2);

        assert_eq!(cache.len(), 2);

        // Replace all ICS events
        let mut new_event = make_event("e3", "New ICS event", 3000, profile);
        new_event.source = "ics".to_string();
        cache.load_from_source("ics", profile, vec![new_event]);

        assert_eq!(cache.len(), 1);
        assert_eq!(cache.list_all()[0].title, "New ICS event");
    }

    #[test]
    fn load_from_multiple_sources() {
        let mut cache = CalendarCache::new();
        let profile = ProfileId::new();

        // ICS source
        let mut ics_event = make_event("uid1", "ICS event", 1000, profile);
        ics_event.source = "ics".to_string();

        // CalDAV source
        let mut caldav_event = make_event("uid2", "CalDAV event", 2000, profile);
        caldav_event.source = "caldav".to_string();

        cache.load_from_source("ics", profile, vec![ics_event]);
        cache.load_from_source("caldav", profile, vec![caldav_event]);

        assert_eq!(cache.len(), 2);
        let all = cache.list_all();
        assert!(all
            .iter()
            .any(|e| e.source == "ics" && e.title == "ICS event"));
        assert!(all
            .iter()
            .any(|e| e.source == "caldav" && e.title == "CalDAV event"));
    }
}
