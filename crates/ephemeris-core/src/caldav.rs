//! Thin CalDAV client (ADR ephemeris-op3 / ephemeris-yvu.2).
//!
//! Implements the subset needed for read sync:
//! - principal + calendar-home discovery via `PROPFIND`
//! - calendar listing via `PROPFIND` depth 1
//! - event pull via `REPORT` `calendar-query`
//!
//! Auth: HTTP Basic (username + password / app token). Bearer can be layered later.
//!
//! Compiled only with the **`caldav`** feature (pulls in `reqwest`).

use crate::ics::IcsReader;
use crate::{AppError, CalendarEvent, ProfileId, Result};
use std::time::{SystemTime, UNIX_EPOCH};

/// Credentials + server root for a CalDAV account.
#[derive(Debug, Clone)]
pub struct CalDavConfig {
    /// Server URL (principal, well-known, or calendar-home). Trailing slash optional.
    pub url: String,
    pub username: String,
    pub password: String,
}

impl CalDavConfig {
    pub fn new(
        url: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            url: url.into(),
            username: username.into(),
            password: password.into(),
        }
    }
}

/// A discovered calendar collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalDavCalendar {
    /// Absolute or server-relative href of the calendar collection.
    pub href: String,
    pub display_name: String,
}

/// Thin CalDAV client over blocking `reqwest`.
pub struct CalDavClient {
    config: CalDavConfig,
    client: reqwest::blocking::Client,
}

impl CalDavClient {
    pub fn new(config: CalDavConfig) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| AppError::Backend(format!("CalDAV HTTP client: {e}")))?;
        Ok(Self { config, client })
    }

    /// Discover calendars: current-user-principal → calendar-home-set → list.
    pub fn discover_calendars(&self) -> Result<Vec<CalDavCalendar>> {
        let root = normalize_url(&self.config.url);
        let principal = self
            .propfind_first_href(&root, 0, CURRENT_USER_PRINCIPAL_BODY)?
            .unwrap_or_else(|| root.clone());
        let principal_url = resolve_href(&root, &principal);
        let home = self
            .propfind_first_href(&principal_url, 0, CALENDAR_HOME_SET_BODY)?
            .unwrap_or(principal_url);
        let home_url = resolve_href(&root, &home);
        self.list_calendars(&home_url)
    }

    /// List calendar collections under a calendar-home URL.
    pub fn list_calendars(&self, home_url: &str) -> Result<Vec<CalDavCalendar>> {
        let xml = self.propfind(home_url, 1, LIST_CALENDARS_BODY)?;
        Ok(parse_calendar_list(&xml))
    }

    /// Pull VEVENTs from one calendar into the given time window (Unix seconds).
    pub fn fetch_events(
        &self,
        calendar_href: &str,
        profile_id: ProfileId,
        window_start: u64,
        window_end: u64,
    ) -> Result<Vec<CalendarEvent>> {
        let root = normalize_url(&self.config.url);
        let cal_url = resolve_href(&root, calendar_href);
        let body = calendar_query_body(window_start, window_end);
        let xml = self.report(&cal_url, &body)?;
        let mut events = Vec::new();
        for ics in extract_calendar_data(&xml) {
            let parsed = IcsReader::parse_events(&ics, profile_id, window_start, window_end)?;
            for mut ev in parsed {
                if ev.source.is_empty() {
                    ev.source = format!("caldav:{cal_url}");
                }
                events.push(ev);
            }
        }
        Ok(events)
    }

    /// Discover all calendars and fetch events in the window from each.
    pub fn sync_all(
        &self,
        profile_id: ProfileId,
        window_start: u64,
        window_end: u64,
    ) -> Result<(Vec<CalDavCalendar>, Vec<CalendarEvent>)> {
        let calendars = self.discover_calendars()?;
        let mut events = Vec::new();
        for cal in &calendars {
            match self.fetch_events(&cal.href, profile_id, window_start, window_end) {
                Ok(mut evs) => events.append(&mut evs),
                Err(e) => tracing::warn!("CalDAV fetch {}: {e}", cal.href),
            }
        }
        Ok((calendars, events))
    }

    fn propfind_first_href(&self, url: &str, depth: u32, body: &str) -> Result<Option<String>> {
        let xml = self.propfind(url, depth, body)?;
        Ok(extract_hrefs(&xml).into_iter().next())
    }

    fn propfind(&self, url: &str, depth: u32, body: &str) -> Result<String> {
        self.request("PROPFIND", url, body, &[("Depth", &depth.to_string())])
    }

    fn report(&self, url: &str, body: &str) -> Result<String> {
        self.request("REPORT", url, body, &[("Depth", "1")])
    }

    fn request(
        &self,
        method: &str,
        url: &str,
        body: &str,
        extra_headers: &[(&str, &str)],
    ) -> Result<String> {
        let mut req = self
            .client
            .request(
                reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET),
                url,
            )
            .basic_auth(&self.config.username, Some(&self.config.password))
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(body.to_string());
        for (k, v) in extra_headers {
            req = req.header(*k, *v);
        }
        let resp = req
            .send()
            .map_err(|e| AppError::Backend(format!("CalDAV {method} {url}: {e}")))?;
        let status = resp.status();
        let text = resp
            .text()
            .map_err(|e| AppError::Backend(format!("CalDAV body: {e}")))?;
        // 207 Multi-Status is success for PROPFIND/REPORT.
        if !(status.is_success() || status.as_u16() == 207) {
            return Err(AppError::Backend(format!(
                "CalDAV {method} {url} → HTTP {status}: {}",
                text.chars().take(200).collect::<String>()
            )));
        }
        Ok(text)
    }
}

// ── XML helpers (fixture-tested; no XML crate dependency) ────────────────────

const CURRENT_USER_PRINCIPAL_BODY: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<d:propfind xmlns:d="DAV:">
  <d:prop><d:current-user-principal/></d:prop>
</d:propfind>"#;

const CALENDAR_HOME_SET_BODY: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><c:calendar-home-set/></d:prop>
</d:propfind>"#;

const LIST_CALENDARS_BODY: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <d:displayname/>
    <d:resourcetype/>
  </d:prop>
</d:propfind>"#;

fn calendar_query_body(window_start: u64, window_end: u64) -> String {
    let start = unix_to_ical_utc(window_start);
    let end = unix_to_ical_utc(window_end);
    format!(
        r#"<?xml version="1.0" encoding="utf-8" ?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <d:getetag/>
    <c:calendar-data/>
  </d:prop>
  <c:filter>
    <c:comp-filter name="VCALENDAR">
      <c:comp-filter name="VEVENT">
        <c:time-range start="{start}" end="{end}"/>
      </c:comp-filter>
    </c:comp-filter>
  </c:filter>
</c:calendar-query>"#
    )
}

fn unix_to_ical_utc(ts: u64) -> String {
    use chrono::{TimeZone, Utc};
    let dt = Utc
        .timestamp_opt(ts as i64, 0)
        .single()
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap());
    dt.format("%Y%m%dT%H%M%SZ").to_string()
}

fn normalize_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

/// Resolve a possibly-relative href against a base URL.
pub fn resolve_href(base: &str, href: &str) -> String {
    let href = href.trim();
    let joined = if href.starts_with("http://") || href.starts_with("https://") {
        href.to_string()
    } else if let Some(scheme_end) = base.find("://") {
        // Absolute path on same origin.
        let after = &base[scheme_end + 3..];
        if let Some(slash) = after.find('/') {
            let origin = &base[..scheme_end + 3 + slash];
            let path = if href.starts_with('/') {
                href.to_string()
            } else {
                format!("/{href}")
            };
            format!("{}{}", origin.trim_end_matches('/'), path)
        } else {
            let path = if href.starts_with('/') {
                href.to_string()
            } else {
                format!("/{href}")
            };
            format!("{}{}", base.trim_end_matches('/'), path)
        }
    } else {
        format!(
            "{}/{}",
            base.trim_end_matches('/'),
            href.trim_start_matches('/')
        )
    };
    joined.trim_end_matches('/').to_string()
}

/// Extract all `<…:href>…</…:href>` / `<href>…</href>` values (order preserved).
pub fn extract_hrefs(xml: &str) -> Vec<String> {
    extract_local_tag_texts(xml, "href")
}

/// Extract `<…:calendar-data>…</…:calendar-data>` ICS payloads.
pub fn extract_calendar_data(xml: &str) -> Vec<String> {
    extract_local_tag_texts(xml, "calendar-data")
        .into_iter()
        .map(|s| xml_unescape(&s))
        .filter(|s| s.contains("BEGIN:VCALENDAR") || s.contains("BEGIN:VEVENT"))
        .collect()
}

/// Parse PROPFIND multistatus into calendar collections (resourcetype contains calendar).
pub fn parse_calendar_list(xml: &str) -> Vec<CalDavCalendar> {
    let mut out = Vec::new();
    for response in split_responses(xml) {
        let Some(rt) = find_tag_inner(&response, "resourcetype") else {
            continue;
        };
        let rt_l = rt.to_ascii_lowercase();
        // Must be a CalDAV calendar collection, not merely a parent folder.
        if !rt_l.contains("calendar") {
            continue;
        }
        let href = extract_hrefs(&response)
            .into_iter()
            .next()
            .unwrap_or_default();
        if href.is_empty() {
            continue;
        }
        let display_name = find_tag_inner(&response, "displayname")
            .map(|s| xml_unescape(s.trim()))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                href.trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or("Calendar")
                    .to_string()
            });
        out.push(CalDavCalendar { href, display_name });
    }
    out
}

fn split_responses(xml: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let lower = xml.to_ascii_lowercase();
    let mut search_from = 0;
    while let Some(rel) = lower[search_from..].find("<d:response") {
        let start = search_from + rel;
        let Some(open_end) = xml[start..].find('>') else {
            break;
        };
        let content_start = start + open_end + 1;
        let end_tag = "</d:response>";
        let end_tag_ci = "</d:response>";
        // Also allow unprefixed.
        let end = lower[content_start..]
            .find(end_tag_ci)
            .or_else(|| lower[content_start..].find("</response>"));
        let Some(end_rel) = end else {
            break;
        };
        let end_abs = content_start + end_rel;
        parts.push(xml[content_start..end_abs].to_string());
        search_from = end_abs + end_tag.len();
    }
    if parts.is_empty() {
        // Fallback: unprefixed <response>
        search_from = 0;
        let lower = xml.to_ascii_lowercase();
        while let Some(rel) = lower[search_from..].find("<response") {
            let start = search_from + rel;
            if lower[start..].starts_with("<response")
                && !lower[start..].starts_with("<response>")
                && xml[start..]
                    .chars()
                    .nth(9)
                    .is_some_and(|c| c != '>' && c != ' ')
            {
                // might be responsedescription — skip careful
            }
            let Some(open_end) = xml[start..].find('>') else {
                break;
            };
            let content_start = start + open_end + 1;
            let end = lower[content_start..].find("</response>");
            let Some(end_rel) = end else {
                break;
            };
            let end_abs = content_start + end_rel;
            parts.push(xml[content_start..end_abs].to_string());
            search_from = end_abs + "</response>".len();
        }
    }
    parts
}

fn extract_local_tag_texts(xml: &str, local: &str) -> Vec<String> {
    let mut out = Vec::new();
    let lower = xml.to_ascii_lowercase();
    let local_l = local.to_ascii_lowercase();
    let mut search_from = 0;
    while search_from < lower.len() {
        // Find '<' then optional prefix then local name.
        let Some(rel) = lower[search_from..].find('<') else {
            break;
        };
        let start = search_from + rel;
        let after_lt = start + 1;
        if after_lt >= lower.len() || lower.as_bytes()[after_lt] == b'/' {
            search_from = after_lt;
            continue;
        }
        // Skip to tag name end.
        let rest = &lower[after_lt..];
        let name_end = rest
            .find(|c: char| c == '>' || c == ' ' || c == '/')
            .unwrap_or(0);
        let raw_name = &rest[..name_end];
        let local_name = raw_name.rsplit(':').next().unwrap_or(raw_name);
        if local_name != local_l {
            search_from = after_lt;
            continue;
        }
        let open = &xml[start..];
        if open.starts_with("</") {
            search_from = after_lt;
            continue;
        }
        // Self-closing?
        let Some(gt) = open.find('>') else {
            break;
        };
        if open[..gt].ends_with('/') {
            search_from = start + gt + 1;
            continue;
        }
        let content_start = start + gt + 1;
        // Closing tag: </prefix:local> or </local>
        let close_patterns = [format!("</{raw_name}>"), format!("</{local_l}>")];
        let mut closed = None;
        for pat in &close_patterns {
            if let Some(rel_c) = lower[content_start..].find(&pat.to_ascii_lowercase()) {
                closed = Some((content_start + rel_c, pat.len()));
                break;
            }
        }
        // Also try any prefix: </*:local>
        if closed.is_none() {
            let needle = format!(":{local_l}>");
            if let Some(rel_c) = lower[content_start..].find(&needle) {
                // walk back to '<'
                let abs = content_start + rel_c;
                if let Some(lt) = xml[..abs].rfind("</") {
                    closed = Some((lt, abs + needle.len() - lt));
                }
            }
        }
        let Some((end, _len)) = closed else {
            search_from = content_start;
            continue;
        };
        out.push(xml[content_start..end].to_string());
        search_from = end + 1;
    }
    out
}

fn find_tag_inner<'a>(xml: &'a str, local: &str) -> Option<&'a str> {
    let lower = xml.to_ascii_lowercase();
    let local_l = local.to_ascii_lowercase();
    let mut search_from = 0;
    while search_from < lower.len() {
        let Some(rel) = lower[search_from..].find('<') else {
            return None;
        };
        let start = search_from + rel;
        let after_lt = start + 1;
        if after_lt >= lower.len() || lower.as_bytes()[after_lt] == b'/' {
            search_from = after_lt;
            continue;
        }
        let rest = &lower[after_lt..];
        let name_end = rest
            .find(|c: char| c == '>' || c == ' ' || c == '/')
            .unwrap_or(0);
        let raw_name = &rest[..name_end];
        let local_name = raw_name.rsplit(':').next().unwrap_or(raw_name);
        if local_name != local_l {
            search_from = after_lt;
            continue;
        }
        let open = &xml[start..];
        let Some(gt) = open.find('>') else {
            return None;
        };
        if open[..gt].ends_with('/') {
            search_from = start + gt + 1;
            continue;
        }
        let content_start = start + gt + 1;
        let needle = format!(":{local_l}>");
        let end = if let Some(rel_c) = lower[content_start..].find(&format!("</{raw_name}>")) {
            content_start + rel_c
        } else if let Some(rel_c) = lower[content_start..].find(&format!("</{local_l}>")) {
            content_start + rel_c
        } else if let Some(rel_c) = lower[content_start..].find(&needle) {
            let abs = content_start + rel_c;
            xml[..abs].rfind("</")?
        } else {
            search_from = content_start;
            continue;
        };
        return Some(&xml[content_start..end]);
    }
    None
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Default sync window: now − 30d … now + 180d.
pub fn default_sync_window() -> (u64, u64) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let start = now.saturating_sub(30 * 86_400);
    let end = now.saturating_add(180 * 86_400);
    (start, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRINCIPAL_XML: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/</d:href>
    <d:propstat>
      <d:prop>
        <d:current-user-principal>
          <d:href>/principals/users/alice/</d:href>
        </d:current-user-principal>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;

    const HOME_XML: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/principals/users/alice/</d:href>
    <d:propstat>
      <d:prop>
        <c:calendar-home-set>
          <d:href>/calendars/alice/</d:href>
        </c:calendar-home-set>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;

    const LIST_XML: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/calendars/alice/</d:href>
    <d:propstat>
      <d:prop>
        <d:displayname>Home</d:displayname>
        <d:resourcetype><d:collection/></d:resourcetype>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/calendars/alice/work/</d:href>
    <d:propstat>
      <d:prop>
        <d:displayname>Work</d:displayname>
        <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/calendars/alice/personal/</d:href>
    <d:propstat>
      <d:prop>
        <d:displayname>Personal</d:displayname>
        <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;

    const REPORT_XML: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/calendars/alice/work/evt1.ics</d:href>
    <d:propstat>
      <d:prop>
        <c:calendar-data>BEGIN:VCALENDAR
VERSION:2.0
BEGIN:VEVENT
UID:evt-1
SUMMARY:Standup
DTSTART:20260301T090000Z
DTEND:20260301T093000Z
END:VEVENT
END:VCALENDAR</c:calendar-data>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;

    #[test]
    fn extracts_principal_href() {
        let hrefs = extract_hrefs(PRINCIPAL_XML);
        assert!(hrefs.iter().any(|h| h.contains("principals/users/alice")));
    }

    #[test]
    fn extracts_home_set_href() {
        let hrefs = extract_hrefs(HOME_XML);
        assert!(hrefs.iter().any(|h| h.contains("/calendars/alice")));
    }

    #[test]
    fn parses_calendar_list_skips_non_calendar() {
        let cals = parse_calendar_list(LIST_XML);
        assert_eq!(cals.len(), 2);
        assert_eq!(cals[0].display_name, "Work");
        assert_eq!(cals[1].display_name, "Personal");
        assert!(cals[0].href.contains("work"));
    }

    #[test]
    fn extracts_calendar_data_ics() {
        let data = extract_calendar_data(REPORT_XML);
        assert_eq!(data.len(), 1);
        assert!(data[0].contains("UID:evt-1"));
        assert!(data[0].contains("Standup"));
    }

    #[test]
    fn resolve_href_absolute_and_path() {
        assert_eq!(
            resolve_href("https://dav.example.com", "/calendars/a/"),
            "https://dav.example.com/calendars/a"
        );
        assert_eq!(
            resolve_href("https://dav.example.com/dav", "https://other/x"),
            "https://other/x"
        );
    }

    #[test]
    fn parse_events_from_report_fixture() {
        let pid = ProfileId::new();
        let ics = &extract_calendar_data(REPORT_XML)[0];
        let events = IcsReader::parse_events(ics, pid, 0, u64::MAX / 2).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].title, "Standup");
        assert_eq!(events[0].uid, "evt-1");
    }
}
