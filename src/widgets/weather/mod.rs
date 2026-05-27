// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

pub mod icons;
pub mod provider;

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Datelike;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};
use serde::Deserialize;

use crate::cache::ScopedCache;
use crate::geolocation::{self, GeoLocation};
use crate::theme::{ColorScheme, Theme};
use crate::ui::apply_title_row;

use super::{AppContext, EventResult, Widget};

use provider::{
    describe_code, icon_for_code, render_icon, OpenMeteoProvider, Units, WeatherData,
};

/// Loaded from `~/.config/glint/weather.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct WeatherConfig {
    /// Display label. Falls back to the IP-geolocation result.
    #[serde(default)]
    pub label: Option<String>,

    #[serde(default)]
    pub latitude: Option<f64>,

    #[serde(default)]
    pub longitude: Option<f64>,

    #[serde(default = "default_units")]
    pub units: Units,

    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    /// IP-geolocate (via ipapi.co) when lat/lon are missing. Cached per session.
    #[serde(default = "default_auto_locate")]
    pub auto_locate: bool,

    /// Per-widget overrides layered on the app theme.
    #[serde(default)]
    pub colors: ColorScheme,

    /// `Shift+<letter>` focus shortcuts; falls back to `['w', 'e', 'a', 't', 'h', 'r']`.
    #[serde(default)]
    pub shortcuts: Vec<char>,
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
            colors: ColorScheme::default(),
            shortcuts: Vec::new(),
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
    /// Set by `:weather <city>` — when Some, overrides `location` for fetches.
    /// Cleared by `x`.
    transient_location: Option<GeoLocation>,
    /// True while a `:weather <city>` lookup is in flight.
    transient_searching: bool,
}

const CACHE_KEY_CURRENT: &str = "current";

pub struct WeatherWidget {
    id: String,
    instance: String,
    /// Cached `Weather` / `Weather (instance)` label so `display_name()`
    /// can hand out a `&str` without per-call allocation.
    display_name_cache: String,
    config: WeatherConfig,
    state: Arc<Mutex<WeatherState>>,
    poll_interval: Duration,
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
    /// Persistent cache of the last successful WeatherData snapshot.
    cache: ScopedCache,
}

impl Default for WeatherWidget {
    fn default() -> Self {
        Self::with_config(
            "main".to_string(),
            WeatherConfig::default(),
            Arc::new(Theme::builtin_defaults()),
            ScopedCache::ephemeral(),
        )
    }
}

impl WeatherWidget {
    pub fn with_config(
        instance: String,
        config: WeatherConfig,
        app_theme: Arc<Theme>,
        cache: ScopedCache,
    ) -> Self {
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
        let poll_interval = Duration::from_secs(config.poll_interval_secs.max(30));
        // Seed from cache so the first frame shows the previous reading.
        // Mapping wall-clock age onto the monotonic `last_attempt` lets the
        // existing poll-interval gate suppress redundant refetches.
        let mut initial_state = WeatherState {
            location: initial_location,
            ..WeatherState::default()
        };
        if let Some(entry) = cache.load::<WeatherData>(CACHE_KEY_CURRENT) {
            let age = entry.age().min(poll_interval);
            initial_state.data = Some(entry.value);
            initial_state.last_attempt = Some(Instant::now() - age);
        }
        let state = Arc::new(Mutex::new(initial_state));
        let theme = app_theme.with_overrides(&config.colors);
        let shortcut_prefs = if config.shortcuts.is_empty() {
            vec!['w', 'e', 'a', 't', 'h', 'r']
        } else {
            config.shortcuts.clone()
        };
        let id = if instance == "main" {
            "weather".to_string()
        } else {
            format!("weather@{instance}")
        };
        let display_name_cache = if instance == "main" {
            "Weather".to_string()
        } else {
            format!("Weather ({instance})")
        };
        Self {
            id,
            instance,
            display_name_cache,
            poll_interval,
            config,
            state,
            app_theme,
            theme,
            shortcut: None,
            shortcut_prefs,
            cache,
        }
    }

    /// What the widget should do on the next tick. Computed inside a single
    /// short lock window. A transient location set by `:weather <city>` takes
    /// priority over the configured (or IP-geolocated) `location`.
    fn next_action(&self) -> NextAction {
        let st = self.state.lock().expect("weather state poisoned");
        let effective = st.transient_location.as_ref().or(st.location.as_ref());
        if effective.is_none() {
            if st.locating || st.transient_searching {
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

    /// Resolve a city / place name to lat/lon via Open-Meteo's free geocoding
    /// API, store the result as `transient_location`, and force a refresh.
    /// Errors are logged; the widget keeps showing the previous data.
    fn lookup_location(&self, query: &str) {
        {
            let mut st = self.state.lock().expect("weather state poisoned");
            st.transient_searching = true;
        }
        let state = self.state.clone();
        let query = query.to_string();
        tokio::spawn(async move {
            let result = crate::geolocation::by_name(&query).await;
            let mut st = state.lock().expect("weather state poisoned");
            st.transient_searching = false;
            match result {
                Ok(loc) => {
                    st.transient_location = Some(loc);
                    // Clear cached weather + force refetch on the next tick.
                    st.data = None;
                    st.last_attempt = None;
                }
                Err(err) => {
                    tracing::warn!(query = %query, error = %err, "weather geocoding failed");
                }
            }
        });
    }

    /// Clear the `:weather <city>` override and re-fetch with the configured
    /// location.
    fn clear_transient(&self) {
        let mut st = self.state.lock().expect("weather state poisoned");
        if st.transient_location.take().is_some() {
            st.data = None;
            st.last_attempt = None;
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
            let Some(loc) = st
                .transient_location
                .as_ref()
                .or(st.location.as_ref())
            else {
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
        let cache = self.cache.clone();
        tokio::spawn(async move {
            let provider = OpenMeteoProvider::new(lat, lon, units);
            let result = provider.fetch().await;
            let mut st = state.lock().expect("weather state poisoned");
            st.inflight = false;
            match result {
                Ok(data) => {
                    if let Err(err) = cache.store(CACHE_KEY_CURRENT, &data) {
                        tracing::warn!(error = %err, "weather cache store failed");
                    }
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

    fn kind(&self) -> &str {
        "weather"
    }

    fn instance(&self) -> &str {
        &self.instance
    }

    fn display_name(&self) -> &str {
        &self.display_name_cache
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
            let label = st
                .transient_location
                .as_ref()
                .map(|l| format!("{} (lookup)", l.label))
                .or_else(|| st.location.as_ref().map(|l| l.label.clone()));
            // `revert_target` is populated only when an override is active.
            // The default location (loaded from weather.toml) is what `x`
            // brings us back to — surface its label so the hint reads
            // "x: revert to Richmond, BC" rather than a vague "revert".
            let revert_target = if st.transient_location.is_some() {
                st.location.as_ref().map(|l| l.label.clone())
            } else {
                None
            };
            Snapshot {
                location_label: label,
                locating: st.locating,
                geolocation_error: st.geolocation_error.clone(),
                data: st.data.clone(),
                last_error: st.last_error.clone(),
                inflight: st.inflight || st.transient_searching,
                attempted: st.last_attempt.is_some(),
                revert_target,
            }
        };
        let title_label = snapshot.location_label.clone();
        let title_prefix = if self.instance == "main" {
            "Weather".to_string()
        } else {
            format!("Weather ({})", self.instance)
        };
        let metadata = title_label.or_else(|| Some("Locating…".to_string()));
        let block = apply_title_row(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(self.theme.border_style(focused)),
            focused,
            &title_prefix,
            metadata.as_deref(),
            self.shortcut,
            &self.theme,
            area.width,
        );
        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Reserve a bottom row for the override hint when `:weather <city>`
        // is active. Falls back to the full inner area when there's no
        // override or the cell is too short to spare a row.
        let (body_area, hint_area) =
            if snapshot.revert_target.is_some() && inner.height >= 2 {
                let h = inner.height - 1;
                (
                    Rect {
                        x: inner.x,
                        y: inner.y,
                        width: inner.width,
                        height: h,
                    },
                    Some(Rect {
                        x: inner.x,
                        y: inner.y + h,
                        width: inner.width,
                        height: 1,
                    }),
                )
            } else {
                (inner, None)
            };

        // When we have weather data, the ASCII art needs its own fixed-width
        // sub-rect so each art row lands at the same x offset. Centered
        // Paragraph alignment treats each line independently — lines with
        // different trimmed widths shift relative to each other, which made
        // the symmetric sun look broken on the bottom row.
        if let Some(data) = &snapshot.data {
            render_with_art(
                frame,
                body_area,
                &snapshot,
                data,
                self.config.units,
                &self.theme,
            );
        } else {
            let lines = loading_lines(&snapshot, &self.theme);
            let mut padded: Vec<Line<'_>> = Vec::with_capacity(lines.len() + 1);
            padded.push(Line::from(""));
            padded.extend(lines);
            let body = Paragraph::new(padded).alignment(Alignment::Center);
            frame.render_widget(body, body_area);
        }

        if let (Some(area), Some(target)) = (hint_area, snapshot.revert_target.as_deref()) {
            let hint = Line::from(Span::styled(
                format!("x: revert to {target}"),
                self.theme.text_dim,
            ));
            frame.render_widget(Paragraph::new(hint).alignment(Alignment::Center), area);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if key.modifiers != KeyModifiers::NONE && key.modifiers != KeyModifiers::SHIFT {
            return EventResult::Ignored;
        }
        // Uppercase ASCII letters are reserved for the app-wide
        // `Shift+<letter>` focus-jump dispatcher — never consume them here.
        if let KeyCode::Char(c) = key.code {
            if c.is_ascii_uppercase() {
                return EventResult::Ignored;
            }
        }
        if matches!(key.code, KeyCode::Char('x')) {
            self.clear_transient();
            EventResult::Handled
        } else {
            EventResult::Ignored
        }
    }

    fn handle_command(&mut self, cmd: &str, args: &[&str]) -> Result<bool> {
        match cmd {
            "weather" | "w" => {
                if args.is_empty() {
                    anyhow::bail!("usage: :weather <city>");
                }
                let query = args.join(" ");
                self.lookup_location(&query);
                Ok(true)
            }
            "refresh" => {
                let mut st = self.state.lock().expect("weather state poisoned");
                st.last_attempt = None;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn keybindings(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("x", "clear :weather lookup (return to default location)"),
            (":weather <city>", "look up weather for a place"),
        ]
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
        let app_theme = self.app_theme.clone();
        let cache = self.cache.clone();
        let instance = self.instance.clone();
        *self = Self::with_config(instance, new_config, app_theme, cache);
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

    fn shortcut(&self) -> Option<char> {
        self.shortcut
    }

    fn title_metadata(&self) -> Option<String> {
        // Active location label — transient override wins, falling
        // back to the configured location. `None` until the first
        // fetch resolves a location.
        let st = self.state.lock().ok()?;
        st.transient_location
            .as_ref()
            .map(|l| l.label.clone())
            .or_else(|| st.location.as_ref().map(|l| l.label.clone()))
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
    /// Label of the configured default location (typed at startup from
    /// `weather.toml`). Populated only when a `:weather <city>` override is
    /// active — used to drive the "x: revert to <default>" footer hint.
    revert_target: Option<String>,
}


fn render_with_art(
    frame: &mut Frame,
    inner: Rect,
    s: &Snapshot,
    data: &WeatherData,
    units: Units,
    theme: &Theme,
) {
    let (label, icon) = describe_code(data.weather_code);

    // Header: top blank + condition label + blank.
    //
    // We center the icon + label manually instead of relying on
    // `Alignment::Center`. Ratatui's center math goes through
    // `unicode_width`, which reports the emoji + VS-16 sequence as width 1,
    // but actual terminals render the glyph as 2 cells. Their disagreement
    // would shift the whole line one cell off-center. `chars().count()`
    // matches the real cell width for our icons (emoji + VS-16 = 2 chars,
    // bare "·" fallback = 1 char) so the hand-rolled padding lines up with
    // what the user actually sees.
    let header_text = format!("{icon}  {label}");
    let visual_width = header_text.chars().count() as u16;
    let pad = inner.width.saturating_sub(visual_width) / 2;
    let header_lines: Vec<Line<'_>> = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("{:pad$}{header_text}", "", pad = pad as usize),
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
        Paragraph::new(header_lines).alignment(Alignment::Left),
        header_area,
    );

    // Art: pick the icon for the current weather + time-of-day, then carve
    // out a sub-rect sized to *that* icon's actual dimensions. Using the
    // icon's own height/width (rather than the worst-case maximum across
    // all 16 glyphs) keeps the gap between the art and the temperature
    // text tight regardless of which sprite is displayed.
    //
    // When the cell is short, the art is the first thing to go — temp,
    // feels-like, humidity, wind, and forecast all communicate the actual
    // weather; the glyph is decoration. `MIN_BOTTOM_ROWS_FOR_ART` is the
    // approximate row count needed to show the full bottom block (temp +
    // feels + blank + humidity/wind + blank + "Next 3 days" header + 3
    // forecast rows + blank + footer = ~11). If we can't fit header + icon +
    // that, drop the icon and let the bottom block use the freed rows.
    const MIN_BOTTOM_ROWS_FOR_ART: u16 = 11;
    let night = data.is_night(chrono::Local::now());
    let icon = icon_for_code(data.weather_code, night);
    let icon_rows = (icon.height as u16).div_ceil(2);
    let icon_cols = (icon.width as u16).min(inner.width);
    let mut used_top = header_height;
    if inner.height >= header_height + icon_rows + MIN_BOTTOM_ROWS_FOR_ART {
        let art_x = inner.x + (inner.width.saturating_sub(icon_cols)) / 2;
        let art_area = Rect {
            x: art_x,
            y: inner.y + header_height,
            width: icon_cols,
            height: icon_rows,
        };
        frame.render_widget(Paragraph::new(render_icon(icon)), art_area);
        used_top = used_top.saturating_add(icon_rows).saturating_add(1); // +1 trailing blank
    }

    // Bottom section: temp, feels-like, humidity/wind, forecast, footer.
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
            theme.text_dim,
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
    // Sub-minute ages aren't useful to surface as a seconds counter —
    // they tick noisily every second when nothing's actually changed.
    // Collapse anything under a minute to a single "Just updated" line.
    let fresh = age_secs < 60;
    let age = format_age(age_secs);
    let footer = if let Some(e) = &s.last_error {
        if fresh {
            format!("⚠ stale ({e}) — just updated")
        } else {
            format!("⚠ stale ({e}) — updated {age} ago")
        }
    } else if fresh {
        "Just updated".to_string()
    } else {
        format!("Updated {age} ago")
    };
    lines.push(Line::from(Span::styled(footer, theme.text_dim)));

    frame.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center),
        bottom_area,
    );
}

fn loading_lines(s: &Snapshot, theme: &Theme) -> Vec<Line<'static>> {
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
                theme.text_dim,
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

pub const KIND: &str = "weather";

/// Wizard descriptor. Lat/lon are optional Text fields so users can leave
/// them blank to opt into IP geolocation; a validator rejects malformed
/// numeric input. The custom `render_toml` omits empty optionals so the
/// resulting `weather.toml` parses cleanly into `WeatherConfig`.
pub fn wizard_descriptor() -> crate::wizard::descriptor::WizardDescriptor {
    use crate::wizard::descriptor::{
        ChoiceOption, WizardDescriptor, WizardField, WizardFieldKind, WizardValue,
    };

    fn validate_latitude(v: &WizardValue) -> Result<(), String> {
        if let WizardValue::Text(s) = v {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Ok(());
            }
            match trimmed.parse::<f64>() {
                Ok(n) if (-90.0..=90.0).contains(&n) => Ok(()),
                Ok(_) => Err("Latitude must be between -90 and 90".into()),
                Err(_) => Err("Latitude must be a number (e.g. 49.166) or blank".into()),
            }
        } else {
            Ok(())
        }
    }

    fn validate_longitude(v: &WizardValue) -> Result<(), String> {
        if let WizardValue::Text(s) = v {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Ok(());
            }
            match trimmed.parse::<f64>() {
                Ok(n) if (-180.0..=180.0).contains(&n) => Ok(()),
                Ok(_) => Err("Longitude must be between -180 and 180".into()),
                Err(_) => Err("Longitude must be a number (e.g. -123.133) or blank".into()),
            }
        } else {
            Ok(())
        }
    }

    WizardDescriptor {
        display_name: "Weather",
        blurb: "Open-Meteo current conditions and short-term forecast. \
                Leave latitude/longitude blank to use IP geolocation on \
                first fetch.",
        load_from_toml: None,
        render_toml: Some(render_weather_toml),
        fields: vec![
            WizardField {
                key: "label",
                label: "Location label",
                help: "Optional display name shown in the cell title \
                       (e.g. \"Richmond, BC\"). Falls back to the \
                       IP-geolocation result when blank.",
                required: false,
                kind: WizardFieldKind::Text {
                    default: None,
                    placeholder: Some("(use geolocation)"),
                },
                validate: None,
            },
            WizardField {
                key: "latitude",
                label: "Latitude",
                help: "Decimal degrees in [-90, 90]. Leave blank to \
                       IP-geolocate on first fetch.",
                required: false,
                kind: WizardFieldKind::Text {
                    default: None,
                    placeholder: Some("e.g. 49.166"),
                },
                validate: Some(validate_latitude),
            },
            WizardField {
                key: "longitude",
                label: "Longitude",
                help: "Decimal degrees in [-180, 180]. Leave blank to \
                       IP-geolocate on first fetch.",
                required: false,
                kind: WizardFieldKind::Text {
                    default: None,
                    placeholder: Some("e.g. -123.133"),
                },
                validate: Some(validate_longitude),
            },
            WizardField {
                key: "units",
                label: "Units",
                help: "\"metric\" — °C and km/h. \"imperial\" — °F and mph.",
                required: true,
                kind: WizardFieldKind::Choice {
                    options: vec![
                        ChoiceOption {
                            value: "metric",
                            label: "Metric (°C, km/h)",
                            help: None,
                        },
                        ChoiceOption {
                            value: "imperial",
                            label: "Imperial (°F, mph)",
                            help: None,
                        },
                    ],
                    default: Some("metric"),
                },
                validate: None,
            },
            WizardField {
                key: "poll_interval_secs",
                label: "Refresh interval (seconds)",
                help: "How often to fetch fresh conditions. Open-Meteo is \
                       fast and free; 600 (10 minutes) is plenty for a \
                       dashboard.",
                required: true,
                kind: WizardFieldKind::Number {
                    default: Some(600.0),
                    range: Some((30.0, 3600.0)),
                    integer: true,
                },
                validate: None,
            },
            WizardField {
                key: "auto_locate",
                label: "IP-geolocate when lat/lon are blank",
                help: "On — the widget calls ipapi.co on first fetch when \
                       no coordinates are configured. Off — the widget \
                       renders a \"location needed\" placeholder until \
                       coordinates are supplied.",
                required: false,
                kind: WizardFieldKind::Bool { default: true },
                validate: None,
            },
        ],
    }
}

/// Render weather.toml from wizard values. Optional fields (label, lat,
/// lon) are omitted when blank so the on-disk file parses cleanly into
/// `WeatherConfig` with its `Option<…>` shapes.
fn render_weather_toml(
    values: &std::collections::HashMap<String, crate::wizard::descriptor::WizardValue>,
    _existing: Option<&str>,
) -> String {
    use crate::wizard::descriptor::WizardValue;
    let mut out = String::from(
        "# Generated by `glint --setup`. Hand-edit freely; the wizard preserves\n\
         # advanced keys it doesn't manage (e.g. [colors], custom shortcuts).\n\n",
    );

    if let Some(WizardValue::Text(label)) = values.get("label") {
        let trimmed = label.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!("label = {}\n", weather_toml_quote(trimmed)));
        }
    }
    if let Some(lat) = optional_float(values.get("latitude")) {
        out.push_str(&format!("latitude = {lat}\n"));
    }
    if let Some(lon) = optional_float(values.get("longitude")) {
        out.push_str(&format!("longitude = {lon}\n"));
    }
    if let Some(WizardValue::Choice(units)) = values.get("units") {
        out.push_str(&format!("units = {}\n", weather_toml_quote(units)));
    }
    if let Some(WizardValue::Number(secs)) = values.get("poll_interval_secs") {
        out.push_str(&format!("poll_interval_secs = {}\n", *secs as i64));
    }
    if let Some(WizardValue::Bool(b)) = values.get("auto_locate") {
        out.push_str(&format!("auto_locate = {b}\n"));
    }
    out
}

/// Coerce either a Text("49.166") or a Number(49.166) wizard value into an
/// f64. Empty / unparseable / wrong-kind inputs return None so the caller
/// can omit the field from the rendered TOML.
fn optional_float(v: Option<&crate::wizard::descriptor::WizardValue>) -> Option<f64> {
    use crate::wizard::descriptor::WizardValue;
    match v? {
        WizardValue::Text(s) => s.trim().parse().ok(),
        WizardValue::Number(n) => Some(*n),
        _ => None,
    }
}

fn weather_toml_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn build(ctx: &super::WidgetCtx) -> Box<dyn super::Widget> {
    let cfg: WeatherConfig =
        crate::config::load_widget_toml_for_instance(KIND, &ctx.instance).unwrap_or_default();
    Box::new(WeatherWidget::with_config(
        ctx.instance.clone(),
        cfg,
        ctx.theme.clone(),
        ctx.cache.clone(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_widget(cfg: WeatherConfig) -> WeatherWidget {
        WeatherWidget::with_config(
            "main".to_string(),
            cfg,
            Arc::new(Theme::builtin_defaults()),
            ScopedCache::ephemeral(),
        )
    }

    #[test]
    fn render_weather_toml_omits_blank_optionals_and_roundtrips() {
        use crate::wizard::descriptor::WizardValue;
        use std::collections::HashMap;
        // Case 1: all-defaults, blank label + lat/lon → omitted in output.
        let mut values: HashMap<String, WizardValue> = HashMap::new();
        values.insert("label".into(), WizardValue::Text("".into()));
        values.insert("latitude".into(), WizardValue::Text("".into()));
        values.insert("longitude".into(), WizardValue::Text("".into()));
        values.insert("units".into(), WizardValue::Choice("metric".into()));
        values.insert("poll_interval_secs".into(), WizardValue::Number(600.0));
        values.insert("auto_locate".into(), WizardValue::Bool(true));
        let body = render_weather_toml(&values, None);
        assert!(!body.contains("label"));
        assert!(!body.contains("latitude"));
        assert!(!body.contains("longitude"));
        assert!(body.contains("units = \"metric\""));
        let parsed: WeatherConfig = toml::from_str(&body).expect("parses");
        assert!(parsed.label.is_none());
        assert!(parsed.latitude.is_none());
        assert!(parsed.longitude.is_none());
        assert!(parsed.auto_locate);

        // Case 2: explicit coords → keys present, deserialise to Some(_).
        values.insert("label".into(), WizardValue::Text("Richmond, BC".into()));
        values.insert("latitude".into(), WizardValue::Text("49.166".into()));
        values.insert("longitude".into(), WizardValue::Text("-123.133".into()));
        let body = render_weather_toml(&values, None);
        assert!(body.contains("label = \"Richmond, BC\""));
        assert!(body.contains("latitude = 49.166"));
        assert!(body.contains("longitude = -123.133"));
        let parsed: WeatherConfig = toml::from_str(&body).expect("parses");
        assert_eq!(parsed.label.as_deref(), Some("Richmond, BC"));
        assert!((parsed.latitude.unwrap() - 49.166).abs() < 1e-9);
        assert!((parsed.longitude.unwrap() - -123.133).abs() < 1e-9);
    }

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
        let w = build_widget(cfg);
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
        let w = build_widget(cfg);
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
        let w = build_widget(cfg);
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
        let w = build_widget(cfg);
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
        let w = build_widget(cfg);
        assert!(matches!(w.next_action(), NextAction::Wait));
    }
}
