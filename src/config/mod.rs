// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

pub mod layout;
pub mod migrate;
pub mod profiles;
pub mod types;
pub mod watcher;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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

/// The default profile name. Always exists; cannot be deleted.
pub const DEFAULT_PROFILE: &str = "default";

static ACTIVE_PROFILE: OnceLock<String> = OnceLock::new();
static CONFIG_DIR_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// The active profile name. **Read-only**: this never initializes the lock
/// (no `get_or_init`), so an early read can't silently pin `"default"` and
/// turn a later [`set_active_profile`] into a no-op. Falls back to
/// [`DEFAULT_PROFILE`] until `main` sets it.
pub fn active_profile() -> &'static str {
    ACTIVE_PROFILE
        .get()
        .map(String::as_str)
        .unwrap_or(DEFAULT_PROFILE)
}

/// Set the active profile. Called **exactly once** in `main`, before any
/// config access. Panics on a second call so an accidental re-set is loud
/// rather than a silent wrong-tree.
pub fn set_active_profile(name: impl Into<String>) {
    ACTIVE_PROFILE
        .set(name.into())
        .expect("active profile set more than once");
}

/// Point the per-profile config dir at an explicit directory, bypassing
/// profile resolution. Used by `--config <FILE>` (explicit single-file mode)
/// to resolve sibling files from the file's own directory. Set once.
pub fn set_config_dir_override(dir: PathBuf) {
    let _ = CONFIG_DIR_OVERRIDE.set(dir);
}

/// Validate a profile name: ASCII-alphanumeric start, then
/// alphanumeric/`-`/`_`, 1–64 chars, no path separators.
pub fn validate_profile_name(name: &str) -> Result<()> {
    let ok = (1..=64).contains(&name.len())
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok {
        anyhow::bail!(
            "invalid profile name {name:?}: use letters, digits, '-' or '_' \
             (1–64 chars, no leading dash, no path separators)"
        );
    }
    Ok(())
}

/// The glint root — `~/.config/glint/` (overridable with `$XDG_CONFIG_HOME`).
/// This is the **global layer** shared across profiles. The XDG Base
/// Directory layout is what the spec promises, so we use it consistently
/// rather than `~/Library/Application Support/` (macOS) or `%APPDATA%`.
pub fn glint_root() -> Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("glint"));
        }
    }
    let home = dirs::home_dir().context("could not locate user home directory")?;
    Ok(home.join(".config").join("glint"))
}

/// The active profile's config directory — `<glint_root>/profiles/<active>`.
/// Every per-profile path (widget configs, credentials, runtime/wizard
/// state, notes, log) resolves under this. An explicit `--config` override
/// short-circuits to that file's directory.
pub fn config_dir() -> Result<PathBuf> {
    if let Some(dir) = CONFIG_DIR_OVERRIDE.get() {
        return Ok(dir.clone());
    }
    Ok(glint_root()?.join("profiles").join(active_profile()))
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
    seed_global_layer()?;
    let dir = config_dir()?;
    seed_profile_dir(&dir)?;
    Ok(dir.join("config.toml"))
}

/// Seed the shared **global layer** at the glint root: the colorscheme
/// library and the OAuth client-registration templates. Idempotent.
pub(crate) fn seed_global_layer() -> Result<()> {
    let root = glint_root()?;
    std::fs::create_dir_all(&root)
        .with_context(|| format!("failed to create glint root at {}", root.display()))?;
    seed(&root.join("colorschemes.toml"), DEFAULT_COLORSCHEMES_TOML)?;
    let global_creds = crate::credentials::global_dir()?;
    seed_credentials(
        &global_creds.join("google_oauth_client.toml"),
        DEFAULT_GOOGLE_CLIENT_TEMPLATE,
    )?;
    seed_credentials(
        &global_creds.join("microsoft_oauth_client.toml"),
        DEFAULT_MICROSOFT_CLIENT_TEMPLATE,
    )?;
    Ok(())
}

/// Seed a **profile directory** with the default per-profile config + account
/// credential templates. Parameterized on `dir` so it can seed any profile
/// (the active one, or a freshly-created one). Idempotent.
pub(crate) fn seed_profile_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create profile dir {}", dir.display()))?;
    seed(&dir.join("config.toml"), DEFAULT_CONFIG_TOML)?;
    seed(&dir.join("clock.toml"), DEFAULT_CLOCK_TOML)?;
    seed(&dir.join("weather.toml"), DEFAULT_WEATHER_TOML)?;
    seed(&dir.join("calendar.toml"), DEFAULT_CALENDAR_TOML)?;
    seed(&dir.join("news.toml"), DEFAULT_NEWS_TOML)?;
    seed(&dir.join("stocks.toml"), DEFAULT_STOCKS_TOML)?;
    seed(&dir.join("llm.toml"), DEFAULT_LLM_TOML)?;

    let creds = dir.join("credentials");
    std::fs::create_dir_all(&creds)
        .with_context(|| format!("failed to create {}", creds.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&creds, std::fs::Permissions::from_mode(0o700));
    }
    seed_credentials(
        &creds.join("anthropic_key.toml"),
        DEFAULT_ANTHROPIC_KEY_TEMPLATE,
    )?;
    seed_credentials(&creds.join("openai_key.toml"), DEFAULT_OPENAI_KEY_TEMPLATE)?;
    seed_credentials(&creds.join("caldav.toml"), DEFAULT_CALDAV_TEMPLATE)?;
    Ok(())
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

    #[test]
    fn profile_name_validation() {
        for ok in ["default", "work", "travel-eu", "p_2", "A1"] {
            assert!(validate_profile_name(ok).is_ok(), "{ok:?} should be valid");
        }
        for bad in ["", "-lead", "_lead", "has space", "a/b", "a.b", "café"] {
            assert!(
                validate_profile_name(bad).is_err(),
                "{bad:?} should be invalid"
            );
        }
        // 64 ok, 65 too long.
        assert!(validate_profile_name(&"a".repeat(64)).is_ok());
        assert!(validate_profile_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn active_profile_defaults_without_set() {
        // In the test process the OnceLock is never set → reads the default.
        // (Read-only accessor must not pin the lock.)
        assert_eq!(active_profile(), DEFAULT_PROFILE);
    }
}
