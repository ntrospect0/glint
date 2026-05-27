pub mod big_digits;
pub mod help;
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

/// Per-frame render inputs grouped so the call site doesn't grow another arg
/// every time a new piece of UI state lands.
pub struct RenderState<'a> {
    pub layout: &'a LayoutConfig,
    pub manager: &'a WidgetManager,
    pub focused: Option<&'a str>,
    pub last_fetch: Option<DateTime<Local>>,
    pub show_help: bool,
    /// `Some` while the user is composing a command at the `:` bar.
    pub command_buffer: Option<&'a str>,
    /// Transient feedback line shown when a command fails.
    pub command_feedback: Option<&'a str>,
}

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
/// to the bottom row. Unknown widget ids render a stub placeholder. The help
/// overlay is drawn last on top of everything when enabled.
pub fn render(frame: &mut Frame, state: &RenderState) {
    let area = frame.area();
    // The command bar takes one row when active, immediately above the status
    // bar. When inactive the same row is part of the main grid.
    let command_bar_h: u16 = if state.command_buffer.is_some() { 1 } else { 0 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(command_bar_h),
            Constraint::Length(1),
        ])
        .split(area);
    let main_area = chunks[0];
    let command_area = chunks[1];
    let status_area = chunks[2];

    for resolved in state.layout.resolve(main_area) {
        let id = resolved.cell.widget.as_str();
        let is_focused = state.focused == Some(id);
        match state.manager.get(id) {
            Some(widget) => widget.render(frame, resolved.area, is_focused),
            None => render_unknown(frame, resolved.area, id, is_focused),
        }
    }

    if let Some(buf) = state.command_buffer {
        render_command_bar(frame, command_area, buf, state.command_feedback);
    }

    status_bar::render(frame, status_area, state.focused, state.last_fetch);

    if state.show_help {
        let sections = build_help_sections(state.layout, state.manager);
        help::render(frame, area, &sections);
    }
}

fn render_command_bar(
    frame: &mut Frame,
    area: Rect,
    buffer: &str,
    feedback: Option<&str>,
) {
    let mut spans = vec![
        Span::styled(
            ":",
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(buffer.to_string()),
        Span::styled("▏", Style::default().fg(Color::LightCyan)),
    ];
    if let Some(msg) = feedback {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            msg.to_string(),
            Style::default().fg(Color::LightRed),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn build_help_sections(
    layout: &LayoutConfig,
    manager: &WidgetManager,
) -> Vec<help::Section> {
    let global = help::Section {
        title: "Global".into(),
        bindings: vec![
            ("Tab / Shift+Tab".into(), "cycle focused widget".into()),
            ("click cell".into(), "focus that widget".into()),
            ("?".into(), "toggle this help overlay".into()),
            ("q · Ctrl+C".into(), "quit".into()),
        ],
    };
    let mut sections = vec![global];

    // Per-widget sections, ordered by layout appearance.
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for cell in &layout.cells {
        let id = cell.widget.as_str();
        if !seen.insert(id) {
            continue;
        }
        let Some(widget) = manager.get(id) else {
            continue;
        };
        let bindings: Vec<(String, String)> = widget
            .keybindings()
            .into_iter()
            .map(|(k, d)| (k.to_string(), d.to_string()))
            .collect();
        if bindings.is_empty() {
            continue;
        }
        sections.push(help::Section {
            title: widget.display_name().to_string(),
            bindings,
        });
    }
    sections
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
