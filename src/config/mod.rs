pub mod layout;
pub mod types;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub use layout::LayoutConfig;
pub use types::Config;

/// Returns `~/.config/glint/` on Linux/macOS (or the platform equivalent).
pub fn config_dir() -> Result<PathBuf> {
    let base = dirs::config_dir().context("could not locate user config directory")?;
    Ok(base.join("glint"))
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
columns = [60, 40]
rows = [50, 50]

[[layout.cells]]
widget = "stocks"
col = 0
row = 0
col_span = 1
row_span = 2

[[layout.cells]]
widget = "calendar"
col = 1
row = 0

[[layout.cells]]
widget = "news"
col = 1
row = 1
"#;

/// Create `~/.config/glint/` and seed `config.toml` if it does not already exist.
pub fn init_default_config() -> Result<PathBuf> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create config directory at {}", dir.display()))?;

    let path = dir.join("config.toml");
    if path.exists() {
        tracing::info!(path = %path.display(), "config file already exists, leaving in place");
    } else {
        std::fs::write(&path, DEFAULT_CONFIG_TOML)
            .with_context(|| format!("failed to write default config to {}", path.display()))?;
        tracing::info!(path = %path.display(), "wrote default config");
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_parses() {
        let cfg: Config = toml::from_str(DEFAULT_CONFIG_TOML).expect("default config should parse");
        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.layout.cells.len(), 3);
        assert_eq!(cfg.global.command_key, ":");
    }

    #[test]
    fn minimal_config_uses_defaults() {
        let cfg: Config = toml::from_str("").expect("empty config should parse");
        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.layout.cells.len(), 3);
    }

    #[test]
    fn load_missing_file_returns_defaults() {
        let cfg = load(Some(Path::new("/nonexistent/glint/config.toml")))
            .expect("missing file should not error");
        assert_eq!(cfg.version, 1);
    }
}
