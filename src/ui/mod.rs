// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

pub mod big_digits;
pub mod help;
pub mod status_bar;

use std::{cell::Cell, sync::Arc};

use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{block::Title, Block, BorderType, Borders, Paragraph},
    Frame,
};

use crate::config::LayoutConfig;
use crate::theme::Theme;
use crate::widgets::WidgetManager;

/// Severity tag attached to a command-bar feedback message. Determines
/// which theme role colors the message line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackSeverity {
    /// Successful command outcome (e.g. "scheme → tokyonight").
    Confirmation,
    /// Usage hints, missing-argument prompts, non-fatal warnings.
    Warning,
    /// Failed commands and other hard errors.
    Error,
}

/// Per-frame render inputs grouped so the call site doesn't grow another arg
/// every time a new piece of UI state lands.
pub struct RenderState<'a> {
    pub layout: &'a LayoutConfig,
    pub manager: &'a WidgetManager,
    pub focused: Option<&'a str>,
    pub show_help: bool,
    /// `Some` while the user is composing a command at the `:` bar.
    pub command_buffer: Option<&'a str>,
    /// Transient feedback line shown after a command runs. Caller is
    /// responsible for expiring the message (e.g. clearing it on the
    /// next tick after `feedback_ttl`).
    pub command_feedback: Option<(&'a str, FeedbackSeverity)>,
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
    /// When `false`, the bottom status bar row is suppressed and that
    /// row goes back to the widget grid. Mirrors `[global]
    /// show_status_bar` in config.toml.
    pub show_status_bar: bool,
}

/// Minimum char gap between the title and the right-aligned metadata on
/// the top border. Below this we hide the metadata entirely rather than
/// let the two strings collide.
const TITLE_METADATA_MIN_GAP: usize = 3;

/// Build the title row for a widget's border. The title is the
/// left-aligned `Line` painted just after `┌─`; the optional metadata is
/// the right-aligned `Line` painted just before `─┐`. Caller wraps each
/// in `Title::from(line).alignment(...)` and hands them to
/// `Block::title(...)`, or uses [`apply_title_row`] for the common case.
///
/// Focused/unfocused styling lives entirely in the [`Theme`]:
/// * Title text uses `widget_title.focused` vs `widget_title.unfocused`
///   — the focused variant typically carries a background highlight so
///   the user can spot focus without the title shifting position.
/// * The shortcut letter (the one `Shift+<letter>` focuses) is always
///   painted in `text.shortcut`, regardless of focus state.
/// * Metadata uses `metadata.focused` vs `metadata.unfocused` — usually
///   a quieter color than the title, dimmed when the pane isn't focused.
///
/// Letter placement: case-insensitive search of `base` for the shortcut
/// letter. The matching letter is uppercased and styled. If the letter
/// isn't in the title, a `[X] ` badge is prefixed instead so the user
/// still sees the key hint.
///
/// Metadata is dropped (returned as `None`) when the pane is too narrow
/// to fit `title + min_gap + metadata` inside the inner width.
pub fn title_row(
    focused: bool,
    base: &str,
    metadata: Option<&str>,
    shortcut: Option<char>,
    theme: &Theme,
    area_width: u16,
) -> (Line<'static>, Option<Line<'static>>) {
    let title = build_title_line(
        focused,
        base,
        shortcut,
        theme.widget_title_style(focused),
        theme.text_shortcut,
        theme.border_focused,
    );
    let metadata_line = metadata
        .filter(|s| !s.is_empty())
        .map(|meta| build_metadata_line(meta, theme.metadata_style(focused)));

    let inner_w = (area_width as usize).saturating_sub(2);
    let fits = match &metadata_line {
        Some(m) => line_width(&title) + TITLE_METADATA_MIN_GAP + line_width(m) <= inner_w,
        None => false,
    };
    (title, if fits { metadata_line } else { None })
}

/// Attach the title row to a block. Equivalent to calling [`title_row`]
/// and wrapping each non-empty line in a `Title` with the correct
/// alignment; provided so widgets don't repeat the boilerplate.
pub fn apply_title_row<'a>(
    block: Block<'a>,
    focused: bool,
    base: &str,
    metadata: Option<&str>,
    shortcut: Option<char>,
    theme: &Theme,
    area_width: u16,
) -> Block<'a> {
    let (title, meta) = title_row(focused, base, metadata, shortcut, theme, area_width);
    let mut block = block.title(Title::from(title).alignment(Alignment::Left));
    if let Some(meta) = meta {
        block = block.title(Title::from(meta).alignment(Alignment::Right));
    }
    block
}

/// Build the title text with the shortcut letter highlighted. The title
/// is always wrapped in a 1-char pad cell on each side; when the pane is
/// focused those cells render as `┤` / `├` tee-junction glyphs in the
/// border-focused color (the title visually notches into the border line
/// like a labeled segment, btop-style). When unfocused the pad cells are
/// just spaces. Width is constant across focus states so the title chars
/// never shift position.
fn build_title_line(
    focused: bool,
    base: &str,
    shortcut: Option<char>,
    title_style: Style,
    shortcut_style: Style,
    bracket_style: Style,
) -> Line<'static> {
    let (left_pad, right_pad) = if focused { ("┤", "├") } else { (" ", " ") };
    let pad_style = if focused { bracket_style } else { title_style };
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(5);
    spans.push(Span::styled(left_pad.to_string(), pad_style));
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
    spans.push(Span::styled(right_pad.to_string(), pad_style));
    Line::from(spans)
}

fn build_metadata_line(meta: &str, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(" ".to_string(), style),
        Span::styled(meta.to_string(), style),
        Span::styled(" ".to_string(), style),
    ])
}

fn line_width(line: &Line<'_>) -> usize {
    line.spans.iter().map(|s| s.content.chars().count()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn shortcut_span(line: &Line<'static>, shortcut_style: Style) -> Option<String> {
        line.spans
            .iter()
            .find(|s| s.style == shortcut_style)
            .map(|s| s.content.to_string())
    }

    #[test]
    fn title_row_paints_first_matching_letter_uppercased() {
        let theme = Theme::builtin_defaults();
        let (title, _) = title_row(false, "Calendar", None, Some('d'), &theme, 40);
        assert_eq!(line_text(&title), " CalenDar ");
        assert_eq!(
            shortcut_span(&title, theme.text_shortcut).as_deref(),
            Some("D")
        );
    }

    #[test]
    fn title_row_paints_first_letter_when_shortcut_matches_it() {
        let theme = Theme::builtin_defaults();
        let (title, _) = title_row(false, "Clock", None, Some('c'), &theme, 40);
        assert_eq!(line_text(&title), " Clock ");
        assert_eq!(
            shortcut_span(&title, theme.text_shortcut).as_deref(),
            Some("C")
        );
    }

    #[test]
    fn title_row_falls_back_to_bracket_badge_when_letter_absent() {
        let theme = Theme::builtin_defaults();
        let (title, _) = title_row(false, "Weather", None, Some('z'), &theme, 40);
        let text = line_text(&title);
        assert!(text.contains("[Z] Weather"));
        assert_eq!(
            shortcut_span(&title, theme.text_shortcut).as_deref(),
            Some("[Z] ")
        );
    }

    #[test]
    fn title_row_uses_focused_style_when_focused() {
        let theme = Theme::builtin_defaults();
        let (focused, _) = title_row(true, "Clock", None, None, &theme, 40);
        let (unfocused, _) = title_row(false, "Clock", None, None, &theme, 40);
        // "Clock" sits in spans[1] (after the leading-pad span).
        assert_eq!(focused.spans[1].style, theme.widget_title_focused);
        assert_eq!(unfocused.spans[1].style, theme.widget_title_unfocused);
    }

    #[test]
    fn title_row_pads_with_tee_brackets_when_focused() {
        // Focus indicator: leading and trailing pad cells become ┤ / ├
        // glyphs styled in the border-focused color so the title
        // notches into the surrounding border line.
        let theme = Theme::builtin_defaults();
        let (focused, _) = title_row(true, "Clock", None, None, &theme, 40);
        let (unfocused, _) = title_row(false, "Clock", None, None, &theme, 40);
        assert_eq!(focused.spans.first().map(|s| s.content.as_ref()), Some("┤"));
        assert_eq!(focused.spans.last().map(|s| s.content.as_ref()), Some("├"));
        assert_eq!(focused.spans.first().unwrap().style, theme.border_focused);
        assert_eq!(unfocused.spans.first().map(|s| s.content.as_ref()), Some(" "));
        assert_eq!(unfocused.spans.last().map(|s| s.content.as_ref()), Some(" "));
        // Width invariant: pad slots stay 1 char in both states.
        let focused_text: String =
            focused.spans.iter().map(|s| s.content.as_ref()).collect();
        let unfocused_text: String =
            unfocused.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(focused_text.chars().count(), unfocused_text.chars().count());
    }

    #[test]
    fn title_row_omits_metadata_when_absent_or_empty() {
        let theme = Theme::builtin_defaults();
        let (_, meta) = title_row(true, "Weather", None, None, &theme, 60);
        assert!(meta.is_none());
        let (_, meta_empty) = title_row(true, "Weather", Some(""), None, &theme, 60);
        assert!(meta_empty.is_none());
    }

    #[test]
    fn title_row_returns_metadata_when_pane_wide_enough() {
        let theme = Theme::builtin_defaults();
        let (title, meta) =
            title_row(true, "Weather", Some("Richmond, BC"), Some('w'), &theme, 60);
        let meta = meta.expect("wide pane should keep metadata");
        assert!(line_text(&title).contains("Weather"));
        assert!(line_text(&meta).contains("Richmond, BC"));
        let body = meta
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "Richmond, BC")
            .expect("metadata body span");
        assert_eq!(body.style, theme.metadata_focused);
    }

    #[test]
    fn title_row_drops_metadata_on_narrow_pane() {
        let theme = Theme::builtin_defaults();
        let (_, meta) =
            title_row(true, "Weather", Some("Richmond, BC"), Some('w'), &theme, 20);
        assert!(meta.is_none(), "narrow pane should hide metadata");
    }

    #[test]
    fn title_row_metadata_dims_when_pane_unfocused() {
        let theme = Theme::builtin_defaults();
        let (_, meta) =
            title_row(false, "Weather", Some("Richmond, BC"), None, &theme, 60);
        let meta = meta.expect("metadata visible at this width");
        let body = meta
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "Richmond, BC")
            .expect("metadata body span");
        assert_eq!(body.style, theme.metadata_unfocused);
        assert_ne!(
            theme.metadata_focused, theme.metadata_unfocused,
            "metadata styles should differ between states"
        );
    }

    #[test]
    fn title_row_never_emits_focus_arrows() {
        let theme = Theme::builtin_defaults();
        let (title, _) = title_row(true, "Stocks", None, Some('s'), &theme, 40);
        let text = line_text(&title);
        assert!(!text.contains('▶'));
        assert!(!text.contains('◀'));
    }
}

/// Render the full frame: grid cells on top, single-line status bar pinned
/// to the bottom row. Unknown widget ids render a stub placeholder. The help
/// overlay is drawn last on top of everything when enabled.
pub fn render(frame: &mut Frame, state: &RenderState) {
    let area = frame.area();
    // Bottom-of-screen "chrome row" — a single row that swaps between
    // the command bar, transient feedback, and the status bar. The row
    // is suppressed entirely (handed back to the widget grid) only when
    // the status bar is hidden AND there's no command in flight or
    // recent feedback to surface.
    let chrome_visible = state.command_buffer.is_some()
        || state.command_feedback.is_some()
        || state.show_status_bar;
    let chrome_h: u16 = if chrome_visible { 1 } else { 0 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(chrome_h)])
        .split(area);
    let main_area = chunks[0];
    let chrome_area = chunks[1];

    for resolved in state.layout.resolve(main_area) {
        let Some(id) = resolved.cell.render_target_id() else {
            continue;
        };
        let is_focused = state.focused == Some(id.as_str());
        match state.manager.get(&id) {
            Some(widget) => widget.render(frame, resolved.area, is_focused),
            None => render_unknown(frame, resolved.area, &id, is_focused, state.theme),
        }
    }

    // Priority: active typing wins (user is mid-command); fresh feedback
    // beats the static status bar; status bar is the idle fallback.
    if chrome_h > 0 {
        if let Some(buf) = state.command_buffer {
            render_command_bar(frame, chrome_area, buf, state.theme);
        } else if let Some((msg, severity)) = state.command_feedback {
            render_feedback(frame, chrome_area, msg, severity, state.theme);
        } else if state.show_status_bar {
            status_bar::render(
                frame,
                chrome_area,
                state.focused,
                state.theme_name,
                state.theme,
            );
        }
    }

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

fn render_command_bar(frame: &mut Frame, area: Rect, buffer: &str, theme: &Theme) {
    // Cursor caret strips bold/bg from the focused-text style so the bar
    // line stays calm even when the scheme makes text.focused heavy.
    let caret_style = Style::default().fg(theme.text_focused.fg.unwrap_or(Color::LightCyan));
    let spans = vec![
        Span::styled(":", theme.text_focused),
        Span::raw(buffer.to_string()),
        Span::styled("▏", caret_style),
    ];
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Map a feedback severity onto a scheme-driven theme role so the message
/// inherits whatever the active palette ships for "highlight" / "warning"
/// / "error". The roles deliberately reuse existing slots so a single
/// `[schemes.<name>]` block already styles every severity — no new theme
/// fields required.
fn feedback_style(theme: &Theme, severity: FeedbackSeverity) -> Style {
    match severity {
        FeedbackSeverity::Confirmation => theme.text_focused,
        FeedbackSeverity::Warning => theme.text_selected,
        FeedbackSeverity::Error => theme.text_shortcut,
    }
}

fn render_feedback(
    frame: &mut Frame,
    area: Rect,
    msg: &str,
    severity: FeedbackSeverity,
    theme: &Theme,
) {
    // Leading single-space pad matches the status bar's left edge so the
    // text doesn't kiss the terminal border.
    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled(msg.to_string(), feedback_style(theme, severity)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
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
            ("?".into(), "toggle this help overlay".into()),
            ("q · Ctrl+C".into(), "quit".into()),
        ],
    };
    let mut sections = vec![global];

    // Per-widget sections, ordered by layout appearance. Stack cells
    // expand into a section for the stack itself (rotate prev/next)
    // plus one section per child — including the currently-hidden
    // tabs — so the help overlay reflects everything a user can reach
    // from that pane.
    let mut seen_owned: std::collections::HashSet<String> = std::collections::HashSet::new();
    let push_bindings =
        |sections: &mut Vec<help::Section>, title: String, widget: &dyn crate::widgets::Widget| {
            let bindings: Vec<(String, String)> = widget
                .keybindings()
                .into_iter()
                .map(|(k, d)| (k.to_string(), d.to_string()))
                .collect();
            if bindings.is_empty() {
                return;
            }
            sections.push(help::Section { title, bindings });
        };
    for cell in &layout.cells {
        let Some(id) = cell.render_target_id() else {
            continue;
        };
        if !seen_owned.insert(id.clone()) {
            continue;
        }
        let Some(widget) = manager.get(&id) else {
            continue;
        };

        let child_ids = widget.composite_children();
        if child_ids.is_empty() {
            push_bindings(&mut sections, widget.display_name().to_string(), widget);
            continue;
        }

        // Composite cell: render the stack's own bindings first (so the
        // tab-rotation keys aren't buried under the child sections),
        // then a section per child in tab order. Title the stack
        // section by its children to disambiguate when more than one
        // stack lives in the same layout.
        let mut stack_label_parts: Vec<String> = Vec::with_capacity(child_ids.len());
        for child_id in &child_ids {
            if let Some(child) = widget.composite_child(child_id) {
                stack_label_parts.push(child.display_name().to_string());
            }
        }
        let stack_title = if stack_label_parts.is_empty() {
            "Stack".to_string()
        } else {
            format!("Stack: {}", stack_label_parts.join(" + "))
        };
        push_bindings(&mut sections, stack_title, widget);
        for child_id in &child_ids {
            if let Some(child) = widget.composite_child(child_id) {
                push_bindings(&mut sections, child.display_name().to_string(), child);
            }
        }
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
    let block = apply_title_row(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme.border_style(focused)),
        focused,
        id,
        None,
        None,
        theme,
        area.width,
    );
    // `id` looks like `gallery` or `gallery@home`; strip the instance suffix
    // so the cargo-feature hint matches the on-disk feature name.
    let kind = id.split_once('@').map(|(k, _)| k).unwrap_or(id);
    let body = Paragraph::new(vec![
        Line::from(""),
        Line::from(format!("Widget '{id}' is not available in this build.")),
        Line::from(""),
        Line::from(format!(
            "Rebuild with --features widget-{kind} to enable it,"
        )),
        Line::from("or remove this cell from your layout in config.toml."),
    ])
    .alignment(Alignment::Center)
    .block(block);
    frame.render_widget(body, area);
}
