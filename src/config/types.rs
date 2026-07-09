// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

use serde::{
    de::{self, Visitor},
    Deserialize, Deserializer,
};
use std::fmt;

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

    /// Polling cadence multiplier for widgets hidden inside a stack.
    /// `1` = full rate; `20` (default) = hidden children's `update()`
    /// is called every 20th tick (~5s at the 250ms tick rate); higher
    /// = even less frequent. Saves CPU + API calls for stacks the user
    /// doesn't actively switch through. Visible / non-stacked widgets
    /// are unaffected.
    #[serde(default = "default_stack_hidden_poll_ratio")]
    pub stack_hidden_poll_ratio: u32,

    /// Bottom-of-screen status bar (`glint vX.Y.Z │ clock │ Focus │
    /// Scheme │ hints`). `true` (default) shows the row; `false` hides
    /// it and gives the row back to the widget grid. Discoverability
    /// of `?`/`q`/Tab still flows through the help overlay either way.
    #[serde(default = "default_show_status_bar")]
    pub show_status_bar: bool,

    /// Margin carved off each side of the screen when the zoom overlay is
    /// active. Follows CSS shorthand notation — the value is percent of the
    /// screen per side:
    ///
    /// ```toml
    /// zoom_margin = 5          # uniform: 5% on every side (default)
    /// zoom_margin = [5, 10]    # top+bottom = 5%, left+right = 10%
    /// zoom_margin = [5, 10, 8] # top=5%, left+right=10%, bottom=8%
    /// zoom_margin = [5, 10, 8, 3] # top=5%, right=10%, bottom=8%, left=3%
    /// ```
    ///
    /// Each side must be in `0..=45`; `top+bottom` and `left+right` must
    /// each be `<=90` so the center always has usable space. Any invalid
    /// value (wrong type, out-of-range, too many entries) silently falls
    /// back to the 5% default without erroring the whole config load.
    #[serde(default)]
    pub zoom_margin: ZoomMargin,
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
            zoom_margin: ZoomMargin::default(),
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

/// Per-side margin used to size the zoom overlay, in percent of screen width
/// or height per side. Validated on deserialize; any invalid input falls back
/// to the 5% default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZoomMargin {
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
    pub left: u16,
}

impl Default for ZoomMargin {
    fn default() -> Self {
        Self { top: 5, right: 5, bottom: 5, left: 5 }
    }
}

impl ZoomMargin {
    fn from_sides(top: i64, right: i64, bottom: i64, left: i64) -> Self {
        let valid = [top, right, bottom, left]
            .iter()
            .all(|&v| v >= 0 && v <= 45);
        if !valid || top + bottom > 90 || left + right > 90 {
            let raw = [top, right, bottom, left];
            tracing::warn!(
                "invalid zoom_margin {:?}: each side must be 0–45 and \
                 top+bottom/left+right must be ≤90; using default 5",
                raw
            );
            return Self::default();
        }
        Self {
            top: top as u16,
            right: right as u16,
            bottom: bottom as u16,
            left: left as u16,
        }
    }

    fn from_css(values: &[i64]) -> Self {
        match values {
            [] => {
                tracing::warn!(
                    "invalid zoom_margin []: empty list; using default 5"
                );
                Self::default()
            }
            [all] => Self::from_sides(*all, *all, *all, *all),
            [v, h] => Self::from_sides(*v, *h, *v, *h),
            [t, h, b] => Self::from_sides(*t, *h, *b, *h),
            [t, r, b, l] => Self::from_sides(*t, *r, *b, *l),
            _ => {
                tracing::warn!(
                    "invalid zoom_margin {:?}: at most 4 values allowed; using default 5",
                    values
                );
                Self::default()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ZoomMargin {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Use a custom visitor so that a completely wrong type (string, bool,
        // map, …) is caught and returns the default rather than a hard error.
        struct ZoomMarginVisitor;

        impl<'de> Visitor<'de> for ZoomMarginVisitor {
            type Value = ZoomMargin;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "an integer or an array of 1–4 integers")
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<ZoomMargin, E> {
                Ok(ZoomMargin::from_css(&[v]))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<ZoomMargin, E> {
                Ok(ZoomMargin::from_css(&[v as i64]))
            }

            fn visit_seq<A: de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<ZoomMargin, A::Error> {
                let mut values: Vec<i64> = Vec::new();
                let mut valid = true;
                loop {
                    match seq.next_element::<i64>() {
                        Ok(Some(v)) => values.push(v),
                        Ok(None) => break,
                        Err(_) => {
                            valid = false;
                            break;
                        }
                    }
                }
                if !valid {
                    tracing::warn!(
                        "invalid zoom_margin: array contains a non-integer element; \
                         using default 5"
                    );
                    return Ok(ZoomMargin::default());
                }
                Ok(ZoomMargin::from_css(&values))
            }

            // Catch-all: any unexpected type falls back to the default.
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<ZoomMargin, E> {
                tracing::warn!("invalid zoom_margin: expected integer or array; using default 5");
                Ok(ZoomMargin::default())
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<ZoomMargin, E> {
                tracing::warn!("invalid zoom_margin: expected integer or array; using default 5");
                Ok(ZoomMargin::default())
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<ZoomMargin, E> {
                tracing::warn!(
                    "invalid zoom_margin {:?}: expected integer or array; using default 5",
                    v
                );
                Ok(ZoomMargin::default())
            }
            fn visit_map<A: de::MapAccess<'de>>(
                self,
                _: A,
            ) -> Result<ZoomMargin, A::Error> {
                tracing::warn!("invalid zoom_margin: expected integer or array; using default 5");
                Ok(ZoomMargin::default())
            }
        }

        deserializer.deserialize_any(ZoomMarginVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::ZoomMargin;

    fn deser(toml_value: &str) -> ZoomMargin {
        // Wrap in a minimal table so toml::from_str can parse it.
        let src = format!("[global]\nzoom_margin = {toml_value}");
        let config: crate::config::types::Config =
            toml::from_str(&src).expect("toml parse");
        config.global.zoom_margin
    }

    #[test]
    fn scalar_uniform() {
        assert_eq!(deser("10"), ZoomMargin { top: 10, right: 10, bottom: 10, left: 10 });
    }

    #[test]
    fn array_2_v_h() {
        assert_eq!(
            deser("[8, 12]"),
            ZoomMargin { top: 8, right: 12, bottom: 8, left: 12 }
        );
    }

    #[test]
    fn array_3_t_h_b() {
        assert_eq!(
            deser("[4, 6, 9]"),
            ZoomMargin { top: 4, right: 6, bottom: 9, left: 6 }
        );
    }

    #[test]
    fn array_4_t_r_b_l() {
        assert_eq!(
            deser("[3, 7, 5, 2]"),
            ZoomMargin { top: 3, right: 7, bottom: 5, left: 2 }
        );
    }

    #[test]
    fn default_scalar_5() {
        assert_eq!(deser("5"), ZoomMargin::default());
    }

    #[test]
    fn invalid_empty_array_falls_back() {
        assert_eq!(deser("[]"), ZoomMargin::default());
    }

    #[test]
    fn invalid_five_values_falls_back() {
        assert_eq!(deser("[1, 2, 3, 4, 5]"), ZoomMargin::default());
    }

    #[test]
    fn invalid_out_of_range_side_falls_back() {
        // 46 exceeds the 0..=45 cap.
        assert_eq!(deser("[46, 5, 5, 5]"), ZoomMargin::default());
    }

    #[test]
    fn invalid_top_plus_bottom_too_large_falls_back() {
        // top=50 + bottom=50 = 100 > 90.
        assert_eq!(deser("[50, 5, 50, 5]"), ZoomMargin::default());
    }

    #[test]
    fn invalid_wrong_type_string_falls_back() {
        assert_eq!(deser("\"hello\""), ZoomMargin::default());
    }

    #[test]
    fn missing_key_gives_default() {
        let src = "[global]\n";
        let config: crate::config::types::Config = toml::from_str(src).expect("toml parse");
        assert_eq!(config.global.zoom_margin, ZoomMargin::default());
    }
}
