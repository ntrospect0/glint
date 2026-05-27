use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use crate::providers::DataProvider;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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

/// Snapshot of current conditions as we parse them out of the Open-Meteo response.
#[derive(Debug, Clone)]
pub struct WeatherData {
    pub temperature: f64,
    pub apparent_temperature: f64,
    pub humidity: f64,
    pub wind_speed: f64,
    pub weather_code: u32,
    pub units: Units,
    pub fetched_at: chrono::DateTime<chrono::Local>,
    /// Daily forecast for the next few days, starting with today.
    pub daily: Vec<DailyForecast>,
}

#[derive(Debug, Clone)]
pub struct DailyForecast {
    pub date: chrono::NaiveDate,
    pub temperature_high: f64,
    pub temperature_low: f64,
    pub weather_code: u32,
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
        let client = reqwest::Client::builder()
            .user_agent(concat!("glint-tui/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("reqwest client should build with default features");
        Self {
            client,
            latitude,
            longitude,
            units,
            base_url: "https://api.open-meteo.com/v1/forecast".into(),
            forecast_days: 4, // today + next 3
        }
    }

    fn build_url(&self) -> String {
        format!(
            "{base}?latitude={lat}&longitude={lon}&current=temperature_2m,relative_humidity_2m,apparent_temperature,weather_code,wind_speed_10m&daily=weather_code,temperature_2m_max,temperature_2m_min&forecast_days={days}&temperature_unit={temp}&wind_speed_unit={wind}&timezone=auto",
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
}

#[derive(Debug, Deserialize)]
struct OpenMeteoCurrent {
    temperature_2m: f64,
    relative_humidity_2m: f64,
    apparent_temperature: f64,
    weather_code: u32,
    wind_speed_10m: f64,
}

#[derive(Debug, Deserialize)]
struct OpenMeteoDaily {
    time: Vec<String>,
    weather_code: Vec<u32>,
    temperature_2m_max: Vec<f64>,
    temperature_2m_min: Vec<f64>,
}

#[async_trait]
impl DataProvider for OpenMeteoProvider {
    type Data = WeatherData;

    async fn fetch(&self) -> Result<WeatherData> {
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
        Ok(WeatherData {
            temperature: resp.current.temperature_2m,
            apparent_temperature: resp.current.apparent_temperature,
            humidity: resp.current.relative_humidity_2m,
            wind_speed: resp.current.wind_speed_10m,
            weather_code: resp.current.weather_code,
            units: self.units,
            fetched_at: chrono::Local::now(),
            daily,
        })
    }

    fn name(&self) -> &str {
        "open-meteo"
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
            });
        }
    }
    out
}

/// Compact 4-row ASCII art for the current condition. Inspired by wttr.in's
/// glyph set but trimmed for narrow terminal cells (each row is 13 columns).
pub fn ascii_art(code: u32) -> [&'static str; 4] {
    match code {
        // Clear / mostly clear → sun
        0 | 1 => [
            "    \\   /    ",
            "     .-.     ",
            "  - (   ) -  ",
            "    `-'      ",
        ],
        // Partly cloudy → sun + cloud
        2 => [
            "    \\  /     ",
            "  _ /''.-.   ",
            "    \\_(   ). ",
            "    /(___(__)",
        ],
        // Overcast / fog → cloud
        3 | 45 | 48 => [
            "             ",
            "     .--.    ",
            "  .-(    ).  ",
            " (___.__)__) ",
        ],
        // Drizzle / rain / showers → cloud + drops
        51 | 53 | 55 | 56 | 57 | 61 | 63 | 65 | 66 | 67 | 80 | 81 | 82 => [
            "     .--.    ",
            "  .-(    ).  ",
            " (___.__)__) ",
            "   ' ' ' '   ",
        ],
        // Snow → cloud + flakes
        71 | 73 | 75 | 77 | 85 | 86 => [
            "     .--.    ",
            "  .-(    ).  ",
            " (___.__)__) ",
            "   *  *  *   ",
        ],
        // Thunderstorm → cloud + bolt
        95 | 96 | 99 => [
            "     .--.    ",
            "  .-(    ).  ",
            " (___.__)__) ",
            "    /_  /_   ",
        ],
        _ => [
            "             ",
            "     ·       ",
            "   · · ·     ",
            "     ·       ",
        ],
    }
}

/// Maps a WMO weather code (returned by Open-Meteo's `weather_code` field) to a
/// short human label and a single-glyph icon. See
/// https://open-meteo.com/en/docs#api_form for the code table.
pub fn describe_code(code: u32) -> (&'static str, &'static str) {
    match code {
        0 => ("Clear", "☀"),
        1 => ("Mostly clear", "🌤"),
        2 => ("Partly cloudy", "⛅"),
        3 => ("Overcast", "☁"),
        45 | 48 => ("Fog", "🌫"),
        51 | 53 | 55 => ("Drizzle", "🌦"),
        56 | 57 => ("Freezing drizzle", "🌧"),
        61 | 63 | 65 => ("Rain", "🌧"),
        66 | 67 => ("Freezing rain", "🌧"),
        71 | 73 | 75 => ("Snow", "🌨"),
        77 => ("Snow grains", "❄"),
        80..=82 => ("Rain showers", "🌧"),
        85 | 86 => ("Snow showers", "🌨"),
        95 => ("Thunderstorm", "⛈"),
        96 | 99 => ("Thunderstorm w/ hail", "⛈"),
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
            time: vec!["2026-05-20".into(), "2026-05-21".into(), "2026-05-22".into()],
            weather_code: vec![0, 3, 61],
            temperature_2m_max: vec![22.0, 19.5, 17.0],
            temperature_2m_min: vec![12.0, 11.0, 10.0],
        };
        let out = parse_daily(raw);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].weather_code, 0);
        assert_eq!(out[1].temperature_high, 19.5);
        assert_eq!(out[2].temperature_low, 10.0);
    }

    #[test]
    fn parse_daily_truncates_to_shortest_array() {
        let raw = OpenMeteoDaily {
            time: vec!["2026-05-20".into(), "2026-05-21".into()],
            weather_code: vec![0],
            temperature_2m_max: vec![22.0, 19.5],
            temperature_2m_min: vec![12.0, 11.0],
        };
        assert_eq!(parse_daily(raw).len(), 1);
    }

    #[test]
    fn ascii_art_is_always_four_rows() {
        for code in [0, 2, 3, 61, 75, 95, 9999] {
            let art = ascii_art(code);
            assert_eq!(art.len(), 4);
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
