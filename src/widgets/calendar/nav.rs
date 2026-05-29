// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Navigation primitives for the calendar widget: web deep-links (Google,
//! Outlook), the bottom-row hint hit-test, month/week arithmetic, and the
//! weekday/month name helpers. Per-view rendering lives in `view_day.rs`,
//! `view_week.rs`, and `view_month.rs`; this module owns the
//! widget-agnostic shapes those renderers compose.

use chrono::{DateTime, Datelike, Duration as ChronoDuration, Local, NaiveDate, TimeZone, Weekday};
use ratatui::layout::Rect;

use super::config::{CalendarView, VIEW_TABS};

/// One choice surfaced by the `o` open-picker. `label` shows in the
/// modal; `url` is what we hand to `open::that`. URLs are computed
/// at picker-open time so they carry the calendar's current view +
/// anchor date as deep-link parameters where the provider supports
/// them.
#[derive(Debug, Clone)]
pub(super) struct WebTarget {
    pub(super) label: &'static str,
    pub(super) url: String,
}

/// Google Calendar deep-link URL for `view` on `date`. The `/r/`
/// prefix is the post-redirect canonical route; the trailing
/// `/{year}/{month}/{day}` segment is documented and stable —
/// Google honors it across day, week, and month views.
pub(super) fn google_calendar_url(view: CalendarView, date: NaiveDate) -> String {
    let segment = match view {
        CalendarView::Day => "day",
        CalendarView::Week => "week",
        CalendarView::Month => "month",
    };
    format!(
        "https://calendar.google.com/calendar/u/0/r/{}/{}/{}/{}",
        segment,
        date.year(),
        date.month(),
        date.day(),
    )
}

/// Outlook (Microsoft 365) deep-link URL for `view`. Uses the
/// `outlook.cloud.microsoft` surface Microsoft has been consolidating
/// M365 routes onto, with **lowercase** view segments — an earlier
/// draft used `outlook.office.com` with capitalized segments, and
/// those silently redirected to the user's saved default view
/// instead of honoring the requested one. The cloud.microsoft host
/// and lowercase segments both matter here. Date deep-link via the
/// URL is intentionally omitted — Outlook lands on today in the
/// requested view and the user navigates from there.
pub(super) fn outlook_calendar_url(view: CalendarView) -> &'static str {
    match view {
        CalendarView::Day => "https://outlook.cloud.microsoft/calendar/view/day",
        CalendarView::Week => "https://outlook.cloud.microsoft/calendar/view/week",
        CalendarView::Month => "https://outlook.cloud.microsoft/calendar/view/month",
    }
}

/// Distinct interactions exposed in the bottom hint row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BottomAction {
    Today,
    View(CalendarView),
}

/// Day and Month views get a 1-col gutter on each side of the widget's
/// inner area so the content doesn't sit flush against the rounded border.
/// Week view is already column-packed (7 cells + 6 separators); padding it
/// would compress the day cells, so it stays flush. All views also reserve
/// the bottom row for the `[Today] [Day] [Week] [Month]  ←/→ nav` hint —
/// without that reservation, the last visible agenda row gets painted
/// over by the hint and the user "can't scroll to the end" of a long day.
/// Both `render` and `handle_mouse` route through this helper so
/// click→date mapping aligns with the rendered grid.
pub(super) fn content_rect_for(view: CalendarView, inner: Rect) -> Rect {
    let body_height = inner.height.saturating_sub(1);
    match view {
        CalendarView::Day | CalendarView::Month if inner.width >= 4 => Rect {
            x: inner.x + 1,
            y: inner.y,
            width: inner.width - 2,
            height: body_height,
        },
        _ => Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: body_height,
        },
    }
}

/// Maps a click in the bottom hint row to a button. Layout must mirror the
/// spans emitted in `render`: leading space, `[Today]`, space, then `[Label]`
/// view tabs separated by single spaces.
pub(super) fn bottom_action_at(click_col: u16, hint_x: u16) -> Option<BottomAction> {
    let mut x = hint_x + 1; // leading space
    let today_w = "today".len() as u16 + 2;
    if click_col >= x && click_col < x + today_w {
        return Some(BottomAction::Today);
    }
    x += today_w + 1;
    for (v, label) in VIEW_TABS {
        let w = label.chars().count() as u16 + 2;
        if click_col >= x && click_col < x + w {
            return Some(BottomAction::View(*v));
        }
        x += w + 1;
    }
    None
}

pub(super) fn advance_month(year: i32, month: u32, delta: i32) -> (i32, u32) {
    let total = year * 12 + (month as i32 - 1) + delta;
    let new_year = total.div_euclid(12);
    let new_month = (total.rem_euclid(12) + 1) as u32;
    (new_year, new_month)
}

pub(super) fn local_midnight(date: NaiveDate) -> Option<DateTime<Local>> {
    Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0)?)
        .single()
}

/// Roll `d` back to the start of the week, where the week starts on
/// `first_day_of_week`. `from_sun = (today_dow - first_dow) mod 7` —
/// that's how many days to subtract regardless of which weekday the
/// caller chose to anchor on.
pub(super) fn start_of_week(d: NaiveDate, first_day_of_week: Weekday) -> NaiveDate {
    let today_idx = d.weekday().num_days_from_sunday();
    let first_idx = first_day_of_week.num_days_from_sunday();
    let offset = (today_idx + 7 - first_idx) % 7;
    d - ChronoDuration::days(i64::from(offset))
}

/// The seven weekday-short labels in column order, starting from
/// `first_day_of_week`. Used by Week- and Month-view headers so the
/// label row matches the grid's day ordering.
pub(super) fn rotated_weekday_labels(first_day_of_week: Weekday) -> [&'static str; 7] {
    const SUN_ANCHORED: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let shift = first_day_of_week.num_days_from_sunday() as usize;
    let mut out = [""; 7];
    for i in 0..7 {
        out[i] = SUN_ANCHORED[(i + shift) % 7];
    }
    out
}

pub(super) fn start_of_month(d: NaiveDate) -> NaiveDate {
    NaiveDate::from_ymd_opt(d.year(), d.month(), 1).unwrap_or(d)
}

pub(super) fn first_of_next_month(d: NaiveDate) -> NaiveDate {
    let (y, m) = if d.month() == 12 {
        (d.year() + 1, 1)
    } else {
        (d.year(), d.month() + 1)
    };
    NaiveDate::from_ymd_opt(y, m, 1).unwrap_or(d)
}

pub(super) fn weekday_short(w: Weekday) -> &'static str {
    match w {
        Weekday::Mon => "Mon",
        Weekday::Tue => "Tue",
        Weekday::Wed => "Wed",
        Weekday::Thu => "Thu",
        Weekday::Fri => "Fri",
        Weekday::Sat => "Sat",
        Weekday::Sun => "Sun",
    }
}

pub(super) fn month_long(m: u32) -> &'static str {
    match m {
        1 => "January",
        2 => "February",
        3 => "March",
        4 => "April",
        5 => "May",
        6 => "June",
        7 => "July",
        8 => "August",
        9 => "September",
        10 => "October",
        11 => "November",
        12 => "December",
        _ => "???",
    }
}
