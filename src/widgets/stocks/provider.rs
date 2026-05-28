// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// Time window selectable by the user from the stocks graph toggle bar.
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

    /// (interval, range) query parameters for Yahoo's v8/chart endpoint.
    /// Longer windows use coarser intervals so the series stays a sane size.
    /// Yahoo doesn't expose a native 3-year range; we request 5y and trim
    /// client-side after the response comes back.
    ///
    /// `pub(crate)` so sibling widgets that talk to the same endpoint (forex)
    /// can reuse the period→params mapping verbatim.
    pub(crate) fn yahoo_params(self) -> (&'static str, &'static str) {
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

/// Snapshot of a single ticker, derived from Yahoo Finance's v8/chart endpoint.
/// Some fields the spec calls out (P/E, EPS, market cap, yield) require a
/// separate quoteSummary call and are filled in only when available — keep
/// them `Option` so the renderer can show `—` cleanly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StockQuote {
    pub symbol: String,
    pub short_name: String,
    pub price: f64,
    pub previous_close: f64,
    pub day_high: Option<f64>,
    pub day_low: Option<f64>,
    pub fifty_two_week_high: Option<f64>,
    pub fifty_two_week_low: Option<f64>,
    pub volume: Option<u64>,
    pub avg_volume: Option<u64>,
    pub market_cap: Option<u64>,
    pub shares_outstanding: Option<u64>,
    pub pe_ratio: Option<f64>,
    pub eps: Option<f64>,
    /// Dividend yield as a fraction (e.g. 0.0052 = 0.52%). Multiply by 100
    /// for the display string.
    pub dividend_yield: Option<f64>,
    pub beta: Option<f64>,
    pub currency: Option<String>,
    /// Closing-price intraday series for graphing (5-minute bars by default).
    pub intraday: Vec<f64>,
    /// Unix-second timestamps parallel to `intraday`. Populated only on 1D
    /// (where bars are timestamped); empty on longer periods. The renderer
    /// uses these to draw vertical cutoff lines at the pre/regular and
    /// regular/post session boundaries on the 1D chart.
    #[serde(default)]
    pub intraday_timestamps: Vec<i64>,
    /// Unix-second bounds of the *current/upcoming* regular trading
    /// session, taken from `meta.currentTradingPeriod.regular`. During
    /// the regular session this is today's open/close; outside it
    /// (overnight, weekend, pre-market) this is the next scheduled
    /// session. `None` when Yahoo didn't return the field (e.g. indices
    /// on long ranges) or when the period isn't 1D.
    #[serde(default)]
    pub regular_session_start_ts: Option<i64>,
    #[serde(default)]
    pub regular_session_end_ts: Option<i64>,
    /// Unix-second bounds of the *most recent completed* regular
    /// session — taken from the latest entry in
    /// `meta.tradingPeriods.regular` whose start is strictly before
    /// `regular_session_start_ts`. The 1D chart filter falls back to
    /// these bounds when today's regular session has no bars yet
    /// (overnight gap, pre-market, weekend) so the graph shows the
    /// previous trading day's trend instead of empty space or
    /// pre-market noise. `None` when Yahoo's `tradingPeriods` block
    /// omits a prior period (rare for equities).
    #[serde(default)]
    pub previous_session_start_ts: Option<i64>,
    #[serde(default)]
    pub previous_session_end_ts: Option<i64>,
    #[allow(dead_code)] // surfaced in status bar / staleness label in a follow-up.
    pub fetched_at: chrono::DateTime<chrono::Local>,
    /// Extended-hours quote fields straight from `chart.meta`. Populated for
    /// equities outside regular trading hours; absent for indices and during
    /// the regular session. The graph header renders these on the 1D view as
    /// the `AH` / `PRE` segment.
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
    /// Yahoo's `marketState` — "REGULAR", "PRE", "PREPRE", "POST", "POSTPOST",
    /// "CLOSED". Used to pick which extended-hours session is most recent.
    #[serde(default)]
    pub market_state: Option<String>,
}

impl StockQuote {
    pub fn change(&self) -> f64 {
        self.price - self.previous_close
    }

    pub fn change_pct(&self) -> f64 {
        if self.previous_close == 0.0 {
            0.0
        } else {
            (self.price - self.previous_close) / self.previous_close * 100.0
        }
    }
}

#[derive(Clone)]
pub struct YahooFinanceProvider {
    client: reqwest::Client,
    base_url: String,
    /// query2 is the host Yahoo uses for the auth-gated quoteSummary endpoint.
    summary_base_url: String,
    /// Lazily-fetched crumb. The same crumb is paired with the cookie jar on
    /// the `client` for the lifetime of the provider; if the server rejects
    /// it (401) we invalidate and re-fetch.
    crumb: Arc<Mutex<Option<String>>>,
}

impl YahooFinanceProvider {
    pub fn new() -> Result<Self> {
        // Yahoo blocks generic user-agents on the chart endpoint, so we send
        // a browser-shaped UA. Identifies as glint underneath for transparency
        // in their server logs. cookie_store(true) lets us hold the B / A1
        // session cookies needed for the quoteSummary auth flow.
        let client = reqwest::Client::builder()
            .user_agent(concat!(
                "Mozilla/5.0 (compatible; glint-tui/",
                env!("CARGO_PKG_VERSION"),
                ")"
            ))
            .timeout(std::time::Duration::from_secs(10))
            .cookie_store(true)
            .build()
            .context("failed to build Yahoo Finance HTTP client")?;
        Ok(Self {
            client,
            base_url: "https://query1.finance.yahoo.com".into(),
            summary_base_url: "https://query2.finance.yahoo.com".into(),
            crumb: Arc::new(Mutex::new(None)),
        })
    }

    /// Ensure we have a valid crumb cached. First call seeds session cookies
    /// by hitting fc.yahoo.com, then asks v1/test/getcrumb for the token.
    async fn ensure_crumb(&self) -> Result<String> {
        let mut guard = self.crumb.lock().await;
        if let Some(c) = guard.as_ref() {
            return Ok(c.clone());
        }
        // Seed the B / A1 cookies. Errors here are not fatal — getcrumb may
        // succeed anyway if cookies are already in the jar.
        let _ = self.client.get("https://fc.yahoo.com/").send().await;
        let crumb = self
            .client
            .get(format!("{}/v1/test/getcrumb", self.summary_base_url))
            .send()
            .await
            .context("getcrumb request failed")?
            .text()
            .await
            .context("getcrumb response unreadable")?
            .trim()
            .to_string();
        if crumb.is_empty() || crumb.contains('<') {
            anyhow::bail!("Yahoo returned an empty or invalid crumb");
        }
        *guard = Some(crumb.clone());
        Ok(crumb)
    }

    async fn invalidate_crumb(&self) {
        let mut guard = self.crumb.lock().await;
        *guard = None;
    }

    pub async fn fetch_quote(&self, symbol: &str, period: Period) -> Result<StockQuote> {
        // Chart and summary fetched in parallel — summary failures (common
        // for indices and after crumb expiry) don't fail the whole quote.
        let (chart_res, summary_res) =
            tokio::join!(self.fetch_chart(symbol, period), self.fetch_summary(symbol));
        let mut quote = chart_res?;
        match summary_res {
            Ok(summary) => {
                if summary.market_cap.is_some() {
                    quote.market_cap = summary.market_cap;
                }
                if summary.shares_outstanding.is_some() {
                    quote.shares_outstanding = summary.shares_outstanding;
                }
                quote.pe_ratio = summary.pe_ratio;
                quote.eps = summary.eps;
                quote.dividend_yield = summary.dividend_yield;
                quote.beta = summary.beta;
                // Authoritative "yesterday's regular close" comes from the
                // `price` module — `meta.chartPreviousClose` on the chart
                // endpoint is the close before the chart's *range* starts
                // (e.g. ~1y ago on a 1Y fetch), so it can't be trusted as
                // the 1D change baseline. When summary returns the field,
                // it always wins.
                if let Some(prev) = summary.regular_market_previous_close {
                    quote.previous_close = prev;
                }
            }
            Err(err) => {
                tracing::debug!(symbol = %symbol, error = %err, "quoteSummary fetch failed");
            }
        }
        Ok(quote)
    }

    async fn fetch_chart(&self, symbol: &str, period: Period) -> Result<StockQuote> {
        let (interval, range) = period.yahoo_params();
        // On 1D we ask Yahoo to include pre-market and post-market bars so
        // the graph can render the full 04:00–20:00 ET window. Longer
        // ranges don't benefit and the param just bloats the response.
        let prepost_q = if matches!(period, Period::Day) {
            "&includePrePost=true"
        } else {
            ""
        };
        let url = format!(
            "{base}/v8/finance/chart/{sym}?interval={interval}&range={range}{prepost_q}",
            base = self.base_url.trim_end_matches('/'),
            sym = urlencoding::encode(symbol)
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {symbol} failed"))?
            .error_for_status()
            .with_context(|| format!("{symbol} returned non-2xx"))?
            .json::<ChartResponse>()
            .await
            .with_context(|| format!("failed to deserialize {symbol} response"))?;

        if let Some(err) = resp.chart.error {
            anyhow::bail!("Yahoo returned error for {symbol}: {}", err.description);
        }
        let result = resp
            .chart
            .result
            .and_then(|mut r| r.pop())
            .with_context(|| format!("Yahoo returned no result for {symbol}"))?;
        let meta = result.meta;
        // Pair each close-price bar with its source timestamp before dropping
        // null bars. Yahoo returns `null` close values for many extended-hours
        // bars (low volume); we collapse to "(ts, price)" only for finite
        // values so timestamps line up with the rendered series.
        let timestamps_raw: Vec<i64> = result.timestamp.unwrap_or_default();
        let closes_raw: Vec<Option<f64>> = result
            .indicators
            .and_then(|i| i.quote.into_iter().next())
            .map(|q| q.close)
            .unwrap_or_default();
        let mut paired: Vec<(i64, f64)> = timestamps_raw
            .iter()
            .copied()
            .zip(closes_raw.into_iter())
            .filter_map(|(ts, v)| v.filter(|x| x.is_finite()).map(|v| (ts, v)))
            .collect();

        // 3-year approximation: Yahoo gave us 5y of daily bars, keep the last
        // ~60% (~3 of the 5 years).
        if matches!(period, Period::ThreeYear) {
            let keep = (paired.len() * 3) / 5;
            let skip = paired.len().saturating_sub(keep);
            paired = paired.into_iter().skip(skip).collect();
        }

        // Downsample multi-year series before they hit memory + disk cache.
        // The chart renderer interpolates the series across `trace_w` columns
        // (typically 40–160), so 240 source points is well above the
        // visible resolution. 5Y/10Y daily bars compress 6–12× with no
        // perceptible chart-quality loss. We downsample (timestamp, value)
        // pairs together so the parallel arrays stay aligned.
        let paired = downsample_pairs_to_max(paired, 240);
        let intraday: Vec<f64> = paired.iter().map(|(_, v)| *v).collect();
        let intraday_timestamps: Vec<i64> = paired.iter().map(|(ts, _)| *ts).collect();

        let (regular_session_start_ts, regular_session_end_ts) = match (
            meta.current_trading_period.as_ref().and_then(|p| p.regular.as_ref()),
            matches!(period, Period::Day),
        ) {
            // Session bounds are only meaningful for 1D — they describe
            // today's regular trading window, which the renderer uses to
            // filter the chart to regular-session bars (extended-hours
            // bars stay in `intraday` so the AH/PRE header can derive
            // from them, but the chart paints regular session only).
            (Some(reg), true) => (reg.start, reg.end),
            _ => (None, None),
        };

        // Most-recent completed regular session — used by the 1D chart
        // filter to fall back to yesterday's trend during overnight
        // gap / pre-market / weekend windows. Pick the
        // `tradingPeriods.regular` entry with the latest `start` that's
        // strictly before `regular_session_start_ts` (the current/
        // upcoming session). Strictly-less guards against a corner
        // case where Yahoo's `tradingPeriods` includes the current
        // session — we want the one *before* it.
        let (previous_session_start_ts, previous_session_end_ts) = match (
            meta.trading_periods.as_ref(),
            regular_session_start_ts,
            matches!(period, Period::Day),
        ) {
            (Some(tp), Some(current_start), true) => {
                let prev = tp
                    .regular
                    .iter()
                    .flatten()
                    .filter_map(|p| match (p.start, p.end) {
                        (Some(s), Some(e)) if s < current_start => Some((s, e)),
                        _ => None,
                    })
                    .max_by_key(|(s, _)| *s);
                match prev {
                    Some((s, e)) => (Some(s), Some(e)),
                    None => (None, None),
                }
            }
            _ => (None, None),
        };

        Ok(StockQuote {
            symbol: meta.symbol.unwrap_or_else(|| symbol.to_string()),
            short_name: meta
                .short_name
                .or(meta.long_name)
                .unwrap_or_else(|| symbol.to_string()),
            price: meta.regular_market_price.unwrap_or(0.0),
            previous_close: meta
                .chart_previous_close
                .or(meta.previous_close)
                .unwrap_or(meta.regular_market_price.unwrap_or(0.0)),
            day_high: meta.regular_market_day_high,
            day_low: meta.regular_market_day_low,
            fifty_two_week_high: meta.fifty_two_week_high,
            fifty_two_week_low: meta.fifty_two_week_low,
            volume: meta.regular_market_volume,
            avg_volume: meta.average_daily_volume_10_day,
            market_cap: meta.market_cap,
            // These come from v10/quoteSummary, which fetch_quote merges in
            // after the chart resolves.
            shares_outstanding: None,
            pe_ratio: None,
            eps: None,
            dividend_yield: None,
            beta: None,
            currency: meta.currency,
            intraday,
            intraday_timestamps,
            regular_session_start_ts,
            regular_session_end_ts,
            previous_session_start_ts,
            previous_session_end_ts,
            fetched_at: chrono::Local::now(),
            post_market_price: meta.post_market_price,
            post_market_change: meta.post_market_change,
            post_market_change_percent: meta.post_market_change_percent,
            pre_market_price: meta.pre_market_price,
            pre_market_change: meta.pre_market_change,
            pre_market_change_percent: meta.pre_market_change_percent,
            market_state: meta.market_state,
        })
    }
}

impl YahooFinanceProvider {
    /// Resolve a free-form query (company name or ticker) to a Yahoo symbol.
    /// Uses the public `/v1/finance/search` endpoint which doesn't need a
    /// crumb. Picks the highest-score EQUITY hit, falling back to the top
    /// non-equity hit (so ETFs / indices work too).
    pub async fn search(&self, query: &str) -> Result<String> {
        let url = format!(
            "{base}/v1/finance/search?q={q}",
            base = self.summary_base_url.trim_end_matches('/'),
            q = urlencoding::encode(query),
        );
        let resp: SearchResponse = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("search {query:?} failed"))?
            .error_for_status()
            .with_context(|| format!("search {query:?} returned non-2xx"))?
            .json()
            .await
            .with_context(|| format!("failed to deserialize search for {query:?}"))?;
        // Prefer equities; fall back to whatever's first.
        let equity = resp
            .quotes
            .iter()
            .find(|q| q.quote_type.as_deref() == Some("EQUITY") && q.symbol.is_some());
        let pick = equity.or_else(|| resp.quotes.iter().find(|q| q.symbol.is_some()));
        let symbol = pick
            .and_then(|q| q.symbol.clone())
            .with_context(|| format!("no candidates found for {query:?}"))?;
        Ok(symbol)
    }

    /// Pulls the fundamentals modules from v10/quoteSummary. On a 401 we
    /// invalidate the cached crumb so the next call re-authenticates.
    async fn fetch_summary(&self, symbol: &str) -> Result<SummaryFields> {
        let crumb = self.ensure_crumb().await?;
        let url = format!(
            "{base}/v10/finance/quoteSummary/{sym}?modules=summaryDetail,defaultKeyStatistics,price&crumb={crumb}",
            base = self.summary_base_url.trim_end_matches('/'),
            sym = urlencoding::encode(symbol),
            crumb = urlencoding::encode(&crumb),
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("quoteSummary GET {symbol} failed"))?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            self.invalidate_crumb().await;
            anyhow::bail!("quoteSummary returned 401 (crumb invalidated)");
        }
        let body: SummaryResponse = resp
            .error_for_status()
            .with_context(|| format!("quoteSummary {symbol} returned non-2xx"))?
            .json()
            .await
            .with_context(|| format!("failed to deserialize quoteSummary for {symbol}"))?;
        if let Some(err) = body.quote_summary.error {
            anyhow::bail!("Yahoo quoteSummary error for {symbol}: {}", err.description);
        }
        let result = body
            .quote_summary
            .result
            .and_then(|mut r| r.pop())
            .with_context(|| format!("quoteSummary returned no result for {symbol}"))?;
        let detail = result.summary_detail.as_ref();
        let stats = result.default_key_statistics.as_ref();
        let price = result.price.as_ref();
        Ok(SummaryFields {
            market_cap: detail
                .and_then(|d| d.market_cap.as_ref())
                .and_then(|v| v.raw_u64()),
            shares_outstanding: stats
                .and_then(|s| s.shares_outstanding.as_ref())
                .and_then(|v| v.raw_u64()),
            pe_ratio: detail
                .and_then(|d| d.trailing_pe.as_ref())
                .and_then(|v| v.raw),
            eps: stats
                .and_then(|s| s.trailing_eps.as_ref())
                .and_then(|v| v.raw),
            dividend_yield: detail
                .and_then(|d| d.dividend_yield.as_ref())
                .and_then(|v| v.raw),
            beta: stats.and_then(|s| s.beta.as_ref()).and_then(|v| v.raw),
            regular_market_previous_close: price
                .and_then(|p| p.regular_market_previous_close.as_ref())
                .and_then(|v| v.raw),
        })
    }
}

/// What we extract from one quoteSummary response. All fields optional —
/// indices and many ETFs don't have most of these populated.
#[derive(Debug, Default, Clone)]
struct SummaryFields {
    market_cap: Option<u64>,
    shares_outstanding: Option<u64>,
    pe_ratio: Option<f64>,
    eps: Option<f64>,
    dividend_yield: Option<f64>,
    beta: Option<f64>,
    /// Yesterday's regular-session close. Authoritative for the 1D
    /// change baseline — the chart endpoint's `chartPreviousClose` is the
    /// close before the chart's *range* starts, not the previous regular
    /// trading day, so it's unreliable on non-1D fetches.
    regular_market_previous_close: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct SummaryResponse {
    #[serde(rename = "quoteSummary")]
    quote_summary: SummaryBody,
}

#[derive(Debug, Deserialize)]
struct SummaryBody {
    #[serde(default)]
    result: Option<Vec<SummaryResult>>,
    #[serde(default)]
    error: Option<ChartError>,
}

#[derive(Debug, Deserialize)]
struct SummaryResult {
    #[serde(rename = "summaryDetail", default)]
    summary_detail: Option<SummaryDetail>,
    #[serde(rename = "defaultKeyStatistics", default)]
    default_key_statistics: Option<DefaultKeyStatistics>,
    #[serde(default)]
    price: Option<Price>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SummaryDetail {
    #[serde(default)]
    market_cap: Option<RawF64>,
    #[serde(default)]
    trailing_pe: Option<RawF64>,
    #[serde(default)]
    dividend_yield: Option<RawF64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DefaultKeyStatistics {
    #[serde(default)]
    shares_outstanding: Option<RawF64>,
    #[serde(default)]
    trailing_eps: Option<RawF64>,
    #[serde(default)]
    beta: Option<RawF64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Price {
    #[serde(default)]
    regular_market_previous_close: Option<RawF64>,
}

/// Yahoo's "{raw, fmt, longFmt}" wrapper. Empty objects ({}) also appear when
/// a field is unknown, so all fields are optional.
#[derive(Debug, Deserialize)]
struct RawF64 {
    #[serde(default)]
    raw: Option<f64>,
}

impl RawF64 {
    fn raw_u64(&self) -> Option<u64> {
        self.raw
            .filter(|v| v.is_finite() && *v >= 0.0)
            .map(|v| v as u64)
    }
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    quotes: Vec<SearchQuote>,
}

#[derive(Debug, Deserialize)]
struct SearchQuote {
    #[serde(default)]
    symbol: Option<String>,
    #[serde(rename = "quoteType", default)]
    quote_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChartResponse {
    chart: ChartBody,
}

#[derive(Debug, Deserialize)]
struct ChartBody {
    #[serde(default)]
    result: Option<Vec<ChartResult>>,
    #[serde(default)]
    error: Option<ChartError>,
}

#[derive(Debug, Deserialize)]
struct ChartError {
    #[serde(default)]
    description: String,
}

#[derive(Debug, Deserialize)]
struct ChartResult {
    meta: ChartMeta,
    #[serde(default)]
    timestamp: Option<Vec<i64>>,
    #[serde(default)]
    indicators: Option<ChartIndicators>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChartMeta {
    #[serde(default)]
    symbol: Option<String>,
    #[serde(default)]
    short_name: Option<String>,
    #[serde(default)]
    long_name: Option<String>,
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    regular_market_price: Option<f64>,
    #[serde(default)]
    previous_close: Option<f64>,
    #[serde(default)]
    chart_previous_close: Option<f64>,
    #[serde(default)]
    regular_market_day_high: Option<f64>,
    #[serde(default)]
    regular_market_day_low: Option<f64>,
    #[serde(default)]
    fifty_two_week_high: Option<f64>,
    #[serde(default)]
    fifty_two_week_low: Option<f64>,
    #[serde(default)]
    regular_market_volume: Option<u64>,
    #[serde(default)]
    average_daily_volume_10_day: Option<u64>,
    #[serde(default)]
    market_cap: Option<u64>,
    #[serde(default)]
    post_market_price: Option<f64>,
    #[serde(default)]
    post_market_change: Option<f64>,
    #[serde(default)]
    post_market_change_percent: Option<f64>,
    #[serde(default)]
    pre_market_price: Option<f64>,
    #[serde(default)]
    pre_market_change: Option<f64>,
    #[serde(default)]
    pre_market_change_percent: Option<f64>,
    #[serde(default)]
    market_state: Option<String>,
    #[serde(default)]
    current_trading_period: Option<CurrentTradingPeriod>,
    /// Periods the chart's bars actually cover (one entry per trading
    /// day, inner array per session within a day — usually 1). We pick
    /// the most-recent-completed entry as the previous-session bounds.
    #[serde(default)]
    trading_periods: Option<TradingPeriods>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CurrentTradingPeriod {
    #[serde(default)]
    regular: Option<TradingPeriod>,
}

#[derive(Debug, Deserialize)]
struct TradingPeriods {
    #[serde(default)]
    regular: Vec<Vec<TradingPeriod>>,
}

#[derive(Debug, Deserialize)]
struct TradingPeriod {
    #[serde(default)]
    start: Option<i64>,
    #[serde(default)]
    end: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ChartIndicators {
    #[serde(default)]
    quote: Vec<QuoteBars>,
}

#[derive(Debug, Deserialize)]
struct QuoteBars {
    #[serde(default)]
    close: Vec<Option<f64>>,
}

/// Trim `(timestamp, value)` pairs to at most `max` evenly-spaced points,
/// preserving the first and last samples. Used to keep multi-year daily
/// series from holding thousands of points in memory + disk cache when
/// the chart can only show ~200 columns at the widest. Timestamps follow
/// values through the downsample so the renderer can still find the
/// column position of a specific date.
pub(super) fn downsample_pairs_to_max(pairs: Vec<(i64, f64)>, max: usize) -> Vec<(i64, f64)> {
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

    #[test]
    fn change_and_pct_use_previous_close() {
        let q = StockQuote {
            symbol: "AAPL".into(),
            short_name: "Apple".into(),
            price: 200.0,
            previous_close: 196.0,
            day_high: None,
            day_low: None,
            fifty_two_week_high: None,
            fifty_two_week_low: None,
            volume: None,
            avg_volume: None,
            market_cap: None,
            shares_outstanding: None,
            pe_ratio: None,
            eps: None,
            dividend_yield: None,
            beta: None,
            currency: None,
            intraday: vec![],
            intraday_timestamps: vec![],
            regular_session_start_ts: None,
            regular_session_end_ts: None,
            previous_session_start_ts: None,
            previous_session_end_ts: None,
            fetched_at: chrono::Local::now(),
            post_market_price: None,
            post_market_change: None,
            post_market_change_percent: None,
            pre_market_price: None,
            pre_market_change: None,
            pre_market_change_percent: None,
            market_state: None,
        };
        assert!((q.change() - 4.0).abs() < 1e-9);
        assert!((q.change_pct() - 2.040_816).abs() < 1e-3);
    }

    #[test]
    fn downsample_pairs_returns_input_when_already_under_cap() {
        let s: Vec<(i64, f64)> =
            vec![(100, 1.0), (200, 2.0), (300, 3.0), (400, 4.0)];
        assert_eq!(downsample_pairs_to_max(s.clone(), 10), s);
        assert_eq!(downsample_pairs_to_max(s.clone(), 4), s);
    }

    #[test]
    fn downsample_pairs_preserves_endpoints_and_caps_length() {
        let s: Vec<(i64, f64)> = (0..1000).map(|i| (i as i64, i as f64)).collect();
        let out = downsample_pairs_to_max(s, 240);
        assert_eq!(out.len(), 240);
        assert_eq!(out[0], (0, 0.0));
        assert_eq!(out[239], (999, 999.0));
    }

    #[test]
    fn downsample_pairs_handles_empty_and_zero_max() {
        assert_eq!(
            downsample_pairs_to_max(Vec::new(), 100),
            Vec::<(i64, f64)>::new()
        );
        let s: Vec<(i64, f64)> = vec![(1, 1.0), (2, 2.0), (3, 3.0)];
        assert_eq!(downsample_pairs_to_max(s.clone(), 0), s);
    }

    #[test]
    fn change_pct_handles_zero_previous_close_safely() {
        let q = StockQuote {
            symbol: "TEST".into(),
            short_name: "Test".into(),
            price: 100.0,
            previous_close: 0.0,
            day_high: None,
            day_low: None,
            fifty_two_week_high: None,
            fifty_two_week_low: None,
            volume: None,
            avg_volume: None,
            market_cap: None,
            shares_outstanding: None,
            pe_ratio: None,
            eps: None,
            dividend_yield: None,
            beta: None,
            currency: None,
            intraday: vec![],
            intraday_timestamps: vec![],
            regular_session_start_ts: None,
            regular_session_end_ts: None,
            previous_session_start_ts: None,
            previous_session_end_ts: None,
            fetched_at: chrono::Local::now(),
            post_market_price: None,
            post_market_change: None,
            post_market_change_percent: None,
            pre_market_price: None,
            pre_market_change: None,
            pre_market_change_percent: None,
            market_state: None,
        };
        assert_eq!(q.change_pct(), 0.0);
    }
}
