pub mod status_bar;

use chrono::{DateTime, Local};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::config::LayoutConfig;
use crate::widgets::WidgetManager;

/// Border style for a widget cell. Focused = bright cyan + bold so it stands
/// out even on terminals that render bold box-drawing characters identically
/// to non-bold (which is most of them).
pub fn focus_border_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(Color::LightCyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

/// Wrap a widget's title for display in its border. Focused cells get a
/// `▶ … ◀` decoration so focus is obvious without relying on border color.
pub fn decorate_title(focused: bool, base: &str) -> String {
    if focused {
        format!(" ▶ {base} ◀ ")
    } else {
        format!(" {base} ")
    }
}

/// Render the full frame: grid cells on top, single-line status bar pinned
/// to the bottom row. Unknown widget ids render a stub placeholder.
pub fn render(
    frame: &mut Frame,
    layout: &LayoutConfig,
    manager: &WidgetManager,
    focused_widget: Option<&str>,
    last_fetch: Option<DateTime<Local>>,
) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let main_area = chunks[0];
    let status_area = chunks[1];

    for resolved in layout.resolve(main_area) {
        let id = resolved.cell.widget.as_str();
        let is_focused = focused_widget == Some(id);
        match manager.get(id) {
            Some(widget) => widget.render(frame, resolved.area, is_focused),
            None => render_unknown(frame, resolved.area, id, is_focused),
        }
    }

    status_bar::render(frame, status_area, focused_widget, last_fetch);
}

fn render_unknown(frame: &mut Frame, area: Rect, id: &str, focused: bool) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(focus_border_style(focused))
        .title(Span::styled(
            decorate_title(focused, id),
            Style::default().add_modifier(Modifier::DIM),
        ));
    let body = Paragraph::new(vec![
        Line::from(""),
        Line::from(format!("Widget '{id}' is not registered.")),
        Line::from(""),
        Line::from("Coming in a later phase."),
    ])
    .alignment(Alignment::Center)
    .block(block);
    frame.render_widget(body, area);
}
