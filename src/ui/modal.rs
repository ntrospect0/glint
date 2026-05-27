// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0
//
// Shared confirm-modal primitive for widgets.

//! Centred y/N confirmation modal — the rendering and key dispatch
//! that `notes::confirm_delete`, `stocks::confirm_remove`, and
//! `forex::confirm_remove` were each reimplementing.
//!
//! Each widget keeps its own `Option<T>` state slot (T is typically
//! a `String` identifier — a note name, a ticker symbol, a currency
//! code) so the widget retains control over what "confirmation
//! target" actually means and how to act on it. This module only
//! owns the *presentation* (rounded border, theme-aware title bar,
//! centred target-name body, "[y] confirm" hint) and the *key
//! dispatch* (y/Y commits, any other key cancels).
//!
//! See `docs/widget-sdk.md` § Confirm modal.

#![allow(dead_code)] // some accessors are SDK surface for future widgets.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::theme::Theme;

/// User's choice when the confirm modal is open. Returned by
/// [`dispatch_key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmChoice {
    /// User pressed `y`/`Y` — caller commits the action and clears
    /// its `Option<T>` state slot.
    Confirm,
    /// User pressed anything else — caller clears the state slot
    /// without acting.
    Cancel,
}

/// Configuration for one render of the confirm modal. Borrowed
/// strings so callers can build it inline from their state without
/// extra allocations.
#[derive(Debug, Clone, Copy)]
pub struct ConfirmModal<'a> {
    /// Title bar text including its leading + trailing spaces.
    /// Example: `" Remove ticker? "`, `" Delete note? "`.
    pub title: &'a str,
    /// The identifier being confirmed against — the symbol, name,
    /// currency code, etc. Rendered bold + brilliant in the body.
    pub target: &'a str,
    /// Optional override for the default action hint. `None` →
    /// `"  [y] confirm  ·  any other key cancels"`. Pass `Some(_)`
    /// when the widget wants different verbiage (e.g. "drop").
    pub hint: Option<&'a str>,
    /// Upper bound on the modal's inner width. Widget-specific —
    /// notes likes 54 (long note names); stocks/forex 48 is enough
    /// for tickers. Always clamped down to `parent.width` and up to
    /// `MIN_WIDTH`.
    pub max_width: u16,
}

/// Minimum modal width — anything narrower and the title + hint
/// stop fitting legibly.
const MIN_WIDTH: u16 = 28;

/// Fixed modal height: blank · target · blank · hint = 4 body
/// rows, plus 2 for borders, plus 1 for spacing = 7.
const HEIGHT: u16 = 7;

/// Paint the confirm overlay centred inside `parent`. Pulls accent
/// colors from `theme` (so the title bar inherits the active
/// scheme's `text.selected` instead of hardcoded yellow). No-op
/// when `parent` is too small to host the modal.
pub fn render(frame: &mut Frame, parent: Rect, theme: &Theme, modal: ConfirmModal<'_>) {
    if parent.width < MIN_WIDTH + 2 || parent.height < HEIGHT + 2 {
        // No usable space — fall through silently. Widgets that
        // care should be surfacing the situation in their status
        // line already.
        return;
    }
    let inner_w = parent.width.min(modal.max_width).max(MIN_WIDTH);
    let x = parent.x + parent.width.saturating_sub(inner_w) / 2;
    let y = parent.y + parent.height.saturating_sub(HEIGHT) / 2;
    let rect = Rect {
        x,
        y,
        width: inner_w,
        height: HEIGHT,
    };
    frame.render_widget(Clear, rect);
    let title_bg = theme.text_selected.fg.unwrap_or(Color::Yellow);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.border_focused)
        .title(Span::styled(
            modal.title.to_string(),
            Style::default()
                .fg(Color::Black)
                .bg(title_bg)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let hint = modal
        .hint
        .unwrap_or("  [y] confirm  ·  any other key cancels");
    let lines: Vec<Line> = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(modal.target.to_string(), theme.text_brilliant),
        ]),
        Line::from(""),
        Line::from(Span::styled(hint.to_string(), theme.text_dim)),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Map a key event to a [`ConfirmChoice`]. Returns `None` when the
/// caller's modal isn't actually open (so callers can avoid an
/// extra `is_some()` check at their call site by routing every key
/// through here while the slot is `Some`).
pub fn dispatch_key(key: KeyEvent) -> ConfirmChoice {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => ConfirmChoice::Confirm,
        _ => ConfirmChoice::Cancel,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn dispatch_y_lowercase_confirms() {
        assert_eq!(dispatch_key(key('y')), ConfirmChoice::Confirm);
    }

    #[test]
    fn dispatch_y_uppercase_confirms() {
        assert_eq!(dispatch_key(key('Y')), ConfirmChoice::Confirm);
    }

    #[test]
    fn dispatch_any_other_letter_cancels() {
        for c in ['n', 'N', 'q', 'Q', ' ', 'x', '?'] {
            assert_eq!(
                dispatch_key(key(c)),
                ConfirmChoice::Cancel,
                "key {c:?} should cancel"
            );
        }
    }

    #[test]
    fn dispatch_special_keys_cancel() {
        assert_eq!(
            dispatch_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            ConfirmChoice::Cancel
        );
        assert_eq!(
            dispatch_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ConfirmChoice::Cancel
        );
    }
}
