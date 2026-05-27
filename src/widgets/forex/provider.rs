// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

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

/// Tickers Yahoo serves on the crypto path (`{BASE}-{QUOTE}`) rather
/// than the forex path (`{BASE}{QUOTE}=X`). Used by `symbol_for` to
/// pick the right URL shape and by the widget's list renderer to
/// emit a `── Crypto ──` section header. Extend as needed — symbols
/// not in this set fall through to the forex path and 404 if Yahoo
/// doesn't actually carry them.
pub const CRYPTO_CODES: &[&str] = &[
    "BTC", "ETH", "SOL", "XRP", "ADA", "DOGE", "AVAX", "DOT", "LINK", "LTC",
    "MATIC", "TRX", "BCH", "BNB", "USDT", "USDC", "TON", "SUI", "ATOM", "NEAR",
];

/// `true` if `code` is in the [`CRYPTO_CODES`] table. Case-insensitive.
pub fn is_crypto(code: &str) -> bool {
    let up = code.to_ascii_uppercase();
    CRYPTO_CODES.iter().any(|c| **c == up)
}

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

    /// Format `(BASE, QUOTE)` into Yahoo's symbol convention. Forex
    /// pairs use the `{BASE}{QUOTE}=X` suffix; crypto pairs use the
    /// hyphenated `{BASE}-{QUOTE}` form. Either side being a known
    /// crypto code flips us to the crypto format, since Yahoo serves
    /// crypto-vs-fiat pairs (BTC-USD, ETH-EUR, …) on the latter path.
    pub fn symbol_for(base: &str, quote: &str) -> String {
        let b = base.to_ascii_uppercase();
        let q = quote.to_ascii_uppercase();
        if is_crypto(&b) || is_crypto(&q) {
            format!("{b}-{q}")
        } else {
            format!("{b}{q}=X")
        }
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

        // Yahoo only lists crypto pairs in the `{CRYPTO}-{FIAT}`
        // direction (e.g. `BTC-USD`), never the inverse. When the
        // user wants "1 USD in BTC" we therefore have to fetch
        // `BTC-USD` and invert the rate. Without this, USD-as-primary
        // + crypto-as-alternate returns `USD-BTC returned non-2xx`.
        if base_u == "USD" && is_crypto(&quote_u) {
            let listed = self.fetch_direct(&quote_u, "USD", period).await?;
            return Ok(invert_quote(listed, &base_u, &quote_u));
        }

        // {fiat}USD=X is patchy for exotic currencies (Yahoo lists
        // USDVND=X but 404s on VNDUSD=X). For any `base→USD` ask
        // where `base` is a non-USD fiat, fetch the reliable
        // `USD{base}=X` direction and invert. This is what makes
        // `:fx vnd` then `JPY→VND` (cross-pair) and `VND-as-primary`
        // (every row goes through `VND→x`) keep working.
        if quote_u == "USD" && base_u != "USD" && !is_crypto(&base_u) {
            return self.fetch_to_usd(&base_u, period).await;
        }

        // One side already USD (covers USD→fiat and crypto→USD):
        // Yahoo's native direction works directly, no inversion.
        if base_u == "USD" || quote_u == "USD" {
            return self.fetch_direct(&base_u, &quote_u, period).await;
        }

        // Cross pair → always pivot through USD. Uniform across
        // fiat-fiat, fiat-crypto, crypto-fiat, and crypto-crypto:
        // each leg is fetched via `fetch_to_usd` (which already
        // picks Yahoo's reliable direction per code type), then the
        // cross rate is `R(base, USD) / R(quote, USD)`.
        self.fetch_via_usd_pivot(&base_u, &quote_u, period).await
    }

    /// Fetch "1 unit of `code` expressed in USD" using whichever Yahoo
    /// direction is universally available:
    ///
    /// * **Fiat** (any non-USD ISO code) → fetch `USD{code}=X` and
    ///   invert. Yahoo carries every USD-quoted fiat pair, even for
    ///   exotic currencies like VND that don't have a reverse
    ///   `{code}USD=X` listing.
    /// * **Crypto** → fetch `{code}-USD` directly. Yahoo only lists
    ///   crypto in the crypto-first direction.
    ///
    /// Used by [`fetch_quote`] for any `→USD` request and by
    /// [`fetch_via_usd_pivot`] for both legs of a cross-pair.
    async fn fetch_to_usd(&self, code: &str, period: Period) -> Result<ForexQuote> {
        if is_crypto(code) {
            return self.fetch_direct(code, "USD", period).await;
        }
        let usd_to_code = self.fetch_direct("USD", code, period).await?;
        Ok(invert_quote(usd_to_code, code, "USD"))
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

    /// Compute `{base}-to-{quote}` by triangulating through USD. The
    /// math: each leg gives `R(x, USD)` — the price of 1 x in USD —
    /// fetched in Yahoo's natural direction (`xUSD=X` for fiat,
    /// `x-USD` for crypto). The cross rate is then
    /// `R(base, quote) = R(base, USD) / R(quote, USD)` (1 base buys
    /// R(base, USD) USD, which buys R(base, USD) / R(quote, USD) quote).
    /// Historical series are divided element-wise so the graph reads
    /// like a direct fetch. Day-high/low + 52-week extrema are left
    /// None — they don't compose linearly across the two legs.
    async fn fetch_via_usd_pivot(
        &self,
        base: &str,
        quote: &str,
        period: Period,
    ) -> Result<ForexQuote> {
        // Fan out both legs in parallel so the pivot only costs one
        // round-trip's worth of latency. `fetch_to_usd` picks Yahoo's
        // reliable direction per code type (USD{fiat}=X inverted,
        // {crypto}-USD direct) so exotic-fiat pivots like JPY→VND
        // don't fall over on the leg fetch.
        let (a_res, b_res) = tokio::join!(
            self.fetch_to_usd(base, period),
            self.fetch_to_usd(quote, period),
        );
        let base_to_usd = a_res
            .with_context(|| format!("USD-pivot leg {base}→USD failed"))?;
        let quote_to_usd = b_res
            .with_context(|| format!("USD-pivot leg {quote}→USD failed"))?;

        if quote_to_usd.price <= 0.0 {
            anyhow::bail!("{quote}→USD rate is zero; cannot pivot to {quote}");
        }
        let price = base_to_usd.price / quote_to_usd.price;
        let previous_close = if quote_to_usd.previous_close > 0.0 {
            base_to_usd.previous_close / quote_to_usd.previous_close
        } else {
            price
        };

        // Element-wise series synthesis. Both legs come back at the
        // same periodicity (same `period` arg), so indices align.
        // Bars where the denominator is zero are dropped rather than
        // producing NaN/inf glyphs in the graph.
        let n = base_to_usd.series.len().min(quote_to_usd.series.len());
        let series: Vec<f64> = (0..n)
            .filter_map(|i| {
                let b = base_to_usd.series[i];
                let q = quote_to_usd.series[i];
                if q > 0.0 && b.is_finite() {
                    Some(b / q)
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
/// Invert a `ForexQuote` so its rates reflect the swapped pair
/// direction. Used when Yahoo only lists one direction of a crypto
/// pair (`BTC-USD`) and the caller wanted the inverse (`USD-BTC`):
/// fetch the listed direction, then reciprocate every rate.
///
/// `day_high` / `day_low` swap because the *high* of `B in A` is
/// the reciprocal of the *low* of `A in B` — when A is most
/// expensive in B units, B is cheapest in A units, and vice versa.
/// Same for the 52-week extrema. Zero-or-negative inputs collapse
/// to 0 to avoid div-by-zero / NaN propagating into the graph.
fn invert_quote(q: ForexQuote, new_base: &str, new_quote: &str) -> ForexQuote {
    let inv = |x: f64| if x > 0.0 { 1.0 / x } else { 0.0 };
    ForexQuote {
        symbol: YahooForexProvider::symbol_for(new_base, new_quote),
        base: new_base.to_string(),
        quote: new_quote.to_string(),
        price: inv(q.price),
        previous_close: inv(q.previous_close),
        day_high: q.day_low.map(inv),
        day_low: q.day_high.map(inv),
        fifty_two_week_high: q.fifty_two_week_low.map(inv),
        fifty_two_week_low: q.fifty_two_week_high.map(inv),
        series: q.series.iter().map(|x| inv(*x)).collect(),
        fetched_at: q.fetched_at,
    }
}

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
    fn symbol_for_uses_crypto_format_when_either_side_is_crypto() {
        assert_eq!(YahooForexProvider::symbol_for("BTC", "USD"), "BTC-USD");
        assert_eq!(YahooForexProvider::symbol_for("USD", "ETH"), "USD-ETH");
        assert_eq!(YahooForexProvider::symbol_for("eth", "btc"), "ETH-BTC");
    }

    #[test]
    fn is_crypto_is_case_insensitive_and_matches_seed_set() {
        assert!(is_crypto("BTC"));
        assert!(is_crypto("btc"));
        assert!(is_crypto("Sol"));
        assert!(!is_crypto("USD"));
        assert!(!is_crypto("EUR"));
        assert!(!is_crypto("UNKNOWN"));
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

    /// Live Yahoo round-trip across a representative spread of pairs,
    /// including the exotic-fiat shapes that motivated the
    /// `fetch_to_usd` direction fix. Marked `#[ignore]` so CI / the
    /// default `cargo test` doesn't hit the network — run with
    /// `cargo test --ignored fetch_quote_works_across_random_pairs --
    /// --nocapture` for a manual sanity check.
    ///
    /// Covers each shape the dispatch in `fetch_quote` has a branch for:
    /// - USD→fiat (direct USDxxx=X)
    /// - USD→crypto (crypto-USD inverted)
    /// - fiat→USD (USDfiat=X inverted via `fetch_to_usd`)
    /// - crypto→USD (crypto-USD direct)
    /// - fiat-fiat cross (USD pivot, both legs via `fetch_to_usd`)
    /// - fiat-crypto cross (USD pivot, mixed legs)
    /// - crypto-crypto cross (USD pivot, both legs)
    ///
    /// Asserts each pair returns a finite, positive price. Doesn't
    /// pin numeric values (rates shift) — the load-bearing claim is
    /// that no leg 404s after the fix.
    #[tokio::test]
    #[ignore = "hits live Yahoo Finance; run with --ignored"]
    async fn fetch_quote_works_across_random_pairs() {
        let provider = YahooForexProvider::new().expect("provider build");
        let pairs: &[(&str, &str, &str)] = &[
            // USD-on-base, common fiat (was always fine).
            ("USD", "EUR", "USD→EUR — universal direct"),
            ("USD", "JPY", "USD→JPY — universal direct"),
            // USD-on-base, exotic fiat (was fine; sanity-check).
            ("USD", "VND", "USD→VND — universal direct, exotic"),
            ("USD", "IDR", "USD→IDR — universal direct, exotic"),
            // fiat→USD, exotic — this is the FIX TARGET. Previously
            // tried `{fiat}USD=X` and 404'd; now inverts USD{fiat}=X.
            ("VND", "USD", "VND→USD — exotic, inverted via fetch_to_usd"),
            ("IDR", "USD", "IDR→USD — exotic, inverted via fetch_to_usd"),
            ("NGN", "USD", "NGN→USD — exotic, inverted via fetch_to_usd"),
            ("PHP", "USD", "PHP→USD — exotic, inverted via fetch_to_usd"),
            // fiat→USD, common (already worked via direct fiatUSD=X
            // but should also work via the new inverted path).
            ("EUR", "USD", "EUR→USD — common, now also via fetch_to_usd"),
            ("JPY", "USD", "JPY→USD — common, now also via fetch_to_usd"),
            // fiat-fiat cross — pivots through USD via fetch_to_usd
            // on both legs. Mix of common and exotic legs.
            ("EUR", "JPY", "EUR→JPY — cross, both legs common"),
            ("GBP", "AUD", "GBP→AUD — cross, both legs common"),
            ("JPY", "VND", "JPY→VND — cross, one leg exotic (BUG 1)"),
            ("VND", "JPY", "VND→JPY — cross, primary leg exotic"),
            ("EUR", "VND", "EUR→VND — cross, one leg exotic"),
            ("VND", "EUR", "VND→EUR — cross, primary leg exotic (BUG 2)"),
            ("IDR", "ZAR", "IDR→ZAR — cross, both legs exotic"),
            ("KRW", "PHP", "KRW→PHP — cross, both legs exotic"),
            // Crypto on one side.
            ("BTC", "USD", "BTC→USD — native crypto direction"),
            ("USD", "BTC", "USD→BTC — crypto inverted"),
            ("ETH", "EUR", "ETH→EUR — crypto/fiat cross"),
            ("EUR", "BTC", "EUR→BTC — fiat/crypto cross"),
            // Crypto-crypto.
            ("BTC", "ETH", "BTC→ETH — crypto/crypto cross"),
        ];

        let mut failures: Vec<String> = Vec::new();
        for (base, quote, label) in pairs {
            match provider.fetch_quote(base, quote, Period::Day).await {
                Ok(q) => {
                    if !q.price.is_finite() || q.price <= 0.0 {
                        failures.push(format!(
                            "{label}: price not finite/positive ({})",
                            q.price
                        ));
                        eprintln!("  ✗ {label} → bad price {}", q.price);
                    } else {
                        eprintln!("  ✓ {label} → {:.6}", q.price);
                    }
                }
                Err(err) => {
                    failures.push(format!("{label}: {err:#}"));
                    eprintln!("  ✗ {label} → ERROR: {err:#}");
                }
            }
        }

        assert!(
            failures.is_empty(),
            "{} of {} pairs failed:\n  - {}",
            failures.len(),
            pairs.len(),
            failures.join("\n  - ")
        );
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
    fn invert_quote_reciprocates_rates_and_swaps_extrema() {
        // "1 BTC = 90,000 USD" inverts to "1 USD = 1/90,000 BTC".
        let listed = ForexQuote {
            symbol: "BTC-USD".into(),
            base: "BTC".into(),
            quote: "USD".into(),
            price: 90_000.0,
            previous_close: 88_000.0,
            day_high: Some(91_000.0),
            day_low: Some(89_000.0),
            fifty_two_week_high: Some(100_000.0),
            fifty_two_week_low: Some(50_000.0),
            series: vec![90_000.0, 89_000.0, 91_000.0],
            fetched_at: chrono::Local::now(),
        };
        let inverted = invert_quote(listed, "USD", "BTC");
        assert!((inverted.price - 1.0 / 90_000.0).abs() < 1e-12);
        assert!((inverted.previous_close - 1.0 / 88_000.0).abs() < 1e-12);
        // High of USD-in-BTC is reciprocal of low of BTC-in-USD.
        assert!((inverted.day_high.unwrap() - 1.0 / 89_000.0).abs() < 1e-12);
        assert!((inverted.day_low.unwrap() - 1.0 / 91_000.0).abs() < 1e-12);
        assert!((inverted.fifty_two_week_high.unwrap() - 1.0 / 50_000.0).abs() < 1e-12);
        assert!((inverted.fifty_two_week_low.unwrap() - 1.0 / 100_000.0).abs() < 1e-12);
        assert_eq!(inverted.series.len(), 3);
        assert!((inverted.series[0] - 1.0 / 90_000.0).abs() < 1e-12);
        // Identity restored when used to swap pair labels back.
        assert_eq!(inverted.base, "USD");
        assert_eq!(inverted.quote, "BTC");
        assert_eq!(inverted.symbol, "USD-BTC");
    }

    #[test]
    fn invert_quote_collapses_nonpositive_inputs_to_zero() {
        // After inversion, day_high comes from the listed quote's
        // day_low (and vice versa). Set day_low here so the inverted
        // day_high is non-None.
        let listed = ForexQuote {
            symbol: "BTC-USD".into(),
            base: "BTC".into(),
            quote: "USD".into(),
            price: 0.0,
            previous_close: -1.0,
            day_high: None,
            day_low: Some(0.0),
            fifty_two_week_high: None,
            fifty_two_week_low: None,
            series: vec![0.0, -1.0, 100.0],
            fetched_at: chrono::Local::now(),
        };
        let inv = invert_quote(listed, "USD", "BTC");
        assert_eq!(inv.price, 0.0);
        assert_eq!(inv.previous_close, 0.0);
        assert_eq!(inv.day_high, Some(0.0), "0 input → 0 inverted (no div-by-0)");
        assert_eq!(inv.day_low, None, "missing input stays None");
        assert_eq!(inv.series, vec![0.0, 0.0, 1.0 / 100.0]);
    }

    #[test]
    fn usd_pivot_math_divides_base_leg_by_quote_leg() {
        // Pure-math reproduction of `fetch_via_usd_pivot`:
        //   R(base, quote) = R(base, USD) / R(quote, USD)
        // Worked example: 1 EUR = 1.10 USD; 1 GBP = 1.27 USD;
        //   1 EUR = 1.10 / 1.27 ≈ 0.866 GBP.
        let base_to_usd = ForexQuote {
            symbol: "EURUSD=X".into(),
            base: "EUR".into(),
            quote: "USD".into(),
            price: 1.10,
            previous_close: 1.09,
            day_high: None,
            day_low: None,
            fifty_two_week_high: None,
            fifty_two_week_low: None,
            series: vec![1.10, 1.11, 1.09, 1.12],
            fetched_at: chrono::Local::now(),
        };
        let quote_to_usd = ForexQuote {
            symbol: "GBPUSD=X".into(),
            base: "GBP".into(),
            quote: "USD".into(),
            price: 1.27,
            previous_close: 1.26,
            day_high: None,
            day_low: None,
            fifty_two_week_high: None,
            fifty_two_week_low: None,
            series: vec![1.27, 1.28, 1.26, 1.29],
            fetched_at: chrono::Local::now(),
        };
        let price = base_to_usd.price / quote_to_usd.price;
        let prev = base_to_usd.previous_close / quote_to_usd.previous_close;
        let series: Vec<f64> = (0..base_to_usd.series.len())
            .map(|i| base_to_usd.series[i] / quote_to_usd.series[i])
            .collect();
        assert!((price - 1.10 / 1.27).abs() < 1e-9);
        assert!((prev - 1.09 / 1.26).abs() < 1e-9);
        assert_eq!(series.len(), 4);
        assert!((series[0] - 1.10 / 1.27).abs() < 1e-9);
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
