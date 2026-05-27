// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

/// Best-effort IP-based location lookup. Used as a fallback when the user
/// hasn't configured an explicit lat/lon in a widget.
#[derive(Debug, Clone)]
pub struct GeoLocation {
    pub latitude: f64,
    pub longitude: f64,
    pub label: String,
    /// IANA timezone string when the lookup returned one. Consumed by the
    /// clock widget's `:clock <city>` flow to set a transient secondary
    /// zone. `None` is normal for the IP-geolocation path which we don't
    /// always request the timezone from.
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
/// geocoding API (no key).
///
/// Accepts inputs like:
///   `Vancouver`                — top match wins
///   `Troy, MI`                 — city + state hint (abbreviations OK)
///   `Troy, Michigan`           — city + full state name
///   `Troy, MI, USA`            — city + state + country (any combination of
///                                abbreviation / full name)
///   `Paris, France`            — city + country
///
/// Open-Meteo's `?name=` accepts only a city, not a full address — so we
/// parse commas client-side, fetch the top 10 candidates for the city, then
/// rank by how well each candidate's `admin1` (state/region) and `country`
/// fields match the user's hints. State and country abbreviations are
/// expanded against built-in tables (50 US states + DC, 13 Canadian
/// provinces, common country codes).
pub async fn by_name(name: &str) -> Result<GeoLocation> {
    let (city, admin_hint, country_hint) = parse_query(name);
    if city.is_empty() {
        return Err(anyhow!("empty geocoding query"));
    }

    let client = crate::http::shared();
    let url = format!(
        "https://geocoding-api.open-meteo.com/v1/search?name={q}&count=10",
        q = urlencoding::encode(&city)
    );
    let resp = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
        .context("open-meteo geocoding request failed")?
        .error_for_status()
        .context("open-meteo geocoding returned non-2xx")?
        .json::<GeocodingResponse>()
        .await
        .context("failed to deserialize open-meteo geocoding response")?;
    let mut results: Vec<GeocodingHit> = resp.results.unwrap_or_default();
    if results.is_empty() {
        return Err(anyhow!("no geocoding result for {name:?}"));
    }

    // Rank candidates by hint match. Open-Meteo orders results by
    // population by default, so when the user supplies no hint the top
    // result (largest "Troy" → Troy, MI) wins anyway. When hints exist,
    // a match overrides that ordering.
    results.sort_by(|a, b| {
        score_hit(b, admin_hint.as_deref(), country_hint.as_deref())
            .cmp(&score_hit(a, admin_hint.as_deref(), country_hint.as_deref()))
    });
    let hit = results.into_iter().next().expect("non-empty checked above");

    let mut label = hit.name.clone();
    if let Some(admin1) = hit.admin1.as_ref() {
        label.push_str(", ");
        label.push_str(admin1);
    }
    if let Some(country) = hit.country.as_ref() {
        label.push_str(", ");
        label.push_str(country);
    }
    Ok(GeoLocation {
        latitude: hit.latitude,
        longitude: hit.longitude,
        label,
        timezone: hit.timezone,
    })
}

/// Split `raw` into `(city, admin_hint, country_hint)` on commas, trimming
/// whitespace. Returns expanded hint values when the input is a recognized
/// abbreviation (e.g. `MI` → `Michigan`, `USA` → `United States`).
fn parse_query(raw: &str) -> (String, Option<String>, Option<String>) {
    let parts: Vec<&str> = raw
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    let city = parts.first().copied().unwrap_or("").to_string();
    let admin_hint = parts.get(1).map(|s| expand_admin(s));
    let country_hint = parts.get(2).map(|s| expand_country(s));
    (city, admin_hint, country_hint)
}

/// Score how well a geocoding candidate matches the user's hints. Higher
/// is better. Admin match weights more than country match because the
/// city's country is usually obvious from the city name itself, while the
/// admin/state actually disambiguates between same-named cities.
fn score_hit(hit: &GeocodingHit, admin_hint: Option<&str>, country_hint: Option<&str>) -> u32 {
    let mut score = 0u32;
    if let Some(want) = admin_hint {
        if let Some(got) = hit.admin1.as_deref() {
            if matches_hint(got, want) {
                score += 10;
            }
        }
    }
    if let Some(want) = country_hint {
        if let Some(got) = hit.country.as_deref() {
            if matches_hint(got, want) {
                score += 3;
            }
        }
    }
    score
}

/// `want` matches `got` if they're equal, share a prefix, or one is a
/// substring of the other (all case-insensitive). Loose by design — users
/// type `Michigan`, `michigan`, `Mich`, or `MI` (already expanded by the
/// caller) and any of those should land Troy in Michigan.
fn matches_hint(got: &str, want: &str) -> bool {
    let g = got.to_lowercase();
    let w = want.to_lowercase();
    g == w || g.starts_with(&w) || w.starts_with(&g) || g.contains(&w) || w.contains(&g)
}

/// Expand a state/province/region hint. Returns the original string when
/// no abbreviation match is found, so loose typing like "Michigan" or
/// "Bavaria" passes through verbatim.
fn expand_admin(s: &str) -> String {
    let key = s.trim().to_ascii_uppercase();
    for table in [US_STATES, CA_PROVINCES, AU_STATES, MX_STATES, GB_NATIONS] {
        if let Some(full) = table.iter().find(|(k, _)| *k == key) {
            return full.1.to_string();
        }
    }
    s.trim().to_string()
}

/// Expand a country hint (`USA`, `US`, `UK`, `GB`, `CAN`, ...). Same
/// fall-through as `expand_admin`.
fn expand_country(s: &str) -> String {
    let key = s.trim().to_ascii_uppercase();
    if let Some(full) = COUNTRIES.iter().find(|(k, _)| *k == key) {
        return full.1.to_string();
    }
    s.trim().to_string()
}

/// US state + DC abbreviation → full name.
const US_STATES: &[(&str, &str)] = &[
    ("AL", "Alabama"),
    ("AK", "Alaska"),
    ("AZ", "Arizona"),
    ("AR", "Arkansas"),
    ("CA", "California"),
    ("CO", "Colorado"),
    ("CT", "Connecticut"),
    ("DE", "Delaware"),
    ("DC", "District of Columbia"),
    ("FL", "Florida"),
    ("GA", "Georgia"),
    ("HI", "Hawaii"),
    ("ID", "Idaho"),
    ("IL", "Illinois"),
    ("IN", "Indiana"),
    ("IA", "Iowa"),
    ("KS", "Kansas"),
    ("KY", "Kentucky"),
    ("LA", "Louisiana"),
    ("ME", "Maine"),
    ("MD", "Maryland"),
    ("MA", "Massachusetts"),
    ("MI", "Michigan"),
    ("MN", "Minnesota"),
    ("MS", "Mississippi"),
    ("MO", "Missouri"),
    ("MT", "Montana"),
    ("NE", "Nebraska"),
    ("NV", "Nevada"),
    ("NH", "New Hampshire"),
    ("NJ", "New Jersey"),
    ("NM", "New Mexico"),
    ("NY", "New York"),
    ("NC", "North Carolina"),
    ("ND", "North Dakota"),
    ("OH", "Ohio"),
    ("OK", "Oklahoma"),
    ("OR", "Oregon"),
    ("PA", "Pennsylvania"),
    ("RI", "Rhode Island"),
    ("SC", "South Carolina"),
    ("SD", "South Dakota"),
    ("TN", "Tennessee"),
    ("TX", "Texas"),
    ("UT", "Utah"),
    ("VT", "Vermont"),
    ("VA", "Virginia"),
    ("WA", "Washington"),
    ("WV", "West Virginia"),
    ("WI", "Wisconsin"),
    ("WY", "Wyoming"),
];

/// Canadian province/territory abbreviation → full name.
const CA_PROVINCES: &[(&str, &str)] = &[
    ("AB", "Alberta"),
    ("BC", "British Columbia"),
    ("MB", "Manitoba"),
    ("NB", "New Brunswick"),
    ("NL", "Newfoundland and Labrador"),
    ("NS", "Nova Scotia"),
    ("NT", "Northwest Territories"),
    ("NU", "Nunavut"),
    ("ON", "Ontario"),
    ("PE", "Prince Edward Island"),
    ("QC", "Quebec"),
    ("SK", "Saskatchewan"),
    ("YT", "Yukon"),
];

/// Australian state/territory abbreviation → full name.
const AU_STATES: &[(&str, &str)] = &[
    ("NSW", "New South Wales"),
    ("VIC", "Victoria"),
    ("QLD", "Queensland"),
    ("WA", "Western Australia"),
    ("SA", "South Australia"),
    ("TAS", "Tasmania"),
    ("ACT", "Australian Capital Territory"),
    ("NT", "Northern Territory"),
];

/// Most-populous Mexican states (abbreviation per ISO 3166-2:MX). Not
/// exhaustive — covers the cases users are most likely to type.
const MX_STATES: &[(&str, &str)] = &[
    ("AGU", "Aguascalientes"),
    ("BCN", "Baja California"),
    ("BCS", "Baja California Sur"),
    ("CAM", "Campeche"),
    ("CHP", "Chiapas"),
    ("CHH", "Chihuahua"),
    ("COA", "Coahuila"),
    ("COL", "Colima"),
    ("DUR", "Durango"),
    ("GUA", "Guanajuato"),
    ("GRO", "Guerrero"),
    ("HID", "Hidalgo"),
    ("JAL", "Jalisco"),
    ("MEX", "Mexico State"),
    ("MIC", "Michoacán"),
    ("MOR", "Morelos"),
    ("NAY", "Nayarit"),
    ("NLE", "Nuevo León"),
    ("OAX", "Oaxaca"),
    ("PUE", "Puebla"),
    ("QUE", "Querétaro"),
    ("ROO", "Quintana Roo"),
    ("SLP", "San Luis Potosí"),
    ("SIN", "Sinaloa"),
    ("SON", "Sonora"),
    ("TAB", "Tabasco"),
    ("TAM", "Tamaulipas"),
    ("TLA", "Tlaxcala"),
    ("VER", "Veracruz"),
    ("YUC", "Yucatán"),
    ("ZAC", "Zacatecas"),
    ("CMX", "Mexico City"),
];

/// UK constituent countries — usually written out in full, but accept the
/// common short forms (`Eng`, `Sco`, `NI`) as a courtesy.
const GB_NATIONS: &[(&str, &str)] = &[
    ("ENG", "England"),
    ("SCO", "Scotland"),
    ("WAL", "Wales"),
    ("NI", "Northern Ireland"),
];

/// Common country abbreviation → full name. Not exhaustive — anything not
/// here passes through verbatim, which works for queries like `Spain` or
/// `Brazil` where the user just types the full name.
const COUNTRIES: &[(&str, &str)] = &[
    ("US", "United States"),
    ("USA", "United States"),
    ("U.S.", "United States"),
    ("U.S.A.", "United States"),
    ("UK", "United Kingdom"),
    ("U.K.", "United Kingdom"),
    ("GB", "United Kingdom"),
    ("CA", "Canada"),
    ("CAN", "Canada"),
    ("AU", "Australia"),
    ("AUS", "Australia"),
    ("NZ", "New Zealand"),
    ("DE", "Germany"),
    ("DEU", "Germany"),
    ("GER", "Germany"),
    ("FR", "France"),
    ("FRA", "France"),
    ("IT", "Italy"),
    ("ITA", "Italy"),
    ("ES", "Spain"),
    ("ESP", "Spain"),
    ("PT", "Portugal"),
    ("NL", "Netherlands"),
    ("BE", "Belgium"),
    ("CH", "Switzerland"),
    ("AT", "Austria"),
    ("SE", "Sweden"),
    ("NO", "Norway"),
    ("FI", "Finland"),
    ("DK", "Denmark"),
    ("IE", "Ireland"),
    ("PL", "Poland"),
    ("CZ", "Czech Republic"),
    ("GR", "Greece"),
    ("TR", "Turkey"),
    ("RU", "Russia"),
    ("UA", "Ukraine"),
    ("CN", "China"),
    ("JP", "Japan"),
    ("JPN", "Japan"),
    ("KR", "South Korea"),
    ("IN", "India"),
    ("IND", "India"),
    ("ID", "Indonesia"),
    ("TH", "Thailand"),
    ("VN", "Vietnam"),
    ("PH", "Philippines"),
    ("SG", "Singapore"),
    ("MY", "Malaysia"),
    ("HK", "Hong Kong"),
    ("TW", "Taiwan"),
    ("AE", "United Arab Emirates"),
    ("IL", "Israel"),
    ("EG", "Egypt"),
    ("ZA", "South Africa"),
    ("NG", "Nigeria"),
    ("KE", "Kenya"),
    ("MX", "Mexico"),
    ("BR", "Brazil"),
    ("BRA", "Brazil"),
    ("AR", "Argentina"),
    ("CL", "Chile"),
    ("CO", "Colombia"),
    ("PE", "Peru"),
];

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
    let client = crate::http::shared();
    let resp = client
        .get("https://ipapi.co/json/")
        .timeout(std::time::Duration::from_secs(8))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_token_query() {
        let (city, admin, country) = parse_query("Vancouver");
        assert_eq!(city, "Vancouver");
        assert!(admin.is_none());
        assert!(country.is_none());
    }

    #[test]
    fn parse_city_state_expands_us_abbrev() {
        let (city, admin, country) = parse_query("Troy, MI");
        assert_eq!(city, "Troy");
        assert_eq!(admin.as_deref(), Some("Michigan"));
        assert!(country.is_none());
    }

    #[test]
    fn parse_city_state_country_expands_all() {
        let (city, admin, country) = parse_query("Troy, MI, USA");
        assert_eq!(city, "Troy");
        assert_eq!(admin.as_deref(), Some("Michigan"));
        assert_eq!(country.as_deref(), Some("United States"));
    }

    #[test]
    fn parse_passes_through_unknown_admin_verbatim() {
        // "Bavaria" isn't in our tables — we should pass it through so the
        // matcher can still compare it case-insensitively against the
        // API's admin1 string.
        let (_, admin, _) = parse_query("Munich, Bavaria");
        assert_eq!(admin.as_deref(), Some("Bavaria"));
    }

    #[test]
    fn parse_handles_full_state_names() {
        let (city, admin, _) = parse_query("Troy, Michigan");
        assert_eq!(city, "Troy");
        assert_eq!(admin.as_deref(), Some("Michigan"));
    }

    #[test]
    fn parse_handles_canadian_provinces() {
        let (_, admin, _) = parse_query("Vancouver, BC, Canada");
        assert_eq!(admin.as_deref(), Some("British Columbia"));
    }

    #[test]
    fn parse_handles_australian_states() {
        let (_, admin, _) = parse_query("Sydney, NSW, Australia");
        assert_eq!(admin.as_deref(), Some("New South Wales"));
        let (_, admin, _) = parse_query("Melbourne, VIC");
        assert_eq!(admin.as_deref(), Some("Victoria"));
    }

    #[test]
    fn parse_handles_mexican_states() {
        let (_, admin, _) = parse_query("Cancún, ROO, Mexico");
        assert_eq!(admin.as_deref(), Some("Quintana Roo"));
    }

    #[test]
    fn parse_handles_uk_nations() {
        let (_, admin, _) = parse_query("Edinburgh, SCO, UK");
        assert_eq!(admin.as_deref(), Some("Scotland"));
    }

    #[test]
    fn parse_country_only_aliases() {
        assert_eq!(expand_country("UK"), "United Kingdom");
        assert_eq!(expand_country("U.S."), "United States");
        assert_eq!(expand_country("CAN"), "Canada");
        assert_eq!(expand_country("Brazil"), "Brazil"); // passthrough
    }

    fn hit(name: &str, admin1: Option<&str>, country: Option<&str>) -> GeocodingHit {
        GeocodingHit {
            name: name.into(),
            latitude: 0.0,
            longitude: 0.0,
            admin1: admin1.map(|s| s.into()),
            country: country.map(|s| s.into()),
            timezone: None,
        }
    }

    #[test]
    fn score_prefers_admin_match() {
        // Three Troys: Michigan, New York, Ohio. With admin hint "Michigan"
        // the Michigan Troy should win.
        let troy_mi = hit("Troy", Some("Michigan"), Some("United States"));
        let troy_ny = hit("Troy", Some("New York"), Some("United States"));
        assert!(score_hit(&troy_mi, Some("Michigan"), Some("United States")) > 0);
        assert!(
            score_hit(&troy_mi, Some("Michigan"), Some("United States"))
                > score_hit(&troy_ny, Some("Michigan"), Some("United States"))
        );
    }

    #[test]
    fn score_handles_loose_partial_match() {
        // User typed "Mich" (truncated) — should still match "Michigan".
        let troy_mi = hit("Troy", Some("Michigan"), Some("United States"));
        assert!(score_hit(&troy_mi, Some("Mich"), None) > 0);
    }

    #[test]
    fn score_zero_when_no_hints() {
        let troy = hit("Troy", Some("Michigan"), Some("United States"));
        assert_eq!(score_hit(&troy, None, None), 0);
    }
}
