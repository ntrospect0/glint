// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Local, Utc};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::auth::google::{flow, store::GoogleToken, OAuthClientConfig};

use super::provider::{CalendarProvider, Event};

const CALENDAR_API_BASE: &str = "https://www.googleapis.com/calendar/v3";

pub struct GoogleCalendarProvider {
    http: reqwest::Client,
    client: OAuthClientConfig,
    token: Arc<Mutex<GoogleToken>>,
    calendar_ids: Vec<String>,
    /// `source` stamped on every event (the account label, or `"google"`
    /// for the default account) so multi-account colors don't collide.
    source: String,
    /// Account label whose token file this provider refreshes into.
    account: String,
}

impl GoogleCalendarProvider {
    pub fn new(
        client: OAuthClientConfig,
        token: GoogleToken,
        calendar_ids: Vec<String>,
        source: String,
        account: String,
    ) -> Result<Self> {
        let http = crate::http::shared();
        Ok(Self {
            http,
            client,
            token: Arc::new(Mutex::new(token)),
            calendar_ids: if calendar_ids.is_empty() {
                vec!["primary".into()]
            } else {
                calendar_ids
            },
            source,
            account,
        })
    }

    async fn access_token(&self) -> Result<String> {
        let mut t = self.token.lock().await;
        if t.is_expired(60) {
            let fresh = flow::refresh(&self.client, &t).await?;
            fresh.save_account(&self.account)?;
            *t = fresh;
        }
        Ok(t.access_token.clone())
    }

    async fn fetch_calendar(
        &self,
        calendar_id: &str,
        time_min: DateTime<Utc>,
        time_max: DateTime<Utc>,
    ) -> Result<Vec<Event>> {
        let token = self.access_token().await?;
        let url = format!(
            "{CALENDAR_API_BASE}/calendars/{cal}/events",
            cal = urlencoding::encode(calendar_id)
        );
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .query(&[
                ("timeMin", time_min.to_rfc3339()),
                ("timeMax", time_max.to_rfc3339()),
                ("singleEvents", "true".into()),
                ("orderBy", "startTime".into()),
                ("maxResults", "250".into()),
            ])
            .send()
            .await
            .context("calendar events request failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("calendar fetch failed ({status}): {body}");
        }
        let parsed: EventsResponse = resp
            .json()
            .await
            .context("failed to deserialize calendar events response")?;
        let mut out = Vec::with_capacity(parsed.items.len());
        for raw in parsed.items {
            if let Some(ev) = raw.into_event(calendar_id, &self.source) {
                out.push(ev);
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl CalendarProvider for GoogleCalendarProvider {
    async fn fetch_range(
        &self,
        start: DateTime<Local>,
        end: DateTime<Local>,
    ) -> Result<Vec<Event>> {
        let time_min = start.with_timezone(&Utc);
        let time_max = end.with_timezone(&Utc);
        let mut events = Vec::new();
        for cal in &self.calendar_ids {
            match self.fetch_calendar(cal, time_min, time_max).await {
                Ok(mut chunk) => events.append(&mut chunk),
                Err(err) => {
                    tracing::warn!(calendar = %cal, error = %err, "google calendar fetch failed");
                }
            }
        }
        events.sort_by_key(|e| e.start);
        Ok(events)
    }
}

#[derive(Debug, Deserialize)]
struct EventsResponse {
    #[serde(default)]
    items: Vec<RawGoogleEvent>,
}

#[derive(Debug, Deserialize)]
struct RawGoogleEvent {
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    location: Option<String>,
    start: GoogleTimeRef,
    end: GoogleTimeRef,
    #[serde(default)]
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GoogleTimeRef {
    #[serde(default)]
    date: Option<String>,
    #[serde(rename = "dateTime", default)]
    date_time: Option<String>,
}

impl RawGoogleEvent {
    fn into_event(self, calendar_id: &str, source: &str) -> Option<Event> {
        if matches!(self.status.as_deref(), Some("cancelled")) {
            return None;
        }
        let title = self.summary.unwrap_or_else(|| "(untitled)".to_string());
        let (start, end, all_day) = match (&self.start.date_time, &self.start.date) {
            (Some(dt), _) => {
                let s = chrono::DateTime::parse_from_rfc3339(dt)
                    .ok()?
                    .with_timezone(&Local);
                let e_str = self.end.date_time.as_deref()?;
                let e = chrono::DateTime::parse_from_rfc3339(e_str)
                    .ok()?
                    .with_timezone(&Local);
                (s, e, false)
            }
            (None, Some(d)) => {
                let sd = chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()?;
                let ed = chrono::NaiveDate::parse_from_str(self.end.date.as_deref()?, "%Y-%m-%d")
                    .ok()?;
                let s = local_midnight(sd)?;
                let e = local_midnight(ed)?;
                (s, e, true)
            }
            _ => return None,
        };
        Some(Event {
            title,
            start,
            end,
            all_day,
            source: source.into(),
            calendar: calendar_id.to_string(),
            location: self.location,
        })
    }
}

fn local_midnight(date: chrono::NaiveDate) -> Option<DateTime<Local>> {
    use chrono::TimeZone;
    Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0)?)
        .single()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_timed_event() {
        let raw = RawGoogleEvent {
            summary: Some("Standup".into()),
            location: Some("Zoom".into()),
            start: GoogleTimeRef {
                date: None,
                date_time: Some("2026-05-20T09:30:00-07:00".into()),
            },
            end: GoogleTimeRef {
                date: None,
                date_time: Some("2026-05-20T10:00:00-07:00".into()),
            },
            status: None,
        };
        let e = raw.into_event("primary", "google").unwrap();
        assert_eq!(e.title, "Standup");
        assert_eq!(e.location.as_deref(), Some("Zoom"));
        assert!(!e.all_day);
        assert_eq!(e.calendar, "primary");
    }

    #[test]
    fn parses_all_day_event() {
        let raw = RawGoogleEvent {
            summary: Some("Conference".into()),
            location: None,
            start: GoogleTimeRef {
                date: Some("2026-05-23".into()),
                date_time: None,
            },
            end: GoogleTimeRef {
                date: Some("2026-05-24".into()),
                date_time: None,
            },
            status: None,
        };
        let e = raw.into_event("primary", "google").unwrap();
        assert!(e.all_day);
        assert_eq!(e.title, "Conference");
    }

    #[test]
    fn skips_cancelled_events() {
        let raw = RawGoogleEvent {
            summary: Some("Cancelled".into()),
            location: None,
            start: GoogleTimeRef {
                date: Some("2026-05-23".into()),
                date_time: None,
            },
            end: GoogleTimeRef {
                date: Some("2026-05-24".into()),
                date_time: None,
            },
            status: Some("cancelled".into()),
        };
        assert!(raw.into_event("primary", "google").is_none());
    }
}
