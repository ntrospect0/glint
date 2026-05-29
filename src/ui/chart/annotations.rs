// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Calendar-aware helpers for chart x-axis labels. Short month names,
//! leap-year predicate, the rolling 12-month label generator the long-
//! period x-axis uses, and [`period_annotations`] — the calendar-
//! boundary detector used by both stocks and forex to drive vertical
//! guide lines + their accompanying x-axis labels for periods 1W and
//! longer.

use chrono::{Datelike, Weekday};

use crate::market_data::Period;

/// Three-letter English month abbreviation. Inputs outside `1..=12`
/// return `"???"` — the call sites here all derive `m` from
/// [`chrono::Datelike`] so the fallback is unreachable in practice;
/// the explicit value keeps test output legible if something ever does
/// go wrong.
pub fn month_short_name(m: u32) -> &'static str {
    match m {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => "???",
    }
}

/// Gregorian leap-year predicate. Inlined so x-axis math doesn't need
/// a chrono detour for one ternary.
pub fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

/// 7 month-name labels for a rolling 12-month window ending today,
/// stepped 2 months apart so the right-anchored x-axis layout (which
/// spaces 7 labels into 6 equal intervals = 12 months / 2 months per
/// gap) maps exactly to 12 months. e.g. today = 2026-05-23 →
/// `["May","Jul","Sep","Nov","Jan","Mar","May"]`.
pub fn rolling_year_month_labels(today: chrono::NaiveDate) -> Vec<String> {
    let now_month = today.month() as i32;
    let offsets = [12i32, 10, 8, 6, 4, 2, 0];
    offsets
        .iter()
        .map(|off| {
            let m_idx = (now_month - off - 1).rem_euclid(12);
            month_short_name((m_idx as u32) + 1).to_string()
        })
        .collect()
}

/// Copy a static `&[&str]` into an owned `Vec<String>` so x-axis label
/// match arms can mix static + dynamic label sets without lifetime
/// gymnastics.
pub fn str_labels(labels: &[&str]) -> Vec<String> {
    labels.iter().map(|s| (*s).to_string()).collect()
}

/// Annotation for the calendar-aligned vertical guides + x-axis labels.
/// Each entry pins a label to a specific bar index in the rendered
/// series; the renderer maps that bar's column position to draw both
/// the guide and the label so they share an x-coordinate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodAnnotation {
    pub bar_index: usize,
    pub label: String,
}

/// Compute the calendar-boundary annotations for `period` from a slice
/// of bar timestamps (unix seconds, UTC). Each annotation pins a short
/// label to the first bar of a new natural unit:
///
///   - 1W → start of each new local trading day (Mon..Fri).
///   - 1M → start of each new ISO week (labelled `wk1`..`wk4`).
///   - 6M → start of each new month.
///   - YTD → start of each new calendar quarter (Jan / Apr / Jul / Oct).
///   - 1Y → start of each new calendar quarter.
///   - 3Y / 5Y → start of each new calendar year.
///   - 10Y → every second calendar year boundary.
///
/// 1D returns an empty list — its only useful x-axis markers are the
/// clock-time labels (the per-widget renderer handles those).
pub fn period_annotations(period: Period, timestamps: &[i64]) -> Vec<PeriodAnnotation> {
    if timestamps.is_empty() {
        return Vec::new();
    }
    let to_local = |ts: i64| {
        chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
            .map(|dt| dt.with_timezone(&chrono::Local))
    };
    // Resolve every timestamp once so the boundary-iteration loops below
    // don't redo the chrono conversion.
    let local: Vec<chrono::DateTime<chrono::Local>> =
        timestamps.iter().filter_map(|t| to_local(*t)).collect();
    if local.len() != timestamps.len() || local.is_empty() {
        return Vec::new();
    }
    let mut out = match period {
        Period::Day => Vec::new(),
        Period::Week => annotate_when_changes(
            &local,
            |dt| dt.date_naive().ordinal0() as i32,
            |dt| match dt.weekday() {
                Weekday::Mon => "Mon",
                Weekday::Tue => "Tue",
                Weekday::Wed => "Wed",
                Weekday::Thu => "Thu",
                Weekday::Fri => "Fri",
                Weekday::Sat => "Sat",
                Weekday::Sun => "Sun",
            }
            .to_string(),
        ),
        Period::Month => annotate_when_changes(
            &local,
            |dt| dt.iso_week().week() as i32 * 100 + (dt.iso_week().year() % 100),
            |dt| format!("wk{}", iso_week_of_month_or_zero(*dt) + 1),
        ),
        Period::SixMonth => annotate_when_changes(
            &local,
            |dt| dt.year() * 100 + dt.month() as i32,
            |dt| month_short_name(dt.month()).to_string(),
        ),
        Period::YearToDate | Period::Year => annotate_when_changes(
            &local,
            // Group by (year, calendar quarter) so a guide lands at
            // the first bar of each Q1/Q2/Q3/Q4 boundary.
            |dt| dt.year() * 10 + ((dt.month() as i32 - 1) / 3),
            |dt| month_short_name(dt.month()).to_string(),
        ),
        Period::ThreeYear | Period::FiveYear => {
            annotate_when_changes(&local, |dt| dt.year(), |dt| format!("{}", dt.year()))
        }
        Period::TenYear => {
            // Year-changes filtered to even years (every-other-year guides).
            let mut anns = annotate_when_changes(
                &local,
                |dt| dt.year(),
                |dt| format!("{}", dt.year()),
            );
            anns.retain(|ann| {
                ann.label
                    .parse::<i32>()
                    .map(|y| y % 2 == 0)
                    .unwrap_or(true)
            });
            anns
        }
    };

    // Bar 0 is always emitted by `annotate_when_changes` as the chart's
    // leftmost label, but most charts open mid-unit (a 1Y chart in late
    // May starts at the tail of Q2; a 5Y in May 2021 starts mid-2021;
    // a 1W where Yahoo's first bar lands at 14:00 starts mid-day) and
    // that leading partial-unit label crowds the real next-unit label
    // visually — leaving no space for "Jul" / "2022" / "Tue" to render.
    //
    // Detect this by comparing the leading gap (bar 0 → next
    // annotation) against the median of the later gaps. When bar 0
    // truly is at a unit boundary, the leading gap is approximately a
    // full unit, so `first_gap * 2 > median_later`. When bar 0 is mid-
    // unit, the leading gap is less than half a unit and the
    // inequality flips. The check applies to every period whose
    // annotations are calendar-aligned (i.e., not 1D, which returned
    // an empty list at the top of this match).
    if out.len() >= 3 && out[0].bar_index == 0 {
        let first_gap = out[1].bar_index;
        let mut later_gaps: Vec<usize> = (1..out.len() - 1)
            .map(|i| out[i + 1].bar_index - out[i].bar_index)
            .collect();
        if !later_gaps.is_empty() {
            later_gaps.sort();
            let median_later = later_gaps[later_gaps.len() / 2];
            // Strictly less-than so a bar 0 that happens to land
            // exactly one unit before the next boundary (e.g. Jan 1
            // for a 5Y chart where every annotation is Jan 1)
            // doesn't get dropped.
            if first_gap < median_later {
                out.remove(0);
            }
        }
    }
    out
}

/// Iterate `local` in order, emitting an annotation each time `key(dt)`
/// changes between consecutive bars. The annotation is pinned to the
/// *first* bar of the new value of `key` and the label comes from
/// `label_of` applied to that same bar.
fn annotate_when_changes<K, L>(
    local: &[chrono::DateTime<chrono::Local>],
    key: K,
    label_of: L,
) -> Vec<PeriodAnnotation>
where
    K: Fn(&chrono::DateTime<chrono::Local>) -> i32,
    L: Fn(&chrono::DateTime<chrono::Local>) -> String,
{
    if local.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut last_key = key(&local[0]);
    out.push(PeriodAnnotation {
        bar_index: 0,
        label: label_of(&local[0]),
    });
    for (i, dt) in local.iter().enumerate().skip(1) {
        let k = key(dt);
        if k != last_key {
            out.push(PeriodAnnotation {
                bar_index: i,
                label: label_of(dt),
            });
            last_key = k;
        }
    }
    out
}

/// 0-indexed ISO-week ordinal within the month containing `dt`. Used by
/// the 1M period to label week boundaries as `wk1`, `wk2`, etc., where
/// `wk1` is the week containing the 1st of the month. Falls back to 0
/// if the chrono calculation produces something nonsensical (shouldn't
/// happen in practice).
fn iso_week_of_month_or_zero(dt: chrono::DateTime<chrono::Local>) -> u32 {
    let day = dt.day();
    // Approximate "week of month" as `(day-1)/7` — close enough for
    // labeling, doesn't need to match ISO week boundaries exactly.
    (day.saturating_sub(1)) / 7
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_short_name_covers_all_months_and_falls_back() {
        assert_eq!(month_short_name(1), "Jan");
        assert_eq!(month_short_name(12), "Dec");
        assert_eq!(month_short_name(13), "???");
        assert_eq!(month_short_name(0), "???");
    }

    #[test]
    fn is_leap_year_matches_gregorian_rules() {
        assert!(is_leap_year(2000));
        assert!(is_leap_year(2024));
        assert!(!is_leap_year(1900));
        assert!(!is_leap_year(2023));
    }

    #[test]
    fn rolling_year_month_labels_emits_seven_2month_steps() {
        // May → Mar 12 months back stepped by 2.
        let labels = rolling_year_month_labels(
            chrono::NaiveDate::from_ymd_opt(2026, 5, 23).unwrap()
        );
        assert_eq!(labels, vec!["May", "Jul", "Sep", "Nov", "Jan", "Mar", "May"]);
    }

    #[test]
    fn str_labels_clones_into_owned_strings() {
        let owned = str_labels(&["a", "b", "c"]);
        assert_eq!(owned, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
    }
}
