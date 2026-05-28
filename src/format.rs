// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Compact-label formatters used across widgets.
//!
//! Three flavours, each tuned to a different display budget:
//!
//! * [`relative_time_label`] — "how long ago" for timestamps. Falls
//!   back to absolute `MMM DD` once you're past a few weeks, which
//!   conveys time-of-year better than `8w`. Used by widgets that
//!   display article / event timestamps (news, feeds).
//!
//! * [`short_duration_label`] — compact `Ns / Nm / Nh / Nd / Nmo`
//!   for cumulative durations or short ages. Useful for "data age"
//!   meta rows where the value is bounded by polling intervals.
//!
//! * [`uptime_label`] — `Nd Nh Nm` / `Nh Nm` / `Nm`. Multi-segment
//!   so process uptime ("running for 3d 4h 12m") reads naturally.
//!
//! Tests below cover the bucket boundaries each variant cares
//! about; widgets can adopt the one that matches their column
//! budget without reimplementing the math.
//!
//! See `docs/widget-sdk.md` § Formatting.

#![allow(dead_code)] // some exports are SDK surface for future widgets.

use chrono::{DateTime, Utc};

/// "How long ago" label tuned for narrow timestamp columns
/// (~6-7 cells). Future timestamps (clock skew, scheduled posts)
/// render as `now` rather than emitting negative buckets.
///
/// Buckets:
///   `now` (≤ 60 s) ·
///   `Nm` (< 60 min) ·
///   `Nh` (< 24 h) ·
///   `Nd` (< 7 d) ·
///   `Nw` (< 28 d) ·
///   `MMM DD` (older — absolute month/day for time-of-year clarity).
pub fn relative_time_label(when: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let delta = now.signed_duration_since(when);
    if delta.num_seconds() <= 0 {
        return "now".into();
    }
    let secs = delta.num_seconds();
    if secs < 60 {
        return "now".into();
    }
    let mins = delta.num_minutes();
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = delta.num_hours();
    if hours < 24 {
        return format!("{hours}h");
    }
    let days = delta.num_days();
    if days < 7 {
        return format!("{days}d");
    }
    if days < 28 {
        return format!("{}w", days / 7);
    }
    when.format("%b %d").to_string()
}

/// Compact single-segment label for short bounded durations: `Ns`,
/// `Nm`, `Nh`, `Nd`, then `Nmo` past a month. Negative inputs
/// clamp to `0s` so a clock-skew quirk doesn't render `-1m`.
///
/// Use this for "data age" meta lines, "last fetch" suffixes, and
/// other places where one segment is enough — for human-friendly
/// "ago" labels prefer [`relative_time_label`] which has more
/// precise short-range buckets and an absolute fallback.
pub fn short_duration_label(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else if secs < 86_400 * 30 {
        format!("{}d", secs / 86_400)
    } else {
        format!("{}mo", secs / (86_400 * 30))
    }
}

/// Multi-segment uptime label: `Nd Nh Nm` once a day has passed,
/// `Nh Nm` once an hour has, plain `Nm` otherwise. Reads naturally
/// for "process up for 3d 4h 12m" rather than collapsing to one
/// rough bucket.
pub fn uptime_label(secs: u64) -> String {
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3600;
    let m = (secs % 3600) / 60;
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;

    // ── relative_time_label ────────────────────────────────────

    #[test]
    fn relative_time_now_under_60s() {
        let now = Utc::now();
        assert_eq!(relative_time_label(now, now), "now");
        assert_eq!(
            relative_time_label(now - ChronoDuration::seconds(30), now),
            "now"
        );
    }

    #[test]
    fn relative_time_minute_bucket() {
        let now = Utc::now();
        assert_eq!(
            relative_time_label(now - ChronoDuration::minutes(5), now),
            "5m"
        );
        assert_eq!(
            relative_time_label(now - ChronoDuration::minutes(59), now),
            "59m"
        );
    }

    #[test]
    fn relative_time_hour_and_day_buckets() {
        let now = Utc::now();
        assert_eq!(
            relative_time_label(now - ChronoDuration::hours(3), now),
            "3h"
        );
        assert_eq!(
            relative_time_label(now - ChronoDuration::hours(23), now),
            "23h"
        );
        assert_eq!(
            relative_time_label(now - ChronoDuration::days(2), now),
            "2d"
        );
        assert_eq!(
            relative_time_label(now - ChronoDuration::days(6), now),
            "6d"
        );
    }

    #[test]
    fn relative_time_week_bucket() {
        let now = Utc::now();
        assert_eq!(
            relative_time_label(now - ChronoDuration::days(14), now),
            "2w"
        );
    }

    #[test]
    fn relative_time_falls_back_to_month_day_past_4_weeks() {
        let now = Utc::now();
        let out = relative_time_label(now - ChronoDuration::days(60), now);
        // "Mon DD" — 6 chars, contains a space.
        assert_eq!(out.chars().count(), 6);
        assert!(out.contains(' '));
    }

    #[test]
    fn relative_time_future_clamps_to_now() {
        let now = Utc::now();
        assert_eq!(
            relative_time_label(now + ChronoDuration::minutes(5), now),
            "now"
        );
    }

    // ── short_duration_label ───────────────────────────────────

    #[test]
    fn short_duration_buckets_cover_common_ranges() {
        assert_eq!(short_duration_label(0), "0s");
        assert_eq!(short_duration_label(45), "45s");
        assert_eq!(short_duration_label(59), "59s");
        assert_eq!(short_duration_label(60), "1m");
        assert_eq!(short_duration_label(3599), "59m");
        assert_eq!(short_duration_label(3600), "1h");
        assert_eq!(short_duration_label(86_399), "23h");
        assert_eq!(short_duration_label(86_400), "1d");
        assert_eq!(short_duration_label(86_400 * 30), "1mo");
        assert_eq!(short_duration_label(86_400 * 90), "3mo");
    }

    #[test]
    fn short_duration_clamps_negative_to_zero() {
        assert_eq!(short_duration_label(-5), "0s");
    }

    // ── uptime_label ───────────────────────────────────────────

    #[test]
    fn uptime_under_an_hour() {
        assert_eq!(uptime_label(0), "0m");
        assert_eq!(uptime_label(45), "0m");
        assert_eq!(uptime_label(60), "1m");
        assert_eq!(uptime_label(59 * 60), "59m");
    }

    #[test]
    fn uptime_at_or_above_one_hour_adds_h_segment() {
        assert_eq!(uptime_label(3600), "1h 0m");
        assert_eq!(uptime_label(3600 + 5 * 60), "1h 5m");
        assert_eq!(uptime_label(23 * 3600 + 59 * 60), "23h 59m");
    }

    #[test]
    fn uptime_at_or_above_one_day_adds_d_segment() {
        assert_eq!(uptime_label(86_400), "1d 0h 0m");
        assert_eq!(uptime_label(86_400 + 3600 + 60), "1d 1h 1m");
        assert_eq!(uptime_label(3 * 86_400 + 4 * 3600 + 12 * 60), "3d 4h 12m");
    }
}
