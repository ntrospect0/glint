use serde::Deserialize;

use super::layout::LayoutConfig;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[allow(dead_code)] // surfaced once we start migrating configs across versions.
    #[serde(default = "default_version")]
    pub version: u32,

    #[allow(dead_code)] // wired into status bar + command bar in later phases.
    #[serde(default)]
    pub global: GlobalConfig,

    #[serde(default)]
    pub layout: LayoutConfig,
}

fn default_version() -> u32 {
    1
}

// Phase 1 parses these for forward-compat; they're read in later phases
// (status bar, command bar, log routing, theme overrides).
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct GlobalConfig {
    #[serde(default = "default_theme")]
    pub theme: String,

    #[serde(default = "default_command_key")]
    pub command_key: String,

    #[serde(default = "default_refresh_on_focus")]
    pub refresh_all_on_focus: bool,

    #[serde(default = "default_log_level")]
    pub log_level: String,

    #[serde(default)]
    pub log_file: Option<String>,
}

fn default_theme() -> String {
    "default".into()
}

fn default_command_key() -> String {
    ":".into()
}

fn default_refresh_on_focus() -> bool {
    true
}

fn default_log_level() -> String {
    "info".into()
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            theme: default_theme(),
            command_key: default_command_key(),
            refresh_all_on_focus: default_refresh_on_focus(),
            log_level: default_log_level(),
            log_file: None,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: default_version(),
            global: GlobalConfig::default(),
            layout: LayoutConfig::default(),
        }
    }
}
