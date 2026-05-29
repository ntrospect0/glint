// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

pub mod layout;
pub mod types;
pub mod watcher;

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

/// Like `load_widget_toml`, but resolves to `<kind>@<instance>.toml` for
/// non-main instances. Falls back to `T::default()` when the file doesn't
/// exist.
pub fn load_widget_toml_for_instance<T>(kind: &str, instance: &str) -> Result<T>
where
    T: serde::de::DeserializeOwned + Default,
{
    let stem = crate::widgets::widget_config_stem(kind, instance);
    load_widget_toml(&stem)
}

/// Rewrite a top-level array assignment (`<key> = ["a", "b", ...]`) in a
/// widget's TOML file, preserving comments + other settings verbatim.
/// Missing keys are appended before the first `[table]` header.
/// Operates atomically via a sibling `*.tmp` rename.
///
/// Used by runtime list mutations (stocks watchlist add/remove, forex
/// crypto add/remove). The wizard's [`crate::wizard::toml_merge`] does
/// the actual text munging; this is a thin wrapper that handles I/O
/// and string-array formatting.
pub fn rewrite_widget_top_level_string_array(
    kind: &str,
    instance: &str,
    key: &str,
    items: &[String],
) -> Result<()> {
    let stem = crate::widgets::widget_config_stem(kind, instance);
    let path = config_dir()?.join(format!("{stem}.toml"));
    let original = if path.exists() {
        std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?
    } else {
        // No file yet — start from empty so the helper can append the
        // array as a fresh first line.
        String::new()
    };
    let literal = format_string_array_literal(items);
    let updated = crate::wizard::toml_merge::merge_top_level_scalars(&original, &[(key, literal)]);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to mkdir {}", parent.display()))?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, updated).with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("failed to rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Render a `Vec<String>` as a single-line TOML array literal:
/// `["AAPL", "MSFT"]`. Quotes inside an entry are escaped with a
/// backslash. Empty list renders as `[]`.
fn format_string_array_literal(items: &[String]) -> String {
    if items.is_empty() {
        return "[]".to_string();
    }
    let parts: Vec<String> = items
        .iter()
        .map(|s| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect();
    format!("[{}]", parts.join(", "))
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
pub const DEFAULT_CONFIG_TOML: &str = include_str!("defaults/config.toml");

pub const DEFAULT_CLOCK_TOML: &str = include_str!("defaults/clock.toml");

pub const DEFAULT_WEATHER_TOML: &str = include_str!("defaults/weather.toml");

pub const DEFAULT_NEWS_TOML: &str = include_str!("defaults/news.toml");

pub const DEFAULT_COLORSCHEMES_TOML: &str = include_str!("defaults/colorschemes.toml");

pub const DEFAULT_LLM_TOML: &str = include_str!("defaults/llm.toml");

pub const DEFAULT_ANTHROPIC_KEY_TEMPLATE: &str = include_str!("defaults/credentials/anthropic.toml");

pub const DEFAULT_OPENAI_KEY_TEMPLATE: &str = include_str!("defaults/credentials/openai.toml");

pub const DEFAULT_GOOGLE_CLIENT_TEMPLATE: &str = include_str!("defaults/credentials/google_client.toml");

pub const DEFAULT_MICROSOFT_CLIENT_TEMPLATE: &str = include_str!("defaults/credentials/microsoft_client.toml");

pub const DEFAULT_CALDAV_TEMPLATE: &str = include_str!("defaults/credentials/caldav.toml");

pub const DEFAULT_STOCKS_TOML: &str = include_str!("defaults/stocks.toml");

pub const DEFAULT_CALENDAR_TOML: &str = include_str!("defaults/calendar.toml");

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
    seed(&dir.join("news.toml"), DEFAULT_NEWS_TOML)?;
    seed(&dir.join("stocks.toml"), DEFAULT_STOCKS_TOML)?;
    seed(&dir.join("llm.toml"), DEFAULT_LLM_TOML)?;
    seed(&dir.join("colorschemes.toml"), DEFAULT_COLORSCHEMES_TOML)?;

    // Credentials live in their own subdirectory (created with 0700) so they
    // can be locked down with one chmod.
    let credentials = crate::credentials::dir()?;
    seed_credentials(
        &credentials.join("anthropic_key.toml"),
        DEFAULT_ANTHROPIC_KEY_TEMPLATE,
    )?;
    seed_credentials(
        &credentials.join("openai_key.toml"),
        DEFAULT_OPENAI_KEY_TEMPLATE,
    )?;
    seed_credentials(&credentials.join("caldav.toml"), DEFAULT_CALDAV_TEMPLATE)?;
    seed_credentials(
        &credentials.join("google_oauth_client.toml"),
        DEFAULT_GOOGLE_CLIENT_TEMPLATE,
    )?;
    seed_credentials(
        &credentials.join("microsoft_oauth_client.toml"),
        DEFAULT_MICROSOFT_CLIENT_TEMPLATE,
    )?;
    Ok(main)
}

fn seed_credentials(path: &Path, contents: &str) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    std::fs::write(path, contents)
        .with_context(|| format!("failed to write credentials template at {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    tracing::info!(path = %path.display(), "wrote credentials template");
    Ok(())
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
    fn default_colorschemes_seed_parses_and_has_default_scheme() {
        let file: crate::theme::ColorSchemesFile =
            toml::from_str(DEFAULT_COLORSCHEMES_TOML).expect("colorschemes seed should parse");
        assert!(
            file.schemes.contains_key("default"),
            "default scheme must exist so the unmodified config.toml resolves"
        );
        for expected in [
            "chalktone",
            "gruvbox",
            "tokyonight",
            "rosepine",
            "nord",
            "bluloco",
            "onedark",
            "miasma",
        ] {
            assert!(
                file.schemes.contains_key(expected),
                "expected scheme {expected:?} in seed"
            );
        }
    }

    #[test]
    fn seeded_schemes_populate_every_themable_role() {
        // Guards against the quoted-dotted-key bug (`"border.focused"`
        // silently parses as a single key) AND against new roles being
        // added without each seeded scheme being updated. Every scheme
        // ships values for every role exposed in colorschemes.toml.
        let file: crate::theme::ColorSchemesFile =
            toml::from_str(DEFAULT_COLORSCHEMES_TOML).expect("seed parses");
        for (name, scheme) in &file.schemes {
            assert!(
                scheme.border.focused.is_some(),
                "scheme {name:?} should set border.focused (use unquoted dotted keys)"
            );
            assert!(
                scheme.widget_title.focused.is_some(),
                "scheme {name:?} should set widget_title.focused"
            );
            assert!(
                scheme.widget_title.unfocused.is_some(),
                "scheme {name:?} should set widget_title.unfocused"
            );
            assert!(
                scheme.metadata.focused.is_some(),
                "scheme {name:?} should set metadata.focused"
            );
            assert!(
                scheme.metadata.unfocused.is_some(),
                "scheme {name:?} should set metadata.unfocused"
            );
            assert!(
                scheme.text.focused.is_some(),
                "scheme {name:?} should set text.focused (use unquoted dotted keys)"
            );
        }
    }

    #[test]
    fn default_widget_seed_files_parse() {
        // Each widget's seed is checked only when that widget is compiled
        // in — slim builds drop the type references but the TOML strings
        // themselves stay so `seed_defaults` keeps populating them at
        // install time.
        #[cfg(feature = "widget-clock")]
        {
            let _: crate::widgets::clock::ClockConfig =
                toml::from_str(DEFAULT_CLOCK_TOML).expect("clock seed should parse");
        }
        #[cfg(feature = "widget-weather")]
        {
            let _: crate::widgets::weather::WeatherConfig =
                toml::from_str(DEFAULT_WEATHER_TOML).expect("weather seed should parse");
        }
        #[cfg(feature = "widget-calendar")]
        {
            let cal: crate::widgets::calendar::CalendarConfig =
                toml::from_str(DEFAULT_CALENDAR_TOML).expect("calendar seed should parse");
            assert!(
                !cal.events.is_empty(),
                "calendar seed should ship example events"
            );
        }
        #[cfg(feature = "widget-news")]
        {
            let news: crate::widgets::news::NewsConfig =
                toml::from_str(DEFAULT_NEWS_TOML).expect("news seed should parse");
            assert!(
                !news.feeds.is_empty(),
                "news seed should ship example feeds"
            );
        }
        let llm: crate::llm::LlmConfig =
            toml::from_str(DEFAULT_LLM_TOML).expect("llm seed should parse");
        assert!(llm.enabled);
        assert_eq!(llm.provider.name, "anthropic");
        #[cfg(feature = "widget-stocks")]
        {
            let stocks: crate::widgets::stocks::StocksConfig =
                toml::from_str(DEFAULT_STOCKS_TOML).expect("stocks seed should parse");
            assert!(!stocks.indices.is_empty());
            assert!(!stocks.watchlist.is_empty());
        }
    }

    #[test]
    fn load_missing_file_returns_defaults() {
        let cfg = load(Some(Path::new("/nonexistent/glint/config.toml")))
            .expect("missing file should not error");
        assert_eq!(cfg.version, 1);
    }

    #[test]
    fn format_string_array_literal_quotes_and_escapes() {
        assert_eq!(format_string_array_literal(&[]), "[]");
        assert_eq!(
            format_string_array_literal(&["AAPL".into(), "MSFT".into()]),
            r#"["AAPL", "MSFT"]"#
        );
        assert_eq!(
            format_string_array_literal(&[r#"weird"name"#.into()]),
            r#"["weird\"name"]"#
        );
    }

    #[test]
    fn array_literal_round_trips_through_merge_helper() {
        // Sanity: an existing stocks-shaped TOML retains comments and
        // sibling keys when the watchlist array is rewritten.
        let original = r#"indices = ["^DJI", "^GSPC"]
watchlist = ["AAPL", "MSFT"]

# Press `j` on a selected ticker to open this URL.
jump_url_template = "https://example.com/{ticker}"
"#;
        let updated = crate::wizard::toml_merge::merge_top_level_scalars(
            original,
            &[(
                "watchlist",
                format_string_array_literal(&["NVDA".into(), "TSLA".into()]),
            )],
        );
        assert!(updated.contains(r#"watchlist = ["NVDA", "TSLA"]"#));
        assert!(updated.contains(r#"indices = ["^DJI", "^GSPC"]"#));
        assert!(updated.contains("# Press `j`"));
        assert!(updated.contains("jump_url_template"));
    }
}
