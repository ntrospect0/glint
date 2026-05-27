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
}

#[derive(Clone)]
pub struct OpenMeteoProvider {
    client: reqwest::Client,
    latitude: f64,
    longitude: f64,
    units: Units,
    base_url: String,
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
        }
    }

    fn build_url(&self) -> String {
        format!(
            "{base}?latitude={lat}&longitude={lon}&current=temperature_2m,relative_humidity_2m,apparent_temperature,weather_code,wind_speed_10m&temperature_unit={temp}&wind_speed_unit={wind}",
            base = self.base_url,
            lat = self.latitude,
            lon = self.longitude,
            temp = self.units.temp_unit_param(),
            wind = self.units.wind_unit_param(),
        )
    }
}

#[derive(Debug, Deserialize)]
struct OpenMeteoResponse {
    current: OpenMeteoCurrent,
}

#[derive(Debug, Deserialize)]
struct OpenMeteoCurrent {
    temperature_2m: f64,
    relative_humidity_2m: f64,
    apparent_temperature: f64,
    weather_code: u32,
    wind_speed_10m: f64,
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
        Ok(WeatherData {
            temperature: resp.current.temperature_2m,
            apparent_temperature: resp.current.apparent_temperature,
            humidity: resp.current.relative_humidity_2m,
            wind_speed: resp.current.wind_speed_10m,
            weather_code: resp.current.weather_code,
            units: self.units,
            fetched_at: chrono::Local::now(),
        })
    }

    fn name(&self) -> &str {
        "open-meteo"
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
    fn describe_code_covers_common_categories() {
        assert_eq!(describe_code(0).0, "Clear");
        assert_eq!(describe_code(3).0, "Overcast");
        assert_eq!(describe_code(65).0, "Rain");
        assert_eq!(describe_code(95).0, "Thunderstorm");
        assert_eq!(describe_code(9999).0, "Unknown");
    }
}
