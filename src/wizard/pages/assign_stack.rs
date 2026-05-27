// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Wizard sub-page for composing a stack cell. See
//! `docs/stack-spec.md` §6.
//!
//! Pushed onto the wizard's history stack when the user picks "Stack"
//! as a cell kind on the Assign page. Walks them through 3 slot
//! pickers — each is the same list of widget kinds as the Assign
//! page, plus a trailing "(skip)" entry. A trailing [ Save & Next ]
//! button commits the choices and pops back to Assign.
//!
//! Empty slots are dropped at commit time per spec §1 ("no gaps"),
//! and a single-element stack degrades to a regular single-widget
//! cell so the user can't accidentally produce a tab-strip-of-one.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use super::PageAction;
use crate::widgets::registry::WIDGETS;
use crate::wizard::{
    app::WizardApp,
    state::StackChild,
    style,
};

const SLOTS: usize = 3;
const SKIP_VALUE: &str = "";

/// Snapshot of options for the picker lists. Owned vec because the
/// "skip" sentinel is dynamic and we don't want to lift it into a
/// `'static` slot. Cheap since each tuple is two `&'static str`.
fn options() -> Vec<(&'static str, &'static str)> {
    let mut out: Vec<(&'static str, &'static str)> =
        WIDGETS.iter().map(|d| (d.kind, d.kind)).collect();
    out.push((SKIP_VALUE, "(skip — leave this slot empty)"));
    out
}

/// Per-slot cursor index into the options list. We piggyback on
/// `app.text_buffer` to store the slot lookup state — encoded as
/// "{slot0}:{slot1}:{slot2}:{focus_slot}". This is the same trick
/// other wizard pages use to avoid widening WizardApp every time a
/// page needs scratch state.
fn parse_state(buf: &str, opts_len: usize) -> [usize; SLOTS + 1] {
    // Format: "slot0:slot1:slot2:focus" (defaults to 0/0/0/0).
    let mut parts = [0usize; SLOTS + 1];
    for (i, token) in buf.split(':').enumerate().take(SLOTS + 1) {
        if let Ok(v) = token.parse::<usize>() {
            parts[i] = v.min(opts_len.saturating_sub(1));
        }
    }
    parts
}

fn encode_state(slots: &[usize; SLOTS], focus: usize) -> String {
    format!("{}:{}:{}:{}", slots[0], slots[1], slots[2], focus)
}

pub fn on_enter(app: &mut WizardApp, cell_index: usize) {
    let opts = options();
    let mut slots: [usize; SLOTS] = [opts.len() - 1, opts.len() - 1, opts.len() - 1];
    // Seed from existing stack children (returning user) or from the
    // single-widget kind already on the cell.
    if let Some(cell) = app.state.assignments.get(cell_index) {
        if !cell.stack_children.is_empty() {
            for (i, child) in cell.stack_children.iter().take(SLOTS).enumerate() {
                if let Some(pos) =
                    opts.iter().position(|(v, _)| *v == child.kind.as_str())
                {
                    slots[i] = pos;
                }
            }
        } else if !cell.kind.is_empty() {
            if let Some(pos) = opts.iter().position(|(v, _)| *v == cell.kind.as_str())
            {
                slots[0] = pos;
            }
        }
    }
    app.text_buffer = encode_state(&slots, 0);
    app.focus = 0;
}

pub fn handle_key(key: KeyEvent, app: &mut WizardApp, cell_index: usize) -> PageAction {
    let opts = options();
    let opts_len = opts.len();
    let state = parse_state(&app.text_buffer, opts_len);
    let mut slots: [usize; SLOTS] = [state[0], state[1], state[2]];
    let mut focus = state[3];
    let save_index = SLOTS; // Save button is the last focus slot.
    let focus_total = SLOTS + 1;

    match key.code {
        KeyCode::Esc => return PageAction::Back,
        KeyCode::Tab => {
            focus = (focus + 1) % focus_total;
        }
        KeyCode::BackTab => {
            focus = (focus + focus_total - 1) % focus_total;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if focus < SLOTS {
                slots[focus] = slots[focus].saturating_sub(1);
            } else {
                // From the Save button, Up jumps to the last slot.
                focus = SLOTS - 1;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if focus < SLOTS {
                slots[focus] = (slots[focus] + 1).min(opts_len.saturating_sub(1));
            } else {
                // From the Save button, Down wraps to the first slot.
                focus = 0;
            }
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            if focus == save_index {
                commit_stack(app, cell_index, &slots);
                return PageAction::Back;
            }
            // Enter on a slot row advances to the next focus slot.
            focus = (focus + 1) % focus_total;
        }
        _ => {}
    }

    app.text_buffer = encode_state(&slots, focus);
    PageAction::Stay
}

fn commit_stack(app: &mut WizardApp, cell_index: usize, slots: &[usize; SLOTS]) {
    let opts = options();
    let mut children: Vec<StackChild> = Vec::new();
    for &slot in slots {
        let Some((value, _)) = opts.get(slot) else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        children.push(StackChild {
            kind: (*value).to_string(),
            instance: "main".into(),
        });
    }
    let Some(cell) = app.state.assignments.get_mut(cell_index) else {
        return;
    };
    if children.len() >= 2 {
        // Real stack — clear scalar fields, set children.
        cell.kind = String::new();
        cell.instance = "main".into();
        cell.stack_children = children;
    } else if children.len() == 1 {
        // Degrade to a normal single-widget cell (per spec §1).
        let only = children.into_iter().next().unwrap();
        cell.kind = only.kind;
        cell.instance = only.instance;
        cell.stack_children.clear();
    } else {
        // All slots skipped — leave the cell as-is rather than
        // clobbering an existing assignment with nothing.
    }
}

pub fn render(frame: &mut Frame, area: Rect, app: &WizardApp, cell_index: usize) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Configure stack — cell {cell_index} "));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let opts = options();
    let state = parse_state(&app.text_buffer, opts.len());
    let slots: [usize; SLOTS] = [state[0], state[1], state[2]];
    let focus = state[3];

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "Pick up to three widgets for this stack. Empty slots are dropped.",
        style::section_header(),
    )));
    lines.push(Line::from(Span::styled(
        "  Tab cycles slots · ↑/↓ pick widget · Enter advances · Save commits + returns to Assign.",
        style::blurb(),
    )));
    lines.push(Line::from(""));

    for slot in 0..SLOTS {
        let is_focused = focus == slot;
        let label_style = if is_focused {
            style::label_focused()
        } else {
            style::label()
        };
        let header = if slot == 0 {
            "Slot 1 (default visible)"
        } else if slot == 1 {
            "Slot 2"
        } else {
            "Slot 3"
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{}. ", slot + 1), label_style),
            Span::styled(header.to_string(), label_style),
        ]));

        let cur = slots[slot];
        for (i, (_value, label)) in opts.iter().enumerate() {
            let is_active = i == cur;
            let is_highlighted = is_focused && is_active;
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

    let on_button = focus == SLOTS;
    let button_style = if on_button {
        style::page_button_focused()
    } else {
        style::page_button_idle()
    };
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("[ Save & Return ]", button_style),
    ]));
    if on_button {
        lines.push(Line::from(Span::styled(
            "    Enter commits this stack + pops back. Esc cancels without saving."
                .to_string(),
            style::help_text(),
        )));
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}
