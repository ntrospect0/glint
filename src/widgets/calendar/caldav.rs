//! CalDAV calendar provider (RFC 4791) — talks to Apple iCloud, Fastmail,
//! Nextcloud, Synology, or any other server that speaks the standard.
//!
//! Flow:
//! 1. PROPFIND `/` → extract `<current-user-principal>` URL.
//! 2. PROPFIND on that principal URL → extract `<calendar-home-set>` URL.
//! 3. PROPFIND on the calendar home URL (Depth: 1) → list of calendar
//!    collections that support `VEVENT`.
//! 4. For each fetch, REPORT calendar-query on each calendar with a
//!    `time-range` filter, then parse the returned iCalendar payloads.
//!
//! Authentication is HTTP Basic with the user's app-specific password
//! (loaded from `~/.config/glint/credentials/caldav.toml`). Apple's
//! issuance flow lives at https://appleid.apple.com.

use std::io::BufReader;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::Deserialize;

use crate::auth;

use super::provider::{CalendarProvider, Event};

const DAV_NS: &str = "DAV:";
const CALDAV_NS: &str = "urn:ietf:params:xml:ns:caldav";

#[derive(Debug, Clone, Deserialize)]
pub struct CalDavCredentials {
    /// CalDAV root URL, e.g. `https://caldav.icloud.com`. Apple's server will
    /// redirect to a per-user host (`pNN-caldav.icloud.com`); reqwest follows
    /// it automatically.
    pub server: String,
    /// Account identifier (typically the email address for iCloud).
    pub username: String,
    /// App-specific password. Apple requires this when 2FA is on; generate one
    /// at https://appleid.apple.com under "App-Specific Passwords".
    pub app_password: String,
}

impl CalDavCredentials {
    pub fn load() -> Result<Option<Self>> {
        let path = auth::credentials_dir()?.join("caldav.toml");
        if !path.exists() {
            return Ok(None);
        }
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let creds: CalDavCredentials = toml::from_str(&contents)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        if creds.app_password.is_empty()
            || creds.app_password.starts_with("REPLACE_WITH_")
        {
            return Ok(None);
        }
        Ok(Some(creds))
    }
}

pub struct CalDavProvider {
    client: reqwest::Client,
    creds: CalDavCredentials,
    /// Calendar collection URLs. Empty + None resolution_state means we still
    /// need to auto-discover; we do that lazily on first fetch so widget
    /// construction stays synchronous.
    configured_calendars: Vec<String>,
    discovered: tokio::sync::Mutex<Option<Vec<String>>>,
}

impl CalDavProvider {
    /// Build the provider. Auto-discovery (if no calendars were explicitly
    /// configured) happens lazily on the first `fetch_range` call so widget
    /// construction stays sync.
    pub fn new(creds: CalDavCredentials, configured_calendars: Vec<String>) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "User-Agent",
            HeaderValue::from_static(concat!("glint-tui/", env!("CARGO_PKG_VERSION"))),
        );
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .context("failed to build CalDAV HTTP client")?;
        Ok(Self {
            client,
            creds,
            configured_calendars,
            discovered: tokio::sync::Mutex::new(None),
        })
    }

    /// Resolve the list of calendar URLs to query — either the user-configured
    /// ones, or auto-discovered + cached on first call.
    async fn calendars(&self) -> Result<Vec<String>> {
        if !self.configured_calendars.is_empty() {
            return Ok(self.configured_calendars.clone());
        }
        let mut guard = self.discovered.lock().await;
        if let Some(cached) = guard.as_ref() {
            return Ok(cached.clone());
        }
        let calendars = discover_calendars(&self.client, &self.creds)
            .await
            .context("CalDAV calendar discovery failed")?;
        if calendars.is_empty() {
            anyhow::bail!("CalDAV server returned no calendars");
        }
        *guard = Some(calendars.clone());
        Ok(calendars)
    }

    async fn fetch_calendar(
        &self,
        url: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<Event>> {
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <c:calendar-data/>
  </d:prop>
  <c:filter>
    <c:comp-filter name="VCALENDAR">
      <c:comp-filter name="VEVENT">
        <c:time-range start="{}" end="{}"/>
      </c:comp-filter>
    </c:comp-filter>
  </c:filter>
</c:calendar-query>
"#,
            format_caldav_ts(start),
            format_caldav_ts(end),
        );
        let resp = self
            .client
            .request(
                reqwest::Method::from_bytes(b"REPORT").expect("REPORT is a valid HTTP method"),
                url,
            )
            .basic_auth(&self.creds.username, Some(&self.creds.app_password))
            .header("Depth", "1")
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(body)
            .send()
            .await
            .with_context(|| format!("CalDAV REPORT {url} failed"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("CalDAV REPORT {url} returned {status}: {text}");
        }
        let body = resp.text().await.context("reading CalDAV REPORT body")?;
        let calendar_label = url
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("caldav")
            .to_string();
        Ok(parse_calendar_query_response(&body, &calendar_label))
    }
}

#[async_trait]
impl CalendarProvider for CalDavProvider {
    async fn fetch_range(
        &self,
        start: DateTime<Local>,
        end: DateTime<Local>,
    ) -> Result<Vec<Event>> {
        let start_utc = start.with_timezone(&Utc);
        let end_utc = end.with_timezone(&Utc);
        let calendars = self.calendars().await?;
        let mut events = Vec::new();
        for url in &calendars {
            match self.fetch_calendar(url, start_utc, end_utc).await {
                Ok(mut chunk) => events.append(&mut chunk),
                Err(err) => {
                    tracing::warn!(calendar = %url, error = %err, "CalDAV fetch failed");
                }
            }
        }
        events.sort_by_key(|e| e.start);
        Ok(events)
    }

    fn name(&self) -> &str {
        "caldav"
    }
}

/// PROPFIND chain to discover the user's calendars from a bare CalDAV
/// server URL. Returns absolute URLs.
async fn discover_calendars(
    client: &reqwest::Client,
    creds: &CalDavCredentials,
) -> Result<Vec<String>> {
    // 1. Find the current-user-principal URL.
    let principal = propfind_extract(
        client,
        creds,
        &creds.server,
        "0",
        r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:current-user-principal/></d:prop></d:propfind>"#,
        |doc| extract_first_href_inside(doc, "current-user-principal"),
    )
    .await
    .context("failed to discover current-user-principal")?;

    let principal_url = resolve_url(&creds.server, &principal);

    // 2. Find the calendar-home-set on that principal.
    let calendar_home = propfind_extract(
        client,
        creds,
        &principal_url,
        "0",
        r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><c:calendar-home-set/></d:prop>
</d:propfind>"#,
        |doc| extract_first_href_inside(doc, "calendar-home-set"),
    )
    .await
    .context("failed to discover calendar-home-set")?;

    let home_url = resolve_url(&creds.server, &calendar_home);

    // 3. List calendars under the home (Depth: 1). Filter to collections that
    //    actually support VEVENT.
    let body = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <d:resourcetype/>
    <d:displayname/>
    <c:supported-calendar-component-set/>
  </d:prop>
</d:propfind>"#;
    let xml = propfind(client, creds, &home_url, "1", body).await?;
    let doc =
        roxmltree::Document::parse(&xml).context("failed to parse CalDAV calendar-home response")?;

    let mut calendars: Vec<String> = Vec::new();
    for response in doc
        .descendants()
        .filter(|n| node_matches(n, DAV_NS, "response"))
    {
        let Some(href) = response
            .descendants()
            .find(|n| node_matches(n, DAV_NS, "href"))
            .and_then(|n| n.text())
        else {
            continue;
        };
        // Skip the home collection itself.
        let is_calendar = response.descendants().any(|n| {
            node_matches(&n, CALDAV_NS, "calendar")
                && n.parent()
                    .is_some_and(|p| node_matches(&p, DAV_NS, "resourcetype"))
        });
        if !is_calendar {
            continue;
        }
        // Confirm VEVENT support.
        let supports_vevent = response.descendants().any(|n| {
            node_matches(&n, CALDAV_NS, "comp")
                && n.attribute("name").is_some_and(|v| v == "VEVENT")
        });
        if !supports_vevent {
            continue;
        }
        calendars.push(resolve_url(&creds.server, href.trim()));
    }
    Ok(calendars)
}

/// Issue a PROPFIND and pass the parsed document to `f`. `f` returns Some
/// when it finds the property of interest.
async fn propfind_extract<F>(
    client: &reqwest::Client,
    creds: &CalDavCredentials,
    url: &str,
    depth: &str,
    body: &str,
    f: F,
) -> Result<String>
where
    F: Fn(&roxmltree::Document<'_>) -> Option<String>,
{
    let xml = propfind(client, creds, url, depth, body).await?;
    let doc = roxmltree::Document::parse(&xml).context("failed to parse PROPFIND response")?;
    f(&doc).context("expected property not found in PROPFIND response")
}

async fn propfind(
    client: &reqwest::Client,
    creds: &CalDavCredentials,
    url: &str,
    depth: &str,
    body: &str,
) -> Result<String> {
    let resp = client
        .request(
            reqwest::Method::from_bytes(b"PROPFIND").expect("PROPFIND is a valid HTTP method"),
            url,
        )
        .basic_auth(&creds.username, Some(&creds.app_password))
        .header("Depth", depth)
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(body.to_string())
        .send()
        .await
        .with_context(|| format!("PROPFIND {url} failed"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("PROPFIND {url} returned {status}: {text}");
    }
    resp.text().await.context("reading PROPFIND body")
}

/// Find the first `<DAV:href>` element nested inside a `<DAV:prop>` child
/// matching `inside_local_name` (with any DAV-ish namespace).
fn extract_first_href_inside(doc: &roxmltree::Document<'_>, inside_local_name: &str) -> Option<String> {
    for parent in doc.descendants() {
        if !node_matches(&parent, DAV_NS, inside_local_name)
            && !node_matches(&parent, CALDAV_NS, inside_local_name)
        {
            continue;
        }
        if let Some(h) = parent
            .descendants()
            .find(|n| node_matches(n, DAV_NS, "href"))
            .and_then(|n| n.text())
        {
            return Some(h.trim().to_string());
        }
    }
    None
}

fn node_matches(n: &roxmltree::Node<'_, '_>, ns: &str, local: &str) -> bool {
    n.is_element()
        && n.tag_name().name().eq_ignore_ascii_case(local)
        && n.tag_name().namespace().map(|s| s.eq_ignore_ascii_case(ns)).unwrap_or(false)
}

/// Combine a possibly-relative href with a base server URL.
fn resolve_url(base: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    // Strip path from base, keep scheme + host.
    let scheme_host = match base.find("://") {
        Some(idx) => {
            let after_scheme = &base[idx + 3..];
            match after_scheme.find('/') {
                Some(slash) => &base[..idx + 3 + slash],
                None => base,
            }
        }
        None => base,
    };
    let trimmed = scheme_host.trim_end_matches('/');
    if href.starts_with('/') {
        format!("{trimmed}{href}")
    } else {
        format!("{trimmed}/{href}")
    }
}

fn format_caldav_ts(t: DateTime<Utc>) -> String {
    // YYYYMMDDTHHMMSSZ (no separators) — CalDAV time-range format.
    t.format("%Y%m%dT%H%M%SZ").to_string()
}

/// Parse the multi-status XML body returned by REPORT calendar-query, extract
/// every `<C:calendar-data>` payload, run each through the ICS parser, and
/// collect `Event`s.
fn parse_calendar_query_response(xml: &str, calendar_label: &str) -> Vec<Event> {
    let doc = match roxmltree::Document::parse(xml) {
        Ok(d) => d,
        Err(err) => {
            tracing::warn!(error = %err, "CalDAV REPORT response parse error");
            return Vec::new();
        }
    };
    let mut events = Vec::new();
    for node in doc
        .descendants()
        .filter(|n| node_matches(n, CALDAV_NS, "calendar-data"))
    {
        let Some(text) = node.text() else { continue };
        events.extend(parse_ics_events(text, calendar_label));
    }
    events
}

/// Parse one or more iCalendar payloads via the `ical` crate, flatten the
/// VEVENTs and translate them into glint `Event`s.
fn parse_ics_events(ics: &str, calendar_label: &str) -> Vec<Event> {
    let reader = BufReader::new(ics.as_bytes());
    let parser = ical::IcalParser::new(reader);
    let mut out: Vec<Event> = Vec::new();
    for cal_result in parser {
        let Ok(cal) = cal_result else {
            continue;
        };
        for vevent in cal.events {
            if let Some(ev) = ical_event_to_event(&vevent, calendar_label) {
                out.push(ev);
            }
        }
    }
    out
}

fn ical_event_to_event(
    vevent: &ical::parser::ical::component::IcalEvent,
    calendar_label: &str,
) -> Option<Event> {
    let mut title = String::new();
    let mut location: Option<String> = None;
    let mut dtstart_raw: Option<(String, bool)> = None;
    let mut dtend_raw: Option<(String, bool)> = None;
    let mut status: Option<String> = None;
    for prop in &vevent.properties {
        let value = prop.value.clone().unwrap_or_default();
        match prop.name.as_str() {
            "SUMMARY" => title = value,
            "LOCATION" if !value.is_empty() => {
                location = Some(value);
            }
            "DTSTART" => dtstart_raw = Some((value, prop_is_date(&prop.params))),
            "DTEND" => dtend_raw = Some((value, prop_is_date(&prop.params))),
            "STATUS" => status = Some(value),
            _ => {}
        }
    }
    if matches!(status.as_deref(), Some("CANCELLED")) {
        return None;
    }
    let (start_raw, is_date_start) = dtstart_raw?;
    let (start, all_day) = parse_ics_datetime(&start_raw, is_date_start)?;
    let (end, _) = if let Some((end_raw, is_date_end)) = dtend_raw {
        parse_ics_datetime(&end_raw, is_date_end)?
    } else if all_day {
        (start + chrono::Duration::days(1), true)
    } else {
        (start + chrono::Duration::hours(1), false)
    };
    if title.is_empty() {
        return None;
    }
    Some(Event {
        title,
        start,
        end,
        all_day,
        calendar: calendar_label.to_string(),
        location,
    })
}

fn prop_is_date(params: &Option<Vec<(String, Vec<String>)>>) -> bool {
    let Some(params) = params else {
        return false;
    };
    params.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("VALUE") && v.iter().any(|s| s.eq_ignore_ascii_case("DATE"))
    })
}

/// Parse an iCalendar date-or-datetime string. Returns `(local_dt, all_day)`.
/// Handles `YYYYMMDD`, `YYYYMMDDTHHMMSS`, and `YYYYMMDDTHHMMSSZ` (UTC).
fn parse_ics_datetime(raw: &str, value_is_date: bool) -> Option<(DateTime<Local>, bool)> {
    let raw = raw.trim();
    if value_is_date || raw.len() == 8 {
        let d = NaiveDate::parse_from_str(raw, "%Y%m%d").ok()?;
        let midnight = d.and_hms_opt(0, 0, 0)?;
        let local = Local.from_local_datetime(&midnight).single()?;
        return Some((local, true));
    }
    // Datetime form. UTC if it ends with Z; else floating / TZID — treat
    // floating as local time, which matches what Apple usually sends with a
    // TZID parameter we're not preserving here.
    let (body, is_utc) = match raw.strip_suffix('Z') {
        Some(b) => (b, true),
        None => (raw, false),
    };
    let naive = chrono::NaiveDateTime::parse_from_str(body, "%Y%m%dT%H%M%S").ok()?;
    let local = if is_utc {
        Utc.from_utc_datetime(&naive).with_timezone(&Local)
    } else {
        Local.from_local_datetime(&naive).single()?
    };
    Some((local, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_url_handles_absolute_and_relative_hrefs() {
        let base = "https://caldav.icloud.com";
        assert_eq!(
            resolve_url(base, "/12345/principal/"),
            "https://caldav.icloud.com/12345/principal/"
        );
        assert_eq!(
            resolve_url(base, "https://p25-caldav.icloud.com/foo/"),
            "https://p25-caldav.icloud.com/foo/"
        );
        assert_eq!(
            resolve_url("https://caldav.icloud.com/some/path/", "/abc"),
            "https://caldav.icloud.com/abc"
        );
    }

    #[test]
    fn format_caldav_ts_renders_utc_with_z() {
        let t = Utc.with_ymd_and_hms(2026, 5, 20, 9, 30, 0).unwrap();
        assert_eq!(format_caldav_ts(t), "20260520T093000Z");
    }

    #[test]
    fn parse_ics_datetime_handles_all_forms() {
        let (dt, all_day) = parse_ics_datetime("20260520", true).unwrap();
        assert!(all_day);
        assert_eq!(dt.date_naive(), NaiveDate::from_ymd_opt(2026, 5, 20).unwrap());

        let (_dt, all_day) = parse_ics_datetime("20260520T093000Z", false).unwrap();
        assert!(!all_day);

        let (_dt, all_day) = parse_ics_datetime("20260520T093000", false).unwrap();
        assert!(!all_day);
    }
}
