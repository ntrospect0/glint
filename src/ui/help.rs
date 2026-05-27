use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

/// One labeled group of keybindings — typically "Global" plus one section
/// per focusable widget.
pub struct Section {
    pub title: String,
    pub bindings: Vec<(String, String)>,
}

/// Renders the help overlay centered in `area`. The caller is responsible for
/// only invoking this when help is toggled on.
pub fn render(frame: &mut Frame, area: Rect, sections: &[Section]) {
    let overlay = center_rect(area, 70, 80);

    // Wipe whatever's underneath so the dashboard doesn't bleed through.
    frame.render_widget(Clear, overlay);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        )
        .title(Span::styled(
            " ? Help ",
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        ));
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
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        )));
        for (key, desc) in &section.bindings {
            lines.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(
                    format!("{:<width$}", key, width = max_key_width),
                    Style::default()
                        .fg(Color::LightCyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::raw(desc.clone()),
            ]));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Press ? or Esc to close",
        Style::default().add_modifier(Modifier::DIM),
    )));

    frame.render_widget(
        Paragraph::new(lines).alignment(Alignment::Left),
        inner,
    );
}

/// Center a rect of `width_pct%` × `height_pct%` of `area`.
fn center_rect(area: Rect, width_pct: u16, height_pct: u16) -> Rect {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_pct) / 2),
            Constraint::Percentage(height_pct),
            Constraint::Percentage((100 - height_pct) / 2),
        ])
        .split(area);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_pct) / 2),
            Constraint::Percentage(width_pct),
            Constraint::Percentage((100 - width_pct) / 2),
        ])
        .split(rows[1]);
    cols[1]
}
