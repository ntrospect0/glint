// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Cell → widget assignment. Each cell is a Choice-style picker:
//!
//! - Tab / Shift-Tab moves between cells.
//! - ↑ / ↓ navigates within the focused cell's option list.
//! - Space commits the highlighted option as that cell's widget kind.
//! - Enter advances to the per-widget pages (required gate: at least
//!   one cell must be filled in when a preset layout is chosen).
//!
//! Non-focused cells collapse to a single "Cell N — <assigned kind>" row
//! so the page scales to layouts with many cells. The layout-preview
//! sidebar paints the highlighted cell so the user always knows which
//! pane they're filling in.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use super::PageAction;
use crate::widgets::registry::WIDGETS;
use crate::wizard::{app::WizardApp, state::LayoutChoice, style};

/// The sentinel value used in `CellAssignment.kind` when a cell is left
/// unassigned. Stored as an empty string in state; rendered as
/// "(empty)" in the picker.
const EMPTY_VALUE: &str = "";

/// Called by `pages::on_enter` so the option cursor lands on the focused
/// cell's currently-assigned widget when the page is first opened.
pub fn on_enter(app: &mut WizardApp) {
    app.lookup_offset = current_value_index(app);
}

pub fn handle_key(key: KeyEvent, app: &mut WizardApp) -> PageAction {
    let cell_count = app.state.assignments.len();
    if cell_count == 0 {
        // Keep-existing layout produced zero assignments; nothing to
        // pick. Enter advances directly (there's no button to land on
        // because there's no field to render either).
        return match key.code {
            KeyCode::Enter | KeyCode::Char(' ') => PageAction::Advance,
            KeyCode::Esc => PageAction::Back,
            _ => PageAction::Stay,
        };
    }
    let options = options();
    // One focus slot per cell + 1 trailing [ Save & Next ] button.
    let focus_total = cell_count + 1;
    let on_next_button = app.focus == cell_count;

    match key.code {
        KeyCode::Tab => {
            app.focus = (app.focus + 1) % focus_total;
            app.lookup_offset = current_value_index(app);
            PageAction::Stay
        }
        KeyCode::BackTab => {
            app.focus = (app.focus + focus_total - 1) % focus_total;
            app.lookup_offset = current_value_index(app);
            PageAction::Stay
        }
        KeyCode::Esc => PageAction::Back,
        _ if on_next_button => {
            // Button-focus key handling: Up returns to last cell, Down
            // wraps to first, Enter advances (subject to gate).
            match key.code {
                KeyCode::Up => {
                    app.focus = cell_count.saturating_sub(1);
                    app.lookup_offset = current_value_index(app);
                    PageAction::Stay
                }
                KeyCode::Down => {
                    app.focus = 0;
                    app.lookup_offset = current_value_index(app);
                    PageAction::Stay
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    if app
                        .state
                        .assignments
                        .iter()
                        .all(|a| a.kind.is_empty())
                        && matches!(app.state.layout, LayoutChoice::Preset { .. })
                    {
                        app.feedback = Some(
                            "Assign at least one widget before continuing (Space picks; Tab/↑ returns to the cell list).".into(),
                        );
                        return PageAction::Stay;
                    }
                    PageAction::Advance
                }
                _ => PageAction::Stay,
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.lookup_offset = app.lookup_offset.saturating_sub(1);
            PageAction::Stay
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.lookup_offset = (app.lookup_offset + 1).min(options.len() - 1);
            PageAction::Stay
        }
        KeyCode::Char(' ') => {
            // "Stack" doesn't commit a kind; it opens the AssignStack
            // sub-page so the user can pick 2-3 widgets for the cell.
            if highlighted_value(&options, app) == Some(STACK_VALUE) {
                return PageAction::OpenAssignStack(app.focus);
            }
            commit_focused(app, &options);
            PageAction::Stay
        }
        KeyCode::Enter => {
            if highlighted_value(&options, app) == Some(STACK_VALUE) {
                return PageAction::OpenAssignStack(app.focus);
            }
            // Enter inside a cell row commits the highlighted widget
            // kind (so the user's cursor work isn't dropped) and moves
            // focus forward. Page advance lives on the trailing
            // [ Save & Next ] button.
            commit_focused(app, &options);
            app.focus = (app.focus + 1) % focus_total;
            app.lookup_offset = current_value_index(app);
            PageAction::Stay
        }
        _ => PageAction::Stay,
    }
}

fn commit_focused(app: &mut WizardApp, options: &[(&'static str, &'static str)]) {
    let Some((value, _)) = options.get(app.lookup_offset) else {
        return;
    };
    if *value == STACK_VALUE {
        // Handled by the caller via PageAction::OpenAssignStack.
        return;
    }
    let Some(cell) = app.state.assignments.get_mut(app.focus) else {
        return;
    };
    cell.kind = (*value).to_string();
    // Picking a non-stack kind clears any existing stack_children so
    // the cell unambiguously becomes a single-widget cell.
    cell.stack_children.clear();
}

fn highlighted_value(
    options: &[(&'static str, &'static str)],
    app: &WizardApp,
) -> Option<&'static str> {
    options.get(app.lookup_offset).map(|(v, _)| *v)
}

fn current_value_index(app: &WizardApp) -> usize {
    let options = options();
    let Some(cell) = app.state.assignments.get(app.focus) else {
        return 0;
    };
    options
        .iter()
        .position(|(v, _)| *v == cell.kind.as_str())
        .unwrap_or(0)
}

/// Sentinel value for the "stack" option — picking this on the Assign
/// page kicks off the AssignStack sub-page rather than committing a
/// kind directly. Distinct from `EMPTY_VALUE` because empty means
/// "skip this cell" and stack means "compose multiple widgets here."
const STACK_VALUE: &str = "__stack__";

/// Available widget kinds (from the registry) plus a "Stack" entry
/// and a trailing "(empty)" entry. Returns owned slice each call;
/// cheap since both halves of each tuple are `&'static str`.
fn options() -> Vec<(&'static str, &'static str)> {
    let mut out: Vec<(&'static str, &'static str)> =
        WIDGETS.iter().map(|d| (d.kind, d.kind)).collect();
    out.push((STACK_VALUE, "Stack — pick up to 3 widgets for this cell"));
    out.push((EMPTY_VALUE, "(empty — skip this cell)"));
    out
}

pub fn render(frame: &mut Frame, area: Rect, app: &WizardApp) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Assign widgets to cells ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Three-column split: form on the left, a 2-cell gap, preview on the
    // right (matches the per-widget page layout).
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(55),
            Constraint::Length(2),
            Constraint::Min(20),
        ])
        .split(inner);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3)])
        .split(cols[0]);
    render_cell_list(frame, rows[0], app);
    render_help(frame, rows[1]);

    let active = if app.state.assignments.is_empty() {
        None
    } else {
        Some(app.focus.min(app.state.assignments.len() - 1))
    };
    super::preview::render(
        frame,
        cols[2],
        &app.state.layout,
        &app.state.assignments,
        active,
    );
}

fn render_cell_list(frame: &mut Frame, area: Rect, app: &WizardApp) {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "Pick one widget per cell.",
        style::section_header(),
    )));
    lines.push(Line::from(Span::styled(
        "  Tab moves between cells; ↑/↓ navigates options; Space picks.",
        style::blurb(),
    )));
    lines.push(Line::from(""));

    if app.state.assignments.is_empty() {
        lines.push(Line::from(Span::styled(
            "No cells to assign — layout will be left untouched.",
            style::blurb(),
        )));
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
        return;
    }

    let options = options();
    for (i, cell) in app.state.assignments.iter().enumerate() {
        let focused = i == app.focus;
        let summary = if cell.is_stack() {
            format!(
                "Stack: {}",
                cell.stack_children
                    .iter()
                    .map(|c| c.widget_id())
                    .collect::<Vec<_>>()
                    .join(" + ")
            )
        } else if cell.kind.is_empty() {
            "(empty)".to_string()
        } else {
            cell.widget_id()
        };
        let label_style = if focused {
            style::label_focused()
        } else {
            style::label()
        };
        let summary_style = if focused {
            style::value_focused()
        } else {
            style::value_idle()
        };
        let marker = if focused { "▶ " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(marker.to_string(), label_style),
            Span::styled(format!("{}. Cell {}", i + 1, i + 1), label_style),
            Span::raw("   "),
            Span::styled(summary, summary_style),
        ]));

        if focused {
            // Expand the focused cell into a vertical option list. The
            // ▶ cursor inside the list moves on Up/Down; Space picks.
            let highlight = app.lookup_offset.min(options.len() - 1);
            for (j, (value, label)) in options.iter().enumerate() {
                let is_active = *value == cell.kind.as_str();
                let is_highlighted = j == highlight;
                let marker = if is_active { "(•)" } else { "( )" };
                let marker_style = if is_active {
                    style::marker_active()
                } else {
                    style::marker_idle()
                };
                let row_style = if is_highlighted {
                    style::option_selected()
                } else {
                    style::option_idle()
                };
                lines.push(Line::from(vec![
                    Span::raw("      "),
                    Span::styled(marker.to_string(), marker_style),
                    Span::raw(" "),
                    Span::styled(label.to_string(), row_style),
                ]));
            }
            lines.push(Line::from(""));
        }
    }
    // Trailing [ Save & Next ] button.
    let cell_count = app.state.assignments.len();
    if cell_count > 0 {
        let on_button = app.focus == cell_count;
        let button_style = if on_button {
            style::page_button_focused()
        } else {
            style::page_button_idle()
        };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("[ Save & Next ]", button_style),
        ]));
        if on_button {
            lines.push(Line::from(Span::styled(
                "    Enter advances to the per-widget pages (Tab/↑ to return to the cell list).".to_string(),
                style::help_text(),
            )));
        }
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_help(frame: &mut Frame, area: Rect) {
    let para = Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            "Tab/⇧Tab cycle cells + [Save & Next] · ↑/↓ navigate options · Space picks · Enter advances focus · Esc back",
            style::help_text(),
        )),
    ])
    .wrap(Wrap { trim: true });
    frame.render_widget(para, area);
}
