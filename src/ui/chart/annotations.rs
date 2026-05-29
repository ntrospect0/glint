// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Calendar-aware helpers for chart x-axis labels. Today: short month
//! names, leap-year predicate, and a rolling 12-month label generator
//! the long-period x-axis uses. Annotation-driven label layout (the
//! `lay_out_x_axis_labels_at_cols` path stocks uses today) will land
//! here in a later phase when forex needs it too.

use chrono::Datelike;

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
