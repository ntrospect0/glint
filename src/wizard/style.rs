//! Shared style palette for the wizard. Centralising the colour + modifier
//! choices here keeps the page renderers visually consistent and saves
//! every page from inventing its own ad-hoc `Style::default()` rules.
//!
//! The palette uses ratatui's named ANSI colours rather than hex so it
//! inherits the user's terminal theme — the wizard doesn't load the
//! glint app's full `Theme` (we'd have to plumb in colorschemes.toml just
//! for the setup flow) but ANSI colours respect the terminal's palette
//! mapping.

use ratatui::style::{Color, Modifier, Style};

/// Page section header — page title in the body chrome ("Welcome",
/// "Configure clock", etc.). Bold + magenta to stand out from field
/// content.
pub fn section_header() -> Style {
    Style::default()
        .fg(Color::Magenta)
        .add_modifier(Modifier::BOLD)
}

/// Page-level blurb sentence under the section header. Dim italic so it
/// reads as context, not a field.
pub fn blurb() -> Style {
    Style::default()
        .fg(Color::Gray)
        .add_modifier(Modifier::ITALIC)
}

/// Field label. Bold cyan; clearly distinguished from value rows + help.
pub fn label() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

/// Focused field label — same as `label` but with an additional underline
/// so the eye finds the active field immediately when scanning.
pub fn label_focused() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
}

/// Inline help / explanation text under a focused field. Dim gray so it
/// recedes from labels + values when scanning.
pub fn help_text() -> Style {
    Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC)
}

/// Field value when its field is NOT focused.
pub fn value_idle() -> Style {
    Style::default().fg(Color::White)
}

/// Field value when its field IS focused — single highlighted state used
/// by Text / Number / Bool / TextList. Choice/MultiChoice use the
/// per-row selection helpers below.
pub fn value_focused() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

/// A row inside a Choice / MultiChoice / Lookup list that's currently
/// highlighted by the up/down cursor.
pub fn option_selected() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

/// A row inside a Choice / MultiChoice / Lookup list that's NOT
/// highlighted.
pub fn option_idle() -> Style {
    Style::default().fg(Color::White)
}

/// The `[x]` / `(•)` filled-marker glyph: the option is currently picked
/// (radio) or checked (multi-select).
pub fn marker_active() -> Style {
    Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD)
}

/// The `[ ]` / `( )` empty marker.
pub fn marker_idle() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// "*" required-field indicator + general "needs attention" colour.
pub fn required() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

/// Inline validation error message.
pub fn error() -> Style {
    Style::default()
        .fg(Color::Red)
        .add_modifier(Modifier::BOLD)
}

/// Filled portion of the progress bar.
pub fn progress_filled() -> Style {
    Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD)
}

/// Unfilled portion of the progress bar.
pub fn progress_empty() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// Footer key-hint label (the actual key, e.g. "Tab").
pub fn key_hint() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

/// `[ Save & Next ]`-style page-advance button row, when it has focus.
/// Reuses the option_selected highlight so the visual idiom is
/// consistent with multi-choice cursor highlighting.
pub fn page_button_focused() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(Color::Green)
        .add_modifier(Modifier::BOLD)
}

/// Page-advance button when not focused — visible but not loud.
pub fn page_button_idle() -> Style {
    Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD)
}

/// Footer key-hint description ("focus", "advance"…).
pub fn key_hint_desc() -> Style {
    Style::default().fg(Color::Gray)
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
}
