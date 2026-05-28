// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

use serde::Deserialize;

use super::layout::LayoutConfig;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[allow(dead_code)] // read when migrating configs across versions.
    #[serde(default = "default_version")]
    pub version: u32,

    #[serde(default)]
    pub global: GlobalConfig,

    #[serde(default)]
    pub layout: LayoutConfig,
}

fn default_version() -> u32 {
    1
}

#[allow(dead_code)] // surfaced by status bar + command bar render paths.
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

    /// Vertical mouse-wheel direction. `"natural"` (default) means a wheel-up
    /// scroll moves a list selection or pane content *up*. `"inverted"` flips
    /// every scroll event at the dispatch boundary so widgets — which always
    /// write up=up / down=down — automatically honor the user's preference.
    #[serde(default)]
    pub mouse_scroll: MouseScroll,

    /// Polling cadence multiplier for widgets hidden inside a stack
    /// (see docs/stack-spec.md §2). `1` = full rate; `20` (default) =
    /// hidden children's `update()` is called every 20th tick (~5s
    /// at the 250ms tick rate); higher = even less frequent. Saves
    /// CPU + API calls for stacks the user doesn't actively switch
    /// through. Visible / non-stacked widgets are unaffected.
    #[serde(default = "default_stack_hidden_poll_ratio")]
    pub stack_hidden_poll_ratio: u32,

    /// Bottom-of-screen status bar (`glint vX.Y.Z │ clock │ Focus │
    /// Scheme │ hints`). `true` (default) shows the row; `false` hides
    /// it and gives the row back to the widget grid. Discoverability
    /// of `?`/`q`/Tab still flows through the help overlay either way.
    #[serde(default = "default_show_status_bar")]
    pub show_status_bar: bool,
}

fn default_stack_hidden_poll_ratio() -> u32 {
    20
}

fn default_show_status_bar() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseScroll {
    #[default]
    Natural,
    Inverted,
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
            mouse_scroll: MouseScroll::default(),
            stack_hidden_poll_ratio: default_stack_hidden_poll_ratio(),
            show_status_bar: default_show_status_bar(),
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
