// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Local, NaiveDate};
use serde::{Deserialize, Serialize};

/// A single calendar event normalized to local time. All-day events have
/// `all_day = true` and `start`/`end` set to midnight at the event date.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub title: String,
    pub start: DateTime<Local>,
    pub end: DateTime<Local>,
    pub all_day: bool,
    /// The backend that produced this event: "google", "outlook", "caldav",
    /// "local". Used together with `calendar` so the color-assignment map can
    /// disambiguate accounts that share a calendar id (e.g. both Google and
    /// Outlook have a calendar named "primary").
    pub source: String,
    pub calendar: String,
    pub location: Option<String>,
}

impl Event {
    /// True if any portion of the event falls inside `[range_start, range_end)`.
    pub fn overlaps(&self, range_start: DateTime<Local>, range_end: DateTime<Local>) -> bool {
        self.start < range_end && self.end > range_start
    }

    /// True if the event covers any moment of the given local date.
    pub fn on_date(&self, date: NaiveDate) -> bool {
        let day_start = date
            .and_hms_opt(0, 0, 0)
            .and_then(|d| d.and_local_timezone(Local).single())
            .expect("midnight should always be a valid local datetime");
        let day_end = (date + chrono::Duration::days(1))
            .and_hms_opt(0, 0, 0)
            .and_then(|d| d.and_local_timezone(Local).single())
            .expect("midnight should always be a valid local datetime");
        self.overlaps(day_start, day_end)
    }
}

/// A `CalendarProvider` returns the list of events whose start/end interval
/// overlaps the supplied half-open range.
#[async_trait]
pub trait CalendarProvider: Send + Sync {
    async fn fetch_range(&self, start: DateTime<Local>, end: DateTime<Local>)
        -> Result<Vec<Event>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(start: &str, end: &str, all_day: bool) -> Event {
        Event {
            title: "test".into(),
            start: DateTime::parse_from_rfc3339(start)
                .unwrap()
                .with_timezone(&Local),
            end: DateTime::parse_from_rfc3339(end)
                .unwrap()
                .with_timezone(&Local),
            all_day,
            source: "local".into(),
            calendar: "test".into(),
            location: None,
        }
    }

    fn parse(ts: &str) -> DateTime<Local> {
        DateTime::parse_from_rfc3339(ts)
            .unwrap()
            .with_timezone(&Local)
    }

    #[test]
    fn overlaps_is_half_open() {
        // Use explicit offsets everywhere so the test does not depend on the
        // host's local timezone.
        let e = ev(
            "2026-05-20T09:00:00+00:00",
            "2026-05-20T10:00:00+00:00",
            false,
        );
        let before = parse("2026-05-20T08:00:00+00:00");
        let mid_plus_1 = parse("2026-05-20T09:31:00+00:00");
        let after = parse("2026-05-20T11:00:00+00:00");
        let after_plus_1h = parse("2026-05-20T12:00:00+00:00");
        assert!(e.overlaps(before, mid_plus_1));
        assert!(e.overlaps(before, after));
        assert!(!e.overlaps(after, after_plus_1h));
    }
}
