// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

use anyhow::{Context, Result};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use serde::{Deserialize, Serialize};

use super::icons::{self, WeatherIcon};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Units {
    Metric,
    Imperial,
}

impl Units {
    fn temp_unit_param(self) -> &'static str {
        match self {
            Units::Metric => "celsius",
            Units::Imperial => "fahrenheit",
        }
    }
    fn wind_unit_param(self) -> &'static str {
        match self {
            Units::Metric => "kmh",
            Units::Imperial => "mph",
        }
    }
    pub fn temp_symbol(self) -> &'static str {
        match self {
            Units::Metric => "°C",
            Units::Imperial => "°F",
        }
    }
    pub fn wind_label(self) -> &'static str {
        match self {
            Units::Metric => "km/h",
            Units::Imperial => "mph",
        }
    }
}

/// One hour of forecast data returned by the Open-Meteo hourly endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HourlyPoint {
    /// Wall-clock hour in the location's local timezone (`timezone=auto`).
    /// Stored as `NaiveDateTime` because Open-Meteo returns times without an
    /// offset suffix; callers compare against `Local::now().naive_local()`.
    pub time: chrono::NaiveDateTime,
    /// Air temperature at 2 m in the configured unit.
    pub temperature: f64,
    /// Precipitation probability 0–100.
    pub precipitation_probability: f64,
}

/// Snapshot of current conditions as we parse them out of the Open-Meteo response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeatherData {
    pub temperature: f64,
    pub apparent_temperature: f64,
    pub humidity: f64,
    pub wind_speed: f64,
    /// Wind direction in degrees (0–360, clockwise from north). Absent from
    /// older cached snapshots; `None` degrades gracefully.
    #[serde(default)]
    pub wind_direction: Option<f64>,
    pub weather_code: u32,
    pub units: Units,
    pub fetched_at: chrono::DateTime<chrono::Local>,
    /// Daily forecast for the next few days, starting with today. Includes
    /// each day's sunrise/sunset so the renderer can swap day/night sprites.
    pub daily: Vec<DailyForecast>,
    /// Hourly temperature + precipitation for the next ~48 h.
    /// Empty in older cached snapshots; degrades gracefully.
    #[serde(default)]
    pub hourly: Vec<HourlyPoint>,
}

impl WeatherData {
    /// True when `now` falls outside today's `[sunrise, sunset)` window.
    /// If we couldn't get today's sunrise/sunset, fall back to a simple
    /// 06:00–18:00 day rule rather than mislabel everything as one or the
    /// other.
    pub fn is_night(&self, now: chrono::DateTime<chrono::Local>) -> bool {
        use chrono::Timelike;
        let today = now.date_naive();
        let today_entry = self.daily.iter().find(|d| d.date == today);
        match today_entry.and_then(|d| d.sunrise.zip(d.sunset)) {
            Some((sunrise, sunset)) => now < sunrise || now >= sunset,
            None => !(6..18).contains(&now.hour()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyForecast {
    pub date: chrono::NaiveDate,
    pub temperature_high: f64,
    pub temperature_low: f64,
    pub weather_code: u32,
    /// Local sunrise / sunset for this date. `None` if Open-Meteo didn't
    /// return them (e.g. polar day or polar night), in which case the
    /// caller should fall back to a heuristic.
    pub sunrise: Option<chrono::DateTime<chrono::Local>>,
    pub sunset: Option<chrono::DateTime<chrono::Local>>,
    /// Maximum precipitation probability for the day (0–100). Absent from
    /// older cached snapshots; `None` degrades gracefully.
    #[serde(default)]
    pub precipitation_probability_max: Option<f64>,
    /// Maximum UV index for the day. Absent from older cached snapshots.
    #[serde(default)]
    pub uv_index_max: Option<f64>,
}

#[derive(Clone)]
pub struct OpenMeteoProvider {
    client: reqwest::Client,
    latitude: f64,
    longitude: f64,
    units: Units,
    base_url: String,
    forecast_days: u32,
}

impl OpenMeteoProvider {
    pub fn new(latitude: f64, longitude: f64, units: Units) -> Self {
        let client = crate::http::shared();
        Self {
            client,
            latitude,
            longitude,
            units,
            base_url: "https://api.open-meteo.com/v1/forecast".into(),
            forecast_days: 8, // today + next 7
        }
    }

    fn build_url(&self) -> String {
        format!(
            "{base}?latitude={lat}&longitude={lon}\
            &current=temperature_2m,relative_humidity_2m,apparent_temperature,weather_code,wind_speed_10m,wind_direction_10m\
            &daily=weather_code,temperature_2m_max,temperature_2m_min,sunrise,sunset,precipitation_probability_max,uv_index_max\
            &hourly=temperature_2m,precipitation_probability\
            &forecast_days={days}&temperature_unit={temp}&wind_speed_unit={wind}&timezone=auto",
            base = self.base_url,
            lat = self.latitude,
            lon = self.longitude,
            days = self.forecast_days,
            temp = self.units.temp_unit_param(),
            wind = self.units.wind_unit_param(),
        )
    }
}

#[derive(Debug, Deserialize)]
struct OpenMeteoResponse {
    current: OpenMeteoCurrent,
    #[serde(default)]
    daily: Option<OpenMeteoDaily>,
    #[serde(default)]
    hourly: Option<OpenMeteoHourly>,
}

#[derive(Debug, Deserialize)]
struct OpenMeteoCurrent {
    temperature_2m: f64,
    relative_humidity_2m: f64,
    apparent_temperature: f64,
    weather_code: u32,
    wind_speed_10m: f64,
    #[serde(default)]
    wind_direction_10m: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct OpenMeteoDaily {
    time: Vec<String>,
    weather_code: Vec<u32>,
    temperature_2m_max: Vec<f64>,
    temperature_2m_min: Vec<f64>,
    /// ISO 8601 local datetimes (no offset suffix because we request
    /// `timezone=auto`, which makes Open-Meteo return wall-clock times for
    /// the requested location). `None` per day when Open-Meteo can't compute
    /// one — polar day/night, etc.
    #[serde(default)]
    sunrise: Vec<Option<String>>,
    #[serde(default)]
    sunset: Vec<Option<String>>,
    #[serde(default)]
    precipitation_probability_max: Vec<Option<f64>>,
    #[serde(default)]
    uv_index_max: Vec<Option<f64>>,
}

/// Hourly arrays from Open-Meteo (`hourly=temperature_2m,precipitation_probability`).
#[derive(Debug, Deserialize)]
struct OpenMeteoHourly {
    time: Vec<String>,
    temperature_2m: Vec<f64>,
    #[serde(default)]
    precipitation_probability: Vec<Option<f64>>,
}

impl OpenMeteoProvider {
    pub async fn fetch(&self) -> Result<WeatherData> {
        let url = self.build_url();
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("open-meteo request failed")?
            .error_for_status()
            .context("open-meteo returned non-2xx status")?
            .json::<OpenMeteoResponse>()
            .await
            .context("failed to deserialize open-meteo response")?;
        let daily = resp.daily.map(parse_daily).unwrap_or_default();
        let hourly = resp.hourly.map(parse_hourly).unwrap_or_default();
        Ok(WeatherData {
            temperature: resp.current.temperature_2m,
            apparent_temperature: resp.current.apparent_temperature,
            humidity: resp.current.relative_humidity_2m,
            wind_speed: resp.current.wind_speed_10m,
            wind_direction: resp.current.wind_direction_10m,
            weather_code: resp.current.weather_code,
            units: self.units,
            fetched_at: chrono::Local::now(),
            daily,
            hourly,
        })
    }
}

fn parse_daily(daily: OpenMeteoDaily) -> Vec<DailyForecast> {
    let n = daily
        .time
        .len()
        .min(daily.weather_code.len())
        .min(daily.temperature_2m_max.len())
        .min(daily.temperature_2m_min.len());
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        if let Ok(date) = chrono::NaiveDate::parse_from_str(&daily.time[i], "%Y-%m-%d") {
            out.push(DailyForecast {
                date,
                weather_code: daily.weather_code[i],
                temperature_high: daily.temperature_2m_max[i],
                temperature_low: daily.temperature_2m_min[i],
                sunrise: daily
                    .sunrise
                    .get(i)
                    .and_then(|s| s.as_deref())
                    .and_then(parse_local_dt),
                sunset: daily
                    .sunset
                    .get(i)
                    .and_then(|s| s.as_deref())
                    .and_then(parse_local_dt),
                precipitation_probability_max: daily
                    .precipitation_probability_max
                    .get(i)
                    .and_then(|x| *x),
                uv_index_max: daily.uv_index_max.get(i).and_then(|x| *x),
            });
        }
    }
    out
}

/// Parse Open-Meteo hourly arrays into `HourlyPoint` values. Times are
/// in the same `%Y-%m-%dT%H:%M` local-wall-clock format as `parse_local_dt`.
fn parse_hourly(h: OpenMeteoHourly) -> Vec<HourlyPoint> {
    let n = h.time.len().min(h.temperature_2m.len());
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        if let Ok(time) =
            chrono::NaiveDateTime::parse_from_str(&h.time[i], "%Y-%m-%dT%H:%M")
        {
            out.push(HourlyPoint {
                time,
                temperature: h.temperature_2m[i],
                precipitation_probability: h
                    .precipitation_probability
                    .get(i)
                    .and_then(|x| *x)
                    .unwrap_or(0.0),
            });
        }
    }
    out
}

/// Parse Open-Meteo's `timezone=auto` ISO datetimes (no offset suffix —
/// they're already in the location's wall clock). We turn them into
/// `DateTime<Local>` so comparison with `chrono::Local::now()` is direct.
fn parse_local_dt(s: &str) -> Option<chrono::DateTime<chrono::Local>> {
    use chrono::TimeZone;
    let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M").ok()?;
    chrono::Local.from_local_datetime(&naive).single()
}

/// Maps a WMO weather code to a pixel-accurate icon. When `night` is true,
/// clear-sky and partly-cloudy sprites swap to their MOON / MOON_CLOUD
/// counterparts; everything else (rain, snow, fog, thunder, etc.) renders
/// identically day or night since we don't have separate night sprites.
pub fn icon_for_code(code: u32, night: bool) -> &'static WeatherIcon {
    match code {
        0 | 1 => {
            if night {
                &icons::MOON
            } else {
                &icons::SUN
            }
        }
        2 => {
            if night {
                &icons::MOON_CLOUD
            } else {
                &icons::SUN_CLOUD
            }
        }
        3 => &icons::CLOUD,
        45 | 48 => &icons::FOG,
        // Drizzle, plain rain, freezing variants all render as the cloud +
        // droplets glyph. We could split freezing rain (66/67) to WET_SNOW
        // later if we want to distinguish sleety conditions visually.
        51 | 53 | 55 | 56 | 57 | 61 | 63 | 65 | 66 | 67 => &icons::RAIN,
        // Plain snow (codes 71-77) and snow showers (85/86).
        71 | 73 | 75 | 77 | 85 | 86 => &icons::SNOW,
        // Rain showers — the burstier sibling of plain rain.
        80 | 81 | 82 => &icons::SHOWERS,
        // Thunderstorm: 95 is the plain form; 96/99 (with hail) gets the
        // heavier THUNDER_RAIN glyph.
        95 => &icons::THUNDER,
        96 | 99 => &icons::THUNDER_RAIN,
        _ => &icons::CLOUD,
    }
}

/// Render a `WeatherIcon` as Ratatui Lines using the half-block
/// trick: two pixel rows collapse into one terminal row, with `fg`
/// driving the top half and `bg` (when present) driving the bottom.
///
/// `max_char_rows` clips the output vertically with a symmetric
/// top/bottom crop — tall sprites (e.g. `MOON_CLOUD` at 10 char
/// rows) trim one char row off each end to fit the weather widget's
/// 8-row art slot. Pass `None` to render the full sprite.
pub fn render_icon_clipped(icon: &WeatherIcon, max_char_rows: Option<u16>) -> Vec<Line<'static>> {
    let h = icon.height as usize;
    let w = icon.width as usize;
    let total_char_rows = h.div_ceil(2);
    let char_rows = match max_char_rows {
        Some(cap) => total_char_rows.min(cap as usize),
        None => total_char_rows,
    };
    // Symmetric center-crop in *char-row* units so the half-block
    // pairing stays intact — clipping at an odd pixel offset would
    // shift every pair by one and break the silhouette.
    let crop_top_char_rows = total_char_rows.saturating_sub(char_rows) / 2;
    let crop_top_px = crop_top_char_rows * 2;
    let mut lines = Vec::with_capacity(char_rows);
    for char_row in 0..char_rows {
        let top_idx = crop_top_px + char_row * 2;
        let bot_idx = top_idx + 1;
        let top_row = icon.pixels.get(top_idx).copied().unwrap_or(&[]);
        let bot_row = icon.pixels.get(bot_idx).copied().unwrap_or(&[]);
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(w);
        for col in 0..w {
            let top = top_row
                .get(col)
                .and_then(|x| *x)
                .map(|i| icon.palette[i as usize]);
            let bot = bot_row
                .get(col)
                .and_then(|x| *x)
                .map(|i| icon.palette[i as usize]);
            let (ch, style) = cell_style(top, bot);
            spans.push(Span::styled(ch.to_string(), style));
        }
        lines.push(Line::from(spans));
    }
    lines
}

fn cell_style(top: Option<Color>, bot: Option<Color>) -> (char, Style) {
    match (top, bot) {
        (None, None) => (' ', Style::default()),
        (Some(c), None) => ('▀', Style::default().fg(c)),
        (None, Some(c)) => ('▄', Style::default().fg(c)),
        (Some(t), Some(b)) if t == b => ('█', Style::default().fg(t)),
        (Some(t), Some(b)) => ('▀', Style::default().fg(t).bg(b)),
    }
}

/// Maps a WMO weather code (returned by Open-Meteo's `weather_code` field) to a
/// short human label and a single-glyph icon. See
/// https://open-meteo.com/en/docs#api_form for the code table.
pub fn describe_code(code: u32) -> (&'static str, &'static str) {
    // Each glyph carries a trailing U+FE0F variation selector to force emoji
    // presentation. Without it, codepoints like ☀ ☁ ❄ ⛈ ⛅ are ambiguous —
    // `unicode-width` reports them as 1 cell, but many terminals render them
    // 2 cells wide. The mismatch makes Ratatui mis-lay-out the line and the
    // right side of the glyph gets clipped. VS-16 makes both agree on width 2.
    match code {
        0 => ("Clear", "☀\u{FE0F}"),
        1 => ("Mostly clear", "🌤\u{FE0F}"),
        2 => ("Partly cloudy", "⛅\u{FE0F}"),
        3 => ("Overcast", "☁\u{FE0F}"),
        45 | 48 => ("Fog", "🌫\u{FE0F}"),
        51 | 53 | 55 => ("Drizzle", "🌦\u{FE0F}"),
        56 | 57 => ("Freezing drizzle", "🌧\u{FE0F}"),
        61 | 63 | 65 => ("Rain", "🌧\u{FE0F}"),
        66 | 67 => ("Freezing rain", "🌧\u{FE0F}"),
        71 | 73 | 75 => ("Snow", "🌨\u{FE0F}"),
        77 => ("Snow grains", "❄\u{FE0F}"),
        80..=82 => ("Rain showers", "🌧\u{FE0F}"),
        85 | 86 => ("Snow showers", "🌨\u{FE0F}"),
        95 => ("Thunderstorm", "⛈\u{FE0F}"),
        96 | 99 => ("Thunderstorm w/ hail", "⛈\u{FE0F}"),
        _ => ("Unknown", "·"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_contains_lat_lon_and_units() {
        let p = OpenMeteoProvider::new(40.7128, -74.006, Units::Imperial);
        let url = p.build_url();
        assert!(url.contains("latitude=40.7128"));
        assert!(url.contains("longitude=-74.006"));
        assert!(url.contains("temperature_unit=fahrenheit"));
        assert!(url.contains("wind_speed_unit=mph"));
    }

    #[test]
    fn parse_daily_zips_aligned_arrays() {
        let raw = OpenMeteoDaily {
            time: vec![
                "2026-05-20".into(),
                "2026-05-21".into(),
                "2026-05-22".into(),
            ],
            weather_code: vec![0, 3, 61],
            temperature_2m_max: vec![22.0, 19.5, 17.0],
            temperature_2m_min: vec![12.0, 11.0, 10.0],
            sunrise: vec![Some("2026-05-20T05:30".into()), None, None],
            sunset: vec![Some("2026-05-20T20:15".into()), None, None],
            precipitation_probability_max: vec![Some(30.0), None, Some(80.0)],
            uv_index_max: vec![Some(5.0), None, None],
        };
        let out = parse_daily(raw);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].weather_code, 0);
        assert_eq!(out[1].temperature_high, 19.5);
        assert_eq!(out[2].temperature_low, 10.0);
        assert!(out[0].sunrise.is_some());
        assert!(out[1].sunrise.is_none());
        assert_eq!(out[0].precipitation_probability_max, Some(30.0));
        assert_eq!(out[1].precipitation_probability_max, None);
        assert_eq!(out[0].uv_index_max, Some(5.0));
        assert_eq!(out[2].uv_index_max, None);
    }

    #[test]
    fn parse_daily_truncates_to_shortest_array() {
        let raw = OpenMeteoDaily {
            time: vec!["2026-05-20".into(), "2026-05-21".into()],
            weather_code: vec![0],
            temperature_2m_max: vec![22.0, 19.5],
            temperature_2m_min: vec![12.0, 11.0],
            sunrise: vec![],
            sunset: vec![],
            precipitation_probability_max: vec![],
            uv_index_max: vec![],
        };
        assert_eq!(parse_daily(raw).len(), 1);
    }

    #[test]
    fn parse_hourly_zips_time_and_values() {
        let raw = OpenMeteoHourly {
            time: vec![
                "2026-05-20T12:00".into(),
                "2026-05-20T13:00".into(),
                "2026-05-20T14:00".into(),
            ],
            temperature_2m: vec![18.0, 19.5, 21.0],
            precipitation_probability: vec![Some(10.0), None, Some(60.0)],
        };
        let out = parse_hourly(raw);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].temperature, 18.0);
        assert_eq!(out[1].precipitation_probability, 0.0); // None → 0
        assert_eq!(out[2].precipitation_probability, 60.0);
    }

    #[test]
    fn parse_hourly_missing_precip_field_defaults_to_zero() {
        let raw = OpenMeteoHourly {
            time: vec!["2026-05-20T12:00".into()],
            temperature_2m: vec![20.0],
            precipitation_probability: vec![], // empty → defaults
        };
        let out = parse_hourly(raw);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].precipitation_probability, 0.0);
    }

    #[test]
    fn rendered_icon_dimensions_match_its_declared_size() {
        for code in [0, 1, 2, 3, 45, 48, 61, 75, 80, 95, 96, 9999] {
            let icon = icon_for_code(code, false);
            let lines = render_icon_clipped(icon, None);
            let expected_rows = (icon.height as usize).div_ceil(2);
            assert_eq!(
                lines.len(),
                expected_rows,
                "code {code}: row count should be ceil(height/2)"
            );
            for (i, line) in lines.iter().enumerate() {
                let total: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
                assert_eq!(
                    total, icon.width as usize,
                    "code {code} row {i}: width should be {}",
                    icon.width
                );
            }
        }
    }

    #[test]
    fn night_swaps_sun_for_moon() {
        // Each icon in the set has unique (width, height) so we can use that
        // as identity without relying on pointer equality, which is brittle
        // for const items (rustc may dedup or inline them).
        let dims = |i: &WeatherIcon| (i.width, i.height);
        assert_eq!(dims(icon_for_code(0, false)), dims(&icons::SUN));
        assert_eq!(dims(icon_for_code(0, true)), dims(&icons::MOON));
        assert_eq!(dims(icon_for_code(2, false)), dims(&icons::SUN_CLOUD));
        assert_eq!(dims(icon_for_code(2, true)), dims(&icons::MOON_CLOUD));
        // Non-sun codes are unaffected by night flag.
        assert_eq!(
            dims(icon_for_code(61, false)),
            dims(icon_for_code(61, true))
        );
        assert_eq!(
            dims(icon_for_code(95, false)),
            dims(icon_for_code(95, true))
        );
    }

    #[test]
    fn is_night_uses_today_sunrise_sunset() {
        use chrono::TimeZone;
        let today = chrono::Local::now().date_naive();
        let sunrise = chrono::Local
            .from_local_datetime(&today.and_hms_opt(6, 0, 0).unwrap())
            .single()
            .unwrap();
        let sunset = chrono::Local
            .from_local_datetime(&today.and_hms_opt(20, 0, 0).unwrap())
            .single()
            .unwrap();
        let data = WeatherData {
            temperature: 0.0,
            apparent_temperature: 0.0,
            humidity: 0.0,
            wind_speed: 0.0,
            wind_direction: None,
            weather_code: 0,
            units: Units::Metric,
            fetched_at: chrono::Local::now(),
            daily: vec![DailyForecast {
                date: today,
                temperature_high: 0.0,
                temperature_low: 0.0,
                weather_code: 0,
                sunrise: Some(sunrise),
                sunset: Some(sunset),
                precipitation_probability_max: None,
                uv_index_max: None,
            }],
            hourly: vec![],
        };
        let noon = chrono::Local
            .from_local_datetime(&today.and_hms_opt(12, 0, 0).unwrap())
            .single()
            .unwrap();
        let midnight = chrono::Local
            .from_local_datetime(&today.and_hms_opt(2, 0, 0).unwrap())
            .single()
            .unwrap();
        let evening = chrono::Local
            .from_local_datetime(&today.and_hms_opt(22, 0, 0).unwrap())
            .single()
            .unwrap();
        assert!(!data.is_night(noon));
        assert!(data.is_night(midnight));
        assert!(data.is_night(evening));
    }

    #[test]
    fn max_dimensions_cover_every_icon() {
        use crate::widgets::weather::icons::*;
        let all = [
            &CLOUD,
            &RAIN,
            &FOG,
            &THUNDER,
            &SUN,
            &SUN_CLOUD,
            &SNOW,
            &SHOWERS,
            &MOON,
            &WET_SNOW,
            &TORNADO,
            &MOON_CLOUD,
            &LIGHTNING_BOLT,
            &THUNDER_SHOWERS,
            &SUN_STORM,
            &THUNDER_RAIN,
        ];
        for icon in all {
            assert!(icon.width <= MAX_WIDTH, "icon wider than MAX_WIDTH");
            assert!(
                icon.height <= MAX_HEIGHT_PX,
                "icon taller than MAX_HEIGHT_PX"
            );
        }
    }

    #[test]
    fn describe_code_covers_common_categories() {
        assert_eq!(describe_code(0).0, "Clear");
        assert_eq!(describe_code(3).0, "Overcast");
        assert_eq!(describe_code(65).0, "Rain");
        assert_eq!(describe_code(95).0, "Thunderstorm");
        assert_eq!(describe_code(9999).0, "Unknown");
    }
}
