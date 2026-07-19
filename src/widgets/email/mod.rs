// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Email widget — read-only feed of recent messages across Gmail / Outlook.
//!
//! Closely mirrors the News widget (provider trait, expand/select/open flow,
//! optional LLM summarization, refresh polling). Key differences:
//!   - "Folders" replace News's topic tabs.
//!   - Server-side read state is OR'd with a local "seen via glint" cache
//!     so glint never has to write to the server.
//!   - Bodies come from the provider's body endpoint, with HTML→text fallback.

pub mod gmail;
pub mod html_strip;
pub mod imap;
pub mod outlook;
pub mod provider;
pub mod seen_store;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::text::{pad_or_truncate, truncate, wrap};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Local, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};
use serde::Deserialize;

use crate::cache::ScopedCache;
use crate::llm::{LlmMessage, LlmProvider, LlmRequest, Role};
use crate::theme::{ColorScheme, Theme};
use crate::ui::{apply_title_row, MetadataEmphasis};

use super::{AppContext, EventResult, ViewTier, Widget};

use provider::{EmailMessage, EmailProvider};
use seen_store::SeenStore;

const MAX_SUMMARY_LINES: usize = 5;
const MAX_PER_FOLDER: usize = 100;
/// Minimum list-area content width before the list splits into list + read pane.
/// Intentionally very wide: the read pane is only shown when the widget is
/// genuinely large (zoomed pane or a wide dedicated cell). At 175 cols and
/// below the list fills the full width; the read pane only fires at ≥ 176.
/// (Per-widget deviation — see the ViewTier convention-sweep note.)
const READ_PANE_MIN_WIDTH: u16 = 176;

const SUMMARY_SYSTEM_PROMPT: &str = "You are a concise email summarizer. \
Given a sender, subject, and the message body, return a neutral summary in at \
most 4 sentences. Capture the asks, decisions, and dates only. Do not editorialize, \
do not greet, do not use markdown. If the input is too sparse to summarize \
faithfully, respond with the single sentence: \"Insufficient content to summarize.\"";

#[derive(Debug, Clone)]
enum SummaryState {
    Requested,
    Ready(String),
    Failed,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EmailConfig {
    /// `"outlook"` or `"gmail"`. Anything else renders a placeholder.
    #[serde(default = "default_provider")]
    pub provider: String,

    /// Pull messages received within the last N days.
    #[serde(default = "default_latest_days")]
    pub latest_days: u32,

    #[serde(default = "default_refresh_minutes")]
    pub refresh_minutes: u64,

    /// Gmail label ids (`INBOX`, `SENT`, …) or Outlook well-known names
    /// (`inbox`, `sentitems`, …).
    #[serde(default = "default_folders")]
    pub folders: Vec<String>,

    /// On-demand message summarisation when an LLM provider is configured.
    /// Press `s` on an expanded message.
    #[serde(default)]
    pub summarize_with_llm: bool,

    /// Pre-populates the title's address before the provider's `/me` lookup
    /// resolves. The lookup still runs and overwrites this once it returns.
    #[serde(default)]
    pub account_address: Option<String>,

    #[serde(default)]
    pub colors: ColorScheme,

    /// `Shift+<letter>` focus shortcuts; falls back to the letters in "email".
    #[serde(default)]
    pub shortcuts: Vec<char>,
}

fn default_provider() -> String {
    "outlook".into()
}
fn default_latest_days() -> u32 {
    7
}
fn default_refresh_minutes() -> u64 {
    5
}
fn default_folders() -> Vec<String> {
    vec!["INBOX".into()]
}

impl Default for EmailConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            latest_days: default_latest_days(),
            refresh_minutes: default_refresh_minutes(),
            folders: default_folders(),
            summarize_with_llm: false,
            account_address: None,
            colors: ColorScheme::default(),
            shortcuts: Vec::new(),
        }
    }
}

#[derive(Default)]
struct EmailState {
    /// `Arc<EmailMessage>` so the per-render `Vec::clone()` is O(N)
    /// atomic increments instead of O(N) deep EmailMessage copies. A
    /// typical inbox snapshot is 50+ messages × multiple Strings each,
    /// previously cloned wholesale every time the clock-driven 1 Hz
    /// redraw fired.
    messages: Vec<Arc<EmailMessage>>,
    selected: usize,
    scroll: usize,
    expanded: bool,
    /// Index into `folders`. 0 is always the first configured folder.
    active_folder_idx: usize,
    last_error: Option<String>,
    /// Two-tier polling: while `account` is unresolved we retry on a
    /// fast 30 s cadence (capped so a failing profile endpoint
    /// doesn't spin); once the account address lands we fall back
    /// to the configured mail-refresh interval. Both stamp on every
    /// spawn_refresh since `ensure_account` piggybacks on
    /// `fetch_recent`.
    account_poll: crate::polling::PollTracker,
    mail_poll: crate::polling::PollTracker,
    inflight: bool,
    /// Cached account address (e.g. "alice@example.com") for the title row.
    /// Populated lazily from the provider once the first fetch resolves.
    account: Option<String>,
    /// Per-message LLM summarization state, keyed by message id.
    summaries: std::collections::HashMap<String, SummaryState>,
    /// Per-message view preference, keyed by message id. `true` means
    /// "prefer the LLM summary"; missing/`false` means "show the raw
    /// body" (the historical default). Set by `s`: first press flips
    /// to summary (and kicks off the request if needed); subsequent
    /// presses toggle without re-firing the LLM (cached summary is
    /// reused).
    summary_view: std::collections::HashMap<String, bool>,
    /// Last-rendered row layout for the message list: `(msg_idx, row_start, row_end_exclusive)`
    /// in offsets relative to the list_area's top. Populated on every
    /// render so `handle_mouse` can map a click row back to a message
    /// without recomputing wrap heights.
    row_layout: Vec<(usize, u16, u16)>,
    /// Last-rendered list_area Rect — used together with `row_layout` to
    /// translate raw mouse coordinates into a clicked message index.
    last_list_area: Option<Rect>,
    /// True when the last render painted a read pane (list_area.width ≥
    /// READ_PANE_MIN_WIDTH at an Expanded/Full tier). Written by the render
    /// path; read by handle_key to suppress the inline `e`/Enter expand
    /// while the full body is already visible in the read pane.
    read_pane_active: bool,
    /// Display-state dirty bit drained by `take_dirty`. Set true by
    /// every async-task / tick-time mutation site so the main loop's
    /// dirty-flag gate triggers a redraw.
    dirty: bool,
}

const CACHE_KEY_MESSAGES: &str = "messages";

/// Cache key for the resolved account email address. Persisted with a
/// very long TTL — Gmail / Outlook addresses effectively never change
/// for a given OAuth token, so re-fetching `/me` on every launch just
/// blocks the title row on a network round-trip. We invalidate the
/// cache automatically when the user re-authorizes (token changes,
/// new account possible) — that path explicitly clears the entry.
const CACHE_KEY_ACCOUNT_ADDRESS: &str = "account_address";

/// Cache-key namespace for LLM-generated message summaries. Each summary is
/// keyed by `summary-<sha256(id)>`. Provider IDs are filesystem-safe today
/// (Gmail hex, Outlook alphanumeric) but hashing keeps the namespace bounded
/// and future-provider-proof. Email bodies don't change post-delivery so a
/// cached summary is valid until the user explicitly clears the cache.
const SUMMARY_CACHE_PREFIX: &str = "summary-";

fn summary_cache_key(id: &str) -> String {
    crate::cache::short_hash_key(SUMMARY_CACHE_PREFIX, id)
}

pub struct EmailWidget {
    id: String,
    instance: String,
    display_name_cache: String,
    provider: Arc<EmailProviderHandle>,
    state: Arc<Mutex<EmailState>>,
    /// In-memory + on-disk seen-set, shared with the refresh task so it can
    /// react to expand-induced changes without races.
    seen: Arc<Mutex<SeenStore>>,
    folders: Vec<String>,
    latest_days: u32,
    summarize_with_llm: bool,
    llm: Option<Arc<dyn LlmProvider>>,
    /// "outlook" / "gmail" / "none" — drives the bracketed source tag in the title.
    provider_label: String,
    /// True when no real provider was configurable (missing token, missing
    /// client config, unknown name). The widget shows a placeholder instead
    /// of an empty list.
    provider_ready: bool,
    /// Diagnostic surfaced under the placeholder when `provider_ready` is
    /// false. Walk-through tells the user what to run.
    auth_hint: Option<String>,
    app_theme: Arc<Theme>,
    colors_override: ColorScheme,
    theme: Theme,
    shortcut: Option<char>,
    shortcut_prefs: Vec<char>,
    /// Persistent cache of the merged message list across configured folders.
    cache: ScopedCache,
}

/// Thin wrapper so the widget can fetch a fresh `cached_account()` snapshot
/// from either provider implementation without having to widen the
/// `EmailProvider` trait.
enum EmailProviderHandle {
    Outlook(outlook::OutlookEmailProvider),
    Gmail(gmail::GmailProvider),
    Imap(imap::ImapProvider),
    /// Placeholder used when no provider could be constructed. Holds nothing;
    /// `fetch_recent` returns an empty list so the widget renders a friendly
    /// placeholder instead of crashing.
    Empty,
}

impl EmailProviderHandle {
    fn as_provider(&self) -> Option<&dyn EmailProvider> {
        match self {
            Self::Outlook(p) => Some(p),
            Self::Gmail(p) => Some(p),
            Self::Imap(p) => Some(p),
            Self::Empty => None,
        }
    }

    fn cached_account(&self) -> Option<String> {
        match self {
            Self::Outlook(p) => p.cached_account(),
            Self::Gmail(p) => p.cached_account(),
            Self::Imap(p) => p.cached_account(),
            Self::Empty => None,
        }
    }

    /// Prime the provider's in-memory account cache from a persisted
    /// value (loaded from the on-disk scoped cache or seeded in
    /// email.toml). Skips the next `/me` round-trip so the title row
    /// paints instantly on launch.
    fn seed_account_cache(&self, address: &str) {
        match self {
            Self::Outlook(p) => p.seed_account_cache(address),
            Self::Gmail(p) => p.seed_account_cache(address),
            Self::Imap(p) => p.seed_account_cache(address),
            Self::Empty => {}
        }
    }
}

impl EmailWidget {
    #[cfg(test)]
    pub fn with_config(config: EmailConfig) -> Self {
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
        config: EmailConfig,
        llm: Option<Arc<dyn LlmProvider>>,
        app_theme: Arc<Theme>,
        cache: ScopedCache,
    ) -> Self {
        let folders = if config.folders.is_empty() {
            default_folders()
        } else {
            config.folders.clone()
        };
        let (provider, provider_label, provider_ready, auth_hint) =
            build_provider(&config.provider);

        let colors_override = config.colors.clone();
        let theme = app_theme.with_overrides(&colors_override);
        let shortcut_prefs = if config.shortcuts.is_empty() {
            vec!['e', 'm', 'a', 'i', 'l']
        } else {
            config.shortcuts.clone()
        };

        let id = if instance == "main" {
            "email".to_string()
        } else {
            format!("email@{instance}")
        };
        let display_name_cache = if instance == "main" {
            "Email".to_string()
        } else {
            format!("Email ({instance})")
        };

        // Seed the seen-store using the provider+account pair. We don't know
        // the account yet (the /me call lands on first refresh), so start
        // with a stable "_unknown_" placeholder file; on the first
        // `update_account_cache` call after a successful fetch we transparently
        // swap to the real per-account file. Worst case: a single session's
        // worth of seen state goes to the placeholder file — a fine trade
        // for keeping the widget responsive on cold start.
        let seen = SeenStore::load(&provider_label, "_unknown_").unwrap_or_else(|err| {
            tracing::warn!(error = %err, "failed to load email seen-store, starting empty");
            // SAFETY: SeenStore::load only fails on disk errors; we fall
            // back to a fresh in-memory-only store by trying again with a
            // tmp tag (which will likely succeed; if not, we accept the
            // panic since we've already logged).
            SeenStore::load(&provider_label, "_unknown_").expect("seen-store fallback failed")
        });

        let poll_interval = Duration::from_secs(config.refresh_minutes.max(1) * 60);
        // Seed messages from cache so the first render shows the prior
        // session's inbox while the refresh runs in the background.
        // The account address has its own long-lived cache entry — the
        // address effectively never changes for a given OAuth token, so
        // caching it lets the title row paint with the user's email
        // immediately on launch instead of "(loading…)" until /me
        // returns. `account_address` in email.toml still wins so users
        // can override the cached value by hand.
        let cached_address = cache
            .load::<String>(CACHE_KEY_ACCOUNT_ADDRESS)
            .map(|e| e.value);
        let initial_account = config.account_address.clone().or(cached_address.clone());
        let mut initial_state = EmailState {
            account: initial_account.clone(),
            // Fast retry while account is being resolved (~30s) plus
            // the regular mail-refresh cadence. Both get stamped on
            // every spawn_refresh; is_due picks which one to consult
            // based on whether `account` has landed yet.
            account_poll: crate::polling::PollTracker::new(Duration::from_secs(30)),
            mail_poll: crate::polling::PollTracker::new(poll_interval),
            ..EmailState::default()
        };
        // Seed the provider's in-memory cache too so the first
        // fetch_recent's `ensure_account` is a no-op and doesn't hit
        // the network just to re-derive what we already know.
        if let Some(addr) = &initial_account {
            provider.seed_account_cache(addr);
        }
        if let Some(entry) = cache.load::<Vec<EmailMessage>>(CACHE_KEY_MESSAGES) {
            initial_state.mail_poll.seed_from_cache_age(entry.age());
            // If we have cached messages, an account-resolution retry
            // shouldn't fire instantly either — pretend account was
            // checked recently so we don't double-hit on launch.
            initial_state.account_poll.seed_from_cache_age(entry.age());
            let mut messages = entry.value;
            for m in &mut messages {
                truncate_body_in_place(&mut m.plain_body, 4096);
            }
            initial_state.messages = messages.into_iter().map(Arc::new).collect();
        }
        // Spread first-fire phases across instances so multiple
        // 60s-cadence widgets don't all hit the network in the same
        // 250ms tick. `account_poll` runs at 30s; jittering both keeps
        // the two-stage startup from synchronising either.
        initial_state
            .mail_poll
            .apply_jitter(&format!("email@{instance}"));
        initial_state
            .account_poll
            .apply_jitter(&format!("email-account@{instance}"));

        Self {
            id,
            instance,
            display_name_cache,
            provider: Arc::new(provider),
            state: Arc::new(Mutex::new(initial_state)),
            seen: Arc::new(Mutex::new(seen)),
            folders,
            latest_days: config.latest_days.max(1),
            summarize_with_llm: config.summarize_with_llm,
            llm,
            provider_label,
            provider_ready,
            auth_hint,
            app_theme,
            colors_override,
            theme,
            shortcut: None,
            shortcut_prefs,
            cache,
        }
    }

    fn filtered_messages(&self) -> Vec<Arc<EmailMessage>> {
        let st = self.state.lock().expect("email state poisoned");
        let folder = self
            .folders
            .get(
                st.active_folder_idx
                    .min(self.folders.len().saturating_sub(1)),
            )
            .cloned()
            .unwrap_or_default();
        st.messages
            .iter()
            .filter(|m| m.folder.eq_ignore_ascii_case(&folder))
            .cloned()
            .collect()
    }

    fn is_due(&self) -> bool {
        let st = self.state.lock().expect("email state poisoned");
        if st.inflight {
            return false;
        }
        // Two-tier policy: while the account address is still being
        // resolved, retry on `account_poll`'s fast 30s cadence so
        // the title row doesn't sit on "(loading…)" for the full
        // mail interval. Once the account lands, switch to the
        // configured mail-refresh interval via `mail_poll`. The
        // tracker's `is_due()` handles the elapsed-check uniformly.
        if st.account.is_none() {
            return st.account_poll.is_due();
        }
        st.mail_poll.is_due()
    }

    fn mark_dirty(&self) {
        let mut st = self.state.lock().expect("email state poisoned");
        // User-triggered refresh: dirty both timers so neither stops
        // the next fetch.
        st.account_poll.mark_dirty();
        st.mail_poll.mark_dirty();
    }

    fn spawn_refresh(&self) {
        if !self.provider_ready {
            return;
        }
        {
            let mut st = self.state.lock().expect("email state poisoned");
            st.inflight = true;
            // ensure_account piggybacks on fetch_recent, so a single
            // refresh advances both timers.
            st.account_poll.mark_attempted();
            st.mail_poll.mark_attempted();
            st.dirty = true;
        }
        let provider = self.provider.clone();
        let state = self.state.clone();
        let folders = self.folders.clone();
        let latest_days = self.latest_days;
        let cache = self.cache.clone();
        tokio::spawn(async move {
            let Some(prov) = provider.as_provider() else {
                let mut st = state.lock().expect("email state poisoned");
                st.inflight = false;
                st.dirty = true;
                return;
            };
            let since = Utc::now() - chrono::Duration::days(latest_days as i64);
            let mut messages: Vec<EmailMessage> = Vec::new();
            let mut last_error: Option<String> = None;
            for folder in &folders {
                match prov.fetch_recent(folder, since, MAX_PER_FOLDER).await {
                    Ok(mut chunk) => messages.append(&mut chunk),
                    Err(err) => {
                        tracing::warn!(folder = %folder, error = %err, "email fetch failed");
                        last_error = Some(format!("{folder}: {err}"));
                    }
                }
            }
            // Sort newest-first across all folders.
            messages.sort_by_key(|m| std::cmp::Reverse(m.received));
            // Trim oversized bodies. The expanded view caps at
            // `MAX_SUMMARY_LINES` (5) and full-message read happens via
            // `o` opening the user's mail client, so we never paint
            // more than the first ~400 chars in glint anyway. 4 KB is
            // ample headroom for the visible snippet + LLM summary
            // context, and drops mailing-list bodies that routinely
            // ship 50+ KB of HTML-stripped text per message.
            for m in &mut messages {
                truncate_body_in_place(&mut m.plain_body, 4096);
            }
            // Persist before swapping state so a concurrent reload sees the
            // same payload either way. Errors are warned and ignored.
            if last_error.is_none() {
                if let Err(err) = cache.store(CACHE_KEY_MESSAGES, &messages) {
                    tracing::warn!(error = %err, "email cache store failed");
                }
            }
            // Capture the just-refreshed account address (the providers populate
            // their cache during fetch_recent). Persist it so the next
            // launch paints the title row instantly instead of waiting
            // for `/me` to resolve again.
            let account = provider.cached_account();
            if let Some(addr) = &account {
                if let Err(err) = cache.store(CACHE_KEY_ACCOUNT_ADDRESS, addr) {
                    tracing::warn!(error = %err, "email account-address cache store failed");
                }
            }
            let mut st = state.lock().expect("email state poisoned");
            st.inflight = false;
            st.messages = messages.into_iter().map(Arc::new).collect();
            st.last_error = last_error;
            if account.is_some() {
                st.account = account;
            }
            st.dirty = true;
        });
    }

    fn move_selection(&mut self, delta: isize) {
        let filtered = self.filtered_messages();
        if filtered.is_empty() {
            return;
        }
        let new_idx;
        let was_expanded;
        {
            let mut st = self.state.lock().expect("email state poisoned");
            new_idx = (st.selected as isize + delta).clamp(0, filtered.len() as isize - 1) as usize;
            st.selected = new_idx;
            was_expanded = st.expanded;
        }
        // When the user is in expanded mode, navigating up/down is
        // visually "opening" each message they land on — they can see
        // the body in the expanded pane. Mark seen the same way
        // toggle_expand does so the unread dot disappears after a
        // single scroll-by visit. Without this the unread state
        // lingered until the user explicitly collapsed + re-expanded.
        if was_expanded {
            if let Some(msg) = filtered.get(new_idx) {
                self.mark_seen_if_unseen(&msg.id);
            }
        }
    }

    fn jump_to(&mut self, idx: usize) {
        let filtered = self.filtered_messages();
        if filtered.is_empty() {
            return;
        }
        let new_idx;
        let was_expanded;
        {
            let mut st = self.state.lock().expect("email state poisoned");
            new_idx = idx.min(filtered.len() - 1);
            st.selected = new_idx;
            was_expanded = st.expanded;
        }
        if was_expanded {
            if let Some(msg) = filtered.get(new_idx) {
                self.mark_seen_if_unseen(&msg.id);
            }
        }
    }

    /// Persist a seen-mark for `id` to the seen-store. Logged + ignored
    /// on failure (a stale seen-store is annoying but not data loss —
    /// the user's mail server-side unread state is the canonical
    /// source, and the next fetch will reconcile).
    fn mark_seen_if_unseen(&self, id: &str) {
        let mut seen = self.seen.lock().expect("seen-store poisoned");
        if let Err(err) = seen.mark_seen(id) {
            tracing::warn!(error = %err, id = %id, "failed to persist seen state");
        }
    }

    fn cycle_folder(&mut self, forward: bool) {
        if self.folders.len() <= 1 {
            return;
        }
        let mut st = self.state.lock().expect("email state poisoned");
        let n = self.folders.len();
        st.active_folder_idx = if forward {
            (st.active_folder_idx + 1) % n
        } else {
            (st.active_folder_idx + n - 1) % n
        };
        st.selected = 0;
        st.scroll = 0;
        st.expanded = false;
    }

    fn open_selected(&self) {
        let filtered = self.filtered_messages();
        let url = {
            let st = self.state.lock().expect("email state poisoned");
            filtered.get(st.selected).and_then(|m| m.web_url.clone())
        };
        if let Some(url) = url {
            if let Err(err) = open::that(&url) {
                tracing::warn!(error = %err, url = %url, "failed to open email URL");
            }
        }
    }

    /// Toggle expanded state on the selected message. When expanding,
    /// also mark the message as seen-via-glint and persist the
    /// seen-store. (Subsequent scrolls inside expanded mode also mark
    /// — that's handled inside [`Self::move_selection`] /
    /// [`Self::jump_to`].)
    fn toggle_expand(&mut self) {
        let filtered = self.filtered_messages();
        let selected_id: Option<String> = {
            let st = self.state.lock().expect("email state poisoned");
            filtered.get(st.selected).map(|m| m.id.clone())
        };
        let expanded_now = {
            let mut st = self.state.lock().expect("email state poisoned");
            if st.messages.is_empty() {
                return;
            }
            st.expanded = !st.expanded;
            st.expanded
        };
        if expanded_now {
            if let Some(id) = selected_id {
                self.mark_seen_if_unseen(&id);
            }
        }
    }

    /// Press-`s` entry point. Drives the per-message Body ⇄ Summary
    /// toggle with a side-effect of expanding (and auto-marking-seen)
    /// when the user hits it from collapsed mode:
    ///
    /// - **Collapsed**: expand, mark seen, switch to Summary view, fire
    ///   the LLM (cache-hit returns instantly).
    /// - **Expanded + currently Body**: switch to Summary; if not yet
    ///   requested, fire the LLM (cache-hit returns instantly).
    /// - **Expanded + currently Summary**: switch back to Body — no
    ///   LLM call, no state mutation beyond the view-pref flip.
    fn toggle_summary_view(&mut self) {
        if !self.summarize_with_llm || self.llm.is_none() {
            return;
        }
        let filtered = self.filtered_messages();
        let selected: Option<Arc<EmailMessage>> = {
            let st = self.state.lock().expect("email state poisoned");
            filtered.get(st.selected).cloned()
        };
        let Some(msg) = selected else {
            return;
        };

        let (was_collapsed, will_show_summary) = {
            let mut st = self.state.lock().expect("email state poisoned");
            let was_collapsed = !st.expanded;
            if was_collapsed {
                st.expanded = true;
                st.summary_view.insert(msg.id.clone(), true);
                (true, true)
            } else {
                let cur = *st.summary_view.get(&msg.id).unwrap_or(&false);
                let new = !cur;
                st.summary_view.insert(msg.id.clone(), new);
                (false, new)
            }
        };

        if was_collapsed {
            self.mark_seen_if_unseen(&msg.id);
        }
        if will_show_summary {
            // request_summary is idempotent — cache-hits jump straight
            // to Ready without an LLM call. Calling unconditionally
            // here is safe + cheap.
            self.request_summary();
        }
    }

    fn request_summary(&self) {
        if !self.summarize_with_llm || self.llm.is_none() {
            return;
        }
        let filtered = self.filtered_messages();
        let selected: Option<Arc<EmailMessage>> = {
            let st = self.state.lock().expect("email state poisoned");
            filtered.get(st.selected).cloned()
        };
        let Some(msg) = selected else {
            return;
        };
        {
            let st = self.state.lock().expect("email state poisoned");
            if st.summaries.contains_key(&msg.id) {
                return;
            }
        }
        let cache_key = summary_cache_key(&msg.id);
        if let Some(entry) = self.cache.load::<String>(&cache_key) {
            let mut st = self.state.lock().expect("email state poisoned");
            st.summaries
                .insert(msg.id.clone(), SummaryState::Ready(entry.value));
            st.dirty = true;
            return;
        }
        let Some(llm) = self.llm.clone() else {
            return;
        };
        let state = self.state.clone();
        let cache = self.cache.clone();
        {
            let mut st = self.state.lock().expect("email state poisoned");
            st.summaries.insert(msg.id.clone(), SummaryState::Requested);
            st.dirty = true;
        }
        let id = msg.id.clone();
        let body = msg.plain_body.clone();
        let subject = msg.subject.clone();
        let from = format_sender(&msg.from_name, &msg.from_address);
        tokio::spawn(async move {
            let request = LlmRequest {
                model: None,
                system: Some(SUMMARY_SYSTEM_PROMPT.into()),
                messages: vec![LlmMessage {
                    role: Role::User,
                    content: format!(
                        "From: {from}\nSubject: {subject}\n\n{}",
                        if body.is_empty() {
                            "(empty body)"
                        } else {
                            body.as_str()
                        }
                    ),
                }],
                max_tokens: 300,
                cache_system: true,
            };
            let outcome = match llm.complete(request).await {
                Ok(resp) => {
                    let text = resp.text.trim();
                    if text
                        .to_ascii_lowercase()
                        .starts_with("insufficient content to summarize")
                    {
                        SummaryState::Failed
                    } else {
                        SummaryState::Ready(text.to_string())
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, id = %id, "LLM email summary failed");
                    SummaryState::Failed
                }
            };
            if let SummaryState::Ready(text) = &outcome {
                if let Err(err) = cache.store(&cache_key, text) {
                    tracing::warn!(error = %err, id = %id, "email summary cache store failed");
                }
            }
            let mut st = state.lock().expect("email state poisoned");
            st.summaries.insert(id, outcome);
            st.dirty = true;
        });
    }

    /// True if the message should display the unread `●` indicator: the
    /// server still considers it unread AND the local seen-store has no
    /// record of the user having expanded it inside glint.
    fn is_unread(&self, msg: &EmailMessage) -> bool {
        if !msg.server_unread {
            return false;
        }
        let seen = self.seen.lock().expect("seen-store poisoned");
        !seen.contains(&msg.id)
    }

    /// Mirrors the inner-area split used by `render`.
    fn split_inner(&self, inner: Rect) -> (Rect, Rect, Rect) {
        let has_tabs = self.folders.len() > 1;
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

    fn tab_index_at(&self, click_col: u16, tab_area: Rect) -> Option<usize> {
        let mut x: u16 = tab_area.x + 1;
        for (i, label) in self.folders.iter().enumerate() {
            let w = label.chars().count() as u16 + 2;
            if click_col >= x && click_col < x + w {
                return Some(i);
            }
            x += w + 1;
            if x >= tab_area.x + tab_area.width {
                break;
            }
        }
        None
    }
}

/// Build an `EmailProviderHandle` from the configured provider name. Returns
/// `(handle, label, ready, hint)` where `ready=false` means the widget should
/// render the placeholder; `hint` is the actionable next step shown to the user.
fn build_provider(name: &str) -> (EmailProviderHandle, String, bool, Option<String>) {
    match name.to_ascii_lowercase().as_str() {
        "outlook" => match build_outlook() {
            Ok(p) => (
                EmailProviderHandle::Outlook(p),
                "outlook".into(),
                true,
                None,
            ),
            Err(hint) => (
                EmailProviderHandle::Empty,
                "outlook".into(),
                false,
                Some(hint),
            ),
        },
        "gmail" => match build_gmail() {
            Ok(p) => (EmailProviderHandle::Gmail(p), "gmail".into(), true, None),
            Err(hint) => (
                EmailProviderHandle::Empty,
                "gmail".into(),
                false,
                Some(hint),
            ),
        },
        "imap" => match build_imap() {
            Ok(p) => (EmailProviderHandle::Imap(p), "imap".into(), true, None),
            Err(hint) => (EmailProviderHandle::Empty, "imap".into(), false, Some(hint)),
        },
        other => (
            EmailProviderHandle::Empty,
            other.to_string(),
            false,
            Some(format!(
                "unknown provider {other:?} (expected outlook, gmail, or imap)"
            )),
        ),
    }
}

fn build_outlook() -> Result<outlook::OutlookEmailProvider, String> {
    use crate::auth::microsoft::{store::MicrosoftToken, OAuthClientConfig as MsClient};
    let client = MsClient::load().map_err(|err| {
        tracing::warn!(error = %err, "microsoft_oauth_client.toml missing or invalid");
        "Drop microsoft_oauth_client.toml in ~/.config/glint/credentials/".to_string()
    })?;
    let token = MicrosoftToken::load()
        .map_err(|err| format!("Outlook token unreadable: {err}"))?
        .ok_or_else(|| {
            "Run `glint --auth microsoft` to connect Microsoft Outlook (the Email widget needs Mail.Read — re-run after upgrading)".to_string()
        })?;
    outlook::OutlookEmailProvider::new(client, token)
        .map_err(|err| format!("Outlook email init failed: {err}"))
}

fn build_gmail() -> Result<gmail::GmailProvider, String> {
    use crate::auth::google::{store::GoogleToken, OAuthClientConfig as GClient};
    let client = GClient::load().map_err(|err| {
        tracing::warn!(error = %err, "google_oauth_client.toml missing or invalid");
        "Drop google_oauth_client.toml in ~/.config/glint/credentials/".to_string()
    })?;
    let token = match GoogleToken::load() {
        Ok(Some(t)) => t,
        Ok(None) => {
            return Err(
                "Run `glint --auth google` to connect Gmail (the Email widget needs gmail.readonly — re-run after upgrading)".into(),
            );
        }
        Err(err) => return Err(format!("Google token unreadable: {err}")),
    };
    gmail::GmailProvider::new(client, token).map_err(|err| format!("Gmail init failed: {err}"))
}

fn build_imap() -> Result<imap::ImapProvider, String> {
    let dir = crate::credentials::dir()
        .map_err(|err| format!("IMAP credentials dir unavailable: {err}"))?;
    let path = dir.join("imap.toml");
    if !path.exists() {
        return Err(format!(
            "IMAP credentials missing at {} — run --setup to capture them",
            path.display()
        ));
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("read {} failed: {err}", path.display()))?;
    let creds: imap::ImapCredentials =
        toml::from_str(&text).map_err(|err| format!("parse {} failed: {err}", path.display()))?;
    if creds.username.trim().is_empty() || creds.app_password.trim().is_empty() {
        return Err(format!(
            "{} has empty username or app_password — edit and retry",
            path.display()
        ));
    }
    Ok(imap::ImapProvider::new(creds))
}

// ── Widget trait impl ───────────────────────────────────────────────────────

#[async_trait]
impl Widget for EmailWidget {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> &str {
        "email"
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
        let mut st = self.state.lock().expect("email state poisoned");
        std::mem::replace(&mut st.dirty, false)
    }

    fn render(&self, frame: &mut Frame, area: Rect, focused: bool) {
        let (messages, selected, mut scroll, expanded, active_idx, inflight, last_error, account) = {
            let st = self.state.lock().expect("email state poisoned");
            (
                st.messages.clone(),
                st.selected,
                st.scroll,
                st.expanded,
                st.active_folder_idx,
                st.inflight,
                st.last_error.clone(),
                st.account.clone(),
            )
        };

        // Apply the active folder filter.
        let folder_name = self
            .folders
            .get(active_idx.min(self.folders.len().saturating_sub(1)))
            .cloned()
            .unwrap_or_else(|| "INBOX".into());
        let filtered: Vec<Arc<EmailMessage>> = messages
            .into_iter()
            .filter(|m| m.folder.eq_ignore_ascii_case(&folder_name))
            .collect();

        // Base title is just "Email" / "Email (instance)" — the
        // provider + account address are metadata, rendered via the
        // shared title-with-metadata helper for consistency with
        // other widgets.
        let base = if self.instance == "main" {
            "Email".to_string()
        } else {
            format!("Email ({})", self.instance)
        };
        let account_label = account
            .as_deref()
            .map(String::from)
            .unwrap_or_else(|| "(loading…)".into());
        let metadata = format!("[{}] {}", self.provider_label, account_label);

        let block = apply_title_row(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(self.theme.border_style(focused)),
            focused,
            &base,
            Some(metadata.as_str()),
            MetadataEmphasis::Default,
            self.shortcut,
            &self.theme,
            area.width,
        );
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let tier = ViewTier::from_rect(area);

        let (tab_area, list_area, footer_area) = self.split_inner(inner);

        // At Expanded/Full tiers with enough width, split the list area into:
        //   left (50%) | 3-col gutter | right (remaining)
        // The gutter renders a centered `│` with one blank column on each side.
        // Below READ_PANE_MIN_WIDTH the list column is too narrow, so
        // Compact/Standard (and cramped Expanded) leave the read pane off (None).
        let (list_area, gutter_area, read_area) = if tier >= ViewTier::Expanded
            && list_area.width >= READ_PANE_MIN_WIDTH
        {
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(50),
                    Constraint::Length(3),
                    Constraint::Min(0),
                ])
                .split(list_area);
            (chunks[0], Some(chunks[1]), Some(chunks[2]))
        } else {
            (list_area, None, None)
        };

        // Placeholder when no provider is configured (no token, etc.).
        if !self.provider_ready {
            let mut lines = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "Email provider not connected.",
                    self.theme.text_brilliant,
                )),
            ];
            if let Some(hint) = &self.auth_hint {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(hint.clone(), self.theme.text_dim)));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Run `glint --setup` to configure email.",
                self.theme.text_dim,
            )));
            let body = Paragraph::new(lines).alignment(Alignment::Center);
            frame.render_widget(body, inner);
            return;
        }

        // Folder tab bar.
        let has_tabs = self.folders.len() > 1;
        if has_tabs {
            let mut spans: Vec<Span<'_>> = Vec::with_capacity(self.folders.len() * 2);
            spans.push(Span::raw(" "));
            for (i, label) in self.folders.iter().enumerate() {
                let is_active = i == active_idx;
                let style = if is_active {
                    self.theme.text_selected
                } else {
                    self.theme.text_dim
                };
                spans.push(Span::styled(format!("[{label}]"), style));
                if i + 1 < self.folders.len() {
                    spans.push(Span::raw(" "));
                }
            }
            frame.render_widget(Paragraph::new(Line::from(spans)), tab_area);
        }

        if filtered.is_empty() {
            let msg = if inflight {
                "Loading messages…".to_string()
            } else if let Some(err) = last_error.as_ref() {
                format!("Last fetch failed: {err}")
            } else {
                "No recent messages.".to_string()
            };
            let body =
                Paragraph::new(vec![Line::from(""), Line::from(msg)]).alignment(Alignment::Center);
            frame.render_widget(body, inner);
            return;
        }

        // Layout each message row:
        //   ●  Alice Smith            Re: Project update                                12:43
        // When expanded: subject + body/summary lines underneath. The
        // expansion height is variable (depends on whether the user is
        // viewing the raw body or the LLM summary, and on the wrapped
        // line count), so we measure it explicitly below.
        const ROWS_PER_ITEM: usize = 1;
        let list_height = list_area.height;
        let items_visible = (list_height as usize / ROWS_PER_ITEM).max(1);
        // Baseline: keep the selected message in view.
        if selected < scroll {
            scroll = selected;
        }
        if selected >= scroll + items_visible {
            scroll = selected + 1 - items_visible;
        }
        // Extra: if expanded, scroll up far enough that the full
        // expansion (subject + body/summary) fits below the selected
        // row when possible. Clamps so the selected row never scrolls
        // off the top — for emails whose expansion exceeds the pane
        // height, the selected row pins to the top and the bottom of
        // the expansion clips (the standard "long content" failure
        // mode; the user can collapse or use the LLM summary to
        // shorten).
        if expanded && read_area.is_none() {
            if let Some(msg) = filtered.get(selected) {
                let body_max_width = (list_area.width as usize).saturating_sub(3);
                let subject_lines = wrap_text(&msg.subject, body_max_width, 2).len();
                let body_lines = expanded_body_lines(
                    msg,
                    &self.state,
                    body_max_width,
                    self.summarize_with_llm && self.llm.is_some(),
                    MAX_SUMMARY_LINES,
                )
                .len();
                let expansion_height = subject_lines + body_lines;
                let want = (selected + 1 + expansion_height).saturating_sub(items_visible);
                scroll = scroll.max(want).min(selected);
            }
        }

        let now_local = Local::now();
        // Reserve a 1-cell right buffer so the timestamp column
        // doesn't run flush against the widget's right border.
        // All column-width math below derives from `inner_width`,
        // so shrinking it here automatically gives the row its
        // trailing gutter without touching the per-row span list.
        let inner_width = (list_area.width as usize).saturating_sub(1);
        // Column-width policy:
        //   * Date is fixed at 8 chars (matches the formats produced by
        //     `format_received`: "Fri 14:25", "Yesterday", "Mar 03", …).
        //   * Sender label is 20 chars by default, growing up to 25 when
        //     a wide pane leaves surplus space — long names like
        //     "alex.thompson@example.com" become legible on a roomy
        //     display without crowding subjects on a narrow one.
        //   * Subject text is capped at 95 visible chars (anything past
        //     that scans worse than it reads). Surplus pane width past
        //     that cap first feeds sender, then becomes trailing padding
        //     between subject and date — which keeps the date right-
        //     aligned no matter how wide the pane gets.
        //   * Indicator (●/○) + space prefix = 2 chars, and there are
        //     two single-space inter-column gaps → 4 chars of fixed
        //     chrome on every row.
        const SENDER_LABEL_MIN: usize = 20;
        const SENDER_LABEL_MAX: usize = 25;
        const SUBJECT_TEXT_MAX: usize = 95;
        const DATE_COL_W: usize = 8;
        const INDICATOR_PREFIX_W: usize = 2;
        const COL_GAPS_W: usize = 2;

        let mut sender_label_w = SENDER_LABEL_MIN;
        let mut sender_col_w = sender_label_w + INDICATOR_PREFIX_W;
        let mut subject_col_w = inner_width.saturating_sub(sender_col_w + DATE_COL_W + COL_GAPS_W);
        // When subject would overflow the 95-char cap, donate the excess
        // to sender first (up to SENDER_LABEL_MAX). Any remaining surplus
        // stays in the subject column as trailing padding so the date
        // column hugs the right edge.
        if subject_col_w > SUBJECT_TEXT_MAX {
            let excess = subject_col_w - SUBJECT_TEXT_MAX;
            let donate = excess.min(SENDER_LABEL_MAX - SENDER_LABEL_MIN);
            sender_label_w += donate;
            sender_col_w = sender_label_w + INDICATOR_PREFIX_W;
            subject_col_w = inner_width.saturating_sub(sender_col_w + DATE_COL_W + COL_GAPS_W);
        }
        let date_col_w = DATE_COL_W;
        let subject_text_w = subject_col_w.min(SUBJECT_TEXT_MAX);

        let mut lines: Vec<Line<'_>> = Vec::with_capacity(items_visible);
        let mut rows_emitted: u16 = 0;
        let mut row_layout: Vec<(usize, u16, u16)> = Vec::new();
        for (i, msg) in filtered.iter().enumerate().skip(scroll) {
            let row_start = rows_emitted;
            let is_selected = i == selected;
            let expand_this = is_selected && expanded && read_area.is_none();

            let row_style = if is_selected {
                self.theme.text_selected
            } else if focused {
                self.theme.text_focused
            } else {
                self.theme.text_brilliant
            };

            let unread = self.is_unread(msg);
            let indicator = if unread { "●" } else { "○" };
            let sender = normalize_sender(&msg.from_name, &msg.from_address, sender_label_w);
            let date = format_received(now_local, msg.received);
            let subject = if msg.subject.is_empty() {
                "(no subject)".to_string()
            } else {
                msg.subject.clone()
            };
            // Truncate the subject text at the cap, then pad the column
            // out to its full width so the date column stays pinned to
            // the right edge regardless of how much surplus space the
            // pane has past 95 chars of subject.
            let subject_truncated = truncate(&subject, subject_text_w);

            let sender_padded = pad_or_truncate(&sender, sender_label_w);
            let subject_padded = pad_or_truncate(&subject_truncated, subject_col_w);
            let date_padded = format!("{date:>w$}", w = date_col_w);

            let row = Line::from(vec![
                Span::styled(
                    format!("{indicator} "),
                    if unread {
                        self.theme.text_focused
                    } else {
                        self.theme.text_dim
                    },
                ),
                Span::styled(sender_padded, row_style),
                Span::raw(" "),
                Span::styled(subject_padded, row_style),
                Span::raw(" "),
                Span::styled(date_padded, self.theme.text_dim),
            ]);
            lines.push(row);
            rows_emitted += 1;

            if expand_this {
                let body_lines = expanded_body_lines(
                    msg,
                    &self.state,
                    inner_width.saturating_sub(3),
                    self.summarize_with_llm && self.llm.is_some(),
                    MAX_SUMMARY_LINES,
                );
                // First the full subject on its own row(s) (up to 2).
                for sline in wrap_text(&msg.subject, inner_width.saturating_sub(3), 2) {
                    if rows_emitted >= list_height {
                        break;
                    }
                    lines.push(Line::from(Span::styled(
                        format!("   {sline}"),
                        self.theme.text_brilliant,
                    )));
                    rows_emitted += 1;
                }
                for bline in &body_lines {
                    if rows_emitted >= list_height {
                        break;
                    }
                    lines.push(Line::from(Span::styled(
                        format!("   {bline}"),
                        Style::default(),
                    )));
                    rows_emitted += 1;
                }
            }

            row_layout.push((i, row_start, rows_emitted));
            if rows_emitted >= list_height {
                break;
            }
        }
        frame.render_widget(Paragraph::new(lines), list_area);

        // Expanded/Full tier: render the selected message's full body in the
        // right-hand read pane. `read_area` is None at Compact/Standard tier
        // so this block is a no-op there.
        if let Some(rp) = read_area {
            let mut rp_lines: Vec<Line<'_>> = Vec::new();
            match filtered.get(selected) {
                None => {
                    // Empty selection (e.g. folder just switched, selected
                    // index not yet clamped). Show a dim placeholder.
                    rp_lines.push(Line::from(""));
                    rp_lines.push(Line::from(Span::styled(
                        "Select a message",
                        self.theme.text_dim,
                    )));
                }
                Some(msg) => {
                    // Header row 1: sender (left-aligned) + date (right-aligned).
                    let date_str = format_received(now_local, msg.received);
                    let sender_budget =
                        (rp.width as usize).saturating_sub(date_str.len() + 2).max(1);
                    let sender_display =
                        normalize_sender(&msg.from_name, &msg.from_address, sender_budget);
                    let sender_padded = pad_or_truncate(&sender_display, sender_budget);
                    rp_lines.push(Line::from(vec![
                        Span::styled(sender_padded, self.theme.text_focused),
                        Span::raw("  "),
                        Span::styled(date_str, self.theme.text_dim),
                    ]));
                    // Header row 2: full subject, truncated to pane width.
                    let subject_display = if msg.subject.is_empty() {
                        "(no subject)".to_string()
                    } else {
                        msg.subject.clone()
                    };
                    rp_lines.push(Line::from(Span::styled(
                        truncate(&subject_display, rp.width as usize),
                        self.theme.text_brilliant,
                    )));
                    // Blank separator between header and body.
                    rp_lines.push(Line::from(""));
                    // Body: full message with no MAX_SUMMARY_LINES cap,
                    // wrapped to pane width. Honors the per-message summary
                    // toggle (`s`) — when the user has switched into summary
                    // view this renders the already-generated summary (reused
                    // from the in-memory/disk cache, no fresh LLM call)
                    // instead of the raw body. Clip to available rows with a
                    // trailing dim "…" when the content overflows the pane.
                    const HEADER_ROWS: usize = 3; // sender, subject, blank
                    let body_rows_avail = (rp.height as usize).saturating_sub(HEADER_ROWS);
                    if body_rows_avail > 0 {
                        let body_w = (rp.width as usize).saturating_sub(1).max(1);
                        let all_body = expanded_body_lines(
                            msg,
                            &self.state,
                            body_w,
                            self.summarize_with_llm && self.llm.is_some(),
                            usize::MAX,
                        );
                        let clipped = all_body.len() > body_rows_avail;
                        let take = if clipped {
                            body_rows_avail.saturating_sub(1)
                        } else {
                            body_rows_avail
                        };
                        for bline in all_body.iter().take(take) {
                            rp_lines.push(Line::from(Span::raw(bline.clone())));
                        }
                        if clipped {
                            rp_lines.push(Line::from(Span::styled("…", self.theme.text_dim)));
                        }
                    }
                }
            }
            frame.render_widget(Paragraph::new(rp_lines), rp);
        }

        // Vertical divider between the list pane and the read pane. The gutter
        // is 3 cols wide (from the Layout above); `│` sits in the center column
        // with one blank on each side. Styled dim to match other widget borders.
        if let Some(gutter) = gutter_area {
            let divider_lines: Vec<Line<'_>> = (0..gutter.height)
                .map(|_| {
                    Line::from(vec![
                        Span::raw(" "),
                        Span::styled("│", self.theme.text_dim),
                        Span::raw(" "),
                    ])
                })
                .collect();
            frame.render_widget(Paragraph::new(divider_lines), gutter);
        }

        // Hide the `s summarize` hint when summarisation isn't usable —
        // either the user disabled it in email.toml or there's no LLM
        // key configured. Surfacing an unbindable key in the footer is
        // confusing ("I pressed s and nothing happened…").
        let summarize_usable = self.summarize_with_llm && self.llm.is_some();
        // When the read pane is active the `e`/Enter key is a no-op, so drop
        // that hint to avoid implying a binding that does nothing in this mode.
        let footer_text = if read_area.is_some() {
            if summarize_usable {
                "↑/↓ select · ←/→ folder · o open · s summarize · r refresh"
            } else {
                "↑/↓ select · ←/→ folder · o open · r refresh"
            }
        } else if summarize_usable {
            "↑/↓ select · ←/→ folder · e/⏎/click expand · o open · s summarize · r refresh"
        } else {
            "↑/↓ select · ←/→ folder · e/⏎/click expand · o open · r refresh"
        };
        let footer = Paragraph::new(Line::from(Span::styled(footer_text, self.theme.text_dim)))
            .alignment(Alignment::Right);
        frame.render_widget(footer, footer_area);

        // Persist scroll + the row layout so click handling can map
        // mouse coordinates back to a message index.
        let mut st = self.state.lock().expect("email state poisoned");
        st.scroll = scroll;
        st.row_layout = row_layout;
        st.last_list_area = Some(list_area);
        st.read_pane_active = read_area.is_some();
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
            KeyCode::Char('e') | KeyCode::Enter => {
                let read_pane_active = {
                    let st = self.state.lock().expect("email state poisoned");
                    st.read_pane_active
                };
                if !read_pane_active {
                    self.toggle_expand();
                }
                EventResult::Handled
            }
            KeyCode::Char('o') => {
                self.open_selected();
                EventResult::Handled
            }
            KeyCode::Char('s') => {
                self.toggle_summary_view();
                EventResult::Handled
            }
            KeyCode::Char('r') => {
                self.mark_dirty();
                EventResult::Handled
            }
            KeyCode::Char('[') | KeyCode::Left | KeyCode::Char('h') => {
                self.cycle_folder(false);
                EventResult::Handled
            }
            KeyCode::Char(']') | KeyCode::Right | KeyCode::Char('l') => {
                self.cycle_folder(true);
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
        let inner = Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2);
        let (tab_area, _list_area, _footer_area) = self.split_inner(inner);
        if tab_area.height > 0
            && mouse.row == tab_area.y
            && mouse.column >= tab_area.x
            && mouse.column < tab_area.x + tab_area.width
        {
            if let Some(idx) = self.tab_index_at(mouse.column, tab_area) {
                let mut st = self.state.lock().expect("email state poisoned");
                if st.active_folder_idx != idx {
                    st.active_folder_idx = idx;
                    st.selected = 0;
                    st.scroll = 0;
                    st.expanded = false;
                }
                return EventResult::Handled;
            }
        }
        // Click inside the message list — find the row that owns this
        // mouse position and toggle expand on that message (selecting it
        // first if it wasn't already the active row). Hit-test against the
        // last-rendered list area, which is the narrowed left column when a
        // read pane is present, so a click landing in the read pane is a
        // no-op rather than jumping to whatever row sits at that vertical
        // offset.
        let list_area = {
            let st = self.state.lock().expect("email state poisoned");
            st.last_list_area
        };
        let in_list = list_area.is_some_and(|la| {
            la.height > 0
                && mouse.column >= la.x
                && mouse.column < la.x + la.width
                && mouse.row >= la.y
                && mouse.row < la.y + la.height
        });
        if let (true, Some(list_area)) = (in_list, list_area) {
            let click_offset = mouse.row - list_area.y;
            let hit_and_state = {
                let st = self.state.lock().expect("email state poisoned");
                st.row_layout
                    .iter()
                    .find(|(_, start, end)| click_offset >= *start && click_offset < *end)
                    .map(|(idx, _, _)| (*idx, st.selected))
            };
            if let Some((idx, selected_before)) = hit_and_state {
                if idx != selected_before {
                    // Switch selection first, then force-expand via toggle.
                    // Setting expanded=false beforehand makes toggle_expand
                    // flip to true and run the mark-as-seen side effect.
                    let mut st = self.state.lock().expect("email state poisoned");
                    st.selected = idx;
                    st.expanded = false;
                    drop(st);
                    self.toggle_expand();
                } else {
                    self.toggle_expand();
                }
                return EventResult::Handled;
            }
        }
        EventResult::Ignored
    }

    fn handle_command(&mut self, cmd: &str, _args: &[&str]) -> Result<bool> {
        match cmd {
            "email" => Ok(true),
            "refresh" => {
                self.mark_dirty();
                Ok(false) // let the global :refresh dispatch continue
            }
            _ => Ok(false),
        }
    }

    fn keybindings(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("↑ / ↓ / j / k", "select message"),
            ("← / → / [ / ] / h / l", "cycle folder"),
            ("PgUp / PgDn", "±10 messages"),
            ("g / Home", "jump to top"),
            ("End", "jump to bottom"),
            ("e / Enter / click", "expand selected (marks seen locally)"),
            ("o", "open message in browser"),
            ("s", "request LLM summary (when enabled)"),
            ("r", "force refresh"),
        ]
    }

    fn config(&self) -> serde_json::Value {
        let mail_secs = self
            .state
            .lock()
            .expect("email state poisoned")
            .mail_poll
            .interval()
            .as_secs();
        serde_json::json!({
            "provider": self.provider_label,
            "latest_days": self.latest_days,
            "refresh_minutes": mail_secs / 60,
            "folders": self.folders,
            "summarize_with_llm": self.summarize_with_llm,
        })
    }

    fn apply_config(&mut self, config: serde_json::Value) -> Result<()> {
        let new_config: EmailConfig =
            serde_json::from_value(config).context("invalid email config payload")?;
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

    /// Return whichever tracker is currently in effect — account
    /// resolution while we're still waiting for the address,
    /// otherwise the configured mail-refresh cadence — so the
    /// platform sees the cadence actually driving us right now.
    fn poll_snapshot(&self) -> Option<crate::polling::PollSnapshot> {
        let st = self.state.lock().expect("email state poisoned");
        let snap = if st.account.is_none() {
            st.account_poll.snapshot()
        } else {
            st.mail_poll.snapshot()
        };
        Some(snap)
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
        // Match the standalone email title's suffix: `[gmail]
        // alice@example.com` when the account has resolved; just
        // `[gmail]` until then.
        let label = self.provider_label.as_str();
        if label.is_empty() {
            return None;
        }
        let account = self.state.lock().ok().and_then(|st| st.account.clone());
        match account {
            Some(addr) => Some(format!("[{label}] {addr}")),
            None => Some(format!("[{label}]")),
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Normalize a "Name <addr>" pair into a clean display name capped at
/// `max_len` chars. Falls back to the username portion of the address when
/// no display name is present.
pub(crate) fn normalize_sender(name: &Option<String>, address: &str, max_len: usize) -> String {
    let display = match name {
        Some(n) if !n.trim().is_empty() => n.trim().trim_matches('"').to_string(),
        _ => address.split('@').next().unwrap_or(address).to_string(),
    };
    truncate(&display, max_len)
}

fn format_sender(name: &Option<String>, address: &str) -> String {
    match name {
        Some(n) if !n.trim().is_empty() => format!("{n} <{address}>"),
        _ => address.to_string(),
    }
}

fn format_received(now: DateTime<Local>, received: DateTime<Local>) -> String {
    if now.date_naive() == received.date_naive() {
        received.format("%H:%M").to_string()
    } else {
        received.format("%m/%d").to_string()
    }
}

/// Truncate `s` so it occupies at most `max` *terminal cells* (not code
/// points). Wide glyphs (CJK, most emoji) report a width of 2 via
/// Cap `body`'s in-memory length at `max_chars`, appending a brief "…"
/// marker so a future reader notices the truncation. Operates in place
/// to avoid a clone on the common no-op path. Char-boundary safe.
fn truncate_body_in_place(body: &mut String, max_chars: usize) {
    if body.chars().count() <= max_chars {
        return;
    }
    let cutoff = body
        .char_indices()
        .nth(max_chars.saturating_sub(2))
        .map(|(i, _)| i)
        .unwrap_or(body.len());
    body.truncate(cutoff);
    body.push_str("…");
}

/// Body/summary lines for a message, honoring the per-message summary
/// preference (`summary_view`) and any already-generated summary
/// (`summaries`, populated from the in-memory map or the disk cache —
/// never a fresh LLM call). `body_max_lines` caps the *raw body* view:
/// the compact list/expanded panes pass `MAX_SUMMARY_LINES` to keep
/// long emails from crowding the list, while the wide read pane passes
/// `usize::MAX` and does its own row-clipping. LLM summaries are always
/// uncapped (already bounded by the system prompt).
fn expanded_body_lines(
    msg: &EmailMessage,
    state: &Arc<Mutex<EmailState>>,
    max_width: usize,
    llm_enabled: bool,
    body_max_lines: usize,
) -> Vec<String> {
    let (summary_state, prefer_summary) = {
        let st = state.lock().expect("email state poisoned");
        (
            st.summaries.get(&msg.id).cloned(),
            *st.summary_view.get(&msg.id).unwrap_or(&false),
        )
    };
    // Show the summary only when the user has explicitly toggled into
    // summary view for this message (via `s`). The historical default
    // — "always prefer summary if cached" — caused a `s` press to
    // appear to do nothing because the view was already on the
    // cached summary. With the per-message preference, the user gets
    // a predictable Body ⇄ Summary toggle and never loses the
    // original body view to a stale summary.
    if llm_enabled && prefer_summary {
        if let Some(s) = summary_state {
            match s {
                // Ready summaries render in full — the system prompt
                // caps the LLM output at ~4 sentences, so the line
                // count is naturally bounded.
                SummaryState::Ready(text) => {
                    return wrap_text(&text, max_width, usize::MAX);
                }
                SummaryState::Requested => {
                    let mut out = vec!["Summarizing…".to_string()];
                    if !msg.plain_body.is_empty() {
                        out.extend(wrap_text(
                            &msg.plain_body,
                            max_width,
                            body_max_lines.saturating_sub(1),
                        ));
                    }
                    return out;
                }
                // Failed → fall through to body so the user always
                // sees something readable even when the LLM bailed.
                SummaryState::Failed => {}
            }
        }
    }
    // Body view: cap at `body_max_lines`. In the list/expanded panes
    // this is MAX_SUMMARY_LINES (5) — long raw emails would otherwise
    // push every other message off-screen and require multi-pane
    // scrolling. The read pane passes usize::MAX and clips to its own
    // available rows. The LLM summary above stays uncapped (already
    // bounded by the system prompt's ~4 sentences); users who want the
    // full body open the message in their mail client via `o` instead.
    wrap_text(&msg.plain_body, max_width, body_max_lines)
}

/// Thin wrapper preserving the call sites' `wrap_text` name. The
/// canonical implementation lives in [`crate::text::wrap`]; email
/// always wants paragraph-preservation since `\n` in `msg.plain_body`
/// separates real paragraphs.
fn wrap_text(text: &str, max_width: usize, max_lines: usize) -> Vec<String> {
    wrap(text, max_width, max_lines, true)
}

pub const KIND: &str = "email";

/// Wizard descriptor. Covers provider choice, refresh cadence, the
/// common scalars, plus OAuth triggers for Gmail (Google) and Outlook
/// (Microsoft). Folders, account address, and color overrides stay in
/// email.toml; the wizard's renderer merges in only the keys it manages
/// so hand-edits survive `--setup` re-runs.
///
/// Note on IMAP: glint's email widget currently speaks only the Gmail
/// and Outlook REST APIs. IMAP support is on the roadmap; once it lands
/// this descriptor will gain an `imap_*` field group + a credentials
/// path. For now, IMAP shows up as a disabled choice with explanatory
/// help text.
pub fn wizard_descriptor() -> crate::wizard::descriptor::WizardDescriptor {
    use crate::wizard::descriptor::{ChoiceOption, WizardDescriptor, WizardField, WizardFieldKind};
    WizardDescriptor {
        display_name: "Email",
        blurb: "Lightweight message list backed by Gmail or Outlook. Select \
                a provider, authorize once, and glint surfaces unread + \
                recent messages. IMAP support is planned but not yet \
                wired up.",
        load_from_toml: Some(load_email_from_toml),
        render_toml: Some(render_email_toml),
        fields: vec![
            WizardField {
                key: "provider",
                label: "Email provider",
                help: "Which mailbox to surface. Gmail uses Google OAuth; \
                       Outlook uses Microsoft OAuth. IMAP is reserved — \
                       choosing it will skip the auth step and you'll \
                       need to wait for IMAP support to land before this \
                       widget can fetch.",
                required: true,
                kind: WizardFieldKind::Choice {
                    options: vec![
                        ChoiceOption {
                            value: "gmail",
                            label: "Gmail (Google OAuth)",
                            help: None,
                        },
                        ChoiceOption {
                            value: "outlook",
                            label: "Outlook (Microsoft OAuth)",
                            help: None,
                        },
                        ChoiceOption {
                            value: "imap",
                            label: "IMAP (Gmail+app-password, iCloud, Fastmail, self-hosted)",
                            help: None,
                        },
                    ],
                    default: Some("gmail"),
                },
                validate: None,
            },
            WizardField {
                key: "authorize_google",
                label: "Authorize Google (for Gmail)",
                help: "Only needed if `provider = \"gmail\"`. Opens a \
                       browser tab to console.cloud.google.com for the \
                       OAuth consent.",
                required: false,
                kind: WizardFieldKind::OAuth { provider: "google" },
                validate: None,
            },
            WizardField {
                key: "authorize_microsoft",
                label: "Authorize Microsoft (for Outlook)",
                help: "Only needed if `provider = \"outlook\"`. Opens a \
                       browser tab to login.microsoftonline.com; relies on \
                       credentials/microsoft_oauth_client.toml.",
                required: false,
                kind: WizardFieldKind::OAuth {
                    provider: "microsoft",
                },
                validate: None,
            },
            WizardField {
                key: "authorize_imap",
                label: "Set up IMAP credentials",
                help: "Only needed if `provider = \"imap\"`. Captures \
                       host / port / username / app-password inline and \
                       writes credentials/imap.toml. For Gmail, generate \
                       an app-specific password at \
                       myaccount.google.com → Security → 2-Step → App \
                       passwords. INSTRUCTIONS.md has the full recipe.",
                required: false,
                kind: WizardFieldKind::OAuth { provider: "imap" },
                validate: None,
            },
            WizardField {
                key: "latest_days",
                label: "How many days of mail to show",
                help: "Messages received in the last N days are listed. \
                       7 days is the sensible default.",
                required: true,
                kind: WizardFieldKind::Number {
                    default: Some(7.0),
                    range: Some((1.0, 90.0)),
                    integer: true,
                },
                validate: None,
            },
            WizardField {
                key: "refresh_minutes",
                label: "Refresh interval (minutes)",
                help: "How often to re-query the provider. 5 minutes is \
                       polite for both Gmail and Outlook free APIs.",
                required: true,
                kind: WizardFieldKind::Number {
                    default: Some(5.0),
                    range: Some((1.0, 1440.0)),
                    integer: true,
                },
                validate: None,
            },
            WizardField {
                key: "folders",
                label: "Folders / labels to surface",
                help: "Loaded from your mailbox after you authorize. \
                       Space toggles a folder. Until you authorize, the \
                       list falls back to the default INBOX entry — \
                       additional folders appear automatically once the \
                       OAuth flow completes (Google → Gmail labels;
                       Microsoft → Outlook mailbox folders).",
                required: false,
                kind: WizardFieldKind::RemoteMultiChoice {
                    // Source key is provider-agnostic — the wizard's
                    // post-auth hook populates this same slot with
                    // whichever provider's folder list applies (Gmail
                    // labels or Outlook folders).
                    source: "email_folders",
                    defaults: vec!["INBOX"],
                },
                validate: None,
            },
            WizardField {
                key: "summarize_with_llm",
                label: "Summarise expanded messages with LLM",
                help: "On-demand summarisation via the Anthropic key set \
                       in the Global step. Press `s` on an expanded \
                       message at runtime to invoke.",
                required: false,
                kind: WizardFieldKind::Bool { default: false },
                validate: None,
            },
        ],
    }
}

fn load_email_from_toml(
    doc: &toml::Value,
) -> std::collections::HashMap<String, crate::wizard::descriptor::WizardValue> {
    use crate::wizard::descriptor::WizardValue;
    let mut out = std::collections::HashMap::new();
    if let Some(s) = doc.get("provider").and_then(|v| v.as_str()) {
        out.insert("provider".into(), WizardValue::Choice(s.into()));
    }
    if let Some(n) = doc.get("latest_days").and_then(|v| v.as_integer()) {
        out.insert("latest_days".into(), WizardValue::Number(n as f64));
    }
    if let Some(n) = doc.get("refresh_minutes").and_then(|v| v.as_integer()) {
        out.insert("refresh_minutes".into(), WizardValue::Number(n as f64));
    }
    if let Some(b) = doc.get("summarize_with_llm").and_then(|v| v.as_bool()) {
        out.insert("summarize_with_llm".into(), WizardValue::Bool(b));
    }
    if let Some(arr) = doc.get("folders").and_then(|v| v.as_array()) {
        let folders: Vec<String> = arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        if !folders.is_empty() {
            out.insert("folders".into(), WizardValue::MultiChoice(folders));
        }
    }
    out
}

fn render_email_toml(
    values: &std::collections::HashMap<String, crate::wizard::descriptor::WizardValue>,
    existing: Option<&str>,
) -> String {
    use crate::wizard::descriptor::WizardValue;
    let provider = match values.get("provider") {
        Some(WizardValue::Choice(s)) => s.clone(),
        _ => "gmail".into(),
    };
    let folders: Vec<String> = match values.get("folders") {
        Some(WizardValue::MultiChoice(items)) if !items.is_empty() => items.clone(),
        _ => vec!["INBOX".into()],
    };
    let folders_array = folders
        .iter()
        .map(|s| format!("\"{}\"", s.replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(", ");
    let scalars: Vec<(&str, String)> = vec![
        ("provider", format!("\"{}\"", provider.replace('"', "\\\""))),
        (
            "latest_days",
            match values.get("latest_days") {
                Some(WizardValue::Number(n)) => format!("{}", *n as i64),
                _ => "7".into(),
            },
        ),
        (
            "refresh_minutes",
            match values.get("refresh_minutes") {
                Some(WizardValue::Number(n)) => format!("{}", *n as i64),
                _ => "5".into(),
            },
        ),
        (
            "summarize_with_llm",
            match values.get("summarize_with_llm") {
                Some(WizardValue::Bool(b)) => b.to_string(),
                _ => "false".into(),
            },
        ),
        ("folders", format!("[{folders_array}]")),
    ];
    // No DEFAULT_EMAIL_TOML on first install — emit a minimal scaffold
    // then merge the wizard scalars on top so the file is immediately
    // usable. Pre-existing files preserve their other keys
    // (account_address, [colors], shortcuts, …) via the merge path.
    let base: std::borrow::Cow<str> = match existing {
        Some(text) => std::borrow::Cow::Borrowed(text),
        None => std::borrow::Cow::Owned(
            "# Generated by `glint --setup`. Hand-edit freely; the wizard\n\
             # preserves additional keys (account_address, colors, shortcuts)\n\
             # the next time you run --setup.\n"
                .to_string(),
        ),
    };
    crate::wizard::toml_merge::merge_top_level_scalars(&base, &scalars)
}

pub fn build(ctx: &super::WidgetCtx) -> Box<dyn super::Widget> {
    let cfg: EmailConfig =
        crate::config::load_widget_toml_for_instance(KIND, &ctx.instance).unwrap_or_default();
    Box::new(EmailWidget::with_config_and_llm(
        ctx.instance.clone(),
        cfg,
        ctx.llm.clone(),
        ctx.theme.clone(),
        ctx.cache.clone(),
    ))
}

#[cfg(test)]
mod tests;
