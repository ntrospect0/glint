// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

use std::cell::Cell;

use ratatui::{
    layout::{Alignment, Rect},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::theme::Theme;

/// One labeled group of keybindings — typically "Global" plus one section
/// per focusable widget.
pub struct Section {
    pub title: String,
    pub bindings: Vec<(String, String)>,
}

/// Renders the help overlay centered in `area`. The caller is responsible
/// for only invoking this when help is toggled on.
///
/// `scroll` is the row offset applied to the body Paragraph — clamped
/// inside this function against the actual content height so over-scroll
/// never blanks the overlay. After clamping, the max-scroll value is
/// written back through `scroll_max_out` so the App's keyboard/mouse
/// scroll handler can clamp the next event without re-doing the layout.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    sections: &[Section],
    theme: &Theme,
    scroll: u16,
    scroll_max_out: &Cell<u16>,
) {
    let overlay = super::center_rect(area, 70, 80);

    // Wipe whatever's underneath so the dashboard doesn't bleed through.
    frame.render_widget(Clear, overlay);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.border_focused)
        .title(Span::styled(" ? Help ", theme.text_focused));
    let inner = block.inner(overlay);
    frame.render_widget(block, overlay);

    let max_key_width = sections
        .iter()
        .flat_map(|s| s.bindings.iter())
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0);

    let mut lines: Vec<Line<'_>> = Vec::new();
    lines.push(Line::from(""));
    for (i, section) in sections.iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            format!(" {}", section.title),
            theme.text_selected,
        )));
        for (key, desc) in &section.bindings {
            lines.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(
                    format!("{:<width$}", key, width = max_key_width),
                    theme.text_focused,
                ),
                Span::raw("  "),
                Span::raw(desc.clone()),
            ]));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Press ? or Esc to close · ↑/↓ scroll · PgUp/PgDn page",
        theme.text_dim,
    )));

    // Compute scroll bounds and clamp before applying. The body uses every
    // inner row; `Paragraph::scroll` clips lines above the viewport, so the
    // max useful offset is total_lines - viewport_height.
    let total = lines.len() as u16;
    let viewport_h = inner.height;
    let max_scroll = total.saturating_sub(viewport_h);
    scroll_max_out.set(max_scroll);
    let effective_scroll = scroll.min(max_scroll);

    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Left)
            .scroll((effective_scroll, 0)),
        inner,
    );
}

