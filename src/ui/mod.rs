pub mod big_digits;
pub mod help;
pub mod status_bar;

use std::{cell::Cell, sync::Arc};

use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};

use crate::config::LayoutConfig;
use crate::theme::Theme;
use crate::widgets::WidgetManager;

/// Per-frame render inputs grouped so the call site doesn't grow another arg
/// every time a new piece of UI state lands.
pub struct RenderState<'a> {
    pub layout: &'a LayoutConfig,
    pub manager: &'a WidgetManager,
    pub focused: Option<&'a str>,
    pub show_help: bool,
    /// `Some` while the user is composing a command at the `:` bar.
    pub command_buffer: Option<&'a str>,
    /// Transient feedback line shown when a command fails.
    pub command_feedback: Option<&'a str>,
    /// App-level resolved palette used by chrome (command bar, help overlay,
    /// the unknown-widget placeholder). Each widget already carries its own
    /// merged theme — chrome doesn't need to redo that work.
    pub theme: &'a Arc<Theme>,
    /// Name of the active color scheme (matches a `[schemes.<name>]` block
    /// in colorschemes.toml). Used by the help overlay to mark the current
    /// scheme in its listing.
    pub theme_name: &'a str,
    /// Row offset to apply to the help overlay's vertical scroll. App
    /// owns the canonical value; render reads it and applies it via
    /// `Paragraph::scroll`.
    pub help_scroll: u16,
    /// Read/write cell where `help::render` writes back the maximum
    /// scroll value it computed for the current viewport. App's scroll
    /// handler reads this on the next key/wheel event so it can clamp
    /// without re-running the layout math.
    pub help_scroll_max: &'a Cell<u16>,
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

/// Decorate a widget title with the focus arrows AND paint the assigned
/// shortcut letter in `text.shortcut`. Returns a `Line` of spans suitable
/// for `Block::title`.
///
/// Letter placement: case-insensitive search of `base` for the shortcut
/// letter. The matching letter is uppercased and styled with
/// `shortcut_style`. If the letter isn't found, the title is prefixed with
/// a `[X] ` badge in `shortcut_style` so the user still sees which key
/// focuses this widget.
pub fn decorated_title_line(
    focused: bool,
    base: &str,
    shortcut: Option<char>,
    title_style: Style,
    shortcut_style: Style,
) -> Line<'static> {
    let (prefix, suffix) = if focused {
        (" ▶ ", " ◀ ")
    } else {
        (" ", " ")
    };
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(5);
    spans.push(Span::styled(prefix.to_string(), title_style));

    match shortcut {
        Some(letter) => {
            let lower = letter.to_ascii_lowercase();
            let chars: Vec<char> = base.chars().collect();
            if let Some(idx) = chars.iter().position(|c| c.to_ascii_lowercase() == lower) {
                let before: String = chars[..idx].iter().collect();
                let target = chars[idx].to_ascii_uppercase();
                let after: String = chars[idx + 1..].iter().collect();
                if !before.is_empty() {
                    spans.push(Span::styled(before, title_style));
                }
                spans.push(Span::styled(target.to_string(), shortcut_style));
                if !after.is_empty() {
                    spans.push(Span::styled(after, title_style));
                }
            } else {
                // Letter isn't in the title — show it as a leading badge
                // so the user still has a visible hint about which key
                // focuses this widget.
                spans.push(Span::styled(
                    format!("[{}] ", letter.to_ascii_uppercase()),
                    shortcut_style,
                ));
                spans.push(Span::styled(base.to_string(), title_style));
            }
        }
        None => {
            spans.push(Span::styled(base.to_string(), title_style));
        }
    }

    spans.push(Span::styled(suffix.to_string(), title_style));
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn title_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn shortcut_span(line: &Line<'static>, shortcut_style: Style) -> Option<String> {
        line.spans
            .iter()
            .find(|s| s.style == shortcut_style)
            .map(|s| s.content.to_string())
    }

    #[test]
    fn decorated_title_paints_first_matching_letter_uppercased() {
        let title_style = Style::default();
        let shortcut_style = Style::default().fg(Color::Red);
        let line =
            decorated_title_line(false, "Calendar", Some('d'), title_style, shortcut_style);
        assert_eq!(title_text(&line), " CalenDar ");
        assert_eq!(shortcut_span(&line, shortcut_style).as_deref(), Some("D"));
    }

    #[test]
    fn decorated_title_paints_first_letter_when_shortcut_matches_it() {
        let title_style = Style::default();
        let shortcut_style = Style::default().fg(Color::Red);
        let line = decorated_title_line(false, "Clock", Some('c'), title_style, shortcut_style);
        assert_eq!(title_text(&line), " Clock ");
        assert_eq!(shortcut_span(&line, shortcut_style).as_deref(), Some("C"));
    }

    #[test]
    fn decorated_title_falls_back_to_bracket_badge_when_letter_absent() {
        let title_style = Style::default();
        let shortcut_style = Style::default().fg(Color::Red);
        let line = decorated_title_line(false, "Weather", Some('z'), title_style, shortcut_style);
        // Z isn't in "Weather" — we should see a "[Z] " prefix in the
        // shortcut color, then the title in the default style.
        assert!(title_text(&line).contains("[Z] Weather"));
        assert_eq!(
            shortcut_span(&line, shortcut_style).as_deref(),
            Some("[Z] ")
        );
    }

    #[test]
    fn decorated_title_omits_shortcut_color_when_none_assigned() {
        let title_style = Style::default();
        let shortcut_style = Style::default().fg(Color::Red);
        let line = decorated_title_line(true, "Stocks", None, title_style, shortcut_style);
        assert_eq!(title_text(&line), " ▶ Stocks ◀ ");
        assert!(
            shortcut_span(&line, shortcut_style).is_none(),
            "no shortcut → no span painted with shortcut_style"
        );
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
            None => render_unknown(frame, resolved.area, id, is_focused, state.theme),
        }
    }

    if let Some(buf) = state.command_buffer {
        render_command_bar(frame, command_area, buf, state.command_feedback, state.theme);
    }

    status_bar::render(
        frame,
        status_area,
        state.focused,
        state.theme_name,
        state.theme,
    );

    if state.show_help {
        let sections = build_help_sections(state.layout, state.manager, state.theme_name);
        help::render(
            frame,
            area,
            &sections,
            state.theme,
            state.help_scroll,
            state.help_scroll_max,
        );
    }
}

fn render_command_bar(
    frame: &mut Frame,
    area: Rect,
    buffer: &str,
    feedback: Option<&str>,
    theme: &Theme,
) {
    // Cursor caret strips bold/bg from the focused-text style so the bar
    // line stays calm even when the scheme makes text.focused heavy.
    let caret_style = Style::default().fg(theme.text_focused.fg.unwrap_or(Color::LightCyan));
    let mut spans = vec![
        Span::styled(":", theme.text_focused),
        Span::raw(buffer.to_string()),
        Span::styled("▏", caret_style),
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
    active_scheme: &str,
) -> Vec<help::Section> {
    let global = help::Section {
        title: "Global".into(),
        bindings: vec![
            ("Tab / Shift+Tab".into(), "cycle focused widget".into()),
            (
                "Shift+<letter>".into(),
                "jump focus to widget (red letter in title)".into(),
            ),
            ("click cell".into(), "focus that widget".into()),
            (":".into(), "open command bar".into()),
            (
                ":scheme <name>".into(),
                "switch color scheme (see list below)".into(),
            ),
            (":news <terms>".into(), "filter news by keyword".into()),
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

    // Append a "Color schemes" section that lists every named scheme in
    // ~/.config/glint/colorschemes.toml so the user doesn't have to
    // remember them. Marks the active one with `●`. Read errors and the
    // missing-file case both yield an empty section (skipped silently).
    if let Ok(file) = crate::theme::load_schemes_file() {
        let mut names: Vec<&str> = file.schemes.keys().map(String::as_str).collect();
        names.sort_unstable();
        if !names.is_empty() {
            let bindings: Vec<(String, String)> = names
                .into_iter()
                .map(|name| {
                    let key = if name == active_scheme {
                        format!("● {name}")
                    } else {
                        format!("  {name}")
                    };
                    let desc = if name == active_scheme {
                        "active scheme".to_string()
                    } else {
                        format!(":scheme {name}")
                    };
                    (key, desc)
                })
                .collect();
            sections.push(help::Section {
                title: "Color schemes".into(),
                bindings,
            });
        }
    }

    sections
}

fn render_unknown(frame: &mut Frame, area: Rect, id: &str, focused: bool, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.border_style(focused))
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
