//! Microsoft Graph (Outlook / Microsoft 365) calendar provider. Mirrors the
//! Google provider's shape: takes the user's token, optionally a list of
//! calendar IDs, and fetches events for a time range via REST.
//!
//! API base: <https://graph.microsoft.com/v1.0>. Events come from
//! `/me/calendarView?startDateTime=…&endDateTime=…`, which Microsoft
//! recommends for ranged reads (expands recurring events server-side).

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::auth::microsoft::{flow, store::MicrosoftToken, OAuthClientConfig};

use super::provider::{CalendarProvider, Event};

const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";
const PAGE_SIZE: u32 = 250;

pub struct OutlookCalendarProvider {
    http: reqwest::Client,
    client: OAuthClientConfig,
    token: Arc<Mutex<MicrosoftToken>>,
    /// Calendar IDs to fetch. Empty (or "primary") means the user's default
    /// calendar via `/me/calendarView`.
    calendar_ids: Vec<String>,
}

impl OutlookCalendarProvider {
    pub fn new(
        client: OAuthClientConfig,
        token: MicrosoftToken,
        calendar_ids: Vec<String>,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("glint-tui/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .context("failed to build Microsoft Graph HTTP client")?;
        Ok(Self {
            http,
            client,
            token: Arc::new(Mutex::new(token)),
            calendar_ids,
        })
    }

    async fn access_token(&self) -> Result<String> {
        let mut t = self.token.lock().await;
        if t.is_expired(60) {
            let fresh = flow::refresh(&self.client, &t).await?;
            fresh.save()?;
            *t = fresh;
        }
        Ok(t.access_token.clone())
    }

    /// Fetch events for one calendar id (or the default when `None`).
    async fn fetch_calendar(
        &self,
        calendar_id: Option<&str>,
        time_min: DateTime<Utc>,
        time_max: DateTime<Utc>,
    ) -> Result<Vec<Event>> {
        let token = self.access_token().await?;
        let endpoint = match calendar_id {
            None | Some("primary") => format!("{GRAPH_BASE}/me/calendarView"),
            Some(id) => format!(
                "{GRAPH_BASE}/me/calendars/{}/calendarView",
                urlencoding::encode(id)
            ),
        };
        let mut url = format!(
            "{endpoint}?startDateTime={start}&endDateTime={end}&$top={top}",
            start = urlencoding::encode(&time_min.to_rfc3339()),
            end = urlencoding::encode(&time_max.to_rfc3339()),
            top = PAGE_SIZE,
        );

        let calendar_label = calendar_id.unwrap_or("primary").to_string();
        let mut events: Vec<Event> = Vec::new();
        loop {
            let resp = self
                .http
                .get(&url)
                .bearer_auth(&token)
                // Forces UTC for all returned dateTimes; saves us TZ parsing.
                .header("Prefer", "outlook.timezone=\"UTC\"")
                .send()
                .await
                .context("Graph calendarView request failed")?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("Graph calendarView returned {status}: {body}");
            }
            let parsed: CalendarViewResponse = resp
                .json()
                .await
                .context("failed to deserialize Graph calendarView response")?;
            for raw in parsed.value {
                if let Some(ev) = raw.into_event(&calendar_label) {
                    events.push(ev);
                }
            }
            match parsed.next_link {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(events)
    }
}

#[async_trait]
impl CalendarProvider for OutlookCalendarProvider {
    async fn fetch_range(
        &self,
        start: DateTime<Local>,
        end: DateTime<Local>,
    ) -> Result<Vec<Event>> {
        let time_min = start.with_timezone(&Utc);
        let time_max = end.with_timezone(&Utc);
        let mut events = Vec::new();
        if self.calendar_ids.is_empty() {
            // Default calendar
            match self.fetch_calendar(None, time_min, time_max).await {
                Ok(mut chunk) => events.append(&mut chunk),
                Err(err) => {
                    tracing::warn!(error = %err, "Outlook default calendar fetch failed");
                }
            }
        } else {
            for cal in &self.calendar_ids {
                match self.fetch_calendar(Some(cal), time_min, time_max).await {
                    Ok(mut chunk) => events.append(&mut chunk),
                    Err(err) => {
                        tracing::warn!(calendar = %cal, error = %err, "Outlook calendar fetch failed");
                    }
                }
            }
        }
        events.sort_by_key(|e| e.start);
        Ok(events)
    }
}

#[derive(Debug, Deserialize)]
struct CalendarViewResponse {
    #[serde(default)]
    value: Vec<RawGraphEvent>,
    #[serde(default, rename = "@odata.nextLink")]
    next_link: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawGraphEvent {
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    location: Option<GraphLocation>,
    start: GraphTimeRef,
    end: GraphTimeRef,
    #[serde(default, rename = "isAllDay")]
    is_all_day: bool,
    #[serde(default, rename = "isCancelled")]
    is_cancelled: bool,
}

#[derive(Debug, Deserialize)]
struct GraphLocation {
    #[serde(default, rename = "displayName")]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphTimeRef {
    #[serde(rename = "dateTime")]
    date_time: String,
    /// `timeZone` is what the server normalized to. With the `Prefer:
    /// outlook.timezone="UTC"` header we send, this is always "UTC" for
    /// timed events; all-day events use a date-only form.
    #[serde(rename = "timeZone", default)]
    time_zone: String,
}

impl RawGraphEvent {
    fn into_event(self, calendar_label: &str) -> Option<Event> {
        if self.is_cancelled {
            return None;
        }
        let title = self.subject.unwrap_or_default();
        if title.is_empty() {
            return None;
        }
        let (start, end, all_day) = if self.is_all_day {
            // All-day: dateTime values like "2026-05-20T00:00:00.0000000"
            // representing the local midnight of the start/end dates.
            let start_date = parse_graph_date(&self.start.date_time)?;
            let end_date = parse_graph_date(&self.end.date_time)?;
            let start = local_midnight(start_date)?;
            let end = local_midnight(end_date)?;
            (start, end, true)
        } else {
            let s = parse_graph_datetime(&self.start.date_time, &self.start.time_zone)?;
            let e = parse_graph_datetime(&self.end.date_time, &self.end.time_zone)?;
            (s, e, false)
        };
        let location = self.location.and_then(|l| l.display_name).filter(|s| !s.is_empty());
        Some(Event {
            title,
            start,
            end,
            all_day,
            source: "outlook".into(),
            calendar: calendar_label.to_string(),
            location,
        })
    }
}

/// Parse the date portion out of Graph's `dateTime` field. Examples:
///   "2026-05-20T00:00:00.0000000"
///   "2026-05-20T09:30:00Z"
fn parse_graph_date(raw: &str) -> Option<NaiveDate> {
    let day = raw.get(..10)?;
    NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()
}

/// Parse a Graph datetime into a `DateTime<Local>`. Graph timestamps come
/// without offset suffix (`2026-05-20T09:30:00.0000000`) — they're in the
/// timezone named by `time_zone`, which is "UTC" when we ask for it via the
/// `Prefer` header.
fn parse_graph_datetime(raw: &str, _time_zone: &str) -> Option<DateTime<Local>> {
    let head = raw.split('.').next()?; // strip fractional seconds
    let head = head.trim_end_matches('Z');
    let naive = NaiveDateTime::parse_from_str(head, "%Y-%m-%dT%H:%M:%S").ok()?;
    // With Prefer: outlook.timezone="UTC", time_zone is always UTC. We
    // treat unknown / unspecified zones as UTC too — Graph normalizes
    // everything to UTC when the Prefer header is set.
    Some(Utc.from_utc_datetime(&naive).with_timezone(&Local))
}

fn local_midnight(date: NaiveDate) -> Option<DateTime<Local>> {
    Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0)?)
        .single()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_timed_event() {
        let raw = RawGraphEvent {
            subject: Some("Standup".into()),
            location: Some(GraphLocation {
                display_name: Some("Teams".into()),
            }),
            start: GraphTimeRef {
                date_time: "2026-05-20T16:30:00.0000000".into(),
                time_zone: "UTC".into(),
            },
            end: GraphTimeRef {
                date_time: "2026-05-20T17:00:00.0000000".into(),
                time_zone: "UTC".into(),
            },
            is_all_day: false,
            is_cancelled: false,
        };
        let e = raw.into_event("primary").unwrap();
        assert_eq!(e.title, "Standup");
        assert_eq!(e.location.as_deref(), Some("Teams"));
        assert!(!e.all_day);
        assert_eq!(e.calendar, "primary");
    }

    #[test]
    fn parses_all_day_event() {
        let raw = RawGraphEvent {
            subject: Some("Vacation".into()),
            location: None,
            start: GraphTimeRef {
                date_time: "2026-05-20T00:00:00.0000000".into(),
                time_zone: "UTC".into(),
            },
            end: GraphTimeRef {
                date_time: "2026-05-21T00:00:00.0000000".into(),
                time_zone: "UTC".into(),
            },
            is_all_day: true,
            is_cancelled: false,
        };
        let e = raw.into_event("primary").unwrap();
        assert!(e.all_day);
        assert_eq!(e.title, "Vacation");
    }

    #[test]
    fn skips_cancelled() {
        let raw = RawGraphEvent {
            subject: Some("Cancelled meeting".into()),
            location: None,
            start: GraphTimeRef {
                date_time: "2026-05-20T16:30:00.0000000".into(),
                time_zone: "UTC".into(),
            },
            end: GraphTimeRef {
                date_time: "2026-05-20T17:00:00.0000000".into(),
                time_zone: "UTC".into(),
            },
            is_all_day: false,
            is_cancelled: true,
        };
        assert!(raw.into_event("primary").is_none());
    }
}
