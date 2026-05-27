pub mod provider;

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use serde::Deserialize;

use crate::ui::{decorate_title, focus_border_style};

use super::{AppContext, EventResult, Widget};

use provider::{Article, FeedConfig, NewsProvider, RssProvider, Topic};

#[derive(Debug, Clone, Deserialize)]
pub struct NewsConfig {
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    #[serde(default)]
    pub feeds: Vec<FeedConfig>,

    #[serde(default)]
    pub topics: Vec<Topic>,
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
        }
    }
}

#[derive(Default)]
struct NewsState {
    articles: Vec<Article>,
    selected: usize,
    scroll: usize,
    /// When true, the selected article renders its full summary (up to
    /// `MAX_SUMMARY_LINES` wrapped lines) instead of the one-line excerpt.
    expanded: bool,
    /// Index into the widget's `filter_tabs` list. 0 is always `All`.
    active_filter_idx: usize,
    last_error: Option<String>,
    last_attempt: Option<Instant>,
    inflight: bool,
}

const MAX_SUMMARY_LINES: usize = 6;
const ALL_TAB_LABEL: &str = "All";

pub struct NewsWidget {
    id: String,
    provider: Arc<dyn NewsProvider>,
    state: Arc<Mutex<NewsState>>,
    poll_interval: Duration,
    feeds_configured: bool,
    /// Tabs across the top of the cell. Index 0 is always `All`; the rest
    /// mirror the topic labels in news.toml.
    filter_tabs: Vec<String>,
}

impl NewsWidget {
    pub fn with_config(config: NewsConfig) -> Self {
        let feeds_configured = !config.feeds.is_empty();
        let mut filter_tabs = vec![ALL_TAB_LABEL.to_string()];
        filter_tabs.extend(config.topics.iter().map(|t| t.label.clone()));
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
        }
    }

    fn cycle_filter(&mut self, forward: bool) {
        if self.filter_tabs.len() <= 1 {
            return;
        }
        let mut st = self.state.lock().expect("news state poisoned");
        let n = self.filter_tabs.len();
        st.active_filter_idx = if forward {
            (st.active_filter_idx + 1) % n
        } else {
            (st.active_filter_idx + n - 1) % n
        };
        st.selected = 0;
        st.scroll = 0;
    }

    /// Mirrors the inner-area split used by `render`: tab bar on top (2 rows
    /// when topics exist, otherwise 1 for padding), single-row footer at the
    /// bottom, list fills the middle.
    fn split_inner(&self, inner: Rect) -> (Rect, Rect, Rect) {
        let has_tabs = self.filter_tabs.len() > 1;
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
        let mut x: u16 = tab_area.x + 1; // leading space
        for (i, label) in self.filter_tabs.iter().enumerate() {
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
        if active == 0 {
            return st.articles.clone();
        }
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
        let idx = self.state.lock().expect("news state poisoned").active_filter_idx;
        self.filter_tabs
            .get(idx)
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
                    let prev_url = st.articles.get(st.selected).map(|a| a.url.clone());
                    st.articles = articles;
                    st.last_error = None;
                    // Try to keep the same article selected across refreshes.
                    if let Some(url) = prev_url {
                        if let Some(idx) = st.articles.iter().position(|a| a.url == url) {
                            st.selected = idx;
                        } else {
                            st.selected = 0;
                            st.scroll = 0;
                        }
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
        let mut st = self.state.lock().expect("news state poisoned");
        if st.articles.is_empty() {
            return;
        }
        let len = st.articles.len() as isize;
        let new_idx = (st.selected as isize + delta).clamp(0, len - 1);
        st.selected = new_idx as usize;
    }

    fn jump_to(&mut self, idx: usize) {
        let mut st = self.state.lock().expect("news state poisoned");
        if st.articles.is_empty() {
            return;
        }
        st.selected = idx.min(st.articles.len() - 1);
    }

    fn open_selected(&self) {
        let url = {
            let st = self.state.lock().expect("news state poisoned");
            st.articles.get(st.selected).map(|a| a.url.clone())
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
        let (all_articles, selected, mut scroll, expanded, active_filter_idx, inflight, last_error) = {
            let st = self.state.lock().expect("news state poisoned");
            (
                st.articles.clone(),
                st.selected,
                st.scroll,
                st.expanded,
                st.active_filter_idx,
                st.inflight,
                st.last_error.clone(),
            )
        };

        // Apply the active filter (idx 0 = All; anything else matches a topic).
        let active_filter: Option<&str> = if active_filter_idx == 0 {
            None
        } else {
            self.filter_tabs.get(active_filter_idx).map(String::as_str)
        };
        let articles: Vec<Article> = match active_filter {
            None => all_articles,
            Some(label) => all_articles
                .into_iter()
                .filter(|a| a.topics.iter().any(|t| t == label))
                .collect(),
        };

        let title = if articles.is_empty() {
            "News".to_string()
        } else {
            format!("News — {} articles", articles.len())
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(focus_border_style(focused))
            .title(Span::styled(
                decorate_title(focused, &title),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Reserve a top tab-bar row (only when we have configured topics so
        // the user actually has something to filter on), a bottom footer row,
        // and a blank row between the tabs and the list.
        let has_tabs = self.filter_tabs.len() > 1;
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
            let mut spans: Vec<Span<'_>> = Vec::with_capacity(self.filter_tabs.len() * 2);
            spans.push(Span::raw(" "));
            for (i, label) in self.filter_tabs.iter().enumerate() {
                let is_active = i == active_filter_idx;
                let style = if is_active {
                    // Yellow for the active tab so it matches the selected-
                    // headline color — "yellow = active selection".
                    Style::default()
                        .fg(Color::LightYellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().add_modifier(Modifier::DIM)
                };
                spans.push(Span::styled(format!("[{label}]"), style));
                if i + 1 < self.filter_tabs.len() {
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

            // How many rows would this item consume?
            let summary_lines: Vec<String> = if expand_this {
                article
                    .summary
                    .as_deref()
                    .map(|s| wrap_text(s, inner_width.saturating_sub(3), MAX_SUMMARY_LINES))
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let needed = ROWS_PER_ITEM as u16 + summary_lines.len() as u16;
            if rows_emitted + needed > list_height {
                break;
            }

            let prefix = if is_selected { "▸ " } else { "  " };
            let title_style = if is_selected {
                // LightYellow matches the calendar tear-off-sheet date — the
                // selected article should pop the same way.
                Style::default()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD)
            } else if focused {
                // Cyan only while the widget itself is focused — when focus
                // moves away, the inactive cell stays calm with default text.
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            };
            let title_room = inner_width.saturating_sub(2);
            lines.push(Line::from(vec![
                Span::styled(prefix, title_style),
                Span::styled(truncate(&article.title, title_room), title_style),
            ]));

            // Row 2: 3-space indent + dim metadata. When expanded we drop the
            // summary excerpt from this row (it has its own block underneath).
            let mut meta = format!("   {} · {}", age_label(now, article.published), article.source);
            if !article.topics.is_empty() {
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
                Style::default().add_modifier(Modifier::DIM),
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
            Style::default().add_modifier(Modifier::DIM),
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

    fn handle_command(&mut self, _cmd: &str, _args: &[&str]) -> Result<bool> {
        Ok(false)
    }

    fn config(&self) -> serde_json::Value {
        serde_json::json!({
            "poll_interval_secs": self.poll_interval.as_secs(),
        })
    }

    fn apply_config(&mut self, config: serde_json::Value) -> Result<()> {
        let new_config: NewsConfig =
            serde_json::from_value(config).context("invalid news config payload")?;
        *self = Self::with_config(new_config);
        Ok(())
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
