// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Cache-key helpers for per-period market-data caches.
//!
//! Each widget that pulls quotes from a market-data provider keeps a
//! per-period payload on disk so a fresh start can paint the last-known
//! prices before the network call resolves. Per-period (not flat) because
//! the chart series shape varies by period: 1D carries intraday bars,
//! 1Y carries daily closes, etc.
//!
//! Widget call sites pass a stable `prefix` (e.g. `"quotes-"` for
//! stocks, `"fx-quotes-"` for forex) and the active [`Period`]; this
//! module formats them into a key suitable for `ScopedCache`.

use crate::market_data::Period;

/// Build a per-period cache key by appending the lowercased period label
/// to `prefix`. Examples: `quotes_cache_key("quotes-", Period::Day)` →
/// `"quotes-1d"`, `quotes_cache_key("fx-quotes-", Period::Year)` →
/// `"fx-quotes-1y"`.
pub fn quotes_cache_key(prefix: &str, period: Period) -> String {
    format!("{prefix}{}", period.label().to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_cache_key_round_trips_known_prefixes() {
        assert_eq!(quotes_cache_key("quotes-", Period::Day), "quotes-1d");
        assert_eq!(quotes_cache_key("quotes-", Period::YearToDate), "quotes-ytd");
        assert_eq!(quotes_cache_key("fx-quotes-", Period::Year), "fx-quotes-1y");
        assert_eq!(quotes_cache_key("fx-quotes-", Period::TenYear), "fx-quotes-10y");
    }
}
