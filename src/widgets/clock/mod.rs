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

use super::{AppContext, EventResult, Widget};

/// User-configurable clock options (loaded from `~/.config/glint/clock.toml`).
#[derive(Debug, Clone, Deserialize)]
pub struct ClockConfig {
    /// IANA timezone name (e.g. "America/Los_Angeles"). Defaults to system local time.
    #[serde(default)]
    pub timezone: Option<String>,

    #[serde(default = "default_show_seconds")]
    pub show_seconds: bool,

    #[serde(default = "default_show_date")]
    pub show_date: bool,

    /// 12 or 24. Anything else falls back to 24.
    #[serde(default = "default_hour_format")]
    pub hour_format: u8,
}

fn default_show_seconds() -> bool {
    false
}
fn default_show_date() -> bool {
    true
}
fn default_hour_format() -> u8 {
    24
}

impl Default for ClockConfig {
    fn default() -> Self {
        Self {
            timezone: None,
            show_seconds: default_show_seconds(),
            show_date: default_show_date(),
            hour_format: default_hour_format(),
        }
    }
}

pub struct ClockWidget {
    id: String,
    config: ClockConfig,
    tz: Option<Tz>,
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
        Self {
            id: "clock".into(),
            config,
            tz,
        }
    }

    /// Returns the time string (e.g. "14:32" or "02:32 PM") and the date line.
    fn render_strings(&self, now_utc: DateTime<chrono::Utc>) -> (String, String, String) {
        let (time, ampm, date) = match self.tz {
            Some(tz) => {
                let local = now_utc.with_timezone(&tz);
                self.format_parts(local)
            }
            None => {
                let local = now_utc.with_timezone(&Local);
                self.format_parts(local)
            }
        };
        (time, ampm, date)
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

/// 3-wide × 5-tall block-character font for digits 0-9 and `:`.
const GLYPH_HEIGHT: usize = 5;

fn glyph(c: char) -> Option<[&'static str; GLYPH_HEIGHT]> {
    Some(match c {
        '0' => ["███", "█ █", "█ █", "█ █", "███"],
        '1' => ["  █", "  █", "  █", "  █", "  █"],
        '2' => ["███", "  █", "███", "█  ", "███"],
        '3' => ["███", "  █", "███", "  █", "███"],
        '4' => ["█ █", "█ █", "███", "  █", "  █"],
        '5' => ["███", "█  ", "███", "  █", "███"],
        '6' => ["███", "█  ", "███", "█ █", "███"],
        '7' => ["███", "  █", "  █", "  █", "  █"],
        '8' => ["███", "█ █", "███", "█ █", "███"],
        '9' => ["███", "█ █", "███", "  █", "███"],
        ':' => ["   ", " █ ", "   ", " █ ", "   "],
        _ => return None,
    })
}

/// Render `time` (e.g. "14:32") as five rows of block-character glyphs.
fn render_big_digits(time: &str) -> Vec<String> {
    let mut rows: Vec<String> = vec![String::new(); GLYPH_HEIGHT];
    for (i, ch) in time.chars().enumerate() {
        let Some(g) = glyph(ch) else { continue };
        for (row_idx, row) in rows.iter_mut().enumerate() {
            if i > 0 {
                row.push(' ');
            }
            row.push_str(g[row_idx]);
        }
    }
    rows
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
        let border_style = if focused {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let title = match &self.tz {
            Some(tz) => format!(" Clock — {tz} "),
            None => " Clock ".into(),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(Span::styled(
                title,
                Style::default().add_modifier(Modifier::BOLD),
            ));

        let now = chrono::Utc::now();
        let (time, ampm, date) = self.render_strings(now);
        let big = render_big_digits(&time);

        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Vertically center: 5 digit rows + optional ampm/date lines.
        let mut lines: Vec<Line<'_>> = Vec::with_capacity(GLYPH_HEIGHT + 3);
        for row in big {
            lines.push(Line::from(Span::styled(
                row,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
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

        // Pad with leading blank lines so content is roughly centered vertically.
        let used = lines.len() as u16;
        let pad = inner.height.saturating_sub(used) / 2;
        let mut padded: Vec<Line<'_>> = Vec::with_capacity(lines.len() + pad as usize);
        for _ in 0..pad {
            padded.push(Line::from(""));
        }
        padded.extend(lines);

        let body = Paragraph::new(padded).alignment(Alignment::Center);
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
            "show_date": self.config.show_date,
            "hour_format": self.config.hour_format,
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
    fn glyph_renders_all_chars_in_a_time_string() {
        for ch in "0123456789:".chars() {
            assert!(glyph(ch).is_some(), "missing glyph for {ch}");
        }
    }

    #[test]
    fn big_digits_have_five_rows_and_correct_width() {
        let rows = render_big_digits("12:34");
        assert_eq!(rows.len(), GLYPH_HEIGHT);
        // 5 glyphs × 3 wide, plus 4 single-space separators = 19.
        let widths: Vec<usize> = rows.iter().map(|r| r.chars().count()).collect();
        for w in &widths {
            assert_eq!(*w, 5 * 3 + 4);
        }
    }

    #[test]
    fn twelve_hour_format_renders_midnight_as_12_am() {
        let cfg = ClockConfig {
            timezone: Some("UTC".into()),
            show_seconds: false,
            show_date: false,
            hour_format: 12,
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
            show_date: false,
            hour_format: 24,
        };
        let widget = ClockWidget::with_config(cfg);
        let t = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 9, 5, 7).unwrap();
        let (time, ampm, _) = widget.render_strings(t);
        assert_eq!(time, "09:05:07");
        assert_eq!(ampm, "");
    }
}
