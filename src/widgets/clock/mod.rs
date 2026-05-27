use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Datelike, Local, TimeZone, Timelike};
use chrono_tz::Tz;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Alignment, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};
use serde::Deserialize;

use crate::theme::{ColorScheme, Theme};
use crate::ui::{big_digits, decorated_title_line};

use super::{AppContext, EventResult, Widget};

/// User-configurable clock options (loaded from `~/.config/glint/clock.toml`).
#[derive(Debug, Clone, Deserialize)]
pub struct ClockConfig {
    /// IANA timezone name for the primary clock. Defaults to system local time.
    #[serde(default)]
    pub timezone: Option<String>,

    /// Show seconds inside the big block-digit display (e.g. `HH:MM:SS`).
    #[serde(default)]
    pub show_seconds: bool,

    /// Show a small ticking `HH:MM:SS` text line below the big digits.
    #[serde(default = "default_show_seconds_ticker")]
    pub show_seconds_ticker: bool,

    #[serde(default = "default_show_date")]
    pub show_date: bool,

    /// `"12h"` or `"24h"` (alternatively the bare integers `12` / `24` for
    /// backward compatibility). Anything else falls back to 24-hour.
    #[serde(default = "default_hour_format", deserialize_with = "deserialize_hour_format")]
    pub hour_format: u8,

    /// Additional world clocks rendered below the primary display when the
    /// cell is tall enough.
    #[serde(default)]
    pub secondary_timezones: Vec<SecondaryTimezone>,

    /// Big-digit visual style. `"normal"` (default) keeps the current single-
    /// color block digits; the other variants apply a top-to-bottom color
    /// gradient using half-height block rendering. Press `g` while focused
    /// on the clock widget to cycle through them at runtime.
    #[serde(default)]
    pub gradient: big_digits::Gradient,

    /// Per-widget style overrides layered on top of the active app color
    /// scheme. Any role omitted here inherits from the app theme.
    #[serde(default)]
    pub colors: ColorScheme,

    /// Prioritized `Shift+<letter>` focus shortcut preferences. Leave
    /// empty to use the built-in default (`['c', 'l', 'o', 'k']`).
    #[serde(default)]
    pub shortcuts: Vec<char>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SecondaryTimezone {
    pub label: String,
    /// IANA timezone identifier. Accepts the old short `tz` field for
    /// backward compatibility with existing user configs.
    #[serde(alias = "tz")]
    pub timezone: String,
}

fn default_show_seconds_ticker() -> bool {
    true
}
fn default_show_date() -> bool {
    true
}
fn default_hour_format() -> u8 {
    24
}

/// Accept either `"12h"`/`"24h"` strings or the bare integers `12`/`24` so
/// the field reads consistently with other enum-style settings while keeping
/// existing user configs working.
fn deserialize_hour_format<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Str(String),
        Int(u8),
    }
    let n = match Repr::deserialize(deserializer)? {
        Repr::Int(n) => n,
        Repr::Str(s) => match s.trim().to_lowercase().as_str() {
            "12h" | "12" => 12,
            "24h" | "24" => 24,
            other => {
                return Err(D::Error::custom(format!(
                    "unknown hour_format {other:?}, expected \"12h\" or \"24h\""
                )))
            }
        },
    };
    Ok(n)
}

impl Default for ClockConfig {
    fn default() -> Self {
        Self {
            timezone: None,
            show_seconds: false,
            show_seconds_ticker: default_show_seconds_ticker(),
            show_date: default_show_date(),
            hour_format: default_hour_format(),
            secondary_timezones: Vec::new(),
            gradient: big_digits::Gradient::default(),
            colors: ColorScheme::default(),
            shortcuts: Vec::new(),
        }
    }
}

#[derive(Default)]
struct ClockState {
    /// Override pinned by `:time <location>`. When Some, the big-digit display
    /// renders in that timezone and is tinted purple to make the override
    /// state unmistakable.
    transient_tz: Option<(String, Tz)>,
    /// True while a `:time <location>` geocoding request is in flight.
    transient_searching: bool,
    /// Currently active big-digit gradient. Seeded from config at startup; the
    /// user can cycle through variants by pressing `g`.
    gradient: big_digits::Gradient,
}

pub struct ClockWidget {
    id: String,
    instance: String,
    /// Cached `Clock` / `Clock (instance)` label so `display_name()` can
    /// return a `&str` without per-call allocation.
    display_name_cache: String,
    config: ClockConfig,
    tz: Option<Tz>,
    /// Parsed secondary timezones — entries with invalid IANA names get dropped
    /// at construction time and a warning logged.
    secondaries: Vec<(String, Tz)>,
    state: Arc<Mutex<ClockState>>,
    /// App-level theme; kept so live config reloads can rebuild `theme`
    /// from updated `colors` overrides.
    app_theme: Arc<Theme>,
    /// Merged theme (app + widget overrides). Rebuilt on `apply_config`.
    theme: Theme,
    /// Letter assigned by the app for `Shift+<letter>` focus, painted in
    /// the title via `text.shortcut`. `None` = no shortcut claimed.
    shortcut: Option<char>,
    /// Effective shortcut preference list (TOML override or built-in).
    shortcut_prefs: Vec<char>,
}

impl Default for ClockWidget {
    fn default() -> Self {
        Self::with_config(
            "main".to_string(),
            ClockConfig::default(),
            Arc::new(Theme::builtin_defaults()),
        )
    }
}

impl ClockWidget {
    pub fn with_config(instance: String, config: ClockConfig, app_theme: Arc<Theme>) -> Self {
        let tz = config
            .timezone
            .as_deref()
            .and_then(|name| name.parse::<Tz>().ok());
        let mut secondaries = Vec::with_capacity(config.secondary_timezones.len());
        for st in &config.secondary_timezones {
            match st.timezone.parse::<Tz>() {
                Ok(t) => secondaries.push((st.label.clone(), t)),
                Err(_) => {
                    tracing::warn!(label = %st.label, timezone = %st.timezone, "invalid IANA timezone, skipping");
                }
            }
        }
        let state = ClockState {
            gradient: config.gradient,
            ..ClockState::default()
        };
        let theme = app_theme.with_overrides(&config.colors);
        let shortcut_prefs = if config.shortcuts.is_empty() {
            vec!['c', 'l', 'o', 'k']
        } else {
            config.shortcuts.clone()
        };
        let id = if instance == "main" {
            "clock".to_string()
        } else {
            format!("clock@{instance}")
        };
        let display_name_cache = if instance == "main" {
            "Clock".to_string()
        } else {
            format!("Clock ({instance})")
        };
        Self {
            id,
            instance,
            display_name_cache,
            config,
            tz,
            secondaries,
            state: Arc::new(Mutex::new(state)),
            app_theme,
            theme,
            shortcut: None,
            shortcut_prefs,
        }
    }

    fn snapshot_transient(&self) -> (Option<(String, Tz)>, bool) {
        let st = self.state.lock().expect("clock state poisoned");
        (st.transient_tz.clone(), st.transient_searching)
    }

    /// Effective primary timezone — transient override beats configured tz
    /// beats system local.
    fn effective_tz(&self) -> Option<Tz> {
        self.state
            .lock()
            .expect("clock state poisoned")
            .transient_tz
            .as_ref()
            .map(|(_, tz)| *tz)
            .or(self.tz)
    }

    fn lookup_location(&self, query: &str) {
        {
            let mut st = self.state.lock().expect("clock state poisoned");
            st.transient_searching = true;
        }
        let state = self.state.clone();
        let query = query.to_string();
        tokio::spawn(async move {
            let result = crate::geolocation::by_name(&query).await;
            let mut st = state.lock().expect("clock state poisoned");
            st.transient_searching = false;
            match result {
                Ok(loc) => {
                    let Some(tz_name) = loc.timezone.as_deref() else {
                        tracing::warn!(query = %query, "geocoding succeeded but returned no timezone");
                        return;
                    };
                    match tz_name.parse::<Tz>() {
                        Ok(tz) => {
                            st.transient_tz = Some((loc.label.clone(), tz));
                        }
                        Err(_) => {
                            tracing::warn!(query = %query, tz = %tz_name, "unrecognized IANA timezone");
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(query = %query, error = %err, "clock geocoding failed");
                }
            }
        });
    }

    fn clear_transient(&self) {
        let mut st = self.state.lock().expect("clock state poisoned");
        st.transient_tz = None;
    }

    /// Returns (HH:MM[:SS], AM/PM, date) for the effective primary timezone.
    fn render_strings(&self, now_utc: DateTime<chrono::Utc>) -> (String, String, String) {
        match self.effective_tz() {
            Some(tz) => self.format_parts(now_utc.with_timezone(&tz)),
            None => self.format_parts(now_utc.with_timezone(&Local)),
        }
    }

    fn format_parts<T: TimeZone>(&self, dt: DateTime<T>) -> (String, String, String)
    where
        T::Offset: std::fmt::Display,
    {
        let (hour_disp, ampm) = if self.config.hour_format == 12 {
            let h = dt.hour();
            let (h12, suffix) = match h {
                0 => (12, "AM"),
                1..=11 => (h, "AM"),
                12 => (12, "PM"),
                _ => (h - 12, "PM"),
            };
            (h12, suffix.to_string())
        } else {
            (dt.hour(), String::new())
        };

        let time = if self.config.show_seconds {
            format!("{:02}:{:02}:{:02}", hour_disp, dt.minute(), dt.second())
        } else {
            format!("{:02}:{:02}", hour_disp, dt.minute())
        };

        let date = if self.config.show_date {
            format!(
                "{} {} {}, {}",
                weekday_name(dt.weekday()),
                month_name(dt.month()),
                dt.day(),
                dt.year()
            )
        } else {
            String::new()
        };

        (time, ampm, date)
    }

    fn ticker_string(&self, now_utc: DateTime<chrono::Utc>) -> String {
        match self.effective_tz() {
            Some(tz) => format_ticker(now_utc.with_timezone(&tz), self.config.hour_format),
            None => format_ticker(now_utc.with_timezone(&Local), self.config.hour_format),
        }
    }

    /// Returns (label, "HH:MM Wkd Mon DD") pairs for the World Clocks block.
    /// Primary timezone leads, then any configured secondaries. Each entry
    /// carries its own local date so the user can tell when a clock is on a
    /// different calendar day than local time without having to do timezone
    /// arithmetic in their head.
    fn world_clock_entries(&self) -> Vec<(String, String)> {
        let now = chrono::Utc::now();
        let mut out: Vec<(String, String)> = Vec::with_capacity(self.secondaries.len() + 2);
        let transient = self.state.lock().expect("clock state poisoned").transient_tz.clone();

        // When a `:time <location>` override is active the big-digit display
        // is showing that override, so pin Local to the top of the World
        // Clocks list — otherwise the user has no easy way to see their
        // actual local time at a glance.
        if transient.is_some() {
            let local_now = now.with_timezone(&Local);
            out.push(("Local".to_string(), format_clock_entry(&local_now)));
        }

        let (primary_label, primary_str) = match transient {
            Some((label, tz)) => {
                let t = now.with_timezone(&tz);
                (label, format_clock_entry(&t))
            }
            None => match self.tz {
                Some(tz) => {
                    let t = now.with_timezone(&tz);
                    (city_from_tz_name(tz.name()), format_clock_entry(&t))
                }
                None => {
                    let t = now.with_timezone(&Local);
                    ("Local".to_string(), format_clock_entry(&t))
                }
            },
        };
        out.push((primary_label, primary_str));
        for (label, tz) in &self.secondaries {
            let t = now.with_timezone(tz);
            out.push((label.clone(), format_clock_entry(&t)));
        }
        out
    }
}

fn format_clock_entry<T: TimeZone>(t: &DateTime<T>) -> String
where
    T::Offset: std::fmt::Display,
{
    format!(
        "{} {:02}:{:02} {} {} {}",
        day_night_icon(t.hour()),
        t.hour(),
        t.minute(),
        weekday_name(t.weekday()),
        month_name(t.month()),
        t.day()
    )
}

/// Simple day/night marker keyed off local hour-of-day. Use 06:00–17:59 as
/// "day"; outside that window is "night". Not astronomically accurate but
/// good enough as a glance signal alongside the time.
fn day_night_icon(hour: u32) -> &'static str {
    if (6..=17).contains(&hour) {
        "☀"
    } else {
        "☾"
    }
}

/// Convert an IANA timezone name like "America/Vancouver" into a friendly
/// label ("Vancouver"). Underscores become spaces.
fn city_from_tz_name(name: &str) -> String {
    name.rsplit('/').next().unwrap_or(name).replace('_', " ")
}

fn format_ticker<T: TimeZone>(t: DateTime<T>, hour_format: u8) -> String
where
    T::Offset: std::fmt::Display,
{
    let hour = t.hour();
    if hour_format == 12 {
        let (h12, suffix) = match hour {
            0 => (12, "AM"),
            1..=11 => (hour, "AM"),
            12 => (12, "PM"),
            _ => (hour - 12, "PM"),
        };
        format!("{:02}:{:02}:{:02} {}", h12, t.minute(), t.second(), suffix)
    } else {
        format!("{:02}:{:02}:{:02}", hour, t.minute(), t.second())
    }
}

fn weekday_name(w: chrono::Weekday) -> &'static str {
    use chrono::Weekday::*;
    match w {
        Mon => "Mon",
        Tue => "Tue",
        Wed => "Wed",
        Thu => "Thu",
        Fri => "Fri",
        Sat => "Sat",
        Sun => "Sun",
    }
}

fn month_name(m: u32) -> &'static str {
    match m {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => "???",
    }
}

#[async_trait]
impl Widget for ClockWidget {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> &str {
        "clock"
    }

    fn instance(&self) -> &str {
        &self.instance
    }

    fn display_name(&self) -> &str {
        &self.display_name_cache
    }

    async fn update(&mut self, _ctx: &AppContext) -> Result<()> {
        Ok(())
    }

    fn render(&self, frame: &mut Frame, area: Rect, focused: bool) {
        let (transient, searching) = self.snapshot_transient();
        let base = if self.instance == "main" {
            "Clock".to_string()
        } else {
            format!("Clock ({})", self.instance)
        };
        let title_base = if let Some((label, _)) = &transient {
            format!("{base} — {label} (lookup)")
        } else if searching {
            format!("{base} — looking up…")
        } else {
            match &self.tz {
                Some(tz) => format!("{base} — {tz}"),
                None => base,
            }
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(self.theme.border_style(focused))
            .title(decorated_title_line(
                focused,
                &title_base,
                self.shortcut,
                self.theme.widget_title,
                self.theme.text_shortcut,
            ));

        let now = chrono::Utc::now();
        let (time, ampm, date) = self.render_strings(now);

        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Big-digit color seed: `text.focused` from the active scheme by
        // default; `text.selected` while a `:time <location>` override is
        // active so the user can't miss that they're not on home base. The
        // gradient (subtle / hue_shift / glow / fade) derives its full
        // 10-stop palette from this seed, so the digits restyle on
        // `:scheme` regardless of the gradient mode chosen.
        let big_style = if transient.is_some() {
            self.theme.text_selected
        } else {
            self.theme.text_focused
        };
        let gradient = self
            .state
            .lock()
            .expect("clock state poisoned")
            .gradient;
        let big_lines = big_digits::render_styled(&time, gradient, big_style);

        let mut lines: Vec<Line<'_>> = Vec::new();
        // Top padding so the big digits don't kiss the border.
        lines.push(Line::from(""));
        for line in big_lines {
            lines.push(line);
        }

        if self.config.show_seconds_ticker {
            // Blank line between the big-digit clock and the HH:MM:SS ticker
            // beneath it — gives the ticker some breathing room from the
            // glyphs above.
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                self.ticker_string(now),
                self.theme.text_dim,
            )));
        }

        if !ampm.is_empty() {
            lines.push(Line::from(Span::styled(
                ampm,
                self.theme.text_dim,
            )));
        }
        if !date.is_empty() {
            // No blank line above the date — the ticker and the day-date sit
            // together as one block of secondary info beneath the clock.
            lines.push(Line::from(date));
        }

        // World clocks block — only shown if there's room for at least the
        // separator line + one entry. Primary timezone is listed first so the
        // user can see the local time alongside the rest of the world.
        let clocks = self.world_clock_entries();
        if !clocks.is_empty() {
            let extra_needed = 2 + clocks.len();
            if (lines.len() + extra_needed) as u16 <= inner.height {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "── World Clocks ──",
                    self.theme.text_dim,
                )));
                let max_label = clocks.iter().map(|(l, _)| l.chars().count()).max().unwrap_or(0);
                // Local — and whichever entry the big-digit display is showing
                // — get colored so the user can see at a glance which row
                // matches the big clock. Local picks up `text.focused` from
                // the active scheme; the `:time` override row picks up
                // `text.selected` so it's distinct from Local but still
                // theme-driven.
                let local_highlight_style = self.theme.text_focused;
                let override_highlight_style = self.theme.text_selected;
                let has_override = transient.is_some();
                for (idx, (label, time_str)) in clocks.iter().enumerate() {
                    let style = if has_override {
                        // idx 0 = Local (prepended in world_clock_entries),
                        // idx 1 = the override entry, rest = secondaries.
                        match idx {
                            0 => local_highlight_style,
                            1 => override_highlight_style,
                            _ => Style::default(),
                        }
                    } else if idx == 0 {
                        // No override — the first entry is whatever the big
                        // digits are showing (Local by default, or the
                        // configured `self.tz` if set), so match the focused
                        // big-digit color from the scheme.
                        local_highlight_style
                    } else {
                        Style::default()
                    };
                    let line = format!(
                        "{:<width$}  {}",
                        label,
                        time_str,
                        width = max_label
                    );
                    lines.push(Line::from(Span::styled(line, style)));
                }
            }
        }

        // When a `:time <city>` override is active, append a footer hint
        // pinned to the bottom of the cell so the user has an obvious
        // escape route back to Local time.
        if transient.is_some() {
            let hint = Line::from(Span::styled(
                "x: revert to Local",
                self.theme.text_dim,
            ));
            let body = Paragraph::new(lines).alignment(Alignment::Center);
            let body_h = inner.height.saturating_sub(1);
            let body_area = Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: body_h,
            };
            let hint_area = Rect {
                x: inner.x,
                y: inner.y + body_h,
                width: inner.width,
                height: 1,
            };
            frame.render_widget(body, body_area);
            frame.render_widget(Paragraph::new(hint).alignment(Alignment::Center), hint_area);
        } else {
            let body = Paragraph::new(lines).alignment(Alignment::Center);
            frame.render_widget(body, inner);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        match key.code {
            KeyCode::Char('x') => {
                self.clear_transient();
                EventResult::Handled
            }
            KeyCode::Char('g') => {
                let mut st = self.state.lock().expect("clock state poisoned");
                st.gradient = st.gradient.next();
                EventResult::Handled
            }
            _ => EventResult::Ignored,
        }
    }

    fn handle_command(&mut self, cmd: &str, args: &[&str]) -> Result<bool> {
        match cmd {
            "time" | "t" | "clock" => {
                if args.is_empty() {
                    anyhow::bail!("usage: :time <city or country>");
                }
                let query = args.join(" ");
                self.lookup_location(&query);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn keybindings(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("g", "cycle digit gradient style"),
            ("x", "clear :time lookup (return to local time)"),
            (":time <city>", "switch primary clock to that location"),
            (":clock <city>", "alias for :time"),
        ]
    }

    fn config(&self) -> serde_json::Value {
        serde_json::json!({
            "timezone": self.config.timezone,
            "show_seconds": self.config.show_seconds,
            "show_seconds_ticker": self.config.show_seconds_ticker,
            "show_date": self.config.show_date,
            "hour_format": self.config.hour_format,
            "secondary_timezones": self.config.secondary_timezones.iter().map(|s| {
                serde_json::json!({"label": s.label, "timezone": s.timezone})
            }).collect::<Vec<_>>(),
            "gradient": self.config.gradient.label(),
        })
    }

    fn apply_config(&mut self, config: serde_json::Value) -> Result<()> {
        let new_config: ClockConfig =
            serde_json::from_value(config).context("invalid clock config payload")?;
        let app_theme = self.app_theme.clone();
        let instance = self.instance.clone();
        *self = Self::with_config(instance, new_config, app_theme);
        Ok(())
    }

    fn set_app_theme(&mut self, theme: Arc<Theme>) {
        self.theme = theme.with_overrides(&self.config.colors);
        self.app_theme = theme;
    }

    fn shortcut_preferences(&self) -> &[char] {
        &self.shortcut_prefs
    }

    fn set_shortcut(&mut self, shortcut: Option<char>) {
        self.shortcut = shortcut;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn build_widget(cfg: ClockConfig) -> ClockWidget {
        ClockWidget::with_config("main".to_string(), cfg, Arc::new(Theme::builtin_defaults()))
    }

    #[test]
    fn twelve_hour_format_renders_midnight_as_12_am() {
        let cfg = ClockConfig {
            timezone: Some("UTC".into()),
            show_seconds: false,
            show_seconds_ticker: false,
            show_date: false,
            hour_format: 12,
            secondary_timezones: Vec::new(),
            gradient: big_digits::Gradient::default(),
            colors: ColorScheme::default(),
            shortcuts: Vec::new(),
        };
        let widget = build_widget(cfg);
        let midnight_utc = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap();
        let (time, ampm, date) = widget.render_strings(midnight_utc);
        assert_eq!(time, "12:00");
        assert_eq!(ampm, "AM");
        assert!(date.is_empty());
    }

    #[test]
    fn twenty_four_hour_format_zero_pads() {
        let cfg = ClockConfig {
            timezone: Some("UTC".into()),
            show_seconds: true,
            show_seconds_ticker: false,
            show_date: false,
            hour_format: 24,
            secondary_timezones: Vec::new(),
            gradient: big_digits::Gradient::default(),
            colors: ColorScheme::default(),
            shortcuts: Vec::new(),
        };
        let widget = build_widget(cfg);
        let t = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 9, 5, 7).unwrap();
        let (time, ampm, _) = widget.render_strings(t);
        assert_eq!(time, "09:05:07");
        assert_eq!(ampm, "");
    }

    #[test]
    fn ticker_includes_seconds_in_primary_timezone() {
        let cfg = ClockConfig {
            timezone: Some("UTC".into()),
            show_seconds: false,
            show_seconds_ticker: true,
            show_date: false,
            hour_format: 24,
            secondary_timezones: Vec::new(),
            gradient: big_digits::Gradient::default(),
            colors: ColorScheme::default(),
            shortcuts: Vec::new(),
        };
        let w = build_widget(cfg);
        let t = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 9, 5, 42).unwrap();
        assert_eq!(w.ticker_string(t), "09:05:42");
    }

    #[test]
    fn city_from_tz_name_strips_region_and_underscores() {
        assert_eq!(city_from_tz_name("America/New_York"), "New York");
        assert_eq!(city_from_tz_name("Europe/London"), "London");
        assert_eq!(city_from_tz_name("Asia/Tokyo"), "Tokyo");
        assert_eq!(city_from_tz_name("UTC"), "UTC");
    }

    #[test]
    fn world_clock_entries_pin_local_during_time_override() {
        use chrono_tz::Tz;
        let cfg = ClockConfig {
            secondary_timezones: vec![SecondaryTimezone {
                label: "Tokyo".into(),
                timezone: "Asia/Tokyo".into(),
            }],
            ..ClockConfig::default()
        };
        let w = build_widget(cfg);
        {
            let mut st = w.state.lock().unwrap();
            st.transient_tz = Some(("Berlin".into(), "Europe/Berlin".parse::<Tz>().unwrap()));
        }
        let entries = w.world_clock_entries();
        assert_eq!(entries.len(), 3, "Local + override + 1 secondary");
        assert_eq!(entries[0].0, "Local");
        assert_eq!(entries[1].0, "Berlin");
        assert_eq!(entries[2].0, "Tokyo");
    }

    #[test]
    fn world_clock_entries_lead_with_primary() {
        let cfg = ClockConfig {
            timezone: Some("America/Vancouver".into()),
            secondary_timezones: vec![SecondaryTimezone {
                label: "Tokyo".into(),
                timezone: "Asia/Tokyo".into(),
            }],
            ..ClockConfig::default()
        };
        let w = build_widget(cfg);
        let entries = w.world_clock_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "Vancouver");
        assert_eq!(entries[1].0, "Tokyo");
    }

    #[test]
    fn world_clock_entries_include_icon_time_and_date() {
        let cfg = ClockConfig {
            timezone: Some("America/Vancouver".into()),
            secondary_timezones: vec![SecondaryTimezone {
                label: "Tokyo".into(),
                timezone: "Asia/Tokyo".into(),
            }],
            ..ClockConfig::default()
        };
        let w = build_widget(cfg);
        let entries = w.world_clock_entries();
        for (_label, formatted) in &entries {
            // Format: "<icon> HH:MM Wkd Mon DD"
            let parts: Vec<&str> = formatted.split_whitespace().collect();
            assert_eq!(parts.len(), 5, "unexpected format: {formatted:?}");
            assert!(parts[0] == "☀" || parts[0] == "☾");
            // HH:MM
            assert_eq!(parts[1].chars().nth(2), Some(':'));
            // Weekday abbreviation
            assert!(
                ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].contains(&parts[2]),
                "unexpected weekday: {:?}",
                parts[2]
            );
            // Month abbreviation
            assert!(
                ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]
                    .contains(&parts[3]),
                "unexpected month: {:?}",
                parts[3]
            );
            // Day-of-month is a positive integer
            assert!(parts[4].parse::<u32>().is_ok());
        }
    }

    #[test]
    fn day_night_icon_boundaries() {
        assert_eq!(day_night_icon(5), "☾");
        assert_eq!(day_night_icon(6), "☀");
        assert_eq!(day_night_icon(12), "☀");
        assert_eq!(day_night_icon(17), "☀");
        assert_eq!(day_night_icon(18), "☾");
        assert_eq!(day_night_icon(23), "☾");
        assert_eq!(day_night_icon(0), "☾");
    }

    #[test]
    fn invalid_secondary_timezones_are_dropped() {
        let cfg = ClockConfig {
            secondary_timezones: vec![
                SecondaryTimezone {
                    label: "New York".into(),
                    timezone: "America/New_York".into(),
                },
                SecondaryTimezone {
                    label: "Bogus".into(),
                    timezone: "Not/A_Real_TZ".into(),
                },
            ],
            ..ClockConfig::default()
        };
        let w = build_widget(cfg);
        assert_eq!(w.secondaries.len(), 1);
        assert_eq!(w.secondaries[0].0, "New York");
    }
}
