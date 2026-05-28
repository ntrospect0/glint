// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

pub mod graph;
pub mod provider;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{Datelike, Duration as ChronoDuration, NaiveDate, TimeZone, Timelike, Utc, Weekday};
use chrono_tz::Tz;
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
use crate::ui::status::{live_value, TimedFeedback};
use crate::ui::{apply_title_row, MetadataEmphasis};

use super::{AppContext, EventResult, Widget};

use provider::{Period, StockQuote, YahooFinanceProvider};

/// Loaded from `~/.config/glint/stocks.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct StocksConfig {
    /// Index symbols pinned to the top (Yahoo: `^DJI`, `^GSPC`, `^IXIC`).
    #[serde(default = "default_indices")]
    pub indices: Vec<String>,

    #[serde(default = "default_watchlist")]
    pub watchlist: Vec<String>,

    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    /// Fast cadence used when the widget is the active stack child *and*
    /// holds keyboard focus. Outside that window we fall back to
    /// `poll_interval_secs`, which keeps the background fetch rate down
    /// to ~5min on a multi-widget dashboard.
    #[serde(default = "default_focused_poll_interval")]
    pub focused_poll_interval_secs: u64,

    /// `"percent"` / `"dollar"` / `"change"`.
    #[serde(default)]
    pub default_display_mode: DisplayMode,

    /// `"1d"` / `"1w"` / `"1m"` / `"6m"` / `"ytd"` / `"1y"`.
    #[serde(default)]
    pub default_period: Period,

    /// URL opened on Enter. `{ticker}` is replaced with the URL-encoded symbol.
    #[serde(default)]
    pub jump_url_template: Option<String>,

    /// Cycle period tabs on horizontal scroll. Off by default — trackpad
    /// sideways gestures often fire accidentally while scrolling vertically.
    #[serde(default)]
    pub horizontal_scroll_period: bool,

    /// Faint dashed range-high/low lines on non-Day periods.
    #[serde(default = "default_graph_high_low_lines")]
    pub graph_high_low_lines: bool,

    /// On 1D, render the intraday trace against the full session x-axis so
    /// the remainder of the day stays as empty space — gives a visual sense
    /// of how far along the session is.
    #[serde(default = "default_pad_intraday_to_full_day")]
    pub pad_intraday_to_full_day: bool,

    /// Per-widget overrides layered on the app theme.
    #[serde(default)]
    pub colors: ColorScheme,

    /// `Shift+<letter>` focus shortcuts; falls back to `['s', 't', 'o', 'c', 'k']`.
    #[serde(default)]
    pub shortcuts: Vec<char>,
}

fn default_graph_high_low_lines() -> bool {
    true
}

fn default_pad_intraday_to_full_day() -> bool {
    true
}

/// Number of 5-minute bars in a regular US trading session (6.5 hours).
/// Used to size the intraday plot's "time elapsed" fraction. Yahoo's `1d`
/// chart range only returns regular-session bars at 5m resolution, so the
/// length of `intraday` is a stable proxy for time-of-day.
const TRADING_DAY_BARS: usize = 78;

fn default_indices() -> Vec<String> {
    vec!["^DJI".into(), "^GSPC".into(), "^IXIC".into()]
}
fn default_watchlist() -> Vec<String> {
    // Magnificent Seven + the FAANG holdout (NFLX) + a small set of
    // famous blue chips. Gives a brand-new install a recognisable
    // cross-sector watchlist without going overboard.
    vec![
        // MAG7
        "AAPL".into(),
        "MSFT".into(),
        "GOOGL".into(),
        "AMZN".into(),
        "META".into(),
        "NVDA".into(),
        "TSLA".into(),
        // FAANG round-out
        "NFLX".into(),
        // Blue chips across finance / healthcare / consumer staples / retail
        "BRK-B".into(),
        "JPM".into(),
        "JNJ".into(),
        "V".into(),
        "WMT".into(),
    ]
}
fn default_poll_interval() -> u64 {
    300
}
fn default_focused_poll_interval() -> u64 {
    60
}

impl Default for StocksConfig {
    fn default() -> Self {
        Self {
            indices: default_indices(),
            watchlist: default_watchlist(),
            poll_interval_secs: default_poll_interval(),
            focused_poll_interval_secs: default_focused_poll_interval(),
            default_display_mode: DisplayMode::default(),
            default_period: Period::default(),
            jump_url_template: None,
            horizontal_scroll_period: false,
            graph_high_low_lines: default_graph_high_low_lines(),
            pad_intraday_to_full_day: default_pad_intraday_to_full_day(),
            colors: ColorScheme::default(),
            shortcuts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DisplayMode {
    #[default]
    Percent,
    Dollar,
}

#[derive(Debug, Clone)]
enum QuoteState {
    Inflight,
    /// Successful fetch. `Arc` (not `Box`) so the per-render
    /// `HashMap::clone()` is a pointer bump per symbol instead of a
    /// full StockQuote deep-copy — the intraday Vec alone can be a
    /// couple of KB on long ranges.
    Ready(Arc<StockQuote>),
    /// Last fetch failed. Reason is already logged via tracing; we don't need
    /// to surface it in the UI right now (the row just shows "err").
    Failed,
}

struct StocksState {
    /// Per-period quote snapshots. The active view reads
    /// `quotes_by_period[self.period]`. Switching periods used to
    /// leak the previous period's intraday series under the new
    /// period's x-axis labels until the refetch landed — keeping
    /// each period's data in its own slot makes period switches
    /// instant when we've already fetched the new period this
    /// session, and ensures concurrent in-flight fetches land in
    /// the right bucket even if the user has switched again.
    quotes_by_period: HashMap<Period, HashMap<String, QuoteState>>,
    selected: usize,
    /// First-visible logical row in the list panel. Auto-adjusted on render
    /// so the selected ticker stays in view.
    list_scroll: usize,
    /// Ticker pinned to the list by a `:stock <query>` command. Appears in
    /// its own `── Lookup ──` section at the bottom of the list and stays
    /// until the user presses `x` to clear it.
    transient_ticker: Option<String>,
    /// Set while a `:stock` search is in flight so we can render "Looking up…"
    transient_searching: Option<String>,
    poll: crate::polling::PollTracker,
    any_inflight: bool,
    /// Symbol pending y/N removal confirmation. When `Some`, the
    /// widget paints a modal and the key dispatcher swallows everything
    /// except `y` (confirm) and any-other-key (cancel).
    confirm_remove: Option<String>,
    /// Transient status line for "Added AAPL to watchlist" / "Can't
    /// remove primary" feedback. Cleared once the TTL elapses.
    status: Option<TimedFeedback<String>>,
    /// Display-state dirty bit drained by `take_dirty`. Set true by
    /// every async-task / tick-time mutation site so the main loop's
    /// dirty-flag gate triggers a redraw. User-driven mutations
    /// (handle_key / handle_mouse) don't need to set it — non-tick
    /// events always redraw at the App level.
    dirty: bool,
    /// One-shot "bypass the market-hours fetch gate on the next due
    /// check" flag. Set true at construction (cold start always allows
    /// at least one refresh so a sleeping-overnight reopen gets fresh
    /// data) and by `mark_dirty()` (user-triggered `r` / `:refresh`).
    /// Consumed by `is_due()` after the bypass fires.
    force_next_refresh: bool,
    /// Most-recent `render()` call where the widget held focus. `None`
    /// when the last render was unfocused, or when the widget has never
    /// rendered (e.g. on a hidden stack tab since construction). The
    /// freshness check inside `is_due()` switches between fast (focused)
    /// and slow (background) cadence based on this — see
    /// `FOCUS_FRESHNESS_WINDOW`.
    last_focused_at: Option<Instant>,
}

/// How recently `render()` must have been called with `focused = true`
/// to count as "currently focused" for cadence purposes. The render
/// loop touches every visible widget at least once per second whenever
/// anything on the dashboard is dirty (the clock alone is enough), so
/// 2 s gives slack for one missed tick while still dropping the widget
/// out of fast cadence within a couple of seconds of losing focus.
const FOCUS_FRESHNESS_WINDOW: Duration = Duration::from_secs(2);

impl StocksState {
    /// Read-only view of the active period's quote map. Returns an
    /// empty borrow when nothing has been fetched / cached for that
    /// period yet so callers don't need to special-case `None`.
    fn quotes(&self, period: Period) -> &HashMap<String, QuoteState> {
        // SAFETY: the static empty map lives for the whole program;
        // re-using it avoids the borrow-checker awkwardness of
        // returning `Option<&_>` from a method that's overwhelmingly
        // used as "iterate / look up by symbol."
        static EMPTY: std::sync::OnceLock<HashMap<String, QuoteState>> =
            std::sync::OnceLock::new();
        self.quotes_by_period
            .get(&period)
            .unwrap_or_else(|| EMPTY.get_or_init(HashMap::new))
    }
    /// Mutable view of the active period's quote map. Creates an
    /// empty slot on first access so callers can just `.insert` /
    /// `.entry` without a contains_key dance.
    fn quotes_mut(&mut self, period: Period) -> &mut HashMap<String, QuoteState> {
        self.quotes_by_period.entry(period).or_default()
    }
}

impl Default for StocksState {
    fn default() -> Self {
        Self {
            quotes_by_period: HashMap::new(),
            selected: 0,
            list_scroll: 0,
            transient_ticker: None,
            transient_searching: None,
            poll: crate::polling::PollTracker::default(),
            any_inflight: false,
            confirm_remove: None,
            status: None,
            dirty: false,
            force_next_refresh: true,
            last_focused_at: None,
        }
    }
}

/// How long the status feedback line stays on screen after an
/// add / remove action. Long enough to read, short enough to revert
/// before the next interaction.
const STATUS_TTL: Duration = Duration::from_millis(2500);

/// Which on-disk array a symbol belongs to. Yahoo `^DJI`-style index
/// symbols land in `indices`; everything else lands in `watchlist`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StocksListKind {
    Indices,
    Watchlist,
}

/// Cache key prefix; the active period is appended so each period has its
/// own cached payload (chart data shape varies by period).
const CACHE_KEY_QUOTES_PREFIX: &str = "quotes-";

fn quotes_cache_key(period: Period) -> String {
    format!(
        "{CACHE_KEY_QUOTES_PREFIX}{}",
        period.label().to_ascii_lowercase()
    )
}

pub struct StocksWidget {
    id: String,
    instance: String,
    /// Cached `Stocks` / `Stocks (instance)` label so `display_name()` can
    /// hand out a `&str` without per-call allocation.
    display_name_cache: String,
    config: StocksConfig,
    provider: Arc<YahooFinanceProvider>,
    state: Arc<Mutex<StocksState>>,
    /// Display mode cycled by the `%` / `$` / `c` keys; kept in widget (not
    /// state) since it changes synchronously and never via the network.
    display_mode: DisplayMode,
    /// Currently selected graph period (1D / 1W / 1M / 6M / YTD / 1Y).
    period: Period,
    /// App-level theme; kept so live config reloads can rebuild `theme`
    /// from updated `colors` overrides.
    app_theme: Arc<Theme>,
    /// Merged theme (app + widget overrides). Built at construction and
    /// after every `apply_config`.
    theme: Theme,
    /// Letter assigned by the app for `Shift+<letter>` focus, painted in
    /// the title via `text.shortcut`. `None` = no shortcut claimed.
    shortcut: Option<char>,
    /// Effective shortcut preference list. Either the user's TOML
    /// override or the built-in default. Returned by
    /// `shortcut_preferences` so the trait's borrow lifetime matches.
    shortcut_prefs: Vec<char>,
    /// Persistent cache of fetched quotes keyed by period. Each successful
    /// refresh writes the full `symbol → StockQuote` snapshot.
    cache: ScopedCache,
    /// Atomic gate over the per-tick status-TTL drain. `update()` runs
    /// every 250 ms and would otherwise lock the state mutex on every
    /// tick just to check whether the (almost always None) status slot
    /// had expired. We flip this to `true` whenever a status is set
    /// and clear it from `update()` once the slot is empty again, so
    /// idle ticks skip the lock entirely.
    feedback_pending: AtomicBool,
}

impl StocksWidget {
    pub fn with_config(
        instance: String,
        config: StocksConfig,
        app_theme: Arc<Theme>,
        cache: ScopedCache,
    ) -> Self {
        let provider = match YahooFinanceProvider::new() {
            Ok(p) => Arc::new(p),
            Err(err) => {
                tracing::warn!(error = %err, "failed to build Yahoo Finance provider, stocks widget will be inert");
                Arc::new(provider_dummy())
            }
        };
        let display_mode = config.default_display_mode;
        let theme = app_theme.with_overrides(&config.colors);
        let shortcut_prefs = if config.shortcuts.is_empty() {
            vec!['s', 't', 'o', 'c', 'k']
        } else {
            config.shortcuts.clone()
        };
        let id = if instance == "main" {
            "stocks".to_string()
        } else {
            format!("stocks@{instance}")
        };
        // Restore period + selected symbol from the runtime-state file
        // so a relaunch lands the user back on the ticker/period they
        // were last looking at. Persisted values that no longer fit
        // (unknown period label, symbol no longer in indices /
        // watchlist) silently fall back to the configured defaults
        // rather than refusing to load.
        let persisted = crate::runtime_state::load();
        let widget_entry = persisted.stocks.get(&id);
        let period = widget_entry
            .and_then(|e| e.period.as_deref())
            .and_then(Period::from_label)
            .unwrap_or(config.default_period);
        let persisted_symbol = widget_entry.and_then(|e| e.selected_symbol.clone());
        let display_name_cache = if instance == "main" {
            "Stocks".to_string()
        } else {
            format!("Stocks ({instance})")
        };
        let poll_interval = Duration::from_secs(config.poll_interval_secs.max(15));
        // Seed quotes for the default period so the dashboard paints
        // prior prices instantly. Other periods are lazily loaded from
        // disk on first switch (see `set_period`).
        let mut initial_state = StocksState::default();
        initial_state.poll = crate::polling::PollTracker::new(poll_interval);
        if let Some(entry) = cache.load::<HashMap<String, StockQuote>>(&quotes_cache_key(period)) {
            initial_state.poll.seed_from_cache_age(entry.age());
            initial_state.quotes_by_period.insert(
                period,
                entry
                    .value
                    .into_iter()
                    .map(|(sym, q)| (sym, QuoteState::Ready(Arc::new(q))))
                    .collect(),
            );
        }
        initial_state
            .poll
            .apply_jitter(&format!("stocks@{instance}"));
        let widget = Self {
            id,
            instance,
            display_name_cache,
            config,
            provider,
            state: Arc::new(Mutex::new(initial_state)),
            display_mode,
            period,
            app_theme,
            theme,
            shortcut: None,
            shortcut_prefs,
            cache,
            feedback_pending: AtomicBool::new(false),
        };
        // Restore selection now that the symbol list is reachable
        // via `all_symbols()`. Symbols are matched case-insensitively
        // — Yahoo tickers are nominally uppercase but we accept the
        // persisted casing either way.
        if let Some(sym) = persisted_symbol {
            let symbols = widget.all_symbols();
            if let Some(idx) = symbols
                .iter()
                .position(|s| s.eq_ignore_ascii_case(&sym))
            {
                widget
                    .state
                    .lock()
                    .expect("stocks state poisoned")
                    .selected = idx;
            }
        }
        widget
    }

    /// Snapshot the user's "last view" (selected ticker + active
    /// period) into the runtime-state file. Called from the few
    /// state-change paths where one of those values mutates —
    /// `set_period`, `move_selection`, list-click, `:stock` lookup.
    /// Failures log + return; persisting is best-effort and should
    /// never disrupt the dashboard.
    fn persist_runtime_state(&self) {
        let mut payload = crate::runtime_state::load();
        let selected = self.selected_symbol();
        let entry = payload.stocks.entry(self.id.clone()).or_default();
        entry.selected_symbol = selected;
        entry.period = Some(self.period.label().to_string());
        if let Err(err) = crate::runtime_state::save(&payload) {
            tracing::warn!(error = %err, "failed to persist stocks runtime state");
        }
    }

    fn set_period(&mut self, period: Period) {
        if self.period == period {
            return;
        }
        self.period = period;
        // Force a refresh on the next tick so the chart and change%
        // catch up to the new window. We deliberately do NOT seed
        // the new period's bucket from disk here — disk caches
        // produced by earlier code paths may be poisoned with the
        // wrong period's data (the old shared `state.quotes` could
        // leak data into per-period cache files on partial fetch
        // failures), and showing stale-wrong is worse than showing
        // "Loading…" until the refetch lands. In-session in-memory
        // buckets for previously-visited periods stay intact and
        // re-display instantly.
        self.mark_dirty();
        self.persist_runtime_state();
    }

    /// Cycle period forward (`true`) or backward (`false`) through Period::ALL,
    /// wrapping at the ends.
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

    /// Concatenated list of symbols in display order: indices first, then
    /// watchlist, then the transient lookup (if any). Used for selection
    /// indexing too.
    fn all_symbols(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .config
            .indices
            .iter()
            .chain(self.config.watchlist.iter())
            .cloned()
            .collect();
        if let Some(t) = self
            .state
            .lock()
            .expect("stocks state poisoned")
            .transient_ticker
            .clone()
        {
            if !out.iter().any(|s| s.eq_ignore_ascii_case(&t)) {
                out.push(t);
            }
        }
        out
    }

    fn is_due(&self) -> bool {
        // Compute the "first fetch needed" flag outside the state lock so
        // we don't double-lock via all_symbols().
        let symbols = self.all_symbols();
        let mut st = self.state.lock().expect("stocks state poisoned");
        if st.any_inflight {
            return false;
        }
        // Pick the cadence for *this* check: fast while the user is
        // actively looking at the widget, slow otherwise. A multi-widget
        // dashboard would otherwise burn quota fetching Yahoo every
        // minute for a pane the user can't even see.
        let focused_now = st
            .last_focused_at
            .map(|t| t.elapsed() < FOCUS_FRESHNESS_WINDOW)
            .unwrap_or(false);
        let active_interval_secs = if focused_now {
            self.config.focused_poll_interval_secs.max(15)
        } else {
            self.config.poll_interval_secs.max(15)
        };
        st.poll.set_interval(Duration::from_secs(active_interval_secs));
        if !st.poll.is_due() {
            return false;
        }
        // Bypass the market-hours gate when:
        //   1. Cold start / cache miss — any symbol still without Ready data.
        //   2. The force_next_refresh one-shot flag is set (cold-start
        //      catch-up fetch, or user-triggered `r` / `:refresh`).
        // Consume the flag so subsequent ticks fall back to the gate.
        let active_quotes = st.quotes(self.period);
        let need_first_fetch = symbols
            .iter()
            .any(|s| !matches!(active_quotes.get(s), Some(QuoteState::Ready(_))));
        if st.force_next_refresh || need_first_fetch {
            st.force_next_refresh = false;
            return true;
        }
        // Otherwise: only refresh during pre-market / regular / post-market
        // (04:00–20:00 America/New_York on a non-weekend, non-holiday day).
        // Yahoo's quote doesn't change outside those windows, so polling
        // through the overnight just burns rate-limit budget and chews
        // through the cached crumb.
        is_extended_market_hours(Utc::now())
    }

    fn spawn_refresh(&self) {
        let symbols: Vec<String> = self.all_symbols();
        if symbols.is_empty() {
            return;
        }
        let period = self.period;
        {
            let mut st = self.state.lock().expect("stocks state poisoned");
            st.any_inflight = true;
            st.poll.mark_attempted();
            let bucket = st.quotes_mut(period);
            for sym in &symbols {
                bucket.entry(sym.clone()).or_insert(QuoteState::Inflight);
            }
            st.dirty = true;
        }
        let provider = self.provider.clone();
        let state = self.state.clone();
        let cache = self.cache.clone();
        tokio::spawn(async move {
            // Fetch each symbol in parallel. Yahoo's v8/chart endpoint is
            // per-symbol so we can't batch into one request.
            let futs = symbols.iter().map(|sym| {
                let provider = provider.clone();
                let sym = sym.clone();
                async move {
                    let result = provider.fetch_quote(&sym, period).await;
                    (sym, result)
                }
            });
            let results = futures::future::join_all(futs).await;
            let mut st = state.lock().expect("stocks state poisoned");
            // Seed the cache snapshot from existing Ready entries for
            // *this period* so a partial failure (e.g. one symbol's
            // request errors) doesn't wipe the on-disk snapshot for the
            // symbols that *did* succeed previously.
            let mut snapshot: HashMap<String, StockQuote> = st
                .quotes(period)
                .iter()
                .filter_map(|(k, v)| match v {
                    QuoteState::Ready(q) => Some((k.clone(), (**q).clone())),
                    _ => None,
                })
                .collect();
            let bucket = st.quotes_mut(period);
            for (sym, result) in results {
                match result {
                    Ok(q) => {
                        snapshot.insert(sym.clone(), q.clone());
                        bucket.insert(sym, QuoteState::Ready(Arc::new(q)));
                    }
                    Err(err) => {
                        tracing::warn!(symbol = %sym, error = %err, "stock fetch failed");
                        // Keep the last known-good quote if we have one — the
                        // user sees the prior price instead of `err` through
                        // transient network outages (e.g. wake-from-sleep).
                        // Only flip to `Failed` if we never had a successful
                        // fetch for this symbol.
                        bucket
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
            st.dirty = true;
            drop(st);
            if !snapshot.is_empty() {
                if let Err(err) = cache.store(&quotes_cache_key(period), &snapshot) {
                    tracing::warn!(error = %err, "stocks cache store failed");
                }
            }
        });
    }

    fn mark_dirty(&self) {
        let mut st = self.state.lock().expect("stocks state poisoned");
        st.poll.mark_dirty();
        // User-triggered refreshes (`r`, `:refresh`, period changes) must
        // override the market-hours fetch gate so the user always gets
        // an immediate response to their explicit action.
        st.force_next_refresh = true;
    }

    fn move_selection(&mut self, delta: isize) {
        let n = self.all_symbols().len();
        if n == 0 {
            return;
        }
        let changed = {
            let mut st = self.state.lock().expect("stocks state poisoned");
            let prev = st.selected;
            let new = (prev as isize + delta).clamp(0, n as isize - 1) as usize;
            st.selected = new;
            new != prev
        };
        if changed {
            self.persist_runtime_state();
        }
    }

    fn selected_symbol(&self) -> Option<String> {
        let symbols = self.all_symbols();
        let idx = self.state.lock().expect("stocks state poisoned").selected;
        symbols.into_iter().nth(idx)
    }

    /// Resolve `query` to a ticker (direct or via Yahoo search) and pin it as
    /// the transient symbol, selecting it in the list. Called from
    /// :stock <query> dispatch.
    fn lookup_and_set_transient(&self, query: &str) {
        let query_trim = query.trim().to_string();
        // Fast path: the query already names a row we're displaying —
        // an index, a watchlist entry, or the currently-pinned
        // transient. Snap selection to that row and stop. Without
        // this, `:stock AAPL` against a watchlist that already
        // contains AAPL would set `transient_ticker = Some("AAPL")`
        // and `selected = base_slot`, but `all_symbols()` dedupes the
        // transient against the watchlist — leaving selection
        // pointing past the end of the visible list and nothing
        // loads. Case-insensitive so `:stock aapl` works the same.
        if let Some(idx) = self.locate_displayed(&query_trim) {
            let changed = {
                let mut st = self.state.lock().expect("stocks state poisoned");
                let prev = st.selected;
                st.selected = idx;
                st.transient_searching = None;
                idx != prev
            };
            if changed {
                self.persist_runtime_state();
            }
            return;
        }
        // If it already looks like a ticker (short, ASCII-uppercase + ^ . - =)
        // skip the search round-trip.
        if is_tickerish(&query_trim) {
            self.set_transient_now(query_trim.to_uppercase());
            return;
        }
        // Mark "searching…" so the UI can show feedback while the request flies.
        {
            let mut st = self.state.lock().expect("stocks state poisoned");
            st.transient_searching = Some(query_trim.clone());
        }
        let provider = self.provider.clone();
        let state = self.state.clone();
        // Snapshot the configured lists so the async resolver can check
        // "is the resolved ticker already on screen?" without holding a
        // reference to `self`. The transient slot is read fresh from
        // the locked state inside the task.
        let indices = self.config.indices.clone();
        let watchlist = self.config.watchlist.clone();
        // Total slot count (indices + watchlist) — knowing this lets us snap
        // selection straight to the transient row (last slot) when search
        // resolves.
        let base_slot = indices.len() + watchlist.len();
        tokio::spawn(async move {
            let result = provider.search(&query_trim).await;
            let mut st = state.lock().expect("stocks state poisoned");
            st.transient_searching = None;
            st.dirty = true;
            match result {
                Ok(symbol) => {
                    // Same idea as the fast path, applied to the
                    // resolved ticker: `:stock apple` → Yahoo returns
                    // `AAPL` → if AAPL is already on screen, just
                    // snap. Don't re-pin a row we're already showing.
                    let known = indices
                        .iter()
                        .chain(watchlist.iter())
                        .position(|s| s.eq_ignore_ascii_case(&symbol));
                    if let Some(idx) = known {
                        st.selected = idx;
                        return;
                    }
                    if st
                        .transient_ticker
                        .as_deref()
                        .is_some_and(|t| t.eq_ignore_ascii_case(&symbol))
                    {
                        st.selected = base_slot;
                        return;
                    }
                    st.transient_ticker = Some(symbol);
                    st.selected = base_slot;
                    st.poll.mark_dirty();
                }
                Err(err) => {
                    tracing::warn!(query = %query_trim, error = %err, "stock lookup failed");
                }
            }
        });
    }

    /// Case-insensitive search for `query` against the currently-displayed
    /// symbol list (indices + watchlist + transient). Returns the slot
    /// index when found, `None` otherwise. Used by `:stock <query>` to
    /// short-circuit a lookup that would otherwise pin a duplicate row.
    fn locate_displayed(&self, query: &str) -> Option<usize> {
        let needle = query.trim();
        if needle.is_empty() {
            return None;
        }
        self.all_symbols()
            .iter()
            .position(|s| s.eq_ignore_ascii_case(needle))
    }

    /// Insert `symbol` as the transient lookup synchronously (used when the
    /// query already looked like a ticker, no search needed).
    fn set_transient_now(&self, symbol: String) {
        let base_slot = self.config.indices.len() + self.config.watchlist.len();
        let mut st = self.state.lock().expect("stocks state poisoned");
        st.transient_ticker = Some(symbol);
        st.transient_searching = None;
        st.selected = base_slot;
        st.poll.mark_dirty();
    }

    /// Clear the transient ticker and bounce the selection back to the top
    /// of the configured list. No-op when there's nothing pinned.
    fn clear_transient(&self) {
        let mut st = self.state.lock().expect("stocks state poisoned");
        if st.transient_ticker.take().is_some() {
            st.selected = 0;
            st.list_scroll = 0;
        }
    }

    /// Classify a symbol back to its destination list. Yahoo prefixes
    /// indices with `^` (e.g. `^DJI`); everything else goes to the
    /// regular watchlist. Crypto on Yahoo uses `BTC-USD` form which
    /// also lands in the watchlist — separating crypto out the way
    /// Forex does isn't worth a second list here, since stocks chart
    /// math doesn't care about asset class.
    fn classify_symbol(sym: &str) -> StocksListKind {
        if sym.starts_with('^') {
            StocksListKind::Indices
        } else {
            StocksListKind::Watchlist
        }
    }

    /// Set the transient status line. Cleared automatically once
    /// `STATUS_TTL` elapses on the next render-time check.
    fn set_status(&self, msg: impl Into<String>) {
        let mut st = self.state.lock().expect("stocks state poisoned");
        st.status = Some(TimedFeedback::new(msg.into(), STATUS_TTL));
        drop(st);
        self.feedback_pending.store(true, Ordering::Relaxed);
    }

    /// Persist the current `indices` + `watchlist` arrays back to
    /// `~/.config/glint/<stocks|stocks@instance>.toml`, preserving any
    /// other top-level scalars / `[colors]` block / comments. Logs +
    /// surfaces a status line on failure; never panics.
    fn persist_lists(&self) -> bool {
        let indices = self.config.indices.clone();
        let watchlist = self.config.watchlist.clone();
        let mut ok = true;
        if let Err(err) = crate::config::rewrite_widget_top_level_string_array(
            "stocks",
            &self.instance,
            "indices",
            &indices,
        ) {
            tracing::warn!(error = %err, "stocks: failed to persist indices");
            self.set_status(format!("Save failed: {err}"));
            ok = false;
        }
        if let Err(err) = crate::config::rewrite_widget_top_level_string_array(
            "stocks",
            &self.instance,
            "watchlist",
            &watchlist,
        ) {
            tracing::warn!(error = %err, "stocks: failed to persist watchlist");
            self.set_status(format!("Save failed: {err}"));
            ok = false;
        }
        ok
    }

    /// Open the confirm-remove modal for the currently selected
    /// symbol. No-op when the selection sits on the transient lookup
    /// row (use `x` to clear that) or when the list is empty.
    fn request_remove_selected(&self) {
        let symbols = self.all_symbols();
        if symbols.is_empty() {
            return;
        }
        let base_count = self.config.indices.len() + self.config.watchlist.len();
        let idx = self.state.lock().expect("stocks state poisoned").selected;
        if idx >= base_count {
            // Transient row — `x` clears it; `-` is reserved for
            // persisted-list removal so users don't accidentally write
            // an empty array to disk when they meant "clear the
            // search."
            self.set_status("Press `x` to clear the lookup row");
            return;
        }
        let Some(sym) = symbols.get(idx).cloned() else {
            return;
        };
        self.state
            .lock()
            .expect("stocks state poisoned")
            .confirm_remove = Some(sym);
    }

    /// User answered `y` on the modal: actually remove the symbol from
    /// its source list (indices or watchlist), persist the change,
    /// clamp selection, drop the in-memory quote so a removed row
    /// doesn't briefly flash if the symbol gets re-added later.
    fn confirm_remove(&mut self) {
        let sym = match self
            .state
            .lock()
            .expect("stocks state poisoned")
            .confirm_remove
            .clone()
        {
            Some(s) => s,
            None => return,
        };
        let target = Self::classify_symbol(&sym);
        let list = match target {
            StocksListKind::Indices => &mut self.config.indices,
            StocksListKind::Watchlist => &mut self.config.watchlist,
        };
        let before = list.len();
        list.retain(|s| !s.eq_ignore_ascii_case(&sym));
        let removed = list.len() < before;
        if !removed {
            // Symbol vanished from under us (race with `:reload`).
            // Clear the modal and bail.
            self.state
                .lock()
                .expect("stocks state poisoned")
                .confirm_remove = None;
            return;
        }
        // Drop the stale quote so the row doesn't briefly flicker on
        // re-add. The next refresh repopulates from Yahoo. We sweep
        // every period's bucket — if the user had this symbol on
        // 1D and 1W in this session, dropping just one would leave
        // the other rendering yesterday's price under tomorrow's
        // labels.
        {
            let mut st = self.state.lock().expect("stocks state poisoned");
            for bucket in st.quotes_by_period.values_mut() {
                bucket.remove(&sym);
            }
            st.confirm_remove = None;
            // Clamp selection to the last row of the new list (or 0
            // when everything was removed).
            let new_total = self.config.indices.len() + self.config.watchlist.len();
            let with_transient = if st.transient_ticker.is_some() { 1 } else { 0 };
            let total = new_total + with_transient;
            st.selected = st.selected.min(total.saturating_sub(1));
        }
        let label = match target {
            StocksListKind::Indices => "indices",
            StocksListKind::Watchlist => "watchlist",
        };
        if self.persist_lists() {
            self.set_status(format!("Removed {sym} from {label}"));
        }
    }

    /// User pressed any key other than `y` on the modal — back out
    /// without touching disk.
    fn cancel_remove(&self) {
        self.state
            .lock()
            .expect("stocks state poisoned")
            .confirm_remove = None;
    }

    /// User pressed `+` on the transient lookup row: promote it into
    /// the appropriate persisted list (indices or watchlist) and
    /// remove the transient marker. No-op when the selection isn't on
    /// the transient row or no transient is pinned.
    fn add_transient_to_list(&mut self) {
        let sym = {
            let st = self.state.lock().expect("stocks state poisoned");
            st.transient_ticker.clone()
        };
        let Some(sym) = sym else {
            self.set_status("No lookup row to add — run `:stock <ticker>` first");
            return;
        };
        let target = Self::classify_symbol(&sym);
        let already = self
            .config
            .indices
            .iter()
            .chain(self.config.watchlist.iter())
            .any(|s| s.eq_ignore_ascii_case(&sym));
        if already {
            self.set_status(format!("{sym} is already in the list"));
            return;
        }
        match target {
            StocksListKind::Indices => self.config.indices.push(sym.clone()),
            StocksListKind::Watchlist => self.config.watchlist.push(sym.clone()),
        }
        // Clear the transient slot and re-select the just-added row.
        {
            let mut st = self.state.lock().expect("stocks state poisoned");
            st.transient_ticker = None;
            st.transient_searching = None;
            let new_idx = match target {
                StocksListKind::Indices => self.config.indices.len() - 1,
                StocksListKind::Watchlist => {
                    self.config.indices.len() + self.config.watchlist.len() - 1
                }
            };
            st.selected = new_idx;
        }
        let label = match target {
            StocksListKind::Indices => "indices",
            StocksListKind::Watchlist => "watchlist",
        };
        if self.persist_lists() {
            self.set_status(format!("Added {sym} to {label}"));
        }
        self.mark_dirty();
    }

    /// Open the selected ticker in the user's browser via the configured
    /// `jump_url_template` (replacing `{ticker}` with the URL-encoded symbol).
    /// No-op when no template is configured.
    fn jump_to_external(&self) {
        let Some(template) = &self.config.jump_url_template else {
            tracing::info!("Enter pressed but no jump_url_template is configured");
            return;
        };
        let Some(symbol) = self.selected_symbol() else {
            return;
        };
        let url = template.replace("{ticker}", &urlencoding::encode(&symbol));
        if let Err(err) = open::that(&url) {
            tracing::warn!(error = %err, url = %url, "failed to open jump URL");
        }
    }

    /// Single-lock helper called once at the top of `render`. Records
    /// the focus state for the next `is_due()` cadence pick AND
    /// returns the per-render quote snapshot — folding what would
    /// otherwise be two separate `state.lock()` calls into one.
    /// `QuoteState::Ready` carries `Arc<StockQuote>`, so the
    /// HashMap clone is O(N) atomic-increments, not O(N) deep
    /// StockQuote copies.
    fn record_focus_and_snapshot_quotes(
        &self,
        focused: bool,
    ) -> HashMap<String, QuoteState> {
        let mut st = self.state.lock().expect("stocks state poisoned");
        st.last_focused_at = focused.then(Instant::now);
        st.quotes(self.period).clone()
    }

    /// Return the active status string, clearing it if its TTL has
    /// elapsed. Called from `render` so feedback messages auto-revert
    /// without needing a separate timer task.
    fn live_status(&self) -> Option<String> {
        let mut st = self.state.lock().expect("stocks state poisoned");
        live_value(&mut st.status).cloned()
    }

    /// Paint the "Remove <symbol>?" overlay. Thin call into the
    /// shared [`crate::ui::modal`] helper so the styling stays
    /// consistent with notes / forex / future widgets.
    fn render_confirm_modal(&self, frame: &mut Frame, parent: Rect, symbol: &str) {
        crate::ui::modal::render(
            frame,
            parent,
            &self.theme,
            crate::ui::modal::ConfirmModal {
                title: " Remove ticker? ",
                target: symbol,
                hint: None,
                max_width: 48,
            },
        );
    }

    /// Compute the same panel rects `render` uses so click handlers can map
    /// coordinates back to a target panel.
    fn compute_layout(&self, inner: Rect) -> StocksPanels {
        // List column is sized to exactly fit the widest row content
        // (prefix + 7-col symbol + 10-col price + 2-col gap + 10-col
        // change = 31 chars), leaving a single col of trailing
        // whitespace. Combined with the 1-col explicit gap between
        // panels, that's ~2 visual spaces between the list and the
        // stats column — tight without crowding.
        const WIDE_LIST_W: u16 = 32;
        const WIDE_STATS_W: u16 = 30;
        const MIN_GRAPH_W: u16 = 24;
        let is_wide = inner.width >= WIDE_LIST_W + MIN_GRAPH_W;
        let with_stats = is_wide && inner.width >= WIDE_LIST_W + WIDE_STATS_W + MIN_GRAPH_W;
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
                .split(inner);
            let (stats_area, graph_area) = if with_stats {
                (Some(cols[2]), Some(cols[4]))
            } else {
                (None, Some(cols[2]))
            };
            StocksPanels {
                list_area: Some(cols[0]),
                stats_area,
                graph_area,
            }
        } else {
            let list_h = ((inner.height as f32) * 0.55).round() as u16;
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(list_h),
                    Constraint::Length(1),
                    Constraint::Fill(1),
                ])
                .split(inner);
            StocksPanels {
                list_area: Some(rows[0]),
                stats_area: None,
                graph_area: Some(rows[2]),
            }
        }
    }
}

struct StocksPanels {
    list_area: Option<Rect>,
    #[allow(dead_code)] // referenced by future "click stats panel" follow-up.
    stats_area: Option<Rect>,
    graph_area: Option<Rect>,
}

/// Maps a click row inside the list panel to the ticker index, accounting
/// for the current scroll offset. The list lays out: optional `── Indices ──`
/// header + N index rows, blank + `── Watchlist ──` + M watchlist rows,
/// optionally blank + `── Lookup ──` + 1 transient row.
fn list_ticker_at(
    click_row: u16,
    list_area: Rect,
    indices_count: usize,
    watchlist_count: usize,
    has_lookup: bool,
    scroll: usize,
) -> Option<usize> {
    let visible_rel = click_row.checked_sub(list_area.y)? as usize;
    let rel = visible_rel + scroll;
    let mut cursor = 0usize;
    if indices_count > 0 {
        if rel == cursor {
            return None;
        }
        cursor += 1;
        for i in 0..indices_count {
            if rel == cursor {
                return Some(i);
            }
            cursor += 1;
        }
    }
    if watchlist_count > 0 {
        if indices_count > 0 {
            if rel == cursor || rel == cursor + 1 {
                return None;
            }
            cursor += 2;
        } else {
            if rel == cursor {
                return None;
            }
            cursor += 1;
        }
        for i in 0..watchlist_count {
            if rel == cursor {
                return Some(indices_count + i);
            }
            cursor += 1;
        }
    }
    if has_lookup {
        // blank + header before the single transient row
        if rel == cursor || rel == cursor + 1 {
            return None;
        }
        cursor += 2;
        if rel == cursor {
            return Some(indices_count + watchlist_count);
        }
    }
    None
}

/// Maps a click on the toggle bar row to a Period.
fn period_at_click(click_col: u16, graph_area: Rect, active: Period) -> Option<Period> {
    let active_idx = Period::ALL.iter().position(|p| *p == active).unwrap_or(0);
    let widths: Vec<u16> = Period::ALL
        .iter()
        .map(|p| (p.label().len() as u16) + 2 + 1)
        .collect();
    let total: u16 = widths.iter().sum::<u16>().saturating_sub(1);
    let all_fit = (total + 2) <= graph_area.width;

    let mut x = graph_area.x + 1;
    if all_fit {
        for p in Period::ALL.iter() {
            let w = (p.label().len() as u16) + 2;
            if click_col >= x && click_col < x + w {
                return Some(*p);
            }
            x += w + 1;
        }
    } else {
        // Reconstruct same window as render_period_toggle_bar.
        let budget = graph_area.width.saturating_sub(4);
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
        if start > 0 {
            x += 2; // "‹ "
        }
        for i in start..end {
            let w = (Period::ALL[i].label().len() as u16) + 2;
            if click_col >= x && click_col < x + w {
                return Some(Period::ALL[i]);
            }
            x += w + 1;
        }
    }
    None
}

#[async_trait]
impl Widget for StocksWidget {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> &str {
        "stocks"
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
        // Surface tick-time status TTL expiry through the dirty bit so
        // the render filter actually gets to drop the now-stale chrome
        // — without this the "Added AAPL" line would linger until the
        // next unrelated redraw. Atomic-gated so an idle dashboard
        // (no pending status) doesn't lock the state mutex every 250 ms
        // just to check that the slot is still empty.
        if self.feedback_pending.load(Ordering::Relaxed) {
            let mut st = self.state.lock().expect("stocks state poisoned");
            if crate::ui::status::drain_if_expired(&mut st.status) {
                st.dirty = true;
            }
            if st.status.is_none() {
                self.feedback_pending.store(false, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    fn take_dirty(&mut self) -> bool {
        let mut st = self.state.lock().expect("stocks state poisoned");
        std::mem::replace(&mut st.dirty, false)
    }

    fn render(&self, frame: &mut Frame, area: Rect, focused: bool) {
        // Single-lock entry: record focus for the next is_due() cadence
        // pick AND grab the per-render quote snapshot in one critical
        // section. Hidden stack tabs don't get render() at all, so a
        // stale `last_focused_at` from a prior focused render naturally
        // ages out via the `FOCUS_FRESHNESS_WINDOW` check.
        let quotes = self.record_focus_and_snapshot_quotes(focused);
        let title = if self.instance == "main" {
            "Stocks".to_string()
        } else {
            format!("Stocks ({})", self.instance)
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

        let symbols: Vec<String> = self.all_symbols();
        let selected_sym = self.selected_symbol();
        let base_count = self.config.indices.len() + self.config.watchlist.len();
        let lookup_start = if symbols.len() > base_count {
            Some(base_count)
        } else {
            None
        };

        // Reserve the bottom row of the cell for the footer hint
        // before splitting up the rest. Without this carve-off the
        // graph's x-axis labels (rendered on the last row of the
        // graph panel) end up on the same row as the footer hint, so
        // the footer overwrites them.
        let footer_h: u16 = if inner.height >= 2 { 1 } else { 0 };
        let body = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: inner.height - footer_h,
        };

        // Adaptive layout: in landscape mode (wide), list | stats | graph
        // run horizontally — list + stats get their full width first, graph
        // fills whatever's left. In portrait mode (narrow), they stack
        // vertically: list on top, graph on the bottom.
        // List column is sized to exactly fit the widest row content
        // (prefix + 7-col symbol + 10-col price + 2-col gap + 10-col
        // change = 31 chars), leaving a single col of trailing
        // whitespace. Combined with the 1-col explicit gap between
        // panels, that's ~2 visual spaces between the list and the
        // stats column — tight without crowding.
        const WIDE_LIST_W: u16 = 32;
        const WIDE_STATS_W: u16 = 30;
        const MIN_GRAPH_W: u16 = 24;
        let is_wide = body.width >= WIDE_LIST_W + MIN_GRAPH_W;
        let with_stats = is_wide && body.width >= WIDE_LIST_W + WIDE_STATS_W + MIN_GRAPH_W;

        // 1-col gaps between panels so they don't visually run together.
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
            let new_scroll = render_list_panel(
                frame,
                list_area,
                &symbols,
                self.config.indices.len(),
                lookup_start,
                &quotes,
                sel,
                self.display_mode,
                self.period,
                cur_scroll,
                &self.theme,
            );
            self.state.lock().unwrap().list_scroll = new_scroll;
            if let Some(stats_area) = stats_area {
                render_stats_panel(
                    frame,
                    stats_area,
                    selected_sym.as_deref(),
                    &quotes,
                    &self.theme,
                );
            }
            render_graph_panel(
                frame,
                graph_area,
                selected_sym.as_deref(),
                &quotes,
                self.period,
                self.config.graph_high_low_lines,
                self.config.pad_intraday_to_full_day,
                &self.theme,
            );
        } else {
            // Portrait: list on top (clamped to ~55% so it's readable), graph below.
            let list_h = ((body.height as f32) * 0.55).round() as u16;
            let rows = Layout::default()
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
            let new_scroll = render_list_panel(
                frame,
                rows[0],
                &symbols,
                self.config.indices.len(),
                lookup_start,
                &quotes,
                sel,
                self.display_mode,
                self.period,
                cur_scroll,
                &self.theme,
            );
            self.state.lock().unwrap().list_scroll = new_scroll;
            render_graph_panel(
                frame,
                rows[2],
                selected_sym.as_deref(),
                &quotes,
                self.period,
                self.config.graph_high_low_lines,
                self.config.pad_intraday_to_full_day,
                &self.theme,
            );
        }

        // Footer hint along the bottom of the cell. The status line
        // (when present and not yet TTL-expired) replaces the static
        // hint so add/remove feedback grabs the user's eye.
        if inner.height >= 2 {
            let footer = Rect {
                x: inner.x,
                y: inner.y + inner.height - 1,
                width: inner.width,
                height: 1,
            };
            let status = self.live_status();
            let (text, style) = match status {
                Some(msg) => (msg, self.theme.text_selected),
                None => (
                    format!(
                        "↑/↓ select · c mode ({}) · o open · - remove · + add lookup · r refresh",
                        display_mode_label(self.display_mode)
                    ),
                    self.theme.text_dim,
                ),
            };
            frame.render_widget(
                Paragraph::new(Span::styled(text, style)).alignment(Alignment::Right),
                footer,
            );
        }

        // Confirm-remove modal: paints on top of everything else so
        // the user can't miss the `y/N` prompt.
        let pending = self
            .state
            .lock()
            .expect("stocks state poisoned")
            .confirm_remove
            .clone();
        if let Some(sym) = pending {
            self.render_confirm_modal(frame, inner, &sym);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if key.modifiers != KeyModifiers::NONE && key.modifiers != KeyModifiers::SHIFT {
            return EventResult::Ignored;
        }
        // Uppercase ASCII letters are reserved for the app-wide
        // `Shift+<letter>` focus-jump dispatcher — never consume them here.
        // SHIFT itself stays permitted above so shifted non-letter chars
        // like `%` and `$` (the display-mode toggles below) keep working
        // on terminals that propagate the modifier with the symbol.
        if let KeyCode::Char(c) = key.code {
            if c.is_ascii_uppercase() {
                return EventResult::Ignored;
            }
        }
        // Confirm-remove modal: y commits, any other key cancels.
        // Handled before the normal dispatch so the user can't
        // accidentally move selection / cycle period while the prompt
        // is up.
        if self
            .state
            .lock()
            .expect("stocks state poisoned")
            .confirm_remove
            .is_some()
        {
            match crate::ui::modal::dispatch_key(key) {
                crate::ui::modal::ConfirmChoice::Confirm => self.confirm_remove(),
                crate::ui::modal::ConfirmChoice::Cancel => self.cancel_remove(),
            }
            return EventResult::Handled;
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
            // `o` opens the selected ticker in the browser, per the
            // platform-wide convention (Enter is reserved for the
            // primary in-place action — and stocks has none today, so
            // we leave Enter unbound rather than reusing it for an
            // external jump that's too easy to mis-fire).
            KeyCode::Char('o') => {
                self.jump_to_external();
                EventResult::Handled
            }
            KeyCode::Char('%') => {
                self.display_mode = DisplayMode::Percent;
                EventResult::Handled
            }
            KeyCode::Char('$') => {
                self.display_mode = DisplayMode::Dollar;
                EventResult::Handled
            }
            // `c` cycles between the two — convenient single-key toggle.
            KeyCode::Char('c') => {
                self.display_mode = match self.display_mode {
                    DisplayMode::Percent => DisplayMode::Dollar,
                    DisplayMode::Dollar => DisplayMode::Percent,
                };
                EventResult::Handled
            }
            KeyCode::Char('r') => {
                self.mark_dirty();
                EventResult::Handled
            }
            // `x` clears the :stock <query> transient selection if any.
            KeyCode::Char('x') => {
                self.clear_transient();
                EventResult::Handled
            }
            // `-` prompts to remove the selected ticker/index. The
            // actual mutation runs after the user confirms with `y`.
            KeyCode::Char('-') => {
                self.request_remove_selected();
                EventResult::Handled
            }
            // `+` promotes the transient `:stock` lookup row into
            // indices (^prefix) or watchlist. No confirmation —
            // additions are non-destructive.
            KeyCode::Char('+') => {
                self.add_transient_to_list();
                EventResult::Handled
            }
            // 1..9 picks a graph period directly.
            KeyCode::Char(d @ '1'..='9') => {
                let idx = (d as u8 - b'1') as usize;
                if let Some(p) = Period::ALL.get(idx) {
                    self.set_period(*p);
                }
                EventResult::Handled
            }
            // ← / → (and h / l for vim-style symmetry with j/k selection)
            // cycle the graph period through Period::ALL, wrapping at the
            // ends. Matches what horizontal scroll does when the user has
            // `horizontal_scroll_period` enabled — but available
            // unconditionally from the keyboard.
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

    fn handle_mouse(&mut self, mouse: MouseEvent, area: Rect) -> EventResult {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.move_selection(-1);
                EventResult::Handled
            }
            MouseEventKind::ScrollDown => {
                self.move_selection(1);
                EventResult::Handled
            }
            // Horizontal scroll cycles the period toggles only when the user
            // has opted into it via `horizontal_scroll_period` in stocks.toml
            // — accidental trackpad sideways gestures are common otherwise.
            MouseEventKind::ScrollLeft if self.config.horizontal_scroll_period => {
                self.cycle_period(false);
                EventResult::Handled
            }
            MouseEventKind::ScrollRight if self.config.horizontal_scroll_period => {
                self.cycle_period(true);
                EventResult::Handled
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if area.width < 2 || area.height < 2 {
                    return EventResult::Ignored;
                }
                let inner = Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2);
                let layout = self.compute_layout(inner);
                // Try the toggle bar first — its row is fixed at the top of
                // the graph panel.
                if let Some(graph_area) = layout.graph_area {
                    let toggle_y = graph_area.y + 1;
                    if mouse.row == toggle_y
                        && mouse.column >= graph_area.x
                        && mouse.column < graph_area.x + graph_area.width
                    {
                        if let Some(p) = period_at_click(mouse.column, graph_area, self.period) {
                            self.set_period(p);
                            return EventResult::Handled;
                        }
                    }
                }
                // Then list row click.
                if let Some(list_area) = layout.list_area {
                    if mouse.row >= list_area.y
                        && mouse.row < list_area.y + list_area.height
                        && mouse.column >= list_area.x
                        && mouse.column < list_area.x + list_area.width
                    {
                        let (scroll, has_lookup) = {
                            let st = self.state.lock().expect("stocks state poisoned");
                            (st.list_scroll, st.transient_ticker.is_some())
                        };
                        if let Some(idx) = list_ticker_at(
                            mouse.row,
                            list_area,
                            self.config.indices.len(),
                            self.config.watchlist.len(),
                            has_lookup,
                            scroll,
                        ) {
                            let changed = {
                                let mut st = self.state.lock().expect("stocks state poisoned");
                                let prev = st.selected;
                                st.selected = idx;
                                idx != prev
                            };
                            if changed {
                                self.persist_runtime_state();
                            }
                            return EventResult::Handled;
                        }
                    }
                }
                EventResult::Ignored
            }
            _ => EventResult::Ignored,
        }
    }

    fn handle_command(&mut self, cmd: &str, args: &[&str]) -> Result<bool> {
        match cmd {
            "stock" | "stocks" | "s" => {
                if args.is_empty() {
                    anyhow::bail!("usage: :stock <symbol-or-name>");
                }
                let query = args.join(" ");
                self.lookup_and_set_transient(&query);
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
            ("↑ / ↓ / j / k", "select ticker (j = down, k = up)"),
            ("← / → / h / l", "cycle graph period (prev / next)"),
            ("c", "cycle display mode (% / $)"),
            ("% / $", "set display mode directly"),
            ("1-9", "set graph period directly"),
            ("o", "open selected ticker in browser"),
            ("r", "force refresh"),
            ("x", "clear :stock lookup (return to default list)"),
            ("-", "remove the selected ticker (with confirmation)"),
            ("+", "add the :stock lookup ticker to the watchlist"),
            ("click ticker", "select that ticker"),
            ("click toggle", "switch graph period"),
            (":stock <sym|name>", "look up a ticker and pin it"),
        ]
    }

    fn config(&self) -> serde_json::Value {
        serde_json::json!({
            "indices": self.config.indices,
            "watchlist": self.config.watchlist,
            "poll_interval_secs": self.config.poll_interval_secs,
            "focused_poll_interval_secs": self.config.focused_poll_interval_secs,
            "display_mode": display_mode_label(self.display_mode),
        })
    }

    fn apply_config(&mut self, config: serde_json::Value) -> Result<()> {
        let new_config: StocksConfig =
            serde_json::from_value(config).context("invalid stocks config payload")?;
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

    fn poll_snapshot(&self) -> Option<crate::polling::PollSnapshot> {
        Some(
            self.state
                .lock()
                .expect("stocks state poisoned")
                .poll
                .snapshot(),
        )
    }

    fn shortcut_preferences(&self) -> &[char] {
        // Effective preference list — user TOML override if non-empty,
        // otherwise the built-in `s, t, o, c, k` fallback. Built once at
        // construction so the trait can hand out a borrow.
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

impl StocksWidget {
    /// Dynamic metadata for the title bar: ticker count + active
    /// period (e.g. `"8 tickers · 1d"`). `None` for an empty
    /// watchlist — happens during first-launch before the user has
    /// added any.
    fn title_metadata_string(&self) -> Option<String> {
        let n = self.config.indices.len() + self.config.watchlist.len();
        if n == 0 {
            return None;
        }
        Some(format!("{n} tickers"))
    }
}

fn display_mode_label(m: DisplayMode) -> &'static str {
    match m {
        DisplayMode::Percent => "%",
        DisplayMode::Dollar => "$",
    }
}

/// Heuristic: does `s` look like a Yahoo ticker (e.g. `AAPL`, `^GSPC`,
/// `BRK-A`, `CAD=X`) for which we can skip the search hop? Requires that
/// every letter already be uppercase — a query like "vertex" (6 lowercase
/// letters) passes the alphanumeric test but is almost always a company
/// name, not a ticker. Forcing case-sensitivity routes those through
/// Yahoo's search where they belong.
fn is_tickerish(s: &str) -> bool {
    let len = s.chars().count();
    if !(1..=8).contains(&len) {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || matches!(c, '^' | '.' | '-' | '='))
}

fn render_graph_panel(
    frame: &mut Frame,
    area: Rect,
    selected: Option<&str>,
    quotes: &HashMap<String, QuoteState>,
    period: Period,
    show_high_low_lines: bool,
    pad_intraday_to_full_day: bool,
    theme: &Theme,
) {
    if area.width < 4 || area.height < 4 {
        return;
    }
    // Row 0 = ticker header, row 1 = period toggle bar, last row = x-axis.
    let toggle_row_y = area.y + 1;
    render_period_toggle_bar(
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

    let quote = selected.and_then(|s| match quotes.get(s) {
        Some(QuoteState::Ready(q)) => Some(q.as_ref()),
        _ => None,
    });
    let Some(q) = quote else {
        let msg = match selected {
            Some(s) => format!("Loading {s}…"),
            None => "Select a ticker".to_string(),
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
        return;
    };

    // Reserve rows: header(1) + toggle(1) + xaxis(1).
    let header_h = 2u16; // header + toggle
    let xaxis_h = 1u16;
    let plot_top = area.y + header_h;
    let plot_h = area.height.saturating_sub(header_h + xaxis_h);

    // Header.
    let (chg, pct) = period_change(q, period);
    let (color, glyph) = if chg >= 0.0 {
        (Color::Green, '▲')
    } else {
        (Color::Red, '▼')
    };
    let currency = q.currency.as_deref().unwrap_or("");
    let mut header_spans = vec![
        Span::styled(
            q.symbol.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{:.2} {currency}", q.price),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{glyph} {:+.2} ({:+.2}%) {}", chg, pct, period.label()),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ];

    // Extended-hours segment — only on 1D. Picks the most recent session via
    // Yahoo's `marketState`, falling back to post-market when present and pre
    // otherwise. Hidden when the change is exactly zero (no movement yet).
    if matches!(period, Period::Day) {
        if let Some((label, ah_chg, ah_pct)) = extended_hours_segment(q) {
            let (ah_color, ah_glyph) = if ah_chg >= 0.0 {
                (Color::Green, '▲')
            } else {
                (Color::Red, '▼')
            };
            header_spans.push(Span::raw("  "));
            header_spans.push(Span::styled(
                format!("{ah_glyph} {:+.2} ({:+.2}%) {label}", ah_chg, ah_pct),
                Style::default().fg(ah_color).add_modifier(Modifier::BOLD),
            ));
        }
    }
    let header = Line::from(header_spans);
    frame.render_widget(
        Paragraph::new(header),
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        },
    );

    if plot_h == 0 || q.intraday.is_empty() {
        return;
    }

    // Filter to the bars we'll actually paint BEFORE computing the
    // y-range — on 1D `q.intraday` includes pre-market and after-
    // hours bars (we keep `includePrePost=true` on the fetch so the
    // AH/PRE header can derive from them), but only the regular-
    // session bars land on the chart. Scanning the full extended-
    // hours window for min/max stretched the y-axis to fit price
    // moves that the visible trace never reaches, leaving dead
    // space at the top and/or bottom of the plot.
    //
    // For non-1D periods `filtered` is None and `intraday_render`
    // points at `q.intraday` directly, so the behavior there is
    // unchanged.
    let filtered: Option<(Vec<f64>, Vec<i64>, (i64, i64))> = if matches!(period, Period::Day) {
        pick_day_chart_bars_with_session(q)
    } else {
        None
    };
    let (intraday_render, timestamps_render): (&[f64], &[i64]) = match &filtered {
        Some((vs, ts, _)) => (vs.as_slice(), ts.as_slice()),
        None => (q.intraday.as_slice(), q.intraday_timestamps.as_slice()),
    };
    if intraday_render.is_empty() {
        return;
    }

    // Compute y-range from the visible bars.
    let (mut min, mut max) = (f64::INFINITY, f64::NEG_INFINITY);
    for v in intraday_render {
        if *v < min {
            min = *v;
        }
        if *v > max {
            max = *v;
        }
    }
    if min == max {
        min -= 0.5;
        max += 0.5;
    }
    // Padding above/below the data range. Set to 0 so the trace peak
    // touches the top row and the trough touches the bottom — that way
    // the high/low reference lines line up exactly with where the trace
    // visually reaches. A non-zero pad (e.g. 0.05) was the previous
    // default; it gave the trace breathing room from the border but
    // pushed reference lines ~1 row off the edges.
    let pad = 0.0;
    let plot_min = min - pad;
    let plot_max = max + pad;

    // Y-axis: reserve 7 cols for labels (e.g., "198.32 ").
    const Y_LABEL_W: u16 = 8;
    let plot_x = area.x + Y_LABEL_W;
    let plot_w = area.width.saturating_sub(Y_LABEL_W);
    if plot_w < 4 {
        return;
    }

    // Render y-axis labels — place each at its actual row in the plot, not
    // at consecutive rows (which was the previous bug: labels clustered at
    // the top of the chart because they all rendered at plot_top + idx).
    for row in label_rows(plot_h) {
        let frac = row as f64 / (plot_h as f64 - 1.0).max(1.0);
        let v = plot_max - frac * (plot_max - plot_min);
        let rect = Rect {
            x: area.x,
            y: plot_top + row,
            width: Y_LABEL_W,
            height: 1,
        };
        let label = format!("{:>6.2} ", v);
        frame.render_widget(Paragraph::new(Span::styled(label, theme.text_dim)), rect);
    }

    // === Trace rendering ===
    //
    // Session selection (computed above for the y-range) drives the
    // bars that actually paint. On 1D the chart shows only the
    // regular session bars; on longer periods `intraday_render` is
    // the unfiltered series. See the doc-comment on
    // `pick_day_chart_bars_with_session` for how the today/yesterday
    // fallback works on 1D.

    // trace_w = how many columns the actual trace fills.
    //   - 1D pad mode: elapsed time within the regular session, not
    //     a bar count. The provider downsamples 1D to 240 total
    //     points across pre+regular+post, so the regular-session
    //     slice ends up much smaller than the natural 78-at-5m count
    //     — using bar count would under-fill the chart by ~38% even
    //     after the session has fully closed.
    //   - YTD: day-of-year / days-in-year × plot_w.
    //   - other periods / non-pad: full plot_w.
    let trace_w = if pad_intraday_to_full_day && matches!(period, Period::Day) {
        let frac = match &filtered {
            Some((_, _, (start, end))) if *end > *start => {
                let now = chrono::Utc::now().timestamp();
                let span = (*end - *start) as f64;
                let elapsed = (now - *start) as f64;
                (elapsed / span).clamp(0.0, 1.0)
            }
            // No session bounds → can't reason about elapsed time;
            // fall back to bar-count fraction so we at least paint
            // *something* sized to the data.
            _ => (intraday_render.len() as f64 / TRADING_DAY_BARS as f64).clamp(0.0, 1.0),
        };
        let w = (plot_w as f64 * frac).round() as u16;
        w.clamp(2, plot_w)
    } else if matches!(period, Period::YearToDate) {
        let now = chrono::Local::now();
        let day_of_year = now.ordinal() as f64; // 1..=366
        let days_in_year = if is_leap_year(now.year()) {
            366.0
        } else {
            365.0
        };
        let frac = (day_of_year / days_in_year).clamp(0.0, 1.0);
        let w = (plot_w as f64 * frac).round() as u16;
        w.clamp(2, plot_w)
    } else {
        plot_w
    };
    let rows = graph::render_series(intraday_render, plot_h, trace_w, plot_min, plot_max);
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

    // Calendar-aligned vertical guides for periods 1W and longer. Each
    // guide marks the start of the natural unit appropriate to the
    // period (day, week, month, quarter, year, or biennium). The same
    // (column, label) pairs drive both the guides and the x-axis labels
    // so they line up vertically.
    let annotations = period_annotations(period, timestamps_render);
    if !annotations.is_empty() && timestamps_render.len() >= 2 {
        let n = timestamps_render.len();
        let faint = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM);
        for ann in &annotations {
            // Skip column 0 — the left edge of the chart is implicitly
            // the start of the first unit; drawing a guide there would
            // overlay the y-axis labels and look noisy.
            if ann.bar_index == 0 {
                continue;
            }
            let frac = ann.bar_index as f64 / (n - 1) as f64;
            let col = (frac * (trace_w as f64 - 1.0)).round() as u16;
            let col = col.min(trace_w.saturating_sub(1));
            draw_vertical_guide(frame, plot_x + col, plot_top, plot_h, &rows, col, faint);
        }
    }

    // Reference lines: drawn AFTER the trace so we can overlay only on cells
    // the trace left blank (preserves the trace where they would overlap).
    //
    // Anchor (always drawn):
    //   - 1D view → previous day's close
    //   - other periods → first sample of the visible range
    // High/low lines are drawn on non-Day periods when `show_high_low_lines`
    // is true. They're styled "very faint" so they don't compete with the trace.
    let anchor_value = if matches!(period, Period::Day) {
        q.previous_close
    } else {
        q.intraday.first().copied().unwrap_or(q.previous_close)
    };
    draw_reference_line(
        frame,
        plot_x,
        plot_top,
        plot_h,
        plot_w,
        plot_min,
        plot_max,
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
            frame, plot_x, plot_top, plot_h, plot_w, plot_min, plot_max, &rows, max, '┈', faint,
        );
        draw_reference_line(
            frame, plot_x, plot_top, plot_h, plot_w, plot_min, plot_max, &rows, min, '┈', faint,
        );
    }

    // X-axis labels: a few evenly-spaced markers — content varies by period.
    let xaxis_rect = Rect {
        x: plot_x,
        y: plot_top + plot_h,
        width: plot_w,
        height: 1,
    };
    // 1Y is a rolling 12-month window ending today — labels walk back
    // from this month at 2-month intervals, 7 labels total. Static
    // `Jan Mar May Jul Sep Nov` would mis-represent any 1Y graph that
    // doesn't happen to start in January. YTD adds a trailing `Dec`
    // so the year visibly spans Jan→Dec across the plot.
    // 1D keeps the legacy even-distribution labels (the regular session
    // is a uniform 6h 15m window, so even spacing maps cleanly to time).
    // Longer periods drive labels from the same annotation list as the
    // vertical guides so the labels line up directly under their guides.
    let line = if matches!(period, Period::Day) || annotations.is_empty() {
        let labels: Vec<String> = match period {
            Period::Day => {
                str_labels(&["9:30", "10:45", "12:00", "13:15", "14:30", "15:45"])
            }
            Period::Week => str_labels(&["Mon", "Tue", "Wed", "Thu", "Fri"]),
            Period::Month => str_labels(&["wk1", "wk2", "wk3", "wk4"]),
            Period::SixMonth => str_labels(&["1mo", "2mo", "3mo", "4mo", "5mo", "6mo"]),
            Period::YearToDate => str_labels(&["Jan", "Mar", "May", "Jul", "Sep", "Nov", "Dec"]),
            Period::Year => rolling_year_month_labels(chrono::Local::now().date_naive()),
            Period::ThreeYear => str_labels(&["-3y", "-2y", "-1y", "now"]),
            Period::FiveYear => str_labels(&["-5y", "-4y", "-3y", "-2y", "-1y", "now"]),
            Period::TenYear => str_labels(&["-10y", "-8y", "-6y", "-4y", "-2y", "now"]),
        };
        let label_refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
        lay_out_x_axis_labels(&label_refs, plot_w as usize)
    } else {
        // Place each annotation's label at the column matching its
        // vertical guide. Every annotation lands a label, including
        // the first (bar_index 0) — without it, 1W readers were
        // counting only 4 day labels (Tue/Wed/Thu/Fri) and thinking
        // the chart covered 4 days when there's a full 5th day of
        // data on the left. The vertical-guide pass still skips
        // bar_index 0 to avoid colliding with the y-axis labels;
        // the x-axis label row is one row below the plot, well
        // clear of that overlap. Overlap collisions between labels
        // resolve in favor of the earlier (leftmost) one.
        let cols: Vec<(usize, &str)> = annotations
            .iter()
            .map(|ann| {
                let n = timestamps_render.len();
                let frac = if n <= 1 {
                    0.0
                } else {
                    ann.bar_index as f64 / (n - 1) as f64
                };
                let col = (frac * (trace_w as f64 - 1.0)).round() as usize;
                (col.min(trace_w.saturating_sub(1) as usize), ann.label.as_str())
            })
            .collect();
        lay_out_x_axis_labels_at_cols(&cols, plot_w as usize)
    };
    frame.render_widget(
        Paragraph::new(Span::styled(line, theme.text_dim)),
        xaxis_rect,
    );
}

/// Pack `labels` into a single string `width` cells wide where the
/// first label is left-anchored at column 0 and the last label is
/// right-anchored at column `width`. Intermediate labels are spaced
/// linearly. Trailing chars that would overflow `width` are clipped.
/// Copy a static `&[&str]` into an owned `Vec<String>` so the x-axis
/// label match arm can mix static + dynamic sets without lifetime
/// gymnastics.
fn str_labels(labels: &[&str]) -> Vec<String> {
    labels.iter().map(|s| (*s).to_string()).collect()
}

/// 7 month-name labels for a rolling 12-month window ending today,
/// stepped 2 months apart so the `lay_out_x_axis_labels` 6-interval
/// layout maps exactly to 12 months. e.g. today=2026-05-23 →
/// `["May","Jul","Sep","Nov","Jan","Mar","May"]`.
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

fn lay_out_x_axis_labels(labels: &[&str], width: usize) -> String {
    if labels.is_empty() || width == 0 {
        return String::new();
    }
    let n = labels.len();
    if n == 1 {
        return labels[0].chars().take(width).collect();
    }
    let last_w = labels.last().map(|s| s.chars().count()).unwrap_or(0);
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

/// Overlay a horizontal reference line at the row corresponding to `value`,
/// painting `ch` only at columns the trace left blank. Writing directly into
/// the frame buffer keeps the trace's braille glyphs intact where they sit on
/// the same row.
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
    // Walk the full plot width even when the trace is narrower than `plot_w`
    // (1D trading-day-progress mode). For cells the trace covers we skip
    // anywhere the trace has a glyph; for cells past the trace's right edge
    // we always paint, so the reference line extends across the empty
    // "future trading time" portion too.
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

/// Annotation for the calendar-aligned vertical guides + x-axis labels.
/// Each entry pins a label to a specific bar index; the renderer maps
/// that bar's column position to draw both the guide and the label so
/// they share an x-coordinate.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PeriodAnnotation {
    bar_index: usize,
    label: String,
}

/// Compute the calendar-boundary annotations for `period` from a slice of
/// bar timestamps (unix seconds, UTC). Each annotation pins a short label
/// to the first bar of a new natural unit:
///   - 1W → start of each new ET trading day (Mon/Tue/Wed/Thu/Fri).
///   - 1M → start of each new ISO week (Mon).
///   - 6M / YTD → start of each new month.
///   - 1Y → start of each new calendar quarter.
///   - 3Y / 5Y → start of each new calendar year.
///   - 10Y → every second calendar year boundary.
/// 1D returns an empty list — the regular session is one unit, so the
/// only useful x-axis markers are the legacy time-of-day labels.
fn period_annotations(period: Period, timestamps: &[i64]) -> Vec<PeriodAnnotation> {
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
    match period {
        Period::Day => Vec::new(),
        Period::Week => annotate_when_changes(&local, |dt| dt.date_naive().ordinal0() as i32, |dt| {
            // Mon/Tue/Wed/Thu/Fri abbreviation from the weekday.
            match dt.weekday() {
                Weekday::Mon => "Mon",
                Weekday::Tue => "Tue",
                Weekday::Wed => "Wed",
                Weekday::Thu => "Thu",
                Weekday::Fri => "Fri",
                Weekday::Sat => "Sat",
                Weekday::Sun => "Sun",
            }
            .to_string()
        }),
        Period::Month => annotate_when_changes(
            &local,
            |dt| dt.iso_week().week() as i32 * 100 + (dt.iso_week().year() % 100),
            |dt| format!("wk{}", iso_week_of_month_or_zero(*dt) + 1),
        ),
        Period::SixMonth | Period::YearToDate => annotate_when_changes(
            &local,
            |dt| dt.year() * 100 + dt.month() as i32,
            |dt| short_month_name(dt.month()).to_string(),
        ),
        Period::Year => annotate_when_changes(
            &local,
            |dt| dt.year() * 10 + ((dt.month() as i32 - 1) / 3),
            |dt| short_month_name(dt.month()).to_string(),
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
    }
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

fn short_month_name(month: u32) -> &'static str {
    match month {
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
        _ => "—",
    }
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

/// Place `(col, label)` pairs into a `width`-cell line so each label
/// starts at its requested column. Labels rendered in input order; an
/// earlier label wins any overlap with a later one. Trailing labels
/// that would extend past `width` are truncated.
fn lay_out_x_axis_labels_at_cols(items: &[(usize, &str)], width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut buf: Vec<char> = vec![' '; width];
    for (col, label) in items {
        let start = (*col).min(width.saturating_sub(1));
        let chars: Vec<char> = label.chars().collect();
        // Center the label on `col` so it visually anchors on the guide.
        // Right edge would overflow off the chart for the rightmost label —
        // clamp so the last label's right edge sits at width-1.
        let half = chars.len() / 2;
        let mut left = start.saturating_sub(half);
        if left + chars.len() > width {
            left = width.saturating_sub(chars.len());
        }
        // Skip if the slot is already painted (earlier label wins).
        if buf[left..(left + chars.len()).min(width)]
            .iter()
            .any(|c| *c != ' ')
        {
            continue;
        }
        for (i, ch) in chars.iter().enumerate() {
            if left + i >= width {
                break;
            }
            buf[left + i] = *ch;
        }
    }
    buf.iter().collect()
}

/// Draw a faint vertical guide line at a fixed column inside the plot.
/// Skips rows where the trace already painted a glyph at that column so the
/// guide reads as "behind" the trace where they overlap. Used for 1D
/// pre-market / post-market cutoffs.
#[allow(clippy::too_many_arguments)]
fn draw_vertical_guide(
    frame: &mut Frame,
    x: u16,
    plot_top: u16,
    plot_h: u16,
    trace_rows: &[String],
    trace_col: u16,
    style: Style,
) {
    if plot_h == 0 {
        return;
    }
    let buf = frame.buffer_mut();
    for row in 0..plot_h as usize {
        let trace_owns_cell = trace_rows
            .get(row)
            .and_then(|s| s.chars().nth(trace_col as usize))
            .map(|c| c != ' ')
            .unwrap_or(false);
        if trace_owns_cell {
            continue;
        }
        let y = plot_top + row as u16;
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_char('│');
            cell.set_style(style);
        }
    }
}

/// Gregorian leap-year predicate. Inlined so the YTD x-axis math
/// doesn't need a chrono detour for one ternary.
fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

/// Returns (change_abs, change_pct) for the given period. 1D uses the
/// previous-close convention (standard ticker change); longer windows use
/// the first sample in the series as the baseline.
/// Pick which extended-hours segment (if any) to render in the 1D header.
/// Returns `(label, change, change_pct)` where `label` is "AH" for the
/// post-market session and "PRE" for the pre-market session.
///
/// Source-of-truth ranking:
///   1. The 1D intraday bars themselves — most accurate, naturally
///      persists through the overnight gap (Yahoo's `1d` response still
///      contains today's post-market bars until tomorrow's pre-market
///      opens), and reflects exactly what the chart is showing.
///   2. Yahoo's `meta.postMarket*` / `preMarket*` fields — fallback for
///      old cached quotes without timestamps, or non-1D periods.
///
/// Returns `None` during the regular session (no extended movement to
/// surface) or when the change is exactly zero (no movement yet).
/// Pick the regular-session bars the 1D chart should render. Tries
/// today's regular session first (`regular_session_*_ts`), falls back
/// to yesterday's (`previous_session_*_ts`) when today has no bars
/// yet, returns `None` when neither yields anything. Pre-market and
/// after-hours bars are never included — those sessions only update
/// the AH/PRE header line.
/// Same shape as [`pick_day_chart_bars_with_session`] but discards
/// the session-bounds tuple. Test-only helper — production renderer
/// needs the bounds to compute trace-width fill fraction from
/// elapsed time.
#[cfg(test)]
fn pick_day_chart_bars(q: &StockQuote) -> Option<(Vec<f64>, Vec<i64>)> {
    pick_day_chart_bars_with_session(q).map(|(vs, ts, _)| (vs, ts))
}

/// Select the regular-session bars for the 1D chart and report the
/// `(start, end)` timestamps of whichever session was chosen. Tries
/// today's regular session first; falls back to yesterday's when
/// today has no bars yet (overnight gap, pre-market). Returns the
/// session bounds alongside so the trace-width calc can compute
/// fill fraction from *elapsed time*, not from the downsampled
/// bar count — the provider tops the 1D series at 240 points
/// across pre+regular+post, so the regular-session slice ends up
/// much smaller than the natural 78-bar count and would under-
/// fill the chart even at session close.
fn pick_day_chart_bars_with_session(q: &StockQuote) -> Option<(Vec<f64>, Vec<i64>, (i64, i64))> {
    if q.intraday.len() != q.intraday_timestamps.len() {
        return None;
    }
    let filter_range = |start: i64, end: i64| -> Vec<(f64, i64)> {
        q.intraday
            .iter()
            .zip(q.intraday_timestamps.iter())
            .filter(|(_, t)| **t >= start && **t <= end)
            .map(|(v, t)| (*v, *t))
            .collect()
    };
    let today = match (q.regular_session_start_ts, q.regular_session_end_ts) {
        (Some(s), Some(e)) => (filter_range(s, e), (s, e)),
        _ => (Vec::new(), (0, 0)),
    };
    let (chosen, bounds) = if !today.0.is_empty() {
        today
    } else {
        match (q.previous_session_start_ts, q.previous_session_end_ts) {
            (Some(s), Some(e)) => (filter_range(s, e), (s, e)),
            _ => (Vec::new(), (0, 0)),
        }
    };
    if chosen.is_empty() {
        None
    } else {
        let (vs, ts): (Vec<f64>, Vec<i64>) = chosen.into_iter().unzip();
        Some((vs, ts, bounds))
    }
}

fn extended_hours_segment(q: &StockQuote) -> Option<(&'static str, f64, f64)> {
    if let Some(seg) = extended_hours_from_bars(q) {
        return Some(seg);
    }
    extended_hours_from_meta(q)
}

/// How close to `regular_session_start_ts` the latest bar must be for
/// us to consider it a pre-market bar. Yahoo's pre-market window runs
/// ~04:00–09:30 ET (5.5 hours); we use 7 hours as a safety cushion
/// for holidays and platform edge cases. Bars older than this — but
/// still before `regular_session_start_ts` — get treated as the
/// previous day's after-hours carried through the overnight gap.
const PRE_MARKET_LOOKBACK_SECS: i64 = 7 * 3600;

/// Derive AH/PRE from the intraday timestamps + the regular-session
/// boundaries.
///
/// **Baseline = `q.price` (Yahoo's `meta.regularMarketPrice`).** This
/// is *the official closing-auction price* of the most recently
/// completed regular session — what MarketWatch, finance.yahoo.com,
/// and macOS Stocks all use to compute AH/PRE change. We deliberately
/// do NOT pick a baseline bar from `intraday`: Yahoo's 5-min bars
/// straddle the regular close (the 15:55 bar covers 15:55–16:00 and
/// the bar at ts == reg_end is the *first AH bar*, not the close
/// auction). Either of those bar closes differs from the official
/// auction price by a few cents, producing AH-change values that
/// don't agree with what users see elsewhere. `q.price` is the
/// canonical close.
///
/// Three cases:
///   1. **Post-market / post-current-day AH** — latest bar past
///      `regular_session_end_ts`. Label "AH". Baseline `q.price`
///      (today's close).
///   2. **Pre-market of the upcoming session** — latest bar before
///      `regular_session_start_ts`, within `PRE_MARKET_LOOKBACK_SECS`
///      of it. Label "PRE". Baseline `q.price` (yesterday's close —
///      Yahoo updates `regularMarketPrice` to the last completed
///      session's close once that session ends).
///   3. **Overnight gap** — latest bar before
///      `regular_session_start_ts` but older than the pre-market
///      lookback window. Label "AH" (yesterday's after-hours session
///      is the most recent activity, carried through the gap).
///      Baseline `q.price` (yesterday's close). This is the case the
///      original logic mis-labeled as "PRE" with the wrong baseline.
///
/// Returns `None` during the regular session.
fn extended_hours_from_bars(q: &StockQuote) -> Option<(&'static str, f64, f64)> {
    if q.intraday.is_empty() || q.intraday_timestamps.is_empty() {
        return None;
    }
    if q.intraday.len() != q.intraday_timestamps.len() {
        return None;
    }
    let reg_end = q.regular_session_end_ts?;
    let reg_start = q.regular_session_start_ts?;

    let last_idx = q.intraday.len() - 1;
    let last_ts = q.intraday_timestamps[last_idx];
    let last_price = q.intraday[last_idx];

    let label = if last_ts > reg_end {
        "AH"
    } else if last_ts < reg_start {
        // Pre-market of the upcoming session vs. overnight gap
        // carrying yesterday's AH. Distinguished by how close
        // `last_ts` is to `reg_start`: real pre-market bars sit
        // within the pre-market window; anything older is overnight.
        if (reg_start - last_ts) < PRE_MARKET_LOOKBACK_SECS {
            "PRE"
        } else {
            "AH"
        }
    } else {
        // Latest bar sits inside the regular session — no
        // extended-hours segment to render.
        return None;
    };

    finalize_segment(label, last_price, q.price)
}

/// Build the `(label, change, change_pct)` triple, returning `None`
/// when the baseline is zero/invalid or the change is exactly zero
/// (nothing worth surfacing on the header).
fn finalize_segment(
    label: &'static str,
    last_price: f64,
    baseline: f64,
) -> Option<(&'static str, f64, f64)> {
    if baseline == 0.0 {
        return None;
    }
    let chg = last_price - baseline;
    if chg == 0.0 {
        return None;
    }
    let pct = chg / baseline * 100.0;
    Some((label, chg, pct))
}

/// Fallback: read Yahoo's `meta` post/pre fields directly. Used when bars
/// aren't available (e.g., older cached quotes that pre-date the
/// timestamp+session-bounds plumbing). Less reliable: Yahoo nulls these
/// out during the regular session and in the overnight gap on some
/// symbols.
fn extended_hours_from_meta(q: &StockQuote) -> Option<(&'static str, f64, f64)> {
    let post = match (q.post_market_change, q.post_market_change_percent) {
        (Some(c), Some(p)) => Some((c, p)),
        _ => None,
    };
    let pre = match (q.pre_market_change, q.pre_market_change_percent) {
        (Some(c), Some(p)) => Some((c, p)),
        _ => None,
    };
    let prefer_pre = matches!(
        q.market_state.as_deref(),
        Some("PRE") | Some("PREPRE")
    );
    let (label, chg, pct) = if prefer_pre {
        let (c, p) = pre.or(post)?;
        if pre.is_some() {
            ("PRE", c, p)
        } else {
            ("AH", c, p)
        }
    } else {
        let (c, p) = post.or(pre)?;
        if post.is_some() {
            ("AH", c, p)
        } else {
            ("PRE", c, p)
        }
    };
    if chg == 0.0 && pct == 0.0 {
        return None;
    }
    Some((label, chg, pct))
}

fn period_change(q: &StockQuote, period: Period) -> (f64, f64) {
    match period {
        Period::Day => (q.change(), q.change_pct()),
        _ => {
            let baseline = q
                .intraday
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

/// Renders the `[1D] [1W] [1M] [6M] [YTD] [1Y]` selector. If the row isn't
/// wide enough to host all six, prepends/appends `‹` / `›` markers and shows
/// only a window of toggles centered on the active one.
fn render_period_toggle_bar(frame: &mut Frame, area: Rect, active: Period, theme: &Theme) {
    if area.width == 0 {
        return;
    }
    let active_idx = Period::ALL.iter().position(|p| *p == active).unwrap_or(0);

    // Each toggle is `[LBL] ` — variable because YTD is wider. Compute widths.
    let widths: Vec<u16> = Period::ALL
        .iter()
        .map(|p| (p.label().len() as u16) + 2 + 1) // [LBL]+space
        .collect();
    let total: u16 = widths.iter().sum::<u16>().saturating_sub(1); // last trailing space

    let mut spans: Vec<Span<'_>> = vec![Span::raw(" ")];
    if (total + 2) <= area.width {
        // Everything fits — render all toggles.
        for (i, p) in Period::ALL.iter().enumerate() {
            let style = if i == active_idx {
                Style::default()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                theme.text_dim
            };
            spans.push(Span::styled(format!("[{}]", p.label()), style));
            if i + 1 < Period::ALL.len() {
                spans.push(Span::raw(" "));
            }
        }
    } else {
        // Show a window centered on active. Compute how many fit.
        let budget = area.width.saturating_sub(4); // "‹ " + " ›" = 4
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
        }
        for i in start..end {
            let style = if i == active_idx {
                Style::default()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                dim
            };
            spans.push(Span::styled(format!("[{}]", Period::ALL[i].label()), style));
            if i + 1 < end {
                spans.push(Span::raw(" "));
            }
        }
        if end < Period::ALL.len() {
            spans.push(Span::raw(" "));
            spans.push(Span::styled("›", dim));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Row indices (0 = top, plot_h-1 = bottom) where we want y-axis labels.
/// Always includes the top + bottom (range high + low); adds the midpoint
/// when there's room, and quarter points when plot_h is large.
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

#[allow(clippy::too_many_arguments)] // 10 args, all distinct render inputs — no obvious bundle.
fn render_list_panel(
    frame: &mut Frame,
    area: Rect,
    symbols: &[String],
    indices_count: usize,
    lookup_start: Option<usize>,
    quotes: &HashMap<String, QuoteState>,
    selected: usize,
    mode: DisplayMode,
    period: Period,
    current_scroll: usize,
    theme: &Theme,
) -> usize {
    let (lines, ticker_lines) = build_list_lines(
        symbols,
        indices_count,
        lookup_start,
        quotes,
        selected,
        mode,
        period,
        theme,
    );

    // Reserve the bottom row for the footer hint rendered in `render`.
    let usable_h = area.height.saturating_sub(1) as usize;
    let list_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: usable_h as u16,
    };

    // Keep the selected ticker visible by adjusting scroll. The
    // cushion row hosts the last line when exactly one is hidden,
    // so when the user navigates to the very last ticker we don't
    // need to shift the viewport up — the cushion already shows it.
    // Without this carve-out, moving the cursor onto the final row
    // pushed everything else up by one, which is jarring when the
    // user could already see the row they're moving to.
    let sel_line = ticker_lines.get(selected).copied().unwrap_or(0);
    let mut scroll = current_scroll;
    if sel_line < scroll {
        scroll = sel_line;
    }
    if usable_h > 0 && sel_line >= scroll + usable_h {
        let is_last_line = sel_line + 1 == lines.len();
        scroll = if is_last_line {
            // Land sel_line on the cushion (one past usable_h) so the
            // visible window stays put.
            sel_line.saturating_sub(usable_h)
        } else {
            sel_line + 1 - usable_h
        };
    }
    // Cap scroll to the last useful starting line so we don't waste space.
    let max_scroll = lines.len().saturating_sub(usable_h.max(1));
    if scroll > max_scroll {
        scroll = max_scroll;
    }

    let total_lines = lines.len();
    let end = (scroll + usable_h).min(total_lines);
    let hidden_below = total_lines.saturating_sub(end);
    // Capture the would-be-next line BEFORE consuming `lines` below
    // — when exactly one row is hidden, the cushion row promotes
    // itself to show that row instead of a `↓` arrow. Saves the user
    // a scroll for an indicator that pointed at one item anyway.
    let cushion_line: Option<Line<'_>> = if hidden_below == 1 {
        lines.get(end).cloned()
    } else {
        None
    };
    let visible: Vec<Line<'_>> = lines.into_iter().skip(scroll).take(end - scroll).collect();
    frame.render_widget(Paragraph::new(visible), list_area);

    // Cushion row: show the last hidden line when only one is below
    // the viewport; show `↓` when two or more are; leave blank when
    // everything fits. The bottom row of `area` is the cushion we
    // reserved with `usable_h = area.height - 1`.
    if area.height > 0 {
        let cushion_rect = Rect {
            x: area.x,
            y: area.y + area.height - 1,
            width: area.width,
            height: 1,
        };
        if let Some(line) = cushion_line {
            frame.render_widget(Paragraph::new(line), cushion_rect);
        } else if hidden_below >= 2 {
            let arrow = Line::from(Span::styled("↓", theme.text_dim)).alignment(Alignment::Center);
            frame.render_widget(Paragraph::new(arrow), cushion_rect);
        }
    }
    scroll
}

/// Build the full set of lines for the list panel plus a `ticker_idx → line_idx`
/// map. Used by both render (for scrolling) and the click handler (for mapping
/// row clicks back to ticker indices when the list is scrolled).
fn build_list_lines<'a>(
    symbols: &'a [String],
    indices_count: usize,
    lookup_start: Option<usize>,
    quotes: &HashMap<String, QuoteState>,
    selected: usize,
    mode: DisplayMode,
    period: Period,
    theme: &Theme,
) -> (Vec<Line<'a>>, Vec<usize>) {
    let mut lines: Vec<Line<'a>> = Vec::with_capacity(symbols.len() + 4);
    let mut ticker_lines: Vec<usize> = Vec::with_capacity(symbols.len());
    if indices_count > 0 {
        lines.push(Line::from(Span::styled("── Indices ──", theme.text_dim)));
    }
    let mut watchlist_header_emitted = indices_count == 0;
    let mut lookup_header_emitted = false;
    for (i, sym) in symbols.iter().enumerate() {
        if !watchlist_header_emitted && i == indices_count {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("── Watchlist ──", theme.text_dim)));
            watchlist_header_emitted = true;
        }
        if let Some(start) = lookup_start {
            if !lookup_header_emitted && i == start {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "── Lookup (press x to clear) ──",
                    theme.text_dim,
                )));
                lookup_header_emitted = true;
            }
        }
        ticker_lines.push(lines.len());
        let is_selected = i == selected;
        lines.push(format_list_row(
            sym,
            quotes.get(sym),
            is_selected,
            mode,
            period,
        ));
    }
    (lines, ticker_lines)
}

fn format_list_row<'a>(
    symbol: &'a str,
    state: Option<&QuoteState>,
    selected: bool,
    mode: DisplayMode,
    period: Period,
) -> Line<'a> {
    let (price_str, change_str, color) = match state {
        Some(QuoteState::Ready(q)) => {
            let (chg_abs, chg_pct) = period_change(q, period);
            let color = if chg_abs >= 0.0 {
                Color::Green
            } else {
                Color::Red
            };
            let glyph = if chg_abs >= 0.0 { '▲' } else { '▼' };
            let price_str = format!("{:>10.2}", q.price);
            let change_str = match mode {
                DisplayMode::Percent => format!("{glyph} {:+.2}%", chg_pct),
                DisplayMode::Dollar => format!("{glyph} {:+.2}", chg_abs),
            };
            (price_str, change_str, color)
        }
        Some(QuoteState::Inflight) | None => ("       …".to_string(), "    …".into(), Color::Gray),
        Some(QuoteState::Failed) => ("     err".to_string(), "  err".into(), Color::DarkGray),
    };
    let prefix = if selected { "▸ " } else { "  " };
    let sym_style = if selected {
        Style::default()
            .fg(Color::LightYellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    // Symbol column needs 7 cols to fit "^GSPC" (5) + a space, or longer
    // watchlist symbols with up to 6 chars.
    Line::from(vec![
        Span::styled(prefix, sym_style),
        Span::styled(format!("{:<7}", symbol), sym_style),
        Span::styled(price_str, Style::default()),
        Span::raw("  "),
        Span::styled(format!("{:>10}", change_str), Style::default().fg(color)),
    ])
}

fn render_stats_panel(
    frame: &mut Frame,
    area: Rect,
    selected: Option<&str>,
    quotes: &HashMap<String, QuoteState>,
    theme: &Theme,
) {
    let q = match selected.and_then(|s| quotes.get(s)) {
        Some(QuoteState::Ready(q)) => q.as_ref(),
        _ => {
            let para = Paragraph::new(Span::styled("(no stats)", theme.text_dim))
                .alignment(Alignment::Center);
            frame.render_widget(para, area);
            return;
        }
    };

    let mut lines: Vec<Line<'_>> = Vec::new();
    lines.push(Line::from(Span::styled(
        q.short_name.clone(),
        theme.text_focused,
    )));
    lines.push(Line::from(""));
    lines.push(stat_line("Price", &format!("{:.2}", q.price), theme));
    lines.push(stat_line(
        "Prev Close",
        &format!("{:.2}", q.previous_close),
        theme,
    ));
    if let (Some(h), Some(l)) = (q.day_high, q.day_low) {
        lines.push(stat_line("Day H/L", &format!("{h:.2} / {l:.2}"), theme));
    }
    if let (Some(h), Some(l)) = (q.fifty_two_week_high, q.fifty_two_week_low) {
        lines.push(stat_line("52w H/L", &format!("{h:.2} / {l:.2}"), theme));
    }
    if let Some(v) = q.volume {
        lines.push(stat_line("Volume", &humanize_big(v as f64), theme));
    }
    if let Some(v) = q.avg_volume {
        lines.push(stat_line("Avg Vol", &humanize_big(v as f64), theme));
    }
    lines.push(stat_line(
        "Mkt Cap",
        &q.market_cap
            .map(|v| humanize_big(v as f64))
            .unwrap_or_else(|| "—".into()),
        theme,
    ));
    lines.push(stat_line(
        "Shares",
        &q.shares_outstanding
            .map(|v| humanize_big(v as f64))
            .unwrap_or_else(|| "—".into()),
        theme,
    ));
    if let Some(pe) = q.pe_ratio {
        lines.push(stat_line("P/E", &format!("{pe:.2}"), theme));
    }
    if let Some(eps) = q.eps {
        lines.push(stat_line("EPS", &format!("{eps:.2}"), theme));
    }
    if let Some(y) = q.dividend_yield {
        lines.push(stat_line("Yield", &format!("{:.2}%", y * 100.0), theme));
    }
    if let Some(b) = q.beta {
        lines.push(stat_line("Beta", &format!("{b:.2}"), theme));
    }

    // Market-hours countdown at the bottom of the ticker profile. The
    // emphasis flips on state so a glance tells you whether the market is
    // currently live or quiet: `text.focused` when open (counting down to
    // close), `text.dim` when closed (counting down to next open).
    let status = market_status_line(Utc::now());
    let style = if status.is_open {
        theme.text_focused
    } else {
        theme.text_dim
    };
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(status.message, style)));

    frame.render_widget(Paragraph::new(lines), area);
}

/// Snapshot of US equity-market hours: whether the regular session is
/// currently live, and a `XhYm until …` human label describing what's
/// next.
struct MarketStatus {
    is_open: bool,
    message: String,
}

/// NYSE/Nasdaq regular session is 09:30–16:00 America/New_York, Mon–Fri.
/// Half-days (Black Friday, certain Christmas Eves) close at 13:00 ET.
const MARKET_TZ: &str = "America/New_York";
const MARKET_OPEN_HOUR: u32 = 9;
const MARKET_OPEN_MINUTE: u32 = 30;
const MARKET_CLOSE_HOUR: u32 = 16;
const MARKET_EARLY_CLOSE_HOUR: u32 = 13;
/// Pre-market opens 04:00 ET and post-market closes 20:00 ET. Used by the
/// poll gate so we don't pound Yahoo overnight or on weekends.
const EXTENDED_OPEN_HOUR: u32 = 4;
const EXTENDED_CLOSE_HOUR: u32 = 20;

/// True while `now_utc` sits inside the extended-hours window
/// (pre-market + regular + post-market) on a trading day. False on
/// weekends, holidays, and overnight (20:00–04:00 ET). Used as the
/// quotes-poll gate so the widget doesn't burn rate-limit budget when
/// Yahoo's quote can't change.
fn is_extended_market_hours(now_utc: chrono::DateTime<Utc>) -> bool {
    let Ok(tz) = MARKET_TZ.parse::<Tz>() else {
        // If the tz lookup ever fails, default to "always poll" rather
        // than silently strand the widget.
        return true;
    };
    let now = now_utc.with_timezone(&tz);
    let date = now.date_naive();
    if matches!(date.weekday(), Weekday::Sat | Weekday::Sun) {
        return false;
    }
    if is_market_holiday(date) {
        return false;
    }
    let hour = now.hour();
    hour >= EXTENDED_OPEN_HOUR && hour < EXTENDED_CLOSE_HOUR
}

fn market_status_line(now_utc: chrono::DateTime<Utc>) -> MarketStatus {
    let Ok(tz) = MARKET_TZ.parse::<Tz>() else {
        return MarketStatus {
            is_open: false,
            message: "Market schedule unavailable".into(),
        };
    };
    let now = now_utc.with_timezone(&tz);

    // `session(date)` returns `None` when the market doesn't trade that
    // day (weekend or full-closure holiday). On half-days the close hour
    // is bumped down to 13:00 ET.
    let session = |date: NaiveDate| -> Option<(chrono::DateTime<Tz>, chrono::DateTime<Tz>)> {
        if matches!(date.weekday(), Weekday::Sat | Weekday::Sun) {
            return None;
        }
        if is_market_holiday(date) {
            return None;
        }
        let close_hour = if is_market_half_day(date) {
            MARKET_EARLY_CLOSE_HOUR
        } else {
            MARKET_CLOSE_HOUR
        };
        let open_naive = date.and_hms_opt(MARKET_OPEN_HOUR, MARKET_OPEN_MINUTE, 0)?;
        let close_naive = date.and_hms_opt(close_hour, 0, 0)?;
        let open = tz.from_local_datetime(&open_naive).single()?;
        let close = tz.from_local_datetime(&close_naive).single()?;
        Some((open, close))
    };

    if let Some((open, close)) = session(now.date_naive()) {
        if now >= open && now < close {
            return MarketStatus {
                is_open: true,
                message: format!("{} until market close", format_hm(close - now)),
            };
        }
    }

    // Find the next session open. Today (if pre-open and a trading day),
    // tomorrow, or whichever non-weekend, non-holiday day comes next.
    // Cap at 14 iterations — that's enough to cross a Thanksgiving long
    // weekend, the year-end stretch (Christmas + Boxing weekend + New
    // Year's), or any plausible holiday cluster.
    let mut date = now.date_naive();
    for _ in 0..14 {
        if let Some((open, _close)) = session(date) {
            if open > now {
                return MarketStatus {
                    is_open: false,
                    message: format!("{} until market open", format_hm(open - now)),
                };
            }
        }
        date = date + ChronoDuration::days(1);
    }
    MarketStatus {
        is_open: false,
        message: "Market schedule unavailable".into(),
    }
}

/// NYSE full-closure holidays. Ten official holidays per year, observed on
/// Friday when they fall on a Saturday and on Monday when they fall on a
/// Sunday — except for MLK Day, Presidents Day, Memorial Day, Labor Day,
/// and Thanksgiving (which are floating "nth weekday of month" dates and
/// never need observation shifts) and Good Friday (Friday by definition).
fn is_market_holiday(date: NaiveDate) -> bool {
    let y = date.year();
    let m = date.month();
    let d = date.day();

    // Fixed-date holidays, observed Friday/Monday if weekend.
    let fixed = [
        (1, 1),   // New Year's Day
        (6, 19),  // Juneteenth
        (7, 4),   // Independence Day
        (12, 25), // Christmas
    ];
    for (fm, fd) in fixed {
        if let Some(actual) = NaiveDate::from_ymd_opt(y, fm, fd) {
            if observed(actual) == date {
                return true;
            }
        }
    }

    // Floating holidays — fixed weekday rules, no observation shift.
    if m == 1
        && date == nth_weekday(y, 1, Weekday::Mon, 3).unwrap_or(date.succ_opt().unwrap_or(date))
    {
        return true; // MLK Day (3rd Mon of Jan)
    }
    if m == 2
        && date == nth_weekday(y, 2, Weekday::Mon, 3).unwrap_or(date.succ_opt().unwrap_or(date))
    {
        return true; // Presidents Day (3rd Mon of Feb)
    }
    if m == 5 && date == last_weekday(y, 5, Weekday::Mon).unwrap_or(date.succ_opt().unwrap_or(date))
    {
        return true; // Memorial Day (last Mon of May)
    }
    if m == 9
        && date == nth_weekday(y, 9, Weekday::Mon, 1).unwrap_or(date.succ_opt().unwrap_or(date))
    {
        return true; // Labor Day (1st Mon of Sep)
    }
    if m == 11
        && date == nth_weekday(y, 11, Weekday::Thu, 4).unwrap_or(date.succ_opt().unwrap_or(date))
    {
        return true; // Thanksgiving (4th Thu of Nov)
    }
    if let Some(gf) = good_friday(y) {
        if date == gf {
            return true;
        }
    }
    // Special case: when Dec 25 falls on Saturday, NYSE moves the
    // observance to Friday Dec 24 *as a full closure* — not a half-day.
    // The fixed-date check above already covers Dec 25 → observed Fri;
    // nothing more to do here.
    // Independence Day already handled via observed() above.
    let _ = d;
    false
}

/// Half-day closures: market closes early at 13:00 ET.
/// - Day after Thanksgiving (always Friday).
/// - Christmas Eve when Dec 24 is Mon/Tue/Wed/Thu (Dec 25 falls on
///   Tue/Wed/Thu/Fri respectively). When Dec 24 is Fri (Dec 25 = Sat),
///   it's the Christmas observed-closure day, not a half-day, and
///   `is_market_holiday` catches it.
fn is_market_half_day(date: NaiveDate) -> bool {
    if date.month() == 11 {
        if let Some(thx) = nth_weekday(date.year(), 11, Weekday::Thu, 4) {
            if let Some(black_friday) = thx.succ_opt() {
                if date == black_friday {
                    return true;
                }
            }
        }
    }
    if date.month() == 12 && date.day() == 24 {
        return matches!(
            date.weekday(),
            Weekday::Mon | Weekday::Tue | Weekday::Wed | Weekday::Thu
        );
    }
    false
}

/// Observation-shift rule: holiday on Saturday → observed Friday; holiday
/// on Sunday → observed Monday. Weekdays pass through unchanged.
fn observed(date: NaiveDate) -> NaiveDate {
    match date.weekday() {
        Weekday::Sat => date - ChronoDuration::days(1),
        Weekday::Sun => date + ChronoDuration::days(1),
        _ => date,
    }
}

/// `n`th occurrence of `weekday` in (`year`, `month`). e.g.
/// `nth_weekday(2026, 11, Weekday::Thu, 4)` = 4th Thursday of Nov 2026.
fn nth_weekday(year: i32, month: u32, weekday: Weekday, n: u32) -> Option<NaiveDate> {
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    let first_dow = first.weekday().num_days_from_monday();
    let target = weekday.num_days_from_monday();
    let delta = (target + 7 - first_dow) % 7;
    let day = 1 + delta + 7 * (n - 1);
    NaiveDate::from_ymd_opt(year, month, day)
}

/// Last occurrence of `weekday` in (`year`, `month`) — used for "last
/// Monday of May" (Memorial Day).
fn last_weekday(year: i32, month: u32, weekday: Weekday) -> Option<NaiveDate> {
    let first_next = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)
    }?;
    let last = first_next - ChronoDuration::days(1);
    let last_dow = last.weekday().num_days_from_monday();
    let target = weekday.num_days_from_monday();
    let delta = (last_dow + 7 - target) % 7;
    Some(last - ChronoDuration::days(delta as i64))
}

/// Western (Gregorian) Easter via the Meeus/Jones/Butcher algorithm. Good
/// Friday = Easter - 2 days.
fn good_friday(year: i32) -> Option<NaiveDate> {
    let a = year % 19;
    let b = year / 100;
    let c = year % 100;
    let d = b / 4;
    let e = b % 4;
    let f = (b + 8) / 25;
    let g = (b - f + 1) / 3;
    let h = (19 * a + b - d - g + 15) % 30;
    let i = c / 4;
    let k = c % 4;
    let l = (32 + 2 * e + 2 * i - h - k) % 7;
    let mo = (a + 11 * h + 22 * l) / 451;
    let month = ((h + l - 7 * mo + 114) / 31) as u32;
    let day = (((h + l - 7 * mo + 114) % 31) + 1) as u32;
    let easter = NaiveDate::from_ymd_opt(year, month, day)?;
    Some(easter - ChronoDuration::days(2))
}

fn format_hm(d: ChronoDuration) -> String {
    let total = d.num_seconds().max(0);
    let h = total / 3600;
    let m = (total % 3600) / 60;
    if h == 0 {
        format!("{m}m")
    } else {
        format!("{h}h{m}m")
    }
}

fn stat_line(label: &str, value: &str, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{:<10}", label), theme.text_dim),
        Span::styled(value.to_string(), theme.text_plain),
    ])
}

/// Format a large count compactly: `48.2M`, `3.02T`, `1.23B`, `15.4K`.
fn humanize_big(v: f64) -> String {
    let abs = v.abs();
    if abs >= 1e12 {
        format!("{:.2}T", v / 1e12)
    } else if abs >= 1e9 {
        format!("{:.2}B", v / 1e9)
    } else if abs >= 1e6 {
        format!("{:.2}M", v / 1e6)
    } else if abs >= 1e3 {
        format!("{:.1}K", v / 1e3)
    } else {
        format!("{v:.0}")
    }
}

/// Inert provider used when the real one fails to construct (e.g., reqwest
/// builder failure). Lets the widget still render gracefully.
fn provider_dummy() -> YahooFinanceProvider {
    // YahooFinanceProvider::new() builds a reqwest::Client; if that fails the
    // caller has already logged. We unwrap here as the failure path is
    // essentially impossible (default reqwest config).
    YahooFinanceProvider::new().expect("dummy yahoo provider should build")
}

pub const KIND: &str = "stocks";

/// Wizard descriptor for the stocks widget. All fields are flat — the
/// default field-by-field TOML renderer handles emission, so no custom
/// `render_toml` is needed.
pub fn wizard_descriptor() -> crate::wizard::descriptor::WizardDescriptor {
    use crate::wizard::descriptor::{
        ChoiceOption, Separator, WizardDescriptor, WizardField, WizardFieldKind,
    };
    WizardDescriptor {
        display_name: "Stocks",
        blurb: "Watchlist quotes + intraday and historical graphs via Yahoo \
                Finance. Index tickers stay separate from the user-defined \
                watchlist so the header row pinning works correctly.",
        load_from_toml: None,
        render_toml: None,
        fields: vec![
            WizardField {
                key: "indices",
                label: "Index tickers (comma-separated)",
                help: "Yahoo conventions: ^DJI (Dow), ^GSPC (S&P 500), \
                       ^IXIC (Nasdaq Composite). Indices render in a \
                       pinned header row above the user watchlist.",
                required: false,
                kind: WizardFieldKind::TextList {
                    default: vec!["^DJI".into(), "^GSPC".into(), "^IXIC".into()],
                    separator: Separator::Comma,
                },
                validate: None,
            },
            WizardField {
                key: "watchlist",
                label: "Watchlist tickers (comma-separated)",
                help: "Free-form watchlist. Use standard exchange suffixes \
                       for non-US markets (e.g. SHOP.TO for Toronto).",
                required: false,
                kind: WizardFieldKind::TextList {
                    // Keep this in sync with [`default_watchlist`]: MAG7 +
                    // NFLX + a handful of blue chips. The wizard's defaults
                    // double as the on-disk defaults when the user accepts
                    // the form without editing.
                    default: vec![
                        // MAG7
                        "AAPL".into(),
                        "MSFT".into(),
                        "GOOGL".into(),
                        "AMZN".into(),
                        "META".into(),
                        "NVDA".into(),
                        "TSLA".into(),
                        // FAANG round-out
                        "NFLX".into(),
                        // Blue chips
                        "BRK-B".into(),
                        "JPM".into(),
                        "JNJ".into(),
                        "V".into(),
                        "WMT".into(),
                    ],
                    separator: Separator::Comma,
                },
                validate: None,
            },
            WizardField {
                key: "poll_interval_secs",
                label: "Background refresh interval (seconds)",
                help: "Quote-poll cadence when the widget is *not* the \
                       focused pane. 300s (5min) keeps Yahoo quota use \
                       low on a multi-widget dashboard. The widget \
                       speeds up to `focused_poll_interval_secs` while \
                       it has focus.",
                required: true,
                kind: WizardFieldKind::Number {
                    default: Some(300.0),
                    range: Some((15.0, 3600.0)),
                    integer: true,
                },
                validate: None,
            },
            WizardField {
                key: "focused_poll_interval_secs",
                label: "Focused refresh interval (seconds)",
                help: "Cadence used while the widget is the active stack \
                       child and holds keyboard focus. Defaults to 60s — \
                       Yahoo's chart endpoint refreshes about once a \
                       minute, so going lower won't yield fresher data.",
                required: true,
                kind: WizardFieldKind::Number {
                    default: Some(60.0),
                    range: Some((15.0, 3600.0)),
                    integer: true,
                },
                validate: None,
            },
            WizardField {
                key: "default_display_mode",
                label: "Change-column display",
                help: "How the rightmost column renders today's move: \
                       \"percent\" — ±N%; \"dollar\" — ±$N; \"change\" — \
                       absolute price change. Press `c` in the widget to \
                       cycle at runtime.",
                required: true,
                kind: WizardFieldKind::Choice {
                    options: vec![
                        ChoiceOption {
                            value: "percent",
                            label: "Percent (±N%)",
                            help: None,
                        },
                        ChoiceOption {
                            value: "dollar",
                            label: "Dollar (±$N)",
                            help: None,
                        },
                        ChoiceOption {
                            value: "change",
                            label: "Absolute change",
                            help: None,
                        },
                    ],
                    default: Some("percent"),
                },
                validate: None,
            },
            WizardField {
                key: "default_period",
                label: "Initial graph period",
                help: "Time window the graph opens with. Press `1`-`9` or \
                       `←`/`→` in the widget to cycle at runtime.",
                required: true,
                kind: WizardFieldKind::Choice {
                    options: vec![
                        ChoiceOption {
                            value: "1d",
                            label: "1 day (intraday 5m bars)",
                            help: None,
                        },
                        ChoiceOption {
                            value: "1w",
                            label: "1 week (30m bars)",
                            help: None,
                        },
                        ChoiceOption {
                            value: "1m",
                            label: "1 month (daily)",
                            help: None,
                        },
                        ChoiceOption {
                            value: "6m",
                            label: "6 months (daily)",
                            help: None,
                        },
                        ChoiceOption {
                            value: "ytd",
                            label: "Year to date",
                            help: None,
                        },
                        ChoiceOption {
                            value: "1y",
                            label: "1 year",
                            help: None,
                        },
                    ],
                    default: Some("1d"),
                },
                validate: None,
            },
        ],
    }
}

pub fn build(ctx: &super::WidgetCtx) -> Box<dyn super::Widget> {
    let cfg: StocksConfig =
        crate::config::load_widget_toml_for_instance(KIND, &ctx.instance).unwrap_or_default();
    Box::new(StocksWidget::with_config(
        ctx.instance.clone(),
        cfg,
        ctx.theme.clone(),
        ctx.cache.clone(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_widget(cfg: StocksConfig) -> StocksWidget {
        StocksWidget::with_config(
            "main".to_string(),
            cfg,
            Arc::new(Theme::builtin_defaults()),
            ScopedCache::ephemeral(),
        )
    }

    fn quote(symbol: &str, price: f64, prev: f64) -> StockQuote {
        StockQuote {
            symbol: symbol.into(),
            short_name: symbol.into(),
            price,
            previous_close: prev,
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
        }
    }

    #[test]
    fn quotes_buckets_are_isolated_per_period() {
        // Per-period buckets must keep their data when switching back
        // and forth. If a write to one bucket leaks into another the
        // user sees the wrong period's bars under the new x-axis
        // labels (the bug that motivated the per-period split).
        let mut st = StocksState::default();
        let day_q = quote("AAPL", 200.0, 199.0);
        st.quotes_mut(Period::Day)
            .insert("AAPL".into(), QuoteState::Ready(Arc::new(day_q.clone())));
        assert_eq!(st.quotes(Period::Day).len(), 1);
        assert_eq!(
            st.quotes(Period::Week).len(),
            0,
            "Week bucket should not see Day's data"
        );

        let week_q = quote("AAPL", 210.0, 200.0);
        st.quotes_mut(Period::Week)
            .insert("AAPL".into(), QuoteState::Ready(Arc::new(week_q.clone())));
        assert_eq!(st.quotes(Period::Day).len(), 1);
        assert_eq!(st.quotes(Period::Week).len(), 1);

        // The Day quote remains intact after the Week insert (no
        // cross-contamination).
        match st.quotes(Period::Day).get("AAPL") {
            Some(QuoteState::Ready(q)) => {
                assert_eq!(q.price, 200.0, "Day price should still be 200.0");
            }
            _ => panic!("Day bucket should still hold a Ready entry"),
        }
        match st.quotes(Period::Week).get("AAPL") {
            Some(QuoteState::Ready(q)) => {
                assert_eq!(q.price, 210.0);
            }
            _ => panic!("Week bucket should hold a Ready entry"),
        }
    }

    #[test]
    fn humanize_big_uses_expected_suffixes() {
        assert_eq!(humanize_big(500.0), "500");
        assert_eq!(humanize_big(15_400.0), "15.4K");
        assert_eq!(humanize_big(48_200_000.0), "48.20M");
        assert_eq!(humanize_big(3_020_000_000_000.0), "3.02T");
    }

    #[test]
    fn all_symbols_orders_indices_first_then_watchlist() {
        let w = build_widget(StocksConfig::default());
        let syms = w.all_symbols();
        assert_eq!(syms[0], "^DJI");
        assert_eq!(syms[3], "AAPL");
    }

    /// `:stock AAPL` against a watchlist that already contains AAPL
    /// used to set `transient_ticker = Some("AAPL")` + `selected =
    /// base_slot`, but `all_symbols()` dedupes the transient against
    /// the watchlist — so selection landed past the visible list and
    /// the graph stayed blank. Snap to the existing row instead.
    #[test]
    fn lookup_existing_uppercase_ticker_jumps_to_watchlist_row() {
        let w = build_widget(StocksConfig::default());
        // AAPL sits at index 3 (3 indices + AAPL at watchlist[0]).
        w.lookup_and_set_transient("AAPL");
        let st = w.state.lock().unwrap();
        assert_eq!(st.selected, 3);
        assert!(st.transient_ticker.is_none(), "no transient pin needed");
    }

    /// Same fast path, lower-cased. The case-insensitive match
    /// catches `:stock aapl` typed by muscle memory.
    #[test]
    fn lookup_existing_lowercase_ticker_jumps_to_watchlist_row() {
        let w = build_widget(StocksConfig::default());
        w.lookup_and_set_transient("aapl");
        let st = w.state.lock().unwrap();
        assert_eq!(st.selected, 3);
        assert!(st.transient_ticker.is_none());
    }

    /// Indices count too: `:stock ^GSPC` should land on the S&P row,
    /// not pin a duplicate transient. ^GSPC is indices[1].
    #[test]
    fn lookup_existing_index_symbol_jumps_to_index_row() {
        let w = build_widget(StocksConfig::default());
        w.lookup_and_set_transient("^GSPC");
        let st = w.state.lock().unwrap();
        assert_eq!(st.selected, 1);
        assert!(st.transient_ticker.is_none());
    }

    /// Already-pinned transient gets snapped to instead of re-pinned.
    /// User pins SPY (transient row appears at the end), then types
    /// `:stock spy` again — selection should land on the existing
    /// transient row.
    #[test]
    fn lookup_existing_transient_jumps_to_transient_row() {
        let w = build_widget(StocksConfig::default());
        w.lookup_and_set_transient("SPY");
        let base_slot = w.config.indices.len() + w.config.watchlist.len();
        {
            let st = w.state.lock().unwrap();
            assert_eq!(st.selected, base_slot);
            assert_eq!(st.transient_ticker.as_deref(), Some("SPY"));
        }
        // Second invocation — same symbol, different case. Should
        // snap to the existing transient row, not re-pin it.
        w.lookup_and_set_transient("spy");
        let st = w.state.lock().unwrap();
        assert_eq!(st.selected, base_slot);
        assert_eq!(
            st.transient_ticker.as_deref(),
            Some("SPY"),
            "transient should still be SPY, not re-pinned"
        );
    }

    /// New ticker that isn't on screen → pin it as transient at
    /// `base_slot`, same as the original behavior. Guards against the
    /// fast-path swallowing legitimate new-ticker lookups.
    #[test]
    fn lookup_new_ticker_pins_transient_at_base_slot() {
        let w = build_widget(StocksConfig::default());
        let base_slot = w.config.indices.len() + w.config.watchlist.len();
        w.lookup_and_set_transient("BRK-A");
        let st = w.state.lock().unwrap();
        assert_eq!(st.selected, base_slot);
        assert_eq!(st.transient_ticker.as_deref(), Some("BRK-A"));
    }

    #[test]
    fn cycle_period_wraps_at_both_ends() {
        let mut w = build_widget(StocksConfig::default());
        assert_eq!(w.period, Period::Day);
        w.cycle_period(true);
        assert_eq!(w.period, Period::Week);
        // Walk forward to the last variant, then once more to wrap to first.
        for _ in 0..Period::ALL.len() - 2 {
            w.cycle_period(true);
        }
        assert_eq!(w.period, Period::TenYear);
        w.cycle_period(true);
        assert_eq!(w.period, Period::Day);
        // Backward wraps too.
        w.cycle_period(false);
        assert_eq!(w.period, Period::TenYear);
    }

    #[test]
    fn move_selection_clamps() {
        let mut w = build_widget(StocksConfig::default());
        w.move_selection(-5);
        assert_eq!(w.state.lock().unwrap().selected, 0);
        w.move_selection(100);
        let total = w.all_symbols().len() - 1;
        assert_eq!(w.state.lock().unwrap().selected, total);
    }

    #[test]
    fn label_rows_spans_full_plot_height() {
        // Tall plot → 5 labels at top/¼/mid/¾/bottom.
        let rows = label_rows(8);
        assert_eq!(rows.first(), Some(&0));
        assert_eq!(rows.last(), Some(&7));
        assert!(rows.len() >= 3);
        // Medium plot → 3 labels.
        let rows = label_rows(5);
        assert_eq!(rows.first(), Some(&0));
        assert_eq!(rows.last(), Some(&4));
        // Short plot → top + bottom only.
        let rows = label_rows(3);
        assert_eq!(rows, vec![0, 2]);
        // Single-row plot.
        assert_eq!(label_rows(1), vec![0]);
        assert!(label_rows(0).is_empty());
    }

    #[test]
    fn list_row_includes_arrow_for_selected_only() {
        let q = quote("AAPL", 200.0, 196.0);
        let qs: HashMap<String, QuoteState> = {
            let mut m = HashMap::new();
            m.insert("AAPL".to_string(), QuoteState::Ready(Arc::new(q)));
            m
        };
        let line_sel = format_list_row(
            "AAPL",
            qs.get("AAPL"),
            true,
            DisplayMode::Percent,
            Period::Day,
        );
        let line_un = format_list_row(
            "AAPL",
            qs.get("AAPL"),
            false,
            DisplayMode::Percent,
            Period::Day,
        );
        let sel_text: String = line_sel.spans.iter().map(|s| s.content.as_ref()).collect();
        let un_text: String = line_un.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(sel_text.contains("▸"));
        assert!(!un_text.contains("▸"));
    }

    /// Construct a `DateTime<Utc>` for a given America/New_York local
    /// timestamp. Centralized so each market-hours test reads cleanly.
    fn et_to_utc(y: i32, m: u32, d: u32, hour: u32, minute: u32) -> chrono::DateTime<Utc> {
        let tz: Tz = MARKET_TZ.parse().unwrap();
        let local = tz
            .with_ymd_and_hms(y, m, d, hour, minute, 0)
            .single()
            .expect("valid local time");
        local.with_timezone(&Utc)
    }

    #[test]
    fn extended_market_hours_covers_premarket_through_postmarket() {
        // 04:00 ET Monday → in window.
        assert!(is_extended_market_hours(et_to_utc(2026, 5, 18, 4, 0)));
        // 09:30 ET (regular open) → in window.
        assert!(is_extended_market_hours(et_to_utc(2026, 5, 18, 9, 30)));
        // 19:59 ET → still in window (post-market closes at 20:00).
        assert!(is_extended_market_hours(et_to_utc(2026, 5, 18, 19, 59)));
        // 20:00 ET → out of window.
        assert!(!is_extended_market_hours(et_to_utc(2026, 5, 18, 20, 0)));
        // 03:00 ET → before pre-market open.
        assert!(!is_extended_market_hours(et_to_utc(2026, 5, 18, 3, 0)));
        // Saturday → never in window even during pre/post hours.
        assert!(!is_extended_market_hours(et_to_utc(2026, 5, 16, 10, 0)));
    }

    #[test]
    fn extended_hours_segment_meta_fallback_prefers_post_after_close() {
        // No bars/timestamps populated → falls back to Yahoo's meta fields.
        let mut q = quote("AAPL", 200.0, 196.0);
        q.market_state = Some("POST".into());
        q.post_market_change = Some(0.42);
        q.post_market_change_percent = Some(0.21);
        let seg = extended_hours_segment(&q).expect("AH segment present");
        assert_eq!(seg.0, "AH");
        assert!((seg.1 - 0.42).abs() < 1e-9);
    }

    #[test]
    fn extended_hours_segment_meta_fallback_uses_pre_in_morning() {
        let mut q = quote("AAPL", 200.0, 196.0);
        q.market_state = Some("PRE".into());
        q.pre_market_change = Some(-0.15);
        q.pre_market_change_percent = Some(-0.08);
        let seg = extended_hours_segment(&q).expect("PRE segment present");
        assert_eq!(seg.0, "PRE");
        assert!(seg.1 < 0.0);
    }

    #[test]
    fn extended_hours_segment_hidden_when_change_is_zero() {
        let mut q = quote("AAPL", 200.0, 196.0);
        q.market_state = Some("POST".into());
        q.post_market_change = Some(0.0);
        q.post_market_change_percent = Some(0.0);
        assert!(extended_hours_segment(&q).is_none());
    }

    #[test]
    fn extended_hours_from_bars_shows_ah_when_latest_bar_is_post_market() {
        // 4:10pm ET scenario: regular session just closed at 196.0
        // (q.price = official close auction), latest AH bar is at 198.0.
        // Expect AH = +2.00 (+1.02%) against the official close, not
        // against any boundary bar's close.
        let mut q = quote("AAPL", 196.0, 195.0); // q.price = today's close, prev = yesterday
        q.regular_session_start_ts = Some(1_700_000_000); // anchor
        q.regular_session_end_ts = Some(1_700_023_400); // anchor + 6.5h
        q.intraday_timestamps = vec![
            1_700_023_100, // 15:55 — last regular bar
            1_700_023_400, // 16:00 — boundary (first AH bar in Yahoo's convention)
            1_700_023_700, // 16:05 AH
            1_700_024_000, // 16:10 AH
        ];
        // Boundary bar's close differs from official close — that's the
        // whole point. We use q.price, not the boundary bar.
        q.intraday = vec![195.93, 196.07, 197.0, 198.0];
        let seg = extended_hours_segment(&q).expect("AH from bars");
        assert_eq!(seg.0, "AH");
        assert!((seg.1 - 2.0).abs() < 1e-9, "got {}", seg.1);
        assert!((seg.2 - (2.0 / 196.0 * 100.0)).abs() < 1e-6);
    }

    #[test]
    fn extended_hours_from_bars_persists_after_post_market_closes() {
        // 21:22 ET scenario: post-market closed at 20:00 ET, no new bars
        // are coming. Last bar is past reg_end → still AH against
        // today's close.
        let mut q = quote("AAPL", 196.0, 195.0);
        q.regular_session_start_ts = Some(1_700_000_000);
        q.regular_session_end_ts = Some(1_700_023_400);
        q.intraday_timestamps = vec![1_700_023_400, 1_700_037_800];
        q.intraday = vec![196.07, 198.0];
        let seg = extended_hours_segment(&q).expect("AH persists overnight");
        assert_eq!(seg.0, "AH");
        assert!((seg.1 - 2.0).abs() < 1e-9);
    }

    #[test]
    fn extended_hours_from_bars_shows_pre_when_latest_bar_is_premarket() {
        // Next morning: pre-market bars exist, no regular bars yet.
        // q.price still reflects yesterday's close (Yahoo updates it
        // at the close auction and holds until next open).
        let mut q = quote("AAPL", 196.0, 195.0); // q.price = yesterday's close
        q.regular_session_start_ts = Some(1_700_100_000);
        q.regular_session_end_ts = Some(1_700_123_400);
        // Bars at ~06:00 ET (3.5h before today's open — well within
        // PRE_MARKET_LOOKBACK_SECS of 7h).
        q.intraday_timestamps = vec![1_700_086_000, 1_700_087_800];
        q.intraday = vec![198.0, 199.0];
        let seg = extended_hours_segment(&q).expect("PRE from bars");
        assert_eq!(seg.0, "PRE");
        // chg = 199 (last PRE bar) - 196 (yesterday's close) = +3.
        assert!((seg.1 - 3.0).abs() < 1e-9, "got {}", seg.1);
    }

    #[test]
    fn extended_hours_from_bars_shows_ah_in_overnight_gap() {
        // The 2am-ET case. Today's regular session hasn't started; the
        // latest bar is yesterday's last AH bar from ~7:30pm. The old
        // logic mis-labeled this PRE and computed chg against Yahoo's
        // `chartPreviousClose` (= the day-before-yesterday's close).
        // Now it labels AH against q.price (= yesterday's official
        // close auction).
        let mut q = quote("AAPL", 196.0, 100.0); // previous_close junk to prove
                                                  // we don't depend on it.
        // Today's reg session: anchor (~09:30 ET) to anchor + 6.5h.
        q.regular_session_start_ts = Some(1_700_100_000);
        q.regular_session_end_ts = Some(1_700_123_400);
        // Yesterday's last AH bar is well before today's reg_start —
        // ~16h before, outside the 7h PRE_MARKET_LOOKBACK window.
        q.intraday_timestamps = vec![
            1_700_037_000, // yesterday boundary bar
            1_700_040_000, // yesterday AH
            1_700_044_000, // yesterday last AH (~7:30 ET)
        ];
        q.intraday = vec![196.07, 197.0, 198.0];
        let seg = extended_hours_segment(&q).expect("AH from overnight gap");
        assert_eq!(seg.0, "AH");
        // chg = 198 - 196 (q.price).
        assert!((seg.1 - 2.0).abs() < 1e-9, "got {}", seg.1);
        assert!((seg.2 - (2.0 / 196.0 * 100.0)).abs() < 1e-6);
    }

    #[test]
    fn extended_hours_ignores_boundary_bar_for_baseline() {
        // Regression test for the bug the user found by cross-checking
        // against MarketWatch: Yahoo's bar at ts == reg_end is the
        // FIRST AH bar, not the regular-close bar, and its close
        // differs from the official close auction price by a few
        // cents. We must use q.price (the auction price), not any
        // bar's close, as the AH baseline. With real AAPL boundary
        // data: close auction 310.85, boundary bar 310.92, last AH
        // 310.60. MW shows -$0.25 (= 310.60 - 310.85); the old logic
        // computed -$0.32 against the boundary bar.
        let mut q = quote("AAPL", 310.85, 308.33);
        q.regular_session_start_ts = Some(1_700_000_000);
        q.regular_session_end_ts = Some(1_700_023_400);
        q.intraday_timestamps = vec![
            1_700_023_100, // 15:55 last regular bar
            1_700_023_400, // 16:00 boundary (close = 310.92 ≠ 310.85 auction)
            1_700_024_300, // 16:15 AH
            1_700_037_800, // last AH (~20:00, 4h after close)
        ];
        q.intraday = vec![310.93, 310.92, 310.71, 310.60];
        let seg = extended_hours_segment(&q).expect("AH segment present");
        assert_eq!(seg.0, "AH");
        let expected = 310.60 - 310.85;
        assert!(
            (seg.1 - expected).abs() < 1e-6,
            "expected {expected}, got {}",
            seg.1
        );
    }

    #[test]
    fn period_annotations_1d_returns_empty() {
        // 1D suppresses annotation-driven labels (the regular session is a
        // single uniform block; the legacy 9:30/10:45/... labels work fine).
        let ts: Vec<i64> = (0..78).map(|i| 1_700_000_000 + i * 300).collect();
        assert!(period_annotations(Period::Day, &ts).is_empty());
    }

    #[test]
    fn period_annotations_six_month_marks_month_boundaries() {
        // Use mid-month noon-UTC timestamps to dodge TZ-boundary ambiguity:
        // mid-Mar / mid-Apr / mid-May land in their named month in any
        // local TZ, so we should see three distinct month annotations.
        let ts = vec![
            1_773_172_800, // 2026-03-15 12:00 UTC
            1_775_851_200, // 2026-04-15 12:00 UTC
            1_778_443_200, // 2026-05-15 12:00 UTC
        ];
        let anns = period_annotations(Period::SixMonth, &ts);
        assert_eq!(anns.len(), 3);
        for ann in &anns {
            assert_eq!(ann.label.len(), 3, "label {:?}", ann.label);
        }
    }

    #[test]
    fn period_annotations_year_uses_quarter_boundaries() {
        // Months 1, 2, 3 (same quarter), 4 (new quarter), 7 (new quarter)
        // — expect 3 annotations (Q1, Q2, Q3).
        let ts = vec![
            1_767_225_600, // Jan 1 2026
            1_769_904_000, // Feb 1 2026
            1_772_323_200, // Mar 1 2026
            1_775_001_600, // Apr 1 2026
            1_783_036_800, // Jul 1 2026
        ];
        let anns = period_annotations(Period::Year, &ts);
        assert_eq!(anns.len(), 3);
    }

    #[test]
    fn period_annotations_five_year_uses_year_boundaries() {
        // Jan 1 of 2022, 2023, 2024, 2025, 2026.
        let ts = vec![
            1_640_995_200, // 2022-01-01 UTC
            1_672_531_200, // 2023-01-01 UTC
            1_704_067_200, // 2024-01-01 UTC
            1_735_689_600, // 2025-01-01 UTC
            1_767_225_600, // 2026-01-01 UTC
        ];
        let anns = period_annotations(Period::FiveYear, &ts);
        assert_eq!(anns.len(), 5);
    }

    #[test]
    fn period_annotations_ten_year_keeps_only_even_years() {
        let ts = vec![
            1_577_836_800, // 2020-01-01 UTC
            1_609_459_200, // 2021-01-01 UTC
            1_640_995_200, // 2022-01-01 UTC
            1_672_531_200, // 2023-01-01 UTC
            1_704_067_200, // 2024-01-01 UTC
        ];
        let anns = period_annotations(Period::TenYear, &ts);
        let years: Vec<i32> = anns.iter().filter_map(|a| a.label.parse().ok()).collect();
        assert!(years.iter().all(|y| y % 2 == 0), "got {:?}", years);
    }

    #[test]
    fn lay_out_x_axis_labels_at_cols_places_labels_around_target_columns() {
        // 30-wide line, three labels at cols 0, 14, 29. The middle label
        // is centered: a 3-char label centered on col 14 occupies cols
        // 13..16 (left = col - len/2).
        let items = vec![(0, "Jan"), (14, "May"), (29, "Dec")];
        let line = lay_out_x_axis_labels_at_cols(&items, 30);
        assert_eq!(line.len(), 30);
        assert!(line.starts_with("Jan"));
        assert!(line.ends_with("Dec"));
        let chars: Vec<char> = line.chars().collect();
        let mid: String = chars[13..16].iter().collect();
        assert_eq!(mid, "May");
    }

    #[test]
    fn lay_out_x_axis_labels_at_cols_skips_overlaps() {
        // Two labels too close to fit — second one collides with the first
        // and gets dropped.
        let items = vec![(0, "Jan"), (1, "Feb")];
        let line = lay_out_x_axis_labels_at_cols(&items, 10);
        assert!(line.starts_with("Jan"));
        assert!(!line.contains("Feb"));
    }

    #[test]
    fn pick_day_chart_bars_uses_today_regular_when_present() {
        // Mid-regular-session: bars exist within today's reg range.
        // Chart should use today's regular bars only.
        let mut q = quote("AAPL", 200.0, 195.0);
        q.regular_session_start_ts = Some(1_700_000_000);
        q.regular_session_end_ts = Some(1_700_023_400);
        q.previous_session_start_ts = Some(1_699_900_000);
        q.previous_session_end_ts = Some(1_699_923_400);
        q.intraday_timestamps = vec![
            1_699_920_000, // yesterday regular (would be in prev range)
            1_700_005_000, // today regular
            1_700_010_000, // today regular
        ];
        q.intraday = vec![196.0, 199.0, 200.0];
        let (vs, ts) = pick_day_chart_bars(&q).expect("today's regular session present");
        assert_eq!(ts, vec![1_700_005_000, 1_700_010_000]);
        assert_eq!(vs, vec![199.0, 200.0]);
    }

    #[test]
    fn pick_day_chart_bars_falls_back_to_previous_when_today_empty() {
        // Pre-market on a new day: today's regular range has no bars
        // yet, but yesterday's full session is in the data. Chart
        // should show yesterday's bars (not the pre-market bars).
        let mut q = quote("AAPL", 196.0, 195.0);
        q.regular_session_start_ts = Some(1_700_100_000); // today reg_start (well in future)
        q.regular_session_end_ts = Some(1_700_123_400);
        q.previous_session_start_ts = Some(1_700_000_000);
        q.previous_session_end_ts = Some(1_700_023_400);
        q.intraday_timestamps = vec![
            1_700_005_000, // yesterday regular
            1_700_010_000, // yesterday regular
            1_700_044_000, // yesterday AH (~7:30 ET) — must NOT appear
            1_700_086_000, // today PRE (~6:00 ET) — must NOT appear
            1_700_087_800, // today PRE — must NOT appear
        ];
        q.intraday = vec![196.0, 197.0, 198.0, 198.5, 199.0];
        let (vs, ts) = pick_day_chart_bars(&q).expect("previous session fallback");
        assert_eq!(
            ts,
            vec![1_700_005_000, 1_700_010_000],
            "pre-market and AH bars must be excluded"
        );
        assert_eq!(vs, vec![196.0, 197.0]);
    }

    #[test]
    fn pick_day_chart_bars_excludes_post_market_when_today_has_regular_bars() {
        // After today's regular close: today's regular bars are present
        // alongside today's AH bars. Chart should drop the AH bars.
        let mut q = quote("AAPL", 200.0, 195.0);
        q.regular_session_start_ts = Some(1_700_000_000);
        q.regular_session_end_ts = Some(1_700_023_400);
        q.intraday_timestamps = vec![
            1_700_005_000, // regular
            1_700_023_000, // regular (5min before close)
            1_700_023_400, // boundary (Yahoo: first AH bar) — must NOT appear
            1_700_024_000, // AH — must NOT appear
        ];
        q.intraday = vec![198.0, 200.0, 200.5, 201.0];
        let (vs, ts) = pick_day_chart_bars(&q).expect("today's regular bars");
        // Filter is inclusive on both ends, so boundary bar is included
        // intentionally — keeps the regular-close auction within the
        // chart even when Yahoo timestamps it at ts == reg_end. AH bars
        // after that drop off.
        assert_eq!(ts, vec![1_700_005_000, 1_700_023_000, 1_700_023_400]);
        assert_eq!(vs, vec![198.0, 200.0, 200.5]);
    }

    #[test]
    fn pick_day_chart_bars_returns_none_when_no_session_bounds() {
        // Yahoo didn't return trading periods at all (rare edge case).
        // Helper returns None and the caller falls back to unfiltered
        // data.
        let mut q = quote("AAPL", 196.0, 195.0);
        q.intraday_timestamps = vec![1_700_005_000, 1_700_010_000];
        q.intraday = vec![195.0, 196.0];
        assert!(pick_day_chart_bars(&q).is_none());
    }

    #[test]
    fn extended_hours_from_bars_hidden_during_regular_session() {
        // Latest bar is mid-day regular session — no AH/PRE segment.
        let mut q = quote("AAPL", 197.0, 196.0);
        q.regular_session_start_ts = Some(1_700_000_000);
        q.regular_session_end_ts = Some(1_700_023_400);
        q.intraday_timestamps = vec![1_700_005_000, 1_700_010_000]; // both inside session
        q.intraday = vec![196.5, 197.0];
        assert!(extended_hours_segment(&q).is_none());
    }

    #[test]
    fn market_status_open_counts_down_to_close() {
        // 2026-05-18 is a Monday. 10:30 ET → 5h30m until 16:00 close.
        let now = et_to_utc(2026, 5, 18, 10, 30);
        let s = market_status_line(now);
        assert!(s.is_open);
        assert_eq!(s.message, "5h30m until market close");
    }

    #[test]
    fn market_status_after_hours_counts_down_to_next_open() {
        // Monday 18:00 ET → next open is Tuesday 09:30 ET → 15h30m.
        let now = et_to_utc(2026, 5, 18, 18, 0);
        let s = market_status_line(now);
        assert!(!s.is_open);
        assert_eq!(s.message, "15h30m until market open");
    }

    #[test]
    fn market_status_pre_open_counts_down_to_today_open() {
        // Monday 08:00 ET → today's open at 09:30 ET → 1h30m.
        let now = et_to_utc(2026, 5, 18, 8, 0);
        let s = market_status_line(now);
        assert!(!s.is_open);
        assert_eq!(s.message, "1h30m until market open");
    }

    #[test]
    fn market_status_weekend_skips_to_monday() {
        // Saturday 12:00 ET → Monday 09:30 ET. ~45h30m.
        let now = et_to_utc(2026, 5, 16, 12, 0);
        let s = market_status_line(now);
        assert!(!s.is_open);
        assert!(
            s.message.ends_with("until market open"),
            "got {:?}",
            s.message
        );
        // Saturday 12:00 → Monday 09:30 = 45.5h
        assert!(s.message.starts_with("45h"), "got {:?}", s.message);
    }

    #[test]
    fn format_hm_drops_hours_when_zero() {
        assert_eq!(format_hm(ChronoDuration::minutes(45)), "45m");
        assert_eq!(format_hm(ChronoDuration::minutes(0)), "0m");
        assert_eq!(format_hm(ChronoDuration::seconds(7200 + 65)), "2h1m");
    }

    fn d(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn holiday_fixed_date_new_years_and_christmas() {
        assert!(is_market_holiday(d(2026, 1, 1)));
        assert!(is_market_holiday(d(2026, 12, 25)));
    }

    #[test]
    fn holiday_independence_day_observed_friday_when_saturday() {
        // July 4 2026 falls on Saturday → observed Friday Jul 3.
        assert!(is_market_holiday(d(2026, 7, 3)));
        // July 4 itself is the weekend; the observation logic shifts
        // the closure to the 3rd. The 4th alone wouldn't be a trading
        // day either way, so the test is about the observed day catching.
    }

    #[test]
    fn holiday_floating_dates() {
        // MLK = 3rd Mon of Jan 2026 → Jan 19.
        assert!(is_market_holiday(d(2026, 1, 19)));
        // Presidents = 3rd Mon of Feb 2026 → Feb 16.
        assert!(is_market_holiday(d(2026, 2, 16)));
        // Memorial = last Mon of May 2026 → May 25.
        assert!(is_market_holiday(d(2026, 5, 25)));
        // Labor = 1st Mon of Sep 2026 → Sep 7.
        assert!(is_market_holiday(d(2026, 9, 7)));
        // Thanksgiving = 4th Thu of Nov 2026 → Nov 26.
        assert!(is_market_holiday(d(2026, 11, 26)));
    }

    #[test]
    fn holiday_good_friday_via_easter_2026() {
        // Easter 2026 = April 5; Good Friday = April 3.
        assert!(is_market_holiday(d(2026, 4, 3)));
        // April 2 (Thursday) is NOT a holiday.
        assert!(!is_market_holiday(d(2026, 4, 2)));
    }

    #[test]
    fn half_day_black_friday_2026() {
        // Day after Thanksgiving 2026 → Friday Nov 27.
        assert!(is_market_half_day(d(2026, 11, 27)));
        // The Thursday before (Thanksgiving) is a full closure, not half.
        assert!(!is_market_half_day(d(2026, 11, 26)));
    }

    #[test]
    fn half_day_christmas_eve_weekday_only() {
        // 2026: Dec 24 is Thursday → half-day.
        assert!(is_market_half_day(d(2026, 12, 24)));
        // 2027: Dec 24 is Friday → observed Christmas closure (not
        // a half-day per our model).
        assert!(!is_market_half_day(d(2027, 12, 24)));
        // 2028: Dec 24 is Sunday → not a trading day either way.
        assert!(!is_market_half_day(d(2028, 12, 24)));
    }

    #[test]
    fn market_status_skips_full_closure_holidays() {
        // Thursday Nov 26, 2026 is Thanksgiving → countdown points to
        // Friday Nov 27 (the half-day) at 09:30 ET.
        let now = et_to_utc(2026, 11, 26, 11, 0);
        let s = market_status_line(now);
        assert!(!s.is_open);
        assert!(s.message.contains("until market open"));
    }

    #[test]
    fn market_status_uses_early_close_on_half_day() {
        // 2026-11-27 (Black Friday) at 12:00 ET → 1h until 13:00 close.
        let now = et_to_utc(2026, 11, 27, 12, 0);
        let s = market_status_line(now);
        assert!(s.is_open);
        assert_eq!(s.message, "1h0m until market close");
    }

    #[test]
    fn nth_weekday_matches_known_dates() {
        // 3rd Mon of Jan 2026 = Jan 19.
        assert_eq!(nth_weekday(2026, 1, Weekday::Mon, 3), Some(d(2026, 1, 19)));
        // 4th Thu of Nov 2026 = Nov 26.
        assert_eq!(
            nth_weekday(2026, 11, Weekday::Thu, 4),
            Some(d(2026, 11, 26))
        );
    }

    #[test]
    fn last_weekday_handles_december_rollover() {
        // Last Mon of Dec 2026 = Dec 28.
        assert_eq!(last_weekday(2026, 12, Weekday::Mon), Some(d(2026, 12, 28)));
    }

    #[test]
    fn is_leap_year_matches_gregorian_rule() {
        assert!(is_leap_year(2024));
        assert!(!is_leap_year(2025));
        assert!(!is_leap_year(2026));
        assert!(is_leap_year(2000)); // /400 → leap
        assert!(!is_leap_year(1900)); // /100 but not /400 → common
        assert!(is_leap_year(2400));
    }

    #[test]
    fn x_axis_label_layout_anchors_last_label_at_right_edge() {
        // 6 labels into 60 cells: the old `step = plot_w/n` formula
        // placed "now" at col 50, leaving 7 cells of trailing
        // whitespace short of the plot's right edge. The new layout
        // puts "now" so its right edge lands at col 60 (i.e. left
        // edge at col 57).
        let labels = ["-5y", "-4y", "-3y", "-2y", "-1y", "now"];
        let line = lay_out_x_axis_labels(&labels, 60);
        let trimmed = line.trim_end_matches(' ');
        assert!(trimmed.ends_with("now"));
        assert_eq!(line.chars().count(), 60);
        // "now" sits at cols 57..60 (left-edge 57 = (5 * 57) / 5 = 57).
        assert_eq!(&line[line.len() - 3..], "now");
    }

    #[test]
    fn x_axis_label_layout_left_anchors_first_label() {
        let labels = ["Jan", "Mar", "May", "Jul", "Sep", "Nov"];
        let line = lay_out_x_axis_labels(&labels, 60);
        assert!(line.starts_with("Jan"));
    }

    #[test]
    fn x_axis_label_layout_handles_single_label() {
        let labels = ["solo"];
        let line = lay_out_x_axis_labels(&labels, 20);
        assert_eq!(line, "solo");
    }

    #[test]
    fn rolling_year_labels_walk_back_from_today_in_2_month_steps() {
        // Today = 2026-05-23 → 12mo ago = May 2025, 10mo = Jul, …,
        // 2mo = Mar 2026, 0mo = May 2026.
        let today = NaiveDate::from_ymd_opt(2026, 5, 23).unwrap();
        let labels = rolling_year_month_labels(today);
        assert_eq!(
            labels,
            vec!["May", "Jul", "Sep", "Nov", "Jan", "Mar", "May"]
        );
    }

    #[test]
    fn rolling_year_labels_handle_year_boundaries() {
        // Today = 2026-01-15 → 12mo ago = Jan 2025, 10mo = Mar 2025,
        // …, 0mo = Jan 2026. Walks through Mar/May/Jul/Sep/Nov.
        let today = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
        let labels = rolling_year_month_labels(today);
        assert_eq!(
            labels,
            vec!["Jan", "Mar", "May", "Jul", "Sep", "Nov", "Jan"]
        );
    }
}
