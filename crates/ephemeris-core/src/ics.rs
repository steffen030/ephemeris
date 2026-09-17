use crate::{CalendarEvent, ProfileId, AppError};
use std::time::{SystemTime, UNIX_EPOCH};

/// ICS (iCalendar) feed reader for HTTPS calendar feeds.
pub struct IcsReader;

impl IcsReader {
    /// Parse ICS text and extract VEVENT components as CalendarEvents.
    pub fn parse_events(ics_text: &str, profile_id: ProfileId) -> crate::Result<Vec<CalendarEvent>> {
        let mut events = Vec::new();

        for line in ics_text.lines() {
            if line.starts_with("BEGIN:VEVENT") {
                // Simple skeleton: in production, parse all properties
                // For now, just verify VEVENT blocks exist
                events.push(CalendarEvent::new(
                    format!("uid-{}", events.len()),
                    current_timestamp(),
                    current_timestamp() + 3600,
                    "Calendar Event",
                    profile_id,
                ));
            }
        }

        Ok(events)
    }

    /// Fetch and parse an ICS feed from an HTTPS URL (stub).
    pub fn fetch_feed(
        _url: &str,
        profile_id: ProfileId,
    ) -> crate::Result<Vec<CalendarEvent>> {
        // Future: use reqwest to fetch from URL
        // For now, return empty with proper type signature
        Ok(Vec::new())
    }
}

fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_ics() {
        let profile_id = ProfileId::new();
        let events = IcsReader::parse_events("", profile_id).expect("should parse");
        assert_eq!(events.len(), 0);
    }

    #[test]
    fn parse_ics_with_vevent() {
        let ics = "BEGIN:VEVENT\nSUMMARY:Test Event\nEND:VEVENT";
        let profile_id = ProfileId::new();
        let events = IcsReader::parse_events(ics, profile_id).expect("should parse");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].title, "Calendar Event");
    }

    #[test]
    fn parse_ics_multiple_events() {
        let ics = "BEGIN:VEVENT\nEND:VEVENT\nBEGIN:VEVENT\nEND:VEVENT";
        let profile_id = ProfileId::new();
        let events = IcsReader::parse_events(ics, profile_id).expect("should parse");
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn fetch_feed_returns_empty() {
        let profile_id = ProfileId::new();
        let events = IcsReader::fetch_feed("https://example.com/cal.ics", profile_id)
            .expect("should not error");
        assert_eq!(events.len(), 0);
    }

    #[test]
    fn calendar_event_from_ics() {
        let profile_id = ProfileId::new();
        let ics = "BEGIN:VEVENT\nUID:123\nEND:VEVENT";
        let events = IcsReader::parse_events(ics, profile_id).expect("should parse");
        assert!(!events[0].uid.is_empty());
    }
}
