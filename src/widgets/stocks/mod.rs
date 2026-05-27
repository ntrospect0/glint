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
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use serde::Deserialize;

use crate::ui::{decorate_title, focus_border_style};

use super::{AppContext, EventResult, Widget};

use provider::{Period, StockQuote, YahooFinanceProvider};

/// User-configurable stocks options (loaded from `~/.config/glint/stocks.toml`).
#[derive(Debug, Clone, Deserialize)]
pub struct StocksConfig {
    /// Index symbols listed at the top of the ticker list. Yahoo conventions:
    /// `^DJI` (Dow), `^GSPC` (S&P 500), `^IXIC` (Nasdaq Composite).
    #[serde(default = "default_indices")]
    pub indices: Vec<String>,

    /// User-defined watchlist tickers shown alongside the indices.
    #[serde(default = "default_watchlist")]
    pub watchlist: Vec<String>,

    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    /// Initial display mode. One of "percent" / "dollar" / "change".
    /// Initial display mode for the change column. Accepts the legacy name
    /// `display_mode` for backward compatibility.
    #[serde(default, alias = "display_mode")]
    pub default_display_mode: DisplayMode,

    /// Initial graph period. One of "1d" / "1w" / "1m" / "6m" / "ytd" / "1y".
    #[serde(default)]
    pub default_period: Period,

    /// Optional URL template opened when the user presses `j` (Jump). The
    /// literal token `{ticker}` is replaced with the URL-encoded ticker
    /// symbol — e.g. `"https://www.marketwatch.com/investing/stock/{ticker}"`.
    /// Leave unset to make `j` a no-op.
    #[serde(default)]
    pub jump_url_template: Option<String>,

    /// When true, horizontal mouse scroll cycles the period toggles. Default
    /// is false because trackpad horizontal-scroll gestures often fire
    /// accidentally while scrolling vertically.
    #[serde(default)]
    pub horizontal_scroll_period: bool,
}

fn default_indices() -> Vec<String> {
    vec!["^DJI".into(), "^GSPC".into(), "^IXIC".into()]
}
fn default_watchlist() -> Vec<String> {
    vec![
        "AAPL".into(),
        "MSFT".into(),
        "GOOGL".into(),
        "NVDA".into(),
        "TSLA".into(),
    ]
}
fn default_poll_interval() -> u64 {
    60
}

impl Default for StocksConfig {
    fn default() -> Self {
        Self {
            indices: default_indices(),
            watchlist: default_watchlist(),
            poll_interval_secs: default_poll_interval(),
            default_display_mode: DisplayMode::default(),
            default_period: Period::default(),
            jump_url_template: None,
            horizontal_scroll_period: false,
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
    Ready(Box<StockQuote>),
    /// Last fetch failed. Reason is already logged via tracing; we don't need
    /// to surface it in the UI right now (the row just shows "err").
    Failed,
}

#[derive(Default)]
struct StocksState {
    quotes: HashMap<String, QuoteState>,
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
    last_attempt: Option<Instant>,
    any_inflight: bool,
}

pub struct StocksWidget {
    id: String,
    config: StocksConfig,
    provider: Arc<YahooFinanceProvider>,
    state: Arc<Mutex<StocksState>>,
    poll_interval: Duration,
    /// Display mode cycled by the `%` / `$` / `c` keys; kept in widget (not
    /// state) since it changes synchronously and never via the network.
    display_mode: DisplayMode,
    /// Currently selected graph period (1D / 1W / 1M / 6M / YTD / 1Y).
    period: Period,
}

impl StocksWidget {
    pub fn with_config(config: StocksConfig) -> Self {
        let provider = match YahooFinanceProvider::new() {
            Ok(p) => Arc::new(p),
            Err(err) => {
                tracing::warn!(error = %err, "failed to build Yahoo Finance provider, stocks widget will be inert");
                Arc::new(provider_dummy())
            }
        };
        let display_mode = config.default_display_mode;
        let period = config.default_period;
        Self {
            id: "stocks".into(),
            poll_interval: Duration::from_secs(config.poll_interval_secs.max(15)),
            config,
            provider,
            state: Arc::new(Mutex::new(StocksState::default())),
            display_mode,
            period,
        }
    }

    fn set_period(&mut self, period: Period) {
        if self.period == period {
            return;
        }
        self.period = period;
        // Force a refresh on the next tick so the chart and change%
        // catch up to the new window.
        self.mark_dirty();
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
        if let Some(t) = self.state.lock().expect("stocks state poisoned").transient_ticker.clone() {
            if !out.iter().any(|s| s.eq_ignore_ascii_case(&t)) {
                out.push(t);
            }
        }
        out
    }


    fn is_due(&self) -> bool {
        let st = self.state.lock().expect("stocks state poisoned");
        if st.any_inflight {
            return false;
        }
        match st.last_attempt {
            None => true,
            Some(t) => t.elapsed() >= self.poll_interval,
        }
    }

    fn spawn_refresh(&self) {
        let symbols: Vec<String> = self.all_symbols();
        if symbols.is_empty() {
            return;
        }
        {
            let mut st = self.state.lock().expect("stocks state poisoned");
            st.any_inflight = true;
            st.last_attempt = Some(Instant::now());
            for sym in &symbols {
                st.quotes
                    .entry(sym.clone())
                    .or_insert(QuoteState::Inflight);
            }
        }
        let provider = self.provider.clone();
        let state = self.state.clone();
        let period = self.period;
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
            for (sym, result) in results {
                match result {
                    Ok(q) => {
                        st.quotes.insert(sym, QuoteState::Ready(Box::new(q)));
                    }
                    Err(err) => {
                        tracing::warn!(symbol = %sym, error = %err, "stock fetch failed");
                        st.quotes.insert(sym, QuoteState::Failed);
                    }
                }
            }
            st.any_inflight = false;
        });
    }

    fn mark_dirty(&self) {
        let mut st = self.state.lock().expect("stocks state poisoned");
        st.last_attempt = None;
    }

    fn move_selection(&mut self, delta: isize) {
        let n = self.all_symbols().len();
        if n == 0 {
            return;
        }
        let mut st = self.state.lock().expect("stocks state poisoned");
        let new = (st.selected as isize + delta).clamp(0, n as isize - 1);
        st.selected = new as usize;
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
        // If it already looks like a ticker (short, ASCII-uppercase + ^ . - =)
        // skip the search round-trip.
        let direct = is_tickerish(&query_trim).then(|| query_trim.to_uppercase());
        if let Some(symbol) = direct {
            self.set_transient_now(symbol);
            return;
        }
        // Mark "searching…" so the UI can show feedback while the request flies.
        {
            let mut st = self.state.lock().expect("stocks state poisoned");
            st.transient_searching = Some(query_trim.clone());
        }
        let provider = self.provider.clone();
        let state = self.state.clone();
        // Total slot count (indices + watchlist) — knowing this lets us snap
        // selection straight to the transient row (last slot) when search
        // resolves.
        let base_slot = self.config.indices.len() + self.config.watchlist.len();
        tokio::spawn(async move {
            let result = provider.search(&query_trim).await;
            let mut st = state.lock().expect("stocks state poisoned");
            st.transient_searching = None;
            match result {
                Ok(symbol) => {
                    st.transient_ticker = Some(symbol);
                    st.selected = base_slot;
                    st.last_attempt = None;
                }
                Err(err) => {
                    tracing::warn!(query = %query_trim, error = %err, "stock lookup failed");
                }
            }
        });
    }

    /// Insert `symbol` as the transient lookup synchronously (used when the
    /// query already looked like a ticker, no search needed).
    fn set_transient_now(&self, symbol: String) {
        let base_slot = self.config.indices.len() + self.config.watchlist.len();
        let mut st = self.state.lock().expect("stocks state poisoned");
        st.transient_ticker = Some(symbol);
        st.transient_searching = None;
        st.selected = base_slot;
        st.last_attempt = None;
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

    /// Open the selected ticker in the user's browser via the configured
    /// `jump_url_template` (replacing `{ticker}` with the URL-encoded symbol).
    /// No-op when no template is configured.
    fn jump_to_external(&self) {
        let Some(template) = &self.config.jump_url_template else {
            tracing::info!("'j' pressed but no jump_url_template is configured");
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

    fn snapshot_quotes(&self) -> HashMap<String, QuoteState> {
        let st = self.state.lock().expect("stocks state poisoned");
        st.quotes.clone()
    }

    /// Compute the same panel rects `render` uses so click handlers can map
    /// coordinates back to a target panel.
    fn compute_layout(&self, inner: Rect) -> StocksPanels {
        const WIDE_LIST_W: u16 = 36;
        const WIDE_STATS_W: u16 = 30;
        const MIN_GRAPH_W: u16 = 24;
        let is_wide = inner.width >= WIDE_LIST_W + MIN_GRAPH_W;
        let with_stats = is_wide && inner.width >= WIDE_LIST_W + WIDE_STATS_W + MIN_GRAPH_W;
        if is_wide {
            let mut constraints: Vec<Constraint> = vec![
                Constraint::Length(WIDE_LIST_W),
                Constraint::Length(1),
            ];
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

    fn display_name(&self) -> &str {
        "Stocks"
    }

    async fn update(&mut self, _ctx: &AppContext) -> Result<()> {
        if self.is_due() {
            self.spawn_refresh();
        }
        Ok(())
    }

    fn render(&self, frame: &mut Frame, area: Rect, focused: bool) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(focus_border_style(focused))
            .title(Span::styled(
                decorate_title(focused, "Stocks"),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let quotes = self.snapshot_quotes();
        let symbols: Vec<String> = self.all_symbols();
        let selected_sym = self.selected_symbol();
        let base_count = self.config.indices.len() + self.config.watchlist.len();
        let lookup_start = if symbols.len() > base_count {
            Some(base_count)
        } else {
            None
        };

        // Adaptive layout: in landscape mode (wide), list | stats | graph
        // run horizontally — list + stats get their full width first, graph
        // fills whatever's left. In portrait mode (narrow), they stack
        // vertically: list on top, graph on the bottom.
        const WIDE_LIST_W: u16 = 36;
        const WIDE_STATS_W: u16 = 30;
        const MIN_GRAPH_W: u16 = 24;
        let is_wide = inner.width >= WIDE_LIST_W + MIN_GRAPH_W;
        let with_stats = is_wide && inner.width >= WIDE_LIST_W + WIDE_STATS_W + MIN_GRAPH_W;

        // 1-col gaps between panels so they don't visually run together.
        if is_wide {
            let mut constraints: Vec<Constraint> = vec![
                Constraint::Length(WIDE_LIST_W),
                Constraint::Length(1),
            ];
            if with_stats {
                constraints.push(Constraint::Length(WIDE_STATS_W));
                constraints.push(Constraint::Length(1));
            }
            constraints.push(Constraint::Fill(1));
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(constraints)
                .split(inner);
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
            );
            self.state.lock().unwrap().list_scroll = new_scroll;
            if let Some(stats_area) = stats_area {
                render_stats_panel(frame, stats_area, selected_sym.as_deref(), &quotes);
            }
            render_graph_panel(
                frame,
                graph_area,
                selected_sym.as_deref(),
                &quotes,
                self.period,
            );
        } else {
            // Portrait: list on top (clamped to ~55% so it's readable), graph below.
            let list_h = ((inner.height as f32) * 0.55).round() as u16;
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(list_h),
                    Constraint::Length(1),
                    Constraint::Fill(1),
                ])
                .split(inner);
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
            );
            self.state.lock().unwrap().list_scroll = new_scroll;
            render_graph_panel(
                frame,
                rows[2],
                selected_sym.as_deref(),
                &quotes,
                self.period,
            );
        }

        // Footer hint along the bottom of the cell.
        if inner.height >= 2 {
            let footer = Rect {
                x: inner.x,
                y: inner.y + inner.height - 1,
                width: inner.width,
                height: 1,
            };
            let hint = format!(
                "↑/↓ select · c mode ({}) · j jump · r refresh",
                display_mode_label(self.display_mode)
            );
            frame.render_widget(
                Paragraph::new(Span::styled(
                    hint,
                    Style::default().add_modifier(Modifier::DIM),
                ))
                .alignment(Alignment::Right),
                footer,
            );
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if key.modifiers != KeyModifiers::NONE && key.modifiers != KeyModifiers::SHIFT {
            return EventResult::Ignored;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                EventResult::Handled
            }
            // `j` is the Jump key (open ticker in browser). Down navigation
            // stays on ↓ — vim's `j`-as-down is freed up here on purpose.
            KeyCode::Down => {
                self.move_selection(1);
                EventResult::Handled
            }
            KeyCode::Char('j') => {
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
            // 1..9 picks a graph period directly.
            KeyCode::Char(d @ '1'..='9') => {
                let idx = (d as u8 - b'1') as usize;
                if let Some(p) = Period::ALL.get(idx) {
                    self.set_period(*p);
                }
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
                            let mut st = self.state.lock().expect("stocks state poisoned");
                            st.selected = idx;
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
            ("↑ / ↓ / k", "select ticker (k = up)"),
            ("c", "cycle display mode (% / $)"),
            ("% / $", "set display mode directly"),
            ("1-9", "set graph period directly"),
            ("j", "Jump: open ticker URL in browser"),
            ("r", "force refresh"),
            ("x", "clear :stock lookup (return to default list)"),
            ("click ticker", "select that ticker"),
            ("click toggle", "switch graph period"),
            (":stock <sym|name>", "look up a ticker and pin it"),
        ]
    }

    fn config(&self) -> serde_json::Value {
        serde_json::json!({
            "indices": self.config.indices,
            "watchlist": self.config.watchlist,
            "poll_interval_secs": self.poll_interval.as_secs(),
            "display_mode": display_mode_label(self.display_mode),
        })
    }

    fn apply_config(&mut self, config: serde_json::Value) -> Result<()> {
        let new_config: StocksConfig =
            serde_json::from_value(config).context("invalid stocks config payload")?;
        *self = Self::with_config(new_config);
        Ok(())
    }
}

fn display_mode_label(m: DisplayMode) -> &'static str {
    match m {
        DisplayMode::Percent => "%",
        DisplayMode::Dollar => "$",
    }
}

/// Heuristic: does `s` look like a Yahoo ticker (e.g. AAPL, ^GSPC, BRK-A,
/// CAD=X)? If so we skip the search hop. Tickers are short and use a small
/// punctuation set; company names have lowercase letters or spaces.
fn is_tickerish(s: &str) -> bool {
    let len = s.chars().count();
    if !(1..=8).contains(&len) {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '^' | '.' | '-' | '='))
}

fn render_graph_panel(
    frame: &mut Frame,
    area: Rect,
    selected: Option<&str>,
    quotes: &HashMap<String, QuoteState>,
    period: Period,
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
        let para = Paragraph::new(Line::from(Span::styled(
            msg,
            Style::default().add_modifier(Modifier::DIM),
        )))
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
    let header = Line::from(vec![
        Span::styled(
            q.symbol.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{:>10.2} {currency}", q.price),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{glyph} {:+.2} ({:+.2}%) {}", chg, pct, period.label()),
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

    if plot_h == 0 || q.intraday.is_empty() {
        return;
    }

    // Compute y-range from the data, with a small padding so the trace doesn't
    // hug the borders.
    let (mut min, mut max) = (f64::INFINITY, f64::NEG_INFINITY);
    for v in &q.intraday {
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
    let pad = (max - min) * 0.08;
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
        frame.render_widget(
            Paragraph::new(Span::styled(
                label,
                Style::default().add_modifier(Modifier::DIM),
            )),
            rect,
        );
    }

    let rows = graph::render_series(&q.intraday, plot_h, plot_w, plot_min, plot_max);
    for (i, row) in rows.iter().enumerate() {
        let rect = Rect {
            x: plot_x,
            y: plot_top + i as u16,
            width: plot_w,
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

    // X-axis labels: a few evenly-spaced markers — content varies by period.
    let xaxis_rect = Rect {
        x: plot_x,
        y: plot_top + plot_h,
        width: plot_w,
        height: 1,
    };
    let labels: &[&str] = match period {
        Period::Day => &["9:30", "10:45", "12:00", "13:15", "14:30", "15:45"],
        Period::Week => &["Mon", "Tue", "Wed", "Thu", "Fri"],
        Period::Month => &["wk1", "wk2", "wk3", "wk4"],
        Period::SixMonth => &["1mo", "2mo", "3mo", "4mo", "5mo", "6mo"],
        Period::YearToDate => &["Q1", "Q2", "Q3", "Q4"],
        Period::Year => &["Jan", "Mar", "May", "Jul", "Sep", "Nov"],
        Period::ThreeYear => &["-3y", "-2y", "-1y", "now"],
        Period::FiveYear => &["-5y", "-4y", "-3y", "-2y", "-1y", "now"],
        Period::TenYear => &["-10y", "-8y", "-6y", "-4y", "-2y", "now"],
    };
    let step = (plot_w / labels.len() as u16).max(1);
    let mut line = String::with_capacity(plot_w as usize);
    for (i, lbl) in labels.iter().enumerate() {
        let target = (i as u16 * step) as usize;
        while line.chars().count() < target {
            line.push(' ');
        }
        line.push_str(lbl);
    }
    frame.render_widget(
        Paragraph::new(Span::styled(
            line,
            Style::default().add_modifier(Modifier::DIM),
        )),
        xaxis_rect,
    );
}

/// Returns (change_abs, change_pct) for the given period. 1D uses the
/// previous-close convention (standard ticker change); longer windows use
/// the first sample in the series as the baseline.
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
fn render_period_toggle_bar(frame: &mut Frame, area: Rect, active: Period) {
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
                Style::default().add_modifier(Modifier::DIM)
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
        let dim = Style::default().add_modifier(Modifier::DIM);
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

#[allow(clippy::too_many_arguments)] // 9 args, all distinct render inputs — no obvious bundle.
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
) -> usize {
    let (lines, ticker_lines) = build_list_lines(
        symbols,
        indices_count,
        lookup_start,
        quotes,
        selected,
        mode,
        period,
    );

    // Reserve the bottom row for the footer hint rendered in `render`.
    let usable_h = area.height.saturating_sub(1) as usize;
    let list_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: usable_h as u16,
    };

    // Keep the selected ticker visible by adjusting scroll.
    let sel_line = ticker_lines.get(selected).copied().unwrap_or(0);
    let mut scroll = current_scroll;
    if sel_line < scroll {
        scroll = sel_line;
    }
    if usable_h > 0 && sel_line >= scroll + usable_h {
        scroll = sel_line + 1 - usable_h;
    }
    // Cap scroll to the last useful starting line so we don't waste space.
    let max_scroll = lines.len().saturating_sub(usable_h.max(1));
    if scroll > max_scroll {
        scroll = max_scroll;
    }

    let end = (scroll + usable_h).min(lines.len());
    let visible: Vec<Line<'_>> = lines.into_iter().skip(scroll).take(end - scroll).collect();
    frame.render_widget(Paragraph::new(visible), list_area);
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
) -> (Vec<Line<'a>>, Vec<usize>) {
    let mut lines: Vec<Line<'a>> = Vec::with_capacity(symbols.len() + 4);
    let mut ticker_lines: Vec<usize> = Vec::with_capacity(symbols.len());
    if indices_count > 0 {
        lines.push(Line::from(Span::styled(
            "── Indices ──",
            Style::default().add_modifier(Modifier::DIM),
        )));
    }
    let mut watchlist_header_emitted = indices_count == 0;
    let mut lookup_header_emitted = false;
    for (i, sym) in symbols.iter().enumerate() {
        if !watchlist_header_emitted && i == indices_count {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "── Watchlist ──",
                Style::default().add_modifier(Modifier::DIM),
            )));
            watchlist_header_emitted = true;
        }
        if let Some(start) = lookup_start {
            if !lookup_header_emitted && i == start {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "── Lookup (press x to clear) ──",
                    Style::default().add_modifier(Modifier::DIM),
                )));
                lookup_header_emitted = true;
            }
        }
        ticker_lines.push(lines.len());
        let is_selected = i == selected;
        lines.push(format_list_row(sym, quotes.get(sym), is_selected, mode, period));
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
            let color = if chg_abs >= 0.0 { Color::Green } else { Color::Red };
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
) {
    let q = match selected.and_then(|s| quotes.get(s)) {
        Some(QuoteState::Ready(q)) => q.as_ref(),
        _ => {
            let para = Paragraph::new(Span::styled(
                "(no stats)",
                Style::default().add_modifier(Modifier::DIM),
            ))
            .alignment(Alignment::Center);
            frame.render_widget(para, area);
            return;
        }
    };

    let mut lines: Vec<Line<'_>> = Vec::new();
    lines.push(Line::from(Span::styled(
        q.short_name.clone(),
        Style::default()
            .fg(Color::LightCyan)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));
    lines.push(stat_line("Price", &format!("{:.2}", q.price)));
    lines.push(stat_line("Prev Close", &format!("{:.2}", q.previous_close)));
    if let (Some(h), Some(l)) = (q.day_high, q.day_low) {
        lines.push(stat_line("Day H/L", &format!("{h:.2} / {l:.2}")));
    }
    if let (Some(h), Some(l)) = (q.fifty_two_week_high, q.fifty_two_week_low) {
        lines.push(stat_line("52w H/L", &format!("{h:.2} / {l:.2}")));
    }
    if let Some(v) = q.volume {
        lines.push(stat_line("Volume", &humanize_big(v as f64)));
    }
    if let Some(v) = q.avg_volume {
        lines.push(stat_line("Avg Vol", &humanize_big(v as f64)));
    }
    lines.push(stat_line(
        "Mkt Cap",
        &q.market_cap
            .map(|v| humanize_big(v as f64))
            .unwrap_or_else(|| "—".into()),
    ));
    lines.push(stat_line(
        "Shares",
        &q.shares_outstanding
            .map(|v| humanize_big(v as f64))
            .unwrap_or_else(|| "—".into()),
    ));
    if let Some(pe) = q.pe_ratio {
        lines.push(stat_line("P/E", &format!("{pe:.2}")));
    }
    if let Some(eps) = q.eps {
        lines.push(stat_line("EPS", &format!("{eps:.2}")));
    }
    if let Some(y) = q.dividend_yield {
        lines.push(stat_line("Yield", &format!("{:.2}%", y * 100.0)));
    }
    if let Some(b) = q.beta {
        lines.push(stat_line("Beta", &format!("{b:.2}")));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn stat_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{:<10}", label),
            Style::default().add_modifier(Modifier::DIM),
        ),
        Span::styled(value.to_string(), Style::default()),
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

#[cfg(test)]
mod tests {
    use super::*;

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
            fetched_at: chrono::Local::now(),
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
        let w = StocksWidget::with_config(StocksConfig::default());
        let syms = w.all_symbols();
        assert_eq!(syms[0], "^DJI");
        assert_eq!(syms[3], "AAPL");
    }

    #[test]
    fn cycle_period_wraps_at_both_ends() {
        let mut w = StocksWidget::with_config(StocksConfig::default());
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
        let mut w = StocksWidget::with_config(StocksConfig::default());
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
            m.insert("AAPL".to_string(), QuoteState::Ready(Box::new(q)));
            m
        };
        let line_sel = format_list_row("AAPL", qs.get("AAPL"), true, DisplayMode::Percent, Period::Day);
        let line_un = format_list_row("AAPL", qs.get("AAPL"), false, DisplayMode::Percent, Period::Day);
        let sel_text: String = line_sel
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        let un_text: String = line_un
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(sel_text.contains("▸"));
        assert!(!un_text.contains("▸"));
    }
}
