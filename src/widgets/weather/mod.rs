pub mod provider;

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use crossterm::event::KeyEvent;
use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use serde::Deserialize;

use crate::providers::DataProvider;

use super::{AppContext, EventResult, Widget};

use provider::{describe_code, OpenMeteoProvider, Units, WeatherData};

/// User-configurable weather options (loaded from `~/.config/glint/weather.toml`).
#[derive(Debug, Clone, Deserialize)]
pub struct WeatherConfig {
    #[serde(default = "default_label")]
    pub label: String,

    #[serde(default = "default_latitude")]
    pub latitude: f64,

    #[serde(default = "default_longitude")]
    pub longitude: f64,

    #[serde(default = "default_units")]
    pub units: Units,

    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
}

fn default_label() -> String {
    "New York".into()
}
fn default_latitude() -> f64 {
    40.7128
}
fn default_longitude() -> f64 {
    -74.006
}
fn default_units() -> Units {
    Units::Imperial
}
fn default_poll_interval() -> u64 {
    600
}

impl Default for WeatherConfig {
    fn default() -> Self {
        Self {
            label: default_label(),
            latitude: default_latitude(),
            longitude: default_longitude(),
            units: default_units(),
            poll_interval_secs: default_poll_interval(),
        }
    }
}

#[derive(Default)]
struct WeatherState {
    data: Option<WeatherData>,
    last_error: Option<String>,
    last_attempt: Option<Instant>,
    inflight: bool,
}

pub struct WeatherWidget {
    id: String,
    config: WeatherConfig,
    provider: Arc<OpenMeteoProvider>,
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
        let provider = Arc::new(OpenMeteoProvider::new(
            config.latitude,
            config.longitude,
            config.units,
        ));
        Self {
            id: "weather".into(),
            poll_interval: Duration::from_secs(config.poll_interval_secs.max(30)),
            config,
            provider,
            state: Arc::new(Mutex::new(WeatherState::default())),
        }
    }

    /// Returns true if a new fetch should be kicked off.
    fn is_due(&self) -> bool {
        let st = self.state.lock().expect("weather state poisoned");
        if st.inflight {
            return false;
        }
        match st.last_attempt {
            None => true,
            Some(t) => t.elapsed() >= self.poll_interval,
        }
    }

    fn spawn_refresh(&self) {
        {
            let mut st = self.state.lock().expect("weather state poisoned");
            st.inflight = true;
            st.last_attempt = Some(Instant::now());
        }
        let provider = self.provider.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
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

#[async_trait]
impl Widget for WeatherWidget {
    fn id(&self) -> &str {
        &self.id
    }

    fn display_name(&self) -> &str {
        "Weather"
    }

    async fn update(&mut self, _ctx: &AppContext) -> Result<()> {
        if self.is_due() {
            self.spawn_refresh();
        }
        Ok(())
    }

    fn render(&self, frame: &mut Frame, area: Rect, focused: bool) {
        let border_style = if focused {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let title = format!(" Weather — {} ", self.config.label);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(Span::styled(
                title,
                Style::default().add_modifier(Modifier::BOLD),
            ));

        let snapshot = {
            let st = self.state.lock().expect("weather state poisoned");
            (
                st.data.clone(),
                st.last_error.clone(),
                st.inflight,
                st.last_attempt.is_some(),
            )
        };
        let (data, err, inflight, attempted) = snapshot;

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let mut lines: Vec<Line<'_>> = Vec::new();
        match data {
            Some(d) => {
                let (label, icon) = describe_code(d.weather_code);
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    format!("{icon}  {label}"),
                    Style::default().add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    format!("{:.0}{}", d.temperature, d.units.temp_symbol()),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(format!(
                    "Feels like {:.0}{}",
                    d.apparent_temperature,
                    d.units.temp_symbol()
                )));
                lines.push(Line::from(""));
                lines.push(Line::from(format!(
                    "Humidity: {:.0}%   Wind: {:.0} {}",
                    d.humidity,
                    d.wind_speed,
                    d.units.wind_label()
                )));
                lines.push(Line::from(""));
                let age = chrono::Local::now()
                    .signed_duration_since(d.fetched_at)
                    .num_seconds()
                    .max(0);
                let footer = if let Some(e) = err {
                    format!("⚠ stale ({e}) — updated {age}s ago")
                } else {
                    format!("Updated {age}s ago")
                };
                lines.push(Line::from(Span::styled(
                    footer,
                    Style::default().add_modifier(Modifier::DIM),
                )));
            }
            None => {
                lines.push(Line::from(""));
                let msg = if inflight {
                    "Loading…"
                } else if let Some(ref e) = err {
                    lines.push(Line::from(Span::styled(
                        "Weather unavailable",
                        Style::default().add_modifier(Modifier::BOLD),
                    )));
                    lines.push(Line::from(""));
                    e.as_str()
                } else if attempted {
                    "Loading…"
                } else {
                    "Fetching first reading…"
                };
                lines.push(Line::from(msg.to_string()));
            }
        }

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
            "label": self.config.label,
            "latitude": self.config.latitude,
            "longitude": self.config.longitude,
            "poll_interval_secs": self.config.poll_interval_secs,
        })
    }

    fn apply_config(&mut self, config: serde_json::Value) -> Result<()> {
        let new_config: WeatherConfig =
            serde_json::from_value(config).context("invalid weather config payload")?;
        *self = Self::with_config(new_config);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_widget_is_not_inflight_and_has_no_data() {
        let w = WeatherWidget::default();
        let st = w.state.lock().unwrap();
        assert!(st.data.is_none());
        assert!(!st.inflight);
        assert!(st.last_attempt.is_none());
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
    fn is_due_when_never_attempted() {
        let w = WeatherWidget::default();
        assert!(w.is_due());
    }
}
