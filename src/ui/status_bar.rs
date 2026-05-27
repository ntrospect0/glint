use chrono::Local;
use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

/// Bottom-of-screen status bar:
/// `glint vX.Y.Z │ HH:MM:SS │ Focus: <id> │ Last fetch: HH:MM:SS │ Tab: switch · q: quit`
pub fn render(
    frame: &mut Frame,
    area: Rect,
    focused_widget: Option<&str>,
    last_fetch: Option<chrono::DateTime<Local>>,
) {
    let now = Local::now();
    let clock = now.format("%H:%M:%S").to_string();
    let last = match last_fetch {
        Some(t) => t.format("%H:%M:%S").to_string(),
        None => "—".into(),
    };
    let version = env!("CARGO_PKG_VERSION");
    let focus = focused_widget.unwrap_or("—");

    let dim = Style::default().add_modifier(Modifier::DIM);
    let focus_style = Style::default()
        .fg(Color::LightCyan)
        .add_modifier(Modifier::BOLD);
    let sep = Span::styled("│", dim);

    let line = Line::from(vec![
        Span::styled(format!(" glint v{version} "), dim),
        sep.clone(),
        Span::styled(format!(" {clock} "), dim),
        sep.clone(),
        Span::styled(" Focus: ", dim),
        Span::styled(focus.to_string(), focus_style),
        Span::styled(" ", dim),
        sep.clone(),
        Span::styled(format!(" Last fetch: {last} "), dim),
        sep,
        Span::styled(" Tab: switch · q: quit ", dim),
    ])
    .alignment(Alignment::Left);

    frame.render_widget(Paragraph::new(line), area);
}
