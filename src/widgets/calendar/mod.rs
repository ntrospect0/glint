// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

pub mod caldav;
mod colors;
mod config;
pub mod google;
pub mod local;
mod nav;
pub mod outlook;
pub mod provider;
mod state;
mod wiring;

#[allow(unused_imports)]
pub use config::{
    wizard_descriptor, CalDavConfig, CalendarConfig, CalendarView, FirstDayOfWeek, ProviderEntry,
    ProviderKind, KIND,
};
use colors::CalendarColors;
use config::VIEW_TABS;
use nav::{
    advance_month, bottom_action_at, content_rect_for, google_calendar_url, month_long,
    outlook_calendar_url, rotated_weekday_labels, start_of_week, weekday_short, BottomAction,
    WebTarget,
};
use state::{CalendarState, CACHE_KEY_EVENTS};
use wiring::build_provider;

use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{Datelike, Duration as ChronoDuration, Local, NaiveDate, Timelike, Weekday};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};
use super::{view_tier::ViewTier, AppContext, EventResult, Widget};

use provider::{CalendarProvider, Event};

use crate::cache::ScopedCache;
use crate::theme::{ColorScheme, Theme};
use crate::ui::{apply_title_row, big_digits, MetadataEmphasis};

/// TTL for transient title-bar status messages (e.g. open-failed
/// reasons, "no web-viewable calendar configured" notices).
const STATUS_TTL: Duration = Duration::from_millis(2500);

/// How long a *focused* calendar must sit without key/mouse activity
/// before the day-rollover auto-advance is allowed to fire. Keeps the
/// view from jumping out from under someone actively reading or
/// navigating it as midnight passes. An unfocused calendar rolls
/// immediately — see `maybe_auto_roll`.
pub(super) const AUTO_ROLL_FOCUSED_IDLE: Duration = Duration::from_secs(5 * 60);


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
    /// Provider kinds configured at construction time — used by `o`
    /// to derive the list of web-viewable open targets. Stored
    /// separately from `provider` (which composes the runtime fetch
    /// stack) so the `o` handler doesn't have to inspect the live
    /// provider tree to know whether Google / Outlook were enabled.
    configured_provider_kinds: Vec<ProviderKind>,
    /// Atomic gate over the per-tick status-TTL drain. `true` whenever
    /// a `TimedFeedback` is set; flips back to false the next time
    /// `update()` finds the slot drained. Lets idle ticks skip the
    /// state lock entirely. See the same field on `StocksWidget`.
    feedback_pending: AtomicBool,
    /// Local date the anchor was last positioned as-of — set on
    /// construction, advanced by the auto-roll, and resynced whenever
    /// the user repositions the anchor. `maybe_auto_roll` compares it
    /// against the real local date: once a day has passed unattended it
    /// snaps the (now stale) view home to today.
    rollover_date: NaiveDate,
    /// Instant of the last key/mouse interaction with this widget. The
    /// auto-roll uses it to hold off while the calendar is focused and
    /// the user is active, only advancing after [`AUTO_ROLL_FOCUSED_IDLE`]
    /// of quiet.
    last_activity: Instant,
    /// Mirror of the most recent `render(focused)` flag — render is the
    /// only place the widget learns whether it's focused, and focus only
    /// changes on redraw-forcing events, so the last-rendered value is
    /// still current at tick time. Read by `maybe_auto_roll` to pick the
    /// immediate (unfocused) vs. idle-gated (focused) path.
    is_focused: AtomicBool,
    /// Set during `render` from `ViewTier::from_rect(area) == ViewTier::Full`.
    /// Key and tab handlers (which never receive `area`) read this to know
    /// whether to suppress Month-view selection at the Full tier.
    last_full: AtomicBool,
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
            poll: crate::polling::PollTracker::new(poll_interval),
            ..CalendarState::default()
        };
        // Seed events from cache so the first frame shows last session's
        // timeline while the provider refresh runs in the background.
        if let Some(entry) = cache.load::<Vec<Event>>(CACHE_KEY_EVENTS) {
            state.poll.seed_from_cache_age(entry.age());
            state.events = entry.value.into_iter().map(Arc::new).collect();
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
        let today = Local::now().date_naive();
        Self {
            id,
            instance,
            display_name_cache,
            view: config.default_view,
            anchor: today,
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
            configured_provider_kinds: config.providers.iter().map(|p| p.kind).collect(),
            feedback_pending: AtomicBool::new(false),
            rollover_date: today,
            last_activity: Instant::now(),
            is_focused: AtomicBool::new(false),
            last_full: AtomicBool::new(false),
        }
    }


    /// Web-viewable open targets derived from the configured
    /// `[[providers]]`. Google and Outlook entries map to their
    /// canonical web calendars; CalDAV and Local entries have no
    /// canonical browser surface and are silently skipped. Duplicate
    /// URLs (e.g. two Google entries for different calendar IDs)
    /// collapse to a single target so the picker doesn't list the
    /// same URL twice.
    fn web_targets(&self) -> Vec<WebTarget> {
        let mut out: Vec<WebTarget> = Vec::new();
        let view = self.view;
        let date = self.anchor;
        for kind in &self.configured_provider_kinds {
            let target = match kind {
                ProviderKind::Google => Some(WebTarget {
                    label: "Google Calendar",
                    url: google_calendar_url(view, date),
                }),
                // Microsoft 365 surface — covers the majority of OAuth-
                // Microsoft callers. Consumer Outlook.com users get
                // redirected from this URL after signing in, so the
                // single default works for both. View is deep-linked;
                // date is left implicit (the provider lands on today).
                ProviderKind::Outlook => Some(WebTarget {
                    label: "Outlook Calendar",
                    url: outlook_calendar_url(view).to_string(),
                }),
                // CalDAV servers (iCloud, FastMail, Nextcloud, …) each
                // have their own web UI with no canonical URL we can
                // derive from the credentials.
                ProviderKind::Caldav => None,
                // Local-only calendars have no web surface.
                ProviderKind::Local => None,
            };
            if let Some(target) = target {
                if !out.iter().any(|t| t.url == target.url) {
                    out.push(target);
                }
            }
        }
        out
    }

    /// `o` — open one of the configured web calendars in the browser.
    /// Behavior depends on how many web-viewable providers are
    /// configured:
    /// * 0 → status toast `"No web-viewable calendar configured"`.
    /// * 1 → open directly via `open::that`.
    /// * 2+ → store the targets in `state.open_picker`; render shows
    ///   a numbered modal, and `handle_key` consumes the next 1-N
    ///   keypress to open the chosen URL.
    fn jump_to_external(&mut self) {
        let targets = self.web_targets();
        match targets.len() {
            0 => {
                self.set_status("No web-viewable calendar configured");
            }
            1 => {
                let url = targets[0].url.clone();
                if let Err(err) = open::that(&url) {
                    tracing::warn!(error = %err, url = %url, "calendar: failed to open URL");
                    self.set_status(format!("Failed to open browser: {err}"));
                }
            }
            _ => {
                {
                    let mut st = self.state.lock().expect("calendar state poisoned");
                    st.open_picker = Some(targets);
                    st.dirty = true;
                }
            }
        }
    }

    /// Resolve a picker keypress. Returns `true` if the press
    /// consumed the picker (selection made or cancelled), `false` if
    /// the picker isn't open. Called at the top of `handle_key` so
    /// the picker takes priority over normal calendar bindings while
    /// open.
    fn handle_open_picker_key(&mut self, key: KeyEvent) -> bool {
        // Snapshot targets without holding the lock across `open::that`.
        let targets = {
            let st = self.state.lock().expect("calendar state poisoned");
            st.open_picker.clone()
        };
        let Some(targets) = targets else {
            return false;
        };
        let dismiss = |this: &Self| {
            let mut st = this.state.lock().expect("calendar state poisoned");
            st.open_picker = None;
            st.dirty = true;
        };
        match key.code {
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let idx = c.to_digit(10).unwrap() as usize;
                if idx >= 1 && idx <= targets.len() {
                    let url = targets[idx - 1].url.clone();
                    dismiss(self);
                    if let Err(err) = open::that(&url) {
                        tracing::warn!(error = %err, url = %url, "calendar: failed to open URL");
                        self.set_status(format!("Failed to open browser: {err}"));
                    }
                } else {
                    // Out-of-range digit — cancel the picker.
                    dismiss(self);
                }
            }
            // Esc / q cancel; any other key also dismisses without action
            // (matches the ConfirmModal "any other key cancels" convention).
            _ => dismiss(self),
        }
        true
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
    fn month_day_at(
        &self,
        col: u16,
        row: u16,
        inner: Rect,
        opts: MiniMonthOpts,
    ) -> Option<NaiveDate> {
        let usable_height = inner.height.saturating_sub(1);
        let rel_y = row.checked_sub(inner.y)?;
        // Header rows before the first week: without rules it's pad, name,
        // weekday (weeks at row 3); with rules it's name, rule, weekday, rule
        // (no pad → weeks at row 4). Week rows are two apart when `row_gap` is
        // set.
        let first_week_row = if opts.header_rules { 4 } else { 3 };
        if rel_y < first_week_row || rel_y >= usable_height {
            return None;
        }
        let row_stride = if opts.row_gap { 2 } else { 1 };
        let week = ((rel_y - first_week_row) / row_stride) as i64;
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

        // Each month's grid (35 cols + `col_gap` between each of 7 columns) is
        // centered within its column; the per-day stride widens with the gap.
        let cell_stride = 5 + opts.col_gap;
        let grid_width = MONTH_GRID_WIDTH + 6 * opts.col_gap;
        let col_start_x = inner.x + month_idx as u16 * col_width;
        let grid_offset = col_width.saturating_sub(grid_width) / 2;
        let rel_x = col.checked_sub(col_start_x + grid_offset)?;
        let cell = rel_x / cell_stride;
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

/// Bottom-block height (`day_full_areas`) at or above which the mini-months
/// space their weeks one row apart. The compact bottom is 12 rows; the tall
/// bottom is 17 (see `day_full_areas`), so 15 cleanly separates them.
const MINI_MONTH_TALL_H: u16 = 15;

/// Layout knobs for a mini-month grid in the Full-tier Day/Week bottom block.
/// The renderer (`render_month_grid`) and the click hit-test (`month_day_at`)
/// both read this so they always agree. The unzoomed month grid uses the
/// all-off default.
#[derive(Clone, Copy, Default)]
struct MiniMonthOpts {
    /// Extra columns inserted between each day column.
    col_gap: u16,
    /// Leave a blank row between week rows (roomy blocks only).
    row_gap: bool,
    /// Draw a horizontal rule under the month name and under the weekday
    /// header; also drops the top pad row to make room.
    header_rules: bool,
}

/// Spacing for the Full-tier Day/Week bottom 3-month block, derived from the
/// block's dimensions so the renderer and hit-test compute identical geometry.
/// Wider blocks spread the day columns; a taller (roomy) block spaces the
/// weeks. Header rules always frame the mini-month header in this context.
fn mini_month_spacing(block_w: u16, block_h: u16) -> MiniMonthOpts {
    // Three months share the width; each month's 35-col grid sits inside a
    // 2-col card border, so surplus width spreads over the 6 inter-column gaps.
    let col_w = block_w / 3;
    let col_gap = (col_w.saturating_sub(MONTH_GRID_WIDTH + 2) / 6).min(2);
    MiniMonthOpts {
        col_gap,
        row_gap: block_h >= MINI_MONTH_TALL_H,
        header_rules: true,
    }
}

/// Minimum per-month width for the Full-tier zoomed Month view — wider than
/// `MONTH_GRID_MIN_WIDTH` on purpose. A month is only added to the multi-month
/// layout when each shown month keeps cells wide enough (~9 cols) for the
/// hybrid busyness dots to *vary* rather than collapsing to the 2-dot floor.
/// Current month always shows; next/prev are added only when they'd stay
/// dot-rich, so at typical Full widths you get 1–2 rich months, 3 only when
/// the terminal is wide enough that every month still has room for the dots.
const MONTH_FULL_RICH_WIDTH: u16 = 63;

/// Row budget for one month in the Full-tier Month grid: 1 pad + 1 month name
/// + 1 weekday header + 6×(date row + dot row).
const MONTH_FULL_GRID_H: u16 = 15;

/// One blank row above the Full-tier Month grid so it doesn't sit flush against
/// the widget's top border. Shared by `month_full_layout` and its hit-test so
/// clicks stay aligned.
const MONTH_FULL_TOP_MARGIN: u16 = 1;

/// How much decoration the Full-tier Month grid can afford given the vertical
/// space. Richer styles cost rows, so the tallest one that still leaves room
/// for the agenda is chosen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MonthGridStyle {
    /// Borderless two-row-per-week cells (tight vertical space).
    Plain,
    /// Wall-calendar grid: every day sits in a bordered cell.
    Wall,
    /// Wall grid plus the month label wrapped in its own border box.
    WallTitled,
}

impl MonthGridStyle {
    /// `(rows before the first week's date row, rows per week)`. The per-week
    /// stride is 2 for `Plain` (date + dot) and 3 for the grid styles (date +
    /// dot + separator/border). Both the renderer and the click hit-test read
    /// these so they can never disagree on where a week sits.
    fn week_geometry(self) -> (u16, u16) {
        match self {
            // pad, month name, weekday header.
            MonthGridStyle::Plain => (3, 2),
            // month name, weekday header, top border.
            MonthGridStyle::Wall => (3, 3),
            // label box (3), weekday header, top border.
            MonthGridStyle::WallTitled => (5, 3),
        }
    }

    /// Total rows a month grid with `weeks` week-rows occupies in this style.
    fn grid_rows(self, weeks: u16) -> u16 {
        let (first_date_row, stride) = self.week_geometry();
        first_date_row + stride * weeks
    }
}

/// Number of week-rows a month occupies in its 6-week grid once trailing weeks
/// that fall entirely in the next month are trimmed (4, 5, or 6).
fn weeks_in_month_grid(first: NaiveDate, first_day_of_week: Weekday) -> u16 {
    let grid_start = start_of_week(first, first_day_of_week);
    let month = first.month();
    (0..6)
        .take_while(|&week| {
            (0..7).any(|d| (grid_start + ChronoDuration::days(week * 7 + d)).month() == month)
        })
        .count() as u16
}

/// Build one horizontal grid line — `left` + `fill×cw` per column joined by
/// `mid`, closed by `right` — for a 7-column wall-calendar grid with `cw`-wide
/// cells (e.g. `┌─┬─┐`, `├─┼─┤`, `└─┴─┘`).
fn month_grid_border(cw: usize, left: &str, mid: &str, right: &str) -> String {
    let seg = "─".repeat(cw);
    let mut s = String::from(left);
    for i in 0..7 {
        s.push_str(&seg);
        s.push_str(if i == 6 { right } else { mid });
    }
    s
}

/// Geometry of the Full-tier Month view, shared by `render_month_full` and its
/// click hit-test (`month_full_day_at`) so they never disagree on where each
/// month column sits.
struct MonthFullLayout {
    /// Chronological month list — current always; next, then prev, added only
    /// as width keeps every shown month dot-rich.
    months: Vec<(i32, u32)>,
    /// Per-month column rects from the horizontal split.
    cols: std::rc::Rc<[Rect]>,
    /// Decoration style the vertical space affords.
    style: MonthGridStyle,
    /// Rows reserved for the grid band (sized for the tallest shown month).
    grid_h: u16,
    /// Whether the day-agenda strip is shown below the grid.
    show_agenda: bool,
}

/// Controls which cells the 3-month block highlights. Day view highlights a
/// single day (the anchor); Week view highlights every day in the in-view week
/// so the user can see the week's position across the mini-calendar.
#[derive(Clone, Copy)]
enum MonthHighlight {
    /// Highlight the single given date (Day view).
    Day(NaiveDate),
    /// Highlight all 7 days of the week that contains `anchor` (Week view).
    /// Uses the same first-day-of-week convention as the rest of the widget.
    Week(NaiveDate),
}

/// Renders one month's 6-week grid into `area`. `is_anchor` controls header
/// styling so the currently-focused month stands out among neighbors.
/// `highlight` determines which cells receive the REVERSED style:
/// - `MonthHighlight::Day(d)` – only that day.
/// - `MonthHighlight::Week(d)` – all 7 days of the week containing `d`.
///
/// `dot_fn`: when `Some`, called per day to get colored dot specs appended
/// inline into the 5-char cell (Full-tier mini-month, color-by-calendar).
/// When `None`, the existing boolean cell-color highlight is used unchanged.
#[allow(clippy::too_many_arguments)]
fn render_month_grid(
    frame: &mut Frame,
    area: Rect,
    year: i32,
    month: u32,
    is_anchor: bool,
    highlight: MonthHighlight,
    events: &[Arc<Event>],
    theme: &Theme,
    first_day_of_week: Weekday,
    dot_fn: Option<&dyn Fn(NaiveDate) -> Vec<(Color, u8)>>,
    opts: MiniMonthOpts,
) {
    let Some(first) = NaiveDate::from_ymd_opt(year, month, 1) else {
        return;
    };
    let col_gap = opts.col_gap;
    let gap = " ".repeat(col_gap as usize);
    let grid_start = start_of_week(first, first_day_of_week);
    let today = Local::now().date_naive();

    // The real-life current month always reads in the "current" cyan accent
    // (matching today's event-day highlight) so it stays identifiable no matter
    // where the anchor has been navigated; the anchored month (when it isn't the
    // current one) gets the selection highlight; everything else dims back.
    let month_header_style = if today.year() == year && today.month() == month {
        theme.text_focused
    } else if is_anchor {
        theme.text_selected
    } else {
        theme.text_dim
    };
    // Rotate the Sun-anchored weekday label list so the configured
    // first-day-of-week appears in the leftmost column.
    let weekday_labels = rotated_weekday_labels(first_day_of_week);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let mut weekday_spans: Vec<Span<'_>> = Vec::with_capacity(7);
    for (i, s) in weekday_labels.iter().enumerate() {
        if i > 0 && col_gap > 0 {
            weekday_spans.push(Span::raw(gap.clone()));
        }
        weekday_spans.push(Span::styled(format!("{s:^5}"), bold));
    }
    let weekday_header = Line::from(weekday_spans);

    let mut lines: Vec<Line<'_>> = Vec::with_capacity(11);
    // A rule spans the day-column band (7 cells of 5 + the inter-column gaps).
    let grid_width = (MONTH_GRID_WIDTH + 6 * col_gap) as usize;
    let header_rule = || Line::from(Span::styled("─".repeat(grid_width), theme.text_dim));
    if !opts.header_rules {
        // Top pad for breathing room; the header rules replace it when on.
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        format!("{} {}", month_long(month), year),
        month_header_style,
    )));
    if opts.header_rules {
        lines.push(header_rule());
    }
    lines.push(weekday_header);
    if opts.header_rules {
        lines.push(header_rule());
    }

    for week in 0..6 {
        // Stop before a week that sits entirely outside this month — a fixed
        // 6-row grid otherwise trails a full week of next-month spill. Weeks
        // are chronological and week 0 always holds day 1, so the first
        // all-spill week means we're done.
        if (0..7).all(|d| (grid_start + ChronoDuration::days(week * 7 + d)).month() != month) {
            break;
        }
        let mut spans: Vec<Span<'_>> = Vec::with_capacity(7);
        for dow in 0..7 {
            if dow > 0 && col_gap > 0 {
                spans.push(Span::raw(gap.clone()));
            }
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
            let highlighted = match highlight {
                MonthHighlight::Day(d) => date == d,
                MonthHighlight::Week(anchor) => {
                    let week_start = start_of_week(anchor, first_day_of_week);
                    date >= week_start && date < week_start + ChronoDuration::days(7)
                }
            };
            if highlighted {
                style = style.add_modifier(Modifier::REVERSED);
            }

            // When a dot_fn is provided (Full-tier mini-months), replace the
            // rightmost char(s) of the 5-char cell with colored dot bullets.
            // Dots belong to the owning month only: spill days from adjacent
            // months stay dim-numbered (they carry their own month's dots
            // there), and today's bracket cell (`[DD]`) uses all 5 chars, so
            // both are skipped. Otherwise " DD" occupies 3 chars and the
            // remaining 2 hold dots.
            let used_dot_spans = if let Some(f) = dot_fn.filter(|_| in_month && date != today) {
                let dot_specs = f(date);
                if !dot_specs.is_empty() {
                    let date_part = Span::styled(format!("{day_str:>3}"), style);
                    let mut cell_spans: Vec<Span<'_>> = vec![date_part];
                    for (color, _) in dot_specs.iter().take(2) {
                        cell_spans.push(Span::styled("•", Style::default().fg(*color)));
                    }
                    let dot_count = dot_specs.len().min(2);
                    let trailing = 2usize.saturating_sub(dot_count);
                    if trailing > 0 {
                        cell_spans.push(Span::raw(" ".repeat(trailing)));
                    }
                    Some(cell_spans)
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(cell_spans) = used_dot_spans {
                spans.extend(cell_spans);
            } else {
                spans.push(Span::styled(format!("{cell:<5}"), style));
            }
        }
        lines.push(Line::from(spans));
        // Optional breathing room between week rows (roomy blocks only). The
        // trailing blank after the final week is harmless — it lands on the
        // card's bottom padding or is clipped.
        if opts.row_gap {
            lines.push(Line::from(""));
        }
    }

    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
}

/// Compute colored dot specs for a single day. Used by both the Full-tier
/// zoomed Month and the mini-month blocks inside Full-tier Day/Week views.
///
/// Two modes:
///
/// **Color-by-calendar** (`hybrid = false`): one dot per distinct active
/// calendar, ordered by event count descending then `source:calendar` for
/// ties, capped at `cap`. Each entry has count = 1.
///
/// **Hybrid** (`hybrid = true`): proportional allocation of `cap` dots
/// across active calendars by event count; each active calendar gets at
/// least 1 dot (min-1 floor). When more calendars are active than `cap`
/// allows (even with min-1), the lowest-count calendars are silently dropped
/// (tie-break: `source:calendar` ascending keeps the alphabetically-first
/// calendars).
fn day_dot_specs(
    date: NaiveDate,
    events: &[Arc<Event>],
    colors: &CalendarColors,
    cap: u8,
    hybrid: bool,
) -> Vec<(Color, u8)> {
    if cap == 0 {
        return Vec::new();
    }

    // Count events per (source, calendar) pair.
    let mut counts: Vec<(String, String, Color, u32)> = Vec::new();
    for e in events {
        if !e.on_date(date) {
            continue;
        }
        let key = (&e.source, &e.calendar);
        if let Some(entry) = counts.iter_mut().find(|c| &c.0 == key.0 && &c.1 == key.1) {
            entry.3 += 1;
        } else {
            let color = colors.resolve(&e.source, &e.calendar);
            counts.push((e.source.clone(), e.calendar.clone(), color, 1));
        }
    }

    if counts.is_empty() {
        return Vec::new();
    }

    // Sort descending by count; stable tie-break on "source:calendar".
    counts.sort_by(|a, b| {
        b.3.cmp(&a.3).then_with(|| {
            let ka = format!("{}:{}", a.0, a.1);
            let kb = format!("{}:{}", b.0, b.1);
            ka.cmp(&kb)
        })
    });

    if !hybrid {
        // Color-by-calendar: one dot per active calendar, capped.
        counts
            .into_iter()
            .take(cap as usize)
            .map(|(_, _, color, _)| (color, 1u8))
            .collect()
    } else {
        // Hybrid: the dot COUNT indicates the day's busyness — total events,
        // saturating at `cap` — and those dots are split across the active
        // calendars proportionally to each one's share, with a min-1 floor so
        // any calendar with an event always shows.
        let n = counts.len();
        let cap = cap as usize;
        let total: u32 = counts.iter().map(|c| c.3).sum();
        // Busyness, capped. A quiet day (few events) shows few dots; a packed
        // day fills to the cap.
        let n_dots = (total as usize).min(cap);

        // Over-cap: more active calendars than dots to go around, so not every
        // one can get its min-1. Keep the busiest `n_dots` (already sorted desc
        // by count; tie-break keeps the alphabetically-first) at one dot each.
        if n > n_dots {
            return counts
                .into_iter()
                .take(n_dots)
                .map(|(_, _, color, _)| (color, 1u8))
                .collect();
        }

        // Largest-remainder apportionment of `n_dots` by event share.
        let raw: Vec<f64> = counts
            .iter()
            .map(|c| c.3 as f64 / total as f64 * n_dots as f64)
            .collect();
        let mut alloc: Vec<usize> = raw.iter().map(|r| r.floor() as usize).collect();
        let mut remaining = n_dots - alloc.iter().sum::<usize>();
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|&a, &b| {
            (raw[b] - raw[b].floor())
                .partial_cmp(&(raw[a] - raw[a].floor()))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for &idx in order.iter().cycle() {
            if remaining == 0 {
                break;
            }
            alloc[idx] += 1;
            remaining -= 1;
        }

        // Min-1 floor: bump any calendar the apportionment zeroed, reclaiming
        // the dot from the current largest allocation so the total holds at
        // `n_dots` (n <= n_dots guarantees a donor with >1 exists).
        for i in 0..n {
            if alloc[i] == 0 {
                alloc[i] = 1;
                if let Some(donor) = (0..n)
                    .filter(|&j| j != i && alloc[j] > 1)
                    .max_by_key(|&j| alloc[j])
                {
                    alloc[donor] -= 1;
                }
            }
        }

        counts
            .into_iter()
            .enumerate()
            .map(|(i, (_, _, color, _))| (color, alloc[i] as u8))
            .collect()
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
    ///
    /// When a transient status message is live (set by
    /// [`Self::set_status`]) it overrides the normal metadata until
    /// its `STATUS_TTL` elapses, so the user sees "open" feedback
    /// (e.g. "No web-viewable calendar configured") right where their
    /// eye goes for chrome.
    fn title_metadata_string(&self) -> String {
        if let Some(msg) = self.live_status() {
            return msg;
        }
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

    fn render_day(&self, frame: &mut Frame, area: Rect, events: &[Arc<Event>]) {
        // Full-tier: multi-column day agendas + bottom 3-month grid.
        // Gated here so Week/Month rendering and the non-Full Day path
        // are entirely untouched.
        if ViewTier::from_rect(area) == ViewTier::Full {
            return self.render_day_full(frame, area, events);
        }

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

    /// Compute the top and optional bottom rects for the Full-tier Day and
    /// Week views.
    ///
    /// The bottom block contains the 3-month card row:
    ///   - card border top:    1 row  (Block rounded border)
    ///   - month grid content: 9 rows (1 pad + 1 name + 1 header + 6 weeks),
    ///                         or 14 in the roomy variant (a blank row between
    ///                         each of the 6 weeks — see `mini_month_spacing`)
    ///   - card border bottom: 1 row
    ///   - trailing blank:     1 row  (visual breathing room below the cards)
    ///   Bottom rect height: BOTTOM_RECT_H = 12 rows (compact) or 17 (roomy).
    ///
    /// **Degrade rule:** show the bottom only when the top can still fit
    /// ≥ 20 rows. Prefer the roomy block when even that fits above the top
    /// minimum; else the compact block; else drop the bottom entirely.
    ///
    /// This single helper is used by both `render_day_full`, `render_week_full`,
    /// and the Full-tier mouse branches so the hit-test and the painted layout
    /// always agree.
    fn day_full_areas(inner: Rect) -> (Rect, Option<Rect>) {
        // 1 month name + 1 rule + 1 weekday header + 1 rule + 6 week rows = 10
        // (the mini-months in this block always draw the header rules); the
        // roomy variant adds a blank row between each of the 6 weeks.
        const GRID_HEIGHT: u16 = 10;
        const GRID_HEIGHT_ROOMY: u16 = 15;
        // Card borders (top + bottom) surrounding the grid.
        const CARD_BORDERS: u16 = 2;
        // One blank line below the card row so the bottom breathes.
        const TRAILING_BLANK: u16 = 1;
        const BOTTOM_RECT_H: u16 = CARD_BORDERS + GRID_HEIGHT + TRAILING_BLANK;
        const BOTTOM_RECT_H_ROOMY: u16 = CARD_BORDERS + GRID_HEIGHT_ROOMY + TRAILING_BLANK;
        // Gap row between top and bottom rects.
        const SPACER: u16 = 1;
        const TOP_MIN_H: u16 = 20;

        // Roomy block only when the top keeps its minimum above it.
        let bottom_rect_h = if inner.height >= TOP_MIN_H + BOTTOM_RECT_H_ROOMY + SPACER {
            BOTTOM_RECT_H_ROOMY
        } else {
            BOTTOM_RECT_H
        };
        if inner.height >= TOP_MIN_H + bottom_rect_h + SPACER {
            let top_h = inner.height - bottom_rect_h - SPACER;
            let top = Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: top_h,
            };
            // SPACER row separates the two halves visually; the bottom
            // block starts SPACER rows after the top ends.
            let bottom = Rect {
                x: inner.x,
                y: inner.y + top_h + SPACER,
                width: inner.width,
                height: bottom_rect_h,
            };
            (top, Some(bottom))
        } else {
            (inner, None)
        }
    }

    /// The Full-tier Week view has no side gutters (unlike Day/Month, whose
    /// `content_rect_for` insets them), so both its top week grid and its bottom
    /// 3-month block are nudged in one column each side to match the Day view's
    /// margin. Applied identically by `render_week_full` and the click hit-test
    /// so the painted layout and the hit-test stay aligned.
    fn week_full_side_margin(rect: Rect) -> Rect {
        Rect {
            x: rect.x + 1,
            y: rect.y,
            width: rect.width.saturating_sub(2),
            height: rect.height,
        }
    }

    /// Full-tier Day view: N columnar day agendas (top) + 3-month calendar
    /// grid (bottom, when height allows). Gated on `ViewTier::Full` by the
    /// caller; non-Full paths still go through `render_day`.
    ///
    /// Column width is 54 chars, separated by ` │ ` (3-char gutter), so
    /// N = (width + 3) / 57.  At ~120 cols this is ≈2; wider terminals
    /// show more.  Leftmost column = `self.anchor`.
    fn render_day_full(&self, frame: &mut Frame, area: Rect, events: &[Arc<Event>]) {
        const COL_W: u16 = 54;
        const GUTTER: u16 = 3; // " │ "

        let (top, bottom_opt) = Self::day_full_areas(area);

        // --- top: columnar day agendas ---
        let n_cols = ((top.width + GUTTER) / (COL_W + GUTTER)).max(1);
        let col_step = COL_W + GUTTER;
        for i in 0..n_cols {
            let col_x = top.x + i * col_step;
            if col_x >= top.x + top.width {
                break;
            }
            let col_w = COL_W.min((top.x + top.width).saturating_sub(col_x));
            if col_w == 0 {
                break;
            }
            let col_area = Rect {
                x: col_x,
                y: top.y,
                width: col_w,
                height: top.height,
            };
            let date = self.anchor + ChronoDuration::days(i as i64);
            let is_anchor = i == 0;
            // Body left-pad: 0 for first column; 1 for subsequent columns
            // so the agenda text breathes away from the separator to their
            // left.  Right-pad: 1 for every column except the last, to keep
            // the agenda text off the separator to their right.
            let body_left = if i == 0 { 0 } else { 1 };
            let body_right = if i + 1 < n_cols { 1 } else { 0 };
            self.render_day_column(frame, col_area, date, is_anchor, body_left, body_right, events);

            // Draw ` │ ` separator between columns.
            if i + 1 < n_cols {
                let sep_x = col_x + COL_W;
                if sep_x < top.x + top.width {
                    let sep_h = top.height.saturating_sub(2);
                    if sep_h > 0 {
                        let sep_area = Rect {
                            x: sep_x + 1, // center char of " │ "
                            y: top.y + 1,
                            width: 1,
                            height: sep_h,
                        };
                        let sep_lines: Vec<Line<'_>> = (0..sep_h)
                            .map(|_| Line::from(Span::styled("│", self.theme.text_dim)))
                            .collect();
                        frame.render_widget(Paragraph::new(sep_lines), sep_area);
                    }
                }
            }
        }

        // --- bottom: 3-month calendar block (prev · anchor · next) ---
        let Some(bottom) = bottom_opt else {
            return;
        };
        self.render_three_month_block(frame, bottom, self.anchor, MonthHighlight::Day(self.anchor), events);
    }

    /// Renders the shared 3-month calendar block used by Full-tier Day and Week
    /// views. `area` is the full rect allocated for the block; it must be at
    /// least `CARD_BORDERS(2) + GRID_HEIGHT(9) + TRAILING_BLANK(1) = 12` rows
    /// tall (guaranteed by `day_full_areas`).
    ///
    /// Each of the three months (prev · `center_month` · next) is rendered
    /// inside its own rounded `Block` card so they read as distinct panels
    /// matching the weather/clock card style. The cards share the row
    /// horizontally via `Constraint::Ratio(1, 3)`.
    ///
    /// The last row of `area` is left blank (trailing blank line below cards).
    fn render_three_month_block(
        &self,
        frame: &mut Frame,
        area: Rect,
        center_month: NaiveDate,
        highlight: MonthHighlight,
        events: &[Arc<Event>],
    ) {
        let (anchor_y, anchor_m) = (center_month.year(), center_month.month());
        let months = [
            advance_month(anchor_y, anchor_m, -1),
            (anchor_y, anchor_m),
            advance_month(anchor_y, anchor_m, 1),
        ];
        let n = months.len() as u32;
        // Spacing scales with the block: wider spreads the day columns, taller
        // (roomy zoom frames) spaces the weeks; the header is always ruled.
        // Derived from the block rect so the hit-test computes the same values.
        let opts = mini_month_spacing(area.width, area.height);
        // Reserve the last row for the trailing blank line.
        let cards_h = area.height.saturating_sub(1);
        if cards_h == 0 {
            return;
        }
        let cards_area = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: cards_h,
        };
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(
                months
                    .iter()
                    .map(|_| Constraint::Ratio(1, n))
                    .collect::<Vec<_>>(),
            )
            .split(cards_area);
        for ((y, m), col_area) in months.iter().zip(cols.iter()) {
            let is_anchor = (*y, *m) == (anchor_y, anchor_m);
            let card = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(self.theme.border_style(false));
            let grid_area = card.inner(*col_area);
            frame.render_widget(card, *col_area);
            // Full-tier mini-months: color-by-calendar dots (MINI_DOT_CAP=2).
            let dot_f = |date: NaiveDate| day_dot_specs(date, events, &self.colors, 2, false);
            render_month_grid(
                frame,
                grid_area,
                *y,
                *m,
                is_anchor,
                highlight,
                events,
                &self.theme,
                self.first_day_of_week,
                Some(&dot_f),
                opts,
            );
        }
        // The row at area.y + cards_h is intentionally left blank (trailing blank line).
    }

    /// Full-tier Week view: the existing 7-column week grid fills the top, and
    /// the shared 3-month block (with the in-view week highlighted) fills the
    /// bottom. Gated on `ViewTier::Full` by the caller.
    fn render_week_full(&self, frame: &mut Frame, area: Rect, events: &[Arc<Event>], focused: bool) {
        let (top, bottom_opt) = Self::day_full_areas(area);

        // --- top: standard 7-column week grid, inset one column each side so
        // the grid doesn't run flush into the zoom frame borders ---
        self.render_week_grid(frame, Self::week_full_side_margin(top), events, focused, false);

        // --- bottom: 3-month block with the in-view week highlighted, inset to
        // the same margin so it lines up with the grid above ---
        let Some(bottom) = bottom_opt else {
            return;
        };
        self.render_three_month_block(
            frame,
            Self::week_full_side_margin(bottom),
            self.anchor,
            MonthHighlight::Week(self.anchor),
            events,
        );
    }

    /// The inner 7-column week grid renderer, extracted so `render_week` and
    /// `render_week_full` can both call it without duplication.
    /// `connect_sides` overpaints the `├`/`┤` rule connectors one column
    /// outside `area` so the divider kisses the widget border in the normal
    /// week view. The Full-tier view insets the grid for a readability margin
    /// and passes `false`, so those connectors don't poke into the margin.
    fn render_week_grid(
        &self,
        frame: &mut Frame,
        area: Rect,
        events: &[Arc<Event>],
        focused: bool,
        connect_sides: bool,
    ) {
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
            if connect_sides {
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
        }

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

            let day_events: Vec<&Event> = events
                .iter()
                .map(Arc::as_ref)
                .filter(|e| e.on_date(day))
                .collect();
            let mut event_lines: Vec<Line<'_>> = Vec::new();
            if day_events.is_empty() {
                event_lines.push(Line::from(Span::styled("·", self.theme.text_dim)));
            } else {
                let wrap_width = col_area.width.saturating_sub(1) as usize;
                for e in day_events {
                    let color = self.colors.resolve(&e.source, &e.calendar);
                    let (prefix_str, prefix_style) = if e.all_day {
                        ("• ".to_string(), Style::default().fg(color))
                    } else {
                        (
                            format!("{:02}:{:02} ", e.start.hour(), e.start.minute()),
                            Style::default().fg(Color::Gray),
                        )
                    };
                    let prefix_w = prefix_str.chars().count();
                    let line1_budget = wrap_width.saturating_sub(prefix_w).max(1);
                    let cont_budget = wrap_width.saturating_sub(1).max(1);
                    const MAX_LINES: usize = 3;
                    let title_chars: Vec<char> = e.title.chars().collect();
                    let mut wrapped: Vec<String> = Vec::new();
                    let mut idx = 0;
                    for line_i in 0..MAX_LINES {
                        if idx >= title_chars.len() {
                            break;
                        }
                        if line_i > 0 {
                            while idx < title_chars.len()
                                && title_chars[idx].is_whitespace()
                            {
                                idx += 1;
                            }
                            if idx >= title_chars.len() {
                                break;
                            }
                        }
                        let budget = if line_i == 0 { line1_budget } else { cont_budget };
                        let end = (idx + budget).min(title_chars.len());
                        wrapped.push(title_chars[idx..end].iter().collect());
                        idx = end;
                    }
                    if idx < title_chars.len() {
                        let budget = if wrapped.len() == 1 {
                            line1_budget
                        } else {
                            cont_budget
                        };
                        if let Some(last) = wrapped.last_mut() {
                            let count = last.chars().count();
                            if count < budget {
                                last.push('…');
                            } else if !last.ends_with('…') {
                                let mut tail: Vec<char> = last.chars().collect();
                                tail.pop();
                                tail.push('…');
                                *last = tail.into_iter().collect();
                            }
                        }
                    }
                    if wrapped.is_empty() {
                        event_lines.push(Line::from(Span::styled(prefix_str, prefix_style)));
                    } else {
                        let title_style = Style::default().fg(color);
                        for (i, chunk) in wrapped.into_iter().enumerate() {
                            if i == 0 {
                                event_lines.push(Line::from(vec![
                                    Span::styled(prefix_str.clone(), prefix_style),
                                    Span::styled(chunk, title_style),
                                ]));
                            } else {
                                event_lines.push(Line::from(vec![
                                    Span::raw(" ".to_string()),
                                    Span::styled(chunk, title_style),
                                ]));
                            }
                        }
                    }
                    if let Some(loc) = &e.location {
                        let loc_chars: Vec<char> = loc.chars().collect();
                        let mut loc_idx = 0;
                        for line_i in 0..MAX_LINES {
                            if loc_idx >= loc_chars.len() {
                                break;
                            }
                            if line_i > 0 {
                                while loc_idx < loc_chars.len()
                                    && loc_chars[loc_idx].is_whitespace()
                                {
                                    loc_idx += 1;
                                }
                                if loc_idx >= loc_chars.len() {
                                    break;
                                }
                            }
                            let end = (loc_idx + cont_budget).min(loc_chars.len());
                            let mut chunk: String = loc_chars[loc_idx..end].iter().collect();
                            loc_idx = end;
                            if line_i + 1 == MAX_LINES && loc_idx < loc_chars.len() {
                                let count = chunk.chars().count();
                                if count < cont_budget {
                                    chunk.push('…');
                                } else if !chunk.ends_with('…') {
                                    let mut tail: Vec<char> = chunk.chars().collect();
                                    tail.pop();
                                    tail.push('…');
                                    chunk = tail.into_iter().collect();
                                }
                            }
                            event_lines.push(Line::from(vec![
                                Span::raw(" ".to_string()),
                                Span::styled(chunk, self.theme.text_dim),
                            ]));
                        }
                    }
                }
            }

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

    fn render_day_column(
        &self,
        frame: &mut Frame,
        area: Rect,
        date: NaiveDate,
        is_anchor: bool,
        body_left_pad: u16,
        body_right_pad: u16,
        events: &[Arc<Event>],
    ) {
        let day_events: Vec<&Event> = events
            .iter()
            .map(Arc::as_ref)
            .filter(|e| e.on_date(date))
            .collect();

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
    fn render_month_agenda(&self, frame: &mut Frame, area: Rect, events: &[Arc<Event>]) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let day_events: Vec<&Event> = events
            .iter()
            .map(Arc::as_ref)
            .filter(|e| e.on_date(self.anchor))
            .collect();
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
        // Blank line so the day's events breathe away from the date header.
        lines.push(Line::from(""));
        // The header + blank count as lead-in; agenda events begin at
        // relative line 0 inside `agenda_lines`, which lands after them in
        // `lines`. Track that offset so the autoscroll target maps correctly.
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

    fn render_week(&self, frame: &mut Frame, area: Rect, events: &[Arc<Event>], focused: bool) {
        // Full-tier: week grid on top + 3-month block on bottom.
        if ViewTier::from_rect(area) == ViewTier::Full {
            return self.render_week_full(frame, area, events, focused);
        }
        self.render_week_grid(frame, area, events, focused, true);
    }

    fn render_month(&self, frame: &mut Frame, area: Rect, events: &[Arc<Event>]) {
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
            // Unzoomed month grid: no gaps or header rules (kept compact).
            render_month_grid(
                frame,
                *col_area,
                *y,
                *m,
                is_anchor,
                MonthHighlight::Day(self.anchor),
                events,
                &self.theme,
                self.first_day_of_week,
                None,
                MiniMonthOpts::default(),
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

    /// Full-tier Month view: two-row-per-week grid (date row + dot strip)
    /// with a responsive multi-month layout. Inclusion priority as width
    /// grows: current month always → next month if width allows → previous
    /// month tertiary. When 2+ months are shown they are ordered
    /// chronologically left-to-right (prev · current · next).
    ///
    /// DOT_CAP per day cell = `(cell_width - 4).clamp(2, 6)`.
    /// Dots use the hybrid allocation algorithm (proportional + min-1 floor).
    /// Shared geometry for the Full-tier Month view so `render_month_full` and
    /// its click hit-test (`month_full_day_at`) never disagree on where each
    /// month column sits. Returns the chronological month list (current always;
    /// next, then prev added only when width keeps every month dot-rich), the
    /// per-month column rects, and whether the day-agenda strip is shown.
    /// `None` when the area is too short for the Full grid — the caller then
    /// falls back to the standard month view.
    fn month_full_layout(&self, area: Rect) -> Option<MonthFullLayout> {
        const FOOTER_RESERVED: u16 = 1;
        // Blank · horizontal rule · blank between the grid and the agenda.
        const SEPARATOR_ROWS: u16 = 3;
        const AGENDA_MIN_ROWS: u16 = 3;

        if area.height < MONTH_FULL_GRID_H + FOOTER_RESERVED + MONTH_FULL_TOP_MARGIN {
            return None;
        }
        let (anchor_y, anchor_m) = (self.anchor.year(), self.anchor.month());
        let months: Vec<(i32, u32)> = if area.width >= 3 * MONTH_FULL_RICH_WIDTH {
            vec![
                advance_month(anchor_y, anchor_m, -1),
                (anchor_y, anchor_m),
                advance_month(anchor_y, anchor_m, 1),
            ]
        } else if area.width >= 2 * MONTH_FULL_RICH_WIDTH {
            vec![(anchor_y, anchor_m), advance_month(anchor_y, anchor_m, 1)]
        } else {
            vec![(anchor_y, anchor_m)]
        };

        // The grid band is sized for the tallest month shown, so every column
        // shares the same style and the agenda sits below all of them.
        let weeks = months
            .iter()
            .filter_map(|(y, m)| NaiveDate::from_ymd_opt(*y, *m, 1))
            .map(|first| weeks_in_month_grid(first, self.first_day_of_week))
            .max()
            .unwrap_or(6);

        // A one-row top margin keeps the grid off the widget's top border.
        let usable = area.height.saturating_sub(FOOTER_RESERVED + MONTH_FULL_TOP_MARGIN);
        // Richest decoration that still leaves room for the agenda; failing
        // that, the richest grid that fits on its own (no agenda).
        let with_agenda = |style: MonthGridStyle| {
            usable >= style.grid_rows(weeks) + SEPARATOR_ROWS + AGENDA_MIN_ROWS
        };
        let (style, show_agenda) = if with_agenda(MonthGridStyle::WallTitled) {
            (MonthGridStyle::WallTitled, true)
        } else if with_agenda(MonthGridStyle::Wall) {
            (MonthGridStyle::Wall, true)
        } else if with_agenda(MonthGridStyle::Plain) {
            (MonthGridStyle::Plain, true)
        } else if usable >= MonthGridStyle::WallTitled.grid_rows(weeks) {
            (MonthGridStyle::WallTitled, false)
        } else if usable >= MonthGridStyle::Wall.grid_rows(weeks) {
            (MonthGridStyle::Wall, false)
        } else {
            (MonthGridStyle::Plain, false)
        };
        let grid_h = if show_agenda {
            style.grid_rows(weeks)
        } else {
            usable
        };

        let grid_area = Rect {
            x: area.x,
            y: area.y + MONTH_FULL_TOP_MARGIN,
            width: area.width,
            height: grid_h,
        };
        let n = months.len() as u32;
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(
                (0..months.len())
                    .map(|_| Constraint::Ratio(1, n))
                    .collect::<Vec<_>>(),
            )
            .split(grid_area);
        Some(MonthFullLayout {
            months,
            cols,
            style,
            grid_h,
            show_agenda,
        })
    }

    /// Hit-test a click against the Full-tier Month grid. Returns the in-month
    /// date under `(col, row)`, or `None` for the header rows, spill days, the
    /// agenda strip below the grid, or when the Full grid isn't shown.
    fn month_full_day_at(&self, area: Rect, col: u16, row: u16) -> Option<NaiveDate> {
        let layout = self.month_full_layout(area)?;
        let (first_date_row, stride) = layout.style.week_geometry();
        for ((year, month), col_area) in layout.months.iter().zip(layout.cols.iter()) {
            if col < col_area.x || col >= col_area.x + col_area.width {
                continue;
            }
            if row < col_area.y || row >= col_area.y + col_area.height {
                return None;
            }
            // Map the column to a weekday. The content is centered in the
            // column (Alignment::Center); the grid styles add border columns
            // between cells, so their day pitch is `cw + 1`.
            let dow = match layout.style {
                MonthGridStyle::Plain => {
                    let cell_w = (col_area.width / 7).max(4);
                    let x0 = col_area.x + col_area.width.saturating_sub(cell_w * 7) / 2;
                    if col < x0 {
                        return None;
                    }
                    (col - x0) / cell_w
                }
                MonthGridStyle::Wall | MonthGridStyle::WallTitled => {
                    let cw = (col_area.width.saturating_sub(8) / 7).max(3);
                    let grid_w = cw * 7 + 8;
                    // Skip the centering offset and the left border column.
                    let x0 = col_area.x + col_area.width.saturating_sub(grid_w) / 2 + 1;
                    if col < x0 {
                        return None;
                    }
                    (col - x0) / (cw + 1)
                }
            };
            if dow >= 7 {
                return None;
            }
            // Rows before the first date row are headers/borders; within a week
            // the date and dot rows select the day, the separator row does not.
            let rel_row = row - col_area.y;
            if rel_row < first_date_row {
                return None;
            }
            let into = rel_row - first_date_row;
            if stride == 3 && into % stride == 2 {
                return None; // the grid separator/border line between weeks
            }
            let week = (into / stride) as i64;
            let first = NaiveDate::from_ymd_opt(*year, *month, 1)?;
            let grid_start = start_of_week(first, self.first_day_of_week);
            let date = grid_start + ChronoDuration::days(week * 7 + dow as i64);
            // Only in-month cells act — spill days are de-emphasized, and this
            // also rejects clicks below the trimmed trailing weeks.
            return (date.month() == *month).then_some(date);
        }
        None
    }

    fn render_month_full(&self, frame: &mut Frame, area: Rect, events: &[Arc<Event>]) {
        const SEPARATOR_ROWS: u16 = 3;
        const FOOTER_RESERVED: u16 = 1;

        let Some(MonthFullLayout {
            months,
            cols,
            style,
            grid_h,
            show_agenda,
        }) = self.month_full_layout(area)
        else {
            self.render_month(frame, area, events);
            return;
        };

        let (anchor_y, anchor_m) = (self.anchor.year(), self.anchor.month());
        let today = Local::now().date_naive();
        let weekday_labels = rotated_weekday_labels(self.first_day_of_week);

        for ((year, month), col_area) in months.iter().zip(cols.iter()) {
            let is_anchor = (*year, *month) == (anchor_y, anchor_m);
            // Current real-life month always in the cyan "current" accent; the
            // anchored month (when different) gets the selection highlight.
            let header_style = if (*year, *month) == (today.year(), today.month()) {
                self.theme.text_focused
            } else if is_anchor {
                self.theme.text_selected
            } else {
                self.theme.text_dim
            };
            self.render_month_cell(
                frame,
                *col_area,
                *year,
                *month,
                style,
                header_style,
                &weekday_labels,
                today,
                events,
            );
        }

        if show_agenda {
            // The grid band starts below the one-row top margin.
            let grid_top = area.y + MONTH_FULL_TOP_MARGIN;
            let usable = area.height.saturating_sub(FOOTER_RESERVED + MONTH_FULL_TOP_MARGIN);
            // Blank · horizontal rule · blank between the grid band and the
            // agenda; the grid Paragraphs leave the flanking rows empty.
            let rule_y = grid_top + grid_h + 1;
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "─".repeat(area.width as usize),
                    self.theme.text_dim,
                ))),
                Rect {
                    x: area.x,
                    y: rule_y,
                    width: area.width,
                    height: 1,
                },
            );
            let agenda_y = grid_top + grid_h + SEPARATOR_ROWS;
            let agenda_h = (grid_top + usable).saturating_sub(agenda_y);
            let agenda_area = Rect {
                x: area.x,
                y: agenda_y,
                width: area.width,
                height: agenda_h,
            };
            self.render_month_agenda(frame, agenda_area, events);
        }
    }

    /// Render one month into `col_area` for the Full-tier Month view, in the
    /// given decoration `style`. Layout rows must stay in lockstep with
    /// [`MonthGridStyle::week_geometry`] (read by the click hit-test).
    #[allow(clippy::too_many_arguments)]
    fn render_month_cell(
        &self,
        frame: &mut Frame,
        col_area: Rect,
        year: i32,
        month: u32,
        style: MonthGridStyle,
        header_style: Style,
        weekday_labels: &[&'static str; 7],
        today: NaiveDate,
        events: &[Arc<Event>],
    ) {
        let Some(first) = NaiveDate::from_ymd_opt(year, month, 1) else {
            return;
        };
        let grid_start = start_of_week(first, self.first_day_of_week);
        let weeks = weeks_in_month_grid(first, self.first_day_of_week) as i64;
        let month_label = format!("{} {}", month_long(month), year);
        let border = self.theme.text_dim;
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let plain = style == MonthGridStyle::Plain;

        // Grid styles spend 8 columns on cell borders (`│`×8); Plain doesn't.
        let cw = if plain {
            (col_area.width / 7).max(4)
        } else {
            (col_area.width.saturating_sub(8) / 7).max(3)
        };
        let cwu = cw as usize;
        let dot_cap = (cw.saturating_sub(4)).clamp(2, 6) as u8;

        let mut lines: Vec<Line<'_>> = Vec::new();

        // Month label — plain, spanning, or wrapped in its own border box.
        match style {
            MonthGridStyle::Plain => {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(month_label, header_style)));
            }
            MonthGridStyle::Wall => {
                let gw = cwu * 7 + 8;
                lines.push(Line::from(Span::styled(
                    format!("{month_label:^gw$}"),
                    header_style,
                )));
            }
            MonthGridStyle::WallTitled => {
                let inner = cwu * 7 + 6;
                let bar = "─".repeat(inner);
                lines.push(Line::from(Span::styled(format!("╭{bar}╮"), header_style)));
                lines.push(Line::from(Span::styled(
                    format!("│{month_label:^inner$}│"),
                    header_style,
                )));
                lines.push(Line::from(Span::styled(format!("╰{bar}╯"), header_style)));
            }
        }

        // Weekday header, column-aligned with the day cells below.
        let weekday_line = if plain {
            Line::from(
                weekday_labels
                    .iter()
                    .map(|s| Span::styled(format!("{s:^cwu$}"), bold))
                    .collect::<Vec<_>>(),
            )
        } else {
            let mut s = String::from(" ");
            for lbl in weekday_labels {
                s.push_str(&format!("{lbl:^cwu$}"));
                s.push(' ');
            }
            Line::from(Span::styled(s, bold))
        };
        lines.push(weekday_line);

        if !plain {
            lines.push(Line::from(Span::styled(
                month_grid_border(cwu, "┌", "┬", "┐"),
                border,
            )));
        }

        for week in 0..weeks {
            let mut date_spans: Vec<Span<'_>> = Vec::new();
            let mut dot_spans: Vec<Span<'_>> = Vec::new();
            if !plain {
                date_spans.push(Span::styled("│", border));
                dot_spans.push(Span::styled("│", border));
            }
            for dow in 0..7i64 {
                let date = grid_start + ChronoDuration::days(week * 7 + dow);
                let in_month = date.month() == month;
                let core = if date == today {
                    format!("[{}]", date.day())
                } else {
                    format!("{}", date.day())
                };
                let mut st = if in_month {
                    self.theme.text_plain
                } else {
                    self.theme.text_dim
                };
                if date == self.anchor {
                    st = st.add_modifier(Modifier::REVERSED);
                }
                date_spans.push(Span::styled(format!("{core:^cwu$}"), st));

                // Dot cell — dots centered, owning-month only.
                let specs = if in_month {
                    day_dot_specs(date, events, &self.colors, dot_cap, true)
                } else {
                    Vec::new()
                };
                let total = specs.iter().map(|s| s.1 as usize).sum::<usize>().min(cwu);
                let left = (cwu - total) / 2;
                if left > 0 {
                    dot_spans.push(Span::raw(" ".repeat(left)));
                }
                let mut drawn = 0usize;
                for (color, count) in &specs {
                    let n = (*count as usize).min(cwu - left - drawn);
                    if n == 0 {
                        break;
                    }
                    dot_spans.push(Span::styled("•".repeat(n), Style::default().fg(*color)));
                    drawn += n;
                }
                let placed = left + drawn;
                if placed < cwu {
                    dot_spans.push(Span::raw(" ".repeat(cwu - placed)));
                }
                if !plain {
                    date_spans.push(Span::styled("│", border));
                    dot_spans.push(Span::styled("│", border));
                }
            }
            lines.push(Line::from(date_spans));
            lines.push(Line::from(dot_spans));
            if !plain {
                let (l, m, r) = if week == weeks - 1 {
                    ("└", "┴", "┘")
                } else {
                    ("├", "┼", "┤")
                };
                lines.push(Line::from(Span::styled(month_grid_border(cwu, l, m, r), border)));
            }
        }

        frame.render_widget(
            Paragraph::new(lines).alignment(Alignment::Center),
            col_area,
        );
    }

    /// Numbered open-target picker. Drawn over the calendar's outer
    /// `area` (not the inner content area) so the modal can extend
    /// to the borders if needed; we still hold to a small centred
    /// box. Looks similar to `ConfirmModal` but lists numbered
    /// choices instead of a y/N pair, so it lives here rather than
    /// in `ui::modal` until a second widget needs the same shape.
    fn render_open_picker(&self, frame: &mut Frame, area: Rect, targets: &[WebTarget]) {
        if targets.is_empty() {
            return;
        }
        let widest_label = targets
            .iter()
            .map(|t| t.label.chars().count())
            .max()
            .unwrap_or(0);
        // Inner width: digit + ") " + label, plus 2-col left/right padding.
        let inner_w = (widest_label + 4 + 4).max(28) as u16;
        // Height: title row + blank + N target rows + blank + hint row.
        let inner_h = (targets.len() as u16) + 4;
        let modal_w = (inner_w + 2).min(area.width.saturating_sub(2));
        let modal_h = (inner_h + 2).min(area.height.saturating_sub(2));
        if modal_w < 6 || modal_h < 5 {
            return;
        }
        let modal_area = Rect {
            x: area.x + (area.width.saturating_sub(modal_w)) / 2,
            y: area.y + (area.height.saturating_sub(modal_h)) / 2,
            width: modal_w,
            height: modal_h,
        };
        // Clear the cells the modal will paint so the calendar
        // chrome behind it doesn't bleed through.
        frame.render_widget(ratatui::widgets::Clear, modal_area);

        // Modal title surfaces the view + anchor so the user sees
        // what's about to open before picking a source. For Google
        // both view and date round-trip; for Outlook the view does
        // and the date is implicit (see `outlook_calendar_url`).
        let view_label = match self.view {
            CalendarView::Day => "day",
            CalendarView::Week => "week",
            CalendarView::Month => "month",
        };
        let title = format!(
            " Open · {} · {} ",
            view_label,
            self.anchor.format("%b %-d, %Y"),
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(self.theme.border_style(true))
            .title(Span::styled(title, self.theme.text_selected));
        let inner = block.inner(modal_area);
        frame.render_widget(block, modal_area);

        let mut lines: Vec<Line> = Vec::with_capacity(targets.len() + 2);
        for (i, target) in targets.iter().enumerate() {
            let key = Span::styled(
                format!("  {})", i + 1),
                self.theme.text_focused,
            );
            let label = Span::styled(
                format!(" {}", target.label),
                self.theme.text_plain,
            );
            lines.push(Line::from(vec![key, label]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Any other key cancels.",
            self.theme.text_dim,
        )));

        let body_area = Rect {
            x: inner.x,
            y: inner.y + 1,
            width: inner.width,
            height: inner.height.saturating_sub(1),
        };
        frame.render_widget(Paragraph::new(lines), body_area);
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
        // Track the real calendar day so an unattended widget rolls to
        // the new day at midnight instead of going stale. Runs before
        // the poll check so a fresh anchor's range is fetched this tick.
        self.maybe_auto_roll();
        if self.is_due() {
            self.spawn_refresh();
        }
        // Drive the dirty flag when a transient status crosses its
        // TTL so the title-bar metadata reverts on the next frame.
        // Atomic-gated so an idle dashboard (no pending status) skips
        // the state lock entirely.
        if self.feedback_pending.load(Ordering::Relaxed) {
            let mut st = self.state.lock().expect("calendar state poisoned");
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
        let mut st = self.state.lock().expect("calendar state poisoned");
        std::mem::replace(&mut st.dirty, false)
    }

    fn render(&self, frame: &mut Frame, area: Rect, focused: bool) {
        // Stash focus for `maybe_auto_roll` — render is the only place
        // the widget is told whether it's focused, and focus only shifts
        // on redraw-forcing events, so this stays current between draws.
        self.is_focused.store(focused, Ordering::Relaxed);
        // Track whether we're in Full tier so key/tab handlers (which
        // don't receive `area`) can suppress Month-view selection.
        let is_full = ViewTier::from_rect(area) == ViewTier::Full;
        self.last_full.store(is_full, Ordering::Relaxed);
        let effective_view = self.view;
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
        let content = content_rect_for(effective_view, inner);
        match effective_view {
            CalendarView::Day => self.render_day(frame, content, &events),
            CalendarView::Week => self.render_week(frame, content, &events, focused),
            CalendarView::Month => {
                if is_full {
                    self.render_month_full(frame, content, &events);
                } else {
                    self.render_month(frame, content, &events);
                }
            }
        }

        // Footer row: [Today] action + view tabs on the left, dim keyboard
        // hint on the right.
        if inner.height >= 2 {
            let hint_area = Rect {
                x: inner.x,
                y: inner.y + inner.height - 1,
                width: inner.width,
                height: 1,
            };
            let mut spans: Vec<Span<'_>> = vec![Span::raw(" ")];
            let today_style = if self.current_view_contains_today() {
                self.theme.text_focused
            } else {
                self.theme.text_dim
            };
            let shortcut_style = self.theme.text_selected;
            let today_active = self.current_view_contains_today();
            let today_first_style = if today_active {
                today_style
            } else {
                shortcut_style
            };
            spans.push(Span::styled("[", today_style));
            spans.push(Span::styled("t", today_first_style));
            spans.push(Span::styled("oday", today_style));
            spans.push(Span::styled("]", today_style));
            spans.push(Span::raw(" "));
            for (v, label) in VIEW_TABS {
                let active = *v == effective_view;
                let base = if active {
                    self.theme.text_selected
                } else {
                    self.theme.text_dim
                };
                let first_style = if active { base } else { shortcut_style };
                spans.push(Span::styled("[", base));
                let mut chars = label.chars();
                if let Some(first) = chars.next() {
                    spans.push(Span::styled(first.to_string(), first_style));
                }
                let rest: String = chars.collect();
                if !rest.is_empty() {
                    spans.push(Span::styled(rest, base));
                }
                spans.push(Span::styled("]", base));
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled("  ←/→ nav  ·  o open", self.theme.text_dim));
            frame.render_widget(Paragraph::new(Line::from(spans)), hint_area);
        }

        // Open-picker modal overlays the calendar when active. Drawn
        // last so it sits on top of every other paint.
        let picker_targets = self
            .state
            .lock()
            .expect("calendar state poisoned")
            .open_picker
            .clone();
        if let Some(targets) = picker_targets {
            self.render_open_picker(frame, area, &targets);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        self.last_activity = Instant::now();
        if key.modifiers != KeyModifiers::NONE && key.modifiers != KeyModifiers::SHIFT {
            return EventResult::Ignored;
        }
        // Open-picker takes priority over normal bindings when shown,
        // so digit keys route to it rather than (e.g.) Day/Week/Month
        // view shortcuts.
        if self.handle_open_picker_key(key) {
            return EventResult::Handled;
        }
        // Uppercase ASCII letters are reserved for the app-wide
        // `Shift+<letter>` focus-jump dispatcher — never consume them here.
        if let KeyCode::Char(c) = key.code {
            if c.is_ascii_uppercase() {
                return EventResult::Ignored;
            }
        }
        let step = self.nav_step();
        let anchor_before = self.anchor;
        let result = match key.code {
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
            // Month view: the arrow keys walk the selected day across the grid
            // (←/→ ±1 day, ↑/↓ ±1 week) so it reads like a wall calendar. h/l
            // still page months and j/k still scroll the agenda (handled by the
            // combined arms below, which the guards let the letters fall through
            // to). Day/Week views keep arrows bound to h/l · j/k as before.
            KeyCode::Left if self.view == CalendarView::Month => {
                self.anchor -= ChronoDuration::days(1);
                self.reset_agenda_scroll();
                self.mark_dirty_if_uncovered();
                EventResult::Handled
            }
            KeyCode::Right if self.view == CalendarView::Month => {
                self.anchor += ChronoDuration::days(1);
                self.reset_agenda_scroll();
                self.mark_dirty_if_uncovered();
                EventResult::Handled
            }
            KeyCode::Up if self.view == CalendarView::Month => {
                self.anchor -= ChronoDuration::days(7);
                self.reset_agenda_scroll();
                self.mark_dirty_if_uncovered();
                EventResult::Handled
            }
            KeyCode::Down if self.view == CalendarView::Month => {
                self.anchor += ChronoDuration::days(7);
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
            // events than fit in the body. Day/Week time navigation lives on
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
            // `o` — open one of the configured web calendars in the
            // browser. See `jump_to_external` for the 0 / 1 / 2+
            // provider routing.
            KeyCode::Char('o') => {
                self.jump_to_external();
                EventResult::Handled
            }
            _ => EventResult::Ignored,
        };
        // A user reposition re-bases the auto-roll: future day-rollovers
        // advance from where the user just landed, not from a stale date.
        if self.anchor != anchor_before {
            self.rollover_date = Local::now().date_naive();
        }
        result
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, area: Rect) -> EventResult {
        self.last_activity = Instant::now();
        let anchor_before = self.anchor;
        let result = self.handle_mouse_event(mouse, area);
        if self.anchor != anchor_before {
            self.rollover_date = Local::now().date_naive();
        }
        result
    }

    fn handle_command(&mut self, _cmd: &str, _args: &[&str]) -> Result<bool> {
        Ok(false)
    }

    fn keybindings(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("d / w / m", "switch view: day / week / month"),
            ("h / l", "previous / next period (month: page months)"),
            ("← / →", "month: move day  ·  day/week: as h / l"),
            ("↑ / ↓", "month: move week  ·  day/week: scroll agenda"),
            ("j / k", "scroll the day's agenda"),
            ("PgUp / PgDn", "scroll agenda ±10 lines"),
            ("wheel", "scroll the day's agenda"),
            ("t", "jump to today"),
            ("o", "open calendar in browser (picker when multiple configured)"),
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

impl CalendarWidget {
    /// Mouse-event body for [`Widget::handle_mouse`]. Split out so the
    /// trait method can wrap it with the activity-stamp + rollover
    /// re-base bookkeeping around the early returns inside.
    fn handle_mouse_event(&mut self, mouse: MouseEvent, area: Rect) -> EventResult {
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

        // Bottom hint row hosts the [Today] button + view tabs.
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
        //
        // `content` is computed here (before the Full-tier check) so the
        // Full-tier bottom hit-test uses the same rect that `render_day`
        // receives — both strip the 1-col gutters and hint row.
        let content = content_rect_for(self.view, inner);

        let is_full_tier = ViewTier::from_rect(area) == ViewTier::Full;

        // Full-tier Day and Week views: the bottom 3-month block is a click
        // target. The block uses rounded card borders (1 row top + 1 row
        // bottom), so the grid content inside starts 1 row into `bottom`.
        // `month_day_at` expects a rect where row 0 is the grid's blank pad
        // line, so we shift `y` up by 1 (and shrink `height` by 2) to
        // align the hit-test with the rendered grid layout.
        if (self.view == CalendarView::Day || self.view == CalendarView::Week) && is_full_tier {
            let (_top, bottom_opt) = Self::day_full_areas(content);
            // Week view nudges the bottom block in one column each side (Day
            // already has that margin via its content gutters); match here.
            let bottom_opt = bottom_opt.map(|b| {
                if self.view == CalendarView::Week {
                    Self::week_full_side_margin(b)
                } else {
                    b
                }
            });
            if let Some(bottom) = bottom_opt {
                if mouse.row >= bottom.y && mouse.row < bottom.y + bottom.height {
                    // Offset past the card top border so month_day_at's
                    // "skip rows 0-2 (pad + name + header)" math aligns
                    // with the rendered grid.
                    let grid_rect = Rect {
                        x: bottom.x,
                        y: bottom.y + 1,
                        width: bottom.width,
                        height: bottom.height.saturating_sub(2),
                    };
                    // Same spacing the block was rendered with (from the block
                    // rect), so the hit-test lands on the right day.
                    let opts = mini_month_spacing(bottom.width, bottom.height);
                    if let Some(date) = self.month_day_at(mouse.column, mouse.row, grid_rect, opts) {
                        if self.view == CalendarView::Day {
                            // Day view: jump anchor to the clicked date.
                            self.anchor = date;
                            self.reset_agenda_scroll();
                        } else {
                            // Week view: navigate to the week containing
                            // the clicked date; stay in Week view.
                            self.anchor = date;
                            self.reset_agenda_scroll();
                        }
                        self.mark_dirty_if_uncovered();
                        return EventResult::Handled;
                    }
                }
            }
        }

        match self.view {
            CalendarView::Week => {
                // In Full tier the bottom block handled clicks in its area above;
                // clicks in the top week-grid still promote to Day view as normal.
                if let Some(date) = self.week_day_at(mouse.column, mouse.row, content) {
                    self.anchor = date;
                    self.view = CalendarView::Day;
                    self.reset_agenda_scroll();
                    self.mark_dirty_if_uncovered();
                    return EventResult::Handled;
                }
            }
            CalendarView::Month => {
                // Full tier renders the two-row-per-week `render_month_full`
                // grid, so hit-test against its geometry; other tiers use the
                // standard month grid. A click retargets the agenda below.
                let hit = if is_full_tier {
                    self.month_full_day_at(content, mouse.column, mouse.row)
                } else {
                    // Unzoomed month grid uses no gaps or rules.
                    self.month_day_at(mouse.column, mouse.row, content, MiniMonthOpts::default())
                };
                if let Some(date) = hit {
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
mod tests;
