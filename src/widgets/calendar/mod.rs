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
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
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
        let today_events: Vec<&Event> =
            events.iter().filter(|e| e.on_date(self.anchor)).collect();

        // Split the inner area: tear-off-sheet header on top (centered), the
        // hint banner + event list below (left-aligned). 8 rows fits the
        // padding + weekday/month line + 5 block-digit rows + bottom padding;
        // shrink gracefully when the cell is shorter than that.
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
            weekday_short(self.anchor.weekday()),
            month_long(self.anchor.month()),
        );
        let mut header_lines: Vec<Line<'_>> = vec![
            Line::from(""),
            Line::from(Span::styled(
                header_text,
                Style::default().add_modifier(Modifier::DIM),
            )),
        ];
        for row in big_digits::render(&self.anchor.day().to_string()) {
            header_lines.push(Line::from(Span::styled(
                row,
                Style::default()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD),
            )));
        }
        frame.render_widget(
            Paragraph::new(header_lines).alignment(Alignment::Center),
            header_area,
        );

        let mut lines: Vec<Line<'_>> = Vec::new();
        if let Some(hint) = &self.auth_hint {
            lines.push(Line::from(Span::styled(
                format!("⚠ {hint}"),
                Style::default().fg(Color::Yellow),
            )));
            lines.push(Line::from(""));
        }
        if today_events.is_empty() {
            lines.push(Line::from(Span::styled(
                "No events.",
                Style::default().add_modifier(Modifier::DIM),
            )));
        } else {
            for e in today_events {
                let color = color_for_calendar(&e.calendar);
                let time_label = if e.all_day {
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
                lines.push(Line::from(vec![
                    Span::styled(format!("{time_label}  "), Style::default().fg(Color::Gray)),
                    Span::styled(
                        e.title.clone(),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                ]));
                if let Some(loc) = &e.location {
                    lines.push(Line::from(Span::styled(
                        format!("              {loc}"),
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
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(vec![Constraint::Ratio(1, 7); 7])
            .split(area);
        let today = Local::now().date_naive();
        for (i, col_area) in cols.iter().enumerate() {
            let day = s + ChronoDuration::days(i as i64);
            let is_today = day == today;
            // Stack weekday on top, date underneath — keeps both visible even
            // in narrow columns where `Mon 18` would otherwise truncate.
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
            // Top padding so headers don't kiss the border.
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
                for e in day_events {
                    let color = color_for_calendar(&e.calendar);
                    let prefix = if e.all_day {
                        "•".to_string()
                    } else {
                        format!("{:02}:{:02}", e.start.hour(), e.start.minute())
                    };
                    lines.push(Line::from(Span::styled(
                        format!("{prefix} {}", truncate(&e.title, 14)),
                        Style::default().fg(color),
                    )));
                }
            }
            frame.render_widget(
                Paragraph::new(lines).alignment(Alignment::Center),
                *col_area,
            );
        }
    }

    fn render_month(&self, frame: &mut Frame, area: Rect, events: &[Event]) {
        let first = start_of_month(self.anchor);
        // First displayed cell is the Sunday on/before the 1st.
        let grid_start = start_of_week(first);
        let today = Local::now().date_naive();

        // Day-of-month header row
        let header_line = Line::from(
            ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"]
                .iter()
                .map(|s| Span::styled(format!("{s:^5}"), Style::default().add_modifier(Modifier::BOLD)))
                .collect::<Vec<_>>(),
        );

        let mut lines: Vec<Line<'_>> = Vec::with_capacity(9);
        // Top padding so the weekday header doesn't kiss the border.
        lines.push(Line::from(""));
        lines.push(header_line);

        // 6 weeks always fit any month.
        for week in 0..6 {
            let mut spans: Vec<Span<'_>> = Vec::with_capacity(7);
            for dow in 0..7 {
                let date = grid_start + ChronoDuration::days(week * 7 + dow);
                let in_month = date.month() == self.anchor.month();
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
                    Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                spans.push(Span::styled(format!("{cell:<5}"), style));
            }
            lines.push(Line::from(spans));
        }

        frame.render_widget(Paragraph::new(lines), area);
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

        // Footer hint at bottom of the cell.
        if inner.height >= 2 {
            let hint_area = Rect {
                x: inner.x,
                y: inner.y + inner.height - 1,
                width: inner.width,
                height: 1,
            };
            let hint = Paragraph::new(Line::from(Span::styled(
                "d/w/m view · ←/→ nav · t today",
                Style::default().add_modifier(Modifier::DIM),
            )))
            .alignment(Alignment::Right);
            frame.render_widget(hint, hint_area);
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

    fn handle_command(&mut self, _cmd: &str, _args: &[&str]) -> Result<bool> {
        Ok(false)
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
    fn truncate_appends_ellipsis_when_too_long() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abc", 4), "abc");
    }
}
