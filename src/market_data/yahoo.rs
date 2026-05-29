// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Yahoo Finance v8/chart adapter shared by the stocks and forex widgets.
//!
//! Provides:
//!
//! * [`build_client`] — a reqwest client preconfigured with the
//!   browser-shaped User-Agent Yahoo requires, the shared 10s timeout,
//!   and a cookie jar (used by stocks for the quoteSummary crumb auth
//!   flow; ignored by forex).
//! * Wire-shape deserializer types for `/v8/finance/chart/{sym}`
//!   responses. The [`ChartMeta`] struct is the union of fields stocks
//!   and forex consume; every field is `#[serde(default)]` so a widget
//!   that only reads a subset pays nothing for the unused fields.
//! * [`downsample_pairs`] for `(timestamp, value)` chart series. Both
//!   stocks and forex use it; the paired form is what lets the
//!   renderer place calendar-aligned vertical guides on the chart.

use anyhow::{Context, Result};
use serde::Deserialize;

/// Build the shared Yahoo HTTP client. Browser-shaped UA so the chart
/// endpoint doesn't reject us, 10s timeout, cookie jar (needed by stocks
/// for the v10/quoteSummary crumb auth; harmless for forex). Identifies
/// as glint underneath for transparency in Yahoo's server logs.
pub fn build_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!(
            "Mozilla/5.0 (compatible; glint-tui/",
            env!("CARGO_PKG_VERSION"),
            ")"
        ))
        .timeout(std::time::Duration::from_secs(10))
        .cookie_store(true)
        .build()
        .context("failed to build Yahoo Finance HTTP client")
}

/// query1 is the host Yahoo uses for the v8/chart endpoint that both
/// stocks and forex talk to. quoteSummary lives on query2 (stocks only).
pub const CHART_BASE_URL: &str = "https://query1.finance.yahoo.com";

#[derive(Debug, Deserialize)]
pub struct ChartResponse {
    pub chart: ChartBody,
}

#[derive(Debug, Deserialize)]
pub struct ChartBody {
    #[serde(default)]
    pub result: Option<Vec<ChartResult>>,
    #[serde(default)]
    pub error: Option<ChartError>,
}

#[derive(Debug, Deserialize)]
pub struct ChartError {
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Deserialize)]
pub struct ChartResult {
    pub meta: ChartMeta,
    #[serde(default)]
    pub timestamp: Option<Vec<i64>>,
    #[serde(default)]
    pub indicators: Option<ChartIndicators>,
}

/// Union of the meta fields stocks and forex consume. Every field is
/// `#[serde(default)]` so a widget that reads a subset pays nothing for
/// the unused fields; forex deserializes the stocks-only fields and
/// discards them.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChartMeta {
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub short_name: Option<String>,
    #[serde(default)]
    pub long_name: Option<String>,
    #[serde(default)]
    pub currency: Option<String>,
    #[serde(default)]
    pub regular_market_price: Option<f64>,
    #[serde(default)]
    pub previous_close: Option<f64>,
    #[serde(default)]
    pub chart_previous_close: Option<f64>,
    #[serde(default)]
    pub regular_market_day_high: Option<f64>,
    #[serde(default)]
    pub regular_market_day_low: Option<f64>,
    #[serde(default)]
    pub fifty_two_week_high: Option<f64>,
    #[serde(default)]
    pub fifty_two_week_low: Option<f64>,
    #[serde(default)]
    pub regular_market_volume: Option<u64>,
    #[serde(default)]
    pub average_daily_volume_10_day: Option<u64>,
    #[serde(default)]
    pub market_cap: Option<u64>,
    #[serde(default)]
    pub post_market_price: Option<f64>,
    #[serde(default)]
    pub post_market_change: Option<f64>,
    #[serde(default)]
    pub post_market_change_percent: Option<f64>,
    #[serde(default)]
    pub pre_market_price: Option<f64>,
    #[serde(default)]
    pub pre_market_change: Option<f64>,
    #[serde(default)]
    pub pre_market_change_percent: Option<f64>,
    #[serde(default)]
    pub market_state: Option<String>,
    #[serde(default)]
    pub current_trading_period: Option<CurrentTradingPeriod>,
    /// Periods the chart's bars actually cover (one entry per trading
    /// day, inner array per session within a day — usually 1). Stocks
    /// picks the most-recent-completed entry as the previous-session
    /// bounds.
    #[serde(default)]
    pub trading_periods: Option<TradingPeriods>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentTradingPeriod {
    #[serde(default)]
    pub regular: Option<TradingPeriod>,
}

/// `tradingPeriods` arrives in two different shapes depending on the
/// chart endpoint's interval/range:
///
/// * On 1D (`interval=5m&range=2d`) Yahoo nests the sessions under
///   pre / regular / post keys: `{ regular: [[…]], pre: [[…]], … }`.
/// * On 1W (`interval=30m&range=5d`) Yahoo elides the keys and
///   ships the regular sessions as a bare list-of-lists at the top
///   level: `[[…], [[…], …]`.
/// * On longer ranges (1M / 6M / 1Y / 5Y / 10Y / YTD) the field is
///   absent or null altogether.
///
/// The single-struct deserializer we had only matched the first
/// shape, so the 1W fetch failed at the response-parse step for
/// every symbol — list rows fell back to `err` and the chart
/// showed `Loading {sym}…` forever. An untagged enum accepts both
/// concrete shapes; null still maps to `None` via the surrounding
/// `Option<TradingPeriods>` field's `#[serde(default)]`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum TradingPeriods {
    /// Object form: `{ regular: [[…]], pre: [[…]], post: [[…]] }`.
    /// We only care about `regular`; the other keys ride along but
    /// stay unused.
    Structured {
        #[serde(default)]
        regular: Vec<Vec<TradingPeriod>>,
    },
    /// Bare list-of-lists. Each inner list is a single regular
    /// trading session.
    Flat(Vec<Vec<TradingPeriod>>),
}

impl TradingPeriods {
    /// Regular-session list-of-lists, abstracted across both Yahoo
    /// response shapes so the consumer doesn't have to branch.
    pub fn regular(&self) -> &Vec<Vec<TradingPeriod>> {
        match self {
            TradingPeriods::Structured { regular } => regular,
            TradingPeriods::Flat(sessions) => sessions,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct TradingPeriod {
    #[serde(default)]
    pub start: Option<i64>,
    #[serde(default)]
    pub end: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ChartIndicators {
    #[serde(default)]
    pub quote: Vec<QuoteBars>,
}

#[derive(Debug, Deserialize)]
pub struct QuoteBars {
    #[serde(default)]
    pub close: Vec<Option<f64>>,
}

/// Trim `(timestamp, value)` pairs to at most `max` evenly-spaced points,
/// preserving the first and last samples. Used to keep multi-year daily
/// series from holding thousands of points in memory + disk cache when
/// the chart can only show ~200 columns at the widest. Timestamps follow
/// values through the downsample so the renderer can still find the
/// column position of a specific date.
pub fn downsample_pairs(pairs: Vec<(i64, f64)>, max: usize) -> Vec<(i64, f64)> {
    if max == 0 || pairs.len() <= max {
        return pairs;
    }
    let n = pairs.len();
    let mut out = Vec::with_capacity(max);
    for i in 0..max {
        let idx = (i * (n - 1)) / (max - 1);
        out.push(pairs[idx]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Yahoo ships `tradingPeriods` in three shapes; our deserializer
    /// has to accept all three or the whole `chart` response fails
    /// to parse for any range that hits a shape we missed. The
    /// regression that motivated this: 1W (`interval=30m&range=5d`)
    /// returns the bare list-of-lists form. Before the untagged
    /// enum, this broke every 1W fetch — symbol rows fell back to
    /// `err` and the chart said `Loading {sym}…` forever.
    #[test]
    fn trading_periods_accepts_object_list_and_null_shapes() {
        let object_shape = r#"{ "regular": [[{"start": 1, "end": 2}]] }"#;
        let parsed: TradingPeriods = serde_json::from_str(object_shape).unwrap();
        assert_eq!(parsed.regular().len(), 1);
        assert_eq!(parsed.regular()[0][0].start, Some(1));

        let list_shape = r#"[
            [{"start": 10, "end": 20}],
            [{"start": 30, "end": 40}]
        ]"#;
        let parsed: TradingPeriods = serde_json::from_str(list_shape).unwrap();
        assert_eq!(parsed.regular().len(), 2);
        assert_eq!(parsed.regular()[1][0].start, Some(30));

        let null_shape = r#"null"#;
        let parsed: Option<TradingPeriods> = serde_json::from_str(null_shape).unwrap();
        assert!(parsed.is_none());
    }

    #[test]
    fn downsample_pairs_returns_input_when_already_under_cap() {
        let s: Vec<(i64, f64)> = vec![(100, 1.0), (200, 2.0), (300, 3.0), (400, 4.0)];
        assert_eq!(downsample_pairs(s.clone(), 10), s);
        assert_eq!(downsample_pairs(s.clone(), 4), s);
    }

    #[test]
    fn downsample_pairs_preserves_endpoints_and_caps_length() {
        let s: Vec<(i64, f64)> = (0..1000).map(|i| (i as i64, i as f64)).collect();
        let out = downsample_pairs(s, 240);
        assert_eq!(out.len(), 240);
        assert_eq!(out[0], (0, 0.0));
        assert_eq!(out[239], (999, 999.0));
    }

    #[test]
    fn downsample_pairs_handles_empty_and_zero_max() {
        assert_eq!(
            downsample_pairs(Vec::new(), 100),
            Vec::<(i64, f64)>::new()
        );
        let s: Vec<(i64, f64)> = vec![(1, 1.0), (2, 2.0), (3, 3.0)];
        assert_eq!(downsample_pairs(s.clone(), 0), s);
    }

}
