use chrono::Local;
use ratatui::{
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::theme::Theme;

/// Bottom-of-screen status bar:
/// `glint vX.Y.Z │ HH:MM:SS │ Focus: <id> │ Scheme: <name> │ Tab: switch · ? help · q quit`
pub fn render(
    frame: &mut Frame,
    area: Rect,
    focused_widget: Option<&str>,
    scheme_name: &str,
    theme: &Theme,
) {
    let now = Local::now();
    let clock = now.format("%H:%M:%S").to_string();
    let version = env!("CARGO_PKG_VERSION");
    let focus = focused_widget.unwrap_or("—");

    let dim = Style::default().add_modifier(Modifier::DIM);
    let sep = Span::styled("│", dim);

    let line = Line::from(vec![
        Span::styled(format!(" glint v{version} "), dim),
        sep.clone(),
        Span::styled(format!(" {clock} "), dim),
        sep.clone(),
        Span::styled(" Focus: ", dim),
        Span::styled(focus.to_string(), theme.text_focused),
        Span::styled(" ", dim),
        sep.clone(),
        Span::styled(" Scheme: ", dim),
        Span::styled(scheme_name.to_string(), theme.text_selected),
        Span::styled(" ", dim),
        sep,
        Span::styled(" Tab: switch · ? help · q quit ", dim),
    ])
    .alignment(Alignment::Left);

    frame.render_widget(Paragraph::new(line), area);
}
