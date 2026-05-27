use ratatui::{
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::config::LayoutConfig;
use crate::widgets::WidgetManager;

/// Render the full frame: each grid cell delegated to its widget,
/// unknown widgets rendered as a stub placeholder.
pub fn render(
    frame: &mut Frame,
    layout: &LayoutConfig,
    manager: &WidgetManager,
    focused_widget: Option<&str>,
) {
    let area = frame.area();
    for resolved in layout.resolve(area) {
        let id = resolved.cell.widget.as_str();
        let is_focused = focused_widget == Some(id);
        match manager.get(id) {
            Some(widget) => widget.render(frame, resolved.area, is_focused),
            None => render_unknown(frame, resolved.area, id, is_focused),
        }
    }
}

fn render_unknown(frame: &mut Frame, area: Rect, id: &str, focused: bool) {
    let border_style = if focused {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(
            format!(" {id} "),
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
