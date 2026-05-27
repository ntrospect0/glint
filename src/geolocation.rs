use anyhow::{Context, Result};
use serde::Deserialize;

/// Best-effort IP-based location lookup. Used as a fallback when the user
/// hasn't configured an explicit lat/lon in a widget.
#[derive(Debug, Clone)]
pub struct GeoLocation {
    pub latitude: f64,
    pub longitude: f64,
    pub label: String,
    #[allow(dead_code)] // surfaced when the clock widget gains auto-locate (later phase).
    pub timezone: Option<String>,
}

#[derive(Debug, Deserialize)]
struct IpApiResponse {
    latitude: f64,
    longitude: f64,
    city: Option<String>,
    region: Option<String>,
    country_name: Option<String>,
    timezone: Option<String>,
}

/// Resolve a free-form place name to a `GeoLocation` via Open-Meteo's free
/// geocoding API (no key). Returns the top match — usually accurate even for
/// short queries like "Vancouver" (defaults to the largest one by population).
pub async fn by_name(name: &str) -> Result<GeoLocation> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .user_agent(concat!("glint-tui/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to build geocoding HTTP client")?;
    let url = format!(
        "https://geocoding-api.open-meteo.com/v1/search?name={q}&count=1",
        q = urlencoding::encode(name)
    );
    let resp = client
        .get(&url)
        .send()
        .await
        .context("open-meteo geocoding request failed")?
        .error_for_status()
        .context("open-meteo geocoding returned non-2xx")?
        .json::<GeocodingResponse>()
        .await
        .context("failed to deserialize open-meteo geocoding response")?;
    let hit = resp
        .results
        .and_then(|r| r.into_iter().next())
        .with_context(|| format!("no geocoding result for {name:?}"))?;
    let mut label = hit.name.clone();
    if let Some(admin1) = hit.admin1 {
        label.push_str(", ");
        label.push_str(&admin1);
    }
    if let Some(country) = hit.country {
        label.push_str(", ");
        label.push_str(&country);
    }
    Ok(GeoLocation {
        latitude: hit.latitude,
        longitude: hit.longitude,
        label,
        timezone: hit.timezone,
    })
}

#[derive(Debug, Deserialize)]
struct GeocodingResponse {
    #[serde(default)]
    results: Option<Vec<GeocodingHit>>,
}

#[derive(Debug, Deserialize)]
struct GeocodingHit {
    name: String,
    latitude: f64,
    longitude: f64,
    #[serde(default)]
    admin1: Option<String>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    timezone: Option<String>,
}

/// Geolocate by the caller's egress IP via ipapi.co (free, HTTPS, no API key).
/// Returns an error if the request fails or the response is malformed — callers
/// are expected to fall back to a sensible default.
pub async fn by_ip() -> Result<GeoLocation> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .user_agent(concat!("glint-tui/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to build geolocation HTTP client")?;
    let resp = client
        .get("https://ipapi.co/json/")
        .send()
        .await
        .context("ipapi.co request failed")?
        .error_for_status()
        .context("ipapi.co returned non-2xx")?
        .json::<IpApiResponse>()
        .await
        .context("failed to deserialize ipapi.co response")?;
    let city = resp.city.clone().unwrap_or_else(|| "Unknown".into());
    let label = match (&resp.region, &resp.country_name) {
        (Some(r), Some(c)) => format!("{city}, {r}, {c}"),
        (Some(r), None) => format!("{city}, {r}"),
        (None, Some(c)) => format!("{city}, {c}"),
        (None, None) => city,
    };
    Ok(GeoLocation {
        latitude: resp.latitude,
        longitude: resp.longitude,
        label,
        timezone: resp.timezone,
    })
}
