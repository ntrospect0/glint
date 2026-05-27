//! Yahoo Finance adapter for foreign-exchange pairs.
//!
//! Reuses Yahoo's v8/chart endpoint via the same symbol convention as
//! Stocks but formatted as `{BASE}{QUOTE}=X` (e.g. `EURUSD=X` for the
//! euro/dollar rate, where the price = 1 EUR in USD). Periods, intervals
//! and ranges piggyback on the Stocks `Period` enum so the toggle bar,
//! keybindings, and the chart x-axis can be shared verbatim.
//!
//! Same caveats as Stocks: no API key, no rate-limit budget, fetches
//! are silent on failure (rendered as `err`), the cache provides the
//! fallback view.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// Reuse the Period enum + yahoo_params mapping that Stocks already
// exposes — same interval/range query shapes for forex symbols.
pub use crate::widgets::stocks::provider::Period;

/// Snapshot of a single FX pair, derived from Yahoo Finance. Mirrors
/// the Stocks `StockQuote` shape but trimmed to the fields that actually
/// apply to currency rates (no market cap / PE / dividend / etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForexQuote {
    /// Yahoo symbol — `{BASE}{QUOTE}=X`.
    pub symbol: String,
    /// ISO-4217 base currency code (the "1 unit of" side).
    pub base: String,
    /// ISO-4217 quote currency code (the "in" side).
    pub quote: String,
    /// Current rate: 1 base = `price` quote.
    pub price: f64,
    pub previous_close: f64,
    pub day_high: Option<f64>,
    pub day_low: Option<f64>,
    pub fifty_two_week_high: Option<f64>,
    pub fifty_two_week_low: Option<f64>,
    /// Closing-price series for the active period, used for graph + 30d
    /// rolling volatility + 1y change %.
    pub series: Vec<f64>,
    pub fetched_at: chrono::DateTime<chrono::Local>,
}

impl ForexQuote {
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

/// Yahoo Finance forex provider. Internally wraps a reqwest client
/// identical to the Stocks provider's — same browser-shaped UA, same
/// timeouts, same v8/chart endpoint. No quoteSummary call needed since
/// FX pairs don't expose the company-fundamentals modules.
#[derive(Clone)]
pub struct YahooForexProvider {
    client: reqwest::Client,
    base_url: String,
}

impl YahooForexProvider {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!(
                "Mozilla/5.0 (compatible; glint-tui/",
                env!("CARGO_PKG_VERSION"),
                ")"
            ))
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .context("failed to build Yahoo Forex HTTP client")?;
        Ok(Self {
            client,
            base_url: "https://query1.finance.yahoo.com".into(),
        })
    }

    /// Format `(BASE, QUOTE)` into Yahoo's forex symbol convention.
    /// e.g. `("USD", "EUR")` → `"USDEUR=X"` (price = 1 USD in EUR).
    pub fn symbol_for(base: &str, quote: &str) -> String {
        format!("{}{}=X", base.to_ascii_uppercase(), quote.to_ascii_uppercase())
    }

    /// Fetch a single FX pair quote, trying the direct Yahoo symbol
    /// first and falling back to a USD-pivoted computation when the
    /// direct symbol 404s. Yahoo only ships a subset of cross-pairs
    /// (most exotic-vs-exotic pairs return non-2xx); USD-paired
    /// symbols are universally available, so we can synthesize any
    /// pair as `USD{quote}=X / USD{base}=X`.
    pub async fn fetch_quote(&self, base: &str, quote: &str, period: Period) -> Result<ForexQuote> {
        let base_u = base.to_ascii_uppercase();
        let quote_u = quote.to_ascii_uppercase();
        match self.fetch_direct(&base_u, &quote_u, period).await {
            Ok(q) => Ok(q),
            Err(direct_err) => {
                // No pivot can help when one side is already USD —
                // those symbols are first-class on Yahoo. Surface the
                // original error so callers see the real reason.
                if base_u == "USD" || quote_u == "USD" {
                    return Err(direct_err);
                }
                tracing::debug!(
                    pair = %format!("{base_u}{quote_u}=X"),
                    error = %direct_err,
                    "direct FX fetch failed; falling back to USD pivot"
                );
                self.fetch_via_usd_pivot(&base_u, &quote_u, period).await
            }
        }
    }

    /// Direct Yahoo `{base}{quote}=X` fetch. Errors when Yahoo doesn't
    /// carry the pair (404 / 422 etc.) — handled at the `fetch_quote`
    /// layer above.
    async fn fetch_direct(&self, base: &str, quote: &str, period: Period) -> Result<ForexQuote> {
        let symbol = Self::symbol_for(base, quote);
        let (interval, range) = period.yahoo_params();
        let url = format!(
            "{base_url}/v8/finance/chart/{sym}?interval={interval}&range={range}",
            base_url = self.base_url.trim_end_matches('/'),
            sym = urlencoding::encode(&symbol)
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
        let series: Vec<f64> = result
            .indicators
            .and_then(|i| i.quote.into_iter().next())
            .map(|q| q.close.into_iter().flatten().collect())
            .unwrap_or_default();

        // Same 3-year trim trick Stocks uses: Yahoo doesn't ship a native
        // 3y range so we ask for 5y of daily bars and slice to ~the last
        // 3y client-side.
        let series: Vec<f64> = if matches!(period, Period::ThreeYear) {
            let keep = (series.len() * 3) / 5;
            let skip = series.len().saturating_sub(keep);
            series.into_iter().skip(skip).collect()
        } else {
            series
        };

        // Downsample before the series hits memory + disk cache. 240 is well
        // above any TUI pane width; multi-year daily traces compress 6–12×
        // with no perceptible chart-quality loss. USD-pivot synthesis (the
        // other producer of forex series) consumes already-downsampled legs,
        // so this is the single chokepoint.
        let series = downsample_to_max(series, 240);

        Ok(ForexQuote {
            symbol: meta.symbol.unwrap_or(symbol),
            base: base.to_string(),
            quote: quote.to_string(),
            price: meta.regular_market_price.unwrap_or(0.0),
            previous_close: meta
                .chart_previous_close
                .or(meta.previous_close)
                .unwrap_or(meta.regular_market_price.unwrap_or(0.0)),
            day_high: meta.regular_market_day_high,
            day_low: meta.regular_market_day_low,
            fifty_two_week_high: meta.fifty_two_week_high,
            fifty_two_week_low: meta.fifty_two_week_low,
            series,
            fetched_at: chrono::Local::now(),
        })
    }

    /// Compute `{base}{quote}=X` synthetically when Yahoo doesn't ship
    /// the direct symbol. Math:
    /// `1 base = (USD/base)⁻¹ USD = USD/quote ÷ USD/base quote`.
    /// The historical series is element-wise divided so the graph
    /// looks identical to a direct fetch; day-high/low and 52-week
    /// extrema are left None because they don't compose linearly
    /// (the two underlying highs may not have occurred at the same
    /// time, so multiplying them would over- or under-estimate).
    async fn fetch_via_usd_pivot(
        &self,
        base: &str,
        quote: &str,
        period: Period,
    ) -> Result<ForexQuote> {
        // Fan out both USD-leg fetches in parallel so the pivot only
        // costs one round-trip's worth of latency.
        let (a_res, b_res) = tokio::join!(
            self.fetch_direct("USD", base, period),
            self.fetch_direct("USD", quote, period),
        );
        let usd_to_base = a_res.with_context(|| format!("USD-pivot leg USD{base}=X failed"))?;
        let usd_to_quote = b_res.with_context(|| format!("USD-pivot leg USD{quote}=X failed"))?;

        if usd_to_base.price <= 0.0 {
            anyhow::bail!("USD/{base} rate is zero; cannot pivot to {quote}");
        }
        let price = usd_to_quote.price / usd_to_base.price;
        let previous_close = if usd_to_base.previous_close > 0.0 {
            usd_to_quote.previous_close / usd_to_base.previous_close
        } else {
            price
        };

        // Element-wise series synthesis. The two legs come back with
        // identical periodicity (we asked for the same `period`), so
        // they line up by index. Bars where the denominator is zero
        // are dropped rather than producing NaN/inf glyphs in the
        // graph.
        let n = usd_to_base.series.len().min(usd_to_quote.series.len());
        let series: Vec<f64> = (0..n)
            .filter_map(|i| {
                let b = usd_to_base.series[i];
                let q = usd_to_quote.series[i];
                if b > 0.0 && q.is_finite() {
                    Some(q / b)
                } else {
                    None
                }
            })
            .collect();

        Ok(ForexQuote {
            symbol: Self::symbol_for(base, quote),
            base: base.to_string(),
            quote: quote.to_string(),
            price,
            previous_close,
            day_high: None,
            day_low: None,
            fifty_two_week_high: None,
            fifty_two_week_low: None,
            series,
            fetched_at: chrono::Local::now(),
        })
    }
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
    indicators: Option<ChartIndicators>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChartMeta {
    #[serde(default)]
    symbol: Option<String>,
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

/// Trim `series` to at most `max` evenly-spaced points, preserving the
/// first and last samples. Mirrors `stocks::provider::downsample_to_max`
/// — both modules sample Yahoo's daily-bar series and share the same
/// "TUI charts can't show more than ~200 columns" constraint.
fn downsample_to_max(series: Vec<f64>, max: usize) -> Vec<f64> {
    if max == 0 || series.len() <= max {
        return series;
    }
    let n = series.len();
    let mut out = Vec::with_capacity(max);
    for i in 0..max {
        let idx = (i * (n - 1)) / (max - 1);
        out.push(series[idx]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_for_uppercases_and_concatenates() {
        assert_eq!(YahooForexProvider::symbol_for("usd", "eur"), "USDEUR=X");
        assert_eq!(YahooForexProvider::symbol_for("EUR", "GBP"), "EURGBP=X");
        assert_eq!(YahooForexProvider::symbol_for("JpY", "kRw"), "JPYKRW=X");
    }

    #[test]
    fn downsample_preserves_endpoints_and_caps_length() {
        let s: Vec<f64> = (0..1200).map(|i| i as f64).collect();
        let out = downsample_to_max(s, 240);
        assert_eq!(out.len(), 240);
        assert_eq!(out[0], 0.0);
        assert_eq!(out[239], 1199.0);
    }

    #[test]
    fn downsample_returns_input_when_already_under_cap() {
        let s = vec![1.0, 2.0, 3.0];
        assert_eq!(downsample_to_max(s.clone(), 10), s);
    }

    #[test]
    fn change_and_pct_use_previous_close() {
        let q = ForexQuote {
            symbol: "USDEUR=X".into(),
            base: "USD".into(),
            quote: "EUR".into(),
            price: 0.9300,
            previous_close: 0.9200,
            day_high: None,
            day_low: None,
            fifty_two_week_high: None,
            fifty_two_week_low: None,
            series: vec![],
            fetched_at: chrono::Local::now(),
        };
        assert!((q.change() - 0.0100).abs() < 1e-9);
        assert!((q.change_pct() - 1.0870).abs() < 1e-3);
    }

    #[test]
    fn usd_pivot_math_divides_quote_leg_by_base_leg() {
        // The pivot computation is pure math — no HTTP needed. Drive
        // it directly so we pin the formula: 1 BRL = (USD→CAD) /
        // (USD→BRL) CAD. If USD/BRL = 5.00 and USD/CAD = 1.40, then
        // 1 BRL = 1.40 / 5.00 = 0.28 CAD.
        let usd_to_base = ForexQuote {
            symbol: "USDBRL=X".into(),
            base: "USD".into(),
            quote: "BRL".into(),
            price: 5.00,
            previous_close: 4.95,
            day_high: None,
            day_low: None,
            fifty_two_week_high: None,
            fifty_two_week_low: None,
            series: vec![5.00, 4.98, 5.02, 5.05],
            fetched_at: chrono::Local::now(),
        };
        let usd_to_quote = ForexQuote {
            symbol: "USDCAD=X".into(),
            base: "USD".into(),
            quote: "CAD".into(),
            price: 1.40,
            previous_close: 1.39,
            day_high: None,
            day_low: None,
            fifty_two_week_high: None,
            fifty_two_week_low: None,
            series: vec![1.40, 1.41, 1.39, 1.42],
            fetched_at: chrono::Local::now(),
        };
        let price = usd_to_quote.price / usd_to_base.price;
        let prev = usd_to_quote.previous_close / usd_to_base.previous_close;
        let series: Vec<f64> = (0..usd_to_base.series.len())
            .map(|i| usd_to_quote.series[i] / usd_to_base.series[i])
            .collect();
        assert!((price - 0.28).abs() < 1e-9);
        assert!((prev - 1.39 / 4.95).abs() < 1e-9);
        assert_eq!(series.len(), 4);
        assert!((series[0] - 1.40 / 5.00).abs() < 1e-9);
    }

    #[test]
    fn change_pct_handles_zero_previous_close_safely() {
        let q = ForexQuote {
            symbol: "TEST=X".into(),
            base: "AAA".into(),
            quote: "BBB".into(),
            price: 1.0,
            previous_close: 0.0,
            day_high: None,
            day_low: None,
            fifty_two_week_high: None,
            fifty_two_week_low: None,
            series: vec![],
            fetched_at: chrono::Local::now(),
        };
        assert_eq!(q.change_pct(), 0.0);
    }
}
