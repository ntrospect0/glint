use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Local, NaiveDate, TimeZone};
use serde::Deserialize;

use super::provider::{CalendarProvider, Event};

/// Schema for `~/.config/glint/calendar.toml`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct LocalCalendarFile {
    #[serde(default)]
    pub events: Vec<RawEvent>,
}

/// One row in `[[events]]`. Either timestamps must be RFC3339 (e.g.
/// `2026-05-20T09:30:00-07:00`) for timed events, or plain `YYYY-MM-DD` dates
/// for all-day events.
#[derive(Debug, Clone, Deserialize)]
pub struct RawEvent {
    pub title: String,
    pub start: String,
    pub end: String,
    #[serde(default)]
    pub all_day: bool,
    #[serde(default = "default_calendar")]
    pub calendar: String,
    #[serde(default)]
    pub location: Option<String>,
}

fn default_calendar() -> String {
    "default".into()
}

impl RawEvent {
    fn parse(self) -> Result<Event> {
        let (start, end, all_day) = if self.all_day || is_bare_date(&self.start) {
            let s = parse_local_date(&self.start)
                .with_context(|| format!("invalid start date {:?}", self.start))?;
            let e = parse_local_date(&self.end)
                .with_context(|| format!("invalid end date {:?}", self.end))?;
            // For an all-day event ending on date D, treat the end as the
            // beginning of D+1 so single-day events still have non-zero length.
            let e_exclusive = e
                .checked_add_signed(chrono::Duration::days(1))
                .context("date overflow extending all-day end")?;
            (s, e_exclusive, true)
        } else {
            let s = DateTime::parse_from_rfc3339(&self.start)
                .with_context(|| format!("invalid RFC3339 start {:?}", self.start))?
                .with_timezone(&Local);
            let e = DateTime::parse_from_rfc3339(&self.end)
                .with_context(|| format!("invalid RFC3339 end {:?}", self.end))?
                .with_timezone(&Local);
            (s, e, false)
        };
        Ok(Event {
            title: self.title,
            start,
            end,
            all_day,
            source: "local".into(),
            calendar: self.calendar,
            location: self.location,
        })
    }
}

fn is_bare_date(s: &str) -> bool {
    s.len() == 10 && NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
}

fn parse_local_date(s: &str) -> Result<DateTime<Local>> {
    let date = NaiveDate::parse_from_str(s, "%Y-%m-%d")?;
    let midnight = date
        .and_hms_opt(0, 0, 0)
        .context("date had no midnight (clock change?)")?;
    Local
        .from_local_datetime(&midnight)
        .single()
        .context("ambiguous local time at midnight")
}

pub struct LocalCalendarProvider {
    events: Vec<Event>,
}

impl LocalCalendarProvider {
    pub fn from_file(file: LocalCalendarFile) -> Result<Self> {
        let mut events = Vec::with_capacity(file.events.len());
        for raw in file.events {
            events.push(raw.parse()?);
        }
        Ok(Self { events })
    }

    pub fn empty() -> Self {
        Self { events: Vec::new() }
    }
}

#[async_trait]
impl CalendarProvider for LocalCalendarProvider {
    async fn fetch_range(
        &self,
        start: DateTime<Local>,
        end: DateTime<Local>,
    ) -> Result<Vec<Event>> {
        let mut filtered: Vec<Event> = self
            .events
            .iter()
            .filter(|e| e.overlaps(start, end))
            .cloned()
            .collect();
        filtered.sort_by_key(|e| e.start);
        Ok(filtered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(start: &str, end: &str, all_day: bool) -> RawEvent {
        RawEvent {
            title: "x".into(),
            start: start.into(),
            end: end.into(),
            all_day,
            calendar: "default".into(),
            location: None,
        }
    }

    #[test]
    fn rfc3339_timed_event_parses_into_local_time() {
        let e = raw("2026-05-20T09:00:00Z", "2026-05-20T10:00:00Z", false)
            .parse()
            .unwrap();
        assert!(!e.all_day);
        assert!(e.end > e.start);
    }

    #[test]
    fn bare_date_treated_as_all_day_with_exclusive_end() {
        let e = raw("2026-05-20", "2026-05-20", false).parse().unwrap();
        assert!(e.all_day);
        assert_eq!(e.end - e.start, chrono::Duration::days(1));
    }

    #[tokio::test]
    async fn fetch_range_filters_and_sorts() {
        let file = LocalCalendarFile {
            events: vec![
                raw("2026-05-20T15:00:00Z", "2026-05-20T16:00:00Z", false),
                raw("2026-05-20T09:00:00Z", "2026-05-20T10:00:00Z", false),
                raw("2026-06-01T09:00:00Z", "2026-06-01T10:00:00Z", false),
            ],
        };
        let p = LocalCalendarProvider::from_file(file).unwrap();
        let start = Local.with_ymd_and_hms(2026, 5, 20, 0, 0, 0).unwrap();
        let end = Local.with_ymd_and_hms(2026, 5, 21, 0, 0, 0).unwrap();
        let got = p.fetch_range(start, end).await.unwrap();
        assert_eq!(got.len(), 2);
        assert!(got[0].start < got[1].start);
    }
}
