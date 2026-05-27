use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Datelike, Local, TimeZone, Timelike};
use chrono_tz::Tz;
use crossterm::event::KeyEvent;
use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use serde::Deserialize;

use crate::ui::{big_digits, decorate_title, focus_border_style};

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
        }
    }
}

pub struct ClockWidget {
    id: String,
    config: ClockConfig,
    tz: Option<Tz>,
    /// Parsed secondary timezones — entries with invalid IANA names get dropped
    /// at construction time and a warning logged.
    secondaries: Vec<(String, Tz)>,
}

impl Default for ClockWidget {
    fn default() -> Self {
        Self::with_config(ClockConfig::default())
    }
}

impl ClockWidget {
    pub fn with_config(config: ClockConfig) -> Self {
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
        Self {
            id: "clock".into(),
            config,
            tz,
            secondaries,
        }
    }

    /// Returns (HH:MM[:SS], AM/PM, date) for the primary timezone.
    fn render_strings(&self, now_utc: DateTime<chrono::Utc>) -> (String, String, String) {
        match self.tz {
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
        match self.tz {
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
        let mut out: Vec<(String, String)> = Vec::with_capacity(self.secondaries.len() + 1);
        let (primary_label, primary_str) = match self.tz {
            Some(tz) => {
                let t = now.with_timezone(&tz);
                (city_from_tz_name(tz.name()), format_clock_entry(&t))
            }
            None => {
                let t = now.with_timezone(&Local);
                ("Local".to_string(), format_clock_entry(&t))
            }
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

    fn display_name(&self) -> &str {
        "Clock"
    }

    async fn update(&mut self, _ctx: &AppContext) -> Result<()> {
        Ok(())
    }

    fn render(&self, frame: &mut Frame, area: Rect, focused: bool) {
        let title_base = match &self.tz {
            Some(tz) => format!("Clock — {tz}"),
            None => "Clock".into(),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(focus_border_style(focused))
            .title(Span::styled(
                decorate_title(focused, &title_base),
                Style::default().add_modifier(Modifier::BOLD),
            ));

        let now = chrono::Utc::now();
        let (time, ampm, date) = self.render_strings(now);
        let big = big_digits::render(&time);

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let mut lines: Vec<Line<'_>> = Vec::new();
        // Top padding so the big digits don't kiss the border.
        lines.push(Line::from(""));
        for row in big {
            lines.push(Line::from(Span::styled(
                row,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
        }

        if self.config.show_seconds_ticker {
            lines.push(Line::from(Span::styled(
                self.ticker_string(now),
                Style::default().add_modifier(Modifier::DIM),
            )));
        }

        if !ampm.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                ampm,
                Style::default().add_modifier(Modifier::DIM),
            )));
        }
        if !date.is_empty() {
            lines.push(Line::from(""));
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
                    Style::default().add_modifier(Modifier::DIM),
                )));
                let max_label = clocks.iter().map(|(l, _)| l.chars().count()).max().unwrap_or(0);
                for (label, time_str) in &clocks {
                    let line = format!(
                        "{:<width$}  {}",
                        label,
                        time_str,
                        width = max_label
                    );
                    lines.push(Line::from(line));
                }
            }
        }

        let body = Paragraph::new(lines).alignment(Alignment::Center);
        frame.render_widget(body, inner);
    }

    fn handle_key(&mut self, _key: KeyEvent) -> EventResult {
        EventResult::Ignored
    }

    fn handle_command(&mut self, _cmd: &str, _args: &[&str]) -> Result<bool> {
        Ok(false)
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
        })
    }

    fn apply_config(&mut self, config: serde_json::Value) -> Result<()> {
        let new_config: ClockConfig =
            serde_json::from_value(config).context("invalid clock config payload")?;
        *self = Self::with_config(new_config);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn twelve_hour_format_renders_midnight_as_12_am() {
        let cfg = ClockConfig {
            timezone: Some("UTC".into()),
            show_seconds: false,
            show_seconds_ticker: false,
            show_date: false,
            hour_format: 12,
            secondary_timezones: Vec::new(),
        };
        let widget = ClockWidget::with_config(cfg);
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
        };
        let widget = ClockWidget::with_config(cfg);
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
        };
        let w = ClockWidget::with_config(cfg);
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
    fn world_clock_entries_lead_with_primary() {
        let cfg = ClockConfig {
            timezone: Some("America/Vancouver".into()),
            secondary_timezones: vec![SecondaryTimezone {
                label: "Tokyo".into(),
                timezone: "Asia/Tokyo".into(),
            }],
            ..ClockConfig::default()
        };
        let w = ClockWidget::with_config(cfg);
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
        let w = ClockWidget::with_config(cfg);
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
        let w = ClockWidget::with_config(cfg);
        assert_eq!(w.secondaries.len(), 1);
        assert_eq!(w.secondaries[0].0, "New York");
    }
}
