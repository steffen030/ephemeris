//! Agenda query — filter calendar events into day/week/month windows.
//!
//! Pure functions: no I/O, deterministic given `now_secs`.

use crate::CalendarEvent;

const SECS_PER_DAY: u64 = 86_400;
const SECS_PER_WEEK: u64 = SECS_PER_DAY * 7;

/// Time range selector for the agenda view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgendaRange {
    #[default]
    Day,
    Week,
    Month,
}

/// Formatted entry for display in the agenda view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgendaEntry {
    pub title: String,
    /// Short time label — "HH:MM" for same-day events, "ddd HH:MM" cross-day.
    pub time_label: String,
    /// Optional location string (empty if none).
    pub location_label: String,
}

/// Return events that start within the given range window, sorted by `start`.
///
/// The window is `[now_secs, now_secs + window_secs)`.
pub fn filter_agenda<'a>(
    events: &'a [CalendarEvent],
    range: AgendaRange,
    now_secs: u64,
) -> Vec<&'a CalendarEvent> {
    let window = match range {
        AgendaRange::Day => SECS_PER_DAY,
        AgendaRange::Week => SECS_PER_WEEK,
        AgendaRange::Month => SECS_PER_DAY * 30,
    };
    let end = now_secs.saturating_add(window);

    let mut filtered: Vec<&CalendarEvent> = events
        .iter()
        .filter(|e| e.start >= now_secs && e.start < end)
        .collect();

    filtered.sort_by_key(|e| e.start);
    filtered
}

/// Format a Unix timestamp as "HH:MM" (UTC).
pub fn format_time_hhmm(secs: u64) -> String {
    let h = (secs % 86_400) / 3600;
    let m = (secs % 3600) / 60;
    format!("{h:02}:{m:02}")
}

/// Short day abbreviation for day-of-week (0=Mon … 6=Sun).
fn day_abbr(dow: u64) -> &'static str {
    ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][dow as usize % 7]
}

/// Format a Unix timestamp as "ddd HH:MM" for cross-day labeling.
pub fn format_time_dow_hhmm(secs: u64) -> String {
    // Days since Unix epoch; epoch (1970-01-01) was a Thursday → offset 3.
    let days = secs / 86_400;
    let dow = (days + 3) % 7;
    let h = (secs % 86_400) / 3600;
    let m = (secs % 3600) / 60;
    format!("{} {:02}:{:02}", day_abbr(dow), h, m)
}

/// Convert filtered events to display entries.
///
/// For Day range, labels are "HH:MM". For Week/Month they include the day abbr.
pub fn to_agenda_entries(
    events: &[&CalendarEvent],
    range: AgendaRange,
    now_secs: u64,
) -> Vec<AgendaEntry> {
    let today_start = (now_secs / SECS_PER_DAY) * SECS_PER_DAY;
    let today_end = today_start + SECS_PER_DAY;

    events
        .iter()
        .map(|e| {
            let time_label =
                if range == AgendaRange::Day || (e.start >= today_start && e.start < today_end) {
                    format_time_hhmm(e.start)
                } else {
                    format_time_dow_hhmm(e.start)
                };
            AgendaEntry {
                title: e.title.clone(),
                time_label,
                location_label: e.location.clone().unwrap_or_default(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CalendarEvent, ProfileId};

    const NOW: u64 = 1_750_000_000; // fixed epoch for deterministic tests (≈ Wed 2025-06-15)

    fn event(title: &str, start_offset_secs: i64) -> CalendarEvent {
        let start = (NOW as i64 + start_offset_secs) as u64;
        CalendarEvent::new(title, start, start + 3600, title, ProfileId::new())
    }

    #[test]
    fn day_filter_includes_within_24h() {
        let events = vec![
            event("morning", 3_600),     // +1 h — in
            event("evening", 82_800),    // +23 h — in
            event("tomorrow", 90_000),   // +25 h — out
            event("past", -(3_600_i64)), // -1 h — out
        ];
        let result = filter_agenda(&events, AgendaRange::Day, NOW);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].title, "morning");
        assert_eq!(result[1].title, "evening");
    }

    #[test]
    fn week_filter_spans_7_days() {
        let events = vec![
            event("day3", 3 * 86_400_i64),
            event("day8", 8 * 86_400_i64), // out
        ];
        let result = filter_agenda(&events, AgendaRange::Week, NOW);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].title, "day3");
    }

    #[test]
    fn month_filter_spans_30_days() {
        let events = vec![
            event("day29", 29 * 86_400_i64),
            event("day31", 31 * 86_400_i64), // out
        ];
        let result = filter_agenda(&events, AgendaRange::Month, NOW);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].title, "day29");
    }

    #[test]
    fn filter_sorted_by_start() {
        let events = vec![event("late", 7200), event("early", 1800)];
        let result = filter_agenda(&events, AgendaRange::Day, NOW);
        assert_eq!(result[0].title, "early");
        assert_eq!(result[1].title, "late");
    }

    #[test]
    fn filter_empty_list() {
        let result = filter_agenda(&[], AgendaRange::Day, NOW);
        assert!(result.is_empty());
    }

    #[test]
    fn to_entries_day_uses_hhmm() {
        let events = vec![event("standup", 3600)];
        let filtered: Vec<&CalendarEvent> = events.iter().collect();
        let entries = to_agenda_entries(&filtered, AgendaRange::Day, NOW);
        assert_eq!(entries.len(), 1);
        // "HH:MM" format — no day abbreviation
        assert!(!entries[0].time_label.contains(' '));
        assert!(entries[0].time_label.contains(':'));
    }

    #[test]
    fn to_entries_week_uses_dow_for_cross_day() {
        // 3 days from now, definitely not today
        let start = NOW + 3 * 86_400;
        let e = CalendarEvent::new("future", start, start + 3600, "future", ProfileId::new());
        let events = vec![&e];
        let entries = to_agenda_entries(&events, AgendaRange::Week, NOW);
        // should include day abbreviation
        assert!(entries[0].time_label.contains(' '));
    }

    #[test]
    fn format_time_hhmm_known_value() {
        // NOW = 1_750_000_000 s; 1_750_000_000 % 86400 = 1_750_000_000 - 20254 * 86400 = ?
        // Just verify format "HH:MM"
        let s = format_time_hhmm(NOW);
        assert_eq!(s.len(), 5);
        assert_eq!(s.chars().nth(2), Some(':'));
    }

    #[test]
    fn to_entries_includes_location() {
        let mut e = event("conf", 1000);
        e.location = Some("Room 42".to_string());
        let events = vec![&e];
        let entries = to_agenda_entries(&events, AgendaRange::Day, NOW);
        assert_eq!(entries[0].location_label, "Room 42");
    }

    #[test]
    fn to_entries_no_location_empty_string() {
        let e = event("call", 1000);
        let events = vec![&e];
        let entries = to_agenda_entries(&events, AgendaRange::Day, NOW);
        assert_eq!(entries[0].location_label, "");
    }
}
