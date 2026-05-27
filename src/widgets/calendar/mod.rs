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
use crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
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
use crate::theme::{ColorScheme, Theme};
use crate::ui::{big_digits, decorated_title_line};

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
        }
    }
}

#[derive(Default)]
struct CalendarState {
    events: Vec<Event>,
    last_error: Option<String>,
    last_attempt: Option<Instant>,
    inflight: bool,
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
    poll_interval: Duration,
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
}

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
            ..CalendarState::default()
        };
        // Seed events from cache so the first frame shows last session's
        // timeline while the provider refresh runs in the background.
        if let Some(entry) = cache.load::<Vec<Event>>(CACHE_KEY_EVENTS) {
            let age = entry.age().min(poll_interval);
            state.events = entry.value;
            state.last_attempt = Some(Instant::now() - age);
        }
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
            poll_interval,
            app_theme,
            colors_override,
            theme,
            shortcut: None,
            shortcut_prefs,
            cache,
        }
    }

    fn is_due(&self) -> bool {
        let st = self.state.lock().expect("calendar state poisoned");
        if st.inflight {
            return false;
        }
        match st.last_attempt {
            None => true,
            Some(t) => t.elapsed() >= self.poll_interval,
        }
    }

    fn current_range(&self) -> (DateTime<Local>, DateTime<Local>) {
        let (start, end) = match self.view {
            // Fetch two days when in Day view: the wide layout previews the
            // *next* day next to the anchor, and we don't want it to render
            // "No events" just because the next day's events weren't fetched.
            CalendarView::Day => (self.anchor, self.anchor + ChronoDuration::days(2)),
            CalendarView::Week => {
                let s = start_of_week(self.anchor);
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
        let (start, end) = self.current_range();
        {
            let mut st = self.state.lock().expect("calendar state poisoned");
            st.inflight = true;
            st.last_attempt = Some(Instant::now());
        }
        let provider = self.provider.clone();
        let state = self.state.clone();
        let cache = self.cache.clone();
        tokio::spawn(async move {
            let result = provider.fetch_range(start, end).await;
            let mut st = state.lock().expect("calendar state poisoned");
            st.inflight = false;
            match result {
                Ok(events) => {
                    if let Err(err) = cache.store(CACHE_KEY_EVENTS, &events) {
                        tracing::warn!(error = %err, "calendar cache store failed");
                    }
                    st.events = events;
                    st.last_error = None;
                }
                Err(err) => {
                    tracing::warn!(error = %err, "calendar fetch failed");
                    st.last_error = Some(err.to_string());
                }
            }
        });
    }

    /// Force a refresh on the next tick by clearing the last_attempt clock.
    fn mark_dirty(&self) {
        let mut st = self.state.lock().expect("calendar state poisoned");
        st.last_attempt = None;
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
        Some(start_of_week(self.anchor) + ChronoDuration::days(dow))
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
        let grid_start = start_of_week(first);
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
/// would compress the day cells, so it stays flush. Both `render` and
/// `handle_mouse` route through this helper so click→date mapping aligns
/// with the rendered grid.
fn content_rect_for(view: CalendarView, inner: Rect) -> Rect {
    match view {
        CalendarView::Day | CalendarView::Month if inner.width >= 4 => Rect {
            x: inner.x + 1,
            y: inner.y,
            width: inner.width - 2,
            height: inner.height,
        },
        _ => inner,
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
fn build_provider(
    config: &CalendarConfig,
) -> (Arc<dyn CalendarProvider>, String, Option<String>) {
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
            let p = LocalCalendarProvider::from_file(file)
                .map_err(|e| format!("local events: {e}"))?;
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
    let client = MicrosoftClientConfig::load()
        .map_err(|err| {
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
            return Err(
                "Fill in ~/.config/glint/credentials/caldav.toml to connect CalDAV".into(),
            );
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
        let futs = self
            .inner
            .iter()
            .map(|p| p.fetch_range(start, end));
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
) {
    let Some(first) = NaiveDate::from_ymd_opt(year, month, 1) else {
        return;
    };
    let grid_start = start_of_week(first);
    let today = Local::now().date_naive();

    let month_header_style = if is_anchor {
        theme.text_selected
    } else {
        theme.text_dim
    };
    let weekday_header = Line::from(
        ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"]
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

    frame.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center),
        area,
    );
}

fn local_midnight(date: NaiveDate) -> Option<DateTime<Local>> {
    Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0)?)
        .single()
}

fn start_of_week(d: NaiveDate) -> NaiveDate {
    let from_sunday = d.weekday().num_days_from_sunday();
    d - ChronoDuration::days(i64::from(from_sunday))
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
        for b in source.bytes().chain(b":".iter().copied()).chain(calendar.bytes()) {
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
fn parse_color(s: &str) -> Option<Color> {
    let norm = s.trim().to_ascii_lowercase().replace('-', "_");
    Some(match norm.as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" | "purple" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::White,
        "gray" | "grey" | "dark_gray" | "dark_grey" => Color::DarkGray,
        "light_red" | "bright_red" => Color::LightRed,
        "light_green" | "bright_green" => Color::LightGreen,
        "light_yellow" | "bright_yellow" => Color::LightYellow,
        "light_blue" | "bright_blue" => Color::LightBlue,
        "light_magenta" | "bright_magenta" | "light_purple" => Color::LightMagenta,
        "light_cyan" | "bright_cyan" => Color::LightCyan,
        _ => return None,
    })
}

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
    fn title_for_header(&self) -> String {
        let source = self.source_label.as_str();
        let base = if self.instance == "main" {
            "Calendar".to_string()
        } else {
            format!("Calendar ({})", self.instance)
        };
        match self.view {
            CalendarView::Day => format!(
                "{base} [{source}] — {} {} {}, {}",
                weekday_short(self.anchor.weekday()),
                month_long(self.anchor.month()),
                self.anchor.day(),
                self.anchor.year()
            ),
            CalendarView::Week => {
                let s = start_of_week(self.anchor);
                let e = s + ChronoDuration::days(6);
                format!(
                    "{base} [{source}] — week of {} {}–{}",
                    month_long(s.month()),
                    s.day(),
                    e.day()
                )
            }
            CalendarView::Month => format!(
                "{base} [{source}] — {} {}",
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
                    .map(|_| {
                        Line::from(Span::styled(
                            "│",
                            self.theme.text_dim,
                        ))
                    })
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
            Line::from(Span::styled(
                header_text,
                self.theme.text_dim,
            )),
        ];
        // For today's date we hand the big-digit numeral to `render_styled`
        // so the user's gradient choice applies. Anchor and preview days keep
        // their dim single-color render — putting a vibrant gradient on a
        // non-today date would defeat the visual hierarchy.
        if is_today {
            let gradient = self
                .state
                .lock()
                .expect("calendar state poisoned")
                .gradient;
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
            let do_autoscroll = needs_autoscroll
                && date == today
                && max_scroll > 0;
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
            return vec![Line::from(Span::styled(
                "No events.",
                self.theme.text_dim,
            ))];
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
                    lines.push(Line::from(vec![
                        Span::raw(cont_indent.clone()),
                        title_span,
                    ]));
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
        let day_events: Vec<&Event> =
            events.iter().filter(|e| e.on_date(self.anchor)).collect();
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
        let today_naive = today;
        let do_autoscroll =
            needs_autoscroll && self.anchor == today_naive && max_scroll > 0;
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

    fn render_week(&self, frame: &mut Frame, area: Rect, events: &[Event]) {
        let s = start_of_week(self.anchor);
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

        // Draw vertical separators between day columns. Skip the bottom hint
        // row so the separator doesn't collide with the view-tab buttons.
        let separator_height = area.height.saturating_sub(1);
        for i in 0..6 {
            let sep_area = cols[i * 2 + 1];
            let sep_lines: Vec<Line<'_>> = (0..separator_height)
                .map(|_| {
                    Line::from(Span::styled(
                        "│",
                        self.theme.text_dim,
                    ))
                })
                .collect();
            frame.render_widget(Paragraph::new(sep_lines), sep_area);
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
            let mut lines: Vec<Line<'_>> = vec![
                Line::from(""),
                Line::from(Span::styled(weekday_label, header_style)),
                Line::from(Span::styled(date_label, header_style)),
                Line::from(""),
            ];
            let day_events: Vec<&Event> = events.iter().filter(|e| e.on_date(day)).collect();
            if day_events.is_empty() {
                lines.push(Line::from(Span::styled(
                    "·",
                    self.theme.text_dim,
                )));
            } else {
                let wrap_width = col_area.width.saturating_sub(1) as usize;
                for e in day_events {
                    let color = self.colors.resolve(&e.source, &e.calendar);
                    let prefix = if e.all_day {
                        "•".to_string()
                    } else {
                        format!("{:02}:{:02}", e.start.hour(), e.start.minute())
                    };
                    // First line is "<prefix> <first wrapped chunk>".
                    let title_lines = wrap_event_title(&e.title, wrap_width, 3);
                    let (first, rest) = title_lines.split_first().map(|(f, r)| (f.clone(), r)).unwrap_or((String::new(), &[][..]));
                    lines.push(Line::from(Span::styled(
                        format!("{prefix} {first}"),
                        Style::default().fg(color),
                    )));
                    for cont in rest {
                        lines.push(Line::from(Span::styled(
                            cont.clone(),
                            Style::default().fg(color),
                        )));
                    }
                }
            }
            frame.render_widget(
                Paragraph::new(lines).alignment(Alignment::Left),
                col_area,
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
            height: if show_agenda { GRID_HEIGHT } else { area.height },
        };

        let constraints: Vec<Constraint> =
            (0..months.len()).map(|_| Constraint::Ratio(1, months.len() as u32)).collect();
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
/// fills each line up to `max_width` columns, returning at most `max_lines`
/// lines. If the title doesn't fit, the last line gets an ellipsis. Oversized
/// single words are character-truncated.
fn wrap_event_title(text: &str, max_width: usize, max_lines: usize) -> Vec<String> {
    if max_width == 0 || max_lines == 0 {
        return Vec::new();
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut consumed = 0usize;
    for (i, word) in words.iter().enumerate() {
        let needed = if current.is_empty() {
            word.chars().count()
        } else {
            current.chars().count() + 1 + word.chars().count()
        };
        if needed <= max_width {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
            consumed = i + 1;
        } else if current.is_empty() {
            // word longer than the column — character-truncate with ellipsis.
            let t: String = word.chars().take(max_width.saturating_sub(1)).collect();
            lines.push(format!("{t}…"));
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
            if last.chars().count() < max_width {
                last.push('…');
            } else if !last.ends_with('…') {
                let mut chars: Vec<char> = last.chars().collect();
                chars.pop();
                chars.push('…');
                *last = chars.into_iter().collect();
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

    fn render(&self, frame: &mut Frame, area: Rect, focused: bool) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(self.theme.border_style(focused))
            .title(decorated_title_line(
                focused,
                &self.title_for_header(),
                self.shortcut,
                self.theme.widget_title,
                self.theme.text_shortcut,
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let events = self.snapshot_events();
        let content = content_rect_for(self.view, inner);
        match self.view {
            CalendarView::Day => self.render_day(frame, content, &events),
            CalendarView::Week => self.render_week(frame, content, &events),
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
            spans.push(Span::styled("[Today]", self.theme.text_focused));
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
        let step = match self.view {
            CalendarView::Day => ChronoDuration::days(1),
            CalendarView::Week => ChronoDuration::days(7),
            CalendarView::Month => ChronoDuration::days(30),
        };
        match key.code {
            KeyCode::Char('d') => {
                self.view = CalendarView::Day;
                self.reset_agenda_scroll();
                self.mark_dirty();
                EventResult::Handled
            }
            KeyCode::Char('w') => {
                self.view = CalendarView::Week;
                self.reset_agenda_scroll();
                self.mark_dirty();
                EventResult::Handled
            }
            KeyCode::Char('m') => {
                self.view = CalendarView::Month;
                self.reset_agenda_scroll();
                self.mark_dirty();
                EventResult::Handled
            }
            KeyCode::Char('t') => {
                self.anchor = Local::now().date_naive();
                self.reset_agenda_scroll();
                self.mark_dirty();
                EventResult::Handled
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.anchor -= step;
                self.reset_agenda_scroll();
                self.mark_dirty();
                EventResult::Handled
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.anchor += step;
                self.reset_agenda_scroll();
                self.mark_dirty();
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
        // Scroll wheel: walks the selected day's agenda. Time navigation
        // lives on clicks (the day grid in Week/Month) and on ←/→ keys.
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_agenda(-1);
                return EventResult::Handled;
            }
            MouseEventKind::ScrollDown => {
                self.scroll_agenda(1);
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
                        self.mark_dirty();
                        return EventResult::Handled;
                    }
                    Some(BottomAction::View(v)) => {
                        if self.view != v {
                            self.view = v;
                            self.mark_dirty();
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
                    self.mark_dirty();
                    return EventResult::Handled;
                }
            }
            CalendarView::Month => {
                if let Some(date) = self.month_day_at(mouse.column, mouse.row, content) {
                    self.anchor = date;
                    self.reset_agenda_scroll();
                    self.mark_dirty();
                    return EventResult::Handled;
                }
            }
            CalendarView::Day => {
                // Two-column day view: clicking the right preview column
                // promotes that day to the new anchor.
                if content.width >= 50 && mouse.column >= content.x + content.width / 2 {
                    self.anchor += ChronoDuration::days(1);
                    self.reset_agenda_scroll();
                    self.mark_dirty();
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
            ("click day", "week: open in day view; month: select for agenda"),
            ("click tab", "switch view / today"),
        ]
    }

    fn config(&self) -> serde_json::Value {
        serde_json::json!({
            "default_view": self.view,
            "poll_interval_secs": self.poll_interval.as_secs(),
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

    fn shortcut_preferences(&self) -> &[char] {
        &self.shortcut_prefs
    }

    fn set_shortcut(&mut self, shortcut: Option<char>) {
        self.shortcut = shortcut;
    }
}

pub const KIND: &str = "calendar";

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

    #[test]
    fn start_of_week_lands_on_sunday() {
        // 2026-05-20 is a Wednesday.
        let wed = NaiveDate::from_ymd_opt(2026, 5, 20).unwrap();
        let sun = start_of_week(wed);
        assert_eq!(sun.weekday(), Weekday::Sun);
        assert_eq!(sun, NaiveDate::from_ymd_opt(2026, 5, 17).unwrap());
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
    fn parse_color_accepts_common_names() {
        assert_eq!(parse_color("red"), Some(Color::Red));
        assert_eq!(parse_color("Light-Blue"), Some(Color::LightBlue));
        assert_eq!(parse_color("BRIGHT_GREEN"), Some(Color::LightGreen));
        assert_eq!(parse_color(" gray "), Some(Color::DarkGray));
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
        assert_eq!(bottom_action_at(10, 0), Some(BottomAction::View(CalendarView::Day)));
        assert_eq!(bottom_action_at(16, 0), Some(BottomAction::View(CalendarView::Week)));
        assert_eq!(bottom_action_at(23, 0), Some(BottomAction::View(CalendarView::Month)));
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
    fn wrap_event_title_truncates_oversized_word() {
        let lines = wrap_event_title("supercalifragilistic", 5, 3);
        assert!(lines[0].ends_with('…'));
        assert!(lines[0].chars().count() <= 5);
    }

    fn make_event(start: chrono::DateTime<Local>, end: chrono::DateTime<Local>, title: &str) -> Event {
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
            make_event(now - chrono::Duration::hours(5), now - chrono::Duration::hours(4), "morning standup"),
            make_event(now - chrono::Duration::hours(2), now - chrono::Duration::hours(1), "lunch chat"),
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
