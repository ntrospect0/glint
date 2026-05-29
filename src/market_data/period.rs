// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Time-window selector shared across market-data widgets (stocks, forex).
//!
//! Each [`Period`] variant maps to:
//!
//! * a short user-facing label (`1D`, `1W`, `1Y`, …) for the toggle bar,
//! * a Yahoo `(interval, range)` parameter pair for the v8/chart endpoint.
//!
//! Yahoo doesn't expose a native 3-year range, so [`Period::ThreeYear`]
//! asks for 5y of daily bars and trims client-side after the response
//! comes back.

use serde::{Deserialize, Serialize};

/// Time window selectable by the user from a market-data widget's toggle
/// bar. Used by stocks and forex.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Period {
    #[default]
    #[serde(alias = "1d", alias = "day")]
    Day,
    #[serde(alias = "1w", alias = "week", alias = "5d")]
    Week,
    #[serde(alias = "1m", alias = "month", alias = "1mo")]
    Month,
    #[serde(alias = "6m", alias = "6mo")]
    SixMonth,
    #[serde(alias = "ytd", alias = "year_to_date")]
    YearToDate,
    #[serde(alias = "1y", alias = "year")]
    Year,
    #[serde(alias = "3y", alias = "threeyear")]
    ThreeYear,
    #[serde(alias = "5y", alias = "fiveyear")]
    FiveYear,
    #[serde(alias = "10y", alias = "tenyear")]
    TenYear,
}

impl Period {
    pub const ALL: [Period; 9] = [
        Period::Day,
        Period::Week,
        Period::Month,
        Period::SixMonth,
        Period::YearToDate,
        Period::Year,
        Period::ThreeYear,
        Period::FiveYear,
        Period::TenYear,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Period::Day => "1D",
            Period::Week => "1W",
            Period::Month => "1M",
            Period::SixMonth => "6M",
            Period::YearToDate => "YTD",
            Period::Year => "1Y",
            Period::ThreeYear => "3Y",
            Period::FiveYear => "5Y",
            Period::TenYear => "10Y",
        }
    }

    /// Parse a period label (case-insensitive). Used by the runtime-state
    /// restore path so persisted labels round-trip through disk. `None`
    /// for anything that doesn't match, which lets the caller fall back
    /// to the configured default instead of panicking.
    pub fn from_label(label: &str) -> Option<Self> {
        match label.to_ascii_uppercase().as_str() {
            "1D" => Some(Period::Day),
            "1W" => Some(Period::Week),
            "1M" => Some(Period::Month),
            "6M" => Some(Period::SixMonth),
            "YTD" => Some(Period::YearToDate),
            "1Y" => Some(Period::Year),
            "3Y" => Some(Period::ThreeYear),
            "5Y" => Some(Period::FiveYear),
            "10Y" => Some(Period::TenYear),
            _ => None,
        }
    }

    /// (interval, range) query parameters for Yahoo's v8/chart endpoint.
    /// Longer windows use coarser intervals so the series stays a sane size.
    /// Yahoo doesn't expose a native 3-year range; we request 5y and trim
    /// client-side after the response comes back.
    pub fn yahoo_params(self) -> (&'static str, &'static str) {
        match self {
            // 2d (not 1d) so the response carries yesterday's full
            // regular session in addition to whatever today has so far.
            // The 1D chart renders only one trading day at a time, but
            // the chart filter falls back to yesterday's bars before
            // today's regular session opens — and during pre-market on
            // a fresh trading day, a literal 1d query returns only
            // today's pre-market bars with no yesterday data to fall
            // back to. The post-fetch downsampler caps bar count, so
            // doubling the range here doesn't bloat the cache.
            Period::Day => ("5m", "2d"),
            Period::Week => ("30m", "5d"),
            Period::Month => ("1d", "1mo"),
            Period::SixMonth => ("1d", "6mo"),
            Period::YearToDate => ("1d", "ytd"),
            Period::Year => ("1d", "1y"),
            // For 3y we ask for 5y of daily bars then slice client-side.
            Period::ThreeYear => ("1d", "5y"),
            // 1-week and 1-month bars for the long ranges keep the series
            // small enough to render cleanly in a braille graph.
            Period::FiveYear => ("1wk", "5y"),
            Period::TenYear => ("1mo", "10y"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn period_from_label_round_trips_all_labels() {
        for p in Period::ALL {
            assert_eq!(Period::from_label(p.label()), Some(p));
            // Lower-case input must also work — the runtime-state file
            // stores labels as the widget hands them out (we already
            // emit upper-case), but a hand-edited file could lower-case
            // them.
            assert_eq!(Period::from_label(&p.label().to_ascii_lowercase()), Some(p));
        }
        assert_eq!(Period::from_label("not-a-period"), None);
    }
}
