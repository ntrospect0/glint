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
use super::{AppContext, EventResult, Widget};

use provider::{CalendarProvider, Event};

use crate::cache::ScopedCache;
use crate::theme::{ColorScheme, Theme};
use crate::ui::{apply_title_row, big_digits, MetadataEmphasis};

/// TTL for transient title-bar status messages (e.g. open-failed
/// reasons, "no web-viewable calendar configured" notices).
const STATUS_TTL: Duration = Duration::from_millis(2500);


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
            configured_provider_kinds: config.providers.iter().map(|p| p.kind).collect(),
            feedback_pending: AtomicBool::new(false),
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
    events: &[Arc<Event>],
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

    fn render_week(&self, frame: &mut Frame, area: Rect, events: &[Arc<Event>], focused: bool) {
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
                    // Prefix slot: HH:MM (5 chars) for timed events, a
                    // bullet for all-day. Trailing space separates it
                    // from the title. Timestamps render in `Color::Gray`
                    // so the times don't blend into the per-calendar
                    // title color line-to-line — matches the day-view
                    // and month-agenda time column.
                    let (prefix_str, prefix_style) = if e.all_day {
                        ("• ".to_string(), Style::default().fg(color))
                    } else {
                        (
                            format!("{:02}:{:02} ", e.start.hour(), e.start.minute()),
                            Style::default().fg(Color::Gray),
                        )
                    };
                    let prefix_w = prefix_str.chars().count();
                    // Title wraps with two budgets: line 1 leaves room
                    // for the prefix; continuation lines reserve 1
                    // column for the hanging-indent space so wrapped
                    // text aligns just under the title (not the time)
                    // and reads as "more of the same event."
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
                        // Continuation lines skip any leading whitespace
                        // so wraps mid-word don't leave a phantom indent.
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
                        // Ran out of line budget — ellipsize the last
                        // line in place so width stays invariant.
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
                        // Edge case: empty title — still emit the
                        // prefix line so the time is visible.
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
                    // Location row(s), dim styled to match the
                    // day-view agenda's `cont_indent + text_dim`
                    // formatting. Wraps under the same 1-space
                    // hanging-indent budget as the title so the
                    // visual stack reads cleanly.
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
                            // Ellipsize the last line of the location
                            // when it spills past MAX_LINES so width
                            // stays invariant — same behavior as the
                            // title wrap above.
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
            // Lowercase labels. On *inactive* tabs the first letter
            // takes the scheme's selection-highlight style
            // (`theme.text_selected`) to surface the t/d/w/m keyboard
            // shortcuts. On the *active* tab the whole label runs in
            // text_selected and the shortcut accent blends in — the
            // hint is redundant when the user is already there.
            // `[today]` follows the same rule via the already-computed
            // today_style. Pulling from the scheme means user theme
            // overrides flow through, and selection-highlight is
            // visually distinct from the `text_shortcut` red used for
            // the app-level `Shift+<letter>` widget-focus shortcuts.
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
                let active = *v == self.view;
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
            // `o` — open one of the configured web calendars in the
            // browser. See `jump_to_external` for the 0 / 1 / 2+
            // provider routing.
            KeyCode::Char('o') => {
                self.jump_to_external();
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
