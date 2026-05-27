// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Shared style palette for the wizard. Centralising the colour + modifier
//! choices here keeps the page renderers visually consistent and saves
//! every page from inventing its own ad-hoc `Style::default()` rules.
//!
//! Colors are derived from the user's active [`crate::theme::Theme`]: the
//! wizard starts on Nord by default and refreshes the palette when the
//! user picks a different scheme on the Global page. Schemes that don't
//! exist on disk degrade to built-in defaults via [`crate::theme::load`].

#![allow(dead_code)] // border_* helpers are surface for the next round of theming.

use std::sync::{OnceLock, RwLock};

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
};

use crate::theme::Theme;

/// Default scheme the wizard boots on when the user hasn't picked one yet.
pub const DEFAULT_SCHEME: &str = "nord";

/// Resolved style table painted from the active theme. Held behind a
/// process-global `RwLock` so the Global page can swap palettes
/// mid-flow without rethreading the theme through every page renderer.
#[derive(Debug, Clone)]
pub struct WizardPalette {
    pub border_focused: Style,
    pub border_unfocused: Style,
    pub section_header: Style,
    pub blurb: Style,
    pub label: Style,
    pub label_focused: Style,
    pub help_text: Style,
    pub value_idle: Style,
    pub value_focused: Style,
    pub option_selected: Style,
    pub option_idle: Style,
    pub marker_active: Style,
    pub marker_idle: Style,
    pub required: Style,
    pub error: Style,
    pub progress_filled: Style,
    pub progress_empty: Style,
    pub key_hint: Style,
    pub key_hint_desc: Style,
    pub page_button_focused: Style,
    pub page_button_idle: Style,
    pub cursor: Style,
}

impl WizardPalette {
    /// Build a palette from a resolved [`Theme`]. The wizard's UI roles
    /// don't map 1:1 onto the dashboard's roles, so we pull from the
    /// theme's closest equivalents and add appropriate modifiers (bold,
    /// underline) for wizard-only emphasis.
    pub fn from_theme(theme: &Theme) -> Self {
        let accent_focused = theme.text_focused; // bold accent color
        let accent_selected = theme.text_selected; // selection / important
        let plain = theme.text_plain;
        let brilliant = theme.text_brilliant;
        let dim = theme.text_dim;
        let shortcut = theme.text_shortcut;

        let accent_focused_fg = accent_focused.fg.unwrap_or(Color::Cyan);
        let accent_selected_fg = accent_selected.fg.unwrap_or(Color::Yellow);
        let dim_fg = dim.fg.unwrap_or(Color::DarkGray);
        let plain_fg = plain.fg.unwrap_or(Color::White);
        let brilliant_fg = brilliant.fg.unwrap_or(Color::White);

        Self {
            border_focused: theme.border_focused,
            border_unfocused: theme.border_unfocused,
            section_header: Style::default()
                .fg(accent_focused_fg)
                .add_modifier(Modifier::BOLD),
            blurb: Style::default().fg(dim_fg).add_modifier(Modifier::ITALIC),
            label: Style::default()
                .fg(accent_focused_fg)
                .add_modifier(Modifier::BOLD),
            label_focused: Style::default()
                .fg(accent_selected_fg)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            help_text: Style::default().fg(dim_fg).add_modifier(Modifier::ITALIC),
            value_idle: Style::default().fg(plain_fg),
            value_focused: Style::default()
                .fg(accent_selected_fg)
                .add_modifier(Modifier::BOLD),
            option_selected: Style::default()
                .bg(accent_selected_fg)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
            option_idle: Style::default().fg(plain_fg),
            marker_active: Style::default()
                .fg(accent_focused_fg)
                .add_modifier(Modifier::BOLD),
            marker_idle: Style::default().fg(dim_fg),
            required: Style::default()
                .fg(accent_selected_fg)
                .add_modifier(Modifier::BOLD),
            error: Style::default()
                .fg(shortcut.fg.unwrap_or(Color::Red))
                .add_modifier(Modifier::BOLD),
            progress_filled: Style::default()
                .fg(accent_focused_fg)
                .add_modifier(Modifier::BOLD),
            progress_empty: Style::default().fg(dim_fg),
            key_hint: Style::default()
                .fg(accent_focused_fg)
                .add_modifier(Modifier::BOLD),
            key_hint_desc: Style::default().fg(dim_fg),
            page_button_focused: Style::default()
                .bg(accent_focused_fg)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
            page_button_idle: Style::default()
                .fg(accent_focused_fg)
                .add_modifier(Modifier::BOLD),
            cursor: Style::default()
                .fg(brilliant_fg)
                .add_modifier(Modifier::SLOW_BLINK | Modifier::BOLD),
        }
    }
}

static PALETTE: OnceLock<RwLock<WizardPalette>> = OnceLock::new();

fn palette_lock() -> &'static RwLock<WizardPalette> {
    PALETTE.get_or_init(|| {
        let theme = crate::theme::load(DEFAULT_SCHEME)
            .unwrap_or_else(|_| std::sync::Arc::new(Theme::builtin_defaults()));
        RwLock::new(WizardPalette::from_theme(&theme))
    })
}

/// Reload the wizard's active palette from a named color scheme. Called
/// by the Global page when the user picks a new theme so the rest of the
/// wizard refreshes immediately. Unknown / missing schemes fall back to
/// built-in defaults (already handled by [`crate::theme::load`]).
pub fn set_active_scheme(name: &str) {
    let theme = match crate::theme::load(name) {
        Ok(t) => t,
        Err(err) => {
            tracing::warn!(scheme = %name, error = %err, "wizard could not load scheme, keeping current palette");
            return;
        }
    };
    if let Ok(mut guard) = palette_lock().write() {
        *guard = WizardPalette::from_theme(&theme);
    }
}

fn read<R>(f: impl FnOnce(&WizardPalette) -> R) -> R {
    let guard = palette_lock().read().expect("wizard palette poisoned");
    f(&guard)
}

pub fn border_focused() -> Style {
    read(|p| p.border_focused)
}
pub fn border_unfocused() -> Style {
    read(|p| p.border_unfocused)
}
pub fn section_header() -> Style {
    read(|p| p.section_header)
}
pub fn blurb() -> Style {
    read(|p| p.blurb)
}
pub fn label() -> Style {
    read(|p| p.label)
}
pub fn label_focused() -> Style {
    read(|p| p.label_focused)
}
pub fn help_text() -> Style {
    read(|p| p.help_text)
}
pub fn value_idle() -> Style {
    read(|p| p.value_idle)
}
pub fn value_focused() -> Style {
    read(|p| p.value_focused)
}
pub fn option_selected() -> Style {
    read(|p| p.option_selected)
}
pub fn option_idle() -> Style {
    read(|p| p.option_idle)
}
pub fn marker_active() -> Style {
    read(|p| p.marker_active)
}
pub fn marker_idle() -> Style {
    read(|p| p.marker_idle)
}
pub fn required() -> Style {
    read(|p| p.required)
}
pub fn error() -> Style {
    read(|p| p.error)
}
pub fn progress_filled() -> Style {
    read(|p| p.progress_filled)
}
pub fn progress_empty() -> Style {
    read(|p| p.progress_empty)
}
pub fn key_hint() -> Style {
    read(|p| p.key_hint)
}
pub fn key_hint_desc() -> Style {
    read(|p| p.key_hint_desc)
}
pub fn page_button_focused() -> Style {
    read(|p| p.page_button_focused)
}
pub fn page_button_idle() -> Style {
    read(|p| p.page_button_idle)
}
pub fn cursor() -> Style {
    read(|p| p.cursor)
}

/// One-cell padding applied inside every page's body block. Shrinks
/// `rect` by 1 on all sides so the content doesn't butt up against the
/// border. Returns the original rect when it's too small to pad (so we
/// don't accidentally hand callers a zero-sized area on tiny terminals).
pub fn pad_inner(rect: Rect) -> Rect {
    if rect.width < 4 || rect.height < 4 {
        return rect;
    }
    Rect {
        x: rect.x + 1,
        y: rect.y + 1,
        width: rect.width - 2,
        height: rect.height - 2,
    }
}

/// Width of the progress bar in cells.
pub const PROGRESS_BAR_WIDTH: usize = 40;

/// Render a progress bar as a styled `(filled, empty)` span pair.
/// Returns `(filled_text, empty_text)` so the caller can wrap them in
/// Spans with `progress_filled` / `progress_empty` styles.
pub fn progress_chars(current: usize, total: usize) -> (String, String) {
    let total = total.max(1);
    let current = current.min(total);
    let filled = (current * PROGRESS_BAR_WIDTH) / total;
    let empty = PROGRESS_BAR_WIDTH - filled;
    ("█".repeat(filled), "░".repeat(empty))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_chars_total_width_matches_constant() {
        let (f, e) = progress_chars(3, 7);
        assert_eq!(
            f.chars().count() + e.chars().count(),
            PROGRESS_BAR_WIDTH,
            "filled + empty must always equal the bar width"
        );
    }

    #[test]
    fn progress_chars_clamps_current_over_total() {
        let (f, e) = progress_chars(99, 7);
        assert_eq!(f.chars().count(), PROGRESS_BAR_WIDTH);
        assert_eq!(e.chars().count(), 0);
    }

    #[test]
    fn progress_chars_handles_zero_total() {
        // Avoid division-by-zero; degrade to "fully empty".
        let (f, e) = progress_chars(0, 0);
        assert_eq!(f.chars().count() + e.chars().count(), PROGRESS_BAR_WIDTH);
    }

    #[test]
    fn pad_inner_shrinks_by_one_on_each_side() {
        let r = Rect {
            x: 5,
            y: 5,
            width: 20,
            height: 10,
        };
        let p = pad_inner(r);
        assert_eq!((p.x, p.y, p.width, p.height), (6, 6, 18, 8));
    }

    #[test]
    fn pad_inner_no_op_when_too_small() {
        let r = Rect {
            x: 0,
            y: 0,
            width: 3,
            height: 3,
        };
        assert_eq!(pad_inner(r), r);
    }

    #[test]
    fn palette_from_nord_theme_uses_nord_accent() {
        // Sanity: a manually-built Nord-ish theme produces a palette whose
        // accent styles use the configured fg colors.
        let mut theme = Theme::builtin_defaults();
        theme.text_focused = Style::default()
            .fg(Color::Rgb(0x88, 0xc0, 0xd0))
            .add_modifier(Modifier::BOLD);
        theme.text_selected = Style::default()
            .fg(Color::Rgb(0xeb, 0xcb, 0x8b))
            .add_modifier(Modifier::BOLD);
        let p = WizardPalette::from_theme(&theme);
        assert_eq!(p.section_header.fg, Some(Color::Rgb(0x88, 0xc0, 0xd0)));
        assert_eq!(p.label_focused.fg, Some(Color::Rgb(0xeb, 0xcb, 0x8b)));
    }
}
