// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Color scheme system.
//!
//! Loads `~/.config/glint/colorschemes.toml`, picks the active scheme named in
//! `config.toml`'s `[global] theme`, and resolves it into a [`Theme`] struct of
//! ready-to-use Ratatui [`Style`]s. Missing roles fall back to built-in
//! defaults so a scheme can override one or two things and leave the rest
//! alone. Each widget can layer its own overrides on top via a `[colors]`
//! section in its TOML.
//!
//! Roles exposed today:
//!   - `border.focused` / `border.unfocused`
//!   - `widget_title.focused` / `widget_title.unfocused` — title text on the
//!     pane's top border. Focused panes paint a background-highlighted
//!     variant; unfocused stays plain. Stacks use the same pair for the
//!     active tab while inactive tabs fall through to `text.dim`.
//!   - `metadata.focused` / `metadata.unfocused` — right-aligned suffix on
//!     the title row (Weather's location, Email's account, News's article
//!     count). Dimmed when the pane isn't focused.
//!   - `text.plain` / `text.brilliant` (default body / emphasized body)
//!   - `text.selected` (yellow-orange — selected tab, "[Today]")
//!   - `text.focused`  (cyan — focused entity within a widget)
//!
//! `StyleSpec` deserializes from either a shorthand string (`"light_cyan"`,
//! sets fg only) or a table (`{ fg = "...", bg = "...", modifiers = [...] }`).
//! Color names are case-insensitive ANSI names plus a few aliases, or hex
//! literals like `"#7dd3fc"`. `"default"`/`"reset"`/`"none"` mean "inherit
//! from the terminal".

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use ratatui::style::{Color, Modifier, Style};
use serde::{Deserialize, Deserializer};

use crate::config::{config_dir, glint_root};

/// A single style declaration, decoupled from Ratatui's [`Style`] so we can
/// distinguish "absent" (inherit) from "explicit default". Convert into a
/// concrete [`Style`] with [`StyleSpec::to_style`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StyleSpec {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub modifiers: Modifier,
}

impl StyleSpec {
    pub fn to_style(&self) -> Style {
        let mut s = Style::default();
        if let Some(fg) = self.fg {
            s = s.fg(fg);
        }
        if let Some(bg) = self.bg {
            s = s.bg(bg);
        }
        if !self.modifiers.is_empty() {
            s = s.add_modifier(self.modifiers);
        }
        s
    }
}

impl<'de> Deserialize<'de> for StyleSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Short(String),
            Long {
                #[serde(default)]
                fg: Option<String>,
                #[serde(default)]
                bg: Option<String>,
                #[serde(default)]
                modifiers: Vec<String>,
            },
        }
        let repr = Repr::deserialize(deserializer)?;
        Ok(match repr {
            Repr::Short(s) => StyleSpec {
                fg: parse_color(&s),
                bg: None,
                modifiers: Modifier::empty(),
            },
            Repr::Long { fg, bg, modifiers } => {
                let mut mods = Modifier::empty();
                for name in &modifiers {
                    if let Some(m) = parse_modifier(name) {
                        mods |= m;
                    } else {
                        tracing::warn!(modifier = %name, "unknown style modifier, ignoring");
                    }
                }
                StyleSpec {
                    fg: fg.as_deref().and_then(parse_color),
                    bg: bg.as_deref().and_then(parse_color),
                    modifiers: mods,
                }
            }
        })
    }
}

/// Partial scheme — every role is optional so users can override one thing
/// without restating the rest. Used both for full schemes in
/// `colorschemes.toml` and for per-widget overrides in widget TOMLs.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ColorScheme {
    #[serde(default)]
    pub border: BorderColors,
    #[serde(default)]
    pub widget_title: TitleColors,
    #[serde(default)]
    pub metadata: MetadataColors,
    #[serde(default)]
    pub text: TextColors,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct BorderColors {
    #[serde(default)]
    pub focused: Option<StyleSpec>,
    #[serde(default)]
    pub unfocused: Option<StyleSpec>,
}

/// Focused/unfocused pair for the title text painted on the top border.
/// `focused` is the "this pane has focus" variant — typically a
/// background-color highlight so the user can spot focus from across the
/// dashboard without the title shifting position.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TitleColors {
    #[serde(default)]
    pub focused: Option<StyleSpec>,
    #[serde(default)]
    pub unfocused: Option<StyleSpec>,
}

/// Focused/unfocused pair for the right-aligned metadata suffix on the
/// title row. Held separate from `widget_title` so colorschemes can pick a
/// quieter color for the metadata (often a dim variant of the title color).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MetadataColors {
    #[serde(default)]
    pub focused: Option<StyleSpec>,
    #[serde(default)]
    pub unfocused: Option<StyleSpec>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TextColors {
    #[serde(default)]
    pub plain: Option<StyleSpec>,
    #[serde(default)]
    pub brilliant: Option<StyleSpec>,
    #[serde(default)]
    pub dim: Option<StyleSpec>,
    #[serde(default)]
    pub selected: Option<StyleSpec>,
    #[serde(default)]
    pub focused: Option<StyleSpec>,
    /// Color of the single shortcut letter painted inside a widget title
    /// (e.g. the bold red `C` in `Clock` indicating `Shift+C` focuses it).
    #[serde(default)]
    pub shortcut: Option<StyleSpec>,
}

/// Resolved color palette — every field is a concrete [`Style`] ready to hand
/// to Ratatui. Built from a [`ColorScheme`] layered on top of
/// [`Theme::builtin_defaults`]. Widgets cache one of these merged with their
/// own [`ColorScheme`] overrides.
#[derive(Debug, Clone)]
pub struct Theme {
    pub border_focused: Style,
    pub border_unfocused: Style,
    pub widget_title_focused: Style,
    pub widget_title_unfocused: Style,
    pub metadata_focused: Style,
    pub metadata_unfocused: Style,
    pub text_plain: Style,
    pub text_brilliant: Style,
    pub text_dim: Style,
    pub text_selected: Style,
    pub text_focused: Style,
    pub text_shortcut: Style,
}

impl Default for Theme {
    fn default() -> Self {
        Self::builtin_defaults()
    }
}

impl Theme {
    /// Hardcoded fallback palette — matches the colors glint shipped with
    /// before the theme system existed. Returned when no `colorschemes.toml`
    /// is present and used to fill in any roles a scheme omits.
    pub fn builtin_defaults() -> Self {
        Self {
            border_focused: Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
            border_unfocused: Style::default(),
            // Focus on the title is conveyed by the `┤ ├` bracket pad
            // (painted in border_focused) — the title text itself stays
            // the same bold across states. Schemes can still override
            // either side to add a fg shift if they want.
            widget_title_focused: Style::default().add_modifier(Modifier::BOLD),
            widget_title_unfocused: Style::default().add_modifier(Modifier::BOLD),
            metadata_focused: Style::default(),
            metadata_unfocused: Style::default().add_modifier(Modifier::DIM),
            text_plain: Style::default(),
            text_brilliant: Style::default().add_modifier(Modifier::BOLD),
            text_dim: Style::default().add_modifier(Modifier::DIM),
            text_selected: Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
            text_focused: Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
            text_shortcut: Style::default()
                .fg(Color::LightRed)
                .add_modifier(Modifier::BOLD),
        }
    }

    /// Apply `overrides` on top of `self`. Any role the override declares
    /// wins; everything else passes through unchanged. Used both for
    /// scheme→theme (overlay on `builtin_defaults`) and widget overrides
    /// (overlay on the app theme).
    pub fn with_overrides(&self, overrides: &ColorScheme) -> Self {
        let pick = |slot: &Option<StyleSpec>, default: Style| {
            slot.as_ref().map(StyleSpec::to_style).unwrap_or(default)
        };
        Self {
            border_focused: pick(&overrides.border.focused, self.border_focused),
            border_unfocused: pick(&overrides.border.unfocused, self.border_unfocused),
            widget_title_focused: pick(&overrides.widget_title.focused, self.widget_title_focused),
            widget_title_unfocused: pick(
                &overrides.widget_title.unfocused,
                self.widget_title_unfocused,
            ),
            metadata_focused: pick(&overrides.metadata.focused, self.metadata_focused),
            metadata_unfocused: pick(&overrides.metadata.unfocused, self.metadata_unfocused),
            text_plain: pick(&overrides.text.plain, self.text_plain),
            text_brilliant: pick(&overrides.text.brilliant, self.text_brilliant),
            text_dim: pick(&overrides.text.dim, self.text_dim),
            text_selected: pick(&overrides.text.selected, self.text_selected),
            text_focused: pick(&overrides.text.focused, self.text_focused),
            text_shortcut: pick(&overrides.text.shortcut, self.text_shortcut),
        }
    }

    /// Helper used by widget borders: picks `border_focused` when the cell
    /// is currently focused, `border_unfocused` otherwise.
    pub fn border_style(&self, focused: bool) -> Style {
        if focused {
            self.border_focused
        } else {
            self.border_unfocused
        }
    }

    /// Focused/unfocused pair for the title text on the border.
    pub fn widget_title_style(&self, focused: bool) -> Style {
        if focused {
            self.widget_title_focused
        } else {
            self.widget_title_unfocused
        }
    }

    /// Focused/unfocused pair for the right-aligned metadata suffix.
    pub fn metadata_style(&self, focused: bool) -> Style {
        if focused {
            self.metadata_focused
        } else {
            self.metadata_unfocused
        }
    }
}

/// On-disk shape of `colorschemes.toml`:
///
/// ```toml
/// [schemes.default]
/// border.focused           = { fg = "light_cyan", modifiers = ["bold"] }
/// border.unfocused         = "default"
/// widget_title.focused     = { fg = "black", bg = "light_cyan", modifiers = ["bold"] }
/// widget_title.unfocused   = { modifiers = ["bold"] }
/// metadata.focused         = "default"
/// metadata.unfocused       = { modifiers = ["dim"] }
/// text.plain               = "default"
/// text.brilliant           = { fg = "white",        modifiers = ["bold"] }
/// text.selected            = { fg = "light_yellow", modifiers = ["bold"] }
/// text.focused             = { fg = "light_cyan",   modifiers = ["bold"] }
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ColorSchemesFile {
    #[serde(default)]
    pub schemes: HashMap<String, ColorScheme>,
}

/// Path to the **global** colorscheme library at the glint root. This is
/// shared across profiles; a profile may add/override schemes via its own
/// `colorschemes.toml` (see [`load_schemes_file`]).
pub fn colorschemes_path() -> Result<PathBuf> {
    Ok(glint_root()?.join("colorschemes.toml"))
}

/// Load the colorscheme library: the global root file as a base, with an
/// optional per-profile `<config_dir>/colorschemes.toml` overlaid — schemes
/// merge by name, profile definitions winning on collision. A missing file
/// at either tier contributes nothing. Callers fall back to built-in
/// defaults when the merged set lacks the requested scheme.
pub fn load_schemes_file() -> Result<ColorSchemesFile> {
    let mut merged = read_schemes_file(&colorschemes_path()?)?;
    let profile_override = config_dir()?.join("colorschemes.toml");
    if profile_override.exists() {
        for (name, scheme) in read_schemes_file(&profile_override)?.schemes {
            merged.schemes.insert(name, scheme); // profile wins
        }
    }
    Ok(merged)
}

fn read_schemes_file(path: &std::path::Path) -> Result<ColorSchemesFile> {
    if !path.exists() {
        return Ok(ColorSchemesFile::default());
    }
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    toml::from_str(&contents).with_context(|| format!("failed to parse {}", path.display()))
}

/// Load the app theme. Looks up scheme `name` in `~/.config/glint/colorschemes.toml`
/// and overlays it on the built-in defaults. Missing file → defaults only.
/// Missing scheme name → warn and use defaults. Missing roles within a
/// scheme → silently fall back to the built-in for that role.
pub fn load(name: &str) -> Result<Arc<Theme>> {
    let file = load_schemes_file()?;
    let base = Theme::builtin_defaults();
    let Some(scheme) = file.schemes.get(name) else {
        if !file.schemes.is_empty() {
            tracing::warn!(
                scheme = %name,
                available = ?file.schemes.keys().collect::<Vec<_>>(),
                "color scheme not found in colorschemes.toml, using built-in defaults"
            );
        }
        return Ok(Arc::new(base));
    };
    Ok(Arc::new(base.with_overrides(scheme)))
}

/// Build a [`Theme`] from a parsed [`ColorScheme`] layered on the built-in
/// defaults. Used by `:scheme` after the user picks a scheme by name.
pub fn theme_from_scheme(scheme: &ColorScheme) -> Arc<Theme> {
    Arc::new(Theme::builtin_defaults().with_overrides(scheme))
}

/// Persist the active scheme name to `~/.config/glint/config.toml` so the
/// choice survives a restart. Does a targeted line edit rather than
/// re-serializing the whole struct so the user's comments and formatting
/// stay intact. If `config.toml` doesn't exist, this is a silent no-op (the
/// app is running on built-in defaults; the user can save their choice by
/// running `glint --init`).
///
/// The edit looks for the first `theme = "..."` line under a `[global]`
/// section and rewrites just its value, preserving indentation. If
/// `[global]` exists but has no `theme` line, one is appended directly
/// after the section header. If `[global]` is missing entirely we
/// prepend a fresh section.
pub fn persist_active_scheme(name: &str) -> Result<()> {
    let path = crate::config::config_path()?;
    if !path.exists() {
        tracing::debug!(
            path = %path.display(),
            "config.toml missing, scheme choice won't survive restart"
        );
        return Ok(());
    }
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let updated = rewrite_theme_line(&contents, name);
    if updated == contents {
        // Nothing changed — the file already says what we want.
        return Ok(());
    }
    std::fs::write(&path, updated)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Pure helper for `persist_active_scheme` so we can unit-test the
/// line-rewriting rules without touching the filesystem. Walks lines,
/// tracks `[section]` context, and rewrites the first `theme = ".."` it
/// finds inside `[global]`. Falls back to inserting if missing.
fn rewrite_theme_line(contents: &str, name: &str) -> String {
    let lines: Vec<&str> = contents.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 2);
    let mut current_section: Option<String> = None;
    let mut in_global = false;
    let mut wrote = false;
    let mut saw_global_header = false;

    for line in &lines {
        let trimmed = line.trim_start();

        // Track section context. Only commit a section change once we've
        // emitted the header line, so the header itself is still attributed
        // to the previous section's "we just finished" point.
        if let Some(header) = trimmed.strip_prefix('[') {
            if let Some(name) = header.strip_suffix(']') {
                let name = name.trim();
                current_section = Some(name.to_string());
                in_global = name == "global";
                if in_global {
                    saw_global_header = true;
                }
                out.push((*line).to_string());
                continue;
            }
        }

        // Inside [global], try to rewrite the existing theme line.
        if in_global && !wrote && is_theme_assignment(trimmed) {
            let indent_len = line.len() - trimmed.len();
            let indent = &line[..indent_len];
            out.push(format!("{indent}theme = \"{name}\""));
            wrote = true;
            continue;
        }

        out.push((*line).to_string());
    }

    if !wrote {
        if saw_global_header {
            // Append `theme = "..."` immediately after the [global] header
            // line. Scan once to find it.
            let header_idx = out
                .iter()
                .position(|l| l.trim_start().starts_with("[global]"))
                .expect("we already saw the header above");
            out.insert(header_idx + 1, format!("theme = \"{name}\""));
        } else {
            // No [global] section anywhere — prepend a fresh stanza.
            // Use an empty line to separate from whatever came before if the
            // file isn't empty.
            let prefix = if out.is_empty() || out[0].is_empty() {
                vec![format!("[global]"), format!("theme = \"{name}\"")]
            } else {
                vec![
                    String::new(),
                    format!("[global]"),
                    format!("theme = \"{name}\""),
                ]
            };
            for (i, l) in prefix.into_iter().enumerate() {
                out.insert(i, l);
            }
        }
    }

    // Preserve trailing newline if the original had one.
    let had_trailing_newline = contents.ends_with('\n');
    let mut result = out.join("\n");
    if had_trailing_newline {
        result.push('\n');
    }
    // Silence unused-variable warning in builds where the section tracker
    // isn't otherwise consulted at the end.
    let _ = current_section;
    result
}

/// Does `line` start with a `theme = "..."` assignment (allowing
/// whitespace variations)? Used by `rewrite_theme_line` to find the line
/// it should overwrite.
fn is_theme_assignment(trimmed_line: &str) -> bool {
    let rest = match trimmed_line.strip_prefix("theme") {
        Some(r) => r.trim_start(),
        None => return false,
    };
    let rest = match rest.strip_prefix('=') {
        Some(r) => r.trim_start(),
        None => return false,
    };
    rest.starts_with('"')
}

/// Maps a color name or hex literal to a Ratatui [`Color`]. Returns `None`
/// for `"default"`/`"reset"`/`"none"`/`""` so the caller can leave the field
/// unset and inherit from the terminal.
///
/// `pub(crate)` so widgets that surface per-thing color config (calendar
/// sources, future heatmap legends, etc.) reuse one parser instead of
/// re-implementing a subset that's missing hex support.
pub(crate) fn parse_color(s: &str) -> Option<Color> {
    let norm = s.trim().to_ascii_lowercase().replace('-', "_");
    if norm.is_empty() || norm == "default" || norm == "reset" || norm == "none" {
        return None;
    }
    if let Some(c) = parse_hex_color(&norm) {
        return Some(c);
    }
    Some(match norm.as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" | "purple" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::White,
        "gray" | "grey" => Color::Gray,
        "dark_gray" | "dark_grey" => Color::DarkGray,
        "light_red" | "bright_red" => Color::LightRed,
        "light_green" | "bright_green" => Color::LightGreen,
        "light_yellow" | "bright_yellow" => Color::LightYellow,
        "light_blue" | "bright_blue" => Color::LightBlue,
        "light_magenta" | "bright_magenta" | "light_purple" => Color::LightMagenta,
        "light_cyan" | "bright_cyan" => Color::LightCyan,
        _ => {
            tracing::warn!(color = %s, "unknown color name, treating as inherit");
            return None;
        }
    })
}

fn parse_hex_color(s: &str) -> Option<Color> {
    let s = s.strip_prefix('#').unwrap_or(s);
    if s.len() != 6 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

fn parse_modifier(s: &str) -> Option<Modifier> {
    let norm = s.trim().to_ascii_lowercase();
    Some(match norm.as_str() {
        "bold" => Modifier::BOLD,
        "dim" | "faint" => Modifier::DIM,
        "italic" => Modifier::ITALIC,
        "underline" | "underlined" => Modifier::UNDERLINED,
        "slow_blink" | "blink" => Modifier::SLOW_BLINK,
        "rapid_blink" => Modifier::RAPID_BLINK,
        "reversed" | "reverse" | "invert" => Modifier::REVERSED,
        "hidden" => Modifier::HIDDEN,
        "crossed_out" | "strikethrough" | "strike" => Modifier::CROSSED_OUT,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorthand_string_sets_fg_only() {
        let spec: StyleSpec = toml::from_str(r#"x = "light_cyan""#)
            .and_then(|v: toml::Value| v["x"].clone().try_into())
            .unwrap();
        assert_eq!(spec.fg, Some(Color::LightCyan));
        assert_eq!(spec.bg, None);
        assert!(spec.modifiers.is_empty());
    }

    #[test]
    fn long_form_parses_fg_bg_modifiers() {
        let spec: StyleSpec = toml::from_str::<toml::Value>(
            r#"x = { fg = "light_yellow", bg = "black", modifiers = ["bold", "italic"] }"#,
        )
        .unwrap()["x"]
            .clone()
            .try_into()
            .unwrap();
        assert_eq!(spec.fg, Some(Color::LightYellow));
        assert_eq!(spec.bg, Some(Color::Black));
        assert!(spec.modifiers.contains(Modifier::BOLD));
        assert!(spec.modifiers.contains(Modifier::ITALIC));
    }

    #[test]
    fn hex_color_literal_parses() {
        let spec: StyleSpec = toml::from_str::<toml::Value>(r##"x = "#7dd3fc""##).unwrap()["x"]
            .clone()
            .try_into()
            .unwrap();
        assert_eq!(spec.fg, Some(Color::Rgb(0x7d, 0xd3, 0xfc)));
    }

    #[test]
    fn default_keyword_means_inherit() {
        let spec: StyleSpec = toml::from_str::<toml::Value>(r#"x = "default""#).unwrap()["x"]
            .clone()
            .try_into()
            .unwrap();
        assert_eq!(spec.fg, None);
    }

    #[test]
    fn missing_roles_fall_back_to_defaults() {
        let scheme: ColorScheme = toml::from_str(
            r#"
            [text]
            selected = "light_green"
            "#,
        )
        .unwrap();
        let theme = Theme::builtin_defaults().with_overrides(&scheme);
        // Override took effect.
        assert_eq!(theme.text_selected.fg, Some(Color::LightGreen));
        // Untouched roles match the built-in.
        assert_eq!(
            theme.border_focused.fg,
            Some(Color::LightCyan),
            "untouched roles should keep the built-in value"
        );
    }

    #[test]
    fn quoted_dotted_keys_do_not_deserialize_into_nested_struct() {
        // Regression guard: TOML treats "border.focused" (quoted) as a single
        // literal key, not a nested table. Serde then can't see the
        // BorderColors struct, the override silently vanishes, and the user
        // sees a scheme that has no visible effect. We bake the negative
        // case into the test suite so the next person who edits
        // colorschemes.toml will at least notice.
        let file: ColorSchemesFile = toml::from_str(
            r##"
            [schemes.broken]
            "border.focused" = "light_cyan"

            [schemes.fixed]
            border.focused = "light_cyan"
            "##,
        )
        .unwrap();
        assert!(
            file.schemes["broken"].border.focused.is_none(),
            "quoted dotted key should silently fail to populate — that's the bug we're guarding against"
        );
        assert_eq!(
            file.schemes["fixed"].border.focused.as_ref().unwrap().fg,
            Some(Color::LightCyan),
            "unquoted dotted key is the canonical form"
        );
    }

    #[test]
    fn loads_full_colorschemes_file() {
        let file: ColorSchemesFile = toml::from_str(
            r##"
            [schemes.default]
            border.focused = "light_cyan"
            text.selected  = { fg = "light_yellow", modifiers = ["bold"] }

            [schemes.nord]
            border.focused = "#88c0d0"
            text.focused   = "#88c0d0"
            "##,
        )
        .unwrap();
        assert!(file.schemes.contains_key("default"));
        assert!(file.schemes.contains_key("nord"));
        let nord = &file.schemes["nord"];
        assert_eq!(
            nord.border.focused.as_ref().unwrap().fg,
            Some(Color::Rgb(0x88, 0xc0, 0xd0))
        );
    }

    #[test]
    fn rewrite_theme_line_replaces_existing_value_preserving_comments() {
        let input = r#"version = 1

[global]
# active color scheme
theme = "default"
command_key = ":"

[layout]
columns = [40, 60]
"#;
        let out = rewrite_theme_line(input, "nord");
        assert!(out.contains(r#"theme = "nord""#));
        assert!(!out.contains(r#"theme = "default""#));
        assert!(out.contains("# active color scheme"));
        assert!(out.contains("command_key"));
        assert!(out.contains("[layout]"));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn rewrite_theme_line_only_touches_global_section_theme() {
        // A `theme = "..."` outside [global] (hypothetical) shouldn't be
        // touched — we only rewrite inside the global stanza.
        let input = r#"[other]
theme = "ignored"

[global]
theme = "default"
"#;
        let out = rewrite_theme_line(input, "miasma");
        assert!(out.contains(r#"theme = "ignored""#));
        assert!(out.contains(r#"theme = "miasma""#));
        assert!(!out.contains(r#"theme = "default""#));
    }

    #[test]
    fn rewrite_theme_line_appends_to_global_when_missing() {
        let input = "[global]\ncommand_key = \":\"\n";
        let out = rewrite_theme_line(input, "chalktone");
        assert!(out.contains("[global]"));
        // theme = ... should sit between the header and command_key
        let header_pos = out.find("[global]").unwrap();
        let theme_pos = out.find(r#"theme = "chalktone""#).expect("theme inserted");
        let cmdkey_pos = out.find("command_key").unwrap();
        assert!(header_pos < theme_pos && theme_pos < cmdkey_pos);
    }

    #[test]
    fn rewrite_theme_line_prepends_section_when_missing() {
        let input = "[layout]\ncolumns = [40, 60]\n";
        let out = rewrite_theme_line(input, "bluloco");
        assert!(out.starts_with("[global]") || out.contains("\n[global]"));
        assert!(out.contains(r#"theme = "bluloco""#));
        assert!(out.contains("[layout]"));
    }

    #[test]
    fn widget_override_layers_on_app_theme() {
        let app = Theme::builtin_defaults();
        let widget_override: ColorScheme = toml::from_str(
            r#"
            [text]
            focused = "light_green"
            "#,
        )
        .unwrap();
        let widget_theme = app.with_overrides(&widget_override);
        // Widget changed text.focused.
        assert_eq!(widget_theme.text_focused.fg, Some(Color::LightGreen));
        // App-level text.selected untouched.
        assert_eq!(widget_theme.text_selected.fg, Some(Color::LightYellow));
    }

    #[test]
    fn widget_title_focused_and_unfocused_split() {
        let scheme: ColorScheme = toml::from_str(
            r##"
            widget_title.focused   = { fg = "black", bg = "light_cyan", modifiers = ["bold"] }
            widget_title.unfocused = { fg = "white", modifiers = ["bold"] }
            "##,
        )
        .unwrap();
        let theme = Theme::builtin_defaults().with_overrides(&scheme);
        assert_eq!(theme.widget_title_focused.fg, Some(Color::Black));
        assert_eq!(theme.widget_title_focused.bg, Some(Color::LightCyan));
        assert_eq!(theme.widget_title_unfocused.fg, Some(Color::White));
        assert!(theme.widget_title_unfocused.bg.is_none());
    }

    #[test]
    fn metadata_focused_and_unfocused_split() {
        let scheme: ColorScheme = toml::from_str(
            r#"
            metadata.focused   = "light_yellow"
            metadata.unfocused = { fg = "gray", modifiers = ["dim"] }
            "#,
        )
        .unwrap();
        let theme = Theme::builtin_defaults().with_overrides(&scheme);
        assert_eq!(theme.metadata_focused.fg, Some(Color::LightYellow));
        assert_eq!(theme.metadata_unfocused.fg, Some(Color::Gray));
        assert!(theme
            .metadata_unfocused
            .add_modifier
            .contains(Modifier::DIM));
    }
}
