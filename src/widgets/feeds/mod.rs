// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Feeds widget — tabbed single-source RSS reader.
//!
//! One widget kind, many instances. Each instance picks a built-in
//! source preset (`source = "wsj"` / `"barrons"` / `"marketwatch"`)
//! which supplies the catalogue of `(topic, URL)` pairs, the
//! default display name, the LLM summarizer's source label, and
//! the preferred shortcut letters. Per-instance TOML can override
//! the display name and shortcut letters, and selects which topics
//! become tabs inside the widget.
//!
//! Hero images render inline when an article is expanded.
//! Everything we read is public — RSS feeds and the corresponding
//! image CDNs don't require a logged-in session.
//!
//! Framework note: this widget lives entirely under
//! `src/widgets/feeds/`. The only central edit is the
//! `WidgetDescriptor` entry in `widgets/registry.rs` and the feature
//! gate.

pub mod image;
pub mod provider;
pub mod templates;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use ratatui_image::{Resize, StatefulImage};
use serde::Deserialize;

use crate::cache::ScopedCache;
use crate::llm::{LlmMessage, LlmProvider, LlmRequest, Role};
use crate::theme::{ColorScheme, Theme};
use crate::ui::status::{live_value, TimedFeedback};
use crate::ui::{apply_title_row, MetadataEmphasis};

use super::{AppContext, EventResult, Widget, WidgetCtx};

use image::HeroImageStore;
use provider::{FeedArticle, FeedDefinition, FeedsRssProvider};

pub const KIND: &str = "feeds";

/// Loaded from `~/.config/glint/feeds.toml` (or
/// `feeds@<instance>.toml`).
///
/// The catalogue lives directly inside this file as `[[feeds]]`
/// blocks — no separate "source preset" indirection. Built-in
/// starter catalogues (WSJ, MarketWatch) live under
/// `src/widgets/feeds/templates/` and are surfaced by the
/// `--setup` wizard, but at runtime the per-instance TOML is the
/// sole source of truth.
#[derive(Debug, Clone, Deserialize)]
pub struct FeedsConfig {
    /// Title-bar / dashboard label and the LLM summarizer's
    /// "house-style" label. When unset (or empty), falls back to
    /// the instance name (or "Feeds" for the default `main`
    /// instance).
    #[serde(default)]
    pub display_name: Option<String>,

    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    /// Default summary length for a fresh widget session. Cycled
    /// at runtime with `S`.
    #[serde(default)]
    pub default_summary_length: SummaryLength,

    #[serde(default)]
    pub colors: ColorScheme,

    /// Preferred `Shift+<letter>` shortcut keys. The app's first-fit
    /// dispatcher walks this list and claims the first letter not
    /// already taken by another widget. Empty list opts out of the
    /// shortcut dispatcher entirely (widget still reachable via Tab
    /// and mouse).
    #[serde(default)]
    pub shortcuts: Vec<char>,

    /// Per-instance command aliases. The kind-level commands
    /// (`:feeds <terms>`, `:feeds-summary`, `:feeds-refresh`) are
    /// always available; entries here add additional aliases so
    /// instances can be addressed by their source name (e.g.
    /// `commands = ["wsj"]` makes `:wsj <terms>`, `:wsj-summary`,
    /// and `:wsj-refresh` route to this instance).
    #[serde(default)]
    pub commands: Vec<String>,

    /// One `[[feeds]]` block per active tab. The runtime catalogue
    /// is exactly this list — no preset/template lookup. Empty list
    /// triggers a one-time fallback to the WSJ starter template so a
    /// fresh install still paints content; the fallback emits a
    /// warning into the log so the user notices and edits their
    /// TOML.
    #[serde(default)]
    pub feeds: Vec<FeedSpec>,
}

/// One `[[feeds]]` entry from the per-instance TOML.
#[derive(Debug, Clone, Deserialize)]
pub struct FeedSpec {
    pub topic: String,
    pub url: String,
}

fn default_poll_interval() -> u64 {
    900
}

impl Default for FeedsConfig {
    fn default() -> Self {
        Self {
            display_name: None,
            poll_interval_secs: default_poll_interval(),
            default_summary_length: SummaryLength::default(),
            colors: ColorScheme::default(),
            shortcuts: Vec::new(),
            commands: Vec::new(),
            feeds: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SummaryLength {
    Short,
    Medium,
    Long,
}

impl Default for SummaryLength {
    fn default() -> Self {
        Self::Medium
    }
}

impl SummaryLength {
    fn next(self) -> Self {
        match self {
            Self::Short => Self::Medium,
            Self::Medium => Self::Long,
            Self::Long => Self::Short,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Short => "short",
            Self::Medium => "medium",
            Self::Long => "long",
        }
    }
    /// System prompt tuned to the requested paragraph count. The
    /// `source_label` (e.g. "WSJ", "Barron's", "MarketWatch") is
    /// inlined so the LLM knows which publication's house style to
    /// echo back.
    fn system_prompt(self, source_label: &str) -> String {
        match self {
            Self::Short => format!(
                "You are a concise {source_label} news summarizer. Given a \
                headline and a short excerpt, return a neutral ONE-paragraph \
                summary (3-5 sentences) capturing the key facts. No \
                editorializing, no preamble, no markdown."
            ),
            Self::Medium => format!(
                "You are a concise {source_label} news summarizer. Given a \
                headline and a short excerpt, return a neutral TWO-paragraph \
                summary capturing the key facts and any direct quotes. \
                Separate paragraphs with a blank line. No editorializing, \
                no preamble, no markdown."
            ),
            Self::Long => format!(
                "You are a thorough {source_label} news summarizer. Given a \
                headline and a short excerpt, return a neutral THREE-paragraph \
                summary: paragraph 1 covers the lead facts; paragraph 2 covers \
                context or quotes; paragraph 3 covers implications or \
                outlook. Separate paragraphs with a blank line. No \
                editorializing, no preamble, no markdown."
            ),
        }
    }
    fn max_tokens(self) -> u32 {
        match self {
            Self::Short => 200,
            Self::Medium => 400,
            Self::Long => 700,
        }
    }
    fn cache_suffix(self) -> &'static str {
        match self {
            Self::Short => "s",
            Self::Medium => "m",
            Self::Long => "l",
        }
    }
}

#[derive(Debug, Clone)]
enum SummaryState {
    Requested,
    Ready(String),
    Failed(String),
}

/// Decoded shape of a `:<word>` command for this widget. The
/// dispatcher accepts both the kind-level `feeds*` triple and any
/// per-instance alias declared in `FeedsConfig.commands`
/// (`<alias>`, `<alias>-summary`, `<alias>-refresh`).
#[derive(Debug, Clone, Copy)]
enum CommandAction {
    /// `:<root> <terms>` — keyword search; bare `:<root>` clears.
    Search,
    /// `:<root>-summary <short|medium|long>` — set summary length.
    SummaryLength,
    /// `:<root>-refresh` — force a refresh.
    Refresh,
}

/// Free-text search built by `:feeds <terms>`. Articles match if any
/// term appears (case-insensitive substring) in the title, topic, or
/// summary; results are ranked by total occurrence count so the most
/// densely-matching headline floats to the top.
///
/// Tokenization is aggressive: any non-alphanumeric character is
/// treated as a separator, so `:feeds climate, change; AI/tech` and
/// `:feeds climate change AI tech` produce the same four terms. Apostrophes
/// and other punctuation inside a token are *also* split — a small wart
/// for words like "don't" but in exchange the user never has to think
/// about how to delimit their query.
#[derive(Debug, Clone)]
struct SearchFilter {
    /// Original cleaned query — used for the search-tab label.
    query: String,
    /// Lowercased tokens to match against article text.
    terms: Vec<String>,
}

impl SearchFilter {
    fn new(raw: &str) -> Option<Self> {
        let terms: Vec<String> = raw
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .map(|t| t.to_lowercase())
            .collect();
        if terms.is_empty() {
            return None;
        }
        // Reconstruct a tidy display query by space-joining the tokens —
        // this also normalizes weird separators in the tab label.
        Some(Self {
            query: terms.join(" "),
            terms,
        })
    }

    /// Total case-insensitive substring matches across title, topic,
    /// and (if present) summary. Used both as the "include?" predicate
    /// (>0 means include) and as the sort key (higher wins).
    fn hit_count(&self, article: &FeedArticle) -> usize {
        let title = article.title.to_lowercase();
        let topic = article.topic.to_lowercase();
        let summary = article
            .summary
            .as_deref()
            .map(str::to_lowercase)
            .unwrap_or_default();
        let mut total = 0usize;
        for term in &self.terms {
            total += count_substring(&title, term);
            total += count_substring(&topic, term);
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
struct FeedsState {
    articles: Vec<FeedArticle>,
    /// Index into the *filtered* article list (after the active topic
    /// tab is applied). Cleared on tab change.
    selected: usize,
    /// First-visible row in the list panel. Auto-adjusted on render.
    list_scroll: usize,
    /// Currently active topic-filter tab. "All" => no filter; a
    /// topic label => only show articles tagged with that label.
    active_tab: String,
    /// True when the user has expanded the selected article. Shows
    /// the hero image + summary panel.
    expanded: bool,
    /// First-visible row of the expanded panel's content. PgUp/PgDn
    /// adjust this so long summaries can be read fully without
    /// resizing the widget. Reset to 0 on every article change.
    expanded_scroll: u16,
    /// Total rendered rows of the expanded panel's content, captured
    /// at render time. Used by PgDn to clamp scrolling so it doesn't
    /// run past the end of the body.
    expanded_content_height: u16,
    /// Hit-test rectangles captured at render time so `handle_mouse`
    /// can route clicks / scrolls to the right sub-panel without
    /// recomputing the layout. `None` when the widget hasn't rendered
    /// yet (very first frame); mouse events safely no-op.
    list_rect: Option<Rect>,
    expanded_rect: Option<Rect>,
    /// Per-article `(article_idx, abs_y_first, abs_y_last_inclusive)`
    /// captured at render time. Lets click-on-list map back to the
    /// article index when each article spans 1–3 rows (titles wrap).
    list_rows: Vec<(usize, u16, u16)>,
    /// Per-tab `(label, abs_x_start, abs_x_end_exclusive, abs_y)`
    /// captured at render time so left-click on the tab strip can
    /// route to the corresponding topic filter.
    tab_rects: Vec<(String, u16, u16, u16)>,
    /// Per-(url, length) summary state. Keyed so the user can flip
    /// between short/medium/long without losing earlier requests.
    summaries: HashMap<String, SummaryState>,
    /// Active `:feeds <terms>` filter, if any. When set, an extra
    /// `🔎 <query>` tab is appended to the tab bar and articles whose
    /// title/topic/summary contain at least one term surface there,
    /// ranked by hit count. Cleared by `x` or `:feeds` with no args.
    search: Option<SearchFilter>,
    /// True between a refresh kick-off and the corresponding
    /// articles update landing in `articles`.
    fetching: bool,
    poll: crate::polling::PollTracker,
    /// Transient status feedback (e.g. "LLM disabled — set an API key").
    status: Option<TimedFeedback<String>>,
    /// Display-state dirty bit drained by `take_dirty`. Set true by
    /// every async-task / tick-time mutation site so the main loop's
    /// dirty-flag gate triggers a redraw.
    dirty: bool,
}

const STATUS_TTL: Duration = Duration::from_millis(2500);
const ALL_TAB_LABEL: &str = "All";
/// Prefix for the dynamic search tab built by `:feeds <terms>`. Matches
/// the news widget's `🔎 <query>` convention so users transferring
/// muscle memory between the two read the same icon as "search".
const SEARCH_TAB_PREFIX: &str = "🔎 ";
/// Maximum number of rows a single article's title is allowed to
/// occupy in the list view. Longer titles get truncated with `…` on
/// the third line. Anything beyond 3 rows would push too few
/// articles into view.
const MAX_TITLE_LINES: usize = 3;

pub struct FeedsWidget {
    id: String,
    instance: String,
    display_name_cache: String,
    config: FeedsConfig,
    state: Arc<Mutex<FeedsState>>,
    summary_length: SummaryLength,
    /// Active feeds — exactly the per-instance `[[feeds]]` blocks.
    /// When the config arrived empty, this is seeded from the WSJ
    /// starter template at construction time (with a warning) so a
    /// fresh install still paints headlines.
    feeds: Vec<FeedDefinition>,
    /// LLM provider (None when disabled in llm.toml).
    llm: Option<Arc<dyn LlmProvider>>,
    cache: ScopedCache,
    app_theme: Arc<Theme>,
    theme: Theme,
    images: Arc<HeroImageStore>,
    shortcut: Option<char>,
    shortcut_prefs: Vec<char>,
}

impl FeedsWidget {
    pub fn with_config(
        instance: String,
        config: FeedsConfig,
        app_theme: Arc<Theme>,
        cache: ScopedCache,
        llm: Option<Arc<dyn LlmProvider>>,
    ) -> Self {
        let theme = app_theme.with_overrides(&config.colors);
        let shortcut_prefs = config.shortcuts.clone();
        let id = if instance == "main" {
            KIND.to_string()
        } else {
            format!("{KIND}@{instance}")
        };
        // Title-bar / dashboard label and the LLM summarizer's
        // source name. Explicit `display_name` wins; otherwise we
        // try the instance name, then fall back to "Feeds".
        let display_name_cache = config
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                if instance == "main" {
                    "Feeds".to_string()
                } else {
                    // Title-case the instance ("wsj" → "Wsj") so the
                    // dashboard label looks deliberate even when the
                    // user didn't set `display_name = "…"`. Users
                    // who care about exact casing override via
                    // `display_name`.
                    title_case_ascii(&instance)
                }
            });

        // Resolve the active catalogue. Per-instance `[[feeds]]`
        // blocks are the source of truth; when none are configured,
        // seed from the WSJ starter template so a brand-new install
        // still has something to render — but emit a warning so the
        // user knows their TOML is empty.
        let feeds: Vec<FeedDefinition> = if config.feeds.is_empty() {
            tracing::warn!(
                instance = %instance,
                "feeds: no [[feeds]] blocks configured, seeding from the WSJ starter template — \
                 add `[[feeds]]` entries to your TOML to customize"
            );
            match templates::by_id("wsj") {
                Some(t) => t
                    .feeds
                    .into_iter()
                    .filter(|f| f.default)
                    .map(|f| FeedDefinition {
                        topic: f.topic,
                        url: f.url,
                    })
                    .collect(),
                None => Vec::new(),
            }
        } else {
            config
                .feeds
                .iter()
                .map(|f| FeedDefinition {
                    topic: f.topic.clone(),
                    url: f.url.clone(),
                })
                .collect()
        };

        let poll_interval = Duration::from_secs(config.poll_interval_secs.max(60));
        let summary_length = config.default_summary_length;

        // Seed articles from the on-disk cache so the first render
        // paints content immediately while the next fetch happens.
        let mut initial_state = FeedsState {
            active_tab: ALL_TAB_LABEL.to_string(),
            poll: crate::polling::PollTracker::new(poll_interval),
            ..Default::default()
        };
        if let Some(entry) = cache.load::<Vec<FeedArticle>>(CACHE_KEY_ARTICLES) {
            initial_state.poll.seed_from_cache_age(entry.age());
            initial_state.articles = entry.value;
        }
        initial_state
            .poll
            .apply_jitter(&format!("{KIND}@{instance}"));

        let images = Arc::new(HeroImageStore::new(cache.clone()));

        Self {
            id,
            instance,
            display_name_cache,
            config,
            state: Arc::new(Mutex::new(initial_state)),
            summary_length,
            feeds,
            llm,
            cache,
            app_theme,
            theme,
            images,
            shortcut: None,
            shortcut_prefs,
        }
    }

    fn mark_dirty(&self) {
        self.state
            .lock()
            .expect("feeds state poisoned")
            .poll
            .mark_dirty();
    }

    fn is_due(&self) -> bool {
        let st = self.state.lock().expect("feeds state poisoned");
        if st.fetching {
            return false;
        }
        st.poll.is_due()
    }

    fn spawn_refresh(&self) {
        if self.feeds.is_empty() {
            return;
        }
        {
            let mut st = self.state.lock().expect("feeds state poisoned");
            st.fetching = true;
            st.poll.mark_attempted();
            st.dirty = true;
        }
        let feeds = self.feeds.clone();
        let state = self.state.clone();
        let cache = self.cache.clone();
        tokio::spawn(async move {
            let provider = FeedsRssProvider::new(feeds);
            let articles = provider.fetch().await;
            // Persist for next launch.
            if !articles.is_empty() {
                if let Err(err) = cache.store(CACHE_KEY_ARTICLES, &articles) {
                    tracing::warn!(error = %err, "feeds: articles cache store failed");
                }
            }
            let mut st = state.lock().expect("feeds state poisoned");
            if !articles.is_empty() {
                // Clamp selection so the new list doesn't leave the
                // cursor pointing past the end.
                let max = articles.len().saturating_sub(1);
                st.selected = st.selected.min(max);
                st.articles = articles;
            }
            st.fetching = false;
            st.dirty = true;
        });
    }

    /// Filter `articles` by the active tab — either a topic tab, the
    /// "All" pseudo-tab, or the dynamic search tab. Returns indices
    /// into the unfiltered list so selection math stays anchored to
    /// the canonical article identity. The search tab additionally
    /// sorts results by hit count (descending) so the most densely-
    /// matching headlines appear first.
    fn filtered_indices(&self) -> Vec<usize> {
        let st = self.state.lock().expect("feeds state poisoned");
        if let Some(search) = st.search.as_ref() {
            if st.active_tab.starts_with(SEARCH_TAB_PREFIX) {
                let mut scored: Vec<(usize, usize)> = st
                    .articles
                    .iter()
                    .enumerate()
                    .filter_map(|(i, a)| {
                        let n = search.hit_count(a);
                        if n > 0 {
                            Some((i, n))
                        } else {
                            None
                        }
                    })
                    .collect();
                // Sort by hit count desc; stable tie-break preserves
                // the provider's published-time ordering.
                scored.sort_by(|a, b| b.1.cmp(&a.1));
                return scored.into_iter().map(|(i, _)| i).collect();
            }
        }
        if st.active_tab == ALL_TAB_LABEL {
            return (0..st.articles.len()).collect();
        }
        st.articles
            .iter()
            .enumerate()
            .filter(|(_, a)| a.topic.eq_ignore_ascii_case(&st.active_tab))
            .map(|(i, _)| i)
            .collect()
    }

    fn selected_article(&self) -> Option<FeedArticle> {
        let filtered = self.filtered_indices();
        let idx = self.state.lock().expect("feeds state poisoned").selected;
        let real = filtered.get(idx).copied()?;
        self.state
            .lock()
            .expect("feeds state poisoned")
            .articles
            .get(real)
            .cloned()
    }

    fn move_selection(&self, delta: isize) {
        let total = self.filtered_indices().len();
        if total == 0 {
            return;
        }
        let mut st = self.state.lock().expect("feeds state poisoned");
        let new = (st.selected as isize + delta).clamp(0, total as isize - 1);
        if new as usize != st.selected {
            // Article change → reset the expanded-panel scroll so
            // the new article's content starts from the top.
            st.expanded_scroll = 0;
        }
        st.selected = new as usize;
    }

    fn cycle_tab(&self, forward: bool) {
        let tabs = self.tab_labels();
        if tabs.is_empty() {
            return;
        }
        let mut st = self.state.lock().expect("feeds state poisoned");
        let cur = tabs.iter().position(|t| t == &st.active_tab).unwrap_or(0);
        let n = tabs.len();
        let next = if forward {
            (cur + 1) % n
        } else {
            (cur + n - 1) % n
        };
        st.active_tab = tabs[next].clone();
        st.selected = 0;
        st.list_scroll = 0;
        st.expanded_scroll = 0;
    }

    /// Tab labels: "All", one per activated topic, plus a dynamic
    /// `🔎 <query>` tab when a `:feeds <terms>` search is active.
    fn tab_labels(&self) -> Vec<String> {
        let mut out = vec![ALL_TAB_LABEL.to_string()];
        for f in &self.feeds {
            out.push(f.topic.to_string());
        }
        if let Some(search) = self
            .state
            .lock()
            .expect("feeds state poisoned")
            .search
            .as_ref()
        {
            out.push(format!("{SEARCH_TAB_PREFIX}{}", search.query));
        }
        out
    }

    /// `:feeds <terms>` — install or replace the active search filter,
    /// append the search tab, and jump to it so the user sees results
    /// immediately. Empty / whitespace-only / punctuation-only input
    /// degrades to `clear_search` so a typo in the terminal can't get
    /// the widget stuck on an empty match set.
    fn set_search(&mut self, raw: &str) {
        let Some(filter) = SearchFilter::new(raw) else {
            self.clear_search();
            return;
        };
        let tab_label = format!("{SEARCH_TAB_PREFIX}{}", filter.query);
        let mut st = self.state.lock().expect("feeds state poisoned");
        st.search = Some(filter);
        st.active_tab = tab_label;
        st.selected = 0;
        st.list_scroll = 0;
        st.expanded = false;
        st.expanded_scroll = 0;
        st.dirty = true;
    }

    /// `x` or bare `:feeds` — drop any active search filter and snap
    /// back to the "All" tab. No-op when no search was active.
    fn clear_search(&mut self) {
        let mut st = self.state.lock().expect("feeds state poisoned");
        if st.search.take().is_some() {
            st.active_tab = ALL_TAB_LABEL.to_string();
            st.selected = 0;
            st.list_scroll = 0;
            st.expanded = false;
            st.expanded_scroll = 0;
            st.dirty = true;
        }
    }

    /// Recognize `cmd` against this instance's command surface:
    /// the always-on kind-level triple (`feeds`, `feeds-summary`,
    /// `feeds-refresh`) plus any per-instance aliases configured in
    /// `commands = [...]` (so an instance with `commands = ["wsj"]`
    /// also answers to `:wsj`, `:wsj-summary`, and `:wsj-refresh`).
    /// Returns `None` for commands this widget doesn't claim, which
    /// lets the dispatcher try the next widget.
    fn match_command(&self, cmd: &str) -> Option<CommandAction> {
        // Kind-level baseline — every feeds instance answers to these.
        match cmd {
            "feeds" => return Some(CommandAction::Search),
            "feeds-summary" => return Some(CommandAction::SummaryLength),
            "feeds-refresh" => return Some(CommandAction::Refresh),
            _ => {}
        }
        // Per-instance aliases. Empty trims and case-insensitive
        // matches so a stray space or capitalization in the TOML
        // doesn't silently break the alias.
        for alias in &self.config.commands {
            let alias = alias.trim();
            if alias.is_empty() {
                continue;
            }
            if cmd.eq_ignore_ascii_case(alias) {
                return Some(CommandAction::Search);
            }
            // `{alias}-summary` / `{alias}-refresh`. We build the
            // expected strings rather than splitting `cmd` on '-' so
            // aliases that themselves contain '-' (e.g. "mw-pro")
            // still match correctly.
            if cmd.len() == alias.len() + "-summary".len()
                && cmd[..alias.len()].eq_ignore_ascii_case(alias)
                && cmd[alias.len()..].eq_ignore_ascii_case("-summary")
            {
                return Some(CommandAction::SummaryLength);
            }
            if cmd.len() == alias.len() + "-refresh".len()
                && cmd[..alias.len()].eq_ignore_ascii_case(alias)
                && cmd[alias.len()..].eq_ignore_ascii_case("-refresh")
            {
                return Some(CommandAction::Refresh);
            }
        }
        None
    }

    fn set_status(&self, msg: impl Into<String>) {
        let mut st = self.state.lock().expect("feeds state poisoned");
        st.status = Some(TimedFeedback::new(msg.into(), STATUS_TTL));
    }

    fn live_status(&self) -> Option<String> {
        let mut st = self.state.lock().expect("feeds state poisoned");
        live_value(&mut st.status).cloned()
    }

    /// Cycle short → medium → long and reset visible state for the
    /// currently-selected article so the next `s` re-summarizes at
    /// the new length.
    fn cycle_summary_length(&mut self) {
        self.summary_length = self.summary_length.next();
        self.set_status(format!("Summary length: {}", self.summary_length.label()));
    }

    /// Kick off an LLM summary for the selected article. Idempotent:
    /// if a summary at the active length is already cached / in
    /// flight / ready, this is a no-op.
    fn request_summary(&self) {
        let Some(article) = self.selected_article() else {
            return;
        };
        let Some(llm) = self.llm.clone() else {
            self.set_status("LLM disabled — set an API key in the wizard");
            return;
        };
        let length = self.summary_length;
        let key = summary_key(&article.url, length);

        // Already in some state? Skip unless it was Failed (allow
        // retries with the same length).
        {
            let st = self.state.lock().expect("feeds state poisoned");
            match st.summaries.get(&key) {
                Some(SummaryState::Ready(_)) | Some(SummaryState::Requested) => return,
                _ => {}
            }
        }
        // On-disk cache?
        let cache_key = summary_cache_key(&article.url, length);
        if let Some(entry) = self.cache.load::<String>(&cache_key) {
            let mut st = self.state.lock().expect("feeds state poisoned");
            st.summaries.insert(key, SummaryState::Ready(entry.value));
            return;
        }

        // Compose user message from RSS short description (the only
        // content we have without a cookie-authenticated body fetch).
        let title = article.title.clone();
        let url = article.url.clone();
        let excerpt = article.summary.clone().unwrap_or_default();
        if excerpt.trim().is_empty() {
            self.set_status("No description in RSS — nothing to summarize");
            let mut st = self.state.lock().expect("feeds state poisoned");
            st.summaries
                .insert(key, SummaryState::Failed("RSS description missing".into()));
            return;
        }
        {
            let mut st = self.state.lock().expect("feeds state poisoned");
            st.summaries.insert(key.clone(), SummaryState::Requested);
            st.dirty = true;
        }
        let state = self.state.clone();
        let cache = self.cache.clone();
        let length_static = length;
        let source_label = self.display_name_cache.clone();
        tokio::spawn(async move {
            let user_block = format!("Title: {title}\nURL: {url}\n\nContent:\n{excerpt}\n");
            let request = LlmRequest {
                model: None,
                system: Some(length_static.system_prompt(&source_label)),
                messages: vec![LlmMessage {
                    role: Role::User,
                    content: user_block,
                }],
                max_tokens: length_static.max_tokens(),
                cache_system: true,
            };
            let outcome = match llm.complete(request).await {
                Ok(resp) => {
                    let text = resp.text.trim().to_string();
                    if text.is_empty() {
                        SummaryState::Failed("Empty LLM response".into())
                    } else {
                        if let Err(err) = cache.store(&cache_key, &text) {
                            tracing::warn!(error = %err, url = %url, "feeds: summary cache store failed");
                        }
                        SummaryState::Ready(text)
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, url = %url, "feeds: LLM call failed");
                    SummaryState::Failed(format!("{err}"))
                }
            };
            let mut st = state.lock().expect("feeds state poisoned");
            st.summaries.insert(key, outcome);
            st.dirty = true;
        });
    }

    fn toggle_expanded(&self) {
        let mut st = self.state.lock().expect("feeds state poisoned");
        st.expanded = !st.expanded;
    }

    fn jump_to_external(&self) {
        if let Some(article) = self.selected_article() {
            if let Err(err) = open::that(&article.url) {
                tracing::warn!(error = %err, url = %article.url, "feeds: failed to open URL");
            }
        }
    }

    // ─── render helpers ───────────────────────────────────────────

    fn render_tabs(&self, frame: &mut Frame, area: Rect) {
        let active = self
            .state
            .lock()
            .expect("feeds state poisoned")
            .active_tab
            .clone();
        let tabs = self.tab_labels();
        let mut spans: Vec<Span> = Vec::with_capacity(tabs.len() * 2);
        let mut hits: Vec<(String, u16, u16, u16)> = Vec::with_capacity(tabs.len());
        // Walk the labels accumulating screen-absolute x offsets so
        // we know where each `[Label]` lives for click routing. We
        // mirror the same `[Label]` + "  " separator rendering the
        // Paragraph below uses.
        let mut x = area.x;
        for (i, label) in tabs.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
                x = x.saturating_add(2);
            }
            let bracketed = format!("[{label}]");
            let width = bracketed.chars().count() as u16;
            let style = if label == &active {
                self.theme.text_selected
            } else {
                self.theme.text_dim
            };
            spans.push(Span::styled(bracketed, style));
            hits.push((label.clone(), x, x.saturating_add(width), area.y));
            x = x.saturating_add(width);
        }
        self.state.lock().expect("feeds state poisoned").tab_rects = hits;
        frame.render_widget(
            Paragraph::new(Line::from(spans)).alignment(Alignment::Left),
            area,
        );
    }

    fn render_list(&self, frame: &mut Frame, area: Rect) {
        let filtered = self.filtered_indices();
        let (selected, scroll) = {
            let st = self.state.lock().expect("feeds state poisoned");
            (st.selected, st.list_scroll)
        };
        let articles = self
            .state
            .lock()
            .expect("feeds state poisoned")
            .articles
            .clone();

        if filtered.is_empty() {
            let fetching = self.state.lock().expect("feeds state poisoned").fetching;
            let msg = if fetching {
                format!("Fetching {} headlines…", self.display_name_cache)
            } else {
                "No articles. Press `r` to refresh.".to_string()
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(msg, self.theme.text_dim)))
                    .wrap(Wrap { trim: false }),
                area,
            );
            return;
        }

        let row_budget = area.height as usize;
        if row_budget == 0 {
            return;
        }

        // Pre-compute how many rows each article occupies at the
        // current width. Used for variable-height scrolling so the
        // selected article is fully visible regardless of how many
        // lines its title wraps to.
        let prefix_widths: Vec<usize> = filtered
            .iter()
            .map(|&i| {
                articles
                    .get(i)
                    .map(|a| article_prefix_width(&a.topic))
                    .unwrap_or(0)
            })
            .collect();
        // `LIST_RIGHT_BUFFER = 1` leaves a one-column gutter between
        // the title text and the right border so headlines don't
        // touch the panel edge. Subtracting it from the wrap width
        // is enough; the cell remains empty on its own.
        const LIST_RIGHT_BUFFER: usize = 1;
        let row_counts: Vec<usize> = filtered
            .iter()
            .enumerate()
            .map(|(idx, &real_i)| {
                let Some(a) = articles.get(real_i) else {
                    return 1;
                };
                let prefix_w = prefix_widths[idx];
                let avail = (area.width as usize).saturating_sub(prefix_w + LIST_RIGHT_BUFFER);
                wrap_title(&a.title, avail.max(1), MAX_TITLE_LINES)
                    .len()
                    .max(1)
            })
            .collect();

        // Adjust scroll to keep `selected` fully visible. Scroll up
        // when the selection moves above the viewport; scroll down
        // when the cumulative rows of [scroll..=selected] exceed the
        // visible row budget.
        let mut new_scroll = scroll.min(filtered.len().saturating_sub(1));
        if selected < new_scroll {
            new_scroll = selected;
        }
        // Walk forward from the candidate `new_scroll` and grow it
        // until the selected article fits inside `row_budget`.
        loop {
            let mut used = 0usize;
            let mut fits_selected = false;
            for i in new_scroll..=selected {
                let need = row_counts.get(i).copied().unwrap_or(1);
                if used + need > row_budget {
                    break;
                }
                used += need;
                if i == selected {
                    fits_selected = true;
                }
            }
            if fits_selected || new_scroll >= selected {
                break;
            }
            new_scroll += 1;
        }
        self.state.lock().expect("feeds state poisoned").list_scroll = new_scroll;

        // Render with the dynamic per-article row budget. We track
        // (article_idx, abs_y_start, abs_y_last) ranges so mouse
        // clicks on a multi-line headline still map back to the
        // right article.
        let mut lines: Vec<Line> = Vec::with_capacity(row_budget);
        let mut rows_used = 0u16;
        let mut hit_ranges: Vec<(usize, u16, u16)> = Vec::new();
        for (i, &real_i) in filtered.iter().enumerate().skip(new_scroll) {
            let Some(article) = articles.get(real_i) else {
                continue;
            };
            let needed = row_counts.get(i).copied().unwrap_or(1) as u16;
            if rows_used + needed > row_budget as u16 {
                break;
            }
            let is_sel = i == selected;
            let marker = if is_sel { "▶ " } else { "  " };
            let topic_tag = format!("[{}]", article.topic);
            let line_style = if is_sel {
                self.theme.text_focused
            } else {
                self.theme.text_plain
            };
            let prefix_w = prefix_widths[i];
            let avail = (area.width as usize)
                .saturating_sub(prefix_w + LIST_RIGHT_BUFFER)
                .max(1);
            let chunks = wrap_title(&article.title, avail, MAX_TITLE_LINES);
            let y_start = area.y + rows_used;
            let y_end = y_start + needed.saturating_sub(1);
            hit_ranges.push((i, y_start, y_end));
            for (line_idx, chunk) in chunks.iter().enumerate() {
                if line_idx == 0 {
                    lines.push(Line::from(vec![
                        Span::styled(marker.to_string(), line_style),
                        Span::styled(topic_tag.clone(), self.theme.text_dim),
                        Span::raw(" "),
                        Span::styled(chunk.clone(), line_style),
                    ]));
                } else {
                    // Hanging indent of 4 spaces — pushes wrapped
                    // lines visibly to the right of the topic label
                    // anchor so it's obvious they're a continuation
                    // of the headline above, not a separate row.
                    lines.push(Line::from(vec![
                        Span::raw("    "),
                        Span::styled(chunk.clone(), line_style),
                    ]));
                }
            }
            rows_used += needed;
        }
        // Store the hit-test data for handle_mouse.
        {
            let mut st = self.state.lock().expect("feeds state poisoned");
            st.list_rect = Some(area);
            st.list_rows = hit_ranges;
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }

    /// Narrow + tall stacked layout: list-on-top, horizontal-line
    /// separator, article-details-on-bottom. Targets sidebar-style
    /// widget placements where a side-by-side split would crush both
    /// panels.
    ///
    /// Sizing rules (per the user's spec):
    /// - When there's room (≥ 33 rows total): article list takes the
    ///   top with at least 8 rows; the bottom details panel gets
    ///   exactly 22 rows so the hero image fits comfortably.
    /// - When the panel can't fit both 8 list rows *and* a 22-row
    ///   image-bearing details panel: list stays at 8 rows and the
    ///   bottom details panel takes the remainder *without* the
    ///   hero image so the body + summary still have room.
    /// - When even that fallback won't fit (very short panels):
    ///   the expansion gracefully collapses to a list-only render.
    fn render_expanded_vertical(&self, frame: &mut Frame, area: Rect) {
        const MIN_LIST_ROWS: u16 = 8;
        const SEP_ROWS: u16 = 3; // blank · horizontal line · blank
        const DETAILS_WITH_IMAGE: u16 = 22;
        const DETAILS_NO_IMAGE_MIN: u16 = 6;

        let total = area.height;
        if total < MIN_LIST_ROWS + SEP_ROWS + DETAILS_NO_IMAGE_MIN {
            // Truly too small — degrade to list-only so the user can
            // still scroll headlines. Mouse-collapse via `e` puts
            // them back in flat-list mode anyway.
            self.render_list(frame, area);
            self.state.lock().expect("feeds state poisoned").expanded_rect = None;
            return;
        }

        let (list_h, details_h, allow_image) =
            if total >= MIN_LIST_ROWS + SEP_ROWS + DETAILS_WITH_IMAGE {
                let details_h = DETAILS_WITH_IMAGE;
                let list_h = total - SEP_ROWS - details_h;
                (list_h, details_h, true)
            } else {
                // Fallback: pin the list to its 8-row minimum and give
                // every remaining row to the details *without* the
                // image, per the user's spec.
                let list_h = MIN_LIST_ROWS;
                let details_h = total - SEP_ROWS - list_h;
                (list_h, details_h, false)
            };

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(list_h),
                Constraint::Length(SEP_ROWS),
                Constraint::Length(details_h),
            ])
            .split(area);

        self.render_list(frame, rows[0]);
        self.render_horizontal_separator(frame, rows[1]);
        self.render_expanded(frame, rows[2], allow_image);
        self.state.lock().expect("feeds state poisoned").expanded_rect = Some(rows[2]);
    }

    /// Three-row separator: blank · `─` line · blank. The line uses
    /// the dim style so it sits between the list and the details
    /// panel as a quiet visual divider rather than competing for
    /// attention.
    fn render_horizontal_separator(&self, frame: &mut Frame, area: Rect) {
        if area.height < 3 || area.width == 0 {
            return;
        }
        let line_y = area.y + 1;
        let line_rect = Rect {
            x: area.x,
            y: line_y,
            width: area.width,
            height: 1,
        };
        let bar: String = "─".repeat(area.width as usize);
        frame.render_widget(
            Paragraph::new(Span::styled(bar, self.theme.text_dim)),
            line_rect,
        );
        // Top + bottom rows of `area` are intentionally left blank.
    }

    /// Paint the right-aligned timestamp column next to the article
    /// list (collapsed-mode only). Walks the `list_rows` hit-test
    /// cache that `render_list` populated and writes a relative-age
    /// label at each article's first y-row, dimmed so the headline
    /// stays the visual anchor.
    fn render_timestamps(&self, frame: &mut Frame, area: Rect) {
        let (rows, articles) = {
            let st = self.state.lock().expect("feeds state poisoned");
            (st.list_rows.clone(), st.articles.clone())
        };
        if rows.is_empty() {
            return;
        }
        let now = chrono::Utc::now();
        for (idx, y_start, _) in &rows {
            // Skip rows whose y is outside `area` (defensive — the
            // list and timestamp columns share the same vertical
            // extent, so this should always hit).
            if *y_start < area.y || *y_start >= area.y + area.height {
                continue;
            }
            let Some(article) = articles.get(*idx) else {
                continue;
            };
            let label = format_relative_time(article.published, now);
            let cell = Rect {
                x: area.x,
                y: *y_start,
                width: area.width,
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(Span::styled(label, self.theme.text_dim))
                    .alignment(Alignment::Right),
                cell,
            );
        }
    }

    fn render_expanded(&self, frame: &mut Frame, area: Rect, allow_image: bool) {
        let Some(article) = self.selected_article() else {
            return;
        };
        // Reserve a 1-cell right margin so neither the hero image
        // nor the article body / summary text runs flush against
        // the widget's right border. Matches the list-side
        // LIST_RIGHT_BUFFER convention so both panels look balanced.
        let area = Rect {
            width: area.width.saturating_sub(1),
            ..area
        };
        let has_image = article.hero_image_url.is_some() && allow_image;
        // 13-row hero region (25% taller than the original 10) and a
        // matching bumped threshold (16) so we only carve out the
        // image area when there's still room for the body underneath.
        let has_room = area.height >= 16;
        let constraints: Vec<Constraint> = if has_image && has_room {
            vec![Constraint::Length(13), Constraint::Min(4)]
        } else {
            vec![Constraint::Length(0), Constraint::Min(1)]
        };
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        // Hero image. We *always* paint Clear over the image rect
        // before drawing anything else so the prior frame's iTerm2
        // inline-image protocol (which lays escape data into a single
        // anchor cell + skip flags on neighbors) gets explicitly
        // overwritten when we navigate. Additionally, we build a
        // *fresh* StatefulProtocol from the decoded image each frame:
        // the cached protocol's `needs_resize` short-circuits when
        // the rect is unchanged, leaving stale escape bytes that
        // ratatui's buffer diff happens to consider equal across
        // backward-navigation transitions. A fresh protocol always
        // produces freshly-encoded bytes → diff always writes.
        if has_image && has_room {
            frame.render_widget(Clear, rows[0]);
            let url = article.hero_image_url.as_deref().unwrap();
            self.images.ensure(url);
            let proto_arc = self.images.build_protocol(url);
            if let Some(proto_arc) = proto_arc {
                let mut proto = proto_arc.lock().expect("hero proto poisoned");
                let img_widget = StatefulImage::new(None).resize(Resize::Fit(None));
                frame.render_stateful_widget(img_widget, rows[0], &mut *proto);
            } else {
                // Not Ready yet — show a status hint and let the
                // Clear above wipe any leftover prior image.
                let slot = self.images.slot(url);
                let st = slot.lock().expect("hero state poisoned");
                let msg = match &*st {
                    image::HeroState::Fetching => "  loading hero image…",
                    image::HeroState::Failed => "  (image unavailable)",
                    _ => "",
                };
                if !msg.is_empty() {
                    frame.render_widget(
                        Paragraph::new(Span::styled(msg, self.theme.text_dim)),
                        rows[0],
                    );
                }
            }
        }

        // Body: title + byline + summary state. Composed as a
        // Vec<Line>, then we slice off the top `expanded_scroll`
        // lines so PgUp/PgDn can scroll long summaries.
        let length = self.summary_length;
        let summary_state = self
            .state
            .lock()
            .expect("feeds state poisoned")
            .summaries
            .get(&summary_key(&article.url, length))
            .cloned();
        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(Span::styled(
            article.title.clone(),
            self.theme.text_brilliant,
        )));
        if !article.authors.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("by {}", article.authors.join(", ")),
                self.theme.text_dim,
            )));
        }
        lines.push(Line::from(Span::styled(
            format!(
                "{} · {}",
                article.topic,
                article.published.format("%b %d, %Y %H:%M UTC")
            ),
            self.theme.text_dim,
        )));
        lines.push(Line::from(""));
        match summary_state {
            None => {
                if let Some(s) = article.summary {
                    lines.push(Line::from(Span::styled(s, self.theme.text_plain)));
                }
                // The s / Ctrl+S / e instructions live in the
                // widget footer already, no need to repeat them in
                // the body and waste vertical room.
            }
            Some(SummaryState::Requested) => {
                lines.push(Line::from(Span::styled(
                    format!("Summarizing ({})…", length.label()),
                    self.theme.text_focused,
                )));
            }
            Some(SummaryState::Ready(text)) => {
                for para in text.split("\n\n") {
                    lines.push(Line::from(Span::styled(
                        para.to_string(),
                        self.theme.text_plain,
                    )));
                    lines.push(Line::from(""));
                }
            }
            Some(SummaryState::Failed(reason)) => {
                lines.push(Line::from(Span::styled(
                    format!("Summary failed: {reason}"),
                    self.theme.metadata_unfocused,
                )));
            }
        }

        // Estimate how many *rendered* rows the lines will take at
        // this width so we can clamp scroll and decide whether to
        // paint the "more below" arrow. The ratatui Paragraph word-
        // wraps each Line; we approximate by ceil-dividing each
        // line's char count by the available width. Good enough for
        // the indicator + clamp; off-by-one is harmless.
        let body_w = rows[1].width.max(1) as usize;
        let body_h = rows[1].height;
        let estimated_rows: u16 = lines
            .iter()
            .map(|l| {
                let chars: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
                ((chars + body_w - 1) / body_w).max(1) as u16
            })
            .sum();
        let max_scroll = estimated_rows.saturating_sub(body_h);
        let scroll = {
            let mut st = self.state.lock().expect("feeds state poisoned");
            // Clamp + persist back so PgDn at the end is a no-op.
            st.expanded_scroll = st.expanded_scroll.min(max_scroll);
            st.expanded_content_height = estimated_rows;
            st.expanded_scroll
        };

        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            rows[1],
        );

        // "More below" cue: a small `↓` glyph in the bottom-right of
        // the body when content extends past the visible area.
        if scroll < max_scroll && body_h > 0 && rows[1].width > 0 {
            let glyph_area = Rect {
                x: rows[1].x + rows[1].width - 1,
                y: rows[1].y + body_h - 1,
                width: 1,
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(Span::styled("↓", self.theme.text_selected)),
                glyph_area,
            );
        }
    }
}

const CACHE_KEY_ARTICLES: &str = "articles";

fn summary_key(url: &str, length: SummaryLength) -> String {
    format!("{}-{}", url, length.cache_suffix())
}

fn summary_cache_key(url: &str, length: SummaryLength) -> String {
    crate::cache::short_hash_key(&format!("summary-{}-", length.cache_suffix()), url)
}

/// Compact relative-age label, delegating to the shared
/// [`crate::format::relative_time_label`].
fn format_relative_time(
    when: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    crate::format::relative_time_label(when, now)
}

/// Split the body row's total width into `(list_w, summary_w)`.
///
/// Constraints:
/// - List gets up to 60% of total width.
/// - Summary always retains ≥ `MIN_SUMMARY_W` columns.
/// - When the total width is too narrow to satisfy both, returns
///   `(total, 0)` so the caller can collapse back to a list-only
///   layout rather than showing a useless 1-column summary panel.
fn split_list_summary_widths(total_w: u16) -> (u16, u16) {
    const MIN_SUMMARY_W: u16 = 45;
    const MIN_LIST_W: u16 = 20;
    if total_w < MIN_SUMMARY_W + MIN_LIST_W {
        return (total_w, 0);
    }
    let sixty_pct = ((total_w as u32 * 60 + 50) / 100) as u16;
    let cap_to_keep_summary = total_w - MIN_SUMMARY_W;
    let list_w = sixty_pct.min(cap_to_keep_summary).max(MIN_LIST_W);
    let summary_w = total_w - list_w;
    (list_w, summary_w)
}

/// Column count consumed by the article's row prefix (marker + topic
/// tag + trailing space). Used both to size the title's available
/// width on line 1 and to set the hanging indent on lines 2/3.
fn article_prefix_width(topic: &str) -> usize {
    // marker (2) + "[" + topic + "]" + " "
    2 + 1 + topic.chars().count() + 1 + 1
}

/// Word-wrap an article headline. Delegates to the shared
/// [`crate::text::wrap`] with `preserve_paragraphs = false` since
/// titles are always single-line input.
fn wrap_title(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    crate::text::wrap(text, width, max_lines, false)
}

/// Title-case the first ASCII letter of an instance name so the
/// dashboard label looks deliberate when the user didn't set
/// `display_name = "…"` (e.g., instance `"wsj"` → `"Wsj"`,
/// `"marketwatch"` → `"Marketwatch"`). For exact casing the user
/// should set `display_name` explicitly.
fn title_case_ascii(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

#[async_trait]
impl Widget for FeedsWidget {
    fn id(&self) -> &str {
        &self.id
    }

    fn display_name(&self) -> &str {
        &self.display_name_cache
    }

    fn kind(&self) -> &str {
        KIND
    }

    fn instance(&self) -> &str {
        &self.instance
    }

    async fn update(&mut self, _ctx: &AppContext) -> Result<()> {
        if self.is_due() {
            self.spawn_refresh();
        }
        let mut st = self.state.lock().expect("feeds state poisoned");
        if crate::ui::status::drain_if_expired(&mut st.status) {
            st.dirty = true;
        }
        Ok(())
    }

    fn take_dirty(&mut self) -> bool {
        let mut st = self.state.lock().expect("feeds state poisoned");
        std::mem::replace(&mut st.dirty, false)
    }

    fn render(&self, frame: &mut Frame, area: Rect, focused: bool) {
        let metadata = Some(format!("{} feeds", self.feeds.len()));
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
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let footer_h: u16 = if inner.height >= 2 { 1 } else { 0 };
        let body = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: inner.height - footer_h,
        };

        // Vertical layout: tab bar (1 row), 1-row blank gutter, then
        // the list (and optional expanded panel). The gutter
        // breathes the headline list away from the tab strip so the
        // top of the widget doesn't feel cramped.
        let tab_h: u16 = 1;
        let gap_h: u16 = 1;
        if body.height < tab_h + gap_h + 1 {
            return;
        }
        let body_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(tab_h),
                Constraint::Length(gap_h),
                Constraint::Min(1),
            ])
            .split(body);
        self.render_tabs(frame, body_rows[0]);
        // body_rows[1] is intentionally left blank.

        let expanded = self.state.lock().expect("feeds state poisoned").expanded;
        // Layout switch: when the widget cell is narrow (width <
        // NARROW_VS_WIDE_THRESHOLD), the horizontal list|details
        // split crowds both columns, so we vertically stack
        // list-on-top / details-on-bottom instead. The threshold
        // matches the smallest width where a 45-col summary + a
        // 40-col list still both feel comfortable.
        const NARROW_VS_WIDE_THRESHOLD: u16 = 80;
        let prefer_vertical = body_rows[2].width < NARROW_VS_WIDE_THRESHOLD;
        if expanded && body_rows[2].height >= 8 && prefer_vertical {
            self.render_expanded_vertical(frame, body_rows[2]);
        } else if expanded && body_rows[2].height >= 8 {
            // Two-column split with a 1-cell visual gap between the
            // list and the article-details panel so titles in the
            // list don't visually run into the image / summary on
            // the right. The list grows up to 60% of the body width
            // but yields whatever's needed to keep the expanded
            // summary at ≥45 columns. On very narrow panels
            // (< ~50 cols) we collapse back to list-only.
            let usable = body_rows[2].width.saturating_sub(1); // reserve 1 col for the gap
            let (list_w, summary_w) = split_list_summary_widths(usable);
            if summary_w == 0 {
                self.render_list(frame, body_rows[2]);
            } else {
                let cols = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Length(list_w),
                        Constraint::Length(1),
                        Constraint::Length(summary_w),
                    ])
                    .split(body_rows[2]);
                self.render_list(frame, cols[0]);
                // cols[1] is the gap — intentionally left blank.
                self.render_expanded(frame, cols[2], true);
                // Cache the expanded-pane rect for mouse hit-tests.
                self.state.lock().expect("feeds state poisoned").expanded_rect = Some(cols[2]);
            }
        } else {
            // Not expanded: carve off a small right-aligned column
            // for relative-time stamps next to each article. When
            // the user expands an article, we want the maximum width
            // for headlines + the details panel, so the timestamp
            // column only appears in collapsed mode.
            const TS_W: u16 = 7;
            const TS_GAP: u16 = 1;
            // Right buffer keeps the timestamp text from touching
            // the widget's right border (matches the same 1-cell
            // right margin we apply to the list and details panes).
            const TS_RIGHT_BUFFER: u16 = 1;
            let total = body_rows[2].width;
            if total >= TS_W + TS_GAP + TS_RIGHT_BUFFER + 12 {
                let cols = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Min(12),
                        Constraint::Length(TS_GAP),
                        Constraint::Length(TS_W),
                        Constraint::Length(TS_RIGHT_BUFFER),
                    ])
                    .split(body_rows[2]);
                self.render_list(frame, cols[0]);
                self.render_timestamps(frame, cols[2]);
                // cols[3] is the right buffer — intentionally blank.
            } else {
                // Too narrow for a separate column — just render
                // the list and skip the timestamps. Headlines stay
                // legible.
                self.render_list(frame, body_rows[2]);
            }
            self.state.lock().expect("feeds state poisoned").expanded_rect = None;
        }

        // Footer
        if footer_h > 0 {
            let footer = Rect {
                x: inner.x,
                y: inner.y + inner.height - 1,
                width: inner.width,
                height: 1,
            };
            let (text, style) = match self.live_status() {
                Some(msg) => (msg, self.theme.text_selected),
                None => {
                    // Only show "[/] scroll" when the article is
                    // actually expanded — the keys aren't bound in
                    // collapsed mode, so advertising them just
                    // clutters the footer.
                    let scroll_hint = if expanded { "[/] scroll · " } else { "" };
                    (
                        format!(
                            "↑/↓ select · ⏎/e expand · {scroll_hint}o open · s summary ({}) · Ctrl+S length · r refresh",
                            self.summary_length.label()
                        ),
                        self.theme.text_dim,
                    )
                }
            };
            frame.render_widget(
                Paragraph::new(Span::styled(text, style)).alignment(Alignment::Right),
                footer,
            );
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        // Ctrl+S cycles summary length — handle before the
        // modifier-rejection gate below since the rest of the bindings
        // are plain-letter.
        if key.modifiers == KeyModifiers::CONTROL
            && matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S'))
        {
            self.cycle_summary_length();
            return EventResult::Handled;
        }
        if key.modifiers != KeyModifiers::NONE && key.modifiers != KeyModifiers::SHIFT {
            return EventResult::Ignored;
        }
        // Leave uppercase letters to the focus-jump dispatcher.
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
            KeyCode::Left | KeyCode::Char('h') => {
                self.cycle_tab(false);
                EventResult::Handled
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.cycle_tab(true);
                EventResult::Handled
            }
            // `e` is the primary expand toggle; Space is the
            // Convention: Enter is the in-place primary action
            // (expand), `o` opens externally. `e` and Space are
            // back-compat aliases for users who reach for them
            // reflexively.
            KeyCode::Enter | KeyCode::Char('e') | KeyCode::Char(' ') => {
                self.toggle_expanded();
                EventResult::Handled
            }
            KeyCode::Char('o') => {
                self.jump_to_external();
                EventResult::Handled
            }
            KeyCode::Char('s') => {
                self.request_summary();
                // Auto-expand so the user can see the summary land.
                self.state.lock().expect("feeds state poisoned").expanded = true;
                EventResult::Handled
            }
            KeyCode::Char('r') => {
                self.mark_dirty();
                EventResult::Handled
            }
            // `x` drops any active :feeds <terms> search filter and
            // snaps back to the All tab. No-op when no search is
            // active so a stray `x` press isn't disruptive.
            KeyCode::Char('x') => {
                self.clear_search();
                EventResult::Handled
            }
            // PgUp / PgDn / [ / ] scroll the article-details panel
            // when it overflows the visible body. j/k still navigate
            // articles — using a separate key avoids stealing the
            // primary navigation gesture inside expanded mode. The
            // bracket aliases are easier to reach without leaving
            // the home row.
            KeyCode::PageDown | KeyCode::Char(']') => {
                let mut st = self.state.lock().expect("feeds state poisoned");
                if st.expanded {
                    let step = 5u16;
                    let max = st.expanded_content_height.saturating_sub(1);
                    st.expanded_scroll = (st.expanded_scroll.saturating_add(step)).min(max);
                }
                EventResult::Handled
            }
            KeyCode::PageUp | KeyCode::Char('[') => {
                let mut st = self.state.lock().expect("feeds state poisoned");
                if st.expanded {
                    st.expanded_scroll = st.expanded_scroll.saturating_sub(5);
                }
                EventResult::Handled
            }
            _ => EventResult::Ignored,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, _area: Rect) -> EventResult {
        use crossterm::event::{MouseButton, MouseEventKind};
        // Snapshot hit-test rects (release the lock before doing
        // anything that re-locks state).
        let (list_rect, expanded_rect, list_rows, tab_rects) = {
            let st = self.state.lock().expect("feeds state poisoned");
            (
                st.list_rect,
                st.expanded_rect,
                st.list_rows.clone(),
                st.tab_rects.clone(),
            )
        };
        let col = mouse.column;
        let row = mouse.row;
        let over_list = list_rect
            .map(|r| col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height)
            .unwrap_or(false);
        let over_expanded = expanded_rect
            .map(|r| col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height)
            .unwrap_or(false);

        match mouse.kind {
            // Scroll wheel: route by cursor position. Over the list,
            // wheel = navigate articles. Over the expanded panel,
            // wheel = scroll the body. Anywhere else (gap, tabs,
            // footer): ignore.
            MouseEventKind::ScrollUp => {
                if over_expanded {
                    let mut st = self.state.lock().expect("feeds state poisoned");
                    st.expanded_scroll = st.expanded_scroll.saturating_sub(3);
                    EventResult::Handled
                } else if over_list {
                    self.move_selection(-1);
                    EventResult::Handled
                } else {
                    EventResult::Ignored
                }
            }
            MouseEventKind::ScrollDown => {
                if over_expanded {
                    let mut st = self.state.lock().expect("feeds state poisoned");
                    let max = st.expanded_content_height.saturating_sub(1);
                    st.expanded_scroll = st.expanded_scroll.saturating_add(3).min(max);
                    EventResult::Handled
                } else if over_list {
                    self.move_selection(1);
                    EventResult::Handled
                } else {
                    EventResult::Ignored
                }
            }
            // Left click: route to whichever hit-tested region the
            // cursor lands in. Tab strip → switch topic filter;
            // list row → select that article (multi-line wrapped
            // titles all map back to the right index via list_rows).
            MouseEventKind::Down(MouseButton::Left) => {
                // Tab strip first — its row sits above the list, so
                // an explicit hit-test on it avoids any ambiguity.
                if let Some((label, _, _, _)) = tab_rects
                    .iter()
                    .find(|(_, x0, x1, y)| row == *y && col >= *x0 && col < *x1)
                {
                    let label = label.clone();
                    let mut st = self.state.lock().expect("feeds state poisoned");
                    if st.active_tab != label {
                        st.active_tab = label;
                        st.selected = 0;
                        st.list_scroll = 0;
                        st.expanded_scroll = 0;
                    }
                    return EventResult::Handled;
                }
                if !over_list {
                    return EventResult::Ignored;
                }
                if let Some((idx, _, _)) = list_rows
                    .iter()
                    .find(|(_, y0, y1)| row >= *y0 && row <= *y1)
                {
                    let new_idx = *idx;
                    let mut st = self.state.lock().expect("feeds state poisoned");
                    if new_idx != st.selected {
                        st.expanded_scroll = 0;
                    }
                    st.selected = new_idx;
                    EventResult::Handled
                } else {
                    EventResult::Ignored
                }
            }
            _ => EventResult::Ignored,
        }
    }

    fn handle_command(&mut self, cmd: &str, args: &[&str]) -> Result<bool> {
        let Some(action) = self.match_command(cmd) else {
            return Ok(false);
        };
        match action {
            CommandAction::SummaryLength => {
                let arg = args.first().copied().unwrap_or("").trim();
                let next = match arg.to_ascii_lowercase().as_str() {
                    "short" | "s" => Some(SummaryLength::Short),
                    "medium" | "med" | "m" => Some(SummaryLength::Medium),
                    "long" | "l" => Some(SummaryLength::Long),
                    _ => None,
                };
                let Some(next) = next else {
                    anyhow::bail!("usage: :{cmd} <short|medium|long>");
                };
                self.summary_length = next;
                self.set_status(format!("Summary length: {}", next.label()));
                Ok(true)
            }
            CommandAction::Search => {
                // `:<root> <terms>` → keyword search; bare `:<root>` → clear
                // any active search filter (mirrors `:news` semantics).
                // Explicit refresh stays on `:<root>-refresh` so users
                // haven't lost that capability.
                let query = args.join(" ");
                let trimmed = query.trim();
                if trimmed.is_empty() {
                    self.clear_search();
                } else {
                    self.set_search(trimmed);
                }
                Ok(true)
            }
            CommandAction::Refresh => {
                self.mark_dirty();
                Ok(true)
            }
        }
    }

    fn config(&self) -> serde_json::Value {
        serde_json::json!({
            "display_name": self.display_name_cache,
            "feed_count": self.config.feeds.len(),
            "poll_interval_secs": self.config.poll_interval_secs,
            "default_summary_length": self.config.default_summary_length.label(),
        })
    }

    fn apply_config(&mut self, config: serde_json::Value) -> Result<()> {
        let new_config: FeedsConfig =
            serde_json::from_value(config).context("invalid feeds config")?;
        let app_theme = self.app_theme.clone();
        let cache = self.cache.clone();
        let llm = self.llm.clone();
        let replaced = Self::with_config(self.instance.clone(), new_config, app_theme, cache, llm);
        self.config = replaced.config;
        self.feeds = replaced.feeds;
        self.summary_length = replaced.summary_length;
        self.theme = replaced.theme;
        self.display_name_cache = replaced.display_name_cache;
        self.shortcut_prefs = replaced.shortcut_prefs;
        // Reset poll interval to the new config's value.
        {
            let mut st = self.state.lock().expect("feeds state poisoned");
            st.poll
                .set_interval(Duration::from_secs(self.config.poll_interval_secs.max(60)));
        }
        self.mark_dirty();
        Ok(())
    }

    fn keybindings(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("↑ / ↓ / j / k", "select article"),
            ("← / → / h / l", "switch topic tab"),
            ("Enter / e / Space", "expand / collapse selected article"),
            ("PgUp / PgDn / [ / ]", "scroll the expanded article details"),
            ("click article", "select that article"),
            ("click topic tab", "switch the active topic filter"),
            ("scroll over list", "navigate selection up / down"),
            ("scroll over details", "scroll article body up / down"),
            ("o", "open article in browser"),
            ("s", "LLM summarize at current length"),
            ("Ctrl+S", "cycle summary length (short/medium/long)"),
            ("r", "force refresh"),
            ("x", "clear :feeds <terms> search filter"),
            (
                ":feeds <terms>",
                "filter articles by keyword (ranked by hits, commas/semicolons ignored)",
            ),
        ]
    }

    /// Live scheme switching. The app calls this on every widget
    /// after `:scheme <name>` swaps the global theme; without an
    /// override the feeds widget would keep its merged `theme` field
    /// from construction and changes only show up after restart.
    fn set_app_theme(&mut self, theme: Arc<Theme>) {
        self.theme = theme.with_overrides(&self.config.colors);
        self.app_theme = theme;
    }

    fn poll_snapshot(&self) -> Option<crate::polling::PollSnapshot> {
        Some(
            self.state
                .lock()
                .expect("feeds state poisoned")
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
        let count = self
            .state
            .lock()
            .expect("feeds state poisoned")
            .articles
            .len();
        Some(format!("{count} articles"))
    }
}

pub fn build(ctx: &WidgetCtx) -> Box<dyn Widget> {
    let cfg: FeedsConfig =
        crate::config::load_widget_toml_for_instance(KIND, &ctx.instance).unwrap_or_default();
    Box::new(FeedsWidget::with_config(
        ctx.instance.clone(),
        cfg,
        ctx.theme.clone(),
        ctx.cache.clone(),
        ctx.llm.clone(),
    ))
}

/// Wizard Choice value used when the user wants an empty starter
/// (no `[[feeds]]` blocks pre-filled). Picking this prints commented-
/// out example blocks so the user can replace the URLs with their own.
const EMPTY_TEMPLATE_ID: &str = "empty";

pub fn wizard_descriptor() -> crate::wizard::descriptor::WizardDescriptor {
    use crate::wizard::descriptor::{ChoiceOption, WizardDescriptor, WizardField, WizardFieldKind};

    // Build the source-choice list dynamically from the built-in
    // templates, plus an "Empty" option that seeds zero `[[feeds]]`
    // blocks for users who want to BYO catalogue. Templates are read
    // from disk-embedded TOML files under
    // `src/widgets/feeds/templates/` — to add a new built-in source,
    // drop a new TOML there and append it to `BUILTIN_TEMPLATES` in
    // `templates.rs`.
    //
    // Note: `ChoiceOption` carries `&'static str` for value/label.
    // Templates load into owned Strings, so we leak each pair into
    // 'static once at wizard creation. This runs once per `--setup`
    // invocation — bounded, never on the render hot path.
    let mut source_options: Vec<ChoiceOption> = templates::all()
        .into_iter()
        .map(|t| ChoiceOption {
            value: Box::leak(t.id.into_boxed_str()),
            label: Box::leak(t.display_name.into_boxed_str()),
            help: None,
        })
        .collect();
    source_options.push(ChoiceOption {
        value: EMPTY_TEMPLATE_ID,
        label: "Empty (bring your own feeds)",
        help: Some("Skip the catalogue prefill — generates a TOML with no [[feeds]] blocks and an example you can edit."),
    });

    WizardDescriptor {
        display_name: "Feeds",
        blurb: "Tabbed single-source RSS reader. Pick a starter template — \
                the wizard copies its catalogue into the generated \
                `feeds@<instance>.toml` as live and commented-out \
                `[[feeds]]` blocks you can edit afterward. Pick Empty to \
                start with no feeds and supply your own URLs.",
        load_from_toml: None,
        render_toml: Some(render_feeds_toml),
        fields: vec![
            WizardField {
                key: "source",
                label: "Starter template",
                help: "Which built-in catalogue to seed this instance from. \
                       The template's [[feeds]] become live entries; \
                       additional topics in the same template are written \
                       commented out so you can uncomment to enable. The \
                       template choice is not persisted in the TOML — once \
                       generated, the file is fully yours to edit.",
                required: true,
                kind: WizardFieldKind::Choice {
                    options: source_options,
                    default: Some("wsj"),
                },
                validate: None,
            },
            WizardField {
                key: "display_name",
                label: "Dashboard label (optional)",
                help: "Title-bar / dashboard name for this instance. Also \
                       used as the LLM summarizer's house-style label. \
                       Leave empty to fall back to the template's name \
                       (e.g. \"WSJ\", \"MarketWatch\").",
                required: false,
                kind: WizardFieldKind::Text {
                    default: None,
                    placeholder: Some("(use template default)"),
                },
                validate: None,
            },
            WizardField {
                key: "poll_interval_secs",
                label: "Refresh interval (seconds)",
                help: "How often to repoll the source's RSS. 900s (15 min) \
                       is a polite default — most newsrooms update \
                       throughout the day but feeds won't change faster \
                       than that.",
                required: true,
                kind: WizardFieldKind::Number {
                    default: Some(900.0),
                    range: Some((300.0, 86_400.0)),
                    integer: true,
                },
                validate: None,
            },
            WizardField {
                key: "default_summary_length",
                label: "Default LLM summary length",
                help: "Short = 1 paragraph, Medium = 2 paragraphs, Long = 3. \
                       Toggle live with Ctrl+S in the widget.",
                required: false,
                kind: WizardFieldKind::Choice {
                    options: vec![
                        ChoiceOption {
                            value: "short",
                            label: "Short",
                            help: Some("1 paragraph (3-5 sentences)"),
                        },
                        ChoiceOption {
                            value: "medium",
                            label: "Medium",
                            help: Some("2 paragraphs"),
                        },
                        ChoiceOption {
                            value: "long",
                            label: "Long",
                            help: Some("3 paragraphs"),
                        },
                    ],
                    default: Some("medium"),
                },
                validate: None,
            },
        ],
    }
}

/// Custom TOML renderer. The wizard's default flat renderer would
/// only emit the four scalar fields and skip the catalogue entirely.
/// We instead write the picked template's `[[feeds]]` blocks
/// (default-on entries live, non-default entries commented out) so
/// the generated file is immediately useful and self-documenting.
///
/// Wizard re-runs preserve any hand-edited `[[feeds]]` array from
/// the existing TOML — the template is only consulted when there's
/// no existing file (or the existing file has zero `[[feeds]]`).
/// `shortcuts` is similarly carried forward.
fn render_feeds_toml(
    values: &std::collections::HashMap<String, crate::wizard::descriptor::WizardValue>,
    existing: Option<&str>,
) -> String {
    use crate::wizard::descriptor::WizardValue;

    let source = match values.get("source") {
        Some(WizardValue::Choice(s)) if !s.is_empty() => s.clone(),
        _ => "wsj".to_string(),
    };
    let template = if source == EMPTY_TEMPLATE_ID {
        None
    } else {
        templates::by_id(&source).or_else(|| templates::by_id("wsj"))
    };

    let display_name = match values.get("display_name") {
        Some(WizardValue::Text(t)) => t.trim().to_string(),
        _ => String::new(),
    };
    let poll_interval = match values.get("poll_interval_secs") {
        Some(WizardValue::Number(n)) => *n as i64,
        _ => 900,
    };
    let summary_length = match values.get("default_summary_length") {
        Some(WizardValue::Choice(s)) if !s.is_empty() => s.clone(),
        _ => "medium".to_string(),
    };

    // Pull forward the user's prior `[[feeds]]` blocks, `shortcuts`
    // array, and `commands` array from disk so a wizard re-run never
    // clobbers hand-edits.
    type ExistingState = (
        Option<Vec<(String, String)>>,
        Option<Vec<String>>,
        Option<Vec<String>>,
    );
    let (existing_feeds, existing_shortcuts, existing_commands): ExistingState = existing
        .and_then(|text| toml::from_str::<toml::Value>(text).ok())
        .map(|doc| {
            let feeds = doc.get("feeds").and_then(|v| v.as_array()).map(|arr| {
                arr.iter()
                    .filter_map(|v| {
                        let topic = v.get("topic")?.as_str()?.to_string();
                        let url = v.get("url")?.as_str()?.to_string();
                        Some((topic, url))
                    })
                    .collect::<Vec<_>>()
            });
            let shortcuts = doc.get("shortcuts").and_then(|v| v.as_array()).map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            });
            let commands = doc.get("commands").and_then(|v| v.as_array()).map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            });
            (feeds, shortcuts, commands)
        })
        .unwrap_or((None, None, None));

    let mut out = String::new();
    out.push_str("# Feeds widget — single-source RSS reader.\n");
    out.push_str(
        "# Generated by --setup. Edit [[feeds]] blocks below to add, \n\
         # remove, or rearrange tabs; re-running the wizard preserves \n\
         # your hand-edits.\n\n",
    );

    let template_display_name = template
        .as_ref()
        .map(|t| t.display_name.as_str())
        .unwrap_or("Feeds");
    if display_name.is_empty() {
        out.push_str("# display_name controls the title-bar / dashboard label\n");
        out.push_str("# and the LLM summarizer's house-style name. Leave\n");
        out.push_str("# commented out to fall back to the instance name.\n");
        out.push_str(&format!(
            "# display_name = {}\n",
            toml_string_literal(template_display_name)
        ));
    } else {
        out.push_str(&format!(
            "display_name = {}\n",
            toml_string_literal(&display_name)
        ));
    }
    out.push_str(&format!("poll_interval_secs = {poll_interval}\n"));
    out.push_str(&format!("default_summary_length = \"{summary_length}\"\n"));
    out.push('\n');

    // Shortcuts: preserve existing if any, else seed from the
    // template's default_shortcut_prefs (or omit when Empty).
    if let Some(shortcuts) = existing_shortcuts {
        if !shortcuts.is_empty() {
            out.push_str("# Preferred Shift+<letter> shortcut keys. First-fit:\n");
            out.push_str("# the app claims the first letter not already taken.\n");
            out.push_str("shortcuts = [");
            for (i, s) in shortcuts.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&toml_string_literal(s));
            }
            out.push_str("]\n\n");
        }
    } else if let Some(t) = template.as_ref() {
        if !t.default_shortcut_prefs.is_empty() {
            out.push_str("# Preferred Shift+<letter> shortcut keys. First-fit:\n");
            out.push_str("# the app claims the first letter not already taken.\n");
            out.push_str("shortcuts = [");
            for (i, c) in t.default_shortcut_prefs.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&format!("\"{}\"", c.to_ascii_uppercase()));
            }
            out.push_str("]\n\n");
        }
    }

    // Per-instance command aliases. Each alias adds three names to
    // the dispatcher: `:<alias>`, `:<alias>-summary`,
    // `:<alias>-refresh`. Kind-level `:feeds*` commands are always
    // available regardless.
    if let Some(commands) = existing_commands {
        if !commands.is_empty() {
            out.push_str(
                "# Per-instance command aliases. `commands = [\"wsj\"]` makes\n\
                 # :wsj <terms>, :wsj-summary, and :wsj-refresh route to this\n\
                 # instance on top of the kind-level :feeds* commands.\n",
            );
            out.push_str("commands = [");
            for (i, c) in commands.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&toml_string_literal(c));
            }
            out.push_str("]\n\n");
        }
    } else if let Some(t) = template.as_ref() {
        if !t.default_commands.is_empty() {
            out.push_str(
                "# Per-instance command aliases. Each entry adds :<alias>,\n\
                 # :<alias>-summary, and :<alias>-refresh as additional names\n\
                 # for this instance. Kind-level :feeds* commands stay\n\
                 # available regardless.\n",
            );
            out.push_str("commands = [");
            for (i, c) in t.default_commands.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&toml_string_literal(c));
            }
            out.push_str("]\n\n");
        }
    }

    // Feeds block. Priority: existing on-disk feeds → template feeds
    // (default-on live, others commented out) → Empty (example only).
    if let Some(feeds) = existing_feeds.filter(|f| !f.is_empty()) {
        out.push_str("# Active feeds — each [[feeds]] block becomes a tab.\n");
        for (topic, url) in &feeds {
            out.push_str("[[feeds]]\n");
            out.push_str(&format!("topic = {}\n", toml_string_literal(topic)));
            out.push_str(&format!("url = {}\n", toml_string_literal(url)));
            out.push('\n');
        }
    } else if let Some(t) = template.as_ref() {
        out.push_str(
            "# Active feeds — each [[feeds]] block becomes a tab.\n\
             # Comment out a block to disable that tab.\n\n",
        );
        for f in &t.feeds {
            if f.default {
                out.push_str("[[feeds]]\n");
                out.push_str(&format!("topic = {}\n", toml_string_literal(&f.topic)));
                out.push_str(&format!("url = {}\n", toml_string_literal(&f.url)));
                out.push('\n');
            }
        }
        let non_defaults: Vec<&templates::TemplateFeed> =
            t.feeds.iter().filter(|f| !f.default).collect();
        if !non_defaults.is_empty() {
            out.push_str(
                "# Additional topics available — uncomment to enable.\n",
            );
            for f in non_defaults {
                out.push_str("# [[feeds]]\n");
                out.push_str(&format!(
                    "# topic = {}\n",
                    toml_string_literal(&f.topic)
                ));
                out.push_str(&format!("# url = {}\n", toml_string_literal(&f.url)));
                out.push('\n');
            }
        }
    } else {
        // Empty starter — give the user a commented example to
        // crib from.
        out.push_str(
            "# Active feeds — each [[feeds]] block becomes a tab. Uncomment\n\
             # the example below and replace topic/url with your own source.\n\n\
             # [[feeds]]\n\
             # topic = \"Top Stories\"\n\
             # url = \"https://example.com/rss\"\n",
        );
    }

    out
}

/// Minimal TOML string-literal quoter. Uses double-quoted form with
/// backslash escaping for `"` and `\`. Sufficient for the small
/// values the feeds widget writes (topic labels, source ids,
/// display names).
fn toml_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_length_cycles_short_medium_long() {
        assert_eq!(SummaryLength::Short.next(), SummaryLength::Medium);
        assert_eq!(SummaryLength::Medium.next(), SummaryLength::Long);
        assert_eq!(SummaryLength::Long.next(), SummaryLength::Short);
    }

    #[test]
    fn summary_length_max_tokens_increases_with_length() {
        assert!(SummaryLength::Short.max_tokens() < SummaryLength::Medium.max_tokens());
        assert!(SummaryLength::Medium.max_tokens() < SummaryLength::Long.max_tokens());
    }

    #[test]
    fn summary_keys_differ_by_length() {
        let url = "https://www.wsj.com/articles/xyz";
        assert_ne!(
            summary_key(url, SummaryLength::Short),
            summary_key(url, SummaryLength::Medium)
        );
        assert_ne!(
            summary_cache_key(url, SummaryLength::Short),
            summary_cache_key(url, SummaryLength::Medium)
        );
    }

    #[test]
    fn split_widths_gives_list_60pct_when_room_for_summary() {
        // 200 cols → list = 120 (60%), summary = 80.
        let (l, s) = split_list_summary_widths(200);
        assert_eq!(l, 120);
        assert_eq!(s, 80);
    }

    #[test]
    fn split_widths_caps_list_to_preserve_summary_min() {
        // 90 cols, 60% = 54 → would leave 36 for summary which is
        // < 45 minimum. List must shrink to 90-45 = 45.
        let (l, s) = split_list_summary_widths(90);
        assert_eq!(l, 45);
        assert_eq!(s, 45);
    }

    #[test]
    fn split_widths_collapses_when_total_too_narrow() {
        // 50 cols can't satisfy 45-col summary + 20-col list min.
        let (l, s) = split_list_summary_widths(50);
        assert_eq!(s, 0, "summary collapsed");
        assert_eq!(l, 50, "list takes everything");
    }

    #[test]
    fn wrap_title_returns_single_line_when_fits() {
        let out = wrap_title("hello world", 20, 3);
        assert_eq!(out, vec!["hello world"]);
    }

    #[test]
    fn wrap_title_wraps_on_word_boundary() {
        let out = wrap_title("the quick brown fox jumps", 10, 3);
        for line in &out {
            assert!(line.chars().count() <= 10, "line over budget: {line:?}");
        }
        // Words shouldn't be split mid-word.
        assert!(out
            .iter()
            .all(|l| !l.contains(" the ") || l.starts_with("the ")));
    }

    #[test]
    fn wrap_title_caps_at_max_lines_with_ellipsis() {
        let out = wrap_title(
            "one two three four five six seven eight nine ten eleven",
            8,
            3,
        );
        assert_eq!(out.len(), 3);
        assert!(out[2].ends_with('…'), "last line should end with …");
    }

    #[test]
    fn wrap_title_handles_very_long_word() {
        // A word longer than width gets mid-broken.
        let out = wrap_title("verylongunbreakableword", 5, 2);
        assert_eq!(out[0].chars().count(), 5);
    }

    #[test]
    fn format_relative_time_buckets_cover_the_common_ranges() {
        use chrono::Duration as ChronoDuration;
        let now = chrono::Utc::now();
        assert_eq!(format_relative_time(now, now), "now");
        assert_eq!(
            format_relative_time(now - ChronoDuration::seconds(30), now),
            "now"
        );
        assert_eq!(
            format_relative_time(now - ChronoDuration::minutes(5), now),
            "5m"
        );
        assert_eq!(
            format_relative_time(now - ChronoDuration::minutes(59), now),
            "59m"
        );
        assert_eq!(
            format_relative_time(now - ChronoDuration::hours(3), now),
            "3h"
        );
        assert_eq!(
            format_relative_time(now - ChronoDuration::hours(23), now),
            "23h"
        );
        assert_eq!(
            format_relative_time(now - ChronoDuration::days(2), now),
            "2d"
        );
        assert_eq!(
            format_relative_time(now - ChronoDuration::days(6), now),
            "6d"
        );
        assert_eq!(
            format_relative_time(now - ChronoDuration::days(14), now),
            "2w"
        );
    }

    #[test]
    fn format_relative_time_falls_back_to_month_day_after_a_few_weeks() {
        use chrono::Duration as ChronoDuration;
        let now = chrono::Utc::now();
        let out = format_relative_time(now - ChronoDuration::days(60), now);
        // Expect "Mon DD" — 6 chars, contains a space.
        assert_eq!(out.chars().count(), 6, "{out:?}");
        assert!(out.contains(' '));
    }

    #[test]
    fn format_relative_time_handles_future_timestamps() {
        use chrono::Duration as ChronoDuration;
        let now = chrono::Utc::now();
        // Article published "later" than now → clamp to "now"
        // rather than emitting negative buckets.
        assert_eq!(
            format_relative_time(now + ChronoDuration::minutes(5), now),
            "now"
        );
    }

    #[test]
    fn article_prefix_width_matches_marker_plus_topic_tag() {
        // "▶ " (counted as 2 chars in display terms — `▶` + space) +
        // "[" + topic + "]" + trailing " ".
        assert_eq!(article_prefix_width("Tech"), 2 + 1 + 4 + 1 + 1);
        assert_eq!(article_prefix_width("Politics"), 2 + 1 + 8 + 1 + 1);
    }
}
