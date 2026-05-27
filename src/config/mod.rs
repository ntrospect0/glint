pub mod layout;
pub mod types;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub use layout::LayoutConfig;
pub use types::Config;

/// Load a per-widget TOML config from `~/.config/glint/<name>.toml`. Returns
/// `T::default()` if the file does not exist.
pub fn load_widget_toml<T>(name: &str) -> Result<T>
where
    T: serde::de::DeserializeOwned + Default,
{
    let path = config_dir()?.join(format!("{name}.toml"));
    if !path.exists() {
        return Ok(T::default());
    }
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read widget config at {}", path.display()))?;
    let value: T = toml::from_str(&contents)
        .with_context(|| format!("failed to parse widget config at {}", path.display()))?;
    Ok(value)
}

/// Returns `~/.config/glint/` on every platform (overridable with
/// `$XDG_CONFIG_HOME`). The XDG Base Directory layout is what the spec
/// promises, so we use it consistently rather than falling back to
/// `~/Library/Application Support/` on macOS or `%APPDATA%` on Windows.
pub fn config_dir() -> Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("glint"));
        }
    }
    let home = dirs::home_dir().context("could not locate user home directory")?;
    Ok(home.join(".config").join("glint"))
}

/// Returns the path to the main config file (`config.toml`).
pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

/// Load the main config from disk. If the path does not exist, returns the
/// built-in defaults. CLI-supplied `override_path` takes precedence over the
/// XDG default location.
pub fn load(override_path: Option<&Path>) -> Result<Config> {
    let path: PathBuf = match override_path {
        Some(p) => p.to_path_buf(),
        None => config_path()?,
    };

    if !path.exists() {
        tracing::info!(path = %path.display(), "config file not found, using built-in defaults");
        return Ok(Config::default());
    }

    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read config file at {}", path.display()))?;
    let cfg: Config = toml::from_str(&contents)
        .with_context(|| format!("failed to parse config file at {}", path.display()))?;
    Ok(cfg)
}

/// Default `config.toml` contents written by `--init`.
pub const DEFAULT_CONFIG_TOML: &str = r#"version = 1

[global]
theme = "default"
command_key = ":"
refresh_all_on_focus = true
log_level = "info"

[layout]
columns = [40, 60]
rows = [35, 35, 30]

[[layout.cells]]
widget = "clock"
col = 0
row = 0

[[layout.cells]]
widget = "calendar"
col = 1
row = 0

[[layout.cells]]
widget = "weather"
col = 0
row = 1

[[layout.cells]]
widget = "news"
col = 1
row = 1

[[layout.cells]]
widget = "stocks"
col = 0
row = 2
col_span = 2
"#;

pub const DEFAULT_CLOCK_TOML: &str = r#"# Optional IANA timezone name for the primary clock; defaults to system local time.
# timezone = "America/Vancouver"
show_seconds = false              # show :SS in the big block-digit display
show_seconds_ticker = true        # show a small ticking HH:MM:SS below the big digits
show_date = true
hour_format = 24                  # 12 or 24

# Additional world clocks rendered when there's vertical room.
[[secondary_timezones]]
label = "New York"
tz = "America/New_York"

[[secondary_timezones]]
label = "London"
tz = "Europe/London"

[[secondary_timezones]]
label = "Tokyo"
tz = "Asia/Tokyo"
"#;

pub const DEFAULT_WEATHER_TOML: &str = r#"# Open-Meteo is free and key-less. Set lat/lon to your city.
# Comment out latitude + longitude (and leave auto_locate = true) to fall back
# to IP-based geolocation via ipapi.co.
label = "Richmond, BC"
latitude = 49.166
longitude = -123.133
units = "metric"                  # "metric" (°C, km/h) or "imperial" (°F, mph)
poll_interval_secs = 600
auto_locate = true                # only consulted when lat/lon are unset
"#;

pub const DEFAULT_CALENDAR_TOML: &str = r#"# Default view: "day", "week", or "month".
default_view = "day"
poll_interval_secs = 60

# Example events. Replace these with your own — timed events use RFC3339
# timestamps with a timezone offset; all-day events use bare YYYY-MM-DD.
# Google Calendar wiring lands in a later release.

[[events]]
title = "Team standup"
start = "2026-05-20T09:30:00-07:00"
end = "2026-05-20T10:00:00-07:00"
calendar = "work"
location = "Zoom"

[[events]]
title = "Coffee with Sara"
start = "2026-05-20T15:00:00-07:00"
end = "2026-05-20T16:00:00-07:00"
calendar = "personal"

[[events]]
title = "Project review"
start = "2026-05-21T13:00:00-07:00"
end = "2026-05-21T14:30:00-07:00"
calendar = "work"

[[events]]
title = "Conference"
start = "2026-05-23"
end = "2026-05-24"
all_day = true
calendar = "personal"
"#;

/// Create `~/.config/glint/` and seed the default config files if they do not
/// already exist. Returns the path of the main `config.toml`.
pub fn init_default_config() -> Result<PathBuf> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create config directory at {}", dir.display()))?;

    let main = dir.join("config.toml");
    seed(&main, DEFAULT_CONFIG_TOML)?;
    seed(&dir.join("clock.toml"), DEFAULT_CLOCK_TOML)?;
    seed(&dir.join("weather.toml"), DEFAULT_WEATHER_TOML)?;
    seed(&dir.join("calendar.toml"), DEFAULT_CALENDAR_TOML)?;
    Ok(main)
}

fn seed(path: &Path, contents: &str) -> Result<()> {
    if path.exists() {
        tracing::info!(path = %path.display(), "config file already exists, leaving in place");
        return Ok(());
    }
    std::fs::write(path, contents)
        .with_context(|| format!("failed to write default config to {}", path.display()))?;
    tracing::info!(path = %path.display(), "wrote default config");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_parses() {
        let cfg: Config = toml::from_str(DEFAULT_CONFIG_TOML).expect("default config should parse");
        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.layout.cells.len(), 5);
        assert_eq!(cfg.global.command_key, ":");
    }

    #[test]
    fn minimal_config_uses_defaults() {
        let cfg: Config = toml::from_str("").expect("empty config should parse");
        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.layout.cells.len(), 5);
    }

    #[test]
    fn default_widget_seed_files_parse() {
        let _: crate::widgets::clock::ClockConfig =
            toml::from_str(DEFAULT_CLOCK_TOML).expect("clock seed should parse");
        let _: crate::widgets::weather::WeatherConfig =
            toml::from_str(DEFAULT_WEATHER_TOML).expect("weather seed should parse");
        let cal: crate::widgets::calendar::CalendarConfig =
            toml::from_str(DEFAULT_CALENDAR_TOML).expect("calendar seed should parse");
        assert!(!cal.events.is_empty(), "calendar seed should ship example events");
    }

    #[test]
    fn load_missing_file_returns_defaults() {
        let cfg = load(Some(Path::new("/nonexistent/glint/config.toml")))
            .expect("missing file should not error");
        assert_eq!(cfg.version, 1);
    }
}
