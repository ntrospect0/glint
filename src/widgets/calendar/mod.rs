pub mod google;
pub mod local;
pub mod provider;

use std::{
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
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use serde::{Deserialize, Serialize};

use super::{AppContext, EventResult, Widget};

use google::GoogleCalendarProvider;
use local::{LocalCalendarFile, LocalCalendarProvider};
use provider::{CalendarProvider, Event};

use crate::auth::google::{store::GoogleToken, OAuthClientConfig};
use crate::ui::{big_digits, decorate_title, focus_border_style};

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
}

#[derive(Debug, Clone, Deserialize)]
pub struct CalendarConfig {
    #[serde(default)]
    pub default_view: CalendarView,

    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    #[serde(default)]
    pub provider: ProviderKind,

    /// Google calendar IDs to fetch from when `provider = "google"`. Use
    /// "primary" for the user's main calendar. Ignored for local provider.
    #[serde(default)]
    pub calendar_ids: Vec<String>,

    /// Local events to seed the provider. Kept here so `config::load_widget_toml`
    /// returns the full file in one shot.
    #[serde(default)]
    pub events: Vec<local::RawEvent>,
}

fn default_poll_interval() -> u64 {
    60
}

impl Default for CalendarConfig {
    fn default() -> Self {
        Self {
            default_view: CalendarView::default(),
            poll_interval_secs: default_poll_interval(),
            provider: ProviderKind::default(),
            calendar_ids: Vec::new(),
            events: Vec::new(),
        }
    }
}

#[derive(Default)]
struct CalendarState {
    events: Vec<Event>,
    last_error: Option<String>,
    last_attempt: Option<Instant>,
    inflight: bool,
}

pub struct CalendarWidget {
    id: String,
    view: CalendarView,
    /// Anchor date used by all three views. For Day, it's the day shown.
    /// For Week, the week containing it. For Month, the month containing it.
    anchor: NaiveDate,
    provider: Arc<dyn CalendarProvider>,
    /// Source of the active provider — surfaced in the title and footer so the
    /// user can tell at a glance whether they're on local or Google data.
    provider_kind: ProviderKind,
    /// When Google was requested but failed to initialize (no client config or
    /// no token), we keep the user-visible explanation so the widget can show
    /// "Run `glint --auth google`" instead of silently using the local seed.
    auth_hint: Option<String>,
    state: Arc<Mutex<CalendarState>>,
    poll_interval: Duration,
}

impl CalendarWidget {
    pub fn with_config(config: CalendarConfig) -> Self {
        let (provider, kind, auth_hint) = build_provider(&config);
        Self {
            id: "calendar".into(),
            view: config.default_view,
            anchor: Local::now().date_naive(),
            provider,
            provider_kind: kind,
            auth_hint,
            state: Arc::new(Mutex::new(CalendarState::default())),
            poll_interval: Duration::from_secs(config.poll_interval_secs.max(15)),
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
            CalendarView::Day => (self.anchor, self.anchor + ChronoDuration::days(1)),
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
        tokio::spawn(async move {
            let result = provider.fetch_range(start, end).await;
            let mut st = state.lock().expect("calendar state poisoned");
            st.inflight = false;
            match result {
                Ok(events) => {
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

    fn snapshot_events(&self) -> Vec<Event> {
        let st = self.state.lock().expect("calendar state poisoned");
        st.events.clone()
    }

    fn nav_step(&self) -> ChronoDuration {
        match self.view {
            CalendarView::Day => ChronoDuration::days(1),
            CalendarView::Week => ChronoDuration::days(7),
            CalendarView::Month => ChronoDuration::days(30),
        }
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

/// Returns (provider, effective_kind, auth_hint). When Google is requested but
/// the client/token isn't on disk, falls back to the local provider and stashes
/// a hint string the widget can surface.
fn build_provider(
    config: &CalendarConfig,
) -> (Arc<dyn CalendarProvider>, ProviderKind, Option<String>) {
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

    if config.provider != ProviderKind::Google {
        return (local, ProviderKind::Local, None);
    }

    let client = match OAuthClientConfig::load() {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(error = %err, "google_oauth_client.toml missing or invalid");
            return (local, ProviderKind::Local, Some("Drop google_oauth_client.toml in ~/.config/glint/credentials/".into()));
        }
    };
    let token = match GoogleToken::load() {
        Ok(Some(t)) => t,
        Ok(None) => {
            return (local, ProviderKind::Local, Some("Run `glint --auth google` to connect Google Calendar".into()));
        }
        Err(err) => {
            tracing::warn!(error = %err, "failed to load saved Google token");
            return (local, ProviderKind::Local, Some(format!("Token unreadable: {err}")));
        }
    };
    match GoogleCalendarProvider::new(client, token, config.calendar_ids.clone()) {
        Ok(p) => (Arc::new(p), ProviderKind::Google, None),
        Err(err) => {
            tracing::warn!(error = %err, "failed to build Google calendar provider");
            (local, ProviderKind::Local, Some(format!("Google init failed: {err}")))
        }
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
fn render_month_grid(
    frame: &mut Frame,
    area: Rect,
    year: i32,
    month: u32,
    is_anchor: bool,
    events: &[Event],
) {
    let Some(first) = NaiveDate::from_ymd_opt(year, month, 1) else {
        return;
    };
    let grid_start = start_of_week(first);
    let today = Local::now().date_naive();

    let month_header_style = if is_anchor {
        Style::default()
            .fg(Color::LightYellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::DIM)
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
            let style = if !in_month {
                Style::default().add_modifier(Modifier::DIM)
            } else if has_events {
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
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

/// Stable hash a calendar name to one of a handful of pleasant colors.
fn color_for_calendar(name: &str) -> Color {
    const PALETTE: [Color; 6] = [
        Color::LightBlue,
        Color::LightGreen,
        Color::LightYellow,
        Color::LightMagenta,
        Color::LightCyan,
        Color::LightRed,
    ];
    let mut hash: u32 = 5381;
    for b in name.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(u32::from(b));
    }
    PALETTE[(hash as usize) % PALETTE.len()]
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
        let source = match self.provider_kind {
            ProviderKind::Google => "google",
            ProviderKind::Local => "local",
        };
        match self.view {
            CalendarView::Day => format!(
                "Calendar [{source}] — {} {} {}, {}",
                weekday_short(self.anchor.weekday()),
                month_long(self.anchor.month()),
                self.anchor.day(),
                self.anchor.year()
            ),
            CalendarView::Week => {
                let s = start_of_week(self.anchor);
                let e = s + ChronoDuration::days(6);
                format!(
                    "Calendar [{source}] — week of {} {}–{}",
                    month_long(s.month()),
                    s.day(),
                    e.day()
                )
            }
            CalendarView::Month => format!(
                "Calendar [{source}] — {} {}",
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
            self.render_day_column(frame, cols[0], self.anchor, true, events);
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
                            Style::default().add_modifier(Modifier::DIM),
                        ))
                    })
                    .collect();
                frame.render_widget(Paragraph::new(sep_lines), sep_area);
            }
            let next = self.anchor + ChronoDuration::days(1);
            self.render_day_column(frame, cols[2], next, false, events);
        } else {
            self.render_day_column(frame, area, self.anchor, true, events);
        }
    }

    fn render_day_column(
        &self,
        frame: &mut Frame,
        area: Rect,
        date: NaiveDate,
        is_anchor: bool,
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
        let body_area = Rect {
            x: area.x,
            y: area.y + header_height,
            width: area.width,
            height: area.height.saturating_sub(header_height),
        };

        let header_text = format!(
            "{} · {}",
            weekday_short(date.weekday()),
            month_long(date.month()),
        );
        let date_style = if is_anchor {
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let mut header_lines: Vec<Line<'_>> = vec![
            Line::from(""),
            Line::from(Span::styled(
                header_text,
                Style::default().add_modifier(Modifier::DIM),
            )),
        ];
        for row in big_digits::render(&date.day().to_string()) {
            header_lines.push(Line::from(Span::styled(row, date_style)));
        }
        frame.render_widget(
            Paragraph::new(header_lines).alignment(Alignment::Center),
            header_area,
        );

        let mut lines: Vec<Line<'_>> = Vec::new();
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
        if day_events.is_empty() {
            lines.push(Line::from(Span::styled(
                "No events.",
                Style::default().add_modifier(Modifier::DIM),
            )));
        } else {
            // Widest time label is "HH:MM–HH:MM" (11 chars). Pad every label
            // (including "all day") to that width so every title starts at
            // the same column.
            const TIME_COL_WIDTH: usize = 11;
            const TITLE_GAP: usize = 2;
            let cont_indent = " ".repeat(TIME_COL_WIDTH + TITLE_GAP);

            for e in &day_events {
                let color = color_for_calendar(&e.calendar);
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
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{padded_time}{:gap$}", "", gap = TITLE_GAP),
                        Style::default().fg(Color::Gray),
                    ),
                    Span::styled(
                        e.title.clone(),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                ]));
                if let Some(loc) = &e.location {
                    lines.push(Line::from(Span::styled(
                        format!("{cont_indent}{loc}"),
                        Style::default().add_modifier(Modifier::DIM),
                    )));
                }
            }
        }
        let body = Paragraph::new(lines);
        frame.render_widget(body, body_area);
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
                        Style::default().add_modifier(Modifier::DIM),
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
                    Style::default().add_modifier(Modifier::DIM),
                )));
            } else {
                let wrap_width = col_area.width.saturating_sub(1) as usize;
                for e in day_events {
                    let color = color_for_calendar(&e.calendar);
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

        let constraints: Vec<Constraint> =
            (0..months.len()).map(|_| Constraint::Ratio(1, months.len() as u32)).collect();
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(constraints)
            .split(area);

        for ((y, m), col_area) in months.iter().zip(cols.iter()) {
            let is_anchor = (*y, *m) == (anchor_y, anchor_m);
            render_month_grid(frame, *col_area, *y, *m, is_anchor, events);
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

    fn display_name(&self) -> &str {
        "Calendar"
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
                decorate_title(focused, &self.title_for_header()),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let events = self.snapshot_events();
        match self.view {
            CalendarView::Day => self.render_day(frame, inner, &events),
            CalendarView::Week => self.render_week(frame, inner, &events),
            CalendarView::Month => self.render_month(frame, inner, &events),
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
            spans.push(Span::styled(
                "[Today]",
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(" "));
            for (v, label) in VIEW_TABS {
                let active = *v == self.view;
                let style = if active {
                    Style::default()
                        .fg(Color::LightYellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().add_modifier(Modifier::DIM)
                };
                spans.push(Span::styled(format!("[{label}]"), style));
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(
                "  ←/→ nav",
                Style::default().add_modifier(Modifier::DIM),
            ));
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
                self.mark_dirty();
                EventResult::Handled
            }
            KeyCode::Char('w') => {
                self.view = CalendarView::Week;
                self.mark_dirty();
                EventResult::Handled
            }
            KeyCode::Char('m') => {
                self.view = CalendarView::Month;
                self.mark_dirty();
                EventResult::Handled
            }
            KeyCode::Char('t') => {
                self.anchor = Local::now().date_naive();
                self.mark_dirty();
                EventResult::Handled
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.anchor -= step;
                self.mark_dirty();
                EventResult::Handled
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.anchor += step;
                self.mark_dirty();
                EventResult::Handled
            }
            _ => EventResult::Ignored,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, area: Rect) -> EventResult {
        // Scroll wheel: same effect as ←/→ navigation in the current view.
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                let step = self.nav_step();
                self.anchor -= step;
                self.mark_dirty();
                return EventResult::Handled;
            }
            MouseEventKind::ScrollDown => {
                let step = self.nav_step();
                self.anchor += step;
                self.mark_dirty();
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

        // Day-grid clicks: which date did the user pick? Always switches to
        // Day view so the events for that date come up immediately.
        match self.view {
            CalendarView::Week => {
                if let Some(date) = self.week_day_at(mouse.column, mouse.row, inner) {
                    self.anchor = date;
                    self.view = CalendarView::Day;
                    self.mark_dirty();
                    return EventResult::Handled;
                }
            }
            CalendarView::Month => {
                if let Some(date) = self.month_day_at(mouse.column, mouse.row, inner) {
                    self.anchor = date;
                    self.view = CalendarView::Day;
                    self.mark_dirty();
                    return EventResult::Handled;
                }
            }
            CalendarView::Day => {
                // Two-column day view: clicking the right preview column
                // promotes that day to the new anchor.
                if inner.width >= 50 && mouse.column >= inner.x + inner.width / 2 {
                    self.anchor += ChronoDuration::days(1);
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
            ("t", "jump to today"),
            ("click day", "navigate to that day (week/month view)"),
            ("click tab", "switch view / today"),
        ]
    }

    fn config(&self) -> serde_json::Value {
        serde_json::json!({
            "default_view": self.view,
            "poll_interval_secs": self.poll_interval.as_secs(),
            "provider": self.provider_kind,
        })
    }

    fn apply_config(&mut self, config: serde_json::Value) -> Result<()> {
        let new_config: CalendarConfig =
            serde_json::from_value(config).context("invalid calendar config payload")?;
        *self = Self::with_config(new_config);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn color_for_calendar_is_stable() {
        let a = color_for_calendar("work");
        let b = color_for_calendar("work");
        assert_eq!(a, b);
    }

    #[test]
    fn default_view_is_day_and_widget_starts_today() {
        let w = CalendarWidget::with_config(CalendarConfig::default());
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
        let mut w = CalendarWidget::with_config(cfg);
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
        let mut w = CalendarWidget::with_config(cfg);
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
}
