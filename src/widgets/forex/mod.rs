// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Forex widget — mirrors the Stocks widget's shape but for currency
//! pairs. One configured **primary** currency (e.g. USD), an ordered
//! list of **quote** currencies (EUR, GBP, JPY, …), and an editable
//! **amount** (defaults to the primary currency's canonical unit; USD
//! = 1, JPY = 100, KRW = 1000, …). The list shows what `amount` of
//! primary equals in each quote currency; the selected row drives a
//! historical chart (1D / 1W / … / 10Y) and a stats panel.
//!
//! Data source: Yahoo Finance via `forex::provider::YahooForexProvider`
//! using `{BASE}{QUOTE}=X` symbol convention. No API key required.
//! Same cache-and-fallback pattern as Stocks: each period's full
//! `HashMap<symbol, ForexQuote>` snapshot is persisted under a
//! `quotes-<period>` cache key and replayed at construction so the
//! widget paints prior rates instantly and survives transient fetch
//! failures with the last-known data.
//!
//! Interactions:
//!   - ↑ / ↓ / j / k        — move selection through the list
//!   - ← / → / h / l        — cycle graph period
//!   - 1–9                  — jump directly to a period
//!   - e (or click amount)  — edit the amount inline; Enter commits, Esc cancels
//!   - c                    — reset amount to the canonical unit for the current primary
//!   - r                    — force refresh
//!   - x                    — clear `:fx <code>` transient lookup
//!   - y (or click 📋)      — yank selected row's value to clipboard via OSC 52
//!   - click ↔              — swap that currency to primary (amount converts; old primary becomes selected)
//!   - click row code       — select that row
//!   - Enter                — open the selected pair on Yahoo Finance
//!   - :fx <code>           — switch the primary currency to <code>

pub mod graph;
pub mod provider;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};
use serde::Deserialize;

use crate::cache::ScopedCache;
use crate::theme::{ColorScheme, Theme};
use crate::ui::{apply_title_row, MetadataEmphasis};

use super::{AppContext, EventResult, Widget};

use provider::{ForexQuote, Period, YahooForexProvider};

// ─────────────────────────────────────────────────────────────────────
// Configuration
// ─────────────────────────────────────────────────────────────────────

/// Loaded from `~/.config/glint/forex.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct ForexConfig {
    /// ISO-4217 code of the primary currency. All list rows show how
    /// much `amount` of this currency is worth in their respective
    /// quote currencies.
    #[serde(default = "default_primary")]
    pub primary: String,

    /// Quote currencies, in display order. The primary currency is
    /// surfaced as a row too (marked with ★) so the user can swap with
    /// a single click without reaching for the config.
    #[serde(default = "default_watchlist")]
    pub watchlist: Vec<String>,

    /// Crypto tickers, in display order. Always rendered in a
    /// separate `── Crypto ──` section below the fiat watchlist.
    /// Empty by default — bring your own if you want crypto rows.
    /// Codes here implicitly opt into Yahoo's hyphenated `BTC-USD`
    /// symbol format regardless of whether they appear in the
    /// provider's built-in `CRYPTO_CODES` set.
    #[serde(default = "default_crypto_watchlist")]
    pub crypto_watchlist: Vec<String>,

    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    #[serde(default)]
    pub default_period: Period,

    /// Optional `{base}{quote}` template for the Enter-key external
    /// jump. Defaults to Yahoo Finance's quote page when unset.
    #[serde(default)]
    pub jump_url_template: Option<String>,

    /// Per-currency overrides for the canonical "1.00 of X" display
    /// unit. JPY = 100, KRW = 1000, etc. Falls back to the built-in
    /// `default_canonical_unit` map then to 1.0 if missing.
    #[serde(default)]
    pub canonical_units: HashMap<String, f64>,

    /// Faint dashed range-high/low lines on non-Day periods.
    #[serde(default = "default_graph_high_low_lines")]
    pub graph_high_low_lines: bool,

    /// Pad the 1D chart to a full 24-hour x-axis so the trace's right
    /// edge reflects how far into the day we are. FX trades ~24/5 so
    /// the proxy is rougher than for equities, but better than letting
    /// a half-day's data stretch across the whole panel.
    #[serde(default = "default_pad_intraday_to_full_day")]
    pub pad_intraday_to_full_day: bool,

    /// Per-widget theme overrides layered on the app theme.
    #[serde(default)]
    pub colors: ColorScheme,

    /// `Shift+<letter>` focus shortcuts; defaults to `['f','o','r','e','x']`.
    #[serde(default)]
    pub shortcuts: Vec<char>,
}

fn default_primary() -> String {
    "USD".into()
}

fn default_watchlist() -> Vec<String> {
    vec![
        "EUR".into(),
        "GBP".into(),
        "JPY".into(),
        "CAD".into(),
        "AUD".into(),
        "CHF".into(),
        "CNY".into(),
    ]
}

fn default_crypto_watchlist() -> Vec<String> {
    vec![
        "BTC".into(),
        "ETH".into(),
        "SOL".into(),
        "XRP".into(),
    ]
}

fn default_poll_interval() -> u64 {
    600
}

fn default_graph_high_low_lines() -> bool {
    true
}

fn default_pad_intraday_to_full_day() -> bool {
    true
}

impl Default for ForexConfig {
    fn default() -> Self {
        Self {
            primary: default_primary(),
            watchlist: default_watchlist(),
            crypto_watchlist: default_crypto_watchlist(),
            poll_interval_secs: default_poll_interval(),
            default_period: Period::default(),
            jump_url_template: None,
            canonical_units: HashMap::new(),
            graph_high_low_lines: default_graph_high_low_lines(),
            pad_intraday_to_full_day: default_pad_intraday_to_full_day(),
            colors: ColorScheme::default(),
            shortcuts: Vec::new(),
        }
    }
}

/// Built-in canonical-unit map. Currencies with very small per-USD
/// values are typically quoted in 100s or 1000s — showing them at
/// "1.0000 JPY" requires too many decimals to be readable, so the
/// canonical display unit shifts.
fn default_canonical_unit(code: &str) -> f64 {
    match code.to_ascii_uppercase().as_str() {
        "JPY" | "RUB" | "ISK" => 100.0,
        "KRW" | "IDR" | "HUF" | "CLP" | "COP" => 1_000.0,
        "VND" | "IRR" => 10_000.0,
        _ => 1.0,
    }
}

// ─────────────────────────────────────────────────────────────────────
// State
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum QuoteState {
    Inflight,
    Ready(Box<ForexQuote>),
    Failed,
}

#[derive(Default)]
struct ForexState {
    /// Latest fetched rate for each `{primary}{quote}=X` pair plus the
    /// inverse pairs needed for swap-and-preserve-value math. Indexed
    /// by the Yahoo symbol so a single lookup serves both rendering
    /// and the swap path.
    quotes: HashMap<String, QuoteState>,
    /// Index into `all_rows()` (primary + watchlist + optional lookup).
    selected: usize,
    list_scroll: usize,
    /// Transient currency pinned by `:fx <code>` when the code isn't
    /// already in the watchlist. Shown in a `── Lookup ──` section at
    /// the bottom of the list, cleared by `x`.
    transient_code: Option<String>,
    last_attempt: Option<Instant>,
    any_inflight: bool,
    /// When `Some`, the amount cell is being edited. The buffer is the
    /// in-progress string so `e → 1 → 5 → 2 → backspace → Enter` shows
    /// the partial value with a blinking cursor. Cleared on Enter (commit)
    /// or Esc (cancel).
    editing_amount: Option<String>,
    /// Hit-rects captured by the last render: row index → screen
    /// columns occupied by the ↔ icon, 📋 icon, code+value text, and
    /// the amount input cell (for the primary row only). Used by
    /// handle_mouse to route clicks without re-running the layout math.
    row_hits: Vec<RowHits>,
    /// Hit-rects for the graph-panel period toggle bar, captured during
    /// render. handle_mouse uses these to route a click on `[1Y]` etc.
    /// straight into `set_period`.
    toggle_hits: ToggleHits,
}

/// Screen-absolute click ranges for the period toggle bar (`[1D] [1W]
/// ...`). `row` is the bar's terminal row; each entry in `ranges` is
/// the inclusive-start / exclusive-end column for one period label
/// plus its surrounding brackets.
#[derive(Debug, Default, Clone)]
struct ToggleHits {
    row: u16,
    ranges: Vec<(Period, u16, u16)>,
}

/// Per-row hit ranges captured at render time. All ranges are
/// screen-absolute `[start_col, end_col_exclusive)`. Row 0 of the
/// vector is the primary row; clicks on its swap range are no-ops
/// because `swap_present` is false there.
#[derive(Debug, Default, Clone)]
struct RowHits {
    row: u16,
    /// Anywhere in the row outside the icon hot-zones counts as
    /// "select this row." This range is the **whole** clickable
    /// non-icon area — marker + code + value.
    select_start: u16,
    select_end: u16,
    /// `↔` swap icon range. Empty (start==end) on the primary row.
    swap_start: u16,
    swap_end: u16,
    swap_present: bool,
    /// `📋` copy icon range. Empty (start==end) when absent.
    copy_start: u16,
    copy_end: u16,
    copy_present: bool,
    /// Amount cell range (primary row only — that's where `e` /
    /// click-to-edit applies).
    amount_start: u16,
    amount_end: u16,
    amount_present: bool,
}

// Cache key: per-period since the series shape varies. Same as Stocks.
const CACHE_KEY_QUOTES_PREFIX: &str = "fx-quotes-";

fn quotes_cache_key(period: Period) -> String {
    format!("{CACHE_KEY_QUOTES_PREFIX}{}", period.label().to_ascii_lowercase())
}

// ─────────────────────────────────────────────────────────────────────
// Widget
// ─────────────────────────────────────────────────────────────────────

pub struct ForexWidget {
    id: String,
    instance: String,
    display_name_cache: String,
    config: ForexConfig,
    provider: Arc<YahooForexProvider>,
    state: Arc<Mutex<ForexState>>,
    poll_interval: Duration,
    /// Current primary currency. Starts at config.primary; swap actions
    /// flip it without rewriting the config. Kept in the widget (not
    /// state) since it changes synchronously and feeds row-build math.
    primary: String,
    /// Alternate currencies displayed below the primary, in display
    /// order. Seeded from `config.watchlist` (with `primary` filtered
    /// out) at construction. Swap actions reorder this in-place: the
    /// new primary is removed from here and the old primary slots into
    /// position 0, so "what was just primary" is always one row down
    /// from the top.
    alternates: Vec<String>,
    /// Index in `alternates` where the crypto section starts.
    /// `alternates[..crypto_start]` are fiat; `alternates[crypto_start..]`
    /// are crypto. The list renderer uses this to position the
    /// `── Currencies ──` / `── Crypto ──` headers.
    crypto_start: usize,
    /// Editable amount, in units of `primary`. Defaults to the
    /// canonical unit for the configured primary at construction time.
    /// Survives across renders; reset by `c` or a primary swap.
    amount: f64,
    /// Graph period (mirrors Stocks).
    period: Period,
    app_theme: Arc<Theme>,
    theme: Theme,
    shortcut: Option<char>,
    shortcut_prefs: Vec<char>,
    cache: ScopedCache,
}

impl ForexWidget {
    pub fn with_config(
        instance: String,
        config: ForexConfig,
        app_theme: Arc<Theme>,
        cache: ScopedCache,
    ) -> Self {
        let provider = match YahooForexProvider::new() {
            Ok(p) => Arc::new(p),
            Err(err) => {
                tracing::warn!(error = %err, "failed to build Yahoo Forex provider, widget will be inert");
                Arc::new(YahooForexProvider::new().expect("dummy yahoo forex provider should build"))
            }
        };
        let theme = app_theme.with_overrides(&config.colors);
        let shortcut_prefs = if config.shortcuts.is_empty() {
            vec!['f', 'o', 'r', 'e', 'x']
        } else {
            config.shortcuts.clone()
        };
        let id = if instance == "main" {
            "forex".to_string()
        } else {
            format!("forex@{instance}")
        };
        let display_name_cache = if instance == "main" {
            "Forex".to_string()
        } else {
            format!("Forex ({instance})")
        };
        let primary = config.primary.to_ascii_uppercase();
        let amount = canonical_amount(&config, &primary);
        let period = config.default_period;
        let poll_interval = Duration::from_secs(config.poll_interval_secs.max(60));
        // Alternates: fiat from `watchlist` then crypto from
        // `crypto_watchlist`, primary filtered out, codes normalized
        // to uppercase, duplicates dropped (first wins). The boundary
        // index drives section-header placement at render time.
        let (alternates, crypto_start) = build_alternates(
            &config.watchlist,
            &config.crypto_watchlist,
            &primary,
        );

        // Seed quotes from cache so the widget paints prior rates
        // immediately and survives transient fetch failures with the
        // last-known data (the user's first-launch experience is a
        // blank list until the first poll completes).
        let mut initial_state = ForexState::default();
        if let Some(entry) = cache.load::<HashMap<String, ForexQuote>>(&quotes_cache_key(period)) {
            let age = entry.age().min(poll_interval);
            initial_state.quotes = entry
                .value
                .into_iter()
                .map(|(sym, q)| (sym, QuoteState::Ready(Box::new(q))))
                .collect();
            // Seed last_attempt so we don't blast a refresh immediately;
            // is_due() will let one fire after `poll_interval - age`.
            initial_state.last_attempt = Some(Instant::now() - age);
        }
        // Auto-highlight the first alternate so the stats column and
        // graph have something to show on first paint. Falls back to
        // the primary row when no alternates exist (empty watchlist).
        initial_state.selected = if alternates.is_empty() { 0 } else { 1 };

        Self {
            id,
            instance,
            display_name_cache,
            poll_interval,
            config,
            provider,
            state: Arc::new(Mutex::new(initial_state)),
            primary,
            alternates,
            crypto_start,
            amount,
            period,
            app_theme,
            theme,
            shortcut: None,
            shortcut_prefs,
            cache,
        }
    }

    // ────── helpers ──────

    /// Display-order list: primary first (★), then `self.alternates`,
    /// then the optional transient lookup row at the bottom. Indices
    /// into this vec are what `selected` refers to.
    fn all_rows(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::with_capacity(2 + self.alternates.len());
        out.push(self.primary.clone());
        for code in &self.alternates {
            out.push(code.clone());
        }
        if let Some(t) = self
            .state
            .lock()
            .expect("forex state poisoned")
            .transient_code
            .clone()
        {
            let upper = t.to_ascii_uppercase();
            if upper != self.primary && !self.alternates.iter().any(|c| c == &upper) {
                out.push(upper);
            }
        }
        out
    }

    /// Yahoo symbol needed to render the quote-currency value on a row.
    /// We always fetch `{primary}{quote}=X` so the rate semantics stay
    /// "1 primary in quote".
    fn symbol_for_row(&self, code: &str) -> String {
        YahooForexProvider::symbol_for(&self.primary, code)
    }

    fn is_due(&self) -> bool {
        let st = self.state.lock().expect("forex state poisoned");
        if st.any_inflight {
            return false;
        }
        match st.last_attempt {
            None => true,
            Some(t) => t.elapsed() >= self.poll_interval,
        }
    }

    fn mark_dirty(&self) {
        let mut st = self.state.lock().expect("forex state poisoned");
        st.last_attempt = None;
    }

    fn spawn_refresh(&self) {
        // Build the set of (base, quote) pairs we need to fetch. The
        // primary row doesn't need a quote — its value is the amount
        // itself — so it's skipped.
        let rows = self.all_rows();
        let pairs: Vec<(String, String)> = rows
            .iter()
            .filter(|code| *code != &self.primary)
            .map(|code| (self.primary.clone(), code.clone()))
            .collect();
        if pairs.is_empty() {
            return;
        }
        {
            let mut st = self.state.lock().expect("forex state poisoned");
            st.any_inflight = true;
            st.last_attempt = Some(Instant::now());
            for (base, quote) in &pairs {
                let sym = YahooForexProvider::symbol_for(base, quote);
                st.quotes.entry(sym).or_insert(QuoteState::Inflight);
            }
        }
        let provider = self.provider.clone();
        let state = self.state.clone();
        let cache = self.cache.clone();
        let period = self.period;
        // Only persist to disk when the live primary matches the
        // configured one — the disk cache is the seed for the *next*
        // process start, which always launches at config.primary.
        // Storing quotes from a transient swap (e.g. user was on BTC
        // primary when they quit) would seed the next launch with
        // BTC-keyed symbols that don't match the USD-keyed lookups
        // the widget makes at startup, leaving every row blank
        // until the first fresh fetch returns.
        let persist_cache = self.primary.eq_ignore_ascii_case(&self.config.primary);
        tokio::spawn(async move {
            let futs = pairs.iter().map(|(base, quote)| {
                let provider = provider.clone();
                let base = base.clone();
                let quote = quote.clone();
                async move {
                    let result = provider.fetch_quote(&base, &quote, period).await;
                    (YahooForexProvider::symbol_for(&base, &quote), result)
                }
            });
            let results = futures::future::join_all(futs).await;
            let mut st = state.lock().expect("forex state poisoned");
            // Preserve any existing successful snapshot so individual
            // failed pairs fall back to their previous cached value
            // (loaded at construction or from a prior poll). Only the
            // newly-successful pairs overwrite their entry.
            let mut snapshot: HashMap<String, ForexQuote> = st
                .quotes
                .iter()
                .filter_map(|(k, v)| match v {
                    QuoteState::Ready(q) => Some((k.clone(), (**q).clone())),
                    _ => None,
                })
                .collect();
            for (sym, result) in results {
                match result {
                    Ok(q) => {
                        snapshot.insert(sym.clone(), q.clone());
                        st.quotes.insert(sym, QuoteState::Ready(Box::new(q)));
                    }
                    Err(err) => {
                        tracing::warn!(symbol = %sym, error = %err, "forex fetch failed");
                        // Leave a prior `Ready` entry in place if we have one
                        // — the user keeps seeing the last-known rate rather
                        // than `err`. Only flip to `Failed` if we never had
                        // a successful fetch for this pair.
                        st.quotes
                            .entry(sym)
                            .and_modify(|e| {
                                if !matches!(e, QuoteState::Ready(_)) {
                                    *e = QuoteState::Failed;
                                }
                            })
                            .or_insert(QuoteState::Failed);
                    }
                }
            }
            st.any_inflight = false;
            drop(st);
            if persist_cache && !snapshot.is_empty() {
                if let Err(err) = cache.store(&quotes_cache_key(period), &snapshot) {
                    tracing::warn!(error = %err, "forex cache store failed");
                }
            }
        });
    }

    fn snapshot_quotes(&self) -> HashMap<String, QuoteState> {
        self.state
            .lock()
            .expect("forex state poisoned")
            .quotes
            .clone()
    }

    fn move_selection(&mut self, delta: isize) {
        let n = self.all_rows().len();
        if n == 0 {
            return;
        }
        // Selection traverses alternates + lookup only. Row 0 (the
        // primary) has no graph / stats to surface and exists in the
        // list purely for the amount-edit and ↔-target affordances, so
        // there's no value in highlighting it. When there are no
        // alternates yet, we leave selection at 0 as a safe fallback.
        let min_idx: isize = if n > 1 { 1 } else { 0 };
        let max_idx: isize = (n - 1) as isize;
        let mut st = self.state.lock().expect("forex state poisoned");
        let new = (st.selected as isize + delta).clamp(min_idx, max_idx);
        st.selected = new as usize;
    }

    fn selected_code(&self) -> Option<String> {
        let rows = self.all_rows();
        let idx = self.state.lock().expect("forex state poisoned").selected;
        rows.into_iter().nth(idx)
    }

    fn set_period(&mut self, period: Period) {
        if self.period == period {
            return;
        }
        self.period = period;
        self.mark_dirty();
    }

    fn cycle_period(&mut self, forward: bool) {
        let idx = Period::ALL
            .iter()
            .position(|p| *p == self.period)
            .unwrap_or(0);
        let n = Period::ALL.len();
        let next = if forward {
            (idx + 1) % n
        } else {
            (idx + n - 1) % n
        };
        if let Some(p) = Period::ALL.get(next).copied() {
            self.set_period(p);
        }
    }

    /// Swap primary to `new_primary`. Per the spec: preserve absolute
    /// value of the amount (convert via current rate so $1523.80 USD
    /// becomes its EUR equivalent on the swap), and auto-select the
    /// row of the previously-primary currency so the graph immediately
    /// shows the inverse pair.
    fn swap_primary(&mut self, new_primary: &str) {
        let new_upper = new_primary.to_ascii_uppercase();
        if new_upper == self.primary {
            return;
        }
        let old_primary = std::mem::replace(&mut self.primary, new_upper.clone());

        // Decide the new amount. For fiat→fiat or fiat→fiat-style
        // swaps, convert via the current rate so the user keeps
        // roughly the same buying-power across the swap. For crypto
        // primaries we deliberately *don't* preserve buying power —
        // a converted amount like `0.0000113 BTC` (from 1 USD)
        // buries the per-unit comparison the user actually wants.
        // Always-1 makes "what is 1 BTC worth in X" the immediate
        // read. Falls back to the new primary's canonical unit if
        // no rate is available.
        let new_is_crypto = self
            .config
            .crypto_watchlist
            .iter()
            .any(|c| c.eq_ignore_ascii_case(&new_upper))
            || provider::is_crypto(&new_upper);
        self.amount = if new_is_crypto {
            1.0
        } else {
            let symbol = YahooForexProvider::symbol_for(&old_primary, &new_upper);
            let converted = {
                let st = self.state.lock().expect("forex state poisoned");
                match st.quotes.get(&symbol) {
                    Some(QuoteState::Ready(q)) if q.price > 0.0 => Some(self.amount * q.price),
                    _ => None,
                }
            };
            converted.unwrap_or_else(|| canonical_amount(&self.config, &new_upper))
        };

        // Reorder alternates. Two cases:
        //
        // * **Swap back to the configured/original primary** — restore
        //   the original config.watchlist order verbatim. This makes
        //   "go home" a genuine return to the initial layout instead of
        //   a return-with-shuffled-neighbors, which is what users
        //   actually expect when they undo a series of swaps.
        //
        // * **Swap to anything else** — drop the new primary out of the
        //   alternates list (if present) and push the old primary onto
        //   the front. The old primary becomes row 1 so the graph and
        //   stats auto-target the inverse pair, which is the natural
        //   "what's the just-replaced currency worth now?" follow-up.
        let original_primary = self.config.primary.to_ascii_uppercase();
        if new_upper == original_primary {
            // Re-seed from config with the same fiat-then-crypto
            // grouping `with_config` uses.
            let (alts, cs) = build_alternates(
                &self.config.watchlist,
                &self.config.crypto_watchlist,
                &new_upper,
            );
            self.alternates = alts;
            self.crypto_start = cs;
        } else {
            // Rebuild from config, with up to two prepends:
            //   1. `old_primary` lands at position 0 of its native
            //      category so a swap-back-to-just-swapped is row 1.
            //   2. `config.primary` (the user's *home base*) is then
            //      prepended on top of that, so the original primary
            //      stays permanently at the top of its category no
            //      matter how many hops the user has taken.
            //
            // Order matters: insert `old_primary` first, then
            // `config.primary`, so the final layout is
            //   [config.primary, old_primary, …rest…]
            // when both share a category. The previous logic only did
            // step 1, which caused the configured primary to fall out
            // of the list entirely on the second swap (USD→BTC→ETH
            // would drop USD because old_primary became BTC).
            let configured = self.config.primary.to_ascii_uppercase();
            let category_of = |code: &str| -> bool {
                self.config
                    .crypto_watchlist
                    .iter()
                    .any(|c| c.eq_ignore_ascii_case(code))
                    || provider::is_crypto(code)
            };
            let mut fiat: Vec<String> = self
                .config
                .watchlist
                .iter()
                .map(|s| s.to_ascii_uppercase())
                .collect();
            let mut crypto: Vec<String> = self
                .config
                .crypto_watchlist
                .iter()
                .map(|s| s.to_ascii_uppercase())
                .collect();
            let prepend = |code: &str,
                           fiat: &mut Vec<String>,
                           crypto: &mut Vec<String>| {
                let target = if category_of(code) { crypto } else { fiat };
                target.retain(|c| c != code);
                target.insert(0, code.to_string());
            };
            // Inserts run in reverse-display order so the configured
            // primary ends up at index 0 of its category. Skip the
            // old-primary insert when it IS the configured primary —
            // the second insert handles it once.
            if old_primary != configured {
                prepend(&old_primary, &mut fiat, &mut crypto);
            }
            prepend(&configured, &mut fiat, &mut crypto);
            let (alts, cs) = build_alternates(&fiat, &crypto, &new_upper);
            self.alternates = alts;
            self.crypto_start = cs;
        }

        // Selection: find the old-primary row in the new list. For
        // normal swaps that's row 1 (old primary just landed there);
        // for "swap home" it's wherever old_primary sits in the
        // configured order. Falls back to row 1 (or 0 if no alternates
        // exist at all) when the old primary isn't in the list.
        let new_selected = if self.alternates.is_empty() {
            0
        } else {
            self.alternates
                .iter()
                .position(|c| c == &old_primary)
                .map(|i| i + 1)
                .unwrap_or(1)
        };

        {
            let mut st = self.state.lock().expect("forex state poisoned");
            // Clear the transient if it was the code we just promoted.
            if let Some(t) = &st.transient_code {
                if t.eq_ignore_ascii_case(&new_upper) {
                    st.transient_code = None;
                }
            }
            // Stale rates for the prior primary's pairs no longer apply.
            // Drop them; the next refresh repopulates with the new pair shape.
            let keep_prefix = new_upper.clone();
            st.quotes.retain(|sym, _| sym.starts_with(&keep_prefix));
            st.selected = new_selected;
        }

        self.mark_dirty();
    }

    /// `:fx <code>` adds `<code>` to the Lookup section (or bounces
    /// selection to it if it's already in the list). It does NOT
    /// promote the currency to primary — that's the explicit `p` /
    /// click-↔ gesture so swapping always involves a deliberate
    /// confirmation step.
    fn handle_fx_command(&mut self, args: &[&str]) -> Result<()> {
        let code = args.first().copied().unwrap_or("").trim();
        if code.is_empty() {
            anyhow::bail!("usage: :fx <ISO code, e.g. USD, EUR, JPY>");
        }
        let upper = code.to_ascii_uppercase();
        if !is_iso_currency_codish(&upper) {
            anyhow::bail!("not an ISO-4217 looking code: {code:?}");
        }
        // Already primary → bounce selection to the primary row.
        if upper == self.primary {
            self.state.lock().expect("forex state poisoned").selected = 0;
            return Ok(());
        }
        // Already in alternates → bounce selection to its row instead
        // of duplicating it into the Lookup section.
        if let Some(pos) = self
            .alternates
            .iter()
            .position(|c| c.eq_ignore_ascii_case(&upper))
        {
            self.state.lock().expect("forex state poisoned").selected = 1 + pos;
            return Ok(());
        }
        // New code → pin as the transient Lookup row + select it so
        // the graph + stats panels surface the new pair right away.
        let n_alts = self.alternates.len();
        {
            let mut st = self.state.lock().expect("forex state poisoned");
            st.transient_code = Some(upper);
            st.selected = 1 + n_alts;
        }
        self.mark_dirty();
        Ok(())
    }

    fn clear_transient(&self) {
        let mut st = self.state.lock().expect("forex state poisoned");
        if st.transient_code.take().is_some() {
            // Bounce selection back to the primary row.
            st.selected = 0;
            st.list_scroll = 0;
        }
    }

    /// Swap the currently-selected row into the primary slot. Silent
    /// no-op when selection sits on the primary itself (which the
    /// selection-traversal rules prevent in practice, but we belt-and-
    /// brace the check here too).
    fn swap_selected_to_primary(&mut self) {
        if let Some(code) = self.selected_code() {
            if code != self.primary {
                self.swap_primary(&code);
            }
        }
    }

    fn jump_to_external(&self) {
        let Some(code) = self.selected_code() else {
            return;
        };
        let template = self.config.jump_url_template.clone().unwrap_or_else(|| {
            "https://finance.yahoo.com/quote/{base}{quote}=X".to_string()
        });
        let url = template
            .replace("{base}", &urlencoding::encode(&self.primary))
            .replace("{quote}", &urlencoding::encode(&code));
        if let Err(err) = open::that(&url) {
            tracing::warn!(error = %err, url = %url, "failed to open forex jump URL");
        }
    }

    /// Reset amount to the canonical unit for the current primary.
    fn reset_amount(&mut self) {
        self.amount = canonical_amount(&self.config, &self.primary);
    }

    /// Compute the *displayed* value for a row given the current amount
    /// and the latest rate for `{primary}{code}=X`. The primary row's
    /// value is `amount` itself. `None` means the rate isn't loaded.
    fn row_value(&self, code: &str, quotes: &HashMap<String, QuoteState>) -> Option<f64> {
        if code == self.primary {
            return Some(self.amount);
        }
        let symbol = self.symbol_for_row(code);
        match quotes.get(&symbol) {
            Some(QuoteState::Ready(q)) => Some(self.amount * q.price),
            _ => None,
        }
    }

    fn copy_selected_to_clipboard(&self) {
        let Some(code) = self.selected_code() else {
            return;
        };
        let quotes = self.snapshot_quotes();
        let Some(v) = self.row_value(&code, &quotes) else {
            return;
        };
        let text = format!("{:.4}", v);
        if let Err(err) = crate::clipboard::copy(&text) {
            tracing::warn!(error = %err, "OSC 52 clipboard write failed");
        }
    }

    fn enter_edit_mode(&self) {
        let mut st = self.state.lock().expect("forex state poisoned");
        st.editing_amount = Some(format_amount(self.amount));
    }

    fn cancel_edit(&self) {
        self.state
            .lock()
            .expect("forex state poisoned")
            .editing_amount = None;
    }

    /// Commit the edit buffer; returns true when the parse succeeded
    /// and the amount changed.
    fn commit_edit(&mut self) -> bool {
        let buf = {
            let mut st = self.state.lock().expect("forex state poisoned");
            st.editing_amount.take()
        };
        let Some(buf) = buf else {
            return false;
        };
        match buf.trim().parse::<f64>() {
            Ok(v) if v.is_finite() && v >= 0.0 => {
                let changed = (v - self.amount).abs() > f64::EPSILON;
                self.amount = v;
                changed
            }
            _ => false,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Widget trait
// ─────────────────────────────────────────────────────────────────────

#[async_trait]
impl Widget for ForexWidget {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> &str {
        "forex"
    }

    fn instance(&self) -> &str {
        &self.instance
    }

    fn display_name(&self) -> &str {
        &self.display_name_cache
    }

    async fn update(&mut self, _ctx: &AppContext) -> Result<()> {
        if self.is_due() {
            self.spawn_refresh();
        }
        Ok(())
    }

    fn render(&self, frame: &mut Frame, area: Rect, focused: bool) {
        let title = if self.instance == "main" {
            "Forex".to_string()
        } else {
            format!("Forex ({})", self.instance)
        };
        let metadata = self.title_metadata_string();
        let block = apply_title_row(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(self.theme.border_style(focused)),
            focused,
            &title,
            metadata.as_deref(),
            MetadataEmphasis::Default,
            self.shortcut,
            &self.theme,
            area.width,
        );
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let quotes = self.snapshot_quotes();
        let rows = self.all_rows();
        let selected_code = self.selected_code();
        let editing = self
            .state
            .lock()
            .expect("forex state poisoned")
            .editing_amount
            .clone();

        // Reserve the bottom row for the footer hint before the panel
        // layout split — without this carve-off the graph's x-axis
        // label row (rendered on the last row of the graph panel)
        // collides with the footer and gets overwritten.
        let footer_h: u16 = if inner.height >= 2 { 1 } else { 0 };
        let body = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: inner.height - footer_h,
        };

        // Adaptive layout: same constants as Stocks. The list column
        // is sized for the widest forex row (prefix + code + icons +
        // value); stats gets 30 cols; graph fills the rest.
        // List column is sized to fit the widest content row, which is
        // the `── Lookup (x to clear) ──` header at 25 cells; +2 cells
        // of trailing whitespace keep the right edge from kissing the
        // gap. Currency rows themselves are only 24 cells wide so they
        // sit comfortably inside this column.
        const WIDE_LIST_W: u16 = 27;
        const WIDE_STATS_W: u16 = 30;
        const MIN_GRAPH_W: u16 = 24;
        let is_wide = body.width >= WIDE_LIST_W + MIN_GRAPH_W;
        let with_stats = is_wide && body.width >= WIDE_LIST_W + WIDE_STATS_W + MIN_GRAPH_W;
        // Scaling factor for rate-shaped displays in the graph header,
        // y-axis, and stats panel. e.g. KRW=1000 turns the graph's
        // "1 KRW = 0.0007 USD" into "1000 KRW = 0.7 USD". Configured
        // per-currency via `[canonical_units]` in forex.toml, falling
        // back to the built-in map (`default_canonical_unit`).
        let primary_unit = canonical_amount(&self.config, &self.primary);

        if is_wide {
            let mut constraints: Vec<Constraint> =
                vec![Constraint::Length(WIDE_LIST_W), Constraint::Length(1)];
            if with_stats {
                constraints.push(Constraint::Length(WIDE_STATS_W));
                constraints.push(Constraint::Length(1));
            }
            constraints.push(Constraint::Fill(1));
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(constraints)
                .split(body);
            let list_area = cols[0];
            let (stats_area, graph_area) = if with_stats {
                (Some(cols[2]), cols[4])
            } else {
                (None, cols[2])
            };
            let (sel, cur_scroll) = {
                let st = self.state.lock().unwrap();
                (st.selected, st.list_scroll)
            };
            let (new_scroll, hits) = render_list_panel(
                frame,
                list_area,
                &rows,
                &self.primary,
                // rows = [primary, alternates..., transient?] → crypto
                // section starts at `1 + crypto_start` in rows space.
                1 + self.crypto_start,
                self.amount,
                editing.as_deref(),
                &quotes,
                sel,
                self.transient_present(),
                cur_scroll,
                &self.theme,
                |code, quotes| self.row_value(code, quotes),
            );
            {
                let mut st = self.state.lock().unwrap();
                st.list_scroll = new_scroll;
                st.row_hits = hits;
            }
            if let Some(stats_area) = stats_area {
                render_stats_panel(
                    frame,
                    stats_area,
                    selected_code.as_deref(),
                    &self.primary,
                    primary_unit,
                    &quotes,
                    &self.theme,
                );
            }
            let toggle_hits = render_graph_panel(
                frame,
                graph_area,
                selected_code.as_deref(),
                &self.primary,
                primary_unit,
                &quotes,
                self.period,
                self.config.graph_high_low_lines,
                self.config.pad_intraday_to_full_day,
                &self.theme,
            );
            self.state.lock().unwrap().toggle_hits = toggle_hits;
        } else {
            let list_h = ((body.height as f32) * 0.55).round() as u16;
            let rows_layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(list_h),
                    Constraint::Length(1),
                    Constraint::Fill(1),
                ])
                .split(body);
            let (sel, cur_scroll) = {
                let st = self.state.lock().unwrap();
                (st.selected, st.list_scroll)
            };
            let (new_scroll, hits) = render_list_panel(
                frame,
                rows_layout[0],
                &rows,
                &self.primary,
                1 + self.crypto_start,
                self.amount,
                editing.as_deref(),
                &quotes,
                sel,
                self.transient_present(),
                cur_scroll,
                &self.theme,
                |code, quotes| self.row_value(code, quotes),
            );
            {
                let mut st = self.state.lock().unwrap();
                st.list_scroll = new_scroll;
                st.row_hits = hits;
            }
            let toggle_hits = render_graph_panel(
                frame,
                rows_layout[2],
                selected_code.as_deref(),
                &self.primary,
                primary_unit,
                &quotes,
                self.period,
                self.config.graph_high_low_lines,
                self.config.pad_intraday_to_full_day,
                &self.theme,
            );
            self.state.lock().unwrap().toggle_hits = toggle_hits;
        }

        if inner.height >= 2 {
            let footer = Rect {
                x: inner.x,
                y: inner.y + inner.height - 1,
                width: inner.width,
                height: 1,
            };
            let hint = "e edit · ⏎/s swap · d details · c canonical · y copy · r refresh";
            frame.render_widget(
                Paragraph::new(Span::styled(hint, self.theme.text_dim))
                    .alignment(Alignment::Right),
                footer,
            );
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if key.modifiers != KeyModifiers::NONE && key.modifiers != KeyModifiers::SHIFT {
            return EventResult::Ignored;
        }
        // Uppercase ASCII letters are reserved for the app-wide
        // `Shift+<letter>` focus-jump dispatcher.
        if let KeyCode::Char(c) = key.code {
            if c.is_ascii_uppercase() {
                return EventResult::Ignored;
            }
        }

        // Edit mode swallows most keys: digits/decimal type into the
        // buffer; Enter commits, Esc cancels, Backspace pops a char.
        let editing = self
            .state
            .lock()
            .expect("forex state poisoned")
            .editing_amount
            .is_some();
        if editing {
            return self.handle_key_in_edit_mode(key);
        }

        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                EventResult::Handled
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                EventResult::Handled
            }
            // `Enter` and `s` both swap the currently-selected row
            // into the primary slot — keyboard equivalents of clicking
            // that row's ↔. Silent no-op on the primary row itself.
            KeyCode::Enter | KeyCode::Char('s') => {
                self.swap_selected_to_primary();
                EventResult::Handled
            }
            // `d` — "details" — opens the selected pair on Yahoo
            // Finance in the system browser. Moved off Enter so the
            // more common swap action gets the bigger key.
            KeyCode::Char('d') => {
                self.jump_to_external();
                EventResult::Handled
            }
            KeyCode::Char('e') => {
                self.enter_edit_mode();
                EventResult::Handled
            }
            KeyCode::Char('c') => {
                self.reset_amount();
                EventResult::Handled
            }
            KeyCode::Char('y') => {
                self.copy_selected_to_clipboard();
                EventResult::Handled
            }
            KeyCode::Char('r') => {
                self.mark_dirty();
                EventResult::Handled
            }
            KeyCode::Char('x') => {
                self.clear_transient();
                EventResult::Handled
            }
            KeyCode::Char(d @ '1'..='9') => {
                let idx = (d as u8 - b'1') as usize;
                if let Some(p) = Period::ALL.get(idx) {
                    self.set_period(*p);
                }
                EventResult::Handled
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.cycle_period(false);
                EventResult::Handled
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.cycle_period(true);
                EventResult::Handled
            }
            _ => EventResult::Ignored,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, _area: Rect) -> EventResult {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.move_selection(-1);
                EventResult::Handled
            }
            MouseEventKind::ScrollDown => {
                self.move_selection(1);
                EventResult::Handled
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // 1) Period toggle bar — cached row/col ranges from
                //    the last graph render. Try this first so a click
                //    that happens to land on a toggle isn't shadowed
                //    by a list-row hit.
                let toggle_hits = self
                    .state
                    .lock()
                    .expect("forex state poisoned")
                    .toggle_hits
                    .clone();
                if mouse.row == toggle_hits.row && !toggle_hits.ranges.is_empty() {
                    for (period, start, end) in &toggle_hits.ranges {
                        if mouse.column >= *start && mouse.column < *end {
                            self.set_period(*period);
                            return EventResult::Handled;
                        }
                    }
                }

                // 2) List rows — consult the cached per-row hit ranges.
                let hits = self
                    .state
                    .lock()
                    .expect("forex state poisoned")
                    .row_hits
                    .clone();
                let rows = self.all_rows();
                for (idx, hit) in hits.iter().enumerate() {
                    if mouse.row != hit.row {
                        continue;
                    }
                    // 2a) ↔ swap icon.
                    if hit.swap_present
                        && mouse.column >= hit.swap_start
                        && mouse.column < hit.swap_end
                    {
                        if let Some(code) = rows.get(idx).cloned() {
                            self.swap_primary(&code);
                        }
                        return EventResult::Handled;
                    }
                    // 2b) 📋 copy icon.
                    if hit.copy_present
                        && mouse.column >= hit.copy_start
                        && mouse.column < hit.copy_end
                    {
                        {
                            let mut st = self.state.lock().expect("forex state poisoned");
                            st.selected = idx;
                        }
                        self.copy_selected_to_clipboard();
                        return EventResult::Handled;
                    }
                    // 2c) Amount cell (primary row only).
                    if hit.amount_present
                        && mouse.column >= hit.amount_start
                        && mouse.column < hit.amount_end
                    {
                        {
                            let mut st = self.state.lock().expect("forex state poisoned");
                            st.selected = idx;
                        }
                        self.enter_edit_mode();
                        return EventResult::Handled;
                    }
                    // 2d) Fallback: anywhere on the row's select range
                    //     = select that row.
                    if mouse.column >= hit.select_start && mouse.column < hit.select_end {
                        let mut st = self.state.lock().expect("forex state poisoned");
                        st.selected = idx;
                        return EventResult::Handled;
                    }
                }
                EventResult::Ignored
            }
            _ => EventResult::Ignored,
        }
    }

    fn handle_command(&mut self, cmd: &str, args: &[&str]) -> Result<bool> {
        match cmd {
            "fx" | "forex" => {
                self.handle_fx_command(args)?;
                Ok(true)
            }
            "refresh" => {
                self.mark_dirty();
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn keybindings(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("↑ / ↓ / j / k", "select currency"),
            ("← / → / h / l", "cycle graph period"),
            ("1-9", "set graph period directly"),
            ("e", "edit amount (Enter commits, Esc cancels)"),
            ("c", "reset amount to canonical unit"),
            ("Enter / s", "swap selected currency into primary slot"),
            ("y", "yank selected value to clipboard (OSC 52)"),
            ("d", "open pair details on Yahoo Finance"),
            ("r", "force refresh"),
            ("x", "clear :fx <code> lookup"),
            ("click ↔", "swap that currency to primary"),
            ("click 📋", "yank that row's value"),
            ("click code", "select that row"),
            ("click toggle", "switch graph period"),
            (":fx <code>", "add currency to Lookup section"),
        ]
    }

    fn config(&self) -> serde_json::Value {
        serde_json::json!({
            "primary": self.primary,
            "watchlist": self.config.watchlist,
            "poll_interval_secs": self.poll_interval.as_secs(),
            "period": self.period.label(),
            "amount": self.amount,
        })
    }

    fn apply_config(&mut self, config: serde_json::Value) -> Result<()> {
        let new_config: ForexConfig =
            serde_json::from_value(config).context("invalid forex config payload")?;
        let app_theme = self.app_theme.clone();
        let cache = self.cache.clone();
        let instance = self.instance.clone();
        *self = Self::with_config(instance, new_config, app_theme, cache);
        Ok(())
    }

    fn set_app_theme(&mut self, theme: Arc<Theme>) {
        self.theme = theme.with_overrides(&self.config.colors);
        self.app_theme = theme;
    }

    fn shortcut_preferences(&self) -> &[char] {
        &self.shortcut_prefs
    }

    fn set_shortcut(&mut self, shortcut: Option<char>) {
        self.shortcut = shortcut;
    }

    fn shortcut(&self) -> Option<char> {
        self.shortcut
    }

    fn title_metadata(&self) -> Option<String> {
        self.title_metadata_string()
    }
}

impl ForexWidget {
    fn title_metadata_string(&self) -> Option<String> {
        let n = self.config.watchlist.len();
        Some(format!("{} · {n} pairs", self.primary))
    }

    fn transient_present(&self) -> bool {
        self.state
            .lock()
            .expect("forex state poisoned")
            .transient_code
            .is_some()
    }

    fn handle_key_in_edit_mode(&mut self, key: KeyEvent) -> EventResult {
        match key.code {
            KeyCode::Enter => {
                let _changed = self.commit_edit();
                EventResult::Handled
            }
            KeyCode::Esc => {
                self.cancel_edit();
                EventResult::Handled
            }
            KeyCode::Backspace => {
                let mut st = self.state.lock().expect("forex state poisoned");
                if let Some(buf) = st.editing_amount.as_mut() {
                    buf.pop();
                }
                EventResult::Handled
            }
            KeyCode::Char(c)
                if c.is_ascii_digit() || c == '.' || c == ',' =>
            {
                let mut st = self.state.lock().expect("forex state poisoned");
                if let Some(buf) = st.editing_amount.as_mut() {
                    // Treat `,` as `.` (locale grace) and reject a
                    // second decimal point so the parser doesn't fail
                    // commit.
                    let to_push = if c == ',' { '.' } else { c };
                    if to_push == '.' && buf.contains('.') {
                        return EventResult::Handled;
                    }
                    buf.push(to_push);
                }
                EventResult::Handled
            }
            _ => EventResult::Handled, // swallow everything while editing
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Rendering helpers — list, graph, stats
// ─────────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn render_list_panel<F>(
    frame: &mut Frame,
    area: Rect,
    rows: &[String],
    primary: &str,
    // Index in `rows` where the crypto section begins. Equals
    // `rows.len()` when no crypto alternates are configured —
    // the renderer then never emits a `── Crypto ──` header.
    crypto_row_start: usize,
    amount: f64,
    editing: Option<&str>,
    quotes: &HashMap<String, QuoteState>,
    selected: usize,
    has_transient: bool,
    current_scroll: usize,
    theme: &Theme,
    row_value: F,
) -> (usize, Vec<RowHits>)
where
    F: Fn(&str, &HashMap<String, QuoteState>) -> Option<f64>,
{
    // Reserve the bottom row of the area for the global footer hint;
    // the list itself draws into `usable_h` rows above it.
    let usable_h = area.height.saturating_sub(1) as usize;

    // Build the line list + per-row hit-rect data first so we can
    // honor the scroll math without re-running the layout.
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows.len() + 4);
    let mut row_to_line: Vec<usize> = Vec::with_capacity(rows.len());
    let mut row_hits: Vec<RowHits> = Vec::with_capacity(rows.len());
    // We'll compute screen-absolute hit columns after we know the
    // scroll value (since visible_y depends on it). For now, store the
    // *logical* line index — convert to absolute later.
    let mut hits_by_logical: Vec<(usize, RowHits)> = Vec::new();

    let mut currencies_header_emitted = false;
    let mut crypto_header_emitted = false;
    let mut transient_header_emitted = false;

    // Top padding: a blank row above the primary row gives the list
    // some vertical breathing room from the title border.
    lines.push(Line::from(""));

    for (i, code) in rows.iter().enumerate() {
        let is_primary = code == primary;
        // Emit the appropriate category header the first time we hit
        // each section. `build_alternates` guarantees fiat indices
        // come before crypto indices in the underlying alternates
        // list, so checking against `crypto_row_start` is enough.
        if !is_primary {
            let is_crypto_row = i >= crypto_row_start && !(has_transient && i == rows.len() - 1);
            if is_crypto_row && !crypto_header_emitted {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "── Crypto ──",
                    theme.text_dim,
                )));
                crypto_header_emitted = true;
            } else if !is_crypto_row && !currencies_header_emitted {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "── Currencies ──",
                    theme.text_dim,
                )));
                currencies_header_emitted = true;
            }
        }
        if has_transient && !transient_header_emitted && i == rows.len() - 1 && !is_primary {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "── Lookup (x to clear) ──",
                theme.text_dim,
            )));
            transient_header_emitted = true;
        }
        row_to_line.push(lines.len());
        let is_selected = i == selected;
        let value = if is_primary {
            Some(amount)
        } else {
            row_value(code, quotes)
        };
        let (line, hit) = build_list_row(
            code,
            is_primary,
            is_selected,
            value,
            editing.filter(|_| is_primary),
            theme,
        );
        hits_by_logical.push((lines.len(), hit));
        lines.push(line);
    }

    // Scroll: clamp so the selected row stays visible.
    let sel_line = row_to_line.get(selected).copied().unwrap_or(0);
    let mut scroll = current_scroll;
    if sel_line < scroll {
        scroll = sel_line;
    }
    if usable_h > 0 && sel_line >= scroll + usable_h {
        scroll = sel_line + 1 - usable_h;
    }
    let max_scroll = lines.len().saturating_sub(usable_h.max(1));
    if scroll > max_scroll {
        scroll = max_scroll;
    }

    let end = (scroll + usable_h).min(lines.len());
    let visible: Vec<Line<'static>> = lines.iter().skip(scroll).take(end - scroll).cloned().collect();
    let list_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: usable_h as u16,
    };
    frame.render_widget(Paragraph::new(visible), list_area);

    // Translate logical line indices to screen-absolute row positions,
    // dropping rows that scrolled out of view.
    for (line_idx, mut hit) in hits_by_logical.into_iter() {
        if line_idx < scroll || line_idx >= scroll + usable_h {
            row_hits.push(RowHits::default()); // placeholder, never matches
            continue;
        }
        let screen_row = list_area.y + (line_idx - scroll) as u16;
        hit.row = screen_row;
        // Hit columns are written relative to area.x when built; shift
        // them into screen-absolute by adding area.x.
        hit.select_start = hit.select_start.saturating_add(area.x);
        hit.select_end = hit.select_end.saturating_add(area.x);
        if hit.swap_present {
            hit.swap_start = hit.swap_start.saturating_add(area.x);
            hit.swap_end = hit.swap_end.saturating_add(area.x);
        }
        if hit.copy_present {
            hit.copy_start = hit.copy_start.saturating_add(area.x);
            hit.copy_end = hit.copy_end.saturating_add(area.x);
        }
        if hit.amount_present {
            hit.amount_start = hit.amount_start.saturating_add(area.x);
            hit.amount_end = hit.amount_end.saturating_add(area.x);
        }
        row_hits.push(hit);
    }

    (scroll, row_hits)
}

/// Render one list row. Returns the styled `Line` and a `RowHits`
/// whose columns are written **relative to the panel's `area.x`**;
/// caller offsets them into screen-absolute later.
///
/// Visual layout (cells, left → right):
///   `[marker 2][code 5][swap 2][value 12][copy 2]` — total 23 cells.
/// Icons sit next to their semantic target: `↔` next to the code (the
/// thing it swaps), `📋` next to the value (the thing it copies).
fn build_list_row(
    code: &str,
    is_primary: bool,
    is_selected: bool,
    value: Option<f64>,
    editing: Option<&str>,
    theme: &Theme,
) -> (Line<'static>, RowHits) {
    const MARKER_W: u16 = 2;
    const CODE_W: u16 = 5;
    const SWAP_W: u16 = 2;
    const VALUE_W: u16 = 12;
    /// 1-cell gap between the value column and the 📋 icon so the
    /// number doesn't kiss the emoji. Rendered as plain whitespace on
    /// every row including the primary (where the 📋 itself is absent).
    const VALUE_COPY_GAP_W: u16 = 1;
    const COPY_W: u16 = 2;

    // Primary has no marker — it's the topmost row and never
    // "selected" since the select-traversal skips it. Alternates get
    // a leading `▸` when highlighted, matching Stocks' selection carat.
    let marker = if is_primary {
        "  "
    } else if is_selected {
        "▸ "
    } else {
        "  "
    };
    let code_str = format!("{:<width$}", code, width = CODE_W as usize);
    // Primary row reuses the swap / copy slots (which would otherwise
    // be blank) to host `[` / `]` brackets that frame the amount cell
    // as a visual "this is editable" cue. Alternates get the real
    // ↔ / 📋 icons in the same slots.
    let (swap_str, swap_present) = if is_primary {
        (" [".to_string(), false)
    } else {
        ("↔ ".to_string(), true)
    };
    let (copy_str, copy_present) = if is_primary {
        ("] ".to_string(), false)
    } else {
        ("📋".to_string(), true)
    };
    let value_str = match value {
        Some(v) if is_primary => match editing {
            Some(buf) => fit_edit_buffer_right(buf, VALUE_W as usize),
            None => format_amount(v),
        },
        Some(v) => format!("{:>width$.4}", v, width = VALUE_W as usize),
        None => format!("{:>width$}", "…", width = VALUE_W as usize),
    };

    // Hit ranges (relative to panel x). Caller adds area.x to convert
    // these into screen-absolute later. End columns are exclusive.
    let mut col = MARKER_W;
    // Marker + code own cols [0..MARKER_W+CODE_W); the select range
    // below covers them through end-of-value so a click anywhere
    // outside the icon hot-zones selects the row.
    col += CODE_W;
    let swap_start = col;
    col += SWAP_W;
    let swap_end = col;
    let value_start = col;
    col += VALUE_W;
    let value_end = col;
    col += VALUE_COPY_GAP_W; // breathing room before the 📋 icon
    let copy_start = col;
    col += COPY_W;
    let copy_end = col;

    // Styling
    let marker_style = if is_selected {
        Style::default()
            .fg(Color::LightYellow)
            .add_modifier(Modifier::BOLD)
    } else {
        theme.text_dim
    };
    let code_style = if is_selected {
        Style::default()
            .fg(Color::LightYellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    let icon_dim = theme.text_dim;
    // Bracket frame around the primary's amount cell: dim when idle
    // (subtle hint that the cell is editable), bright when in edit
    // mode (clear "you're modifying this" signal).
    let frame_style = if is_primary && editing.is_some() {
        Style::default()
            .fg(Color::LightYellow)
            .add_modifier(Modifier::BOLD)
    } else {
        theme.text_dim
    };
    let value_style = if editing.is_some() && is_primary {
        Style::default()
            .fg(Color::LightYellow)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else if is_selected {
        Style::default()
            .fg(Color::LightYellow)
            .add_modifier(Modifier::BOLD)
    } else if is_primary {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    // Style routing: on the primary row the swap / copy slots host the
    // `[` / `]` frame and use frame_style; on alternates they're real
    // icons and stay dim. The gap between value and copy normally
    // renders as a plain space, but during edit mode it hosts the `▏`
    // cursor so the digits inside the field stay right-aligned at the
    // same columns they occupy in non-edit mode.
    let swap_style = if is_primary { frame_style } else { icon_dim };
    let copy_style = if is_primary { frame_style } else { icon_dim };
    let gap_span = if is_primary && editing.is_some() {
        Span::styled("▏", frame_style)
    } else {
        Span::raw(" ") // VALUE_COPY_GAP_W
    };
    let line = Line::from(vec![
        Span::styled(marker.to_string(), marker_style),
        Span::styled(code_str, code_style),
        Span::styled(swap_str, swap_style),
        Span::styled(value_str, value_style),
        gap_span,
        Span::styled(copy_str, copy_style),
    ]);

    // Select-on-click hit zone: any non-icon cell on the row. We
    // collapse it to an empty range on the primary row so a body
    // click there is a no-op — the primary row isn't highlightable,
    // only its amount cell is interactive (handled by the separate
    // `amount_present` hit below).
    let (select_start, select_end) = if is_primary {
        (0, 0)
    } else {
        // marker + code + swap + value (icons take priority — see the
        // hit-test order in handle_mouse, swap_start/end and
        // copy_start/end are checked before the select range)
        (0, value_end)
    };
    // Amount-cell click zone. On the primary row we extend it to
    // cover the `[` and `]` bracket slots so clicking the visible
    // frame also enters edit mode — users don't have to aim at the
    // value text exactly. On alternates it's unused (amount_present =
    // false) so the range is academic.
    let (amount_start, amount_end) = if is_primary {
        (swap_start, copy_end)
    } else {
        (value_start, value_end)
    };
    let hit = RowHits {
        row: 0, // caller sets after scroll math
        select_start,
        select_end,
        swap_start,
        swap_end,
        swap_present,
        copy_start,
        copy_end,
        copy_present,
        amount_start,
        amount_end,
        amount_present: is_primary,
    };
    (line, hit)
}

#[allow(clippy::too_many_arguments)]
// `primary_unit` is the canonical display unit for the primary currency
// (USD=1, JPY=100, KRW=1000, …). Rate-shaped values rendered in the
// header and on the y-axis are multiplied by it so "1 KRW = 0.0007 USD"
// becomes "1000 KRW = 0.7 USD" — much easier to read for currencies
// where 1 unit is fractional cents.
#[allow(clippy::too_many_arguments)]
fn render_graph_panel(
    frame: &mut Frame,
    area: Rect,
    selected: Option<&str>,
    primary: &str,
    primary_unit: f64,
    quotes: &HashMap<String, QuoteState>,
    period: Period,
    show_high_low_lines: bool,
    pad_intraday_to_full_day: bool,
    theme: &Theme,
) -> ToggleHits {
    if area.width < 4 || area.height < 4 {
        return ToggleHits::default();
    }
    let toggle_row_y = area.y + 1;
    let toggle_hits = render_period_toggle_bar(
        frame,
        Rect {
            x: area.x,
            y: toggle_row_y,
            width: area.width,
            height: 1,
        },
        period,
        theme,
    );

    // For the primary row, there's no pair to chart — show a hint.
    let target = selected.unwrap_or("");
    if target == primary || target.is_empty() {
        let msg = if target.is_empty() {
            "Select a currency".to_string()
        } else {
            format!(
                "{primary} is the primary — pick another row to chart its rate"
            )
        };
        let para = Paragraph::new(Line::from(Span::styled(msg, theme.text_dim)))
            .alignment(Alignment::Center);
        let centered = Rect {
            x: area.x,
            y: area.y + area.height / 2,
            width: area.width,
            height: 1,
        };
        frame.render_widget(para, centered);
        return toggle_hits;
    }

    let symbol = YahooForexProvider::symbol_for(primary, target);
    let quote = match quotes.get(&symbol) {
        Some(QuoteState::Ready(q)) => q.as_ref(),
        _ => {
            let msg = format!("Loading {primary}/{target}…");
            let para = Paragraph::new(Line::from(Span::styled(msg, theme.text_dim)))
                .alignment(Alignment::Center);
            let centered = Rect {
                x: area.x,
                y: area.y + area.height / 2,
                width: area.width,
                height: 1,
            };
            frame.render_widget(para, centered);
            return toggle_hits;
        }
    };

    let header_h = 2u16;
    let xaxis_h = 1u16;
    let plot_top = area.y + header_h;
    let plot_h = area.height.saturating_sub(header_h + xaxis_h);

    let (chg, pct) = period_change(quote, period);
    let (color, glyph) = if chg >= 0.0 {
        (Color::Green, '▲')
    } else {
        (Color::Red, '▼')
    };
    // Scale the header's rate + absolute-change values by the
    // primary's canonical unit so micro-rates (KRW, IDR, VND) display
    // at a human-readable magnitude. The percentage change is a ratio
    // and stays as-is.
    let scaled_price = quote.price * primary_unit;
    let scaled_chg = chg * primary_unit;
    let unit_label = format_unit_count(primary_unit);
    let header = Line::from(vec![
        Span::styled(
            format!("{unit_label} {primary} = {:.4} {target}", scaled_price),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{glyph} {:+.4} ({:+.2}%) {}", scaled_chg, pct, period.label()),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(header),
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        },
    );

    if plot_h == 0 || quote.series.is_empty() {
        return toggle_hits;
    }

    let (mut min, mut max) = (f64::INFINITY, f64::NEG_INFINITY);
    for v in &quote.series {
        if *v < min {
            min = *v;
        }
        if *v > max {
            max = *v;
        }
    }
    if min == max {
        min -= 0.0001;
        max += 0.0001;
    }

    const Y_LABEL_W: u16 = 9;
    let plot_x = area.x + Y_LABEL_W;
    let plot_w = area.width.saturating_sub(Y_LABEL_W);
    if plot_w < 4 {
        return toggle_hits;
    }
    for row in label_rows(plot_h) {
        let frac = row as f64 / (plot_h as f64 - 1.0).max(1.0);
        // Scale the y-axis tick value by the primary's canonical unit
        // so it matches the header. The graph trace itself stays
        // pinned to the raw `[min, max]` series — only the labels
        // are scaled, which is fine since scaling is linear and the
        // visual position of each tick is unchanged.
        let v = (max - frac * (max - min)) * primary_unit;
        let rect = Rect {
            x: area.x,
            y: plot_top + row,
            width: Y_LABEL_W,
            height: 1,
        };
        let label = format!("{:>7.4} ", v);
        frame.render_widget(
            Paragraph::new(Span::styled(label, theme.text_dim)),
            rect,
        );
    }

    let trace_w = if pad_intraday_to_full_day && matches!(period, Period::Day) {
        // FX is ~24/5 — proxy "fraction of day elapsed" via UTC hour.
        let frac = (chrono::Utc::now()
            .timestamp()
            .rem_euclid(86_400) as f64
            / 86_400.0)
            .clamp(0.0, 1.0);
        let w = (plot_w as f64 * frac).round() as u16;
        w.clamp(2, plot_w)
    } else if matches!(period, Period::YearToDate) {
        // YTD: x-axis spans Jan→Dec but the trace only covers Jan→today.
        // Without this clamp the YTD series would get stretched across
        // the full plot width, visually claiming data through Dec when
        // there isn't any.
        use chrono::Datelike;
        let now = chrono::Local::now();
        let day_of_year = now.ordinal() as f64; // 1..=366
        let days_in_year = if is_leap_year(now.year()) { 366.0 } else { 365.0 };
        let frac = (day_of_year / days_in_year).clamp(0.0, 1.0);
        let w = (plot_w as f64 * frac).round() as u16;
        w.clamp(2, plot_w)
    } else {
        plot_w
    };
    let rows = graph::render_series(&quote.series, plot_h, trace_w, min, max);
    for (i, row) in rows.iter().enumerate() {
        let rect = Rect {
            x: plot_x,
            y: plot_top + i as u16,
            width: trace_w,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                row.clone(),
                Style::default().fg(if chg >= 0.0 {
                    Color::LightGreen
                } else {
                    Color::LightRed
                }),
            )),
            rect,
        );
    }

    // Anchor line at the start-of-period value: previous-close for 1D
    // (standard intraday convention), otherwise the first sample of
    // the visible series. Painted in dim yellow so it stays visible
    // against the trace without competing for attention.
    let anchor_value = if matches!(period, Period::Day) {
        quote.previous_close
    } else {
        quote.series.first().copied().unwrap_or(quote.previous_close)
    };
    draw_reference_line(
        frame,
        plot_x,
        plot_top,
        plot_h,
        plot_w,
        min,
        max,
        &rows,
        anchor_value,
        '┄',
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::DIM),
    );

    if show_high_low_lines && !matches!(period, Period::Day) {
        let faint = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM);
        draw_reference_line(
            frame, plot_x, plot_top, plot_h, plot_w, min, max, &rows, max, '┈', faint,
        );
        draw_reference_line(
            frame, plot_x, plot_top, plot_h, plot_w, min, max, &rows, min, '┈', faint,
        );
    }

    // x-axis labels
    let xaxis_rect = Rect {
        x: plot_x,
        y: plot_top + plot_h,
        width: plot_w,
        height: 1,
    };
    // 1Y is a rolling 12-month window ending today — labels walk back
    // from *this* month, two months at a time, 7 labels total. Static
    // calendar-year labels (Jan Mar May Jul Sep Nov) would misalign
    // any 1Y graph that doesn't start in January.
    // YTD adds a `Dec` label at the right edge so the year visibly
    // spans Jan→Dec; the trace itself only covers the elapsed
    // fraction (see `trace_w` above).
    let labels: Vec<String> = match period {
        Period::Day => str_labels(&["00", "04", "08", "12", "16", "20"]),
        Period::Week => str_labels(&["Mon", "Tue", "Wed", "Thu", "Fri"]),
        Period::Month => str_labels(&["wk1", "wk2", "wk3", "wk4"]),
        Period::SixMonth => str_labels(&["1mo", "2mo", "3mo", "4mo", "5mo", "6mo"]),
        Period::YearToDate => {
            str_labels(&["Jan", "Mar", "May", "Jul", "Sep", "Nov", "Dec"])
        }
        Period::Year => rolling_year_month_labels(chrono::Local::now().date_naive()),
        Period::ThreeYear => str_labels(&["-3y", "-2y", "-1y", "now"]),
        Period::FiveYear => str_labels(&["-5y", "-4y", "-3y", "-2y", "-1y", "now"]),
        Period::TenYear => str_labels(&["-10y", "-8y", "-6y", "-4y", "-2y", "now"]),
    };
    let label_refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
    // Distribute labels so the first one's left edge is at column 0
    // and the last one's right edge sits at the plot's right edge.
    let line = lay_out_x_axis_labels(&label_refs, plot_w as usize);
    frame.render_widget(
        Paragraph::new(Span::styled(line, theme.text_dim)),
        xaxis_rect,
    );

    toggle_hits
}

/// Pack `labels` into a single string `width` cells wide where the
/// first label is left-anchored at column 0 and the last label is
/// right-anchored at column `width`. Intermediate labels are spaced
/// linearly. Each label is placed at its computed left-column; any
/// trailing characters that would overflow `width` are clipped.
/// Convenience: copy a static `&[&str]` into an owned `Vec<String>` so
/// the x-axis labels match arm can mix static + dynamic sets without
/// fighting lifetimes.
fn str_labels(labels: &[&str]) -> Vec<String> {
    labels.iter().map(|s| (*s).to_string()).collect()
}

/// 7 month-name labels for a rolling 12-month window ending today,
/// stepped 2 months at a time so they spread evenly across the
/// `lay_out_x_axis_labels` right-anchored layout (which spaces 7
/// labels into 6 equal intervals = 12 months / 2 months per gap).
/// e.g. today=2026-05-23 → `["May","Jul","Sep","Nov","Jan","Mar","May"]`.
fn rolling_year_month_labels(today: chrono::NaiveDate) -> Vec<String> {
    use chrono::Datelike;
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

fn month_short_name(m: u32) -> &'static str {
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

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

fn lay_out_x_axis_labels(labels: &[&str], width: usize) -> String {
    if labels.is_empty() || width == 0 {
        return String::new();
    }
    let n = labels.len();
    if n == 1 {
        // Single label sits flush-left.
        return labels[0].chars().take(width).collect();
    }
    let last_w = labels.last().map(|s| s.chars().count()).unwrap_or(0);
    // Right-anchor budget: the last label's left edge can be at most
    // `width - last_w` so its right edge lands at exactly `width`.
    // Other labels span evenly from 0 to that position.
    let usable = width.saturating_sub(last_w);
    let mut line = String::with_capacity(width);
    for (i, lbl) in labels.iter().enumerate() {
        let target = (i * usable) / (n - 1);
        while line.chars().count() < target {
            line.push(' ');
        }
        for c in lbl.chars() {
            if line.chars().count() >= width {
                break;
            }
            line.push(c);
        }
    }
    line
}

#[allow(clippy::too_many_arguments)]
fn draw_reference_line(
    frame: &mut Frame,
    plot_x: u16,
    plot_top: u16,
    plot_h: u16,
    plot_w: u16,
    plot_min: f64,
    plot_max: f64,
    trace_rows: &[String],
    value: f64,
    ch: char,
    style: Style,
) {
    if plot_h == 0 || !value.is_finite() || plot_max <= plot_min {
        return;
    }
    if value < plot_min || value > plot_max {
        return;
    }
    let frac = (plot_max - value) / (plot_max - plot_min);
    let ref_row = (frac * (plot_h as f64 - 1.0)).round() as usize;
    if ref_row >= trace_rows.len() {
        return;
    }
    let trace = &trace_rows[ref_row];
    let trace_chars: Vec<char> = trace.chars().collect();
    let y = plot_top + ref_row as u16;
    let buf = frame.buffer_mut();
    for i in 0..plot_w as usize {
        let trace_owns_cell = match trace_chars.get(i) {
            Some(&c) => c != ' ',
            None => false,
        };
        if trace_owns_cell {
            continue;
        }
        let x = plot_x + i as u16;
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_char(ch);
            cell.set_style(style);
        }
    }
}

/// Render the period toggle bar and return the screen-absolute column
/// ranges for each rendered toggle so `handle_mouse` can route a click
/// on `[1Y]` straight into `set_period`.
fn render_period_toggle_bar(
    frame: &mut Frame,
    area: Rect,
    active: Period,
    theme: &Theme,
) -> ToggleHits {
    if area.width == 0 {
        return ToggleHits { row: area.y, ranges: Vec::new() };
    }
    let active_idx = Period::ALL.iter().position(|p| *p == active).unwrap_or(0);
    let widths: Vec<u16> = Period::ALL
        .iter()
        .map(|p| (p.label().len() as u16) + 2 + 1)
        .collect();
    let total: u16 = widths.iter().sum::<u16>().saturating_sub(1);
    let mut spans: Vec<Span<'_>> = vec![Span::raw(" ")];
    let mut ranges: Vec<(Period, u16, u16)> = Vec::with_capacity(Period::ALL.len());
    // Cursor at start of next span we're about to push, relative to
    // `area.x`. Used to record each `[label]` toggle's screen range.
    let mut cursor: u16 = area.x + 1; // leading raw space
    if (total + 2) <= area.width {
        for (i, p) in Period::ALL.iter().enumerate() {
            let style = if i == active_idx {
                Style::default()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                theme.text_dim
            };
            let label = format!("[{}]", p.label());
            let label_w = label.chars().count() as u16;
            ranges.push((*p, cursor, cursor + label_w));
            cursor += label_w;
            spans.push(Span::styled(label, style));
            if i + 1 < Period::ALL.len() {
                spans.push(Span::raw(" "));
                cursor += 1;
            }
        }
    } else {
        let budget = area.width.saturating_sub(4);
        let mut start = active_idx;
        let mut end = active_idx + 1;
        let mut used = widths[active_idx];
        while end < Period::ALL.len() && used + widths[end] <= budget {
            used += widths[end];
            end += 1;
        }
        while start > 0 && used + widths[start - 1] <= budget {
            used += widths[start - 1];
            start -= 1;
        }
        let dim = theme.text_dim;
        if start > 0 {
            spans.push(Span::styled("‹", dim));
            spans.push(Span::raw(" "));
            cursor += 2;
        }
        for i in start..end {
            let style = if i == active_idx {
                Style::default()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                dim
            };
            let label = format!("[{}]", Period::ALL[i].label());
            let label_w = label.chars().count() as u16;
            ranges.push((Period::ALL[i], cursor, cursor + label_w));
            cursor += label_w;
            spans.push(Span::styled(label, style));
            if i + 1 < end {
                spans.push(Span::raw(" "));
                cursor += 1;
            }
        }
        if end < Period::ALL.len() {
            spans.push(Span::raw(" "));
            spans.push(Span::styled("›", dim));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    ToggleHits { row: area.y, ranges }
}

fn label_rows(plot_h: u16) -> Vec<u16> {
    if plot_h == 0 {
        return Vec::new();
    }
    if plot_h == 1 {
        return vec![0];
    }
    let fracs: &[f64] = match plot_h {
        2..=3 => &[0.0, 1.0],
        4..=6 => &[0.0, 0.5, 1.0],
        _ => &[0.0, 0.25, 0.5, 0.75, 1.0],
    };
    let mut rows: Vec<u16> = Vec::with_capacity(fracs.len());
    for f in fracs {
        let row = (f * (plot_h as f64 - 1.0)).round() as u16;
        let row = row.min(plot_h - 1);
        if !rows.contains(&row) {
            rows.push(row);
        }
    }
    rows
}

// `primary_unit` — same scaling factor as the graph (see
// `render_graph_panel`). All rate-shaped stats (Rate, Prev Close, Day
// O/H/L, Day Δ absolute, 52w H/L) get multiplied by this; ratios
// (% change, vs-H/L, volatility) stay raw.
fn render_stats_panel(
    frame: &mut Frame,
    area: Rect,
    selected: Option<&str>,
    primary: &str,
    primary_unit: f64,
    quotes: &HashMap<String, QuoteState>,
    theme: &Theme,
) {
    let Some(target) = selected else {
        let para = Paragraph::new(Span::styled("(no stats)", theme.text_dim))
            .alignment(Alignment::Center);
        frame.render_widget(para, area);
        return;
    };
    if target == primary {
        let para = Paragraph::new(Span::styled(
            "(primary — pick another row)",
            theme.text_dim,
        ))
        .alignment(Alignment::Center);
        frame.render_widget(para, area);
        return;
    }
    let symbol = YahooForexProvider::symbol_for(primary, target);
    let q = match quotes.get(&symbol) {
        Some(QuoteState::Ready(q)) => q.as_ref(),
        _ => {
            let para = Paragraph::new(Span::styled("(loading)", theme.text_dim))
                .alignment(Alignment::Center);
            frame.render_widget(para, area);
            return;
        }
    };

    let mut lines: Vec<Line<'_>> = Vec::new();
    // Top padding so the title doesn't kiss the top border. Stats
    // never approach the bottom of the cell so we don't need a
    // matching bottom pad.
    lines.push(Line::from(""));
    // Title surfaces the scaling factor when it's not 1 ("KRW/USD
    // (per 1000)") so the reader knows the numbers below are scaled
    // and matches what the graph header shows.
    let title = if (primary_unit - 1.0).abs() < 1e-9 {
        format!("{}/{}", primary, target)
    } else {
        format!("{}/{} (per {})", primary, target, format_unit_count(primary_unit))
    };
    lines.push(Line::from(Span::styled(title, theme.text_focused)));
    lines.push(Line::from(""));
    lines.push(stat_line("Rate", &format!("{:.6}", q.price * primary_unit), theme));
    lines.push(stat_line(
        "Prev Close",
        &format!("{:.6}", q.previous_close * primary_unit),
        theme,
    ));
    if let (Some(o), Some(h), Some(l)) = (
        q.series.first().copied(),
        q.day_high,
        q.day_low,
    ) {
        lines.push(stat_line(
            "Day Open",
            &format!("{:.4}", o * primary_unit),
            theme,
        ));
        lines.push(stat_line(
            "Day H/L",
            &format!("{:.4} / {:.4}", h * primary_unit, l * primary_unit),
            theme,
        ));
    } else if let (Some(h), Some(l)) = (q.day_high, q.day_low) {
        lines.push(stat_line(
            "Day H/L",
            &format!("{:.4} / {:.4}", h * primary_unit, l * primary_unit),
            theme,
        ));
    }
    lines.push(stat_line(
        "Day Δ",
        &format!(
            "{:+.4} ({:+.2}%)",
            q.change() * primary_unit,
            q.change_pct()
        ),
        theme,
    ));
    if let (Some(h), Some(l)) = (q.fifty_two_week_high, q.fifty_two_week_low) {
        lines.push(stat_line(
            "52w H/L",
            &format!("{:.4} / {:.4}", h * primary_unit, l * primary_unit),
            theme,
        ));
        let from_h = (h - q.price) / h * 100.0;
        let from_l = (q.price - l) / l * 100.0;
        lines.push(stat_line(
            "vs H/L",
            &format!("{:+.2}% / {:+.2}%", -from_h, from_l),
            theme,
        ));
    }
    // 1-year change %: if the active period's series is long enough,
    // compare first and last sample. Otherwise display "—".
    if !q.series.is_empty() {
        let baseline = q.series.first().copied().unwrap_or(q.price);
        if baseline > 0.0 {
            let pct = (q.price - baseline) / baseline * 100.0;
            lines.push(stat_line(
                "Period Δ",
                &format!("{pct:+.2}%"),
                theme,
            ));
        }
    }
    if let Some(vol) = rolling_volatility_30d(&q.series) {
        lines.push(stat_line("30d Vol", &format!("{:.4}%", vol * 100.0), theme));
    }
    lines.push(stat_line(
        "Updated",
        &q.fetched_at.format("%H:%M:%S").to_string(),
        theme,
    ));

    frame.render_widget(Paragraph::new(lines), area);
}

fn stat_line(label: &str, value: &str, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{:<11}", label), theme.text_dim),
        Span::styled(value.to_string(), theme.text_plain),
    ])
}

/// Period change %: 1D uses prev-close convention; longer windows
/// compare to the first sample of the series.
fn period_change(q: &ForexQuote, period: Period) -> (f64, f64) {
    match period {
        Period::Day => (q.change(), q.change_pct()),
        _ => {
            let baseline = q
                .series
                .iter()
                .copied()
                .find(|v| v.is_finite() && *v > 0.0);
            match baseline {
                Some(b) if b > 0.0 => {
                    let abs = q.price - b;
                    let pct = (q.price - b) / b * 100.0;
                    (abs, pct)
                }
                _ => (q.change(), q.change_pct()),
            }
        }
    }
}

/// Sample standard deviation of daily log-returns over the most recent
/// 30 observations. Returned as a fraction (multiply by 100 for %).
/// Falls back to `None` when the series is too short or has zeros.
fn rolling_volatility_30d(series: &[f64]) -> Option<f64> {
    if series.len() < 31 {
        return None;
    }
    let window: Vec<f64> = series
        .windows(2)
        .rev()
        .take(30)
        .filter_map(|w| {
            if w[0] > 0.0 && w[1] > 0.0 {
                Some((w[1] / w[0]).ln())
            } else {
                None
            }
        })
        .collect();
    if window.len() < 5 {
        return None;
    }
    let mean = window.iter().sum::<f64>() / window.len() as f64;
    let var = window
        .iter()
        .map(|v| {
            let d = v - mean;
            d * d
        })
        .sum::<f64>()
        / (window.len() - 1) as f64;
    Some(var.sqrt())
}

// ─────────────────────────────────────────────────────────────────────
// Small helpers
// ─────────────────────────────────────────────────────────────────────

/// Format a canonical unit count for display in the graph header.
/// Whole-number units (1, 100, 1000) render without trailing zeros;
/// odd values (rare — only via the `[canonical_units]` TOML override)
/// fall back to a generic 4dp formatter.
fn format_unit_count(unit: f64) -> String {
    if unit.fract().abs() < 1e-9 && unit.abs() < 1e9 {
        format!("{:.0}", unit)
    } else {
        format!("{:.4}", unit)
    }
}

fn canonical_amount(config: &ForexConfig, code: &str) -> f64 {
    config
        .canonical_units
        .get(code)
        .copied()
        .unwrap_or_else(|| default_canonical_unit(code))
}

/// Combined alternates list the widget keeps: every fiat code from
/// `watchlist` first, then every crypto code from `crypto_watchlist`,
/// each uppercased and de-duplicated (first wins), with the
/// `primary` code filtered out. Returns the flat list plus the
/// index at which the crypto section starts — used by the list
/// renderer to emit `── Currencies ──` / `── Crypto ──` headers
/// at the right boundaries without re-classifying each code.
fn build_alternates(
    fiat_codes: &[String],
    crypto_codes: &[String],
    primary: &str,
) -> (Vec<String>, usize) {
    let mut out: Vec<String> = Vec::new();
    let push = |code: &str, out: &mut Vec<String>| {
        let upper = code.to_ascii_uppercase();
        if upper == primary {
            return;
        }
        if !out.iter().any(|c| c == &upper) {
            out.push(upper);
        }
    };
    for code in fiat_codes {
        push(code, &mut out);
    }
    let crypto_start = out.len();
    for code in crypto_codes {
        push(code, &mut out);
    }
    (out, crypto_start)
}

fn format_amount(v: f64) -> String {
    if v.fract().abs() < 1e-9 && v.abs() < 1e9 {
        format!("{:>12.0}", v)
    } else {
        format!("{:>12.4}", v)
    }
}

/// Right-justify the in-progress edit buffer (e.g. "1523.80") inside
/// a `width`-cell field. **No cursor is appended** — the cursor lives
/// in the gap cell between the value and the `]` bracket, drawn by
/// the row builder. Keeping it out of the value field means the
/// digits stay anchored to the same columns they occupy in non-edit
/// mode, so toggling `e` doesn't visually shift them.
///
/// When the buffer outgrows the field we keep the rightmost chars
/// (drop leftmost) so the most recent typed char remains visible at
/// the right edge — the user is editing forward, not backward.
fn fit_edit_buffer_right(buf: &str, width: usize) -> String {
    let n = buf.chars().count();
    if n >= width {
        buf.chars().skip(n - width).collect()
    } else {
        format!("{}{}", " ".repeat(width - n), buf)
    }
}

fn is_iso_currency_codish(s: &str) -> bool {
    s.len() == 3 && s.chars().all(|c| c.is_ascii_uppercase())
}

pub const KIND: &str = "forex";

pub fn build(ctx: &super::WidgetCtx) -> Box<dyn super::Widget> {
    let cfg: ForexConfig =
        crate::config::load_widget_toml_for_instance(KIND, &ctx.instance).unwrap_or_default();
    Box::new(ForexWidget::with_config(
        ctx.instance.clone(),
        cfg,
        ctx.theme.clone(),
        ctx.cache.clone(),
    ))
}

pub fn wizard_descriptor() -> crate::wizard::descriptor::WizardDescriptor {
    use crate::wizard::descriptor::{ChoiceOption, Separator, WizardDescriptor, WizardField, WizardFieldKind};
    let primary_options: Vec<ChoiceOption> = COMMON_CURRENCIES
        .iter()
        .map(|c| ChoiceOption {
            value: c,
            label: c,
            help: None,
        })
        .collect();
    WizardDescriptor {
        display_name: "Forex",
        blurb: "Foreign-exchange watchlist with editable amount + historical \
                charts via Yahoo Finance. Pick a primary currency; the rest \
                of the list shows what `amount` of primary equals in each.",
        load_from_toml: None,
        render_toml: None,
        fields: vec![
            WizardField {
                key: "primary",
                label: "Primary currency",
                help: "ISO-4217 code. All list rows show what \
                       `amount` of this currency equals in their respective \
                       quote currencies. Press a row's ↔ to swap at runtime.",
                required: true,
                kind: WizardFieldKind::Choice {
                    options: primary_options,
                    default: Some("USD"),
                },
                validate: None,
            },
            WizardField {
                key: "watchlist",
                label: "Quote currencies (comma-separated)",
                help: "ISO-4217 codes of the currencies to display as \
                       quotes of the primary (e.g. EUR, GBP, JPY).",
                required: false,
                kind: WizardFieldKind::TextList {
                    default: vec![
                        "EUR".into(),
                        "GBP".into(),
                        "JPY".into(),
                        "CAD".into(),
                        "AUD".into(),
                        "CHF".into(),
                        "CNY".into(),
                    ],
                    separator: Separator::Comma,
                },
                validate: None,
            },
            WizardField {
                key: "poll_interval_secs",
                label: "Rate refresh interval (seconds)",
                help: "How often to repoll Yahoo for live FX rates. \
                       Forex moves slower than equities; 600s (10 min) is \
                       comfortable, 60s is the floor.",
                required: true,
                kind: WizardFieldKind::Number {
                    default: Some(600.0),
                    range: Some((60.0, 3600.0)),
                    integer: true,
                },
                validate: None,
            },
            WizardField {
                key: "default_period",
                label: "Initial graph period",
                help: "Time window the graph opens with. Press 1-9 / ←→ in \
                       the widget to cycle at runtime.",
                required: true,
                kind: WizardFieldKind::Choice {
                    options: vec![
                        ChoiceOption { value: "1d", label: "1 day", help: None },
                        ChoiceOption { value: "1w", label: "1 week", help: None },
                        ChoiceOption { value: "1m", label: "1 month", help: None },
                        ChoiceOption { value: "6m", label: "6 months", help: None },
                        ChoiceOption { value: "ytd", label: "Year to date", help: None },
                        ChoiceOption { value: "1y", label: "1 year", help: None },
                        ChoiceOption { value: "3y", label: "3 years", help: None },
                        ChoiceOption { value: "5y", label: "5 years", help: None },
                        ChoiceOption { value: "10y", label: "10 years", help: None },
                    ],
                    default: Some("1y"),
                },
                validate: None,
            },
        ],
    }
}

/// ~30 most-traded ISO-4217 currencies — wizard `Choice` options. Keeps
/// the dropdown navigable without forcing free-text input.
const COMMON_CURRENCIES: &[&str] = &[
    "USD", "EUR", "GBP", "JPY", "CHF", "CAD", "AUD", "NZD", "CNY", "HKD",
    "SGD", "KRW", "TWD", "INR", "MXN", "BRL", "ZAR", "TRY", "RUB", "PLN",
    "SEK", "NOK", "DKK", "CZK", "HUF", "ILS", "THB", "IDR", "MYR", "PHP",
    "VND", "AED", "SAR",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_alternates_concats_lists_dedupes_filters_primary() {
        let fiat: Vec<String> = ["EUR", "JPY", "USD", "eur"]
            .iter().map(|s| s.to_string()).collect();
        let crypto: Vec<String> = ["BTC", "ETH", "btc"]
            .iter().map(|s| s.to_string()).collect();
        let (out, cs) = build_alternates(&fiat, &crypto, "USD");
        assert_eq!(out, vec!["EUR", "JPY", "BTC", "ETH"]);
        assert_eq!(cs, 2, "crypto section starts after the two fiat entries");
    }

    #[test]
    fn build_alternates_returns_empty_when_all_are_primary() {
        let fiat = vec!["usd".into(), "USD".into()];
        let crypto: Vec<String> = vec![];
        let (out, cs) = build_alternates(&fiat, &crypto, "USD");
        assert!(out.is_empty());
        assert_eq!(cs, 0);
    }

    #[test]
    fn build_alternates_handles_only_crypto() {
        let (out, cs) = build_alternates(&[], &["BTC".into(), "ETH".into()], "USD");
        assert_eq!(out, vec!["BTC", "ETH"]);
        assert_eq!(cs, 0, "no fiat entries → crypto starts at index 0");
    }

    fn build_widget(cfg: ForexConfig) -> ForexWidget {
        ForexWidget::with_config(
            "main".to_string(),
            cfg,
            Arc::new(Theme::builtin_defaults()),
            ScopedCache::ephemeral(),
        )
    }

    fn rate(base: &str, quote: &str, price: f64) -> ForexQuote {
        ForexQuote {
            symbol: YahooForexProvider::symbol_for(base, quote),
            base: base.into(),
            quote: quote.into(),
            price,
            previous_close: price,
            day_high: None,
            day_low: None,
            fifty_two_week_high: None,
            fifty_two_week_low: None,
            series: vec![],
            fetched_at: chrono::Local::now(),
        }
    }

    #[test]
    fn default_canonical_unit_handles_jpy_and_krw() {
        assert_eq!(default_canonical_unit("USD"), 1.0);
        assert_eq!(default_canonical_unit("EUR"), 1.0);
        assert_eq!(default_canonical_unit("JPY"), 100.0);
        assert_eq!(default_canonical_unit("KRW"), 1000.0);
        assert_eq!(default_canonical_unit("VND"), 10_000.0);
        // Unknown codes fall back to 1.
        assert_eq!(default_canonical_unit("ZZZ"), 1.0);
    }

    #[test]
    fn config_canonical_unit_override_wins_over_default() {
        let mut cfg = ForexConfig::default();
        cfg.canonical_units.insert("JPY".into(), 1.0); // override "100"
        assert_eq!(canonical_amount(&cfg, "JPY"), 1.0);
        // Code not in override map still falls through to default.
        assert_eq!(canonical_amount(&cfg, "KRW"), 1000.0);
    }

    #[test]
    fn initial_amount_is_canonical_for_configured_primary() {
        let cfg = ForexConfig {
            primary: "JPY".into(),
            crypto_watchlist: Vec::new(),
            ..Default::default()
        };
        let w = build_widget(cfg);
        assert_eq!(w.amount, 100.0);
        assert_eq!(w.primary, "JPY");
    }

    #[test]
    fn all_rows_lists_primary_first_then_non_duplicate_watchlist() {
        let cfg = ForexConfig {
            primary: "USD".into(),
            watchlist: vec!["EUR".into(), "USD".into(), "JPY".into()],
            crypto_watchlist: Vec::new(),
            ..Default::default()
        };
        let w = build_widget(cfg);
        // Primary shouldn't appear twice even when present in the watchlist.
        assert_eq!(w.all_rows(), vec!["USD", "EUR", "JPY"]);
    }

    #[test]
    fn swap_primary_preserves_absolute_value_via_current_rate() {
        let cfg = ForexConfig {
            primary: "USD".into(),
            watchlist: vec!["EUR".into(), "JPY".into()],
            crypto_watchlist: Vec::new(),
            ..Default::default()
        };
        let mut w = build_widget(cfg);
        w.amount = 1523.80;
        // Pretend the rate is 1 USD = 0.9237 EUR.
        let usd_eur = rate("USD", "EUR", 0.9237);
        w.state
            .lock()
            .unwrap()
            .quotes
            .insert("USDEUR=X".into(), QuoteState::Ready(Box::new(usd_eur)));
        w.swap_primary("EUR");
        assert_eq!(w.primary, "EUR");
        // 1523.80 USD × 0.9237 = 1407.53406 EUR
        assert!((w.amount - 1407.53406).abs() < 0.001, "amount={}", w.amount);
    }

    #[test]
    fn swap_to_crypto_primary_forces_amount_to_one() {
        // Fiat-amount preservation is the wrong default for crypto:
        // "1523.80 USD × 1/90,000 BTC ≈ 0.01693 BTC" buries the
        // per-unit comparison the user actually wants from a crypto
        // primary. We always seed crypto primaries with 1.0.
        let mut w = build_widget(ForexConfig {
            primary: "USD".into(),
            watchlist: vec!["EUR".into()],
            crypto_watchlist: vec!["BTC".into()],
            ..Default::default()
        });
        w.amount = 1523.80;
        // Seed a rate so the converted-amount path would otherwise
        // produce a non-1.0 value; the crypto branch must override.
        let usd_btc = rate("USD", "BTC", 1.0 / 90_000.0);
        w.state.lock().unwrap().quotes.insert(
            YahooForexProvider::symbol_for("USD", "BTC"),
            QuoteState::Ready(Box::new(usd_btc)),
        );
        w.swap_primary("BTC");
        assert_eq!(w.primary, "BTC");
        assert_eq!(w.amount, 1.0, "crypto primary always seeds amount=1.0");
    }

    #[test]
    fn swap_primary_auto_selects_old_primary_row() {
        let cfg = ForexConfig {
            primary: "USD".into(),
            watchlist: vec!["EUR".into(), "JPY".into()],
            crypto_watchlist: Vec::new(),
            ..Default::default()
        };
        let mut w = build_widget(cfg);
        // Seed a rate so swap math doesn't fall back to canonical.
        w.state.lock().unwrap().quotes.insert(
            "USDEUR=X".into(),
            QuoteState::Ready(Box::new(rate("USD", "EUR", 0.9))),
        );
        w.swap_primary("EUR");
        let rows = w.all_rows();
        let sel = w.state.lock().unwrap().selected;
        assert_eq!(rows.get(sel).map(|s| s.as_str()), Some("USD"));
    }

    #[test]
    fn swap_key_promotes_selected_row_to_primary() {
        // Drive the `s` shortcut through handle_key so we exercise the
        // same path the user does, not just the helper.
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut w = build_widget(ForexConfig {
            primary: "USD".into(),
            watchlist: vec!["EUR".into(), "JPY".into()],
            crypto_watchlist: Vec::new(),
            ..Default::default()
        });
        // Seed a rate so swap_primary has something to convert through.
        w.state.lock().unwrap().quotes.insert(
            "USDEUR=X".into(),
            QuoteState::Ready(Box::new(rate("USD", "EUR", 0.9))),
        );
        // Move selection to EUR (row 1) and hit `s`.
        w.state.lock().unwrap().selected = 1;
        let _ = w.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(w.primary, "EUR");
    }

    #[test]
    fn enter_key_also_promotes_selected_row_to_primary() {
        // Enter is an alias for `s` (the more discoverable / bigger
        // key gets the more common action).
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut w = build_widget(ForexConfig {
            primary: "USD".into(),
            watchlist: vec!["EUR".into(), "JPY".into()],
            crypto_watchlist: Vec::new(),
            ..Default::default()
        });
        w.state.lock().unwrap().quotes.insert(
            "USDEUR=X".into(),
            QuoteState::Ready(Box::new(rate("USD", "EUR", 0.9))),
        );
        w.state.lock().unwrap().selected = 1;
        let _ = w.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(w.primary, "EUR");
    }

    #[test]
    fn swap_key_is_safe_when_no_alternates_exist() {
        // Empty watchlist → only the primary row, selection stuck at 0.
        // Pressing `s` shouldn't swap (it would try to swap USD with
        // USD, which is the no-op branch of swap_primary).
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut w = build_widget(ForexConfig {
            primary: "USD".into(),
            watchlist: vec![],
            crypto_watchlist: Vec::new(),
            ..Default::default()
        });
        let amount_before = w.amount;
        let _ = w.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(w.primary, "USD");
        assert_eq!(w.amount, amount_before);
    }

    #[test]
    fn move_selection_skips_primary_row() {
        // Initial selected = 1 (first alternate). ↑ should stay at 1
        // rather than slipping down to 0 (the primary row, which has
        // no graph / stats to surface).
        let mut w = build_widget(ForexConfig {
            primary: "USD".into(),
            watchlist: vec!["EUR".into(), "JPY".into()],
            crypto_watchlist: Vec::new(),
            ..Default::default()
        });
        assert_eq!(w.state.lock().unwrap().selected, 1);
        w.move_selection(-1);
        assert_eq!(w.state.lock().unwrap().selected, 1, "↑ at top should not drop to primary");
        w.move_selection(1);
        assert_eq!(w.state.lock().unwrap().selected, 2);
        // Two moves down past the last alternate clamps at last row.
        w.move_selection(5);
        assert_eq!(w.state.lock().unwrap().selected, 2);
    }

    #[test]
    fn swap_to_same_primary_is_no_op() {
        let mut w = build_widget(ForexConfig::default());
        let amount_before = w.amount;
        let primary_before = w.primary.clone();
        w.swap_primary("USD"); // already primary
        assert_eq!(w.amount, amount_before);
        assert_eq!(w.primary, primary_before);
    }

    #[test]
    fn reset_amount_returns_to_canonical_for_current_primary() {
        let mut w = build_widget(ForexConfig {
            primary: "USD".into(),
            crypto_watchlist: Vec::new(),
            ..Default::default()
        });
        w.amount = 9999.0;
        w.reset_amount();
        assert_eq!(w.amount, 1.0);
    }

    #[test]
    fn edit_commit_parses_decimal_buffer() {
        let mut w = build_widget(ForexConfig::default());
        w.enter_edit_mode();
        {
            let mut st = w.state.lock().unwrap();
            st.editing_amount = Some("1523.80".to_string());
        }
        let changed = w.commit_edit();
        assert!(changed);
        assert!((w.amount - 1523.80).abs() < f64::EPSILON);
        // Edit buffer cleared after commit.
        assert!(w.state.lock().unwrap().editing_amount.is_none());
    }

    #[test]
    fn edit_cancel_drops_buffer_without_changing_amount() {
        let mut w = build_widget(ForexConfig::default());
        let amount_before = w.amount;
        w.enter_edit_mode();
        {
            let mut st = w.state.lock().unwrap();
            st.editing_amount = Some("999".to_string());
        }
        w.cancel_edit();
        assert_eq!(w.amount, amount_before);
        assert!(w.state.lock().unwrap().editing_amount.is_none());
    }

    #[test]
    fn edit_commit_rejects_garbage_and_negative_input() {
        let mut w = build_widget(ForexConfig::default());
        let amount_before = w.amount;
        w.enter_edit_mode();
        {
            let mut st = w.state.lock().unwrap();
            st.editing_amount = Some("not a number".to_string());
        }
        assert!(!w.commit_edit());
        assert_eq!(w.amount, amount_before);

        w.enter_edit_mode();
        {
            let mut st = w.state.lock().unwrap();
            st.editing_amount = Some("-5".to_string());
        }
        assert!(!w.commit_edit());
        assert_eq!(w.amount, amount_before);
    }

    #[test]
    fn fx_command_known_code_bounces_selection_does_not_swap() {
        // EUR is already in the alternates list, so :fx EUR just
        // selects EUR's row — it does NOT promote it to primary. Swap
        // is an explicit `p` / click-↔ gesture.
        let mut w = build_widget(ForexConfig {
            primary: "USD".into(),
            watchlist: vec!["EUR".into(), "JPY".into()],
            crypto_watchlist: Vec::new(),
            ..Default::default()
        });
        // Move selection off EUR first so we can prove the bounce works.
        w.state.lock().unwrap().selected = 2; // JPY row
        w.handle_fx_command(&["EUR"]).unwrap();
        assert_eq!(w.primary, "USD", ":fx must not swap");
        assert_eq!(
            w.state.lock().unwrap().selected,
            1,
            ":fx on a known code should bounce selection to its row"
        );
    }

    #[test]
    fn fx_command_rejects_non_iso_codish_input() {
        let mut w = build_widget(ForexConfig::default());
        assert!(w.handle_fx_command(&["dollar"]).is_err());
        assert!(w.handle_fx_command(&["YENS!"]).is_err());
        assert!(w.handle_fx_command(&[""]).is_err());
        assert!(w.handle_fx_command(&[]).is_err());
    }

    #[test]
    fn fx_command_unknown_code_pins_as_transient_lookup_does_not_swap() {
        // NOK isn't in the watchlist — :fx NOK pins it as the Lookup
        // row but leaves USD as primary. User must press `p` or click
        // ↔ to actually promote.
        let mut w = build_widget(ForexConfig {
            primary: "USD".into(),
            watchlist: vec!["EUR".into()],
            crypto_watchlist: Vec::new(),
            ..Default::default()
        });
        assert!(w.handle_fx_command(&["NOK"]).is_ok());
        assert_eq!(w.primary, "USD", ":fx must not swap");
        assert_eq!(
            w.state.lock().unwrap().transient_code.as_deref(),
            Some("NOK"),
            "NOK should be pinned as the Lookup row"
        );
        // Selection lands on the new Lookup row so the graph/stats
        // panels surface the pair immediately.
        let rows = w.all_rows();
        let sel = w.state.lock().unwrap().selected;
        assert_eq!(rows.get(sel).map(|s| s.as_str()), Some("NOK"));
    }

    #[test]
    fn swap_moves_old_primary_to_top_of_alternates() {
        // Configured: USD primary with [EUR, JPY] alternates. After
        // swap to EUR: primary=EUR, alternates=[USD, JPY] (USD at
        // position 0 of alternates, which is row 1 of the full list).
        let mut w = build_widget(ForexConfig {
            primary: "USD".into(),
            watchlist: vec!["EUR".into(), "JPY".into()],
            crypto_watchlist: Vec::new(),
            ..Default::default()
        });
        // Seed a rate so the amount-conversion path doesn't fall back.
        w.state.lock().unwrap().quotes.insert(
            "USDEUR=X".into(),
            QuoteState::Ready(Box::new(rate("USD", "EUR", 0.9))),
        );
        w.swap_primary("EUR");
        assert_eq!(w.primary, "EUR");
        assert_eq!(w.alternates, vec!["USD".to_string(), "JPY".to_string()]);
        // Selection auto-lands on the first alternate = USD.
        let rows = w.all_rows();
        let sel = w.state.lock().unwrap().selected;
        assert_eq!(rows.get(sel).map(|s| s.as_str()), Some("USD"));
    }

    #[test]
    fn configured_primary_stays_anchored_across_multi_hop_swaps() {
        // The bug: USD→BTC→ETH used to drop USD because every swap
        // rebuilds from `config.{watchlist, crypto_watchlist}` (which
        // never contain USD) and only the *immediately previous*
        // primary got re-promoted. After the second hop, the
        // previous primary became BTC and USD fell off the list.
        let mut w = build_widget(ForexConfig {
            primary: "USD".into(),
            watchlist: vec!["EUR".into(), "JPY".into()],
            crypto_watchlist: vec!["BTC".into(), "ETH".into(), "SOL".into()],
            ..Default::default()
        });
        // USD → BTC
        w.swap_primary("BTC");
        assert!(w.alternates.contains(&"USD".to_string()), "after USD→BTC");
        // BTC → ETH (this is the multi-hop step that used to lose USD)
        w.swap_primary("ETH");
        assert!(
            w.alternates.contains(&"USD".to_string()),
            "USD must still be in alternates after multi-hop swap"
        );
        // USD anchored at position 0 of its native (fiat) category.
        assert_eq!(w.alternates[0], "USD");
        // BTC promoted as the "just-swapped-away" primary at the top
        // of the crypto section.
        assert_eq!(w.alternates[w.crypto_start], "BTC");

        // Another hop: ETH → SOL. USD still anchored.
        w.swap_primary("SOL");
        assert_eq!(w.alternates[0], "USD");
        assert!(w.alternates.contains(&"ETH".to_string()));
    }

    #[test]
    fn swap_to_crypto_promotes_configured_primary_into_currency_alternates() {
        // User config: primary=USD, fiat watchlist doesn't include USD,
        // crypto_watchlist seeds BTC. Swapping primary to BTC must
        // surface USD in the currencies section (position 0 of fiat),
        // otherwise USD becomes uncomparable mid-session.
        let mut w = build_widget(ForexConfig {
            primary: "USD".into(),
            watchlist: vec!["EUR".into(), "JPY".into()],
            crypto_watchlist: vec!["BTC".into(), "ETH".into()],
            ..Default::default()
        });
        // Before swap, alternates carry only the configured lists
        // (no USD, since USD is primary).
        assert!(!w.alternates.contains(&"USD".to_string()));
        w.swap_primary("BTC");
        assert_eq!(w.primary, "BTC");
        // USD lands at position 0 of fiat; crypto_start bumps to
        // account for the inserted entry.
        assert_eq!(w.alternates[0], "USD");
        assert!(w.alternates.contains(&"USD".to_string()));
        // ETH is the only remaining crypto (BTC is now primary).
        assert_eq!(w.alternates.last().map(|s| s.as_str()), Some("ETH"));
    }

    #[test]
    fn swap_back_to_configured_primary_restores_original_alternates_order() {
        // Drift scenario: config primary = USD, alternates = [EUR, GBP, JPY].
        // Swap USD → EUR → GBP → USD. After the final swap back to
        // the original primary, alternates must match the config
        // order (NOT [JPY, EUR, GBP] or some intermediate reshuffle).
        let mut w = build_widget(ForexConfig {
            primary: "USD".into(),
            watchlist: vec!["EUR".into(), "GBP".into(), "JPY".into()],
            crypto_watchlist: Vec::new(),
            ..Default::default()
        });
        // Seed rates so amount-conversion doesn't fall back at each hop.
        for (from, to, price) in &[
            ("USD", "EUR", 0.92_f64),
            ("EUR", "GBP", 0.85),
            ("GBP", "USD", 1.27),
        ] {
            w.state.lock().unwrap().quotes.insert(
                YahooForexProvider::symbol_for(from, to),
                QuoteState::Ready(Box::new(rate(from, to, *price))),
            );
        }
        w.swap_primary("EUR");
        w.swap_primary("GBP");
        // Mid-walk: alternates have been reshuffled by the normal
        // "old primary onto the front" logic.
        assert_ne!(
            w.alternates,
            vec!["EUR".to_string(), "GBP".to_string(), "JPY".to_string()]
        );
        // Now swap home.
        w.swap_primary("USD");
        assert_eq!(w.primary, "USD");
        assert_eq!(
            w.alternates,
            vec!["EUR".to_string(), "GBP".to_string(), "JPY".to_string()],
            "swap-home should restore the configured alternate order, not the drifted one"
        );
        // Selection lands on the row that holds the previous primary
        // (GBP, at config index 1 → row 2).
        let rows = w.all_rows();
        let sel = w.state.lock().unwrap().selected;
        assert_eq!(rows.get(sel).map(|s| s.as_str()), Some("GBP"));
    }

    #[test]
    fn swap_from_lookup_row_clears_transient_and_promotes() {
        // :fx NOK pins NOK as the Lookup row, then `p` promotes it.
        // After promotion: primary=NOK, alternates=[USD, EUR],
        // transient_code=None.
        let mut w = build_widget(ForexConfig {
            primary: "USD".into(),
            watchlist: vec!["EUR".into()],
            crypto_watchlist: Vec::new(),
            ..Default::default()
        });
        w.handle_fx_command(&["NOK"]).unwrap();
        assert_eq!(
            w.state.lock().unwrap().transient_code.as_deref(),
            Some("NOK")
        );
        w.swap_primary("NOK");
        assert_eq!(w.primary, "NOK");
        assert_eq!(w.alternates, vec!["USD".to_string(), "EUR".to_string()]);
        assert!(w.state.lock().unwrap().transient_code.is_none());
    }

    #[test]
    fn initial_selection_lands_on_first_alternate() {
        let w = build_widget(ForexConfig {
            primary: "USD".into(),
            watchlist: vec!["EUR".into(), "JPY".into()],
            crypto_watchlist: Vec::new(),
            ..Default::default()
        });
        assert_eq!(w.state.lock().unwrap().selected, 1);
    }

    #[test]
    fn row_value_for_primary_returns_amount_directly() {
        let mut w = build_widget(ForexConfig {
            primary: "USD".into(),
            watchlist: vec!["EUR".into()],
            crypto_watchlist: Vec::new(),
            ..Default::default()
        });
        w.amount = 250.0;
        let q = w.snapshot_quotes();
        assert_eq!(w.row_value("USD", &q), Some(250.0));
    }

    #[test]
    fn row_value_for_quote_multiplies_amount_by_rate() {
        let mut w = build_widget(ForexConfig {
            primary: "USD".into(),
            watchlist: vec!["EUR".into()],
            crypto_watchlist: Vec::new(),
            ..Default::default()
        });
        w.amount = 100.0;
        w.state.lock().unwrap().quotes.insert(
            "USDEUR=X".into(),
            QuoteState::Ready(Box::new(rate("USD", "EUR", 0.9237))),
        );
        let q = w.snapshot_quotes();
        let v = w.row_value("EUR", &q).unwrap();
        assert!((v - 92.37).abs() < 1e-6);
    }

    #[test]
    fn fit_edit_buffer_right_pads_short_input_with_leading_spaces() {
        // "15" = 2 cells. Fit into 12 → 10 leading spaces + "15".
        // The trailing cursor lives outside the value field (in the
        // value-copy gap), so it's not part of this helper's output.
        let out = fit_edit_buffer_right("15", 12);
        assert_eq!(out.chars().count(), 12);
        assert_eq!(out, "          15");
    }

    #[test]
    fn fit_edit_buffer_right_anchors_to_non_edit_alignment() {
        // The whole point of this helper is keeping the digits at the
        // same columns as the non-edit display so toggling `e` doesn't
        // shift them. format_amount(15.0) → "          15" should
        // match the edit-mode render for buf="15".
        assert_eq!(fit_edit_buffer_right("15", 12), format_amount(15.0));
        assert_eq!(fit_edit_buffer_right("1523.8000", 12), format_amount(1523.80));
    }

    #[test]
    fn fit_edit_buffer_right_truncates_left_when_input_overflows() {
        // "1234567890.123" = 14 cells, exceeds 12. Drop the 2 leftmost
        // chars so the most recently typed char stays visible at the
        // right edge — user is editing forward.
        let out = fit_edit_buffer_right("1234567890.123", 12);
        assert_eq!(out.chars().count(), 12);
        assert_eq!(out, "34567890.123");
    }

    #[test]
    fn fit_edit_buffer_right_empty_buffer_is_full_width_padding() {
        let out = fit_edit_buffer_right("", 12);
        assert_eq!(out.chars().count(), 12);
        assert_eq!(out, "            ");
    }

    #[test]
    fn is_iso_currency_codish_accepts_3_uppercase_letters_only() {
        assert!(is_iso_currency_codish("USD"));
        assert!(is_iso_currency_codish("JPY"));
        assert!(!is_iso_currency_codish("usd"));
        assert!(!is_iso_currency_codish("USDX"));
        assert!(!is_iso_currency_codish("US"));
        assert!(!is_iso_currency_codish("U$D"));
    }
}
