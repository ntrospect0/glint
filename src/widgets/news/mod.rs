pub mod provider;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Alignment, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};
use serde::Deserialize;

use crate::llm::{LlmMessage, LlmProvider, LlmRequest, Role};
use crate::theme::{ColorScheme, Theme};
use crate::ui::decorated_title_line;

use super::{AppContext, EventResult, Widget};

use provider::{Article, FeedConfig, NewsProvider, RssProvider, Topic};

#[derive(Debug, Clone)]
enum SummaryState {
    Requested,
    Ready(String),
    /// LLM call failed; we already logged the reason via tracing, so just
    /// remember the failure so the render path can fall back to the raw RSS
    /// excerpt rather than re-requesting.
    Failed,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewsConfig {
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    #[serde(default)]
    pub feeds: Vec<FeedConfig>,

    #[serde(default)]
    pub topics: Vec<Topic>,

    /// When true, horizontal mouse scroll cycles the filter tabs. Default is
    /// false because trackpad horizontal-scroll gestures often fire
    /// accidentally while scrolling vertically.
    #[serde(default)]
    pub horizontal_scroll_filters: bool,

    /// When true, each article's meta row trails with its detected topic
    /// labels (e.g. `[Business,World]`). Many users find the categorization
    /// noise unhelpful — flip this off to suppress it. Default true.
    #[serde(default = "default_show_topic_labels")]
    pub show_topic_labels: bool,

    /// Per-widget style overrides layered on top of the active app
    /// color scheme. Any role omitted here inherits from the app theme.
    #[serde(default)]
    pub colors: ColorScheme,

    /// Prioritized `Shift+<letter>` focus shortcut preferences. Leave
    /// empty to use the built-in default (`['n', 'e', 'w', 's']`).
    #[serde(default)]
    pub shortcuts: Vec<char>,
}

fn default_show_topic_labels() -> bool {
    true
}

fn default_poll_interval() -> u64 {
    900
}

impl Default for NewsConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: default_poll_interval(),
            feeds: Vec::new(),
            topics: Vec::new(),
            horizontal_scroll_filters: false,
            show_topic_labels: default_show_topic_labels(),
            colors: ColorScheme::default(),
            shortcuts: Vec::new(),
        }
    }
}

/// Free-text search filter built by `:news <terms>`. Articles match if any
/// term appears (case-insensitive substring) in the title or summary;
/// results are sorted by total occurrence count.
#[derive(Debug, Clone)]
struct SearchFilter {
    /// Original user input — used for the tab label.
    query: String,
    /// Lowercased tokens to match against article text.
    terms: Vec<String>,
}

impl SearchFilter {
    fn new(raw: &str) -> Option<Self> {
        let terms: Vec<String> = raw
            .split_whitespace()
            .map(|t| t.to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        if terms.is_empty() {
            return None;
        }
        Some(Self {
            query: raw.split_whitespace().collect::<Vec<_>>().join(" "),
            terms,
        })
    }

    /// Total number of case-insensitive substring matches of any term in
    /// `article`'s title + summary. Used both as the "include?" predicate
    /// (>0 means include) and as the sort key (higher wins).
    fn hit_count(&self, article: &Article) -> usize {
        let title = article.title.to_lowercase();
        let summary = article
            .summary
            .as_deref()
            .map(|s| s.to_lowercase())
            .unwrap_or_default();
        let mut total = 0usize;
        for term in &self.terms {
            total += count_substring(&title, term);
            total += count_substring(&summary, term);
        }
        total
    }
}

fn count_substring(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0usize;
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        count += 1;
        start += pos + needle.len();
    }
    count
}

#[derive(Default)]
struct NewsState {
    articles: Vec<Article>,
    selected: usize,
    scroll: usize,
    /// When true, the selected article renders its full summary (up to
    /// `MAX_SUMMARY_LINES` wrapped lines) instead of the one-line excerpt.
    expanded: bool,
    /// Index into the *visible* tab list (static topic tabs + the dynamic
    /// search tab when one exists). 0 is always `All`.
    active_filter_idx: usize,
    last_error: Option<String>,
    last_attempt: Option<Instant>,
    inflight: bool,
    /// Per-article LLM summarization state, keyed by article URL.
    summaries: HashMap<String, SummaryState>,
    /// Active `:news <terms>` filter, if any. When present, an extra tab
    /// is appended to the tab bar and articles matching at least one term
    /// are surfaced (sorted by hit count). Cleared by `x` or `:news` with
    /// no args.
    search: Option<SearchFilter>,
}

const MAX_SUMMARY_LINES: usize = 6;
const ALL_TAB_LABEL: &str = "All";

const SUMMARY_SYSTEM_PROMPT: &str = "You are a concise news summarizer. \
Given a headline and a short excerpt, return a neutral 3-5 sentence summary \
capturing the key facts and any direct quotes. Do not editorialize, do not \
add preamble, do not use markdown. If the input is too sparse to summarize \
faithfully, respond with the single sentence: \"Insufficient content to summarize.\"";

pub struct NewsWidget {
    id: String,
    provider: Arc<dyn NewsProvider>,
    state: Arc<Mutex<NewsState>>,
    poll_interval: Duration,
    feeds_configured: bool,
    /// Tabs across the top of the cell. Index 0 is always `All`; the rest
    /// mirror the topic labels in news.toml.
    filter_tabs: Vec<String>,
    /// Optional LLM provider for on-demand article summarization.
    llm: Option<Arc<dyn LlmProvider>>,
    /// True when the user has opted into LLM news summaries via llm.toml.
    llm_summarize_enabled: bool,
    /// Mirrors NewsConfig.horizontal_scroll_filters — gates the ScrollLeft /
    /// ScrollRight handler so accidental trackpad gestures don't switch tabs
    /// for users who haven't asked for that.
    horizontal_scroll_filters: bool,
    /// Mirrors NewsConfig.show_topic_labels — when false the meta line
    /// won't append `[Business,World,…]`.
    show_topic_labels: bool,
    /// App-level theme; kept so live config reloads can rebuild `theme`
    /// from updated `colors` overrides.
    app_theme: Arc<Theme>,
    /// Cached widget-level `[colors]` overrides. Stored so `:scheme` can
    /// rebuild the merged theme without re-reading `news.toml`.
    colors_override: ColorScheme,
    /// Merged theme (app + widget overrides). Rebuilt on `apply_config`.
    theme: Theme,
    /// Letter assigned by the app for `Shift+<letter>` focus, painted in
    /// the title via `text.shortcut`. `None` = no shortcut claimed.
    shortcut: Option<char>,
    /// Effective shortcut preference list (TOML override or built-in).
    shortcut_prefs: Vec<char>,
}

impl NewsWidget {
    #[cfg(test)]
    pub fn with_config(config: NewsConfig) -> Self {
        Self::with_config_and_llm(config, None, false, Arc::new(Theme::builtin_defaults()))
    }

    pub fn with_config_and_llm(
        config: NewsConfig,
        llm: Option<Arc<dyn LlmProvider>>,
        llm_summarize_enabled: bool,
        app_theme: Arc<Theme>,
    ) -> Self {
        let feeds_configured = !config.feeds.is_empty();
        let horizontal_scroll_filters = config.horizontal_scroll_filters;
        let show_topic_labels = config.show_topic_labels;
        let mut filter_tabs = vec![ALL_TAB_LABEL.to_string()];
        filter_tabs.extend(config.topics.iter().map(|t| t.label.clone()));
        let colors_override = config.colors.clone();
        let theme = app_theme.with_overrides(&colors_override);
        let shortcut_prefs = if config.shortcuts.is_empty() {
            vec!['n', 'e', 'w', 's']
        } else {
            config.shortcuts.clone()
        };
        let provider: Arc<dyn NewsProvider> = match RssProvider::new(config.feeds, config.topics) {
            Ok(p) => Arc::new(p),
            Err(err) => {
                tracing::warn!(error = %err, "failed to build news provider, news widget will be empty");
                Arc::new(EmptyProvider)
            }
        };
        Self {
            id: "news".into(),
            provider,
            state: Arc::new(Mutex::new(NewsState::default())),
            poll_interval: Duration::from_secs(config.poll_interval_secs.max(60)),
            feeds_configured,
            filter_tabs,
            llm,
            llm_summarize_enabled,
            horizontal_scroll_filters,
            show_topic_labels,
            app_theme,
            colors_override,
            theme,
            shortcut: None,
            shortcut_prefs,
        }
    }

    /// Kick off an LLM summarization task for the given article if we have a
    /// provider, summaries are enabled, and we haven't already requested one.
    /// Skips the round-trip when the raw RSS excerpt already has enough
    /// substance — the LLM's contribution there is marginal at best, and
    /// replacing a useful paragraph with "Insufficient content to summarize."
    /// is a net loss.
    fn ensure_summary_requested(&self, article: &Article) {
        if !self.llm_summarize_enabled || self.llm.is_none() {
            return;
        }
        if !needs_llm_summary(article.summary.as_deref()) {
            return;
        }
        {
            let st = self.state.lock().expect("news state poisoned");
            if st.summaries.contains_key(&article.url) {
                return;
            }
        }
        let Some(llm) = self.llm.clone() else {
            return;
        };
        let state = self.state.clone();
        let url = article.url.clone();
        let title = article.title.clone();
        let raw = article.summary.clone().unwrap_or_default();
        {
            let mut st = self.state.lock().expect("news state poisoned");
            st.summaries.insert(url.clone(), SummaryState::Requested);
        }
        tokio::spawn(async move {
            let request = LlmRequest {
                model: None,
                system: Some(SUMMARY_SYSTEM_PROMPT.into()),
                messages: vec![LlmMessage {
                    role: Role::User,
                    content: format!(
                        "Title: {title}\nURL: {url}\nRaw excerpt: {}\n",
                        if raw.is_empty() { "(none)" } else { raw.as_str() }
                    ),
                }],
                max_tokens: 350,
                cache_system: true,
            };
            let outcome = match llm.complete(request).await {
                Ok(resp) => {
                    let text = resp.text.trim();
                    if is_insufficient_reply(text) {
                        // Model said it couldn't summarize — prefer the raw
                        // excerpt over the model's apology.
                        SummaryState::Failed
                    } else {
                        SummaryState::Ready(text.to_string())
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, url = %url, "LLM summarization failed");
                    SummaryState::Failed
                }
            };
            let mut st = state.lock().expect("news state poisoned");
            st.summaries.insert(url, outcome);
        });
    }

    /// Compose the *visible* tab list = static tabs from config + an
    /// extra `🔎 <query>` tab when a search filter is active. Callers walk
    /// this list to render the tab bar, count clickable widths, and resolve
    /// `active_filter_idx` to a label.
    fn visible_tabs(&self, search: Option<&SearchFilter>) -> Vec<String> {
        let mut tabs = self.filter_tabs.clone();
        if let Some(s) = search {
            tabs.push(format!("🔎 {}", s.query));
        }
        tabs
    }

    /// Snapshot of `visible_tabs` taken under the state lock. Used in
    /// render/mouse paths that already need the lock for other state.
    fn snapshot_visible_tabs(&self) -> Vec<String> {
        let st = self.state.lock().expect("news state poisoned");
        self.visible_tabs(st.search.as_ref())
    }

    fn cycle_filter(&mut self, forward: bool) {
        let mut st = self.state.lock().expect("news state poisoned");
        let tabs = self.visible_tabs(st.search.as_ref());
        if tabs.len() <= 1 {
            return;
        }
        let n = tabs.len();
        st.active_filter_idx = if forward {
            (st.active_filter_idx + 1) % n
        } else {
            (st.active_filter_idx + n - 1) % n
        };
        st.selected = 0;
        st.scroll = 0;
    }

    /// `:news <terms>` — replace any prior search with a new one and
    /// switch focus to the search tab. Empty `terms` falls back to
    /// `clear_search`.
    fn set_search(&mut self, raw: &str) {
        let Some(filter) = SearchFilter::new(raw) else {
            self.clear_search();
            return;
        };
        let mut st = self.state.lock().expect("news state poisoned");
        st.search = Some(filter);
        // Switch the active tab to the new (just-appended) search tab.
        st.active_filter_idx = self.filter_tabs.len();
        st.selected = 0;
        st.scroll = 0;
        st.expanded = false;
    }

    /// `x` or `:news` with no args — drop the search filter, snap back to
    /// the All tab. No-op when no search was active.
    fn clear_search(&mut self) {
        let mut st = self.state.lock().expect("news state poisoned");
        if st.search.take().is_some() {
            st.active_filter_idx = 0;
            st.selected = 0;
            st.scroll = 0;
            st.expanded = false;
        }
    }

    /// Mirrors the inner-area split used by `render`: tab bar on top (2 rows
    /// when topics exist, otherwise 1 for padding), single-row footer at the
    /// bottom, list fills the middle.
    fn split_inner(&self, inner: Rect) -> (Rect, Rect, Rect) {
        let has_tabs = self.snapshot_visible_tabs().len() > 1;
        let tab_height: u16 = if has_tabs { 2 } else { 1 };
        let footer_height = 1u16;
        let list_height = inner.height.saturating_sub(footer_height + tab_height);
        let tab_area = Rect::new(inner.x, inner.y, inner.width, tab_height);
        let list_area = Rect::new(inner.x, inner.y + tab_height, inner.width, list_height);
        let footer_area = Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(footer_height),
            inner.width,
            footer_height,
        );
        (tab_area, list_area, footer_area)
    }

    /// Reverse of the tab-bar render: leading space + `[label]` + space.
    fn tab_index_at(&self, click_col: u16, tab_area: Rect) -> Option<usize> {
        let tabs = self.snapshot_visible_tabs();
        let mut x: u16 = tab_area.x + 1; // leading space
        for (i, label) in tabs.iter().enumerate() {
            let w = label.chars().count() as u16 + 2; // [label]
            if click_col >= x && click_col < x + w {
                return Some(i);
            }
            x += w + 1; // single-space separator
            if x >= tab_area.x + tab_area.width {
                break;
            }
        }
        None
    }

    fn filtered_articles(&self) -> Vec<Article> {
        let st = self.state.lock().expect("news state poisoned");
        let active = st.active_filter_idx;
        let search_tab_idx = self.filter_tabs.len();

        // Search tab: rank by hit count desc, drop misses.
        if st.search.is_some() && active == search_tab_idx {
            let search = st.search.as_ref().expect("checked above");
            let mut scored: Vec<(usize, Article)> = st
                .articles
                .iter()
                .map(|a| (search.hit_count(a), a.clone()))
                .filter(|(n, _)| *n > 0)
                .collect();
            // Stable sort so equal-score articles keep recency order from
            // the underlying provider feed.
            scored.sort_by(|a, b| b.0.cmp(&a.0));
            return scored.into_iter().map(|(_, a)| a).collect();
        }

        // "All" tab → unfiltered.
        if active == 0 {
            return st.articles.clone();
        }
        // Topic tab (anything between All and the search tab).
        let Some(label) = self.filter_tabs.get(active) else {
            return st.articles.clone();
        };
        st.articles
            .iter()
            .filter(|a| a.topics.iter().any(|t| t == label))
            .cloned()
            .collect()
    }

    /// Walks the same per-item layout as `render` (2 rows base, +N when
    /// expanded) and returns the article index whose rows contain `click_row`.
    fn article_index_at(&self, click_row: u16, list_area: Rect, articles: &[Article]) -> Option<usize> {
        let st = self.state.lock().expect("news state poisoned");
        let mut scroll = st.scroll;
        let selected = st.selected;
        let expanded = st.expanded;
        drop(st);
        if expanded {
            scroll = selected;
        }
        let inner_width = list_area.width as usize;
        let mut y = list_area.y;
        for (i, article) in articles.iter().enumerate().skip(scroll) {
            let expand_this = i == selected && expanded;
            let summary_lines = if expand_this {
                article
                    .summary
                    .as_deref()
                    .map(|s| wrap_text(s, inner_width.saturating_sub(3), MAX_SUMMARY_LINES).len())
                    .unwrap_or(0) as u16
            } else {
                0
            };
            let item_height = 2u16 + summary_lines;
            if click_row >= y && click_row < y + item_height {
                return Some(i);
            }
            y = y.saturating_add(item_height);
            if y >= list_area.y + list_area.height {
                break;
            }
        }
        None
    }

    #[cfg(test)]
    fn active_filter_label(&self) -> String {
        let st = self.state.lock().expect("news state poisoned");
        let idx = st.active_filter_idx;
        let tabs = self.visible_tabs(st.search.as_ref());
        tabs.get(idx)
            .cloned()
            .unwrap_or_else(|| ALL_TAB_LABEL.to_string())
    }

    fn is_due(&self) -> bool {
        let st = self.state.lock().expect("news state poisoned");
        if st.inflight {
            return false;
        }
        match st.last_attempt {
            None => true,
            Some(t) => t.elapsed() >= self.poll_interval,
        }
    }

    fn spawn_refresh(&self) {
        if !self.feeds_configured {
            return;
        }
        // Snapshot the currently-selected article URL (resolved in the
        // *filtered* view) BEFORE marking inflight, so we can restore the
        // selection in the new filtered list after the fetch lands.
        let prev_url: Option<String> = {
            let filtered = self.filtered_articles();
            let st = self.state.lock().expect("news state poisoned");
            filtered.get(st.selected).map(|a| a.url.clone())
        };
        let active_label: Option<String> = {
            let st = self.state.lock().expect("news state poisoned");
            let idx = st.active_filter_idx;
            if idx == 0 {
                None
            } else {
                self.filter_tabs.get(idx).cloned()
            }
        };
        {
            let mut st = self.state.lock().expect("news state poisoned");
            st.inflight = true;
            st.last_attempt = Some(Instant::now());
        }
        let provider = self.provider.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            let result = provider.fetch().await;
            let mut st = state.lock().expect("news state poisoned");
            st.inflight = false;
            match result {
                Ok(articles) => {
                    st.articles = articles;
                    st.last_error = None;
                    // Look up the previously-selected URL in the NEW filtered
                    // view. If it's still there, snap selection back to it;
                    // otherwise reset to the top.
                    let new_idx = prev_url.as_ref().and_then(|url| match &active_label {
                        None => st.articles.iter().position(|a| &a.url == url),
                        Some(label) => st
                            .articles
                            .iter()
                            .filter(|a| a.topics.iter().any(|t| t == label))
                            .position(|a| &a.url == url),
                    });
                    if let Some(idx) = new_idx {
                        st.selected = idx;
                    } else {
                        st.selected = 0;
                        st.scroll = 0;
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, "news fetch failed");
                    st.last_error = Some(err.to_string());
                }
            }
        });
    }

    fn mark_dirty(&self) {
        let mut st = self.state.lock().expect("news state poisoned");
        st.last_attempt = None;
    }

    fn move_selection(&mut self, delta: isize) {
        // selected is an index into the *filtered* list (matching rendering
        // and click handling), so bounds-check against the filtered length.
        let filtered_len = self.filtered_articles().len();
        if filtered_len == 0 {
            return;
        }
        let mut st = self.state.lock().expect("news state poisoned");
        let new_idx = (st.selected as isize + delta).clamp(0, filtered_len as isize - 1);
        st.selected = new_idx as usize;
    }

    fn jump_to(&mut self, idx: usize) {
        let filtered_len = self.filtered_articles().len();
        if filtered_len == 0 {
            return;
        }
        let mut st = self.state.lock().expect("news state poisoned");
        st.selected = idx.min(filtered_len - 1);
    }

    fn open_selected(&self) {
        // selected is a filtered-list index; look the URL up in the same view
        // the user is seeing on screen.
        let filtered = self.filtered_articles();
        let url = {
            let st = self.state.lock().expect("news state poisoned");
            filtered.get(st.selected).map(|a| a.url.clone())
        };
        if let Some(url) = url {
            if let Err(err) = open::that(&url) {
                tracing::warn!(error = %err, url = %url, "failed to open article URL");
            }
        }
    }
}

/// Placeholder provider used when RssProvider construction fails so the
/// widget still renders cleanly.
struct EmptyProvider;

#[async_trait]
impl NewsProvider for EmptyProvider {
    async fn fetch(&self) -> Result<Vec<Article>> {
        Ok(Vec::new())
    }
    fn name(&self) -> &str {
        "empty"
    }
}

#[async_trait]
impl Widget for NewsWidget {
    fn id(&self) -> &str {
        &self.id
    }

    fn display_name(&self) -> &str {
        "News"
    }

    async fn update(&mut self, _ctx: &AppContext) -> Result<()> {
        if self.is_due() {
            self.spawn_refresh();
        }
        Ok(())
    }

    fn render(&self, frame: &mut Frame, area: Rect, focused: bool) {
        let (
            all_articles,
            selected,
            mut scroll,
            expanded,
            active_filter_idx,
            inflight,
            last_error,
            search,
        ) = {
            let st = self.state.lock().expect("news state poisoned");
            (
                st.articles.clone(),
                st.selected,
                st.scroll,
                st.expanded,
                st.active_filter_idx,
                st.inflight,
                st.last_error.clone(),
                st.search.clone(),
            )
        };

        let visible_tabs = self.visible_tabs(search.as_ref());
        let search_tab_idx = self.filter_tabs.len();
        // Apply the active filter. Tab 0 = All, the last tab when a search
        // is active = scored search results, anything between = topic match.
        let articles: Vec<Article> = if let Some(s) = search
            .as_ref()
            .filter(|_| active_filter_idx == search_tab_idx)
        {
            let mut scored: Vec<(usize, Article)> = all_articles
                .into_iter()
                .map(|a| (s.hit_count(&a), a))
                .filter(|(n, _)| *n > 0)
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0));
            scored.into_iter().map(|(_, a)| a).collect()
        } else if active_filter_idx == 0 {
            all_articles
        } else if let Some(label) = self.filter_tabs.get(active_filter_idx) {
            all_articles
                .into_iter()
                .filter(|a| a.topics.iter().any(|t| t == label))
                .collect()
        } else {
            all_articles
        };

        let title = if articles.is_empty() {
            "News".to_string()
        } else {
            format!("News — {} articles", articles.len())
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(self.theme.border_style(focused))
            .title(decorated_title_line(
                focused,
                &title,
                self.shortcut,
                self.theme.widget_title,
                self.theme.text_shortcut,
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Reserve a top tab-bar row (only when we have configured topics so
        // the user actually has something to filter on), a bottom footer row,
        // and a blank row between the tabs and the list.
        let has_tabs = visible_tabs.len() > 1;
        let tab_height: u16 = if has_tabs { 2 } else { 1 };
        let footer_height = 1u16;
        let list_height = inner.height.saturating_sub(footer_height + tab_height);
        let tab_area = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: tab_height,
        };
        let list_area = Rect {
            x: inner.x,
            y: inner.y + tab_height,
            width: inner.width,
            height: list_height,
        };
        let footer_area = Rect {
            x: inner.x,
            y: inner.y + inner.height.saturating_sub(footer_height),
            width: inner.width,
            height: footer_height,
        };

        // Render the tab bar.
        if has_tabs {
            let mut spans: Vec<Span<'_>> = Vec::with_capacity(visible_tabs.len() * 2);
            spans.push(Span::raw(" "));
            for (i, label) in visible_tabs.iter().enumerate() {
                let is_active = i == active_filter_idx;
                let style = if is_active {
                    // text.selected on the active tab so it matches the
                    // selected-headline color — "yellow-ish = active".
                    self.theme.text_selected
                } else {
                    self.theme.text_dim
                };
                spans.push(Span::styled(format!("[{label}]"), style));
                if i + 1 < visible_tabs.len() {
                    spans.push(Span::raw(" "));
                }
            }
            frame.render_widget(Paragraph::new(Line::from(spans)), tab_area);
        }

        if articles.is_empty() {
            let msg = if !self.feeds_configured {
                "No feeds configured. Edit ~/.config/glint/news.toml to add [[feeds]] entries."
            } else if inflight {
                "Loading news…"
            } else {
                last_error.as_deref().unwrap_or("Fetching first batch…")
            };
            let body = Paragraph::new(vec![Line::from(""), Line::from(msg.to_string())])
                .alignment(Alignment::Center);
            frame.render_widget(body, inner);
            return;
        }

        // Each article occupies two rows by default (title + dim metadata).
        // The selected article expands to (1 + 1 + up to MAX_SUMMARY_LINES)
        // when `expanded` is true.
        const ROWS_PER_ITEM: usize = 2;
        let items_visible = (list_height as usize / ROWS_PER_ITEM).max(1);
        if expanded {
            // Pin the expanded item to the top so its summary has room.
            scroll = selected;
        } else {
            if selected < scroll {
                scroll = selected;
            }
            if selected >= scroll + items_visible {
                scroll = selected + 1 - items_visible;
            }
        }

        let now = Utc::now();
        let inner_width = inner.width as usize;
        let mut lines: Vec<Line<'_>> = Vec::with_capacity(items_visible * ROWS_PER_ITEM);
        let mut rows_emitted: u16 = 0;
        for (i, article) in articles.iter().enumerate().skip(scroll) {
            let is_selected = i == selected;
            let expand_this = is_selected && expanded;

            // Trigger LLM summarization the first time an article is expanded.
            if expand_this {
                self.ensure_summary_requested(article);
            }

            // How many rows would this item consume?
            let summary_lines: Vec<String> = if expand_this {
                expanded_summary_lines(
                    article,
                    &self.state,
                    inner_width.saturating_sub(3),
                    self.llm_summarize_enabled && self.llm.is_some(),
                )
            } else {
                Vec::new()
            };
            let needed = ROWS_PER_ITEM as u16 + summary_lines.len() as u16;
            if rows_emitted + needed > list_height {
                break;
            }

            let prefix = if is_selected { "▸ " } else { "  " };
            let title_style = if is_selected {
                // `text.selected` from the active scheme — the selected
                // article should pop the same way as other selections
                // (e.g. the calendar's "[Today]" pill, stocks' active period).
                self.theme.text_selected
            } else if focused {
                // `text.focused` only while the widget itself is focused —
                // when focus moves away, the inactive cell stays calm with
                // default text styling.
                self.theme.text_focused
            } else {
                self.theme.text_brilliant
            };
            let title_room = inner_width.saturating_sub(2);
            lines.push(Line::from(vec![
                Span::styled(prefix, title_style),
                Span::styled(truncate(&article.title, title_room), title_style),
            ]));

            // Row 2: 3-space indent + dim metadata. When expanded we drop the
            // summary excerpt from this row (it has its own block underneath).
            let mut meta = format!("   {} · {}", age_label(now, article.published), article.source);
            if self.show_topic_labels && !article.topics.is_empty() {
                meta.push_str(&format!(" · [{}]", article.topics.join(",")));
            }
            if !expand_this {
                if let Some(summary) = article.summary.as_deref() {
                    meta.push_str(" · ");
                    meta.push_str(summary);
                }
            }
            let meta = truncate(&meta, inner_width.saturating_sub(1));
            lines.push(Line::from(Span::styled(
                meta,
                self.theme.text_dim,
            )));

            for sline in &summary_lines {
                lines.push(Line::from(Span::styled(
                    format!("   {sline}"),
                    Style::default(),
                )));
            }

            rows_emitted += needed;
        }
        frame.render_widget(Paragraph::new(lines), list_area);

        let footer = Paragraph::new(Line::from(Span::styled(
            "↑/↓ select · ←/→ filter · e expand · Enter open · g/G top/bot · r refresh",
            self.theme.text_dim,
        )))
        .alignment(Alignment::Right);
        frame.render_widget(footer, footer_area);

        // Persist scroll back to state.
        let mut st = self.state.lock().expect("news state poisoned");
        st.scroll = scroll;
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
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                EventResult::Handled
            }
            KeyCode::PageUp => {
                self.move_selection(-10);
                EventResult::Handled
            }
            KeyCode::PageDown => {
                self.move_selection(10);
                EventResult::Handled
            }
            KeyCode::Char('g') => {
                self.jump_to(0);
                EventResult::Handled
            }
            KeyCode::Char('G') => {
                self.jump_to(usize::MAX);
                EventResult::Handled
            }
            KeyCode::Enter => {
                self.open_selected();
                EventResult::Handled
            }
            KeyCode::Char('r') => {
                self.mark_dirty();
                EventResult::Handled
            }
            KeyCode::Char('e') => {
                let mut st = self.state.lock().expect("news state poisoned");
                if !st.articles.is_empty() {
                    st.expanded = !st.expanded;
                }
                EventResult::Handled
            }
            KeyCode::Char('[') | KeyCode::Left | KeyCode::Char('h') => {
                self.cycle_filter(false);
                EventResult::Handled
            }
            KeyCode::Char(']') | KeyCode::Right | KeyCode::Char('l') => {
                self.cycle_filter(true);
                EventResult::Handled
            }
            // `x` drops the `:news <terms>` search filter, snapping back
            // to the All tab. No-op when no search is active.
            KeyCode::Char('x') => {
                self.clear_search();
                EventResult::Handled
            }
            _ => EventResult::Ignored,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, area: Rect) -> EventResult {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.move_selection(-1);
                return EventResult::Handled;
            }
            MouseEventKind::ScrollDown => {
                self.move_selection(1);
                return EventResult::Handled;
            }
            // Horizontal scroll cycles the filter tabs (same as ←/→) only
            // when the user has opted into it via news.toml — trackpads
            // commonly fire sideways scroll accidentally.
            MouseEventKind::ScrollLeft if self.horizontal_scroll_filters => {
                self.cycle_filter(false);
                return EventResult::Handled;
            }
            MouseEventKind::ScrollRight if self.horizontal_scroll_filters => {
                self.cycle_filter(true);
                return EventResult::Handled;
            }
            MouseEventKind::Down(MouseButton::Left) => {}
            _ => return EventResult::Ignored,
        }
        if area.width < 2 || area.height < 2 {
            return EventResult::Ignored;
        }
        // Block::inner trims one row/col on each side for the border.
        let inner = Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2);
        let (tab_area, list_area, _footer_area) = self.split_inner(inner);

        // Tab bar click
        if tab_area.height > 0
            && mouse.row == tab_area.y
            && mouse.column >= tab_area.x
            && mouse.column < tab_area.x + tab_area.width
        {
            if let Some(idx) = self.tab_index_at(mouse.column, tab_area) {
                let mut st = self.state.lock().expect("news state poisoned");
                if st.active_filter_idx != idx {
                    st.active_filter_idx = idx;
                    st.selected = 0;
                    st.scroll = 0;
                }
                return EventResult::Handled;
            }
        }

        // Article list click
        if list_area.height > 0
            && mouse.row >= list_area.y
            && mouse.row < list_area.y + list_area.height
            && mouse.column >= list_area.x
            && mouse.column < list_area.x + list_area.width
        {
            let filtered = self.filtered_articles();
            if let Some(idx) = self.article_index_at(mouse.row, list_area, &filtered) {
                let mut st = self.state.lock().expect("news state poisoned");
                st.selected = idx;
                return EventResult::Handled;
            }
        }

        EventResult::Ignored
    }

    fn handle_command(&mut self, cmd: &str, args: &[&str]) -> Result<bool> {
        if cmd != "news" {
            return Ok(false);
        }
        let query = args.join(" ");
        let trimmed = query.trim();
        if trimmed.is_empty() {
            // Bare `:news` clears any active search and snaps to All.
            self.clear_search();
        } else {
            self.set_search(trimmed);
        }
        // Return Ok(true) so the dispatcher claims focus for us — the user
        // typing `:news climate` clearly wants to see news.
        Ok(true)
    }

    fn keybindings(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("↑ / ↓ / j / k", "select article"),
            ("← / → / [ / ] / h / l", "cycle filter tab"),
            ("PgUp / PgDn", "±10 articles"),
            ("g / G", "jump to top / bottom"),
            ("Enter", "open article URL in browser"),
            ("e", "expand selected article"),
            ("x", "clear :news <terms> search filter"),
            (":news <terms>", "filter articles by keyword (ranked by hits)"),
            ("r", "force refresh"),
        ]
    }

    fn config(&self) -> serde_json::Value {
        serde_json::json!({
            "poll_interval_secs": self.poll_interval.as_secs(),
        })
    }

    fn apply_config(&mut self, config: serde_json::Value) -> Result<()> {
        let new_config: NewsConfig =
            serde_json::from_value(config).context("invalid news config payload")?;
        // Preserve the active LLM provider + summarize flag across reloads —
        // those are app-level, not user-config-level. Same goes for the
        // app theme.
        let llm = self.llm.clone();
        let summarize = self.llm_summarize_enabled;
        let app_theme = self.app_theme.clone();
        *self = Self::with_config_and_llm(new_config, llm, summarize, app_theme);
        Ok(())
    }

    fn set_app_theme(&mut self, theme: Arc<Theme>) {
        self.theme = theme.with_overrides(&self.colors_override);
        self.app_theme = theme;
    }

    fn shortcut_preferences(&self) -> &[char] {
        &self.shortcut_prefs
    }

    fn set_shortcut(&mut self, shortcut: Option<char>) {
        self.shortcut = shortcut;
    }
}

fn truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        let mut out: String = chars.into_iter().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Threshold (in whitespace-separated words) below which an RSS excerpt is
/// considered too thin to display on its own; only then is the LLM consulted.
const RAW_SUMMARY_GOOD_ENOUGH_WORDS: usize = 15;

/// Heuristic: does this article's raw summary look thin enough that an LLM
/// summarization round-trip is worth it? Empty or stubby ("Read more…")
/// excerpts → yes; a substantive paragraph → no, just show the raw text.
fn needs_llm_summary(raw: Option<&str>) -> bool {
    match raw {
        None => true,
        Some(s) => s.split_whitespace().count() < RAW_SUMMARY_GOOD_ENOUGH_WORDS,
    }
}

/// Detects the canonical "I can't summarize this" sentence we asked the model
/// to emit when the input was too sparse. Match is case-insensitive and
/// tolerant of trailing punctuation / whitespace.
fn is_insufficient_reply(text: &str) -> bool {
    let lower = text.trim().to_lowercase();
    lower.starts_with("insufficient content to summarize")
        || lower.starts_with("insufficient information to summarize")
}

/// Returns the wrapped lines to render under an expanded article. Prefers an
/// LLM summary when one has come back; otherwise falls back to the wrapped
/// raw RSS excerpt. While the LLM call is in flight we show a "Summarizing…"
/// placeholder so the user has visual feedback.
fn expanded_summary_lines(
    article: &Article,
    state: &Arc<Mutex<NewsState>>,
    max_width: usize,
    llm_enabled: bool,
) -> Vec<String> {
    let summary_state = {
        let st = state.lock().expect("news state poisoned");
        st.summaries.get(&article.url).cloned()
    };
    let raw = article.summary.as_deref().unwrap_or("");
    let raw_lines = || wrap_text(raw, max_width, MAX_SUMMARY_LINES);

    if !llm_enabled {
        return raw_lines();
    }
    match summary_state {
        Some(SummaryState::Ready(text)) => wrap_text(&text, max_width, MAX_SUMMARY_LINES),
        Some(SummaryState::Requested) => {
            let mut out = vec!["Summarizing…".to_string()];
            if !raw.is_empty() {
                out.extend(wrap_text(raw, max_width, MAX_SUMMARY_LINES.saturating_sub(1)));
            }
            out
        }
        Some(SummaryState::Failed) | None => raw_lines(),
    }
}

/// Naive word-wrap: greedy line-fill at `max_width` columns, capped at
/// `max_lines`. Words longer than `max_width` are character-truncated. If the
/// text doesn't fully fit, the last emitted line ends in `…`.
fn wrap_text(text: &str, max_width: usize, max_lines: usize) -> Vec<String> {
    if max_width == 0 || max_lines == 0 {
        return Vec::new();
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut consumed = 0usize;
    for (i, word) in words.iter().enumerate() {
        let candidate_len = if current.is_empty() {
            word.chars().count()
        } else {
            current.chars().count() + 1 + word.chars().count()
        };
        if candidate_len <= max_width {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
            consumed = i + 1;
        } else if current.is_empty() {
            // Word longer than max_width on its own: char-truncate.
            let truncated: String = word.chars().take(max_width.saturating_sub(1)).collect();
            lines.push(format!("{truncated}…"));
            consumed = i + 1;
            if lines.len() == max_lines {
                return lines;
            }
        } else {
            lines.push(std::mem::take(&mut current));
            if lines.len() == max_lines {
                break;
            }
            current.push_str(word);
            consumed = i + 1;
        }
    }
    if !current.is_empty() && lines.len() < max_lines {
        lines.push(current);
    }
    if consumed < words.len() {
        if let Some(last) = lines.last_mut() {
            ellipsize_in_place(last, max_width);
        }
    }
    lines
}

fn ellipsize_in_place(s: &mut String, max_width: usize) {
    if s.chars().count() < max_width {
        s.push('…');
    } else if !s.ends_with('…') {
        let mut chars: Vec<char> = s.chars().collect();
        chars.pop();
        chars.push('…');
        *s = chars.into_iter().collect();
    }
}

fn age_label(now: chrono::DateTime<Utc>, published: chrono::DateTime<Utc>) -> String {
    let secs = now.signed_duration_since(published).num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else if secs < 86400 * 30 {
        format!("{}d", secs / 86400)
    } else {
        format!("{}mo", secs / (86400 * 30))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn article(url: &str, title: &str, secs_ago: i64) -> Article {
        Article {
            title: title.into(),
            url: url.into(),
            source: "TestFeed".into(),
            published: Utc::now() - chrono::Duration::seconds(secs_ago),
            summary: Some("a short summary".into()),
            topics: vec![],
        }
    }

    fn tagged_article(url: &str, title: &str, topics: &[&str]) -> Article {
        Article {
            title: title.into(),
            url: url.into(),
            source: "TestFeed".into(),
            published: Utc::now(),
            summary: Some("a short summary".into()),
            topics: topics.iter().map(|t| t.to_string()).collect(),
        }
    }

    #[test]
    fn move_selection_respects_active_filter_bounds() {
        let cfg = NewsConfig {
            topics: vec![provider::Topic {
                label: "Tech".into(),
                keywords: vec!["AI".into()],
            }],
            ..NewsConfig::default()
        };
        let mut w = NewsWidget::with_config(cfg);
        {
            let mut st = w.state.lock().unwrap();
            st.articles = vec![
                tagged_article("https://a", "Non-tech A", &[]),
                tagged_article("https://b", "Tech B", &["Tech"]),
                tagged_article("https://c", "Non-tech C", &[]),
                tagged_article("https://d", "Tech D", &["Tech"]),
                tagged_article("https://e", "Non-tech E", &[]),
            ];
            st.active_filter_idx = 1; // Tech (filter_tabs[1])
        }
        // Filter shows 2 articles (B, D). move_selection should clamp to 0..=1.
        w.move_selection(99);
        assert_eq!(w.state.lock().unwrap().selected, 1);
        w.move_selection(-99);
        assert_eq!(w.state.lock().unwrap().selected, 0);
    }

    #[test]
    fn move_selection_clamps_to_bounds() {
        let mut w = NewsWidget::with_config(NewsConfig::default());
        {
            let mut st = w.state.lock().unwrap();
            st.articles = vec![
                article("https://a", "A", 0),
                article("https://b", "B", 0),
                article("https://c", "C", 0),
            ];
        }
        w.move_selection(-5);
        assert_eq!(w.state.lock().unwrap().selected, 0);
        w.move_selection(99);
        assert_eq!(w.state.lock().unwrap().selected, 2);
    }

    #[test]
    fn jump_to_supports_top_and_bottom() {
        let mut w = NewsWidget::with_config(NewsConfig::default());
        {
            let mut st = w.state.lock().unwrap();
            st.articles = vec![
                article("https://a", "A", 0),
                article("https://b", "B", 0),
                article("https://c", "C", 0),
            ];
            st.selected = 1;
        }
        w.jump_to(0);
        assert_eq!(w.state.lock().unwrap().selected, 0);
        w.jump_to(usize::MAX);
        assert_eq!(w.state.lock().unwrap().selected, 2);
    }

    #[test]
    fn age_label_buckets() {
        let now = Utc::now();
        assert_eq!(age_label(now, now - chrono::Duration::seconds(30)), "30s");
        assert_eq!(age_label(now, now - chrono::Duration::seconds(120)), "2m");
        assert_eq!(age_label(now, now - chrono::Duration::seconds(7200)), "2h");
        assert_eq!(
            age_label(now, now - chrono::Duration::seconds(86400 * 3)),
            "3d"
        );
    }

    #[test]
    fn empty_feeds_is_visible_in_state() {
        let w = NewsWidget::with_config(NewsConfig::default());
        assert!(!w.feeds_configured);
    }

    #[test]
    fn needs_llm_summary_skips_substantial_excerpts() {
        // A real paragraph — no need for the LLM.
        let paragraph = "Apple today announced the iPhone 16 with a new A18 chip, \
            improved camera system, and a redesigned aluminium chassis available in \
            five colors. Pre-orders start Friday.";
        assert!(!needs_llm_summary(Some(paragraph)));
    }

    #[test]
    fn needs_llm_summary_kicks_in_for_thin_excerpts() {
        assert!(needs_llm_summary(None));
        assert!(needs_llm_summary(Some("")));
        assert!(needs_llm_summary(Some("Read more")));
        assert!(needs_llm_summary(Some("Apple announces new iPhone today.")));
    }

    #[test]
    fn is_insufficient_reply_recognizes_canonical_phrasings() {
        assert!(is_insufficient_reply("Insufficient content to summarize."));
        assert!(is_insufficient_reply("insufficient content to summarize"));
        assert!(is_insufficient_reply("  INSUFFICIENT CONTENT TO SUMMARIZE.  "));
        assert!(is_insufficient_reply(
            "Insufficient information to summarize this article."
        ));
        assert!(!is_insufficient_reply("Apple announced…"));
    }

    #[test]
    fn wrap_text_greedy_fills_within_width() {
        let out = wrap_text("the quick brown fox jumps over the lazy dog", 12, 5);
        // Expected greedy wrap: "the quick", "brown fox", "jumps over", "the lazy dog"
        assert_eq!(out, vec!["the quick", "brown fox", "jumps over", "the lazy dog"]);
    }

    #[test]
    fn wrap_text_caps_at_max_lines_and_ellipsizes() {
        let out = wrap_text("one two three four five six seven eight nine ten", 4, 3);
        assert_eq!(out.len(), 3);
        let last = out.last().unwrap();
        assert!(last.ends_with('…'), "last line should end in ellipsis: {last:?}");
    }

    #[test]
    fn wrap_text_truncates_oversized_single_words() {
        let out = wrap_text("supercalifragilistic", 10, 3);
        assert_eq!(out.len(), 1);
        assert!(out[0].ends_with('…'));
        assert!(out[0].chars().count() <= 10);
    }

    #[test]
    fn expand_key_toggles_expanded_state() {
        let mut w = NewsWidget::with_config(NewsConfig::default());
        {
            let mut st = w.state.lock().unwrap();
            st.articles = vec![article("https://a", "A", 0)];
        }
        let key = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE);
        assert_eq!(w.handle_key(key), EventResult::Handled);
        assert!(w.state.lock().unwrap().expanded);
        assert_eq!(w.handle_key(key), EventResult::Handled);
        assert!(!w.state.lock().unwrap().expanded);
    }

    #[test]
    fn cycle_filter_wraps_and_resets_selection() {
        let cfg = NewsConfig {
            topics: vec![
                provider::Topic {
                    label: "Tech".into(),
                    keywords: vec!["AI".into()],
                },
                provider::Topic {
                    label: "Finance".into(),
                    keywords: vec!["Fed".into()],
                },
            ],
            ..NewsConfig::default()
        };
        let mut w = NewsWidget::with_config(cfg);
        assert_eq!(w.filter_tabs, vec!["All", "Tech", "Finance"]);
        // Seed selection so we can verify the cycle resets it.
        {
            let mut st = w.state.lock().unwrap();
            st.articles = vec![article("https://a", "x", 0); 5];
            st.selected = 3;
        }
        w.cycle_filter(true);
        assert_eq!(w.active_filter_label(), "Tech");
        assert_eq!(w.state.lock().unwrap().selected, 0);
        w.cycle_filter(true);
        assert_eq!(w.active_filter_label(), "Finance");
        w.cycle_filter(true);
        assert_eq!(w.active_filter_label(), "All");
        w.cycle_filter(false);
        assert_eq!(w.active_filter_label(), "Finance");
    }

    #[test]
    fn cycle_filter_no_op_with_no_topics() {
        let mut w = NewsWidget::with_config(NewsConfig::default());
        assert_eq!(w.filter_tabs, vec!["All"]);
        w.cycle_filter(true);
        assert_eq!(w.active_filter_label(), "All");
    }

    #[test]
    fn tab_index_at_maps_columns_to_tabs() {
        let cfg = NewsConfig {
            topics: vec![
                provider::Topic { label: "Tech".into(), keywords: vec![] },
                provider::Topic { label: "World".into(), keywords: vec![] },
            ],
            ..NewsConfig::default()
        };
        let w = NewsWidget::with_config(cfg);
        // tabs render as: " [All] [Tech] [World]" starting at x=0
        //                  012345678901234567890123
        //                  [All] at 1..6, [Tech] at 7..13, [World] at 14..21
        let tab_area = Rect::new(0, 0, 40, 1);
        assert_eq!(w.tab_index_at(2, tab_area), Some(0));
        assert_eq!(w.tab_index_at(8, tab_area), Some(1));
        assert_eq!(w.tab_index_at(15, tab_area), Some(2));
        // click past the last tab → None
        assert_eq!(w.tab_index_at(30, tab_area), None);
    }

    #[test]
    fn article_index_at_maps_rows_in_compact_mode() {
        let w = NewsWidget::with_config(NewsConfig::default());
        {
            let mut st = w.state.lock().unwrap();
            st.articles = vec![
                article("https://a", "A", 0),
                article("https://b", "B", 0),
                article("https://c", "C", 0),
            ];
        }
        let articles = w.filtered_articles();
        let list_area = Rect::new(0, 5, 60, 10);
        // Each article = 2 rows starting at y=5: A=[5,6], B=[7,8], C=[9,10]
        assert_eq!(w.article_index_at(5, list_area, &articles), Some(0));
        assert_eq!(w.article_index_at(6, list_area, &articles), Some(0));
        assert_eq!(w.article_index_at(7, list_area, &articles), Some(1));
        assert_eq!(w.article_index_at(10, list_area, &articles), Some(2));
        assert_eq!(w.article_index_at(99, list_area, &articles), None);
    }

    #[test]
    fn expand_is_no_op_when_no_articles() {
        let mut w = NewsWidget::with_config(NewsConfig::default());
        let key = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE);
        w.handle_key(key);
        assert!(!w.state.lock().unwrap().expanded);
    }
}
