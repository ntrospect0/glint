// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

pub mod catalogue;
pub mod provider;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
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

use crate::cache::ScopedCache;
use crate::format::relative_time_label;
use crate::llm::{LlmMessage, LlmProvider, LlmRequest, Role};
use crate::text::{truncate, wrap};
use crate::theme::{ColorScheme, Theme};
use crate::ui::{apply_title_row, MetadataEmphasis};

use super::{AppContext, EventResult, Widget};

use provider::{Article, FeedConfig, NewsProvider, RssProvider, Topic};

#[derive(Debug, Clone)]
enum SummaryState {
    Requested,
    Ready(String),
    /// LLM call failed. The render path falls back to the raw RSS excerpt;
    /// the reason was logged via tracing.
    Failed,
}

/// Per-article state for the "fetch the full article body before
/// summarizing" path. Pressing `s` on an article whose feed has
/// `fetch_body` enabled walks: `Requested` → `Ready(plain_text)` (or
/// `Failed`); the LLM summary task is chained off the body's success
/// so it sees the extracted body instead of just the RSS excerpt.
#[derive(Debug, Clone)]
enum BodyState {
    Requested,
    Ready(String),
    /// HTTP fetch or readability extraction failed. The LLM path
    /// falls back to summarizing the RSS excerpt so the user still
    /// sees *something* rather than a hard error.
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

    /// Cycle filter tabs on horizontal scroll. Off by default — trackpad
    /// sideways gestures often fire accidentally while scrolling vertically.
    #[serde(default)]
    pub horizontal_scroll_filters: bool,

    /// Trail each meta row with detected topic labels (e.g. `[Business,World]`).
    #[serde(default = "default_show_topic_labels")]
    pub show_topic_labels: bool,

    /// Per-widget overrides layered on the app theme.
    #[serde(default)]
    pub colors: ColorScheme,

    /// `Shift+<letter>` focus shortcuts; falls back to `['n', 'e', 'w', 's']`.
    #[serde(default)]
    pub shortcuts: Vec<char>,

    /// On-demand article summarisation when an LLM provider is configured.
    /// Flip false to stay fully offline.
    #[serde(default = "default_summarize_with_llm")]
    pub summarize_with_llm: bool,

    /// Widget-wide default for "fetch the article HTML and extract the
    /// body before summarizing." Per-feed `fetch_body` in
    /// `[[feeds]]` blocks overrides this. `true` by default —
    /// summarizing the full article reads better than summarizing
    /// the (often near-empty) RSS excerpt. Flip false widget-wide
    /// for fully-offline use, or per-feed for sources whose feed
    /// excerpt is already the whole article (Phoronix, HN) or that
    /// paywall the article page (WSJ, NYT, FT).
    #[serde(default = "default_fetch_body_for_summary")]
    pub fetch_body_for_summary: bool,
}

fn default_summarize_with_llm() -> bool {
    true
}

fn default_fetch_body_for_summary() -> bool {
    true
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
            summarize_with_llm: default_summarize_with_llm(),
            fetch_body_for_summary: default_fetch_body_for_summary(),
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
    /// `Arc<Article>` (not raw `Article`) so the per-render
    /// `Vec::clone()` is O(N) atomic increments instead of O(N) deep
    /// Article copies. With a 100-item feed this drops 500-ish String
    /// allocations per render to ~100 atomic bumps. The Vec heap
    /// alloc itself is unavoidable.
    articles: Vec<Arc<Article>>,
    selected: usize,
    scroll: usize,
    /// When true, the selected article renders its full body (raw RSS
    /// excerpt or LLM summary, depending on `summary_view`) below its
    /// header. Toggled by `e` (expand/collapse) and `s` (which also
    /// expands if collapsed, like the email widget's `s`).
    expanded: bool,
    /// Per-article "prefer LLM summary over raw excerpt" toggle, keyed
    /// by URL. `s` flips it.
    summary_view: HashMap<String, bool>,
    /// Index into the *visible* tab list (static topic tabs + the dynamic
    /// search tab when one exists). 0 is always `All`.
    active_filter_idx: usize,
    last_error: Option<String>,
    poll: crate::polling::PollTracker,
    inflight: bool,
    /// Per-article LLM summarization state, keyed by article URL.
    summaries: HashMap<String, SummaryState>,
    /// Per-article HTTP body fetch state, keyed by URL. Filled
    /// lazily by `ensure_body_then_summary` and chained into the LLM
    /// task; `Failed` falls back to the RSS excerpt for the summary.
    bodies: HashMap<String, BodyState>,
    /// Active `:news <terms>` filter, if any. When present, an extra tab
    /// is appended to the tab bar and articles matching at least one term
    /// are surfaced (sorted by hit count). Cleared by `x` or `:news` with
    /// no args.
    search: Option<SearchFilter>,
    /// Display-state dirty bit drained by `take_dirty`. Set true by
    /// every async-task / tick-time mutation site so the main loop's
    /// dirty-flag gate triggers a redraw.
    dirty: bool,
}

const MAX_SUMMARY_LINES: usize = 6;
const ALL_TAB_LABEL: &str = "All";

const SUMMARY_SYSTEM_PROMPT: &str = "You are a concise news summarizer. \
Given a headline and an article body (or short excerpt when the body is \
unavailable), return a neutral 3-5 sentence summary capturing the key \
facts and any direct quotes. Do not editorialize, do not add preamble, \
do not use markdown. If the input is too sparse to summarize faithfully, \
respond with the single sentence: \"Insufficient content to summarize.\"";

const CACHE_KEY_ARTICLES: &str = "articles";

/// Cache-key namespace for LLM-generated article summaries. Each summary is
/// keyed by `summary-<short-sha256-of-url>` so URLs with query strings and
/// non-filesystem-safe characters round-trip cleanly. Summaries are
/// content-stable derivations of the article body — safe to persist across
/// restarts; orphaned entries (articles that fell out of the feed) age out
/// when the user clears the cache.
const SUMMARY_CACHE_PREFIX: &str = "summary-";

/// Cache-key namespace for extracted article bodies (the readable
/// plain-text we pass to the LLM instead of the RSS excerpt). Same
/// hashing approach as summaries — keys stay filesystem-safe across
/// URLs with query strings and unusual characters.
const BODY_CACHE_PREFIX: &str = "body-";

/// Cap on extracted body length sent to the LLM. Most news articles
/// are 2-6 KB of readable text; truncating long-form pieces at 30 KB
/// keeps the prompt under the cheap-tier model's context windows
/// while still giving the LLM far more substance than the RSS excerpt.
const MAX_BODY_BYTES: usize = 30_000;

/// Floor for accepting a Readability-extracted body as the real article.
/// Under this we assume extraction misfired (typically: a React-rendered
/// page where each paragraph is its own deeply-nested block, so the
/// density scorer picks one paragraph and stops). At that point we try
/// the `data-component="text-block"` fallback below before giving up.
/// 800 chars is roughly "more than a single quote, less than an article."
const MIN_EXTRACTED_BODY_BYTES: usize = 800;

/// HTTP timeout for fetching an article page. RSS feeds get 30s via the
/// shared client; article pages live behind more layers (Cloudflare,
/// rendering pipelines) so we give them headroom. Still tight enough
/// that a wedged origin doesn't pile up tasks.
const ARTICLE_FETCH_TIMEOUT: Duration = Duration::from_secs(20);

fn summary_cache_key(url: &str) -> String {
    crate::cache::short_hash_key(SUMMARY_CACHE_PREFIX, url)
}

fn body_cache_key(url: &str) -> String {
    crate::cache::short_hash_key(BODY_CACHE_PREFIX, url)
}

pub struct NewsWidget {
    id: String,
    instance: String,
    /// Cached `News` / `News (instance)` label so `display_name()` can hand
    /// out a `&str` without per-call allocation.
    display_name_cache: String,
    provider: Arc<dyn NewsProvider>,
    state: Arc<Mutex<NewsState>>,
    feeds_configured: bool,
    /// Persistent article cache so the first frame after launch can show the
    /// previous session's results while the network refresh runs in the
    /// background. Cloned into spawned tasks to persist newly fetched data.
    cache: ScopedCache,
    /// Tabs across the top of the cell. Index 0 is always `All`; the rest
    /// mirror the topic labels in news.toml.
    filter_tabs: Vec<String>,
    /// Optional LLM provider for on-demand article summarization.
    llm: Option<Arc<dyn LlmProvider>>,
    /// True when the user has opted into LLM news summaries via llm.toml.
    llm_summarize_enabled: bool,
    /// Widget-wide default for "fetch the article body before sending
    /// to the LLM." Per-feed `fetch_body` in `feed_configs` overrides.
    fetch_body_for_summary_default: bool,
    /// Copy of the configured feeds so per-feed lookups (currently:
    /// `fetch_body`) work after the original `Vec<FeedConfig>` has
    /// been moved into the provider. Cloned at construction; small
    /// (typically <30 entries).
    feed_configs: Vec<FeedConfig>,
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
        Self::with_config_and_llm(
            "main".to_string(),
            config,
            None,
            Arc::new(Theme::builtin_defaults()),
            ScopedCache::ephemeral(),
        )
    }

    pub fn with_config_and_llm(
        instance: String,
        config: NewsConfig,
        llm: Option<Arc<dyn LlmProvider>>,
        app_theme: Arc<Theme>,
        cache: ScopedCache,
    ) -> Self {
        let feeds_configured = !config.feeds.is_empty();
        let horizontal_scroll_filters = config.horizontal_scroll_filters;
        let show_topic_labels = config.show_topic_labels;
        let llm_summarize_enabled = config.summarize_with_llm;
        let fetch_body_for_summary_default = config.fetch_body_for_summary;
        let feed_configs = config.feeds.clone();
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
        let id = if instance == "main" {
            "news".to_string()
        } else {
            format!("news@{instance}")
        };
        let display_name_cache = if instance == "main" {
            "News".to_string()
        } else {
            format!("News ({instance})")
        };
        // Seed state from disk so the first render shows the previous run's
        // articles immediately. last_attempt is set from the cache timestamp
        // (translated to monotonic time) so the poll-interval gate naturally
        // suppresses a refetch when the cache is fresh.
        let poll_interval = Duration::from_secs(config.poll_interval_secs.max(60));
        let mut initial_state = NewsState::default();
        initial_state.poll = crate::polling::PollTracker::new(poll_interval);
        if let Some(entry) = cache.load::<Vec<Article>>(CACHE_KEY_ARTICLES) {
            initial_state.poll.seed_from_cache_age(entry.age());
            initial_state.articles = entry.value.into_iter().map(Arc::new).collect();
        }
        initial_state.poll.apply_jitter(&format!("news@{instance}"));

        Self {
            id,
            instance,
            display_name_cache,
            provider,
            state: Arc::new(Mutex::new(initial_state)),
            feeds_configured,
            cache,
            filter_tabs,
            llm,
            llm_summarize_enabled,
            fetch_body_for_summary_default,
            feed_configs,
            horizontal_scroll_filters,
            show_topic_labels,
            app_theme,
            colors_override,
            theme,
            shortcut: None,
            shortcut_prefs,
        }
    }

    /// Resolve the effective `fetch_body` policy for `article`. Looks
    /// up the article's source (the feed label) in `feed_configs`,
    /// returning the per-feed override if set, else the widget-wide
    /// default. Articles whose source doesn't match any configured
    /// feed (shouldn't happen in practice — that'd mean an article
    /// got into state from a deleted feed) fall back to the default.
    fn fetch_body_enabled_for(&self, article: &Article) -> bool {
        let feed = self.feed_configs.iter().find(|f| f.label == article.source);
        feed.and_then(|f| f.fetch_body)
            .unwrap_or(self.fetch_body_for_summary_default)
    }

    /// Kick off an LLM summary for `article`, using `body_override` as
    /// the input when present (e.g. when the body-fetch path extracted
    /// the full article), otherwise the RSS excerpt. Idempotent: hits
    /// in-memory state and on-disk cache before firing a new request.
    /// Failures aren't persisted — a retry after restart is cheap.
    fn ensure_summary_requested(&self, article: &Article, body_override: Option<String>) {
        if !self.llm_summarize_enabled {
            tracing::info!(
                url = %article.url,
                "news summary skipped: summarize_with_llm = false in news.toml"
            );
            return;
        }
        let Some(llm) = self.llm.clone() else {
            tracing::info!(
                url = %article.url,
                "news summary skipped: no LLM provider configured (check llm.toml)"
            );
            return;
        };
        spawn_summary_llm_task(
            llm,
            self.state.clone(),
            self.cache.clone(),
            article.title.clone(),
            article.url.clone(),
            body_override
                .filter(|b| !b.trim().is_empty())
                .unwrap_or_else(|| article.summary.clone().unwrap_or_default()),
        );
    }

    /// Two-step LLM summary path: HTTP-fetch the article page, extract
    /// the readable body with Mozilla's Readability port, then chain
    /// into the LLM call with that body as input. On any failure
    /// (fetch error, extraction blank, paywall page detected) falls
    /// back to summarizing the RSS excerpt — the user still gets
    /// *something*. Idempotent at the body layer (in-memory state +
    /// `body-<hash>` cache) just like the summary layer.
    fn ensure_body_then_summary(&self, article: &Article) {
        if !self.llm_summarize_enabled {
            return;
        }
        let Some(llm) = self.llm.clone() else {
            return;
        };
        // Check in-memory body state first.
        let body_state = {
            let st = self.state.lock().expect("news state poisoned");
            st.bodies.get(&article.url).cloned()
        };
        match body_state {
            Some(BodyState::Ready(text)) => {
                tracing::debug!(url = %article.url, "news body: already in memory; chaining to summary");
                self.ensure_summary_requested(article, Some(text));
                return;
            }
            Some(BodyState::Requested) => {
                tracing::debug!(url = %article.url, "news body: fetch already in flight; summary will chain on completion");
                return;
            }
            Some(BodyState::Failed) => {
                tracing::debug!(url = %article.url, "news body: previously failed; summarizing RSS excerpt");
                self.ensure_summary_requested(article, None);
                return;
            }
            None => {}
        }
        // Try the persisted body cache.
        if let Some(entry) = self.cache.load::<String>(&body_cache_key(&article.url)) {
            tracing::debug!(url = %article.url, "news body: hydrated from on-disk cache; chaining to summary");
            let body = entry.value;
            {
                let mut st = self.state.lock().expect("news state poisoned");
                st.bodies
                    .insert(article.url.clone(), BodyState::Ready(body.clone()));
            }
            self.ensure_summary_requested(article, Some(body));
            return;
        }
        // Fresh fetch + extract + chain.
        tracing::info!(
            url = %article.url,
            title = %article.title,
            "news body: spawning HTTP fetch + readability extract"
        );
        {
            let mut st = self.state.lock().expect("news state poisoned");
            st.bodies.insert(article.url.clone(), BodyState::Requested);
            st.dirty = true;
        }
        let url = article.url.clone();
        let title = article.title.clone();
        let rss_excerpt = article.summary.clone().unwrap_or_default();
        let state = self.state.clone();
        let cache = self.cache.clone();
        tokio::spawn(async move {
            let extracted = fetch_and_extract_body(&url).await;
            let (body_state_value, body_for_summary) = match extracted {
                Ok(text) => {
                    tracing::info!(
                        url = %url,
                        chars = text.chars().count(),
                        "news body: extracted; caching"
                    );
                    if let Err(err) = cache.store(&body_cache_key(&url), &text) {
                        tracing::warn!(error = %err, url = %url, "news body cache store failed");
                    }
                    (BodyState::Ready(text.clone()), Some(text))
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        url = %url,
                        "news body: fetch/extract failed; falling back to RSS excerpt for the summary"
                    );
                    (BodyState::Failed, None)
                }
            };
            {
                let mut st = state.lock().expect("news state poisoned");
                st.bodies.insert(url.clone(), body_state_value);
                st.dirty = true;
            }
            // Always chain to the summary task — body success uses the
            // extracted text, body failure falls back to the RSS excerpt.
            let summary_input = body_for_summary
                .filter(|b| !b.trim().is_empty())
                .unwrap_or(rss_excerpt);
            spawn_summary_llm_task(llm, state, cache, title, url, summary_input);
        });
    }

    /// Press-`s` entry point. Mirrors the email widget's Body ⇄ Summary
    /// toggle:
    /// - **Collapsed**: expand, switch to summary view, fire the LLM
    ///   request (or hydrate from cache).
    /// - **Expanded + Body**: switch to summary view; fire LLM if not
    ///   already cached or in flight.
    /// - **Expanded + Summary**: switch back to Body — no LLM hit,
    ///   the cached summary stays put for the next `s`.
    /// No-op when there are no articles or no LLM is configured.
    fn toggle_summary_view(&mut self) {
        if !self.llm_summarize_enabled {
            tracing::debug!("news `s` ignored: summarize_with_llm = false in news.toml");
            return;
        }
        if self.llm.is_none() {
            tracing::debug!("news `s` ignored: no LLM provider configured (check llm.toml)");
            return;
        }
        // `selected` is an index into the *filtered* view the user
        // is seeing on screen (per `active_filter_idx`) — not the
        // full article list. Reading `st.articles[selected]` here
        // looked up the wrong article on every non-All tab, so `s`
        // toggled summary_view and fired a body fetch against a URL
        // that wasn't even visible. Nothing changed in the rendered
        // row, so the keystroke looked like a no-op. `open_selected`
        // already routes through `filtered_articles()`; mirror it.
        let filtered = self.filtered_articles();
        let (article_url, will_show_summary) = {
            let mut st = self.state.lock().expect("news state poisoned");
            if filtered.is_empty() {
                tracing::debug!("news `s` ignored: no articles in current filter");
                return;
            }
            let url = match filtered.get(st.selected) {
                Some(a) => a.url.clone(),
                None => {
                    tracing::debug!(
                        selected = st.selected,
                        len = filtered.len(),
                        "news `s` ignored: selected index out of range for filter"
                    );
                    return;
                }
            };
            let was_collapsed = !st.expanded;
            let result = if was_collapsed {
                st.expanded = true;
                st.summary_view.insert(url.clone(), true);
                (url, true)
            } else {
                let new = !*st.summary_view.get(&url).unwrap_or(&false);
                st.summary_view.insert(url.clone(), new);
                (url, new)
            };
            tracing::info!(
                url = %result.0,
                was_collapsed,
                prefer_summary = result.1,
                "news `s` toggled"
            );
            result
        };
        // Fire the LLM request only when entering summary view —
        // staying on Body should never cost network or tokens. The
        // dispatch fork: if this article's feed has `fetch_body`
        // enabled, route through the body-fetch + chain path so the
        // LLM gets the full article; otherwise call the LLM directly
        // with the RSS excerpt.
        if will_show_summary {
            let article = {
                let st = self.state.lock().expect("news state poisoned");
                st.articles.iter().find(|a| a.url == article_url).cloned()
            };
            if let Some(article) = article {
                if self.fetch_body_enabled_for(&article) {
                    self.ensure_body_then_summary(&article);
                } else {
                    tracing::info!(
                        url = %article.url,
                        feed = %article.source,
                        "news: fetch_body disabled for this feed; LLM gets the RSS excerpt only"
                    );
                    self.ensure_summary_requested(&article, None);
                }
            }
        }
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

    fn filtered_articles(&self) -> Vec<Arc<Article>> {
        let st = self.state.lock().expect("news state poisoned");
        let active = st.active_filter_idx;
        let search_tab_idx = self.filter_tabs.len();

        // Search tab: rank by hit count desc, drop misses.
        if st.search.is_some() && active == search_tab_idx {
            let search = st.search.as_ref().expect("checked above");
            let mut scored: Vec<(usize, Arc<Article>)> = st
                .articles
                .iter()
                .map(|a| (search.hit_count(a), Arc::clone(a)))
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
    fn article_index_at(
        &self,
        click_row: u16,
        list_area: Rect,
        articles: &[Arc<Article>],
    ) -> Option<usize> {
        let st = self.state.lock().expect("news state poisoned");
        let scroll = st.scroll;
        let selected = st.selected;
        let expanded = st.expanded;
        drop(st);
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
        st.poll.is_due()
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
            st.poll.mark_attempted();
            st.dirty = true;
        }
        let provider = self.provider.clone();
        let state = self.state.clone();
        let cache = self.cache.clone();
        tokio::spawn(async move {
            let result = provider.fetch().await;
            let mut st = state.lock().expect("news state poisoned");
            st.inflight = false;
            st.dirty = true;
            match result {
                Ok(articles) => {
                    if let Err(err) = cache.store(CACHE_KEY_ARTICLES, &articles) {
                        tracing::warn!(error = %err, "news cache store failed");
                    }
                    st.articles = articles.into_iter().map(Arc::new).collect();
                    // Drop in-memory summaries for articles that rotated
                    // out of the feed. The summary text is already on
                    // disk via the scoped cache, so a rare re-encounter
                    // (e.g. the article comes back) just re-loads.
                    let live: std::collections::HashSet<String> =
                        st.articles.iter().map(|a| a.url.clone()).collect();
                    st.summaries.retain(|url, _| live.contains(url));
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
        st.poll.mark_dirty();
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
}

#[async_trait]
impl Widget for NewsWidget {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> &str {
        "news"
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

    fn take_dirty(&mut self) -> bool {
        let mut st = self.state.lock().expect("news state poisoned");
        std::mem::replace(&mut st.dirty, false)
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
        let articles: Vec<Arc<Article>> = if let Some(s) = search
            .as_ref()
            .filter(|_| active_filter_idx == search_tab_idx)
        {
            let mut scored: Vec<(usize, Arc<Article>)> = all_articles
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

        let metadata = if articles.is_empty() {
            None
        } else {
            Some(format!("{} articles", articles.len()))
        };
        let block = apply_title_row(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(self.theme.border_style(focused)),
            focused,
            &self.display_name_cache,
            metadata.as_deref(),
            MetadataEmphasis::Default,
            self.shortcut,
            &self.theme,
            area.width,
        );
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

        // title row + meta row; selected expands by wrapped summary lines on top.
        const ROWS_PER_ITEM: usize = 2;

        let now = Utc::now();
        let inner_width = inner.width as usize;

        // Resolve the selected expansion up front so the scroll math
        // below can factor its row count in.
        let selected_summary_lines: Vec<String> = if expanded && selected < articles.len() {
            let article = &articles[selected];
            expanded_summary_lines(
                article,
                &self.state,
                inner_width.saturating_sub(3),
                self.llm_summarize_enabled && self.llm.is_some(),
            )
        } else {
            Vec::new()
        };
        // `items_visible` is in articles (each takes ROWS_PER_ITEM rows);
        // expansion height is the only term measured in raw rows.
        let summary_rows = selected_summary_lines.len() as u16;
        let items_visible = (list_height / ROWS_PER_ITEM as u16).max(1) as usize;
        if selected < scroll {
            scroll = selected;
        }
        if selected >= scroll + items_visible {
            scroll = selected + 1 - items_visible;
        }
        if expanded {
            let max_items_with_expansion =
                (list_height.saturating_sub(summary_rows) / ROWS_PER_ITEM as u16).max(1) as usize;
            let want = (selected + 1).saturating_sub(max_items_with_expansion);
            scroll = scroll.max(want).min(selected);
        }

        let mut lines: Vec<Line<'_>> = Vec::with_capacity(list_height as usize);
        let mut rows_emitted: u16 = 0;
        for (i, article) in articles.iter().enumerate().skip(scroll) {
            let is_selected = i == selected;
            let expand_this = is_selected && expanded;

            let summary_lines: &[String] = if expand_this {
                &selected_summary_lines
            } else {
                &[]
            };
            let needed = ROWS_PER_ITEM as u16 + summary_lines.len() as u16;
            let rows_remaining = list_height.saturating_sub(rows_emitted);

            // Can't render anything useful without room for title+meta.
            if rows_remaining < ROWS_PER_ITEM as u16 {
                break;
            }
            // Non-expanded items either fit completely or stop the loop.
            // The selected expanded item is allowed to render its
            // title+meta plus a clipped tail of summary lines — better
            // than vanishing entirely if the expansion overruns the pane.
            if !expand_this && needed > rows_remaining {
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
            let mut meta = format!(
                "   {} · {}",
                age_label(now, article.published),
                article.source
            );
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
            lines.push(Line::from(Span::styled(meta, self.theme.text_dim)));

            let summary_room = rows_remaining.saturating_sub(ROWS_PER_ITEM as u16) as usize;
            let summary_to_render = summary_lines.len().min(summary_room);
            for sline in summary_lines.iter().take(summary_to_render) {
                lines.push(Line::from(Span::styled(
                    format!("   {sline}"),
                    Style::default(),
                )));
            }

            rows_emitted += ROWS_PER_ITEM as u16 + summary_to_render as u16;

            if expand_this && summary_to_render < summary_lines.len() {
                // Summary clipped at the pane bottom — no room for items below.
                break;
            }
        }
        frame.render_widget(Paragraph::new(lines), list_area);

        let footer = Paragraph::new(Line::from(Span::styled(
            "↑/↓ select · ←/→ filter · e/⏎ expand · o open · g/G top/bot · r refresh",
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
        // Uppercase ASCII letters are reserved for the app-wide
        // `Shift+<letter>` focus-jump dispatcher — never consume them here.
        // This is why jump-to-bottom is `End`, not the vim-style `G`.
        if let KeyCode::Char(c) = key.code {
            if c.is_ascii_uppercase() {
                return EventResult::Ignored;
            }
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
            KeyCode::Char('g') | KeyCode::Home => {
                self.jump_to(0);
                EventResult::Handled
            }
            KeyCode::End => {
                self.jump_to(usize::MAX);
                EventResult::Handled
            }
            // `o` opens the selected article in the browser. Enter
            // is reserved for the in-place primary action (expand
            // the article inline — falls through to the `e` branch
            // below).
            KeyCode::Char('o') => {
                self.open_selected();
                EventResult::Handled
            }
            KeyCode::Char('r') => {
                self.mark_dirty();
                EventResult::Handled
            }
            KeyCode::Enter | KeyCode::Char('e') => {
                let mut st = self.state.lock().expect("news state poisoned");
                if !st.articles.is_empty() {
                    st.expanded = !st.expanded;
                }
                EventResult::Handled
            }
            KeyCode::Char('s') => {
                self.toggle_summary_view();
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

        // Article list click. Clicking an unselected article selects it
        // and expands; clicking the currently selected article toggles its
        // expanded state — mirrors `e` on the keyboard.
        if list_area.height > 0
            && mouse.row >= list_area.y
            && mouse.row < list_area.y + list_area.height
            && mouse.column >= list_area.x
            && mouse.column < list_area.x + list_area.width
        {
            let filtered = self.filtered_articles();
            if let Some(idx) = self.article_index_at(mouse.row, list_area, &filtered) {
                let mut st = self.state.lock().expect("news state poisoned");
                if st.selected == idx {
                    st.expanded = !st.expanded;
                } else {
                    st.selected = idx;
                    st.expanded = true;
                }
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
            ("g / Home", "jump to top"),
            ("End", "jump to bottom"),
            ("o", "open article URL in browser"),
            ("Enter / e", "expand article inline"),
            ("e", "expand selected article (raw RSS excerpt)"),
            ("s", "toggle LLM summary for the expanded article"),
            ("x", "clear :news <terms> search filter"),
            (
                ":news <terms>",
                "filter articles by keyword (ranked by hits)",
            ),
            ("r", "force refresh"),
        ]
    }

    fn config(&self) -> serde_json::Value {
        let secs = self
            .state
            .lock()
            .expect("news state poisoned")
            .poll
            .interval()
            .as_secs();
        serde_json::json!({ "poll_interval_secs": secs })
    }

    fn apply_config(&mut self, config: serde_json::Value) -> Result<()> {
        let new_config: NewsConfig =
            serde_json::from_value(config).context("invalid news config payload")?;
        // App-level state (LLM provider, theme, cache, instance id) survives
        // a reload; everything else is rebuilt from the parsed TOML.
        let llm = self.llm.clone();
        let app_theme = self.app_theme.clone();
        let cache = self.cache.clone();
        let instance = self.instance.clone();
        *self = Self::with_config_and_llm(instance, new_config, llm, app_theme, cache);
        Ok(())
    }

    fn set_app_theme(&mut self, theme: Arc<Theme>) {
        self.theme = theme.with_overrides(&self.colors_override);
        self.app_theme = theme;
    }

    fn poll_snapshot(&self) -> Option<crate::polling::PollSnapshot> {
        Some(
            self.state
                .lock()
                .expect("news state poisoned")
                .poll
                .snapshot(),
        )
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
        // Same suffix the standalone news title shows after the kind
        // name (`News — 47 articles`). Returns None when there's no
        // article count yet (fresh launch before the first poll).
        let st = self.state.lock().expect("news state poisoned");
        let n = st.articles.len();
        if n == 0 {
            None
        } else {
            Some(format!("{n} articles"))
        }
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

/// Spawn the LLM summary task. Standalone (vs a method on `NewsWidget`)
/// so the body-fetch task can call it directly without juggling
/// `&self` lifetimes. Handles all the idempotency checks (in-memory
/// state, on-disk cache) and falls through to a tokio spawn for the
/// actual network call. `content` is whatever the caller decided to
/// feed the model — the extracted article body when available, the
/// RSS excerpt otherwise.
fn spawn_summary_llm_task(
    llm: Arc<dyn LlmProvider>,
    state: Arc<Mutex<NewsState>>,
    cache: ScopedCache,
    title: String,
    url: String,
    content: String,
) {
    // Short-circuit when we'd be summarizing nothing. The body-fetch
    // chain lands here with `content = rss_excerpt` after a body
    // extraction failure; for sources that ship no `<description>` in
    // RSS (Yahoo Finance) the excerpt is empty too, and the model
    // would just reply "Insufficient content to summarize." Spending a
    // round-trip on that produces a "Summarizing…" flicker followed
    // by a Failed state that the render path treats indistinguishably
    // from "never tried." Mark Failed up-front so the render branch
    // can show a distinct "couldn't extract" message instead.
    if content.trim().is_empty() {
        tracing::info!(
            url = %url,
            title = %title,
            "news summary: no body or excerpt available — marking Failed without LLM call"
        );
        let mut st = state.lock().expect("news state poisoned");
        st.summaries.insert(url, SummaryState::Failed);
        return;
    }
    // Idempotency: if we've already produced (or are producing, or
    // recently failed) a summary for this URL, don't fire again.
    {
        let st = state.lock().expect("news state poisoned");
        if let Some(existing) = st.summaries.get(&url) {
            tracing::info!(
                url = %url,
                state = %match existing {
                    SummaryState::Ready(_) => "ready",
                    SummaryState::Requested => "in-flight",
                    SummaryState::Failed => "failed",
                },
                "news summary already known — no new LLM call"
            );
            return;
        }
    }
    let cache_key = summary_cache_key(&url);
    if let Some(entry) = cache.load::<String>(&cache_key) {
        tracing::info!(url = %url, "news summary hydrated from on-disk cache");
        let mut st = state.lock().expect("news state poisoned");
        st.summaries
            .insert(url.clone(), SummaryState::Ready(entry.value));
        st.dirty = true;
        return;
    }
    tracing::info!(
        url = %url,
        title = %title,
        input_chars = content.chars().count(),
        "news summary firing LLM request"
    );
    {
        let mut st = state.lock().expect("news state poisoned");
        st.summaries.insert(url.clone(), SummaryState::Requested);
        st.dirty = true;
    }
    tokio::spawn(async move {
        let user_block = if content.trim().is_empty() {
            format!("Title: {title}\nURL: {url}\n\nContent:\n(no body or excerpt available)\n")
        } else {
            format!("Title: {title}\nURL: {url}\n\nContent:\n{content}\n")
        };
        let request = LlmRequest {
            model: None,
            system: Some(SUMMARY_SYSTEM_PROMPT.into()),
            messages: vec![LlmMessage {
                role: Role::User,
                content: user_block,
            }],
            max_tokens: 350,
            cache_system: true,
        };
        let outcome = match llm.complete(request).await {
            Ok(resp) => {
                let text = resp.text.trim();
                if is_insufficient_reply(text) {
                    tracing::info!(
                        url = %url,
                        reply = %text,
                        "news summary: LLM returned insufficient-content reply"
                    );
                    SummaryState::Failed
                } else {
                    tracing::info!(
                        url = %url,
                        chars = text.chars().count(),
                        "news summary: LLM returned summary, caching"
                    );
                    SummaryState::Ready(text.to_string())
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, url = %url, "news summary: LLM call failed");
                SummaryState::Failed
            }
        };
        if let SummaryState::Ready(text) = &outcome {
            if let Err(err) = cache.store(&cache_key, text) {
                tracing::warn!(error = %err, url = %url, "news summary cache store failed");
            }
        }
        let mut st = state.lock().expect("news state poisoned");
        st.summaries.insert(url, outcome);
        st.dirty = true;
    });
}

/// HTTP-fetch the article at `url` and extract its readable body via
/// the `readability` crate (Rust port of Mozilla's Readability.js).
/// Returns the extracted plain text on success, truncated to
/// `MAX_BODY_BYTES` so a single huge article can't swell the prompt
/// (and the LLM bill) without bound. Errors propagate normally —
/// callers fall back to the RSS excerpt at the layer above.
async fn fetch_and_extract_body(url: &str) -> Result<String> {
    let parsed = url::Url::parse(url).with_context(|| format!("invalid article URL: {url}"))?;
    let resp = crate::http::shared()
        .get(url)
        // Same browser-shaped headers we send for RSS — article pages
        // are at least as bot-gated as feed endpoints. Per-request so
        // these don't bleed into other widgets' HTTP traffic.
        .header(
            reqwest::header::USER_AGENT,
            concat!(
                "Mozilla/5.0 (compatible; glint-tui/",
                env!("CARGO_PKG_VERSION"),
                "; +https://github.com/ntrospect0/glint) Gecko/20100101 Firefox/120.0",
            ),
        )
        .header(
            reqwest::header::ACCEPT,
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
        .timeout(ARTICLE_FETCH_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("GET {url} failed"))?
        .error_for_status()
        .with_context(|| format!("{url} returned non-2xx"))?
        .bytes()
        .await
        .with_context(|| format!("reading body of {url} failed"))?;
    let html_bytes = resp.to_vec();
    let html_len = html_bytes.len();
    // Readability extraction is CPU-bound (html5ever DOM walk +
    // scoring heuristics); offload to a blocking-friendly task so we
    // don't block the tokio reactor on long documents. We hand the
    // raw HTML to the fallback extractor on the same thread so we
    // pay the UTF-8 conversion (and the marker-scan) once.
    let parsed_clone = parsed.clone();
    let extracted = tokio::task::spawn_blocking(move || {
        let raw_html = String::from_utf8_lossy(&html_bytes).into_owned();
        let readability_text = {
            let mut cursor = std::io::Cursor::new(html_bytes);
            readability::extractor::extract(&mut cursor, &parsed_clone)
                .map(|p| p.text)
                .unwrap_or_default()
        };
        let cleaned = normalize_extracted_text(readability_text);
        // Readability fell short — usually a React-rendered page where
        // each paragraph is its own `data-component="text-block"`
        // sibling and the density scorer picks just one. Try the
        // text-block extractor before bailing.
        if cleaned.len() < MIN_EXTRACTED_BODY_BYTES {
            if let Some(joined) = salvage_paragraphs(&raw_html) {
                let fallback = normalize_extracted_text(joined);
                if fallback.len() > cleaned.len() {
                    return fallback;
                }
            }
        }
        cleaned
    })
    .await
    .context("body extraction task panicked")?;
    let _ = parsed; // keep Url ownership tidy
    if extracted.trim().is_empty() {
        anyhow::bail!("body extraction returned empty text — likely a paywall or JS-rendered page");
    }
    if extracted.len() < MIN_EXTRACTED_BODY_BYTES && html_len > 20_000 {
        anyhow::bail!(
            "body extraction returned only {} chars from {} KB of HTML — \
             likely a layout we can't parse; falling back to RSS excerpt",
            extracted.len(),
            html_len / 1024,
        );
    }
    let mut text = extracted;
    if text.len() > MAX_BODY_BYTES {
        text.truncate(MAX_BODY_BYTES);
        // Don't sever a multi-byte UTF-8 sequence at the truncation
        // boundary.
        while !text.is_char_boundary(text.len()) {
            text.pop();
        }
    }
    Ok(text)
}

/// Trim trailing whitespace per line and collapse 3+ consecutive newlines
/// down to 2. Readability sometimes leaves blocky whitespace behind from
/// sectioning elements; the text-block fallback joins paragraphs with
/// `\n\n` already, so a final pass keeps both inputs uniform before they
/// reach the length floor and the LLM.
fn normalize_extracted_text(input: String) -> String {
    let mut text = input
        .lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n");
    while text.contains("\n\n\n") {
        text = text.replace("\n\n\n", "\n\n");
    }
    text.trim().to_string()
}

/// Best-effort fallback when Readability undershoots on a substantial
/// HTML page (most often a React/styled-components site where each
/// paragraph is its own deeply-nested DOM subtree, so the density
/// scorer picks one block and stops). Tries two strategies in order:
///
/// 1. **Marker-anchored**: known publishers (BBC News uses
///    `data-component="text-block"`) wrap each paragraph in an
///    attributed sibling div. Walking the markers and pulling the
///    inner `<p>` gives an exact article body with no caption /
///    pull-quote noise. Marker list is data-driven so adding the
///    next site we see is one entry.
/// 2. **Generic `<p>` salvage**: scan every `<p>` tag in the
///    document, sanitize, drop fragments under 40 chars (kills
///    nav-link UI text), and drop anything that looks like a
///    concatenated nav menu (very low whitespace ratio). Works on
///    any HTML where the article paragraphs are at least `<p>`-
///    shaped, even when there's no useful site-specific marker.
///
/// Whichever strategy yields more text wins — strategy 1 is usually
/// cleaner on sites it recognizes, but strategy 2 covers the long
/// tail. Hand-rolled string scanner rather than a DOM parse: we'd
/// otherwise be paying for a second html5ever walk on the same
/// document Readability just chewed through.
fn salvage_paragraphs(html: &str) -> Option<String> {
    let marker = extract_marker_anchored_paragraphs(html);
    let generic = extract_p_tag_paragraphs(html);
    match (marker, generic) {
        (Some(m), Some(g)) => Some(if g.len() > m.len() { g } else { m }),
        (Some(m), None) => Some(m),
        (None, Some(g)) => Some(g),
        (None, None) => None,
    }
}

/// Attribute substrings that publishers use to mark editorial paragraph
/// blocks. Each one identifies the *wrapper* of a single paragraph; the
/// extractor pulls the first `<p>` inside that wrapper and moves on to
/// the next sibling. Add new entries here as you encounter sites whose
/// React renderer breaks Readability — no other code changes required.
const PARAGRAPH_BLOCK_MARKERS: &[&str] = &[
    // BBC News / BBC Sport / BBC iPlayer all share this attribute on
    // their per-paragraph wrapper div.
    "data-component=\"text-block\"",
];

fn extract_marker_anchored_paragraphs(html: &str) -> Option<String> {
    for &marker in PARAGRAPH_BLOCK_MARKERS {
        if let Some(joined) = paragraphs_for_marker(html, marker) {
            return Some(joined);
        }
    }
    None
}

fn paragraphs_for_marker(html: &str, marker: &str) -> Option<String> {
    if !html.contains(marker) {
        return None;
    }
    let mut paragraphs: Vec<String> = Vec::new();
    let mut cursor = 0;
    while let Some(rel) = html[cursor..].find(marker) {
        let block_start = cursor + rel + marker.len();
        // Marker hits are siblings — bound the inner-<p> search by
        // the next marker so we never cross into the next block.
        let scope_end = html[block_start..]
            .find(marker)
            .map(|i| block_start + i)
            .unwrap_or(html.len());
        let scope = &html[block_start..scope_end];
        if let Some(p_open) = scope.find("<p") {
            if let Some(p_tag_end) = scope[p_open..].find('>').map(|i| p_open + i + 1) {
                if let Some(p_close) = scope[p_tag_end..].find("</p>").map(|i| p_tag_end + i) {
                    let cleaned = provider::sanitize_summary(&scope[p_tag_end..p_close]);
                    if !cleaned.is_empty() {
                        paragraphs.push(cleaned);
                    }
                }
            }
        }
        cursor = scope_end;
    }
    if paragraphs.is_empty() {
        None
    } else {
        Some(paragraphs.join("\n\n"))
    }
}

/// Pull every `<p>...</p>` from the document, filter out junk, return the
/// rest joined. The filters are deliberately coarse: real article paragraphs
/// average 100+ chars and 5-10 chars per whitespace boundary, while
/// nav/footer/menu content that leaks into a `<p>` is either much shorter
/// or much denser (camel-cased lists of links with no spaces between).
fn extract_p_tag_paragraphs(html: &str) -> Option<String> {
    const MIN_PARAGRAPH_LEN: usize = 40;
    let mut paragraphs: Vec<String> = Vec::new();
    let mut cursor = 0;
    while let Some(p_open_rel) = html[cursor..].find("<p") {
        let p_open = cursor + p_open_rel;
        // Disambiguate `<p>` from `<pre>`, `<picture>`, `<path>`, etc.
        // — require whitespace, `>`, or `/` after the `p`.
        let after_p = &html[p_open + 2..];
        let real_p = after_p
            .chars()
            .next()
            .is_some_and(|c| c.is_whitespace() || c == '>' || c == '/');
        if !real_p {
            cursor = p_open + 2;
            continue;
        }
        let p_tag_end = match html[p_open..].find('>').map(|i| p_open + i + 1) {
            Some(e) => e,
            None => break,
        };
        let p_close = match html[p_tag_end..].find("</p>").map(|i| p_tag_end + i) {
            Some(e) => e,
            None => break,
        };
        let cleaned = provider::sanitize_summary(&html[p_tag_end..p_close]);
        if cleaned.len() >= MIN_PARAGRAPH_LEN && !looks_like_navigation_run(&cleaned) {
            paragraphs.push(cleaned);
        }
        cursor = p_close + 4;
    }
    if paragraphs.is_empty() {
        None
    } else {
        Some(paragraphs.join("\n\n"))
    }
}

/// Detect concatenated nav-menu text masquerading as a paragraph. Real
/// prose averages roughly 5-8 chars between whitespace; menus lists
/// rendered as `<p>HomeNewsSportBusiness…</p>` push that ratio past 25
/// because the words run together. This catches BBC's footer/menu blob
/// that lands in a `<p>` after the article, and similar patterns on
/// other sites.
fn looks_like_navigation_run(text: &str) -> bool {
    let char_count = text.chars().count();
    if char_count == 0 {
        return false;
    }
    let space_count = text.chars().filter(|c| c.is_whitespace()).count();
    if space_count == 0 {
        return true;
    }
    char_count / space_count >= 25
}

/// Returns the wrapped lines to render under an expanded article.
/// Mirrors the email widget's two-mode model: `e` shows the raw RSS
/// excerpt by default, `s` toggles into LLM-summary view (lazily firing
/// the request the first time). `prefer_summary` lookups land here from
/// `state.summary_view[url]` so the per-article preference is the
/// single source of truth — render reads it, the `s` toggle writes it.
///
/// While an in-flight summary is loading we show "Summarizing…" plus
/// the raw excerpt as a placeholder so the user has visual feedback.
/// Failed summaries silently fall back to the raw excerpt — the model
/// already declined, so spamming an error is worse than just showing
/// the original paragraph.
fn expanded_summary_lines(
    article: &Article,
    state: &Arc<Mutex<NewsState>>,
    max_width: usize,
    llm_enabled: bool,
) -> Vec<String> {
    let (summary_state, body_state, prefer_summary) = {
        let st = state.lock().expect("news state poisoned");
        (
            st.summaries.get(&article.url).cloned(),
            st.bodies.get(&article.url).cloned(),
            *st.summary_view.get(&article.url).unwrap_or(&false),
        )
    };
    let raw = article.summary.as_deref().unwrap_or("").trim();
    // Render the body view. When the RSS feed didn't ship a
    // `<description>` (Yahoo Finance, some Atom feeds), the wrapped
    // excerpt is empty and the expansion would otherwise collapse to
    // zero rows — pressing `e` looks broken. Surface a placeholder
    // line so the user sees the toggle took effect, and point at the
    // `s` action when an LLM is configured so they have a path
    // forward.
    let raw_lines = || -> Vec<String> {
        if raw.is_empty() {
            if llm_enabled {
                vec!["(no excerpt — press s for an AI summary)".to_string()]
            } else {
                vec!["(no excerpt available — press `o` to open in browser)".to_string()]
            }
        } else {
            wrap_text(raw, max_width, MAX_SUMMARY_LINES)
        }
    };

    // Default view (`e` only, or `s` after toggling back to body): raw
    // excerpt. The user has to opt in to the LLM summary.
    if !llm_enabled || !prefer_summary {
        return raw_lines();
    }
    // Summary results take priority over body-fetch progress — once
    // the LLM has returned (Ready or Failed), the body's status is
    // immaterial to the displayed lines.
    match summary_state {
        Some(SummaryState::Ready(text)) => return wrap_text(&text, max_width, MAX_SUMMARY_LINES),
        Some(SummaryState::Requested) => {
            let mut out = vec!["Summarizing…".to_string()];
            if !raw.is_empty() {
                out.extend(wrap_text(
                    raw,
                    max_width,
                    MAX_SUMMARY_LINES.saturating_sub(1),
                ));
            }
            return out;
        }
        Some(SummaryState::Failed) => {
            // Distinct from the "never tried" placeholder when there's
            // no raw excerpt to fall back to — otherwise the user
            // can't tell that `s` even ran. With a real excerpt the
            // fallback is good content; show it as-is.
            if raw.is_empty() {
                return vec![
                    "(Couldn't extract article body — press `o` to open in browser)".to_string(),
                ];
            }
            return raw_lines();
        }
        None => {}
    }
    // No summary state yet — either the body fetch is still in flight
    // (show "Fetching article…") or summarization simply hasn't kicked
    // off (fall back to raw). The body-fetch task chains into the LLM
    // on completion, so this placeholder is short-lived.
    match body_state {
        Some(BodyState::Requested) => {
            let mut out = vec!["Fetching article…".to_string()];
            if !raw.is_empty() {
                out.extend(wrap_text(
                    raw,
                    max_width,
                    MAX_SUMMARY_LINES.saturating_sub(1),
                ));
            }
            out
        }
        _ => raw_lines(),
    }
}

/// Greedy word-wrap delegating to the shared [`crate::text::wrap`]
/// implementation. News article bodies don't carry paragraph
/// boundaries in their RSS excerpts, so we wrap with
/// `preserve_paragraphs = false`.
fn wrap_text(text: &str, max_width: usize, max_lines: usize) -> Vec<String> {
    wrap(text, max_width, max_lines, false)
}

/// Compact "how long ago" label for the article meta row. Delegates
/// to the shared [`crate::format::relative_time_label`] so age
/// formatting stays consistent across widgets.
fn age_label(now: chrono::DateTime<Utc>, published: chrono::DateTime<Utc>) -> String {
    relative_time_label(published, now)
}

pub const KIND: &str = "news";


/// Wizard descriptor. Surfaces a checkbox list of common feeds the user
/// can toggle, plus the four common scalar toggles. The feed catalogue
/// here is a curated subset; custom `[[feeds]]` blocks in news.toml
/// outside this list are preserved verbatim across `--setup` re-runs.
pub fn wizard_descriptor() -> crate::wizard::descriptor::WizardDescriptor {
    use crate::wizard::descriptor::{ChoiceOption, WizardDescriptor, WizardField, WizardFieldKind};
    // ChoiceOption holds `&'static str` for value/label; the catalogue
    // comes back from TOML as owned Strings, so leak once per wizard
    // invocation (cold-path, bounded by the catalogue size — fine).
    let cat = catalogue::load();
    let feed_options: Vec<ChoiceOption> = cat
        .feeds
        .iter()
        .map(|f| ChoiceOption {
            value: Box::leak(f.url.clone().into_boxed_str()),
            label: Box::leak(f.label.clone().into_boxed_str()),
            help: None,
        })
        .collect();
    let topic_options: Vec<ChoiceOption> = cat
        .topics
        .iter()
        .map(|t| ChoiceOption {
            value: Box::leak(t.label.clone().into_boxed_str()),
            label: Box::leak(t.label.clone().into_boxed_str()),
            help: None,
        })
        .collect();
    // A small starting set so a brand-new install sees something useful
    // immediately. Picked to span tech + world + markets.
    let default_feeds: Vec<&'static str> = vec![
        "https://hnrss.org/frontpage",
        "https://www.theverge.com/rss/index.xml",
        "http://feeds.bbci.co.uk/news/rss.xml",
        "https://finance.yahoo.com/news/rssindex",
    ];
    WizardDescriptor {
        display_name: "News",
        blurb: "RSS aggregator with topic filtering and optional LLM-generated \
                summaries. Tick the feeds you'd like; topic keywords + any \
                custom feeds you add by hand survive `--setup` re-runs.",
        load_from_toml: Some(load_news_from_toml),
        render_toml: Some(render_news_toml),
        fields: vec![
            WizardField {
                key: "topics",
                label: "Topic categories",
                help: "↑/↓ to move, Space toggles. Ticked categories \
                       become [[topics]] blocks in news.toml; articles \
                       matching any of a category's keywords get the \
                       label rendered in their meta row. Keyword lists \
                       live in news.toml — hand-edits survive re-runs.",
                required: false,
                kind: WizardFieldKind::MultiChoice {
                    options: topic_options,
                    defaults: vec!["Tech", "World", "Business"],
                },
                validate: None,
            },
            WizardField {
                key: "feeds",
                label: "Active news feeds",
                help: "↑/↓ to move, Space toggles. Custom RSS / Atom feeds \
                       you add by editing news.toml's [[feeds]] section will \
                       be preserved here even though they don't appear in \
                       this checkbox list.",
                required: false,
                kind: WizardFieldKind::MultiChoice {
                    options: feed_options,
                    defaults: default_feeds,
                },
                validate: None,
            },
            WizardField {
                key: "poll_interval_secs",
                label: "Feed refresh interval (seconds)",
                help: "How often to re-poll each RSS feed. 900s (15 min) is \
                       a polite default for free public feeds.",
                required: true,
                kind: WizardFieldKind::Number {
                    default: Some(900.0),
                    range: Some((60.0, 86_400.0)),
                    integer: true,
                },
                validate: None,
            },
            WizardField {
                key: "show_topic_labels",
                label: "Show topic labels on each article",
                help: "Adds `[Business,World]`-style tags to the meta row \
                       when a feed's keywords match. Quieter look without \
                       them; toggle freely after install.",
                required: false,
                kind: WizardFieldKind::Bool { default: true },
                validate: None,
            },
            WizardField {
                key: "summarize_with_llm",
                label: "Summarise expanded articles with LLM",
                help: "Requires the LLM provider you picked on the Global page \
                       to have its API key set. When off, glint stays fully \
                       offline and shows raw RSS excerpts.",
                required: false,
                kind: WizardFieldKind::Bool { default: true },
                validate: None,
            },
            WizardField {
                key: "horizontal_scroll_filters",
                label: "Horizontal scroll cycles filter tabs",
                help: "Off by default — trackpad sideways gestures often \
                       fire accidentally and you don't want them stealing \
                       focus mid-read.",
                required: false,
                kind: WizardFieldKind::Bool { default: false },
                validate: None,
            },
        ],
    }
}

/// Inverse of [`render_news_toml`]: pull the wizard-managed scalars and
/// derive the MultiChoice feed selection from any `[[feeds]]` whose URL
/// matches an entry in [`FEED_CATALOGUE`]. Custom feeds (URLs not in
/// the catalogue) are not surfaced to the wizard — they survive
/// untouched because the renderer preserves them verbatim.
fn load_news_from_toml(
    doc: &toml::Value,
) -> std::collections::HashMap<String, crate::wizard::descriptor::WizardValue> {
    use crate::wizard::descriptor::WizardValue;
    let mut out = std::collections::HashMap::new();
    if let Some(n) = doc.get("poll_interval_secs").and_then(|v| v.as_integer()) {
        out.insert("poll_interval_secs".into(), WizardValue::Number(n as f64));
    } else if let Some(f) = doc.get("poll_interval_secs").and_then(|v| v.as_float()) {
        out.insert("poll_interval_secs".into(), WizardValue::Number(f));
    }
    if let Some(b) = doc.get("show_topic_labels").and_then(|v| v.as_bool()) {
        out.insert("show_topic_labels".into(), WizardValue::Bool(b));
    }
    if let Some(b) = doc.get("summarize_with_llm").and_then(|v| v.as_bool()) {
        out.insert("summarize_with_llm".into(), WizardValue::Bool(b));
    }
    if let Some(b) = doc
        .get("horizontal_scroll_filters")
        .and_then(|v| v.as_bool())
    {
        out.insert("horizontal_scroll_filters".into(), WizardValue::Bool(b));
    }
    let cat = catalogue::load();
    if let Some(arr) = doc.get("feeds").and_then(|v| v.as_array()) {
        let catalogue_urls: std::collections::HashSet<&str> =
            cat.feeds.iter().map(|f| f.url.as_str()).collect();
        let selected: Vec<String> = arr
            .iter()
            .filter_map(|entry| entry.get("url").and_then(|v| v.as_str()))
            .filter(|url| catalogue_urls.contains(*url))
            .map(String::from)
            .collect();
        out.insert("feeds".into(), WizardValue::MultiChoice(selected));
    }
    if let Some(arr) = doc.get("topics").and_then(|v| v.as_array()) {
        let catalogue_labels: std::collections::HashSet<&str> =
            cat.topics.iter().map(|t| t.label.as_str()).collect();
        let selected: Vec<String> = arr
            .iter()
            .filter_map(|entry| entry.get("label").and_then(|v| v.as_str()))
            .filter(|label| catalogue_labels.contains(*label))
            .map(String::from)
            .collect();
        out.insert("topics".into(), WizardValue::MultiChoice(selected));
    }
    out
}

/// Render the news widget's TOML. We:
///   1. Compute the [[feeds]] set: catalogue selections from the
///      wizard, plus any custom feeds the user had in their existing
///      news.toml whose URLs aren't in the catalogue (so hand-curated
///      feeds survive `--setup` re-runs).
///   2. Take the existing file as the base — or `DEFAULT_NEWS_TOML`
///      on a fresh install — strip its [[feeds]] blocks, merge the
///      wizard's top-level scalars, then append the new [[feeds]]
///      list. [[topics]], [colors], shortcuts, and comments stay put.
fn render_news_toml(
    values: &std::collections::HashMap<String, crate::wizard::descriptor::WizardValue>,
    existing: Option<&str>,
) -> String {
    use crate::wizard::descriptor::WizardValue;

    let scalars: Vec<(&str, String)> = vec![
        (
            "poll_interval_secs",
            match values.get("poll_interval_secs") {
                Some(WizardValue::Number(n)) => format!("{}", *n as i64),
                _ => "900".into(),
            },
        ),
        (
            "show_topic_labels",
            match values.get("show_topic_labels") {
                Some(WizardValue::Bool(b)) => b.to_string(),
                _ => "true".into(),
            },
        ),
        (
            "summarize_with_llm",
            match values.get("summarize_with_llm") {
                Some(WizardValue::Bool(b)) => b.to_string(),
                _ => "true".into(),
            },
        ),
        (
            "horizontal_scroll_filters",
            match values.get("horizontal_scroll_filters") {
                Some(WizardValue::Bool(b)) => b.to_string(),
                _ => "false".into(),
            },
        ),
    ];

    let cat = catalogue::load();

    // Build the new [[feeds]] list.
    let selected_urls: Vec<&str> = match values.get("feeds") {
        Some(WizardValue::MultiChoice(items)) => items.iter().map(String::as_str).collect(),
        _ => Vec::new(),
    };
    let catalogue_urls: std::collections::HashSet<&str> =
        cat.feeds.iter().map(|f| f.url.as_str()).collect();
    let mut feed_blocks = String::new();
    let mut emitted_urls: std::collections::HashSet<String> = std::collections::HashSet::new();
    for url in &selected_urls {
        let Some(feed) = cat.feeds.iter().find(|f| f.url == *url) else {
            continue;
        };
        feed_blocks.push_str("\n[[feeds]]\n");
        feed_blocks.push_str(&format!("label = {}\n", toml_quote(&feed.label)));
        feed_blocks.push_str(&format!("url = {}\n", toml_quote(url)));
        emitted_urls.insert((*url).to_string());
    }
    // Carry forward any custom feeds (not in catalogue) the user has
    // on disk so a wizard re-run never silently deletes their work.
    if let Some(text) = existing {
        if let Ok(doc) = toml::from_str::<toml::Value>(text) {
            if let Some(arr) = doc.get("feeds").and_then(|v| v.as_array()) {
                for entry in arr {
                    let Some(url) = entry.get("url").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    if catalogue_urls.contains(url) || emitted_urls.contains(url) {
                        continue;
                    }
                    let label = entry.get("label").and_then(|v| v.as_str()).unwrap_or(url);
                    feed_blocks.push_str("\n[[feeds]]\n");
                    feed_blocks.push_str(&format!("label = {}\n", toml_quote(label)));
                    feed_blocks.push_str(&format!("url = {}\n", toml_quote(url)));
                    emitted_urls.insert(url.to_string());
                }
            }
        }
    }

    // Build the new [[topics]] list. Same preservation pattern as
    // feeds: selected catalogue entries (reusing existing keyword lists
    // when the same label was on disk), plus any custom topics whose
    // labels aren't in the catalogue.
    let selected_topics: Vec<&str> = match values.get("topics") {
        Some(WizardValue::MultiChoice(items)) => items.iter().map(String::as_str).collect(),
        _ => Vec::new(),
    };
    let catalogue_topic_labels: std::collections::HashSet<&str> =
        cat.topics.iter().map(|t| t.label.as_str()).collect();
    let existing_topics: std::collections::HashMap<String, Vec<String>> = existing
        .and_then(|t| toml::from_str::<toml::Value>(t).ok())
        .and_then(|doc| doc.get("topics").and_then(|v| v.as_array()).cloned())
        .map(|arr| {
            arr.into_iter()
                .filter_map(|entry| {
                    let label = entry.get("label")?.as_str()?.to_string();
                    let keywords = entry
                        .get("keywords")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    Some((label, keywords))
                })
                .collect()
        })
        .unwrap_or_default();

    let mut topic_blocks = String::new();
    let mut emitted_topic_labels: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for label in &selected_topics {
        let keywords: Vec<String> = if let Some(existing_kws) = existing_topics.get(*label) {
            existing_kws.clone()
        } else {
            // First time we've seen this topic — use the catalogue default.
            cat.topics
                .iter()
                .find(|t| t.label == *label)
                .map(|t| t.keywords.clone())
                .unwrap_or_default()
        };
        topic_blocks.push_str("\n[[topics]]\n");
        topic_blocks.push_str(&format!("label = {}\n", toml_quote(label)));
        let kw_list = keywords
            .iter()
            .map(|k| toml_quote(k))
            .collect::<Vec<_>>()
            .join(", ");
        topic_blocks.push_str(&format!("keywords = [{kw_list}]\n"));
        emitted_topic_labels.insert((*label).to_string());
    }
    // Preserve custom topics whose labels aren't in the catalogue.
    for (label, keywords) in &existing_topics {
        if catalogue_topic_labels.contains(label.as_str()) || emitted_topic_labels.contains(label) {
            continue;
        }
        topic_blocks.push_str("\n[[topics]]\n");
        topic_blocks.push_str(&format!("label = {}\n", toml_quote(label)));
        let kw_list = keywords
            .iter()
            .map(|k| toml_quote(k))
            .collect::<Vec<_>>()
            .join(", ");
        topic_blocks.push_str(&format!("keywords = [{kw_list}]\n"));
    }

    let base: std::borrow::Cow<str> = match existing {
        Some(text) => std::borrow::Cow::Borrowed(text),
        None => std::borrow::Cow::Borrowed(crate::config::DEFAULT_NEWS_TOML),
    };
    let stripped = crate::wizard::toml_merge::strip_array_of_tables_blocks(&base, "feeds");
    let stripped = crate::wizard::toml_merge::strip_array_of_tables_blocks(&stripped, "topics");
    let merged = crate::wizard::toml_merge::merge_top_level_scalars(&stripped, &scalars);

    // Append the new topics + feeds lists. Topics first (smaller; reads
    // like a config sidecar), then the larger feeds list.
    let mut out = merged;
    if !out.ends_with("\n\n") {
        if out.ends_with('\n') {
            out.push('\n');
        } else {
            out.push_str("\n\n");
        }
    }
    if !topic_blocks.is_empty() {
        out.push_str(topic_blocks.trim_start_matches('\n'));
        if !out.ends_with("\n\n") {
            out.push('\n');
        }
    }
    out.push_str(feed_blocks.trim_start_matches('\n'));
    out
}

fn toml_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn build(ctx: &super::WidgetCtx) -> Box<dyn super::Widget> {
    let cfg: NewsConfig =
        crate::config::load_widget_toml_for_instance(KIND, &ctx.instance).unwrap_or_default();
    Box::new(NewsWidget::with_config_and_llm(
        ctx.instance.clone(),
        cfg,
        ctx.llm.clone(),
        ctx.theme.clone(),
        ctx.cache.clone(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn article(url: &str, title: &str, secs_ago: i64) -> Arc<Article> {
        Arc::new(Article {
            title: title.into(),
            url: url.into(),
            source: "TestFeed".into(),
            published: Utc::now() - chrono::Duration::seconds(secs_ago),
            summary: Some("a short summary".into()),
            topics: vec![],
        })
    }

    fn tagged_article(url: &str, title: &str, topics: &[&str]) -> Arc<Article> {
        Arc::new(Article {
            title: title.into(),
            url: url.into(),
            source: "TestFeed".into(),
            published: Utc::now(),
            summary: Some("a short summary".into()),
            topics: topics.iter().map(|t| t.to_string()).collect(),
        })
    }

    #[test]
    fn salvage_paragraphs_returns_none_for_marker_only_pages_with_no_real_p() {
        // No marker, no <p> tags long enough to clear the salvage floor.
        let html = "<html><body><article><p>short.</p></article></body></html>";
        assert!(salvage_paragraphs(html).is_none());
    }

    #[test]
    fn salvage_paragraphs_handles_bbc_style_marker_blocks() {
        // Mimics BBC News: each paragraph wrapped in its own
        // `data-component="text-block"` sibling, with 3 levels of
        // styled-component divs in between. Marker-anchored strategy
        // should pull all three paragraphs out and skip the headline.
        let html = r#"
            <html><body>
            <div data-component="headline-block"><h1>Title</h1></div>
            <div data-component="text-block" class="x"><div class="y"><div class="z"><p class="q">First paragraph of the article.</p></div></div></div>
            <div data-component="text-block" class="x"><div class="y"><div class="z"><p class="q">Second &amp; deeper paragraph.</p></div></div></div>
            <div data-component="text-block" class="x"><div class="y"><div class="z"><p class="q">Third with <a href="/foo">a link</a> inside.</p></div></div></div>
            </body></html>
        "#;
        let out = salvage_paragraphs(html).expect("should extract");
        assert!(out.contains("First paragraph of the article."));
        assert!(out.contains("Second & deeper paragraph."));
        assert!(out.contains("Third with a link inside."));
        assert!(out.contains("\n\n"));
    }

    #[test]
    fn salvage_paragraphs_falls_back_to_p_tags_without_known_markers() {
        // No publisher-specific markers — just plain <p> tags scattered
        // through a React-shaped DOM. Generic-salvage strategy should
        // catch the editorial content while dropping short UI strings
        // and the run-on footer-menu nav blob.
        let html = r#"
            <html><body>
            <nav><p>Home</p><p>About</p></nav>
            <main>
              <div><div><p>This is a substantial article paragraph with several words and punctuation, so it clears the salvage filters easily.</p></div></div>
              <div><div><p>Another reasonably long paragraph that should be preserved verbatim and joined with the first one for the LLM.</p></div></div>
            </main>
            <footer><p>HomeNewsSportBusinessTechnologyHealthCultureWeatherShopBritBoxBBCInOtherLanguages</p></footer>
            </body></html>
        "#;
        let out = salvage_paragraphs(html).expect("should extract");
        assert!(out.contains("substantial article paragraph"));
        assert!(out.contains("Another reasonably long paragraph"));
        // Short UI <p>s (Home, About) filtered by length floor.
        assert!(!out.contains("Home\n") && !out.starts_with("Home"));
        // Concatenated menu blob filtered by the nav-run heuristic.
        assert!(!out.contains("HomeNewsSportBusiness"));
    }

    #[test]
    fn looks_like_navigation_run_flags_concatenated_menus_but_not_prose() {
        assert!(looks_like_navigation_run(
            "HomeNewsSportBusinessTechnologyHealthCulture"
        ));
        assert!(!looks_like_navigation_run(
            "The president signed the bill on Tuesday, citing record support across both chambers."
        ));
        // Mixed: nav blob with a couple of trailing words. Still flags
        // because the per-space ratio is dominated by the run-on.
        assert!(looks_like_navigation_run(
            "HomeNewsSportBusinessTechnologyHealthCultureArtsTravel News"
        ));
    }

    #[test]
    fn normalize_extracted_text_collapses_blank_runs_and_trims() {
        // Trailing whitespace per line is removed, runs of 3+ newlines
        // collapse to 2, and leading/trailing whitespace on the whole
        // string is trimmed. Per-line leading whitespace is intentionally
        // preserved (Readability sometimes uses it for list indentation).
        let raw = "Para one.   \n\n\n\nPara two.  \n\n\nPara three.\n\n".to_string();
        let out = normalize_extracted_text(raw);
        assert_eq!(out, "Para one.\n\nPara two.\n\nPara three.");
    }

    /// `s` on a non-All filter tab used to read `st.articles[selected]`,
    /// but `selected` indexes into the *filtered* view — so the URL we
    /// pinned, the body we fetched, and the summary we requested were
    /// all for some other article (the Nth in the full list). The
    /// visible row never updated and `s` looked dead. Verify that
    /// after `s` fires on the Tech tab, `summary_view` carries the
    /// URL of the *visible filtered* article, not the underlying
    /// full-list article at the same index.
    #[tokio::test]
    async fn toggle_summary_uses_filtered_article_not_full_list_index() {
        use std::sync::atomic::AtomicUsize;

        // Fake LLM — must exist for `toggle_summary_view` to proceed
        // past the `self.llm.is_none()` bail. It won't actually be
        // called in this test because we only check sync state.
        struct UnusedLlm {
            _calls: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl LlmProvider for UnusedLlm {
            async fn complete(&self, _request: LlmRequest) -> Result<crate::llm::LlmResponse> {
                unreachable!("LLM should not be called during this test")
            }
        }
        let cfg = NewsConfig {
            topics: vec![provider::Topic {
                label: "Tech".into(),
                keywords: vec!["AI".into()],
            }],
            ..NewsConfig::default()
        };
        let llm: Arc<dyn LlmProvider> = Arc::new(UnusedLlm {
            _calls: Arc::new(AtomicUsize::new(0)),
        });
        let mut w = NewsWidget::with_config_and_llm(
            "main".into(),
            cfg,
            Some(llm),
            Arc::new(Theme::builtin_defaults()),
            ScopedCache::ephemeral(),
        );
        {
            let mut st = w.state.lock().unwrap();
            st.articles = vec![
                tagged_article("https://full-0", "Non-tech 0", &[]),
                tagged_article("https://full-1-tech", "Tech 1", &["Tech"]),
                tagged_article("https://full-2", "Non-tech 2", &[]),
                tagged_article("https://full-3-tech", "Tech 3", &["Tech"]),
                tagged_article("https://full-4", "Non-tech 4", &[]),
            ];
            // Tech tab → filtered list is [full-1-tech, full-3-tech].
            // selected = 0 → filtered first item = full-1-tech.
            // The OLD bug would read st.articles[0] = full-0 (non-tech).
            st.active_filter_idx = 1;
            st.selected = 0;
        }
        w.toggle_summary_view();
        let st = w.state.lock().unwrap();
        assert!(
            st.summary_view.contains_key("https://full-1-tech"),
            "summary_view should be keyed by the filtered article's URL, got: {:?}",
            st.summary_view.keys().collect::<Vec<_>>()
        );
        assert!(
            !st.summary_view.contains_key("https://full-0"),
            "must NOT mutate the underlying full-list article at the same index"
        );
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
        // Delegates to `format::relative_time_label`, which buckets
        // sub-minute as "now" (minute-resolution suffices for news
        // article timestamps — see SDK doc § Formatting).
        let now = Utc::now();
        assert_eq!(age_label(now, now - chrono::Duration::seconds(30)), "now");
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
    fn is_insufficient_reply_recognizes_canonical_phrasings() {
        assert!(is_insufficient_reply("Insufficient content to summarize."));
        assert!(is_insufficient_reply("insufficient content to summarize"));
        assert!(is_insufficient_reply(
            "  INSUFFICIENT CONTENT TO SUMMARIZE.  "
        ));
        assert!(is_insufficient_reply(
            "Insufficient information to summarize this article."
        ));
        assert!(!is_insufficient_reply("Apple announced…"));
    }

    #[test]
    fn wrap_text_greedy_fills_within_width() {
        let out = wrap_text("the quick brown fox jumps over the lazy dog", 12, 5);
        // Expected greedy wrap: "the quick", "brown fox", "jumps over", "the lazy dog"
        assert_eq!(
            out,
            vec!["the quick", "brown fox", "jumps over", "the lazy dog"]
        );
    }

    #[test]
    fn wrap_text_caps_at_max_lines_and_ellipsizes() {
        let out = wrap_text("one two three four five six seven eight nine ten", 4, 3);
        assert_eq!(out.len(), 3);
        let last = out.last().unwrap();
        assert!(
            last.ends_with('…'),
            "last line should end in ellipsis: {last:?}"
        );
    }

    #[test]
    fn wrap_text_breaks_oversized_single_words_across_lines() {
        // The shared `text::wrap` is lossless on oversized words: it
        // mid-breaks rather than truncating with `…`, so the reader
        // can see the whole word across lines. A previous local
        // implementation truncated; the converged behaviour is
        // strictly more informative.
        let out = wrap_text("supercalifragilistic", 10, 3);
        assert!(out.len() >= 2);
        for line in &out {
            assert!(line.chars().count() <= 10);
        }
        // The full word survives (concatenation reproduces it).
        let joined: String = out.iter().flat_map(|l| l.chars()).collect();
        assert!(joined.contains("supercalifragilistic"));
    }

    /// Yahoo Finance + some Atom feeds ship items without a
    /// `<description>` element, so `article.summary` is `None`. Without
    /// a placeholder, `wrap_text("", …)` returns an empty Vec and the
    /// expanded row count drops to zero — pressing `e` looks broken.
    /// Surface the `s` hint when an LLM is configured so the user has
    /// a path to actual content.
    #[test]
    fn expanded_summary_lines_shows_llm_hint_when_excerpt_empty() {
        let article = Article {
            title: "no-desc".into(),
            url: "https://example/no-desc".into(),
            source: "TestFeed".into(),
            published: Utc::now(),
            summary: None,
            topics: vec![],
        };
        let state = Arc::new(Mutex::new(NewsState::default()));
        let lines = expanded_summary_lines(&article, &state, 80, true);
        assert_eq!(lines.len(), 1, "must render exactly one placeholder line");
        assert!(
            lines[0].contains("press s"),
            "should point at the `s` action: {lines:?}"
        );
    }

    /// Same situation with no LLM configured: still show a placeholder,
    /// but don't promise an AI summary that can't be delivered. Redirect
    /// to Enter (browser open) as the only meaningful action.
    #[test]
    fn expanded_summary_lines_shows_browser_hint_when_no_llm() {
        let article = Article {
            title: "no-desc".into(),
            url: "https://example/no-desc".into(),
            source: "TestFeed".into(),
            published: Utc::now(),
            summary: Some("   \n  \t ".into()), // whitespace-only also counts as empty
            topics: vec![],
        };
        let state = Arc::new(Mutex::new(NewsState::default()));
        let lines = expanded_summary_lines(&article, &state, 80, false);
        assert_eq!(lines.len(), 1);
        // Hint points at the `o` action — the platform convention
        // for "open externally" (was Enter before; Enter is now the
        // in-place expand binding).
        assert!(
            lines[0].contains("`o`"),
            "should point at the `o` action: {lines:?}"
        );
    }

    /// After `s` runs the body fetch + LLM and both come up empty
    /// (Yahoo Finance: no Readability/salvage match + no RSS excerpt),
    /// `summary_state = Failed`. The render path used to fall back to
    /// `raw_lines()`, which produced the SAME placeholder shown before
    /// `s` was pressed — indistinguishable from "never tried." Now
    /// the empty-raw + Failed combination renders a distinct
    /// couldn't-extract message so the user knows the action ran.
    #[test]
    fn expanded_summary_lines_failed_with_empty_raw_shows_couldnt_extract() {
        let article = Article {
            title: "no-desc".into(),
            url: "https://example/empty".into(),
            source: "TestFeed".into(),
            published: Utc::now(),
            summary: None,
            topics: vec![],
        };
        let state = Arc::new(Mutex::new(NewsState::default()));
        {
            let mut st = state.lock().unwrap();
            st.summaries
                .insert(article.url.clone(), SummaryState::Failed);
            st.summary_view.insert(article.url.clone(), true);
        }
        let lines = expanded_summary_lines(&article, &state, 80, true);
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("Couldn't extract"),
            "should show couldn't-extract message: {lines:?}"
        );
    }

    /// Articles with a real excerpt still surface that excerpt on
    /// Failed — losing it would regress vs. the pre-LLM behavior.
    #[test]
    fn expanded_summary_lines_failed_with_real_raw_falls_back_to_excerpt() {
        let article = Article {
            title: "with-desc".into(),
            url: "https://example/with-desc".into(),
            source: "TestFeed".into(),
            published: Utc::now(),
            summary: Some("Real article excerpt content goes here.".into()),
            topics: vec![],
        };
        let state = Arc::new(Mutex::new(NewsState::default()));
        {
            let mut st = state.lock().unwrap();
            st.summaries
                .insert(article.url.clone(), SummaryState::Failed);
            st.summary_view.insert(article.url.clone(), true);
        }
        let lines = expanded_summary_lines(&article, &state, 80, true);
        assert!(!lines.is_empty());
        assert!(
            lines[0].starts_with("Real"),
            "should still show raw excerpt when present: {lines:?}"
        );
    }

    /// Empty content must NOT hit the LLM. The body-fetch chain
    /// passes the RSS excerpt as fallback after extraction failure;
    /// when both are empty, spawning the LLM produces a visible
    /// "Summarizing…" flicker and burns a request to get back
    /// "Insufficient content to summarize." Short-circuit to Failed
    /// instead — the render path picks up the distinct empty-Failed
    /// rendering.
    #[tokio::test]
    async fn spawn_summary_llm_task_skips_llm_when_content_empty() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct PanickingLlm {
            calls: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl LlmProvider for PanickingLlm {
            async fn complete(&self, _request: LlmRequest) -> Result<crate::llm::LlmResponse> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                panic!("LLM should not be called for empty content");
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let llm: Arc<dyn LlmProvider> = Arc::new(PanickingLlm {
            calls: calls.clone(),
        });
        let state = Arc::new(Mutex::new(NewsState::default()));
        let cache = ScopedCache::ephemeral();
        let url = "https://example/empty".to_string();
        spawn_summary_llm_task(
            llm,
            state.clone(),
            cache,
            "title".into(),
            url.clone(),
            "   \n\t  ".into(), // whitespace-only also counts as empty
        );
        // Yield once so any (unexpected) tokio task gets a chance to run.
        tokio::task::yield_now().await;
        let st = state.lock().unwrap();
        assert!(
            matches!(st.summaries.get(&url), Some(SummaryState::Failed)),
            "must mark Failed up-front, got: {:?}",
            st.summaries.get(&url)
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "LLM must not be called for empty content"
        );
    }

    /// Articles with a real excerpt still render the wrapped text —
    /// the empty-summary placeholder mustn't hijack the normal path.
    #[test]
    fn expanded_summary_lines_renders_excerpt_normally_when_present() {
        let article = Article {
            title: "with-desc".into(),
            url: "https://example/with-desc".into(),
            source: "TestFeed".into(),
            published: Utc::now(),
            summary: Some("This is a real article excerpt that should wrap.".into()),
            topics: vec![],
        };
        let state = Arc::new(Mutex::new(NewsState::default()));
        let lines = expanded_summary_lines(&article, &state, 80, true);
        assert!(!lines.is_empty());
        assert!(
            lines[0].starts_with("This is"),
            "should show the real excerpt, not a placeholder: {lines:?}"
        );
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
                provider::Topic {
                    label: "Tech".into(),
                    keywords: vec![],
                },
                provider::Topic {
                    label: "World".into(),
                    keywords: vec![],
                },
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
