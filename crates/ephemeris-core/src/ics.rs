use crate::{AppError, CalendarEvent, ProfileId};
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc, Weekday};
use icalendar::{Calendar, CalendarComponent, CalendarDateTime, Component, DatePerhapsTime, EventLike};
use std::str::FromStr;

/// ICS (iCalendar) feed reader — parses HTTPS calendar feeds (RFC 5545).
pub struct IcsReader;

impl IcsReader {
    /// Parse ICS text and return all non-recurring events plus recurring events
    /// expanded into [`window_start`, `window_end`] (Unix seconds, inclusive).
    ///
    /// Pass `window_end = u64::MAX / 2` and `window_start = 0` to get everything
    /// (but capped at 1000 recurrence instances to guard against unbounded RRULE).
    pub fn parse_events(
        ics_text: &str,
        profile_id: ProfileId,
        window_start: u64,
        window_end: u64,
    ) -> crate::Result<Vec<CalendarEvent>> {
        let calendar: Calendar = ics_text
            .parse()
            .map_err(|e| AppError::InvalidInput(format!("ICS parse failed: {e:?}")))?;

        let mut events = Vec::new();

        for component in &calendar.components {
            let CalendarComponent::Event(ev) = component else {
                continue;
            };

            let uid = ev.get_uid().unwrap_or("").to_string();
            let title = ev.get_summary().unwrap_or("").to_string();
            let location = ev.get_location().map(|s| s.to_string());

            let Some(start_ts) = ev.get_start().and_then(date_to_unix) else {
                continue;
            };
            let duration_secs = ev
                .get_end()
                .and_then(date_to_unix)
                .map(|end| end.saturating_sub(start_ts))
                .unwrap_or(3600);

            if let Some(rrule_val) = ev.property_value("RRULE") {
                let occurrences =
                    expand_rrule(start_ts, duration_secs, rrule_val, window_start, window_end);
                for (occ_start, occ_end) in occurrences {
                    let mut cal_ev = CalendarEvent::new(
                        uid.clone(),
                        occ_start,
                        occ_end,
                        title.clone(),
                        profile_id,
                    );
                    cal_ev.location = location.clone();
                    events.push(cal_ev);
                }
            } else {
                let end_ts = start_ts + duration_secs;
                if end_ts >= window_start && start_ts <= window_end {
                    let mut cal_ev = CalendarEvent::new(uid, start_ts, end_ts, title, profile_id);
                    cal_ev.location = location;
                    events.push(cal_ev);
                }
            }
        }

        Ok(events)
    }

    /// Fetch an ICS feed from an HTTPS URL and parse events in the given window.
    #[cfg(feature = "ics")]
    pub fn fetch_feed(
        url: &str,
        profile_id: ProfileId,
        window_start: u64,
        window_end: u64,
    ) -> crate::Result<Vec<CalendarEvent>> {
        let body = reqwest::blocking::get(url)
            .map_err(|e| AppError::Backend(format!("ICS fetch failed: {e}")))?
            .text()
            .map_err(|e| AppError::Backend(format!("ICS body decode failed: {e}")))?;
        Self::parse_events(&body, profile_id, window_start, window_end)
    }

    /// Stub used when the `ics` feature is not enabled.
    #[cfg(not(feature = "ics"))]
    pub fn fetch_feed(
        _url: &str,
        _profile_id: ProfileId,
        _window_start: u64,
        _window_end: u64,
    ) -> crate::Result<Vec<CalendarEvent>> {
        Err(AppError::InvalidInput(
            "HTTPS ICS fetch requires the `ics` feature".into(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Date/time helpers
// ---------------------------------------------------------------------------

fn date_to_unix(dpt: DatePerhapsTime) -> Option<u64> {
    let ts: i64 = match dpt {
        DatePerhapsTime::Date(d) => NaiveDate::from_ymd_opt(d.year(), d.month(), d.day())?
            .and_hms_opt(0, 0, 0)?
            .and_utc()
            .timestamp(),
        DatePerhapsTime::DateTime(cdt) => match cdt {
            CalendarDateTime::Floating(ndt) => ndt.and_utc().timestamp(),
            CalendarDateTime::Utc(dt) => dt.timestamp(),
            CalendarDateTime::WithTimezone { date_time, tzid } => {
                tz_naive_to_utc(&date_time, &tzid)?.timestamp()
            }
        },
    };
    u64::try_from(ts).ok()
}

fn tz_naive_to_utc(naive: &NaiveDateTime, tzid: &str) -> Option<DateTime<Utc>> {
    let tz = chrono_tz::Tz::from_str(tzid).ok()?;
    tz.from_local_datetime(naive)
        .single()
        .map(|dt| dt.with_timezone(&Utc))
}

// ---------------------------------------------------------------------------
// RRULE expansion (RFC 5545 subset: DAILY/WEEKLY/MONTHLY/YEARLY + BYDAY +
// COUNT + UNTIL + INTERVAL)
// ---------------------------------------------------------------------------

const MAX_RECURRENCE: usize = 1000;

struct RRuleSpec {
    freq: Freq,
    interval: u32,
    count: Option<u32>,
    until: Option<u64>,
    by_day: Vec<Weekday>,
}

#[derive(Clone, Copy)]
enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

fn parse_rrule(rrule: &str) -> Option<RRuleSpec> {
    let mut freq = None;
    let mut interval = 1u32;
    let mut count = None;
    let mut until = None;
    let mut by_day = Vec::new();

    for part in rrule.split(';') {
        let (key, val) = part.split_once('=')?;
        match key.trim() {
            "FREQ" => {
                freq = Some(match val.trim() {
                    "DAILY" => Freq::Daily,
                    "WEEKLY" => Freq::Weekly,
                    "MONTHLY" => Freq::Monthly,
                    "YEARLY" => Freq::Yearly,
                    _ => return None,
                });
            }
            "INTERVAL" => {
                interval = val.trim().parse().ok()?;
            }
            "COUNT" => {
                count = Some(val.trim().parse::<u32>().ok()?);
            }
            "UNTIL" => {
                until = Some(parse_until(val.trim()));
            }
            "BYDAY" => {
                for day in val.split(',') {
                    if let Some(wd) = parse_weekday(day.trim()) {
                        by_day.push(wd);
                    }
                }
            }
            _ => {}
        }
    }

    Some(RRuleSpec {
        freq: freq?,
        interval,
        count,
        until,
        by_day,
    })
}

fn parse_until(s: &str) -> u64 {
    let s = s.trim_end_matches('Z');
    if s.len() >= 15 {
        NaiveDateTime::parse_from_str(&s[..15], "%Y%m%dT%H%M%S")
            .map(|dt| dt.and_utc().timestamp().max(0) as u64)
            .unwrap_or(0)
    } else if s.len() >= 8 {
        NaiveDate::parse_from_str(&s[..8], "%Y%m%d")
            .ok()
            .and_then(|d| d.and_hms_opt(23, 59, 59))
            .map(|dt| dt.and_utc().timestamp().max(0) as u64)
            .unwrap_or(0)
    } else {
        0
    }
}

fn parse_weekday(s: &str) -> Option<Weekday> {
    let s = s.trim_start_matches(|c: char| c.is_ascii_digit() || c == '+' || c == '-');
    match s {
        "MO" => Some(Weekday::Mon),
        "TU" => Some(Weekday::Tue),
        "WE" => Some(Weekday::Wed),
        "TH" => Some(Weekday::Thu),
        "FR" => Some(Weekday::Fri),
        "SA" => Some(Weekday::Sat),
        "SU" => Some(Weekday::Sun),
        _ => None,
    }
}

fn expand_rrule(
    dtstart: u64,
    duration_secs: u64,
    rrule: &str,
    window_start: u64,
    window_end: u64,
) -> Vec<(u64, u64)> {
    let Some(spec) = parse_rrule(rrule) else {
        return Vec::new();
    };

    let Some(start_dt) = DateTime::from_timestamp(dtstart as i64, 0) else {
        return Vec::new();
    };

    let mut results = Vec::new();
    let mut current = start_dt;
    let mut generated = 0usize;
    let mut count_left = spec.count.unwrap_or(u32::MAX);

    loop {
        if generated >= MAX_RECURRENCE || count_left == 0 {
            break;
        }

        let ts = current.timestamp().max(0) as u64;

        if let Some(until) = spec.until {
            if ts > until {
                break;
            }
        }

        if ts > window_end {
            break;
        }

        match spec.freq {
            Freq::Weekly if !spec.by_day.is_empty() => {
                let week_start = week_monday(current);
                for &wd in &spec.by_day {
                    if generated >= MAX_RECURRENCE || count_left == 0 {
                        break;
                    }
                    let occ = week_start + Duration::days(weekday_offset(wd));
                    if occ < start_dt {
                        continue;
                    }
                    let occ_ts = occ.timestamp().max(0) as u64;
                    if let Some(until) = spec.until {
                        if occ_ts > until {
                            continue;
                        }
                    }
                    if occ_ts > window_end {
                        continue;
                    }
                    let occ_end = occ_ts + duration_secs;
                    if occ_end >= window_start {
                        results.push((occ_ts, occ_end));
                    }
                    count_left = count_left.saturating_sub(1);
                    generated += 1;
                }
                current = week_monday(current) + Duration::weeks(spec.interval as i64);
            }
            _ => {
                let end_ts = ts + duration_secs;
                if end_ts >= window_start {
                    results.push((ts, end_ts));
                }
                count_left = count_left.saturating_sub(1);
                generated += 1;
                current = advance(current, spec.freq, spec.interval);
            }
        }
    }

    results
}

fn advance(dt: DateTime<Utc>, freq: Freq, interval: u32) -> DateTime<Utc> {
    match freq {
        Freq::Daily => dt + Duration::days(interval as i64),
        Freq::Weekly => dt + Duration::weeks(interval as i64),
        Freq::Monthly => {
            let mut month = dt.month() as i32 + interval as i32;
            let mut year = dt.year();
            while month > 12 {
                month -= 12;
                year += 1;
            }
            let day = dt.day().min(days_in_month(year, month as u32));
            NaiveDate::from_ymd_opt(year, month as u32, day)
                .and_then(|d| d.and_hms_opt(dt.hour(), dt.minute(), dt.second()))
                .map(|ndt| ndt.and_utc())
                .unwrap_or(dt)
        }
        Freq::Yearly => {
            let year = dt.year() + interval as i32;
            let day = dt.day().min(days_in_month(year, dt.month()));
            NaiveDate::from_ymd_opt(year, dt.month(), day)
                .and_then(|d| d.and_hms_opt(dt.hour(), dt.minute(), dt.second()))
                .map(|ndt| ndt.and_utc())
                .unwrap_or(dt)
        }
    }
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (next_y, next_m) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    NaiveDate::from_ymd_opt(next_y, next_m, 1)
        .and_then(|d| d.pred_opt())
        .map(|d| d.day())
        .unwrap_or(30)
}

fn week_monday(dt: DateTime<Utc>) -> DateTime<Utc> {
    let offset = dt.weekday().num_days_from_monday() as i64;
    dt - Duration::days(offset)
}

fn weekday_offset(wd: Weekday) -> i64 {
    wd.num_days_from_monday() as i64
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: (u64, u64) = (0, u64::MAX / 2);

    fn pid() -> ProfileId {
        ProfileId::new()
    }

    #[test]
    fn parse_empty_ics() {
        let events = IcsReader::parse_events("", pid(), ALL.0, ALL.1).unwrap();
        assert_eq!(events.len(), 0);
    }

    #[test]
    fn parse_single_vevent() {
        let ics = "BEGIN:VCALENDAR\n\
                   VERSION:2.0\n\
                   BEGIN:VEVENT\n\
                   UID:test-uid-1\n\
                   SUMMARY:Team Meeting\n\
                   DTSTART:20260101T100000Z\n\
                   DTEND:20260101T110000Z\n\
                   END:VEVENT\n\
                   END:VCALENDAR";
        let events = IcsReader::parse_events(ics, pid(), ALL.0, ALL.1).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uid, "test-uid-1");
        assert_eq!(events[0].title, "Team Meeting");
        assert_eq!(events[0].end - events[0].start, 3600);
    }

    #[test]
    fn parse_location() {
        let ics = "BEGIN:VCALENDAR\n\
                   VERSION:2.0\n\
                   BEGIN:VEVENT\n\
                   UID:loc-uid\n\
                   SUMMARY:Conf\n\
                   DTSTART:20260201T090000Z\n\
                   DTEND:20260201T100000Z\n\
                   LOCATION:Room 42\n\
                   END:VEVENT\n\
                   END:VCALENDAR";
        let events = IcsReader::parse_events(ics, pid(), ALL.0, ALL.1).unwrap();
        assert_eq!(events[0].location.as_deref(), Some("Room 42"));
    }

    #[test]
    fn parse_multiple_events() {
        let ics = "BEGIN:VCALENDAR\n\
                   VERSION:2.0\n\
                   BEGIN:VEVENT\n\
                   UID:e1\n\
                   SUMMARY:A\n\
                   DTSTART:20260101T100000Z\n\
                   DTEND:20260101T110000Z\n\
                   END:VEVENT\n\
                   BEGIN:VEVENT\n\
                   UID:e2\n\
                   SUMMARY:B\n\
                   DTSTART:20260201T100000Z\n\
                   DTEND:20260201T110000Z\n\
                   END:VEVENT\n\
                   END:VCALENDAR";
        let events = IcsReader::parse_events(ics, pid(), ALL.0, ALL.1).unwrap();
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn window_filters_events() {
        let ics = "BEGIN:VCALENDAR\n\
                   VERSION:2.0\n\
                   BEGIN:VEVENT\n\
                   UID:past\n\
                   SUMMARY:Past\n\
                   DTSTART:20200101T000000Z\n\
                   DTEND:20200101T010000Z\n\
                   END:VEVENT\n\
                   BEGIN:VEVENT\n\
                   UID:future\n\
                   SUMMARY:Future\n\
                   DTSTART:20260601T000000Z\n\
                   DTEND:20260601T010000Z\n\
                   END:VEVENT\n\
                   END:VCALENDAR";
        // window covering only the future event (2025-06 to 2026-06)
        let window_start = 1_748_736_000u64;
        let window_end = 1_780_272_000u64;
        let events = IcsReader::parse_events(ics, pid(), window_start, window_end).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uid, "future");
    }

    #[test]
    fn recurring_daily_with_count() {
        let ics = "BEGIN:VCALENDAR\n\
                   VERSION:2.0\n\
                   BEGIN:VEVENT\n\
                   UID:daily-3\n\
                   SUMMARY:Standup\n\
                   DTSTART:20260101T090000Z\n\
                   DTEND:20260101T093000Z\n\
                   RRULE:FREQ=DAILY;COUNT=3\n\
                   END:VEVENT\n\
                   END:VCALENDAR";
        let events = IcsReader::parse_events(ics, pid(), ALL.0, ALL.1).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[1].start - events[0].start, 86400);
        assert_eq!(events[2].start - events[0].start, 2 * 86400);
    }

    #[test]
    fn recurring_weekly_byday() {
        let ics = "BEGIN:VCALENDAR\n\
                   VERSION:2.0\n\
                   BEGIN:VEVENT\n\
                   UID:weekly-mw\n\
                   SUMMARY:Workout\n\
                   DTSTART:20260105T070000Z\n\
                   DTEND:20260105T080000Z\n\
                   RRULE:FREQ=WEEKLY;BYDAY=MO,WE;COUNT=4\n\
                   END:VEVENT\n\
                   END:VCALENDAR";
        let events = IcsReader::parse_events(ics, pid(), ALL.0, ALL.1).unwrap();
        assert_eq!(events.len(), 4);
        for ev in &events {
            let dt = DateTime::from_timestamp(ev.start as i64, 0).unwrap();
            let wd = dt.weekday();
            assert!(
                wd == Weekday::Mon || wd == Weekday::Wed,
                "expected Mon or Wed, got {wd:?}"
            );
        }
    }

    #[test]
    fn recurring_until() {
        let ics = "BEGIN:VCALENDAR\n\
                   VERSION:2.0\n\
                   BEGIN:VEVENT\n\
                   UID:until-test\n\
                   SUMMARY:Daily\n\
                   DTSTART:20260101T000000Z\n\
                   DTEND:20260101T010000Z\n\
                   RRULE:FREQ=DAILY;UNTIL=20260105T000000Z\n\
                   END:VEVENT\n\
                   END:VCALENDAR";
        let events = IcsReader::parse_events(ics, pid(), ALL.0, ALL.1).unwrap();
        // Jan 1,2,3,4,5 → 5 occurrences (UNTIL is inclusive)
        assert_eq!(events.len(), 5);
    }

    #[test]
    fn recurring_monthly() {
        let ics = "BEGIN:VCALENDAR\n\
                   VERSION:2.0\n\
                   BEGIN:VEVENT\n\
                   UID:monthly\n\
                   SUMMARY:Review\n\
                   DTSTART:20260115T100000Z\n\
                   DTEND:20260115T110000Z\n\
                   RRULE:FREQ=MONTHLY;COUNT=3\n\
                   END:VEVENT\n\
                   END:VCALENDAR";
        let events = IcsReader::parse_events(ics, pid(), ALL.0, ALL.1).unwrap();
        assert_eq!(events.len(), 3);
        let days: Vec<_> = events
            .iter()
            .map(|e| DateTime::from_timestamp(e.start as i64, 0).unwrap().day())
            .collect();
        assert_eq!(days, vec![15, 15, 15]);
    }

    #[test]
    fn event_ids_are_unique() {
        let ics = "BEGIN:VCALENDAR\n\
                   VERSION:2.0\n\
                   BEGIN:VEVENT\n\
                   UID:same-uid\n\
                   SUMMARY:Recurring\n\
                   DTSTART:20260301T120000Z\n\
                   DTEND:20260301T130000Z\n\
                   RRULE:FREQ=DAILY;COUNT=5\n\
                   END:VEVENT\n\
                   END:VCALENDAR";
        let events = IcsReader::parse_events(ics, pid(), ALL.0, ALL.1).unwrap();
        let ids: std::collections::HashSet<_> = events.iter().map(|e| e.id).collect();
        assert_eq!(ids.len(), events.len(), "event IDs should be unique");
    }

    #[test]
    fn profile_id_propagated() {
        let ics = "BEGIN:VCALENDAR\n\
                   VERSION:2.0\n\
                   BEGIN:VEVENT\n\
                   UID:pid-check\n\
                   SUMMARY:Test\n\
                   DTSTART:20260101T000000Z\n\
                   DTEND:20260101T010000Z\n\
                   END:VEVENT\n\
                   END:VCALENDAR";
        let profile_id = pid();
        let events = IcsReader::parse_events(ics, profile_id, ALL.0, ALL.1).unwrap();
        assert_eq!(events[0].profile_id, profile_id);
    }
}
