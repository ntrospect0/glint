pub mod provider;

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Datelike;
use crossterm::event::KeyEvent;
use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use serde::Deserialize;

use crate::geolocation::{self, GeoLocation};
use crate::providers::DataProvider;
use crate::ui::{decorate_title, focus_border_style};

use super::{AppContext, EventResult, Widget};

use provider::{ascii_art, describe_code, OpenMeteoProvider, Units, WeatherData};

/// User-configurable weather options (loaded from `~/.config/glint/weather.toml`).
#[derive(Debug, Clone, Deserialize)]
pub struct WeatherConfig {
    /// Display label for the location. If omitted, the IP-geolocation result is used.
    #[serde(default)]
    pub label: Option<String>,

    /// Explicit latitude. If omitted and `auto_locate` is true, ipapi.co is used.
    #[serde(default)]
    pub latitude: Option<f64>,

    /// Explicit longitude. If omitted and `auto_locate` is true, ipapi.co is used.
    #[serde(default)]
    pub longitude: Option<f64>,

    #[serde(default = "default_units")]
    pub units: Units,

    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    /// When true and lat/lon are missing, glint geolocates by IP on first
    /// refresh and caches the result for the session.
    #[serde(default = "default_auto_locate")]
    pub auto_locate: bool,
}

fn default_units() -> Units {
    Units::Metric
}
fn default_poll_interval() -> u64 {
    600
}
fn default_auto_locate() -> bool {
    true
}

impl Default for WeatherConfig {
    fn default() -> Self {
        // Without a weather.toml on disk we default to Richmond, BC. To opt
        // into IP geolocation, write a weather.toml that leaves latitude and
        // longitude unset (auto_locate defaults to true).
        Self {
            label: Some("Richmond, BC".into()),
            latitude: Some(49.166),
            longitude: Some(-123.133),
            units: default_units(),
            poll_interval_secs: default_poll_interval(),
            auto_locate: default_auto_locate(),
        }
    }
}

#[derive(Default)]
struct WeatherState {
    location: Option<GeoLocation>,
    locating: bool,
    geolocation_error: Option<String>,
    data: Option<WeatherData>,
    last_error: Option<String>,
    last_attempt: Option<Instant>,
    inflight: bool,
}

pub struct WeatherWidget {
    id: String,
    config: WeatherConfig,
    state: Arc<Mutex<WeatherState>>,
    poll_interval: Duration,
}

impl Default for WeatherWidget {
    fn default() -> Self {
        Self::with_config(WeatherConfig::default())
    }
}

impl WeatherWidget {
    pub fn with_config(config: WeatherConfig) -> Self {
        // If the user specified explicit lat/lon, seed the location immediately
        // so we skip the geolocation hop.
        let initial_location = match (config.latitude, config.longitude) {
            (Some(lat), Some(lon)) => Some(GeoLocation {
                latitude: lat,
                longitude: lon,
                label: config
                    .label
                    .clone()
                    .unwrap_or_else(|| format!("{lat:.3}, {lon:.3}")),
                timezone: None,
            }),
            _ => None,
        };
        let state = Arc::new(Mutex::new(WeatherState {
            location: initial_location,
            ..WeatherState::default()
        }));
        Self {
            id: "weather".into(),
            poll_interval: Duration::from_secs(config.poll_interval_secs.max(30)),
            config,
            state,
        }
    }

    /// What the widget should do on the next tick. Computed inside a single
    /// short lock window.
    fn next_action(&self) -> NextAction {
        let st = self.state.lock().expect("weather state poisoned");
        if st.location.is_none() {
            if st.locating {
                return NextAction::Wait;
            }
            return if self.config.auto_locate {
                NextAction::Locate
            } else {
                NextAction::Wait
            };
        }
        if st.inflight {
            return NextAction::Wait;
        }
        let due = match st.last_attempt {
            None => true,
            Some(t) => t.elapsed() >= self.poll_interval,
        };
        if due {
            NextAction::Fetch
        } else {
            NextAction::Wait
        }
    }

    fn spawn_geolocate(&self) {
        {
            let mut st = self.state.lock().expect("weather state poisoned");
            st.locating = true;
        }
        let state = self.state.clone();
        tokio::spawn(async move {
            let result = geolocation::by_ip().await;
            let mut st = state.lock().expect("weather state poisoned");
            st.locating = false;
            match result {
                Ok(loc) => {
                    st.location = Some(loc);
                    st.geolocation_error = None;
                }
                Err(err) => {
                    tracing::warn!(error = %err, "ip geolocation failed");
                    st.geolocation_error = Some(err.to_string());
                }
            }
        });
    }

    fn spawn_refresh(&self) {
        let (lat, lon) = {
            let st = self.state.lock().expect("weather state poisoned");
            let Some(loc) = st.location.as_ref() else {
                return;
            };
            (loc.latitude, loc.longitude)
        };
        {
            let mut st = self.state.lock().expect("weather state poisoned");
            st.inflight = true;
            st.last_attempt = Some(Instant::now());
        }
        let units = self.config.units;
        let state = self.state.clone();
        tokio::spawn(async move {
            let provider = OpenMeteoProvider::new(lat, lon, units);
            let result = provider.fetch().await;
            let mut st = state.lock().expect("weather state poisoned");
            st.inflight = false;
            match result {
                Ok(data) => {
                    st.data = Some(data);
                    st.last_error = None;
                }
                Err(err) => {
                    tracing::warn!(error = %err, "weather fetch failed");
                    st.last_error = Some(err.to_string());
                }
            }
        });
    }
}

#[derive(Debug, Clone, Copy)]
enum NextAction {
    Locate,
    Fetch,
    Wait,
}

#[async_trait]
impl Widget for WeatherWidget {
    fn id(&self) -> &str {
        &self.id
    }

    fn display_name(&self) -> &str {
        "Weather"
    }

    async fn update(&mut self, _ctx: &AppContext) -> Result<()> {
        match self.next_action() {
            NextAction::Locate => self.spawn_geolocate(),
            NextAction::Fetch => self.spawn_refresh(),
            NextAction::Wait => {}
        }
        Ok(())
    }

    fn render(&self, frame: &mut Frame, area: Rect, focused: bool) {
        let snapshot = {
            let st = self.state.lock().expect("weather state poisoned");
            Snapshot {
                location_label: st.location.as_ref().map(|l| l.label.clone()),
                locating: st.locating,
                geolocation_error: st.geolocation_error.clone(),
                data: st.data.clone(),
                last_error: st.last_error.clone(),
                inflight: st.inflight,
                attempted: st.last_attempt.is_some(),
            }
        };
        let title_label = snapshot
            .location_label
            .clone()
            .unwrap_or_else(|| "Locating…".into());
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(focus_border_style(focused))
            .title(Span::styled(
                decorate_title(focused, &format!("Weather — {title_label}")),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        // When we have weather data, the ASCII art needs its own fixed-width
        // sub-rect so each art row lands at the same x offset. Centered
        // Paragraph alignment treats each line independently — lines with
        // different trimmed widths shift relative to each other, which made
        // the symmetric sun look broken on the bottom row.
        if let Some(data) = &snapshot.data {
            render_with_art(frame, inner, &snapshot, data, self.config.units);
        } else {
            let lines = loading_lines(&snapshot);
            let mut padded: Vec<Line<'_>> = Vec::with_capacity(lines.len() + 1);
            padded.push(Line::from(""));
            padded.extend(lines);
            let body = Paragraph::new(padded).alignment(Alignment::Center);
            frame.render_widget(body, inner);
        }
    }

    fn handle_key(&mut self, _key: KeyEvent) -> EventResult {
        EventResult::Ignored
    }

    fn handle_command(&mut self, _cmd: &str, _args: &[&str]) -> Result<bool> {
        Ok(false)
    }

    fn config(&self) -> serde_json::Value {
        serde_json::json!({
            "label": self.config.label,
            "latitude": self.config.latitude,
            "longitude": self.config.longitude,
            "poll_interval_secs": self.config.poll_interval_secs,
            "auto_locate": self.config.auto_locate,
        })
    }

    fn apply_config(&mut self, config: serde_json::Value) -> Result<()> {
        let new_config: WeatherConfig =
            serde_json::from_value(config).context("invalid weather config payload")?;
        *self = Self::with_config(new_config);
        Ok(())
    }
}

struct Snapshot {
    location_label: Option<String>,
    locating: bool,
    geolocation_error: Option<String>,
    data: Option<WeatherData>,
    last_error: Option<String>,
    inflight: bool,
    attempted: bool,
}

/// Width of every row in `ascii_art()`. Used to carve a fixed sub-rect so all
/// art rows render at the same x offset regardless of trailing-whitespace
/// quirks in centered Paragraph layout.
const ASCII_ART_WIDTH: u16 = 13;
const ASCII_ART_HEIGHT: u16 = 4;

fn render_with_art(
    frame: &mut Frame,
    inner: Rect,
    s: &Snapshot,
    data: &WeatherData,
    units: Units,
) {
    let (label, icon) = describe_code(data.weather_code);

    // Header: top blank + condition label + blank.
    let header_lines: Vec<Line<'_>> = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("{icon}  {label}"),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    let header_height: u16 = header_lines.len() as u16;
    let header_area = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: header_height.min(inner.height),
    };
    frame.render_widget(
        Paragraph::new(header_lines).alignment(Alignment::Center),
        header_area,
    );

    // Art: own fixed-width left-aligned sub-rect, horizontally centered.
    if inner.height >= header_height + ASCII_ART_HEIGHT {
        let art_w = ASCII_ART_WIDTH.min(inner.width);
        let art_x = inner.x + (inner.width.saturating_sub(art_w)) / 2;
        let art_area = Rect {
            x: art_x,
            y: inner.y + header_height,
            width: art_w,
            height: ASCII_ART_HEIGHT,
        };
        let art_lines: Vec<Line<'_>> = ascii_art(data.weather_code)
            .iter()
            .map(|s| Line::from(*s))
            .collect();
        frame.render_widget(Paragraph::new(art_lines), art_area);
    }

    // Bottom section: temp, feels-like, humidity/wind, forecast, footer.
    let used_top = header_height + ASCII_ART_HEIGHT + 1; // +1 trailing blank
    if inner.height <= used_top {
        return;
    }
    let bottom_area = Rect {
        x: inner.x,
        y: inner.y + used_top,
        width: inner.width,
        height: inner.height - used_top,
    };
    let mut lines: Vec<Line<'_>> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("{:.0}{}", data.temperature, data.units.temp_symbol()),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(format!(
        "Feels like {:.0}{}",
        data.apparent_temperature,
        data.units.temp_symbol()
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(format!(
        "Humidity: {:.0}%   Wind: {:.0} {}",
        data.humidity,
        data.wind_speed,
        data.units.wind_label()
    )));

    if data.daily.len() >= 2 {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "── Next 3 days ──",
            Style::default().add_modifier(Modifier::DIM),
        )));
        for d in data.daily.iter().skip(1).take(3) {
            let (_, icon) = describe_code(d.weather_code);
            lines.push(Line::from(format!(
                "{}  {}  {:.0}{} / {:.0}{}",
                weekday_short(d.date.weekday()),
                icon,
                d.temperature_high,
                units.temp_symbol(),
                d.temperature_low,
                units.temp_symbol(),
            )));
        }
    }

    lines.push(Line::from(""));
    let age_secs = chrono::Local::now()
        .signed_duration_since(data.fetched_at)
        .num_seconds()
        .max(0);
    let age = format_age(age_secs);
    let footer = if let Some(e) = &s.last_error {
        format!("⚠ stale ({e}) — updated {age} ago")
    } else {
        format!("Updated {age} ago")
    };
    lines.push(Line::from(Span::styled(
        footer,
        Style::default().add_modifier(Modifier::DIM),
    )));

    frame.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center),
        bottom_area,
    );
}

fn loading_lines(s: &Snapshot) -> Vec<Line<'_>> {
    let mut lines: Vec<Line<'_>> = Vec::new();
    lines.push(Line::from(""));
    if s.location_label.is_none() {
        if let Some(err) = &s.geolocation_error {
            lines.push(Line::from(Span::styled(
                "Could not auto-locate",
                Style::default().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(err.clone()));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Set latitude/longitude in ~/.config/glint/weather.toml",
                Style::default().add_modifier(Modifier::DIM),
            )));
        } else if s.locating {
            lines.push(Line::from("Locating you via IP…"));
        } else {
            lines.push(Line::from("Configure latitude/longitude in weather.toml"));
        }
        return lines;
    }
    if s.inflight {
        lines.push(Line::from("Loading weather…"));
    } else if let Some(err) = &s.last_error {
        lines.push(Line::from(Span::styled(
            "Weather unavailable",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(err.clone()));
    } else if s.attempted {
        lines.push(Line::from("Loading weather…"));
    } else {
        lines.push(Line::from("Fetching first reading…"));
    }
    lines
}

/// Format a duration in seconds as a compact `45s`, `7m`, `3h`, or `2d` label.
fn format_age(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

fn weekday_short(w: chrono::Weekday) -> &'static str {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_widget_seeds_richmond_location() {
        let w = WeatherWidget::default();
        let st = w.state.lock().unwrap();
        assert!(st.data.is_none());
        let loc = st.location.as_ref().expect("default should bake in Richmond");
        assert_eq!(loc.latitude, 49.166);
        assert_eq!(loc.longitude, -123.133);
        assert!(!st.inflight);
        assert!(!st.locating);
    }

    #[test]
    fn explicit_lat_lon_seeds_location_immediately() {
        let cfg = WeatherConfig {
            label: Some("Richmond, BC".into()),
            latitude: Some(49.166),
            longitude: Some(-123.133),
            ..WeatherConfig::default()
        };
        let w = WeatherWidget::with_config(cfg);
        let st = w.state.lock().unwrap();
        let loc = st.location.as_ref().expect("location should be seeded");
        assert_eq!(loc.latitude, 49.166);
        assert_eq!(loc.label, "Richmond, BC");
    }

    #[test]
    fn poll_interval_floors_to_thirty_seconds() {
        let cfg = WeatherConfig {
            poll_interval_secs: 5,
            ..WeatherConfig::default()
        };
        let w = WeatherWidget::with_config(cfg);
        assert_eq!(w.poll_interval, Duration::from_secs(30));
    }

    #[test]
    fn format_age_uses_appropriate_units() {
        assert_eq!(format_age(0), "0s");
        assert_eq!(format_age(45), "45s");
        assert_eq!(format_age(59), "59s");
        assert_eq!(format_age(60), "1m");
        assert_eq!(format_age(3599), "59m");
        assert_eq!(format_age(3600), "1h");
        assert_eq!(format_age(86_399), "23h");
        assert_eq!(format_age(86_400), "1d");
        assert_eq!(format_age(86_400 * 5), "5d");
    }

    #[test]
    fn next_action_is_locate_when_no_location_and_auto_locate() {
        // To test the auto-locate path, explicitly clear lat/lon.
        let cfg = WeatherConfig {
            latitude: None,
            longitude: None,
            ..WeatherConfig::default()
        };
        let w = WeatherWidget::with_config(cfg);
        assert!(matches!(w.next_action(), NextAction::Locate));
    }

    #[test]
    fn next_action_is_fetch_when_location_known_and_no_recent_attempt() {
        let cfg = WeatherConfig {
            latitude: Some(49.166),
            longitude: Some(-123.133),
            label: Some("Richmond, BC".into()),
            ..WeatherConfig::default()
        };
        let w = WeatherWidget::with_config(cfg);
        assert!(matches!(w.next_action(), NextAction::Fetch));
    }

    #[test]
    fn next_action_is_wait_when_no_location_and_not_auto_locating() {
        let cfg = WeatherConfig {
            latitude: None,
            longitude: None,
            auto_locate: false,
            ..WeatherConfig::default()
        };
        let w = WeatherWidget::with_config(cfg);
        assert!(matches!(w.next_action(), NextAction::Wait));
    }
}
