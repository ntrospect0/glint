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
        KeyCode::Esc => PageAction::Back,
        _ if on_next_button => {
            // Button-focus key handling: Up returns to last cell, Tab
            // wraps to first, BackTab to last cell, Enter/Space advance
            // (subject to gate).
            match key.code {
                KeyCode::Up | KeyCode::BackTab => {
                    app.focus = cell_count.saturating_sub(1);
                    app.lookup_offset = current_value_index(app);
                    PageAction::Stay
                }
                KeyCode::Down | KeyCode::Tab => {
                    app.focus = 0;
                    app.lookup_offset = current_value_index(app);
                    PageAction::Stay
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    if !any_cell_assigned(&app.state.assignments)
                        && matches!(app.state.layout, LayoutChoice::Preset { .. })
                    {
                        app.feedback = Some(
                            "Assign at least one widget before continuing (Tab/Space/Enter picks the highlighted option).".into(),
                        );
                        return PageAction::Stay;
                    }
                    PageAction::Advance
                }
                _ => PageAction::Stay,
            }
        }
        // Tab / Shift-Tab cycle between cells AND commit the focused
        // cell's highlighted option in the process. Without auto-commit
        // here, a user who navigated to "email" with ↑/↓ and pressed
        // Tab would leave the cell uncommitted — and the validation
        // gate below would then refuse to advance even though the user
        // believed they'd picked widgets. Stack is the documented
        // exception: highlighting Stack and Tabbing past it must NOT
        // change the cell, because a real stack needs the breakout.
        KeyCode::Tab => {
            commit_highlighted_unless_stack(app, &options);
            app.focus = (app.focus + 1) % focus_total;
            app.lookup_offset = current_value_index(app);
            PageAction::Stay
        }
        KeyCode::BackTab => {
            commit_highlighted_unless_stack(app, &options);
            app.focus = (app.focus + focus_total - 1) % focus_total;
            app.lookup_offset = current_value_index(app);
            PageAction::Stay
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.lookup_offset = app.lookup_offset.saturating_sub(1);
            PageAction::Stay
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.lookup_offset = (app.lookup_offset + 1).min(options.len() - 1);
            PageAction::Stay
        }
        // Space is the "pick this option" key, with one special role
        // for Stack: it opens the breakout to pick (or re-pick) the
        // stack's children. For everything else it commits the
        // highlighted option and advances to the next cell.
        KeyCode::Char(' ') => {
            if highlighted_value(&options, app) == Some(STACK_VALUE) {
                return PageAction::OpenAssignStack(app.focus);
            }
            commit_focused(app, &options);
            app.focus = (app.focus + 1) % focus_total;
            app.lookup_offset = current_value_index(app);
            PageAction::Stay
        }
        // Enter commits + advances like Space, but never opens the
        // stack breakout — Tab/Enter on the Stack option keep the
        // cell's existing children (if any) and move on, so a
        // re-visiting user isn't dropped back into the stack picker
        // every time they walk past the row.
        KeyCode::Enter => {
            commit_highlighted_unless_stack(app, &options);
            app.focus = (app.focus + 1) % focus_total;
            app.lookup_offset = current_value_index(app);
            PageAction::Stay
        }
        _ => PageAction::Stay,
    }
}

/// `true` when at least one cell has been assigned a real widget OR a
/// stack. Stack cells store children in `stack_children` and leave the
/// scalar `kind` empty, so a naive `all(|a| a.kind.is_empty())` check
/// would falsely report a stack-only setup as "nothing assigned" and
/// block the [Save & Next] gate.
fn any_cell_assigned(assignments: &[crate::wizard::state::CellAssignment]) -> bool {
    assignments
        .iter()
        .any(|a| !a.kind.is_empty() || a.is_stack())
}

/// Commit the focused cell's highlighted option, with one exception:
/// the Stack sentinel never commits here. Stack edits go through the
/// breakout (opened by Space) — Tab/Enter past the Stack row should
/// leave the cell's current state (single widget or existing stack)
/// untouched.
fn commit_highlighted_unless_stack(app: &mut WizardApp, options: &[(&'static str, &'static str)]) {
    if highlighted_value(options, app) == Some(STACK_VALUE) {
        return;
    }
    commit_focused(app, options);
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
    // Stack cells have an empty `kind` (children live in
    // `stack_children`); without this check `position` would fall
    // through to EMPTY_VALUE and the picker would highlight
    // "(empty — skip this cell)" for a cell that's actually a stack.
    if cell.is_stack() {
        return options
            .iter()
            .position(|(v, _)| *v == STACK_VALUE)
            .unwrap_or(0);
    }
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
    let inner = style::pad_inner(block.inner(area));
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
                // Three exclusive cases for `is_active` (the `(•)`
                // marker): a real stack lights up Stack; an empty
                // `kind` lights up "(empty)"; otherwise the
                // single-widget kind matches.
                let is_active = match *value {
                    STACK_VALUE => cell.is_stack(),
                    EMPTY_VALUE => !cell.is_stack() && cell.kind.is_empty(),
                    kind => !cell.is_stack() && kind == cell.kind.as_str(),
                };
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
                // When the focused cell already has a stack assigned,
                // inline its children in the Stack-option label so
                // the user can see what's there without leaving the
                // row, and remind them Space re-opens the picker.
                let display_label: String = if *value == STACK_VALUE && cell.is_stack() {
                    let children = cell
                        .stack_children
                        .iter()
                        .map(|c| c.widget_id())
                        .collect::<Vec<_>>()
                        .join(" + ");
                    format!("Stack ({children}) — Space to re-pick")
                } else {
                    (*label).to_string()
                };
                lines.push(Line::from(vec![
                    Span::raw("      "),
                    Span::styled(marker.to_string(), marker_style),
                    Span::raw(" "),
                    Span::styled(display_label, row_style),
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
                "    Enter advances to the per-widget pages (Tab/↑ to return to the cell list)."
                    .to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wizard::state::{CellAssignment, StackChild, WizardState};

    fn position_of(value: &str) -> usize {
        options()
            .iter()
            .position(|(v, _)| *v == value)
            .expect("option missing")
    }

    #[test]
    fn current_value_index_returns_stack_for_stack_cells() {
        // Regression: stack cells store children in `stack_children`
        // and leave `kind` empty, so a naive `position(kind)` match
        // would land on EMPTY_VALUE and the picker would show
        // "(empty — skip this cell)" highlighted for a configured
        // stack. The is_stack() short-circuit fixes it.
        let mut state = WizardState::default();
        state.assignments.push(CellAssignment {
            cell_index: 0,
            kind: String::new(),
            instance: "main".into(),
            stack_children: vec![
                StackChild {
                    kind: "clock".into(),
                    instance: "main".into(),
                },
                StackChild {
                    kind: "weather".into(),
                    instance: "main".into(),
                },
            ],
        });
        let mut app = WizardApp::new(state);
        app.focus = 0;
        assert_eq!(current_value_index(&app), position_of(STACK_VALUE));
    }

    #[test]
    fn current_value_index_returns_kind_for_single_widget_cells() {
        let mut state = WizardState::default();
        state.assignments.push(CellAssignment {
            cell_index: 0,
            kind: "stocks".into(),
            instance: "main".into(),
            stack_children: Vec::new(),
        });
        let mut app = WizardApp::new(state);
        app.focus = 0;
        assert_eq!(current_value_index(&app), position_of("stocks"));
    }

    fn empty_cells(n: usize) -> Vec<CellAssignment> {
        (0..n)
            .map(|i| CellAssignment {
                cell_index: i,
                kind: String::new(),
                instance: "main".into(),
                stack_children: Vec::new(),
            })
            .collect()
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    fn tab_app_with_options(highlight_value: &'static str) -> WizardApp {
        let mut state = WizardState::default();
        state.assignments = empty_cells(3);
        let mut app = WizardApp::new(state);
        app.focus = 0;
        // Position the cursor on the requested option without touching
        // commit semantics — mirrors what ↑/↓ navigation would do.
        app.lookup_offset = position_of(highlight_value);
        app
    }

    #[test]
    fn tab_commits_highlighted_non_stack_option_and_advances() {
        // Regression for "I picked email, news, calendar but the wizard
        // sent me straight to Confirm." Without an auto-commit on Tab,
        // a user who highlighted "email" with ↑/↓ and Tab'd to the
        // next cell would leave the cell uncommitted.
        let _ = crate::widgets::registry::WIDGETS;
        let mut app = tab_app_with_options("calendar");
        let r = handle_key(press(KeyCode::Tab), &mut app);
        assert_eq!(r, PageAction::Stay);
        assert_eq!(app.focus, 1, "Tab should advance to the next cell");
        assert_eq!(
            app.state.assignments[0].kind, "calendar",
            "Tab should commit the highlighted non-stack option"
        );
    }

    #[test]
    fn tab_on_stack_option_does_not_commit_but_advances() {
        // Stack edits go through the breakout (opened by Space). Tab on
        // a Stack row must NOT clobber the cell's existing state — a
        // user re-visiting a configured stack cell should be able to
        // Tab past without losing the stack.
        let mut state = WizardState::default();
        state.assignments = empty_cells(2);
        state.assignments[0].kind = "stocks".into();
        let mut app = WizardApp::new(state);
        app.focus = 0;
        app.lookup_offset = position_of(STACK_VALUE);
        let r = handle_key(press(KeyCode::Tab), &mut app);
        assert_eq!(r, PageAction::Stay);
        assert_eq!(app.focus, 1, "Tab should still advance focus");
        assert_eq!(
            app.state.assignments[0].kind, "stocks",
            "highlighting Stack and tabbing past must not clobber the cell"
        );
    }

    #[test]
    fn enter_on_stack_option_advances_without_opening_breakout() {
        // Same rule as Tab for Enter — Space is the dedicated key for
        // opening the stack-config breakout. Returning Advance/Stay
        // (not OpenAssignStack) keeps Enter as a "next cell" key.
        let mut state = WizardState::default();
        state.assignments = empty_cells(2);
        let mut app = WizardApp::new(state);
        app.focus = 0;
        app.lookup_offset = position_of(STACK_VALUE);
        let r = handle_key(press(KeyCode::Enter), &mut app);
        assert_eq!(r, PageAction::Stay);
        assert_eq!(app.focus, 1);
    }

    #[test]
    fn space_on_stack_option_opens_stack_breakout() {
        // The one key that DOES open the stack-config sub-page. Mirrors
        // the assign-page comment: Space is the dedicated stack key.
        let mut state = WizardState::default();
        state.assignments = empty_cells(2);
        let mut app = WizardApp::new(state);
        app.focus = 0;
        app.lookup_offset = position_of(STACK_VALUE);
        let r = handle_key(press(KeyCode::Char(' ')), &mut app);
        assert_eq!(r, PageAction::OpenAssignStack(0));
    }

    #[test]
    fn any_cell_assigned_counts_stack_cells() {
        // Stack-only setups have all-empty `kind` strings but a
        // populated `stack_children`. Validation must treat that as
        // "yes, the user assigned something" so Save & Next isn't
        // wrongly blocked.
        let mut assignments = empty_cells(2);
        assignments[0].stack_children = vec![
            StackChild {
                kind: "clock".into(),
                instance: "main".into(),
            },
            StackChild {
                kind: "weather".into(),
                instance: "main".into(),
            },
        ];
        assert!(any_cell_assigned(&assignments));
    }

    #[test]
    fn current_value_index_returns_empty_for_unassigned_cells() {
        let mut state = WizardState::default();
        state.assignments.push(CellAssignment {
            cell_index: 0,
            kind: String::new(),
            instance: "main".into(),
            stack_children: Vec::new(),
        });
        let mut app = WizardApp::new(state);
        app.focus = 0;
        assert_eq!(current_value_index(&app), position_of(EMPTY_VALUE));
    }
}
