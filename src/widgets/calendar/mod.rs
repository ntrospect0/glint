// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

pub mod caldav;
pub mod google;
pub mod local;
pub mod outlook;
pub mod provider;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{
    DateTime, Datelike, Duration as ChronoDuration, Local, NaiveDate, TimeZone, Timelike, Weekday,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};
use serde::{Deserialize, Serialize};

use super::{AppContext, EventResult, Widget};

use caldav::{CalDavCredentials, CalDavProvider};
use google::GoogleCalendarProvider;
use local::{LocalCalendarFile, LocalCalendarProvider};
use outlook::OutlookCalendarProvider;
use provider::{CalendarProvider, Event};

use crate::auth::google::{store::GoogleToken, OAuthClientConfig as GoogleClientConfig};
use crate::auth::microsoft::{store::MicrosoftToken, OAuthClientConfig as MicrosoftClientConfig};
use crate::cache::ScopedCache;
use crate::theme::{parse_color, ColorScheme, Theme};
use crate::ui::{apply_title_row, big_digits, MetadataEmphasis};

const VIEW_TABS: &[(CalendarView, &str)] = &[
    (CalendarView::Day, "Day"),
    (CalendarView::Week, "Week"),
    (CalendarView::Month, "Month"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CalendarView {
    #[default]
    Day,
    Week,
    Month,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    #[default]
    Local,
    Google,
    #[serde(alias = "apple", alias = "icloud")]
    Caldav,
    #[serde(alias = "microsoft", alias = "ms365")]
    Outlook,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CalendarConfig {
    #[serde(default)]
    pub default_view: CalendarView,

    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    /// Calendar sources. Empty = local-only (use `[[events]]` below).
    #[serde(default)]
    pub providers: Vec<ProviderEntry>,

    /// Fallback URLs for any `caldav` entry without explicit `calendar_ids`.
    #[serde(default)]
    pub caldav: CalDavConfig,

    /// Events for the built-in local provider.
    #[serde(default)]
    pub events: Vec<local::RawEvent>,

    /// ANSI palette cycled across calendars in `[[providers]]` order. Names
    /// like `red`, `light_blue`. Wraps when more calendars than colors.
    #[serde(default)]
    pub color_palette: Vec<String>,

    /// Per-calendar overrides keyed by `"<source>:<calendar_id>"`
    /// (e.g. `"google:primary"`). Wins over the palette sequence.
    #[serde(default)]
    pub calendar_colors: HashMap<String, String>,

    /// Big-digit gradient for the day-of-month numeral in Day view.
    /// `g` cycles. Only applies to today — anchor/preview days stay solid.
    #[serde(default)]
    pub gradient: big_digits::Gradient,

    /// Per-widget overrides layered on the app theme. Distinct from
    /// `calendar_colors`, which colors per-provider event blocks.
    #[serde(default)]
    pub colors: ColorScheme,

    /// `Shift+<letter>` focus shortcuts; falls back to `['c', 'd', 'a', 'l', 'e', 'n', 'r']`.
    #[serde(default)]
    pub shortcuts: Vec<char>,

    /// Which weekday starts the week in Week + Month views. Defaults to
    /// Sunday (US convention); ISO/Europe users typically set
    /// `first_day_of_week = "monday"`. Any chrono-recognized lowercase
    /// weekday name works (sunday/monday/tuesday/...). Invalid values
    /// fall back to Sunday with a `serde` parse error logged.
    #[serde(default)]
    pub first_day_of_week: FirstDayOfWeek,
}

/// Configurable first-day-of-week. Defaults to Sunday. Serialized as
/// a lowercase weekday name (`"sunday"`, `"monday"`, …) so the TOML
/// reads naturally.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FirstDayOfWeek {
    #[default]
    Sunday,
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
}

impl FirstDayOfWeek {
    pub fn as_weekday(self) -> Weekday {
        match self {
            FirstDayOfWeek::Sunday => Weekday::Sun,
            FirstDayOfWeek::Monday => Weekday::Mon,
            FirstDayOfWeek::Tuesday => Weekday::Tue,
            FirstDayOfWeek::Wednesday => Weekday::Wed,
            FirstDayOfWeek::Thursday => Weekday::Thu,
            FirstDayOfWeek::Friday => Weekday::Fri,
            FirstDayOfWeek::Saturday => Weekday::Sat,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderEntry {
    pub kind: ProviderKind,
    /// Google IDs, Outlook IDs, or CalDAV URLs. Empty = the provider's default
    /// (Google `"primary"`, Outlook default, every CalDAV calendar).
    #[serde(default)]
    pub calendar_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CalDavConfig {
    /// Explicit calendar URLs. Empty = walk the CalDAV principal chain
    /// (current-user-principal → calendar-home-set → calendars) to discover.
    #[serde(default)]
    pub calendars: Vec<String>,
}

fn default_poll_interval() -> u64 {
    60
}

impl Default for CalendarConfig {
    fn default() -> Self {
        Self {
            default_view: CalendarView::default(),
            poll_interval_secs: default_poll_interval(),
            providers: Vec::new(),
            caldav: CalDavConfig::default(),
            events: Vec::new(),
            color_palette: Vec::new(),
            calendar_colors: HashMap::new(),
            gradient: big_digits::Gradient::default(),
            colors: ColorScheme::default(),
            shortcuts: Vec::new(),
            first_day_of_week: FirstDayOfWeek::default(),
        }
    }
}

#[derive(Default)]
struct CalendarState {
    events: Vec<Event>,
    last_error: Option<String>,
    poll: crate::polling::PollTracker,
    inflight: bool,
    /// `(start, end)` of the data span the most recent successful
    /// fetch covered — the *fetch* range, which is wider than the
    /// displayed range thanks to the buffer added in `fetch_range`.
    /// Navigation that lands inside this span doesn't trigger a
    /// refetch (`mark_dirty_if_uncovered`), so scrolling through
    /// recently-visited days is instant.
    last_fetched_range: Option<(DateTime<Local>, DateTime<Local>)>,
    /// Active big-digit gradient. Seeded from config; user cycles with `g`.
    gradient: big_digits::Gradient,
    /// Row offset for the day's agenda list (Day view body + Month
    /// view's selected-day footer). ↑/↓ keys and mouse wheel drive this;
    /// reset to 0 whenever the anchor day or view changes so the user
    /// doesn't get stranded mid-list after navigating.
    agenda_scroll: u16,
    /// Last-known maximum scroll offset for the agenda — written by
    /// render after measuring lines vs viewport, read by the scroll
    /// handler so it can clamp without re-running the layout.
    agenda_scroll_max: u16,
    /// False until the agenda has been auto-scrolled once for the
    /// current (anchor, view) pair. Render flips this to true after
    /// it places the viewport on "now or later" events; manual
    /// scrolling (keys / wheel) also sets it so render stops fighting
    /// the user. Inverted (done-flag vs pending-flag) so the
    /// `#[derive(Default)]` default of `false` means "needs an
    /// autoscroll pass on the next render", which matches the right
    /// behavior on first construction without a manual override.
    agenda_autoscroll_done: bool,
    /// Per-column scroll offset for Week view — index 0 is the leftmost
    /// day (Sunday), index 6 the rightmost (Saturday). Wheel scrolling
    /// over a specific day in Week view drives one entry; columns are
    /// independent so scrolling Monday doesn't shift Friday's events.
    week_col_scroll: [u16; 7],
    /// Last-known maximum scroll for each Week-view day column,
    /// written by render after laying out each column's events vs the
    /// available height. The wheel handler clamps against these so
    /// scrolling past the end of one column doesn't accumulate state
    /// that survives a re-render.
    week_col_scroll_max: [u16; 7],
    /// Display-state dirty bit drained by `take_dirty`. Set true by
    /// every async-task / tick-time mutation site so the main loop's
    /// dirty-flag gate triggers a redraw.
    dirty: bool,
}

const CACHE_KEY_EVENTS: &str = "events";

pub struct CalendarWidget {
    id: String,
    instance: String,
    /// Cached `Calendar` / `Calendar (instance)` label so `display_name()`
    /// can hand out a `&str` without per-call allocation.
    display_name_cache: String,
    view: CalendarView,
    /// Anchor date used by all three views. For Day, it's the day shown.
    /// For Week, the week containing it. For Month, the month containing it.
    anchor: NaiveDate,
    provider: Arc<dyn CalendarProvider>,
    /// Source label surfaced in the cell title, e.g. `google`, `local`,
    /// `google+outlook`. Generated when the provider stack is built.
    source_label: String,
    /// When Google was requested but failed to initialize (no client config or
    /// no token), we keep the user-visible explanation so the widget can show
    /// "Run `glint --auth google`" instead of silently using the local seed.
    auth_hint: Option<String>,
    colors: CalendarColors,
    state: Arc<Mutex<CalendarState>>,
    /// App-level theme; kept so live config reloads can rebuild `theme`
    /// from updated `colors` overrides.
    app_theme: Arc<Theme>,
    /// Cached widget-level `[colors]` overrides. Stored so `:scheme` can
    /// rebuild the merged theme without re-reading `calendar.toml`.
    colors_override: ColorScheme,
    /// Merged theme (app + widget overrides). Rebuilt on `apply_config`.
    theme: Theme,
    /// Letter assigned by the app for `Shift+<letter>` focus, painted in
    /// the title via `text.shortcut`. `None` = no shortcut claimed.
    shortcut: Option<char>,
    /// Effective shortcut preference list (TOML override or built-in).
    shortcut_prefs: Vec<char>,
    /// Persistent cache of the merged event timeline.
    cache: ScopedCache,
    /// Timestamp of the last accepted horizontal scroll click. Trackpad
    /// swipes emit ~20-30 ScrollLeft/Right events per gesture, so we
    /// debounce them through [`HORIZONTAL_SCROLL_COOLDOWN`] to collapse
    /// a quick flick into a single navigation step instead of skipping
    /// 20 days at once.
    last_horizontal_scroll: Option<Instant>,
    /// Timestamp of the most recent vertical scroll. macOS trackpads
    /// emit micro horizontal-scroll events alongside any vertical
    /// gesture — without axis-locking off the recent vertical event,
    /// those false-horizontal events would fire date navigation in the
    /// middle of agenda scrolling and undo each row of vertical motion.
    last_vertical_scroll: Option<Instant>,
    /// Resolved first-day-of-week from config — drives both the Week
    /// view's column order and the Month view's grid + header. Cached
    /// as a chrono `Weekday` so the per-render math stays cheap.
    first_day_of_week: Weekday,
}

/// Minimum gap between two horizontal-scroll-driven anchor jumps. A typical
/// trackpad flick (~300ms of ~30 events) should produce one navigation
/// step; slow deliberate scroll still feeds steady steps.
const HORIZONTAL_SCROLL_COOLDOWN: Duration = Duration::from_millis(200);

/// Window during which horizontal-scroll events are dropped after any
/// vertical scroll. macOS trackpads emit a few micro horizontal events
/// per vertical gesture — those are jitter, not intent. Generous enough
/// that the entire vertical gesture is covered, tight enough that a
/// deliberate horizontal flick after the user clearly stops vertical
/// scrolling still gets through.
const VERTICAL_AXIS_LOCK_WINDOW: Duration = Duration::from_millis(700);

impl CalendarWidget {
    pub fn with_config(
        instance: String,
        config: CalendarConfig,
        app_theme: Arc<Theme>,
        cache: ScopedCache,
    ) -> Self {
        let (provider, source_label, auth_hint) = build_provider(&config);
        let colors = CalendarColors::build(&config);
        let poll_interval = Duration::from_secs(config.poll_interval_secs.max(15));
        let mut state = CalendarState {
            gradient: config.gradient,
            poll: crate::polling::PollTracker::new(poll_interval),
            ..CalendarState::default()
        };
        // Seed events from cache so the first frame shows last session's
        // timeline while the provider refresh runs in the background.
        if let Some(entry) = cache.load::<Vec<Event>>(CACHE_KEY_EVENTS) {
            state.poll.seed_from_cache_age(entry.age());
            state.events = entry.value;
        }
        state.poll.apply_jitter(&format!("calendar@{instance}"));
        let colors_override = config.colors.clone();
        let theme = app_theme.with_overrides(&colors_override);
        let shortcut_prefs = if config.shortcuts.is_empty() {
            vec!['c', 'd', 'a', 'l', 'e', 'n', 'r']
        } else {
            config.shortcuts.clone()
        };
        let id = if instance == "main" {
            "calendar".to_string()
        } else {
            format!("calendar@{instance}")
        };
        let display_name_cache = if instance == "main" {
            "Calendar".to_string()
        } else {
            format!("Calendar ({instance})")
        };
        Self {
            id,
            instance,
            display_name_cache,
            view: config.default_view,
            anchor: Local::now().date_naive(),
            provider,
            source_label,
            auth_hint,
            colors,
            state: Arc::new(Mutex::new(state)),
            app_theme,
            colors_override,
            theme,
            shortcut: None,
            shortcut_prefs,
            cache,
            last_horizontal_scroll: None,
            last_vertical_scroll: None,
            first_day_of_week: config.first_day_of_week.as_weekday(),
        }
    }

    fn is_due(&self) -> bool {
        let st = self.state.lock().expect("calendar state poisoned");
        if st.inflight {
            return false;
        }
        st.poll.is_due()
    }

    /// Has the events vec settled to the current view's range? `false`
    /// while a refresh is in flight or pending (`mark_dirty` clears
    /// the tracker); render uses this to decide between showing
    /// "No events." and leaving the agenda blank during the brief
    /// window where the events vec is from the previously-fetched
    /// range and might be spuriously empty for the new anchor.
    fn agenda_data_loaded(&self) -> bool {
        let st = self.state.lock().expect("calendar state poisoned");
        !st.inflight && st.poll.has_attempted()
    }

    /// Range we ask the provider for — `current_range` widened by a
    /// per-view buffer so adjacent days / weeks / months land in the
    /// same fetch and navigation within that buffer is instant.
    /// Without the buffer, every `h` / `l` press in Day view forced
    /// a refetch (Day's `current_range` is only the anchor + preview
    /// = 2 days), so even days the user *just* visited weren't in
    /// `state.events` anymore.
    fn fetch_range(&self) -> (DateTime<Local>, DateTime<Local>) {
        let (start, end) = self.current_range();
        let buffer = match self.view {
            // ±2 weeks lets the user step through a month of days
            // without re-hitting the provider.
            CalendarView::Day => ChronoDuration::days(14),
            // Same buffer; a full extra month each side fits in one fetch.
            CalendarView::Week => ChronoDuration::days(14),
            // ±1 month so Month-view's ←/→ across adjacent months is
            // also covered.
            CalendarView::Month => ChronoDuration::days(31),
        };
        (start - buffer, end + buffer)
    }

    /// True when the most recent successful fetch covers the current
    /// display range. Used by `mark_dirty_if_uncovered` to skip the
    /// forced refresh that fires on every anchor / view change — most
    /// navigations within the fetch buffer are already covered.
    fn current_range_covered(&self) -> bool {
        let (display_start, display_end) = self.current_range();
        let st = self.state.lock().expect("calendar state poisoned");
        match st.last_fetched_range {
            Some((s, e)) => display_start >= s && display_end <= e,
            None => false,
        }
    }

    /// Like `mark_dirty`, but only fires when the new display range
    /// isn't already covered by the last successful fetch. Anchor and
    /// view changes route through this so navigation within the fetch
    /// buffer doesn't trigger spurious refetches. Background polling
    /// still catches stale data on its own cadence.
    fn mark_dirty_if_uncovered(&self) {
        if self.current_range_covered() {
            return;
        }
        let mut st = self.state.lock().expect("calendar state poisoned");
        st.poll.mark_dirty();
    }

    fn current_range(&self) -> (DateTime<Local>, DateTime<Local>) {
        let (start, end) = match self.view {
            // Fetch two days when in Day view: the wide layout previews the
            // *next* day next to the anchor, and we don't want it to render
            // "No events" just because the next day's events weren't fetched.
            CalendarView::Day => (self.anchor, self.anchor + ChronoDuration::days(2)),
            CalendarView::Week => {
                let s = start_of_week(self.anchor, self.first_day_of_week);
                (s, s + ChronoDuration::days(7))
            }
            CalendarView::Month => {
                let s = start_of_month(self.anchor);
                let e = first_of_next_month(self.anchor);
                (s, e)
            }
        };
        (
            local_midnight(start).expect("midnight is valid"),
            local_midnight(end).expect("midnight is valid"),
        )
    }

    fn spawn_refresh(&self) {
        let (start, end) = self.fetch_range();
        {
            let mut st = self.state.lock().expect("calendar state poisoned");
            st.inflight = true;
            st.poll.mark_attempted();
            st.dirty = true;
        }
        let provider = self.provider.clone();
        let state = self.state.clone();
        let cache = self.cache.clone();
        tokio::spawn(async move {
            let result = provider.fetch_range(start, end).await;
            let mut st = state.lock().expect("calendar state poisoned");
            st.inflight = false;
            st.dirty = true;
            match result {
                Ok(events) => {
                    if let Err(err) = cache.store(CACHE_KEY_EVENTS, &events) {
                        tracing::warn!(error = %err, "calendar cache store failed");
                    }
                    st.events = events;
                    st.last_fetched_range = Some((start, end));
                    st.last_error = None;
                }
                Err(err) => {
                    tracing::warn!(error = %err, "calendar fetch failed");
                    st.last_error = Some(err.to_string());
                }
            }
        });
    }

    /// Reset the agenda scroll offset to the top and re-arm the
    /// auto-scroll-to-now pass. Called whenever the anchor day or
    /// the view mode changes so the user doesn't land mid-list after
    /// navigating to a different day — the next render will then
    /// re-position to the first event at-or-after the current time
    /// (when the displayed day is today).
    fn reset_agenda_scroll(&self) {
        let mut st = self.state.lock().expect("calendar state poisoned");
        st.agenda_scroll = 0;
        st.agenda_scroll_max = 0;
        st.agenda_autoscroll_done = false;
        // Per-day Week-view offsets reset alongside the shared agenda
        // scroll — navigating to a different week means the previous
        // week's column-by-column scroll positions are no longer
        // meaningful for the new dates.
        st.week_col_scroll = [0; 7];
        st.week_col_scroll_max = [0; 7];
    }

    /// Does the currently-shown range cover today? Day view checks
    /// anchor-equals-today; Week view checks today falls inside the
    /// Sun..=Sat window containing the anchor; Month view checks
    /// today's calendar month matches the anchor's. Drives the
    /// `[Today]` button's lit-vs-dim styling so the user can tell
    /// at a glance whether jumping to today would change the view.
    fn current_view_contains_today(&self) -> bool {
        let today = Local::now().date_naive();
        match self.view {
            CalendarView::Day => self.anchor == today,
            CalendarView::Week => {
                let start = start_of_week(self.anchor, self.first_day_of_week);
                let end = start + ChronoDuration::days(6);
                today >= start && today <= end
            }
            CalendarView::Month => {
                today.year() == self.anchor.year() && today.month() == self.anchor.month()
            }
        }
    }

    /// Accept-or-drop gate for ScrollLeft/Right. Returns `true` only when
    /// the event represents real horizontal intent: a vertical scroll
    /// hasn't fired recently (axis-lock filters out the micro horizontal
    /// events macOS trackpads emit alongside any vertical gesture) AND
    /// enough time has elapsed since the last accepted horizontal scroll
    /// to take another step (burst-debounce — a trackpad flick emits
    /// ~30 events in 300ms; without this gate one flick would skip 20+
    /// days at once).
    fn consume_horizontal_scroll(&mut self) -> bool {
        let now = Instant::now();
        if let Some(prev_vert) = self.last_vertical_scroll {
            if now.duration_since(prev_vert) < VERTICAL_AXIS_LOCK_WINDOW {
                return false;
            }
        }
        if let Some(prev) = self.last_horizontal_scroll {
            if now.duration_since(prev) < HORIZONTAL_SCROLL_COOLDOWN {
                return false;
            }
        }
        self.last_horizontal_scroll = Some(now);
        true
    }

    /// Distance one ←/→ keystroke advances the anchor by. View-
    /// dependent: Day → 1 day, Week → 7 days, Month → ~30 days.
    fn nav_step(&self) -> ChronoDuration {
        match self.view {
            CalendarView::Day => ChronoDuration::days(1),
            CalendarView::Week => ChronoDuration::days(7),
            CalendarView::Month => ChronoDuration::days(30),
        }
    }

    /// Adjust the agenda scroll by `delta` rows, clamping against the
    /// last-rendered maximum. Up/Down keys and mouse wheel route here.
    /// Also marks the autoscroll as done so render doesn't fight the
    /// user's manual position on the next tick.
    fn scroll_agenda(&self, delta: i32) {
        let mut st = self.state.lock().expect("calendar state poisoned");
        let max = st.agenda_scroll_max as i32;
        let next = (st.agenda_scroll as i32 + delta).clamp(0, max);
        st.agenda_scroll = next as u16;
        st.agenda_autoscroll_done = true;
    }

    /// Week-view per-column scroll. Routes a wheel event over a specific
    /// day-of-week to that column's scroll offset so scrolling Monday
    /// doesn't shift Friday. `mouse_col` is the absolute terminal column;
    /// `area` is the cell rect handed to `handle_mouse`. Out-of-bounds
    /// clicks (e.g. inside the title bar) are silently dropped.
    fn scroll_week_col(&self, mouse_col: u16, area: Rect, delta: i32) {
        if area.width < 2 || area.height < 2 {
            return;
        }
        let inner = Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2);
        let content = content_rect_for(CalendarView::Week, inner);
        if content.width == 0 || mouse_col < content.x || mouse_col >= content.x + content.width {
            return;
        }
        let dow =
            (((mouse_col - content.x) as u32 * 7) / content.width.max(1) as u32).min(6) as usize;
        let mut st = self.state.lock().expect("calendar state poisoned");
        let max = st.week_col_scroll_max[dow] as i32;
        let next = (st.week_col_scroll[dow] as i32 + delta).clamp(0, max);
        st.week_col_scroll[dow] = next as u16;
    }

    fn snapshot_events(&self) -> Vec<Event> {
        let st = self.state.lock().expect("calendar state poisoned");
        st.events.clone()
    }

    /// In week view the inner area is split into 7 equal-ratio columns, with
    /// the bottom row reserved for the hint. Maps a click to the date in the
    /// matching column.
    fn week_day_at(&self, col: u16, row: u16, inner: Rect) -> Option<NaiveDate> {
        let usable_height = inner.height.saturating_sub(1); // last row = hint
        if row < inner.y || row >= inner.y + usable_height {
            return None;
        }
        if col < inner.x || col >= inner.x + inner.width || inner.width == 0 {
            return None;
        }
        let dow = ((col - inner.x) as u32 * 7) / inner.width.max(1) as u32;
        let dow = dow.min(6) as i64;
        Some(start_of_week(self.anchor, self.first_day_of_week) + ChronoDuration::days(dow))
    }

    /// Month view layout: top padding (1 row) + month name (1 row) + weekday
    /// header (1 row) + 6 week rows. With multi-month layout, the inner area
    /// is split into N equal columns; each column hosts a centered 35-char
    /// grid. Maps a click to the (month, week, day-of-week) that landed under
    /// it, returning the actual date.
    fn month_day_at(&self, col: u16, row: u16, inner: Rect) -> Option<NaiveDate> {
        let usable_height = inner.height.saturating_sub(1);
        let rel_y = row.checked_sub(inner.y)?;
        // Padding row 0, month-name row 1, weekday header row 2, weeks 3-8.
        if rel_y < 3 || rel_y >= usable_height {
            return None;
        }
        let week = (rel_y - 3) as i64;
        if !(0..6).contains(&week) {
            return None;
        }

        let (anchor_y, anchor_m) = (self.anchor.year(), self.anchor.month());
        let months: Vec<(i32, u32)> = if inner.width >= 3 * MONTH_GRID_MIN_WIDTH {
            vec![
                advance_month(anchor_y, anchor_m, -1),
                (anchor_y, anchor_m),
                advance_month(anchor_y, anchor_m, 1),
            ]
        } else if inner.width >= 2 * MONTH_GRID_MIN_WIDTH {
            vec![(anchor_y, anchor_m), advance_month(anchor_y, anchor_m, 1)]
        } else {
            vec![(anchor_y, anchor_m)]
        };

        let col_rel = col.checked_sub(inner.x)?;
        let n = months.len() as u16;
        let col_width = inner.width / n;
        if col_width == 0 {
            return None;
        }
        let month_idx = ((col_rel / col_width) as usize).min(months.len() - 1);
        let (y, m) = months[month_idx];

        // Each month's 35-char grid is centered within its column.
        let col_start_x = inner.x + month_idx as u16 * col_width;
        let grid_offset = col_width.saturating_sub(MONTH_GRID_WIDTH) / 2;
        let rel_x = col.checked_sub(col_start_x + grid_offset)?;
        let cell = rel_x / 5;
        if cell >= 7 {
            return None;
        }
        let first = NaiveDate::from_ymd_opt(y, m, 1)?;
        let grid_start = start_of_week(first, self.first_day_of_week);
        Some(grid_start + ChronoDuration::days(week * 7 + cell as i64))
    }
}

const MONTH_GRID_WIDTH: u16 = 35;
const MONTH_GRID_MIN_WIDTH: u16 = 37;

/// Distinct interactions exposed in the bottom hint row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BottomAction {
    Today,
    View(CalendarView),
}

/// Day and Month views get a 1-col gutter on each side of the widget's
/// inner area so the content doesn't sit flush against the rounded border.
/// Week view is already column-packed (7 cells + 6 separators); padding it
/// would compress the day cells, so it stays flush. All views also reserve
/// the bottom row for the `[Today] [Day] [Week] [Month]  ←/→ nav` hint —
/// without that reservation, the last visible agenda row gets painted
/// over by the hint and the user "can't scroll to the end" of a long day.
/// Both `render` and `handle_mouse` route through this helper so
/// click→date mapping aligns with the rendered grid.
fn content_rect_for(view: CalendarView, inner: Rect) -> Rect {
    let body_height = inner.height.saturating_sub(1);
    match view {
        CalendarView::Day | CalendarView::Month if inner.width >= 4 => Rect {
            x: inner.x + 1,
            y: inner.y,
            width: inner.width - 2,
            height: body_height,
        },
        _ => Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: body_height,
        },
    }
}

/// Maps a click in the bottom hint row to a button. Layout must mirror the
/// spans emitted in `render`: leading space, `[Today]`, space, then `[Label]`
/// view tabs separated by single spaces.
fn bottom_action_at(click_col: u16, hint_x: u16) -> Option<BottomAction> {
    let mut x = hint_x + 1; // leading space
    let today_w = "Today".len() as u16 + 2;
    if click_col >= x && click_col < x + today_w {
        return Some(BottomAction::Today);
    }
    x += today_w + 1;
    for (v, label) in VIEW_TABS {
        let w = label.chars().count() as u16 + 2;
        if click_col >= x && click_col < x + w {
            return Some(BottomAction::View(*v));
        }
        x += w + 1;
    }
    None
}

/// Returns `(provider, source_label, auth_hint)`. The provider is either a
/// single backend (Local / Google / Outlook / CalDAV) or a CompositeProvider
/// fanning out to multiple. `source_label` becomes the `[label]` shown in the
/// cell title (`google`, `local`, `google+outlook`, etc.).
fn build_provider(config: &CalendarConfig) -> (Arc<dyn CalendarProvider>, String, Option<String>) {
    let local_file = LocalCalendarFile {
        events: config.events.clone(),
    };
    let local: Arc<dyn CalendarProvider> = match LocalCalendarProvider::from_file(local_file) {
        Ok(p) => Arc::new(p),
        Err(err) => {
            tracing::warn!(error = %err, "failed to parse calendar.toml events, starting empty");
            Arc::new(LocalCalendarProvider::empty())
        }
    };

    // Empty `[[providers]]` means "local only" — bail with the seeded
    // LocalCalendarProvider from above.
    if config.providers.is_empty() {
        return (local, "local".into(), None);
    }
    let entries: Vec<ProviderEntry> = config.providers.clone();

    let mut built: Vec<(Arc<dyn CalendarProvider>, &'static str)> = Vec::new();
    let mut hints: Vec<String> = Vec::new();
    for entry in &entries {
        match build_entry(entry, config) {
            Ok((provider, label)) => built.push((provider, label)),
            Err(hint) => hints.push(hint),
        }
    }

    if built.is_empty() {
        // Every requested provider failed — fall back to local so the widget
        // keeps rendering something useful with the hint banner above.
        let hint = if hints.is_empty() {
            None
        } else {
            Some(hints.join(" · "))
        };
        return (local, "local".into(), hint);
    }

    let labels: Vec<&'static str> = built.iter().map(|(_, l)| *l).collect();
    let source_label = labels.join("+");
    let hint = if hints.is_empty() {
        None
    } else {
        Some(hints.join(" · "))
    };
    let provider: Arc<dyn CalendarProvider> = if built.len() == 1 {
        built.into_iter().next().unwrap().0
    } else {
        Arc::new(CompositeProvider::new(
            built.into_iter().map(|(p, _)| p).collect(),
        ))
    };
    (provider, source_label, hint)
}

/// Build one provider entry. Returns Ok with a static label on success, Err
/// with a human-readable hint string on configuration failure.
fn build_entry(
    entry: &ProviderEntry,
    config: &CalendarConfig,
) -> Result<(Arc<dyn CalendarProvider>, &'static str), String> {
    match entry.kind {
        ProviderKind::Local => {
            let file = LocalCalendarFile {
                events: config.events.clone(),
            };
            let p =
                LocalCalendarProvider::from_file(file).map_err(|e| format!("local events: {e}"))?;
            Ok((Arc::new(p), "local"))
        }
        ProviderKind::Google => build_google_entry(&entry.calendar_ids).map(|p| (p, "google")),
        ProviderKind::Outlook => build_outlook_entry(&entry.calendar_ids).map(|p| (p, "outlook")),
        ProviderKind::Caldav => {
            let urls = if entry.calendar_ids.is_empty() {
                config.caldav.calendars.clone()
            } else {
                entry.calendar_ids.clone()
            };
            build_caldav_entry(urls).map(|p| (p, "caldav"))
        }
    }
}

fn build_outlook_entry(calendar_ids: &[String]) -> Result<Arc<dyn CalendarProvider>, String> {
    let client = MicrosoftClientConfig::load().map_err(|err| {
        tracing::warn!(error = %err, "microsoft_oauth_client.toml missing or invalid");
        "Drop microsoft_oauth_client.toml in ~/.config/glint/credentials/".to_string()
    })?;
    let token = MicrosoftToken::load()
        .map_err(|err| format!("Outlook token unreadable: {err}"))?
        .ok_or_else(|| "Run `glint --auth microsoft` to connect Microsoft Outlook".to_string())?;
    OutlookCalendarProvider::new(client, token, calendar_ids.to_vec())
        .map(|p| Arc::new(p) as Arc<dyn CalendarProvider>)
        .map_err(|err| format!("Outlook init failed: {err}"))
}

fn build_google_entry(calendar_ids: &[String]) -> Result<Arc<dyn CalendarProvider>, String> {
    let client = GoogleClientConfig::load().map_err(|err| {
        tracing::warn!(error = %err, "google_oauth_client.toml missing or invalid");
        "Drop google_oauth_client.toml in ~/.config/glint/credentials/".to_string()
    })?;
    let token = match GoogleToken::load() {
        Ok(Some(t)) => t,
        Ok(None) => {
            return Err("Run `glint --auth google` to connect Google Calendar".into());
        }
        Err(err) => return Err(format!("Google token unreadable: {err}")),
    };
    GoogleCalendarProvider::new(client, token, calendar_ids.to_vec())
        .map(|p| Arc::new(p) as Arc<dyn CalendarProvider>)
        .map_err(|err| format!("Google init failed: {err}"))
}

fn build_caldav_entry(urls: Vec<String>) -> Result<Arc<dyn CalendarProvider>, String> {
    let creds = match CalDavCredentials::load() {
        Ok(Some(c)) => c,
        Ok(None) => {
            return Err("Fill in ~/.config/glint/credentials/caldav.toml to connect CalDAV".into());
        }
        Err(err) => return Err(format!("CalDAV credentials unreadable: {err}")),
    };
    CalDavProvider::new(creds, urls)
        .map(|p| Arc::new(p) as Arc<dyn CalendarProvider>)
        .map_err(|err| format!("CalDAV init failed: {err}"))
}

/// Meta-provider that fans `fetch_range` calls out to every wrapped provider
/// in parallel and merges the results. Each child's failures are logged
/// individually; one failing source doesn't block the others.
struct CompositeProvider {
    inner: Vec<Arc<dyn CalendarProvider>>,
}

impl CompositeProvider {
    fn new(inner: Vec<Arc<dyn CalendarProvider>>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl CalendarProvider for CompositeProvider {
    async fn fetch_range(
        &self,
        start: DateTime<Local>,
        end: DateTime<Local>,
    ) -> Result<Vec<Event>> {
        let futs = self.inner.iter().map(|p| p.fetch_range(start, end));
        let results = futures::future::join_all(futs).await;
        let mut all = Vec::new();
        for r in results {
            match r {
                Ok(mut chunk) => all.append(&mut chunk),
                Err(err) => {
                    tracing::warn!(error = %err, "child calendar provider failed");
                }
            }
        }
        all.sort_by_key(|e| e.start);
        Ok(all)
    }
}

fn advance_month(year: i32, month: u32, delta: i32) -> (i32, u32) {
    let total = year * 12 + (month as i32 - 1) + delta;
    let new_year = total.div_euclid(12);
    let new_month = (total.rem_euclid(12) + 1) as u32;
    (new_year, new_month)
}

/// Renders one month's 6-week grid into `area`. `is_anchor` controls header
/// styling so the currently-focused month stands out among neighbors.
/// `selected` is the day the user has chosen (drives the agenda below); its
/// cell is drawn with reversed colors so the selection is unambiguous even
/// when it coincides with — or differs from — today.
fn render_month_grid(
    frame: &mut Frame,
    area: Rect,
    year: i32,
    month: u32,
    is_anchor: bool,
    selected: NaiveDate,
    events: &[Event],
    theme: &Theme,
    first_day_of_week: Weekday,
) {
    let Some(first) = NaiveDate::from_ymd_opt(year, month, 1) else {
        return;
    };
    let grid_start = start_of_week(first, first_day_of_week);
    let today = Local::now().date_naive();

    let month_header_style = if is_anchor {
        theme.text_selected
    } else {
        theme.text_dim
    };
    // Rotate the Sun-anchored weekday label list so the configured
    // first-day-of-week appears in the leftmost column.
    let weekday_labels = rotated_weekday_labels(first_day_of_week);
    let weekday_header = Line::from(
        weekday_labels
            .iter()
            .map(|s| {
                Span::styled(
                    format!("{s:^5}"),
                    Style::default().add_modifier(Modifier::BOLD),
                )
            })
            .collect::<Vec<_>>(),
    );

    let mut lines: Vec<Line<'_>> = Vec::with_capacity(9);
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("{} {}", month_long(month), year),
        month_header_style,
    )));
    lines.push(weekday_header);

    for week in 0..6 {
        let mut spans: Vec<Span<'_>> = Vec::with_capacity(7);
        for dow in 0..7 {
            let date = grid_start + ChronoDuration::days(week * 7 + dow);
            let in_month = date.month() == month;
            let day_str = format!("{}", date.day());
            let cell = if date == today {
                format!("[{day_str:>2}]")
            } else {
                format!(" {day_str:>2} ")
            };
            let has_events = events.iter().any(|e| e.on_date(date));
            // Cyan-bold "has events" highlight is reserved for the
            // real-life current month (today's month). When the user
            // navigates the anchor to July, July's event days stay neutral
            // — we don't want a non-current month to light up just because
            // an async fetch returned later.
            let is_current_month = date.year() == today.year() && date.month() == today.month();
            let mut style = if !in_month {
                theme.text_dim
            } else if has_events && is_current_month {
                theme.text_focused
            } else {
                theme.text_plain
            };
            if date == selected {
                style = style.add_modifier(Modifier::REVERSED);
            }
            spans.push(Span::styled(format!("{cell:<5}"), style));
        }
        lines.push(Line::from(spans));
    }

    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
}

fn local_midnight(date: NaiveDate) -> Option<DateTime<Local>> {
    Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0)?)
        .single()
}

/// Roll `d` back to the start of the week, where the week starts on
/// `first_day_of_week`. `from_sun = (today_dow - first_dow) mod 7` —
/// that's how many days to subtract regardless of which weekday the
/// caller chose to anchor on.
fn start_of_week(d: NaiveDate, first_day_of_week: Weekday) -> NaiveDate {
    let today_idx = d.weekday().num_days_from_sunday();
    let first_idx = first_day_of_week.num_days_from_sunday();
    let offset = (today_idx + 7 - first_idx) % 7;
    d - ChronoDuration::days(i64::from(offset))
}

/// The seven weekday-short labels in column order, starting from
/// `first_day_of_week`. Used by Week- and Month-view headers so the
/// label row matches the grid's day ordering.
fn rotated_weekday_labels(first_day_of_week: Weekday) -> [&'static str; 7] {
    const SUN_ANCHORED: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let shift = first_day_of_week.num_days_from_sunday() as usize;
    let mut out = [""; 7];
    for i in 0..7 {
        out[i] = SUN_ANCHORED[(i + shift) % 7];
    }
    out
}

fn start_of_month(d: NaiveDate) -> NaiveDate {
    NaiveDate::from_ymd_opt(d.year(), d.month(), 1).unwrap_or(d)
}

fn first_of_next_month(d: NaiveDate) -> NaiveDate {
    let (y, m) = if d.month() == 12 {
        (d.year() + 1, 1)
    } else {
        (d.year(), d.month() + 1)
    };
    NaiveDate::from_ymd_opt(y, m, 1).unwrap_or(d)
}

/// Built-in palette cycled across calendars when the user hasn't supplied
/// their own `color_palette` in calendar.toml. Eight slots so up to eight
/// calendars get unique colors before the sequence repeats.
const DEFAULT_PALETTE: [Color; 8] = [
    Color::LightBlue,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightMagenta,
    Color::LightCyan,
    Color::LightRed,
    Color::Blue,
    Color::Green,
];

/// Resolves an `Event.source + Event.calendar` pair to a terminal color.
///
/// Construction is config-driven: explicit overrides from `[calendar_colors]`
/// win, then everything that appears in `[[providers]]` gets the next palette
/// slot in declaration order, and anything we encounter at runtime that the
/// config didn't anticipate (rare — happens with CalDAV auto-discovery)
/// falls back to a stable hash of the composite key.
struct CalendarColors {
    palette: Vec<Color>,
    /// Explicit per-calendar overrides keyed by `(source, calendar_id)`.
    overrides: HashMap<(String, String), Color>,
    /// Pre-computed palette index for each calendar declared in config.
    assigned: HashMap<(String, String), usize>,
}

impl CalendarColors {
    fn build(config: &CalendarConfig) -> Self {
        // Parse the user palette; fall back to defaults when entries are
        // empty or unrecognized rather than silently dropping the calendar's
        // distinct color.
        let palette: Vec<Color> = if config.color_palette.is_empty() {
            DEFAULT_PALETTE.to_vec()
        } else {
            let mut parsed: Vec<Color> = config
                .color_palette
                .iter()
                .filter_map(|s| parse_color(s))
                .collect();
            if parsed.is_empty() {
                parsed = DEFAULT_PALETTE.to_vec();
            }
            parsed
        };

        // Per-calendar overrides. Keys take the form "source:calendar_id".
        // Anything we can't parse is logged once and dropped so the rest of
        // the map still applies.
        let mut overrides: HashMap<(String, String), Color> = HashMap::new();
        for (key, value) in &config.calendar_colors {
            let Some((source, calendar)) = key.split_once(':') else {
                tracing::warn!(
                    key = %key,
                    "calendar_colors key missing 'source:' prefix — expected e.g. \"google:primary\""
                );
                continue;
            };
            let Some(color) = parse_color(value) else {
                tracing::warn!(key = %key, value = %value, "unrecognized color name");
                continue;
            };
            overrides.insert((source.to_string(), calendar.to_string()), color);
        }

        // Walk `[[providers]]` in order and assign each declared calendar
        // the next palette index. Overrides don't consume a slot, so the
        // next non-overridden calendar still gets palette[0].
        let entries: Vec<ProviderEntry> = config.providers.clone();

        let mut assigned: HashMap<(String, String), usize> = HashMap::new();
        let mut next_idx: usize = 0;
        for entry in &entries {
            let source = provider_kind_label(entry.kind);
            let ids: Vec<String> = if entry.calendar_ids.is_empty() {
                // An empty list means "the provider's default calendar".
                // Each provider names that default slightly differently, but
                // for color purposes we just need a stable key.
                vec!["primary".to_string()]
            } else {
                entry.calendar_ids.clone()
            };
            for id in ids {
                let key = (source.to_string(), id);
                if overrides.contains_key(&key) || assigned.contains_key(&key) {
                    continue;
                }
                assigned.insert(key, next_idx);
                next_idx += 1;
            }
        }

        Self {
            palette,
            overrides,
            assigned,
        }
    }

    fn resolve(&self, source: &str, calendar: &str) -> Color {
        let key = (source.to_string(), calendar.to_string());
        if let Some(c) = self.overrides.get(&key) {
            return *c;
        }
        if let Some(idx) = self.assigned.get(&key) {
            return self.palette[idx % self.palette.len()];
        }
        // Unknown calendar — hash the composite key into the palette so at
        // least same-name events stay one color across renders.
        let mut hash: u32 = 5381;
        for b in source
            .bytes()
            .chain(b":".iter().copied())
            .chain(calendar.bytes())
        {
            hash = hash.wrapping_mul(33).wrapping_add(u32::from(b));
        }
        self.palette[(hash as usize) % self.palette.len()]
    }
}

fn provider_kind_label(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Local => "local",
        ProviderKind::Google => "google",
        ProviderKind::Outlook => "outlook",
        ProviderKind::Caldav => "caldav",
    }
}

/// Maps a color name (case-insensitive, hyphens or underscores) to a
/// Ratatui `Color`. ANSI 16-color names plus a few common aliases.

fn weekday_short(w: Weekday) -> &'static str {
    match w {
        Weekday::Mon => "Mon",
        Weekday::Tue => "Tue",
        Weekday::Wed => "Wed",
        Weekday::Thu => "Thu",
        Weekday::Fri => "Fri",
        Weekday::Sat => "Sat",
        Weekday::Sun => "Sun",
    }
}

fn month_long(m: u32) -> &'static str {
    match m {
        1 => "January",
        2 => "February",
        3 => "March",
        4 => "April",
        5 => "May",
        6 => "June",
        7 => "July",
        8 => "August",
        9 => "September",
        10 => "October",
        11 => "November",
        12 => "December",
        _ => "???",
    }
}

impl CalendarWidget {
    /// The base title — just "Calendar" or "Calendar (instance)" —
    /// without any view-specific metadata. The metadata side of the
    /// title bar comes from [`Self::title_metadata_string`].
    fn title_for_header(&self) -> String {
        if self.instance == "main" {
            "Calendar".to_string()
        } else {
            format!("Calendar ({})", self.instance)
        }
    }

    /// Dynamic metadata appended after the title (e.g. `[google+outlook]
    /// Sat May 23, 2026`). Rendered via the shared `title_row` helper
    /// so the styling matches every other widget's title bar.
    fn title_metadata_string(&self) -> String {
        let source = self.source_label.as_str();
        match self.view {
            CalendarView::Day => format!(
                "[{source}] {} {} {}, {}",
                weekday_short(self.anchor.weekday()),
                month_long(self.anchor.month()),
                self.anchor.day(),
                self.anchor.year()
            ),
            CalendarView::Week => {
                let s = start_of_week(self.anchor, self.first_day_of_week);
                let e = s + ChronoDuration::days(6);
                format!(
                    "[{source}] week of {} {}–{}",
                    month_long(s.month()),
                    s.day(),
                    e.day()
                )
            }
            CalendarView::Month => format!(
                "[{source}] {} {}",
                month_long(self.anchor.month()),
                self.anchor.year()
            ),
        }
    }

    fn render_day(&self, frame: &mut Frame, area: Rect, events: &[Event]) {
        // When the cell is wide enough, preview the selected day alongside
        // the day after. Selected day's date is highlighted; the preview's
        // date is dim/gray so it's clear which is "today's selection".
        // A 1-col `│` separator runs down the middle so the two days don't
        // visually blur into one another.
        const TWO_DAY_MIN_WIDTH: u16 = 50;
        let show_next_day = area.width >= TWO_DAY_MIN_WIDTH;
        if show_next_day {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Fill(1),
                    Constraint::Length(1),
                    Constraint::Fill(1),
                ])
                .split(area);
            // Pad the agenda body 1 col away from the vertical separator on
            // both sides. Headers stay centered in the full column, so the
            // big-digit date numerals keep their visual anchoring while the
            // text-heavy agenda lines breathe around the divider.
            self.render_day_column(frame, cols[0], self.anchor, true, 0, 1, events);
            // Carve a sub-rect that's inset one row from the top (so the
            // separator doesn't kiss the cell border) and one row from the
            // bottom (so it doesn't overlap with the view-tab hint row).
            let sep_height = cols[1].height.saturating_sub(2);
            if sep_height > 0 {
                let sep_area = Rect {
                    x: cols[1].x,
                    y: cols[1].y + 1,
                    width: cols[1].width,
                    height: sep_height,
                };
                let sep_lines: Vec<Line<'_>> = (0..sep_height)
                    .map(|_| Line::from(Span::styled("│", self.theme.text_dim)))
                    .collect();
                frame.render_widget(Paragraph::new(sep_lines), sep_area);
            }
            let next = self.anchor + ChronoDuration::days(1);
            self.render_day_column(frame, cols[2], next, false, 1, 0, events);
        } else {
            self.render_day_column(frame, area, self.anchor, true, 0, 0, events);
        }
    }

    fn render_day_column(
        &self,
        frame: &mut Frame,
        area: Rect,
        date: NaiveDate,
        is_anchor: bool,
        body_left_pad: u16,
        body_right_pad: u16,
        events: &[Event],
    ) {
        let day_events: Vec<&Event> = events.iter().filter(|e| e.on_date(date)).collect();

        let header_height = 8u16.min(area.height);
        let header_area = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: header_height,
        };
        // Body inset by `body_left_pad`/`body_right_pad` lets the two-day
        // split keep its agenda text off the central separator line.
        let body_pad_total = body_left_pad + body_right_pad;
        let body_area = Rect {
            x: area.x + body_left_pad,
            y: area.y + header_height,
            width: area.width.saturating_sub(body_pad_total),
            height: area.height.saturating_sub(header_height),
        };

        let header_text = format!(
            "{} · {}",
            weekday_short(date.weekday()),
            month_long(date.month()),
        );
        // Yellow if this column is showing the actual current date; gray for
        // every other day. Keeps "today" instantly identifiable when the user
        // has navigated away with ← / →.
        let today = Local::now().date_naive();
        let is_today = date == today;
        let date_style = if is_today {
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD)
        } else if is_anchor {
            // The anchor day (selected but not today) — slightly brighter
            // than the preview column so the user can tell which is active.
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let mut header_lines: Vec<Line<'_>> = vec![
            Line::from(""),
            Line::from(Span::styled(header_text, self.theme.text_dim)),
        ];
        // For today's date we hand the big-digit numeral to `render_styled`
        // so the user's gradient choice applies. Anchor and preview days keep
        // their dim single-color render — putting a vibrant gradient on a
        // non-today date would defeat the visual hierarchy.
        if is_today {
            let gradient = self.state.lock().expect("calendar state poisoned").gradient;
            let lines = big_digits::render_styled(
                &date.day().to_string(),
                gradient,
                self.theme.text_selected,
            );
            for line in lines {
                header_lines.push(line);
            }
        } else {
            for row in big_digits::render(&date.day().to_string()) {
                header_lines.push(Line::from(Span::styled(row, date_style)));
            }
        }
        frame.render_widget(
            Paragraph::new(header_lines).alignment(Alignment::Center),
            header_area,
        );

        let mut lines: Vec<Line<'static>> = Vec::new();
        // Auth hint only renders alongside the anchor day so we don't double-up.
        if is_anchor {
            if let Some(hint) = &self.auth_hint {
                lines.push(Line::from(Span::styled(
                    format!("⚠ {hint}"),
                    Style::default().fg(Color::Yellow),
                )));
                lines.push(Line::from(""));
            }
        }
        // How many lines came from auth_hint? Subtract them when
        // mapping event indices to the agenda_lines coordinate space —
        // first_future_event_line reports indices relative to the
        // agenda-only block.
        let agenda_offset_in_lines = lines.len() as u16;
        lines.extend(self.agenda_lines(&day_events, body_area.width));

        // The anchor column owns the scrollable agenda — ↑/↓/wheel
        // drive its offset. The preview column (`is_anchor = false`)
        // stays anchored at the top so the two days stay visually
        // synced.
        let total_lines = lines.len() as u16;
        let max_scroll = total_lines.saturating_sub(body_area.height);
        let effective = if is_anchor {
            let needs_autoscroll = {
                let mut st = self.state.lock().expect("calendar state poisoned");
                st.agenda_scroll_max = max_scroll;
                !st.agenda_autoscroll_done
            };
            // Only consider auto-positioning when the visible day IS
            // today AND the agenda overflows. Past/future days keep
            // the natural top-of-list position; days that fit fully
            // never need scrolling at all.
            let today = Local::now().date_naive();
            let do_autoscroll = needs_autoscroll && date == today && max_scroll > 0;
            if do_autoscroll {
                let now = Local::now();
                if let Some(rel_line) =
                    self.first_future_event_line(&day_events, body_area.width, now)
                {
                    let target = (rel_line + agenda_offset_in_lines).min(max_scroll);
                    let mut st = self.state.lock().expect("calendar state poisoned");
                    st.agenda_scroll = target;
                    st.agenda_autoscroll_done = true;
                    target
                } else {
                    // No current/future events — leave at top and mark
                    // done so render doesn't keep checking.
                    let mut st = self.state.lock().expect("calendar state poisoned");
                    st.agenda_autoscroll_done = true;
                    st.agenda_scroll
                }
            } else {
                let st = self.state.lock().expect("calendar state poisoned");
                st.agenda_scroll
            }
        } else {
            0
        };
        let effective = effective.min(max_scroll);
        let body = Paragraph::new(lines).scroll((effective, 0));
        frame.render_widget(body, body_area);
    }

    /// Build the time-aligned event list shared by Day view and the Month
    /// view's selected-day footer. Title and location each wrap to at most
    /// 2 lines when they overflow `body_width`; further overflow ends with
    /// an ellipsis (via `wrap_event_title`).
    fn agenda_lines(&self, day_events: &[&Event], body_width: u16) -> Vec<Line<'static>> {
        // Widest time label is "HH:MM–HH:MM" (11 chars). Pad every label
        // (including "all day") to that width so every title starts at
        // the same column.
        const TIME_COL_WIDTH: usize = 11;
        const TITLE_GAP: usize = 2;
        const MAX_LINES_PER_FIELD: usize = 2;
        let cont_indent = " ".repeat(TIME_COL_WIDTH + TITLE_GAP);
        let text_width = (body_width as usize)
            .saturating_sub(TIME_COL_WIDTH + TITLE_GAP)
            .max(1);

        if day_events.is_empty() {
            // Distinguish "no events for this day" from "events not yet
            // fetched for this day's range." The latter happens for
            // ~one tick after the user navigates to a new anchor (`h`
            // / `l` in Day view, day-click in Month view): `mark_dirty`
            // clears `last_attempt`, but `state.events` is still the
            // *previous* range's data — its filter for the new date
            // can be spuriously empty until the next refresh lands.
            // Show the "No events." line only once we have authoritative
            // data; before that, return an empty Vec so the agenda body
            // is blank rather than flashing the misleading message.
            return if self.agenda_data_loaded() {
                vec![Line::from(Span::styled("No events.", self.theme.text_dim))]
            } else {
                Vec::new()
            };
        }

        let mut lines: Vec<Line<'static>> = Vec::new();
        for e in day_events {
            let color = self.colors.resolve(&e.source, &e.calendar);
            let raw_time = if e.all_day {
                "all day".to_string()
            } else {
                format!(
                    "{:02}:{:02}–{:02}:{:02}",
                    e.start.hour(),
                    e.start.minute(),
                    e.end.hour(),
                    e.end.minute()
                )
            };
            let padded_time = format!("{:<width$}", raw_time, width = TIME_COL_WIDTH);
            let gap = " ".repeat(TITLE_GAP);

            let title_lines = wrap_event_title(&e.title, text_width, MAX_LINES_PER_FIELD);
            for (i, t) in title_lines.into_iter().enumerate() {
                let title_span =
                    Span::styled(t, Style::default().fg(color).add_modifier(Modifier::BOLD));
                if i == 0 {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("{padded_time}{gap}"),
                            Style::default().fg(Color::Gray),
                        ),
                        title_span,
                    ]));
                } else {
                    lines.push(Line::from(vec![Span::raw(cont_indent.clone()), title_span]));
                }
            }
            if let Some(loc) = &e.location {
                let loc_lines = wrap_event_title(loc, text_width, MAX_LINES_PER_FIELD);
                for t in loc_lines {
                    lines.push(Line::from(vec![
                        Span::raw(cont_indent.clone()),
                        Span::styled(t, self.theme.text_dim),
                    ]));
                }
            }
        }
        lines
    }

    /// Compute the line index where the first "still upcoming or in
    /// progress" event lands inside the agenda layout for `day_events`
    /// (events with `end >= now`). Returns `None` when no events meet
    /// the criterion. Mirrors the per-event line accounting in
    /// `agenda_lines` exactly — title rows (up to 2 wrap lines) +
    /// optional location rows (up to 2 wrap lines).
    fn first_future_event_line(
        &self,
        day_events: &[&Event],
        body_width: u16,
        now: chrono::DateTime<chrono::Local>,
    ) -> Option<u16> {
        const TIME_COL_WIDTH: usize = 11;
        const TITLE_GAP: usize = 2;
        const MAX_LINES_PER_FIELD: usize = 2;
        let text_width = (body_width as usize)
            .saturating_sub(TIME_COL_WIDTH + TITLE_GAP)
            .max(1);

        let mut cursor: u16 = 0;
        for e in day_events {
            // First line of this event — that's the candidate scroll
            // target if the event qualifies.
            if e.end >= now {
                return Some(cursor);
            }
            // Otherwise advance by however many lines this event consumes.
            let title_lines = wrap_event_title(&e.title, text_width, MAX_LINES_PER_FIELD);
            cursor = cursor.saturating_add(title_lines.len().max(1) as u16);
            if let Some(loc) = &e.location {
                let loc_lines = wrap_event_title(loc, text_width, MAX_LINES_PER_FIELD);
                cursor = cursor.saturating_add(loc_lines.len() as u16);
            }
        }
        None
    }

    /// Render the agenda for the month-view's selected day below the calendar
    /// grid. Shares the time-aligned event format with [`agenda_lines`].
    fn render_month_agenda(&self, frame: &mut Frame, area: Rect, events: &[Event]) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let day_events: Vec<&Event> = events.iter().filter(|e| e.on_date(self.anchor)).collect();
        let today = Local::now().date_naive();
        let header_text = format!(
            "{}, {} {}",
            weekday_short(self.anchor.weekday()),
            month_long(self.anchor.month()),
            self.anchor.day(),
        );
        let header_style = if self.anchor == today {
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD)
        };
        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::from(Span::styled(header_text, header_style)));
        // Header row counts as 1 line of lead-in; agenda events begin
        // at relative line 0 inside `agenda_lines`, which lands at
        // line 1 of `lines`. Track that offset so the autoscroll target
        // is mapped correctly.
        let agenda_offset_in_lines: u16 = lines.len() as u16;
        lines.extend(self.agenda_lines(&day_events, area.width));

        // Same scroll wiring as the Day view's anchor column — Month
        // view's footer agenda is also driven by ↑/↓ and the wheel,
        // and auto-scrolls to "now or later" events when the selected
        // day is today.
        let total = lines.len() as u16;
        let max_scroll = total.saturating_sub(area.height);
        let needs_autoscroll = {
            let mut st = self.state.lock().expect("calendar state poisoned");
            st.agenda_scroll_max = max_scroll;
            !st.agenda_autoscroll_done
        };
        let do_autoscroll = needs_autoscroll && self.anchor == today && max_scroll > 0;
        let scroll = if do_autoscroll {
            let now = Local::now();
            if let Some(rel) = self.first_future_event_line(&day_events, area.width, now) {
                let target = (rel + agenda_offset_in_lines).min(max_scroll);
                let mut st = self.state.lock().expect("calendar state poisoned");
                st.agenda_scroll = target;
                st.agenda_autoscroll_done = true;
                target
            } else {
                let mut st = self.state.lock().expect("calendar state poisoned");
                st.agenda_autoscroll_done = true;
                st.agenda_scroll.min(max_scroll)
            }
        } else {
            let st = self.state.lock().expect("calendar state poisoned");
            st.agenda_scroll.min(max_scroll)
        };
        frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), area);
    }

    fn render_week(&self, frame: &mut Frame, area: Rect, events: &[Event], focused: bool) {
        let s = start_of_week(self.anchor, self.first_day_of_week);
        // 7 day columns interleaved with 6 single-char separator columns.
        let constraints: Vec<Constraint> = (0..13)
            .map(|i| {
                if i % 2 == 0 {
                    Constraint::Ratio(1, 7)
                } else {
                    Constraint::Length(1)
                }
            })
            .collect();
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(constraints)
            .split(area);
        let today = Local::now().date_naive();

        // Layout per column:
        //   row 0:                empty top pad (vertical separators
        //                         skip this row so they don't kiss the
        //                         block's top border)
        //   row 1:                weekday short label (Sun/Mon/…)
        //   row 2:                date number (or [date] for today)
        //   row 3:                horizontal divider (─ across the full
        //                         row, with ┼ at every column separator
        //                         and ├ ┤ overpainted on the block's
        //                         left/right borders for clean connection)
        //   rows 4..bottom:       per-column scrollable events
        const WEEK_TOP_PAD: u16 = 1;
        const WEEK_LABEL_ROWS: u16 = 2;
        const WEEK_DIVIDER_ROW_OFFSET: u16 = WEEK_TOP_PAD + WEEK_LABEL_ROWS; // 3
        const WEEK_HEADER_TOTAL: u16 = WEEK_DIVIDER_ROW_OFFSET + 1; // 4

        // Horizontal divider — drawn first so the separators below can
        // overwrite this row at their column with `┼`. Block borders at
        // x = area.x - 1 (left) and x = area.x + area.width (right)
        // get repainted with `├` / `┤` so the divider bridges the box
        // cleanly instead of leaving a gap on each side.
        if WEEK_HEADER_TOTAL <= area.height {
            let divider_y = area.y + WEEK_DIVIDER_ROW_OFFSET;
            let hr_str: String = std::iter::repeat('─').take(area.width as usize).collect();
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(hr_str, self.theme.text_dim))),
                Rect {
                    x: area.x,
                    y: divider_y,
                    width: area.width,
                    height: 1,
                },
            );
            let border_style = self.theme.border_style(focused);
            if area.x >= 1 {
                frame.render_widget(
                    Paragraph::new(Span::styled("├", border_style)),
                    Rect {
                        x: area.x - 1,
                        y: divider_y,
                        width: 1,
                        height: 1,
                    },
                );
            }
            frame.render_widget(
                Paragraph::new(Span::styled("┤", border_style)),
                Rect {
                    x: area.x + area.width,
                    y: divider_y,
                    width: 1,
                    height: 1,
                },
            );
        }

        // Vertical separators between day columns. Start at row 1 (skip
        // the empty top pad) so they don't run up to the block border;
        // use `┼` at the divider intersection so the cross looks clean
        // instead of dashed-out where the lines meet.
        if area.height > WEEK_TOP_PAD {
            let sep_height = area.height - WEEK_TOP_PAD;
            for i in 0..6 {
                let sep_col = cols[i * 2 + 1];
                let mut sep_lines: Vec<Line<'_>> = Vec::with_capacity(sep_height as usize);
                for row_off in 0..sep_height {
                    let ch = if row_off == WEEK_LABEL_ROWS {
                        "┼"
                    } else {
                        "│"
                    };
                    sep_lines.push(Line::from(Span::styled(ch, self.theme.text_dim)));
                }
                frame.render_widget(
                    Paragraph::new(sep_lines),
                    Rect {
                        x: sep_col.x,
                        y: area.y + WEEK_TOP_PAD,
                        width: 1,
                        height: sep_height,
                    },
                );
            }
        }

        for i in 0..7 {
            let col_area = cols[i * 2];
            let day = s + ChronoDuration::days(i as i64);
            let is_today = day == today;
            let weekday_label = weekday_short(day.weekday());
            let date_label = if is_today {
                format!("[{}]", day.day())
            } else {
                format!("{}", day.day())
            };
            let header_style = if is_today {
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            };
            // 3 header rows: blank top pad, weekday, date. The 4th row
            // (the horizontal divider) is drawn cell-wide above, not
            // per-column.
            let header_lines: Vec<Line<'_>> = vec![
                Line::from(""),
                Line::from(Span::styled(weekday_label, header_style)),
                Line::from(Span::styled(date_label, header_style)),
            ];

            let header_h = (WEEK_TOP_PAD + WEEK_LABEL_ROWS).min(col_area.height);
            let header_rect = Rect {
                x: col_area.x,
                y: col_area.y,
                width: col_area.width,
                height: header_h,
            };
            let events_y_offset = WEEK_HEADER_TOTAL.min(col_area.height);
            let events_rect = Rect {
                x: col_area.x,
                y: col_area.y + events_y_offset,
                width: col_area.width,
                height: col_area.height.saturating_sub(events_y_offset),
            };

            frame.render_widget(
                Paragraph::new(header_lines).alignment(Alignment::Left),
                header_rect,
            );

            let day_events: Vec<&Event> = events.iter().filter(|e| e.on_date(day)).collect();
            let mut event_lines: Vec<Line<'_>> = Vec::new();
            if day_events.is_empty() {
                event_lines.push(Line::from(Span::styled("·", self.theme.text_dim)));
            } else {
                let wrap_width = col_area.width.saturating_sub(1) as usize;
                for e in day_events {
                    let color = self.colors.resolve(&e.source, &e.calendar);
                    let prefix = if e.all_day {
                        "•".to_string()
                    } else {
                        format!("{:02}:{:02}", e.start.hour(), e.start.minute())
                    };
                    // Combine the prefix and title so the wrap function
                    // accounts for the prefix's column cost on line 1.
                    // Wrapping the bare title and then prepending the
                    // prefix pushed line 1 past the column edge, which
                    // ratatui silently truncated.
                    let combined = format!("{prefix} {}", e.title);
                    let title_lines = wrap_event_title(&combined, wrap_width, 3);
                    for line in title_lines {
                        event_lines
                            .push(Line::from(Span::styled(line, Style::default().fg(color))));
                    }
                }
            }

            // Publish the per-column max scroll so the wheel handler
            // can clamp without re-running the layout, then clamp the
            // last-saved offset against it (window resize can shrink
            // the events area between renders).
            let max_scroll = (event_lines.len() as u16).saturating_sub(events_rect.height);
            let scroll = {
                let mut st = self.state.lock().expect("calendar state poisoned");
                st.week_col_scroll_max[i] = max_scroll;
                let clamped = st.week_col_scroll[i].min(max_scroll);
                st.week_col_scroll[i] = clamped;
                clamped
            };

            frame.render_widget(
                Paragraph::new(event_lines)
                    .alignment(Alignment::Left)
                    .scroll((scroll, 0)),
                events_rect,
            );
        }
    }

    fn render_month(&self, frame: &mut Frame, area: Rect, events: &[Event]) {
        // Each single-month grid wants ~37 cols (5 chars × 7 cells + a bit of
        // padding). Stack 1, 2, or 3 months side-by-side as width allows.
        let (anchor_y, anchor_m) = (self.anchor.year(), self.anchor.month());
        let months: Vec<(i32, u32)> = if area.width >= 3 * MONTH_GRID_MIN_WIDTH {
            vec![
                advance_month(anchor_y, anchor_m, -1),
                (anchor_y, anchor_m),
                advance_month(anchor_y, anchor_m, 1),
            ]
        } else if area.width >= 2 * MONTH_GRID_MIN_WIDTH {
            vec![(anchor_y, anchor_m), advance_month(anchor_y, anchor_m, 1)]
        } else {
            vec![(anchor_y, anchor_m)]
        };

        // Grid is 9 rows (1 pad + 1 month-name + 1 weekday header + 6 weeks).
        // Reserve the last row for the [Today]/[Day]/[Week]/[Month] hint that
        // `render` paints over us. If there's a comfortable gap below the
        // grid, surface the selected day's agenda there.
        const GRID_HEIGHT: u16 = 9;
        const FOOTER_RESERVED: u16 = 1;
        const SPACER: u16 = 1;
        const AGENDA_MIN_ROWS: u16 = 2;
        let usable = area.height.saturating_sub(FOOTER_RESERVED);
        let show_agenda = usable >= GRID_HEIGHT + SPACER + AGENDA_MIN_ROWS;

        let grid_area = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: if show_agenda {
                GRID_HEIGHT
            } else {
                area.height
            },
        };

        let constraints: Vec<Constraint> = (0..months.len())
            .map(|_| Constraint::Ratio(1, months.len() as u32))
            .collect();
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(constraints)
            .split(grid_area);

        for ((y, m), col_area) in months.iter().zip(cols.iter()) {
            let is_anchor = (*y, *m) == (anchor_y, anchor_m);
            render_month_grid(
                frame,
                *col_area,
                *y,
                *m,
                is_anchor,
                self.anchor,
                events,
                &self.theme,
                self.first_day_of_week,
            );
        }

        if show_agenda {
            let agenda_area = Rect {
                x: area.x,
                y: area.y + GRID_HEIGHT + SPACER,
                width: area.width,
                height: usable - GRID_HEIGHT - SPACER,
            };
            self.render_month_agenda(frame, agenda_area, events);
        }
    }
}

/// Greedy word-wrap for event titles in week view. Splits on whitespace and
/// Character-level wrap: fills each line to `max_width` columns
/// regardless of word boundaries, returning at most `max_lines` lines.
/// If the title doesn't fit, the last line gets an ellipsis (replacing
/// the last visible character when the line is already at width).
///
/// Word-aware wrapping left each line short whenever the next word
/// didn't fit, which in a Week view's narrow event cells meant most
/// lines ended with several columns of blank space. Char-level packing
/// makes use of every column; the human eye reconstructs broken words
/// from context, and the alternative (dropped trailing characters)
/// was worse.
fn wrap_event_title(text: &str, max_width: usize, max_lines: usize) -> Vec<String> {
    if max_width == 0 || max_lines == 0 {
        return Vec::new();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut lines: Vec<String> = Vec::new();
    let mut idx = 0;
    while idx < chars.len() && lines.len() < max_lines {
        // After the first line, skip any leading whitespace at the
        // start of a wrapped line. Spaces happen to land at line
        // starts when the previous slice ended on a non-space; without
        // this skip, narrow columns end up with ugly leading gaps on
        // continuation lines.
        if !lines.is_empty() {
            while idx < chars.len() && chars[idx].is_whitespace() {
                idx += 1;
            }
            if idx >= chars.len() {
                break;
            }
        }
        let end = (idx + max_width).min(chars.len());
        lines.push(chars[idx..end].iter().collect());
        idx = end;
    }
    // Add ellipsis when we ran out of line budget before consuming all
    // characters. Inline replacement (pop the last visible char) keeps
    // the line width invariant.
    if idx < chars.len() {
        if let Some(last) = lines.last_mut() {
            if last.chars().count() < max_width {
                last.push('…');
            } else if !last.ends_with('…') {
                let mut tail: Vec<char> = last.chars().collect();
                tail.pop();
                tail.push('…');
                *last = tail.into_iter().collect();
            }
        }
    }
    lines
}

#[async_trait]
impl Widget for CalendarWidget {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> &str {
        "calendar"
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
        let mut st = self.state.lock().expect("calendar state poisoned");
        std::mem::replace(&mut st.dirty, false)
    }

    fn render(&self, frame: &mut Frame, area: Rect, focused: bool) {
        let metadata = self.title_metadata_string();
        let block = apply_title_row(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(self.theme.border_style(focused)),
            focused,
            &self.title_for_header(),
            Some(metadata.as_str()),
            MetadataEmphasis::Default,
            self.shortcut,
            &self.theme,
            area.width,
        );
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let events = self.snapshot_events();
        let content = content_rect_for(self.view, inner);
        match self.view {
            CalendarView::Day => self.render_day(frame, content, &events),
            CalendarView::Week => self.render_week(frame, content, &events, focused),
            CalendarView::Month => self.render_month(frame, content, &events),
        }

        // Footer row: [Today] action + [Day] [Week] [Month] view tabs on the
        // left, dim keyboard hint on the right.
        if inner.height >= 2 {
            let hint_area = Rect {
                x: inner.x,
                y: inner.y + inner.height - 1,
                width: inner.width,
                height: 1,
            };
            let mut spans: Vec<Span<'_>> = vec![Span::raw(" ")];
            // [Today] lights up when the current view already covers
            // today (clicking it would be a no-op); dims when it
            // wouldn't (clicking jumps the anchor). Mirrors the
            // active/inactive treatment of the view tabs to its right
            // so the whole footer reads as a row of state indicators.
            let today_style = if self.current_view_contains_today() {
                self.theme.text_focused
            } else {
                self.theme.text_dim
            };
            spans.push(Span::styled("[Today]", today_style));
            spans.push(Span::raw(" "));
            for (v, label) in VIEW_TABS {
                let active = *v == self.view;
                let style = if active {
                    self.theme.text_selected
                } else {
                    self.theme.text_dim
                };
                spans.push(Span::styled(format!("[{label}]"), style));
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled("  ←/→ nav", self.theme.text_dim));
            frame.render_widget(Paragraph::new(Line::from(spans)), hint_area);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if key.modifiers != KeyModifiers::NONE && key.modifiers != KeyModifiers::SHIFT {
            return EventResult::Ignored;
        }
        // Uppercase ASCII letters are reserved for the app-wide
        // `Shift+<letter>` focus-jump dispatcher — never consume them here.
        if let KeyCode::Char(c) = key.code {
            if c.is_ascii_uppercase() {
                return EventResult::Ignored;
            }
        }
        let step = self.nav_step();
        match key.code {
            KeyCode::Char('d') => {
                self.view = CalendarView::Day;
                self.reset_agenda_scroll();
                self.mark_dirty_if_uncovered();
                EventResult::Handled
            }
            KeyCode::Char('w') => {
                self.view = CalendarView::Week;
                self.reset_agenda_scroll();
                self.mark_dirty_if_uncovered();
                EventResult::Handled
            }
            KeyCode::Char('m') => {
                self.view = CalendarView::Month;
                self.reset_agenda_scroll();
                self.mark_dirty_if_uncovered();
                EventResult::Handled
            }
            KeyCode::Char('t') => {
                self.anchor = Local::now().date_naive();
                self.reset_agenda_scroll();
                self.mark_dirty_if_uncovered();
                EventResult::Handled
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.anchor -= step;
                self.reset_agenda_scroll();
                self.mark_dirty_if_uncovered();
                EventResult::Handled
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.anchor += step;
                self.reset_agenda_scroll();
                self.mark_dirty_if_uncovered();
                EventResult::Handled
            }
            // ↑ / ↓ (and j/k) scroll the day's agenda when it has more
            // events than fit in the body. Time navigation lives on
            // ←/→ + clicks — no overlap.
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll_agenda(-1);
                EventResult::Handled
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll_agenda(1);
                EventResult::Handled
            }
            KeyCode::PageUp => {
                self.scroll_agenda(-10);
                EventResult::Handled
            }
            KeyCode::PageDown => {
                self.scroll_agenda(10);
                EventResult::Handled
            }
            KeyCode::Char('g') => {
                let mut st = self.state.lock().expect("calendar state poisoned");
                st.gradient = st.gradient.next();
                EventResult::Handled
            }
            _ => EventResult::Ignored,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, area: Rect) -> EventResult {
        // Vertical scroll walks the selected day's agenda. Horizontal
        // scroll walks the anchor by the same view-stride ←/→ does,
        // gated through `consume_horizontal_scroll`: axis-locked off
        // recent vertical events (so trackpad jitter during a vertical
        // gesture doesn't accidentally navigate days) and burst-
        // debounced (so a single horizontal flick is one step, not
        // twenty). Click navigation on the day grid stays untouched.
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.last_vertical_scroll = Some(Instant::now());
                match self.view {
                    CalendarView::Week => self.scroll_week_col(mouse.column, area, -1),
                    _ => self.scroll_agenda(-1),
                }
                return EventResult::Handled;
            }
            MouseEventKind::ScrollDown => {
                self.last_vertical_scroll = Some(Instant::now());
                match self.view {
                    CalendarView::Week => self.scroll_week_col(mouse.column, area, 1),
                    _ => self.scroll_agenda(1),
                }
                return EventResult::Handled;
            }
            MouseEventKind::ScrollLeft => {
                if self.consume_horizontal_scroll() {
                    self.anchor -= self.nav_step();
                    self.reset_agenda_scroll();
                    self.mark_dirty_if_uncovered();
                }
                return EventResult::Handled;
            }
            MouseEventKind::ScrollRight => {
                if self.consume_horizontal_scroll() {
                    self.anchor += self.nav_step();
                    self.reset_agenda_scroll();
                    self.mark_dirty_if_uncovered();
                }
                return EventResult::Handled;
            }
            MouseEventKind::Down(MouseButton::Left) => {}
            _ => return EventResult::Ignored,
        }

        if area.width < 2 || area.height < 2 {
            return EventResult::Ignored;
        }
        let inner = Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2);

        // Bottom hint row hosts the [Today] button + [Day][Week][Month] tabs.
        if inner.height >= 1 {
            let hint_y = inner.y + inner.height - 1;
            if mouse.row == hint_y {
                match bottom_action_at(mouse.column, inner.x) {
                    Some(BottomAction::Today) => {
                        self.anchor = Local::now().date_naive();
                        self.mark_dirty_if_uncovered();
                        return EventResult::Handled;
                    }
                    Some(BottomAction::View(v)) => {
                        if self.view != v {
                            self.view = v;
                            self.mark_dirty_if_uncovered();
                        }
                        return EventResult::Handled;
                    }
                    None => return EventResult::Ignored,
                }
            }
        }

        // Day-grid clicks: which date did the user pick? Week view promotes
        // the clicked date to Day view (the events list needs the room).
        // Month view keeps the user in the calendar — it just moves the
        // selection so the agenda below the grid retargets to that day.
        // Day/Month grids are drawn in a 1-col-inset rect (see
        // `content_rect_for`), so hit-test against the same rect.
        let content = content_rect_for(self.view, inner);
        match self.view {
            CalendarView::Week => {
                if let Some(date) = self.week_day_at(mouse.column, mouse.row, content) {
                    self.anchor = date;
                    self.view = CalendarView::Day;
                    self.reset_agenda_scroll();
                    self.mark_dirty_if_uncovered();
                    return EventResult::Handled;
                }
            }
            CalendarView::Month => {
                if let Some(date) = self.month_day_at(mouse.column, mouse.row, content) {
                    self.anchor = date;
                    self.reset_agenda_scroll();
                    self.mark_dirty_if_uncovered();
                    return EventResult::Handled;
                }
            }
            CalendarView::Day => {
                // Two-column day view: clicking the right preview column
                // promotes that day to the new anchor.
                if content.width >= 50 && mouse.column >= content.x + content.width / 2 {
                    self.anchor += ChronoDuration::days(1);
                    self.reset_agenda_scroll();
                    self.mark_dirty_if_uncovered();
                    return EventResult::Handled;
                }
            }
        }
        EventResult::Ignored
    }

    fn handle_command(&mut self, _cmd: &str, _args: &[&str]) -> Result<bool> {
        Ok(false)
    }

    fn keybindings(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("d / w / m", "switch view: day / week / month"),
            ("← / → / h / l", "previous / next (per view)"),
            ("↑ / ↓ / j / k", "scroll the day's agenda"),
            ("PgUp / PgDn", "scroll agenda ±10 lines"),
            ("wheel", "scroll the day's agenda"),
            ("t", "jump to today"),
            ("g", "cycle digit gradient style (today's date)"),
            (
                "click day",
                "week: open in day view; month: select for agenda",
            ),
            ("click tab", "switch view / today"),
        ]
    }

    fn config(&self) -> serde_json::Value {
        serde_json::json!({
            "default_view": self.view,
            "poll_interval_secs": self
                .state
                .lock()
                .expect("calendar state poisoned")
                .poll
                .interval()
                .as_secs(),
            "provider": self.source_label,
        })
    }

    fn apply_config(&mut self, config: serde_json::Value) -> Result<()> {
        let new_config: CalendarConfig =
            serde_json::from_value(config).context("invalid calendar config payload")?;
        let app_theme = self.app_theme.clone();
        let cache = self.cache.clone();
        let instance = self.instance.clone();
        *self = Self::with_config(instance, new_config, app_theme, cache);
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
                .expect("calendar state poisoned")
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
        Some(self.title_metadata_string())
    }
}

pub const KIND: &str = "calendar";

/// Wizard descriptor. Covers the core common knobs (refresh interval +
/// per-provider OAuth handoff); structured data like
/// [[providers]] / [[events]] / `[calendar_colors]` lives in
/// calendar.toml and is preserved across `--setup` re-runs.
pub fn wizard_descriptor() -> crate::wizard::descriptor::WizardDescriptor {
    use crate::wizard::descriptor::{ChoiceOption, WizardDescriptor, WizardField, WizardFieldKind};
    WizardDescriptor {
        display_name: "Calendar",
        blurb: "Day / week / month agenda views across Google, Outlook, \
                CalDAV, and a built-in local provider. Tick the calendars \
                you'd like to pull from; the wizard runs the OAuth \
                handshakes; per-calendar IDs + CalDAV details live in \
                calendar.toml for hand-tuning.",
        load_from_toml: Some(load_calendar_from_toml),
        render_toml: Some(render_calendar_toml),
        fields: vec![
            WizardField {
                key: "sources",
                label: "Calendar sources",
                help: "Each ticked source becomes a [[providers]] block in \
                       calendar.toml. Google + Outlook need their OAuth \
                       handshake (next two fields). CalDAV credentials \
                       live in credentials/caldav.toml. Local needs no \
                       setup — uses [[events]] entries in calendar.toml.",
                required: false,
                kind: WizardFieldKind::MultiChoice {
                    options: vec![
                        ChoiceOption {
                            value: "google",
                            label: "Google Calendar",
                            help: None,
                        },
                        ChoiceOption {
                            value: "outlook",
                            label: "Outlook (Microsoft 365)",
                            help: None,
                        },
                        ChoiceOption {
                            value: "caldav",
                            label: "CalDAV (iCloud, Fastmail, Nextcloud, …)",
                            help: None,
                        },
                        ChoiceOption {
                            value: "local",
                            label: "Local events (defined in calendar.toml)",
                            help: None,
                        },
                    ],
                    defaults: vec!["local"],
                },
                validate: None,
            },
            WizardField {
                key: "poll_interval_secs",
                label: "Refresh interval (seconds)",
                help: "How often the calendar re-fetches events from each \
                       configured provider. 60–300s is usual.",
                required: true,
                kind: WizardFieldKind::Number {
                    default: Some(60.0),
                    range: Some((30.0, 3600.0)),
                    integer: true,
                },
                validate: None,
            },
            WizardField {
                key: "authorize_google",
                label: "Authorize Google Calendar",
                help: "Required if you want calendar.toml to include \
                       a [[providers]] block with kind = \"google\". Opens \
                       a browser to console.cloud.google.com for the OAuth \
                       consent, then captures the token on a loopback port.",
                required: false,
                kind: WizardFieldKind::OAuth { provider: "google" },
                validate: None,
            },
            WizardField {
                key: "authorize_microsoft",
                label: "Authorize Microsoft (Outlook calendar)",
                help: "Required for an Outlook calendar provider. Opens a \
                       browser to login.microsoftonline.com; if you haven't \
                       set up an Azure app yet, see \
                       credentials/microsoft_oauth_client.toml.",
                required: false,
                kind: WizardFieldKind::OAuth {
                    provider: "microsoft",
                },
                validate: None,
            },
        ],
    }
}

fn load_calendar_from_toml(
    doc: &toml::Value,
) -> HashMap<String, crate::wizard::descriptor::WizardValue> {
    use crate::wizard::descriptor::WizardValue;
    let mut out = HashMap::new();
    if let Some(n) = doc.get("poll_interval_secs").and_then(|v| v.as_integer()) {
        out.insert("poll_interval_secs".into(), WizardValue::Number(n as f64));
    }
    // Derive the MultiChoice from existing [[providers]] blocks. We
    // accept the same aliases the runtime deserializer does
    // (apple/icloud → caldav, microsoft/ms365 → outlook) so a
    // hand-edited file round-trips cleanly.
    if let Some(arr) = doc.get("providers").and_then(|v| v.as_array()) {
        let mut sources: Vec<String> = Vec::new();
        for entry in arr {
            if let Some(kind) = entry.get("kind").and_then(|v| v.as_str()) {
                let canonical = match kind {
                    "google" => "google",
                    "outlook" | "microsoft" | "ms365" => "outlook",
                    "caldav" | "apple" | "icloud" => "caldav",
                    "local" => "local",
                    _ => continue,
                };
                if !sources.iter().any(|s| s == canonical) {
                    sources.push(canonical.to_string());
                }
            }
        }
        if !sources.is_empty() {
            out.insert("sources".into(), WizardValue::MultiChoice(sources));
        }
    }
    out
}

fn render_calendar_toml(
    values: &HashMap<String, crate::wizard::descriptor::WizardValue>,
    existing: Option<&str>,
) -> String {
    use crate::wizard::descriptor::WizardValue;

    let scalars: Vec<(&str, String)> = vec![(
        "poll_interval_secs",
        match values.get("poll_interval_secs") {
            Some(WizardValue::Number(n)) => format!("{}", *n as i64),
            _ => "60".into(),
        },
    )];

    // Build [[providers]] blocks. For each selected source, reuse the
    // user's existing block (preserving calendar_ids etc.) when one
    // exists, else emit a minimal default.
    let selected_kinds: Vec<String> = match values.get("sources") {
        Some(WizardValue::MultiChoice(items)) => items.clone(),
        _ => vec!["local".into()],
    };
    let existing_blocks: HashMap<String, String> =
        existing_provider_blocks_by_kind(existing.unwrap_or(""));

    let mut provider_blocks = String::new();
    for kind in &selected_kinds {
        if let Some(block) = existing_blocks.get(kind) {
            provider_blocks.push_str("\n");
            provider_blocks.push_str(block);
        } else {
            provider_blocks.push_str(&format!("\n[[providers]]\nkind = \"{kind}\"\n"));
        }
    }

    let base: std::borrow::Cow<str> = match existing {
        Some(text) => std::borrow::Cow::Borrowed(text),
        None => std::borrow::Cow::Borrowed(crate::config::DEFAULT_CALENDAR_TOML),
    };
    let stripped = crate::wizard::toml_merge::strip_array_of_tables_blocks(&base, "providers");
    let merged = crate::wizard::toml_merge::merge_top_level_scalars(&stripped, &scalars);

    let mut out = merged;
    if !out.ends_with("\n\n") {
        if out.ends_with('\n') {
            out.push('\n');
        } else {
            out.push_str("\n\n");
        }
    }
    out.push_str(provider_blocks.trim_start_matches('\n'));
    out
}

/// Pull each existing `[[providers]]` block out of the text, keyed by
/// canonicalised kind (apple/icloud → caldav, etc.) so a re-render can
/// preserve the user's `calendar_ids` lists when they keep that
/// source ticked.
fn existing_provider_blocks_by_kind(text: &str) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    let Ok(doc) = toml::from_str::<toml::Value>(text) else {
        return out;
    };
    let Some(arr) = doc.get("providers").and_then(|v| v.as_array()) else {
        return out;
    };
    for entry in arr {
        let Some(kind) = entry.get("kind").and_then(|v| v.as_str()) else {
            continue;
        };
        let canonical = match kind {
            "google" => "google",
            "outlook" | "microsoft" | "ms365" => "outlook",
            "caldav" | "apple" | "icloud" => "caldav",
            "local" => "local",
            _ => continue,
        };
        // Re-emit the block from the parsed Value so we don't have to
        // line-scan the source for boundaries. Manual emit keeps the
        // output predictable (kind first, then calendar_ids).
        let mut block = String::from("[[providers]]\n");
        block.push_str(&format!("kind = \"{canonical}\"\n"));
        if let Some(ids) = entry.get("calendar_ids").and_then(|v| v.as_array()) {
            let items: Vec<String> = ids
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| format!("\"{}\"", s.replace('"', "\\\"")))
                .collect();
            if !items.is_empty() {
                block.push_str(&format!("calendar_ids = [{}]\n", items.join(", ")));
            }
        }
        out.insert(canonical.to_string(), block);
    }
    out
}

pub fn build(ctx: &super::WidgetCtx) -> Box<dyn super::Widget> {
    let cfg: CalendarConfig =
        crate::config::load_widget_toml_for_instance(KIND, &ctx.instance).unwrap_or_default();
    Box::new(CalendarWidget::with_config(
        ctx.instance.clone(),
        cfg,
        ctx.theme.clone(),
        ctx.cache.clone(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_widget(cfg: CalendarConfig) -> CalendarWidget {
        CalendarWidget::with_config(
            "main".to_string(),
            cfg,
            Arc::new(Theme::builtin_defaults()),
            ScopedCache::ephemeral(),
        )
    }

    fn mouse_scroll(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// One horizontal-scroll click steps the anchor by the view's
    /// `nav_step` (Day → 1, Week → 7, Month → 30). Cooldown is cleared
    /// between calls so the test exercises the per-event stride, not
    /// the debounce gate.
    #[test]
    fn horizontal_scroll_steps_anchor_by_view_stride() {
        for (view, days) in [
            (CalendarView::Day, 1),
            (CalendarView::Week, 7),
            (CalendarView::Month, 30),
        ] {
            let mut w = build_widget(CalendarConfig::default());
            w.view = view;
            let start = w.anchor;
            let area = Rect::new(0, 0, 40, 20);
            w.handle_mouse(mouse_scroll(MouseEventKind::ScrollRight), area);
            assert_eq!(
                w.anchor,
                start + ChronoDuration::days(days),
                "view {view:?}: ScrollRight should advance by {days} day(s)"
            );
            // Clear the cooldown so the next click isn't dropped by the
            // burst debounce — we're verifying per-event stride here.
            w.last_horizontal_scroll = None;
            w.handle_mouse(mouse_scroll(MouseEventKind::ScrollLeft), area);
            assert_eq!(w.anchor, start, "view {view:?}: ScrollLeft should reverse");
        }
    }

    /// A burst of ScrollRight events arriving within the cooldown
    /// collapses to one navigation step. Without this, a trackpad flick
    /// (20-30 events in ~300ms) jumps 20+ days at once.
    #[test]
    fn horizontal_scroll_burst_within_cooldown_collapses_to_one_step() {
        let mut w = build_widget(CalendarConfig::default());
        w.view = CalendarView::Day;
        let start = w.anchor;
        let area = Rect::new(0, 0, 40, 20);
        for _ in 0..20 {
            w.handle_mouse(mouse_scroll(MouseEventKind::ScrollRight), area);
        }
        assert_eq!(
            w.anchor,
            start + ChronoDuration::days(1),
            "rapid burst within cooldown should advance only once"
        );
    }

    /// macOS trackpads emit micro horizontal-scroll events interspersed
    /// with vertical ones. Without axis-lock, the horizontal jitter would
    /// fire date navigation in the middle of agenda scrolling and undo
    /// every row of vertical motion. After a vertical scroll, any
    /// horizontal scroll within the lock window must be dropped.
    #[test]
    fn vertical_scroll_locks_out_horizontal_jitter() {
        let mut w = build_widget(CalendarConfig::default());
        w.view = CalendarView::Day;
        let start = w.anchor;
        let area = Rect::new(0, 0, 40, 20);
        w.handle_mouse(mouse_scroll(MouseEventKind::ScrollDown), area);
        // Clearing the horizontal cooldown isolates the test: if the
        // horizontal event gets through, it's the axis-lock that broke,
        // not the burst debounce.
        w.last_horizontal_scroll = None;
        w.handle_mouse(mouse_scroll(MouseEventKind::ScrollLeft), area);
        w.handle_mouse(mouse_scroll(MouseEventKind::ScrollRight), area);
        assert_eq!(
            w.anchor, start,
            "horizontal jitter during a vertical gesture must not navigate"
        );
        // Simulate the lock expiring (user paused after vertical) — a
        // deliberate horizontal flick should now navigate.
        w.last_vertical_scroll = None;
        w.handle_mouse(mouse_scroll(MouseEventKind::ScrollRight), area);
        assert_eq!(w.anchor, start + ChronoDuration::days(1));
    }

    /// In Week view, scrolling over a specific day-column drives that
    /// column's offset only — neighbours stay put. Catches a regression
    /// where one shared scroll state would shift every column together
    /// (or where Week view dropped wheel events entirely).
    #[test]
    fn week_view_wheel_scrolls_targeted_column_only() {
        let mut w = build_widget(CalendarConfig::default());
        w.view = CalendarView::Week;
        // 70 cols wide → ~10 cols per day, so column index 0 sits at
        // x ∈ [1, 10] (after the 1-col border inset). Target column 2
        // (Tuesday) at x = 22.
        let area = Rect::new(0, 0, 70, 20);
        // Pre-seed scroll_max so the clamp lets the offset move.
        // Render normally writes this; in the test we set it directly.
        {
            let mut st = w.state.lock().unwrap();
            st.week_col_scroll_max = [10; 7];
        }
        let mut evt = mouse_scroll(MouseEventKind::ScrollDown);
        evt.column = 22;
        w.handle_mouse(evt, area);
        let scrolls = w.state.lock().unwrap().week_col_scroll;
        let nonzero_count = scrolls.iter().filter(|&&v| v > 0).count();
        assert_eq!(
            nonzero_count, 1,
            "exactly one column should scroll; got {scrolls:?}"
        );
        assert!(
            scrolls[2] > 0,
            "the Tuesday column (index 2) should be the one scrolled; got {scrolls:?}"
        );
    }

    /// Vertical scroll never moves the anchor — even in Week view where
    /// the wheel routes through a different helper.
    #[test]
    fn vertical_scroll_does_not_move_anchor() {
        for view in [CalendarView::Day, CalendarView::Week, CalendarView::Month] {
            let mut w = build_widget(CalendarConfig::default());
            w.view = view;
            let start = w.anchor;
            let area = Rect::new(0, 0, 40, 20);
            w.handle_mouse(mouse_scroll(MouseEventKind::ScrollUp), area);
            w.handle_mouse(mouse_scroll(MouseEventKind::ScrollDown), area);
            assert_eq!(
                w.anchor, start,
                "view {view:?}: vertical scroll moved anchor"
            );
        }
    }

    /// Day view: [Today] is lit only when the anchor IS today.
    #[test]
    fn today_button_state_in_day_view_tracks_anchor() {
        let mut w = build_widget(CalendarConfig::default());
        w.view = CalendarView::Day;
        w.anchor = Local::now().date_naive();
        assert!(w.current_view_contains_today());
        w.anchor -= ChronoDuration::days(3);
        assert!(!w.current_view_contains_today());
    }

    /// Week view: today is "in view" when it falls inside the Sun..=Sat
    /// window containing the anchor — not just when the anchor itself
    /// is today. Walking 3 days forward or back from today stays in
    /// the same week (most of the time).
    #[test]
    fn today_button_state_in_week_view_covers_whole_week() {
        let mut w = build_widget(CalendarConfig::default());
        w.view = CalendarView::Week;
        let today = Local::now().date_naive();
        // Anchor on the start of the current week → should still
        // count as "today in view."
        w.anchor = start_of_week(today, Weekday::Sun);
        assert!(w.current_view_contains_today());
        // Jump to the start of a different week — today is no longer
        // inside the anchored Sun..=Sat range.
        w.anchor = start_of_week(today, Weekday::Sun) - ChronoDuration::days(14);
        assert!(!w.current_view_contains_today());
    }

    /// Month view: any day within today's calendar month counts.
    /// Crossing the month boundary flips the state.
    #[test]
    fn today_button_state_in_month_view_covers_whole_month() {
        let mut w = build_widget(CalendarConfig::default());
        w.view = CalendarView::Month;
        let today = Local::now().date_naive();
        // Anchor on the 1st of this month — same month → lit.
        w.anchor = NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap();
        assert!(w.current_view_contains_today());
        // Anchor on the previous month's 15th.
        w.anchor = first_of_next_month(today) + ChronoDuration::days(45);
        assert!(!w.current_view_contains_today());
    }

    #[test]
    fn start_of_week_anchors_on_configured_first_day() {
        // 2026-05-20 is a Wednesday.
        let wed = NaiveDate::from_ymd_opt(2026, 5, 20).unwrap();
        let sun = start_of_week(wed, Weekday::Sun);
        assert_eq!(sun.weekday(), Weekday::Sun);
        assert_eq!(sun, NaiveDate::from_ymd_opt(2026, 5, 17).unwrap());
        // ISO/Europe default — Monday anchors one day later.
        let mon = start_of_week(wed, Weekday::Mon);
        assert_eq!(mon.weekday(), Weekday::Mon);
        assert_eq!(mon, NaiveDate::from_ymd_opt(2026, 5, 18).unwrap());
        // A weekday that's strictly *after* today rolls back through
        // the prior week, not forward — Saturday-start asked on a
        // Wednesday lands on the previous Saturday (5 days back).
        let sat = start_of_week(wed, Weekday::Sat);
        assert_eq!(sat.weekday(), Weekday::Sat);
        assert_eq!(sat, NaiveDate::from_ymd_opt(2026, 5, 16).unwrap());
    }

    #[test]
    fn rotated_weekday_labels_match_first_day() {
        assert_eq!(
            rotated_weekday_labels(Weekday::Sun),
            ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"]
        );
        assert_eq!(
            rotated_weekday_labels(Weekday::Mon),
            ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
        );
        assert_eq!(
            rotated_weekday_labels(Weekday::Sat),
            ["Sat", "Sun", "Mon", "Tue", "Wed", "Thu", "Fri"]
        );
    }

    #[test]
    fn first_of_next_month_wraps_december() {
        let dec = NaiveDate::from_ymd_opt(2026, 12, 15).unwrap();
        let jan = first_of_next_month(dec);
        assert_eq!(jan, NaiveDate::from_ymd_opt(2027, 1, 1).unwrap());
    }

    #[test]
    fn color_resolver_is_stable_and_disambiguates_sources() {
        let cfg = CalendarConfig {
            providers: vec![
                ProviderEntry {
                    kind: ProviderKind::Google,
                    calendar_ids: vec!["primary".into()],
                },
                ProviderEntry {
                    kind: ProviderKind::Outlook,
                    calendar_ids: vec!["primary".into()],
                },
            ],
            ..Default::default()
        };
        let c = CalendarColors::build(&cfg);
        let g = c.resolve("google", "primary");
        let o = c.resolve("outlook", "primary");
        assert_ne!(g, o, "same calendar id under different sources must differ");
        assert_eq!(g, c.resolve("google", "primary"), "must be deterministic");
    }

    #[test]
    fn explicit_calendar_color_overrides_sequence() {
        let mut overrides = HashMap::new();
        overrides.insert("google:primary".to_string(), "red".to_string());
        let cfg = CalendarConfig {
            providers: vec![ProviderEntry {
                kind: ProviderKind::Google,
                calendar_ids: vec!["primary".into()],
            }],
            calendar_colors: overrides,
            ..Default::default()
        };
        let c = CalendarColors::build(&cfg);
        assert_eq!(c.resolve("google", "primary"), Color::Red);
    }

    #[test]
    fn custom_palette_replaces_default_sequence() {
        let cfg = CalendarConfig {
            providers: vec![ProviderEntry {
                kind: ProviderKind::Google,
                calendar_ids: vec!["a".into(), "b".into()],
            }],
            color_palette: vec!["red".into(), "green".into()],
            ..Default::default()
        };
        let c = CalendarColors::build(&cfg);
        assert_eq!(c.resolve("google", "a"), Color::Red);
        assert_eq!(c.resolve("google", "b"), Color::Green);
    }

    #[test]
    fn parse_color_accepts_common_names_and_hex() {
        assert_eq!(parse_color("red"), Some(Color::Red));
        assert_eq!(parse_color("Light-Blue"), Some(Color::LightBlue));
        assert_eq!(parse_color("BRIGHT_GREEN"), Some(Color::LightGreen));
        // Theme parser distinguishes "gray" (bright) from "dark_gray"
        // (the darker variant) — the calendar parser used to fold both
        // into DarkGray; the shared parser treats them as separate
        // ANSI slots, matching ratatui's enum.
        assert_eq!(parse_color(" gray "), Some(Color::Gray));
        assert_eq!(parse_color("dark_gray"), Some(Color::DarkGray));
        assert_eq!(parse_color("#ff6480"), Some(Color::Rgb(0xff, 0x64, 0x80)));
        assert_eq!(parse_color("#4097E4"), Some(Color::Rgb(0x40, 0x97, 0xe4)));
        assert_eq!(parse_color("nope"), None);
    }

    #[test]
    fn default_view_is_day_and_widget_starts_today() {
        let w = build_widget(CalendarConfig::default());
        assert_eq!(w.view, CalendarView::Day);
        assert_eq!(w.anchor, Local::now().date_naive());
    }

    #[test]
    fn bottom_action_at_maps_cols_to_actions() {
        // Bottom row renders: " [Today] [Day] [Week] [Month]"
        //                       1     7 9   13 15   20 22
        assert_eq!(bottom_action_at(2, 0), Some(BottomAction::Today));
        assert_eq!(bottom_action_at(7, 0), Some(BottomAction::Today)); // ']' position
        assert_eq!(
            bottom_action_at(10, 0),
            Some(BottomAction::View(CalendarView::Day))
        );
        assert_eq!(
            bottom_action_at(16, 0),
            Some(BottomAction::View(CalendarView::Week))
        );
        assert_eq!(
            bottom_action_at(23, 0),
            Some(BottomAction::View(CalendarView::Month))
        );
        assert_eq!(bottom_action_at(60, 0), None);
    }

    #[test]
    fn week_day_at_maps_columns_to_dates() {
        // Anchor on a Wednesday; weeks start Sunday.
        let cfg = CalendarConfig {
            default_view: CalendarView::Week,
            ..CalendarConfig::default()
        };
        let mut w = build_widget(cfg);
        w.anchor = NaiveDate::from_ymd_opt(2026, 5, 20).unwrap();
        let inner = Rect::new(0, 0, 70, 20);
        // 70 wide → each of the 7 cols ≈ 10. Click in col 0 (x=2) → Sunday.
        assert_eq!(
            w.week_day_at(2, 1, inner),
            Some(NaiveDate::from_ymd_opt(2026, 5, 17).unwrap())
        );
        // Click in column for Wednesday (col 3, x≈30+).
        assert_eq!(
            w.week_day_at(32, 5, inner),
            Some(NaiveDate::from_ymd_opt(2026, 5, 20).unwrap())
        );
        // Click in the hint row → None.
        assert_eq!(w.week_day_at(2, 19, inner), None);
    }

    #[test]
    fn month_day_at_maps_grid_cells_to_dates() {
        let cfg = CalendarConfig {
            default_view: CalendarView::Month,
            ..CalendarConfig::default()
        };
        let mut w = build_widget(cfg);
        w.anchor = NaiveDate::from_ymd_opt(2026, 5, 20).unwrap();
        // 40-wide column → 35-char grid centered → 2 cols leading padding,
        // so cell 0 starts at col 2, cell 6 starts at col 32.
        let inner = Rect::new(0, 0, 40, 20);
        // Rows: padding=0, month name=1, weekday header=2, weeks start at 3.
        // May 2026 starts Friday → first grid row is Sun Apr 26 … Sat May 2.
        let apr26 = NaiveDate::from_ymd_opt(2026, 4, 26).unwrap();
        assert_eq!(w.month_day_at(3, 3, inner), Some(apr26));
        let may2 = NaiveDate::from_ymd_opt(2026, 5, 2).unwrap();
        assert_eq!(w.month_day_at(33, 3, inner), Some(may2));
        // Clicks in padding / month-name / weekday-header rows → None.
        assert_eq!(w.month_day_at(3, 0, inner), None);
        assert_eq!(w.month_day_at(3, 1, inner), None);
        assert_eq!(w.month_day_at(3, 2, inner), None);
        // Beyond the 7th column of the grid → None.
        assert_eq!(w.month_day_at(38, 3, inner), None);
    }

    #[test]
    fn advance_month_wraps_year_boundaries() {
        assert_eq!(advance_month(2026, 12, 1), (2027, 1));
        assert_eq!(advance_month(2026, 1, -1), (2025, 12));
        assert_eq!(advance_month(2026, 5, 0), (2026, 5));
        assert_eq!(advance_month(2026, 5, 7), (2026, 12));
        assert_eq!(advance_month(2026, 5, 8), (2027, 1));
    }

    #[test]
    fn wrap_event_title_caps_lines_with_ellipsis() {
        let lines = wrap_event_title("the quick brown fox jumps over the lazy dog", 7, 3);
        assert_eq!(lines.len(), 3);
        assert!(lines.last().unwrap().ends_with('…'));
    }

    #[test]
    fn wrap_event_title_fills_to_column_width() {
        // Char-level wrap: every line except the last (or one short of
        // the truncation point) should hit max_width exactly, so we
        // use every available column instead of leaving trailing
        // whitespace from a word-boundary-only wrap.
        let lines = wrap_event_title("Project planning meeting with vendor", 10, 4);
        for line in lines.iter().take(lines.len() - 1) {
            assert_eq!(
                line.chars().count(),
                10,
                "non-final line should fill the column: {line:?}"
            );
        }
    }

    #[test]
    fn wrap_event_title_splits_oversized_word_across_lines() {
        // A single 20-char word at column width 5: should occupy 4
        // lines of 5 chars each — no characters dropped, no
        // mid-string ellipsis.
        let lines = wrap_event_title("supercalifragilistic", 5, 4);
        assert_eq!(lines, vec!["super", "calif", "ragil", "istic"]);
    }

    #[test]
    fn wrap_event_title_ellipsises_truncated_oversized_word() {
        // Same word, only 3 lines available: the first 15 chars land
        // intact across lines 1+2, the last line keeps 4 chars + the
        // ellipsis (replacing the would-be 5th char).
        let lines = wrap_event_title("supercalifragilistic", 5, 3);
        assert_eq!(lines.len(), 3);
        assert!(lines[2].ends_with('…'));
        assert_eq!(lines[2].chars().count(), 5);
    }

    #[test]
    fn wrap_event_title_skips_leading_space_on_continuation() {
        // When the break lands right before a space, the continuation
        // line shouldn't begin with that space — the user would see
        // an awkward indent.
        let lines = wrap_event_title("Hello World", 5, 3);
        assert_eq!(lines[0], "Hello");
        assert_eq!(lines[1], "World");
    }

    fn make_event(
        start: chrono::DateTime<Local>,
        end: chrono::DateTime<Local>,
        title: &str,
    ) -> Event {
        Event {
            title: title.into(),
            start,
            end,
            all_day: false,
            source: "local".into(),
            calendar: "test".into(),
            location: None,
        }
    }

    #[test]
    fn first_future_event_line_skips_past_events() {
        let w = build_widget(CalendarConfig::default());
        let now = chrono::Local
            .with_ymd_and_hms(2026, 5, 21, 14, 0, 0)
            .unwrap();
        let one_hour = chrono::Duration::hours(1);
        // Three events: 09–10 (past), 12–13 (past), 15–16 (future).
        let events: Vec<Event> = vec![
            make_event(
                now - chrono::Duration::hours(5),
                now - chrono::Duration::hours(4),
                "morning standup",
            ),
            make_event(
                now - chrono::Duration::hours(2),
                now - chrono::Duration::hours(1),
                "lunch chat",
            ),
            make_event(now + one_hour, now + one_hour * 2, "design review"),
        ];
        let refs: Vec<&Event> = events.iter().collect();
        // Each event with no location and a short title takes exactly 1 line.
        // So the third event lands at line 2.
        let line = w.first_future_event_line(&refs, 60, now);
        assert_eq!(line, Some(2));
    }

    #[test]
    fn first_future_event_line_returns_none_when_all_events_past() {
        let w = build_widget(CalendarConfig::default());
        let now = chrono::Local
            .with_ymd_and_hms(2026, 5, 21, 23, 0, 0)
            .unwrap();
        let events = vec![make_event(
            now - chrono::Duration::hours(10),
            now - chrono::Duration::hours(9),
            "long-finished meeting",
        )];
        let refs: Vec<&Event> = events.iter().collect();
        assert_eq!(w.first_future_event_line(&refs, 60, now), None);
    }

    #[test]
    fn first_future_event_line_includes_in_progress_event() {
        let w = build_widget(CalendarConfig::default());
        let now = chrono::Local
            .with_ymd_and_hms(2026, 5, 21, 14, 30, 0)
            .unwrap();
        // Event is 14:00–15:00 — currently in progress; should qualify.
        let events = vec![make_event(
            now - chrono::Duration::minutes(30),
            now + chrono::Duration::minutes(30),
            "in-progress sync",
        )];
        let refs: Vec<&Event> = events.iter().collect();
        assert_eq!(w.first_future_event_line(&refs, 60, now), Some(0));
    }
}
