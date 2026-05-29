// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Generic per-widget setup page. Drives entirely from the widget's
//! `WizardDescriptor` so adding a new widget to the wizard is a matter of
//! filling in fields — no per-widget code in the wizard itself.
//!
//! Widgets that opt out of the form (returning `defer_to_toml_descriptor()`
//! → `fields = []`) get an "advanced — edit TOML" message instead of a form.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use super::PageAction;
use crate::widgets::registry;
use crate::wizard::{
    app::WizardApp,
    descriptor::{ChoiceOption, WizardDescriptor, WizardField, WizardFieldKind, WizardValue},
    style,
};

/// Called once when the widget page becomes the active page. Seeds the
/// TextList buffer from the field's current value (if applicable) and
/// places the option-list cursor on the field's current value so users
/// land on a sensible row instead of row 0.
/// Resolve the kind + widget_id this page is configuring. `child_idx` is
/// `None` for a regular single-widget cell (Page::Widget(i)) and
/// `Some(k)` for the k-th child of a stack cell (Page::StackChild).
/// Returns None if the assignment / child has been removed under us, or
/// if the resolved kind is empty (which is the sentinel for "stack cell
/// with no scalar widget").
fn resolve_target(
    app: &WizardApp,
    cell_idx: usize,
    child_idx: Option<usize>,
) -> Option<(String, String)> {
    let assignment = app.state.assignments.get(cell_idx)?;
    let (kind, widget_id) = match child_idx {
        None => {
            if assignment.kind.is_empty() {
                return None;
            }
            (assignment.kind.clone(), assignment.widget_id())
        }
        Some(k) => {
            let child = assignment.stack_children.get(k)?;
            if child.kind.is_empty() {
                return None;
            }
            (child.kind.clone(), child.widget_id())
        }
    };
    Some((kind, widget_id))
}

pub fn on_enter(app: &mut WizardApp, cell_idx: usize, child_idx: Option<usize>) {
    let Some((kind, widget_id)) = resolve_target(app, cell_idx, child_idx) else {
        return;
    };
    let Some(desc) = registry::find(&kind) else {
        return;
    };
    let wd = (desc.wizard)();
    app.text_buffer.clear();
    populate_textlist_buffer(app, &widget_id, &wd);
    app.lookup_offset = current_value_index(app, &widget_id, &wd);
}

pub fn handle_key(
    key: KeyEvent,
    app: &mut WizardApp,
    cell_idx: usize,
    child_idx: Option<usize>,
) -> PageAction {
    let Some((kind, widget_id)) = resolve_target(app, cell_idx, child_idx) else {
        return PageAction::Advance;
    };
    let Some(desc) = registry::find(&kind) else {
        return PageAction::Advance;
    };
    let wd = (desc.wizard)();

    if wd.fields.is_empty() {
        // Defer-to-TOML page — only Enter/Esc are meaningful.
        return match key.code {
            KeyCode::Enter | KeyCode::Char(' ') => PageAction::Advance,
            KeyCode::Esc => PageAction::Back,
            _ => PageAction::Stay,
        };
    }

    let field_count = wd.fields.len();
    // `widget_id` was resolved up top from the (cell, optional child)
    // target so the rest of the body is target-agnostic.
    // Trailing focus slot is the [ Save & Next ] button. Tab past the
    // last field lands on it; Enter from inside a field also moves
    // here (per `move_focus`); Enter on the button advances the page.
    let focus_total = wd.focus_total();
    let on_next_button = app.focus == field_count;

    // Field-navigation keys behave identically regardless of focused field
    // kind. They commit any in-flight TextList edits, snap the cursor to
    // the next field's current value (so option fields don't land on row
    // 0 unexpectedly), and re-populate the text buffer when the new
    // focus is a TextList.
    match key.code {
        KeyCode::Tab => {
            move_focus(app, &widget_id, &wd, 1, focus_total);
            return PageAction::Stay;
        }
        KeyCode::BackTab => {
            move_focus(app, &widget_id, &wd, -1, focus_total);
            return PageAction::Stay;
        }
        KeyCode::Esc => {
            commit_inflight_edits(app, &widget_id, &wd);
            return PageAction::Back;
        }
        _ => {}
    }

    // The [ Save & Next ] button consumes only ↑/↓/Enter — it's a
    // single action row, not a field with content. Up moves back into
    // the last field; Enter triggers the page advance.
    if on_next_button {
        match key.code {
            KeyCode::Up | KeyCode::BackTab => {
                move_focus(app, &widget_id, &wd, -1, focus_total);
                return PageAction::Stay;
            }
            KeyCode::Down | KeyCode::Tab => {
                move_focus(app, &widget_id, &wd, 1, focus_total);
                return PageAction::Stay;
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                return advance_or_gate(app, &widget_id, &wd);
            }
            _ => return PageAction::Stay,
        }
    }

    // Lookup fields take over Up/Down/Char/Backspace/Enter to drive the
    // type-to-filter dropdown. Non-Lookup fields keep the legacy behavior
    // (Up/Down navigates between fields, Char types into Text/etc).
    let is_lookup = matches!(
        wd.fields.get(app.focus).map(|f| &f.kind),
        Some(WizardFieldKind::Lookup { .. })
    );

    if is_lookup {
        return handle_lookup_key(key, app, &widget_id, &wd);
    }

    // OAuth fields trigger the auth flow on Space; ↑/↓ still navigate
    // between fields. The actual run happens up in the app loop via
    // PageAction::RunAuth so the TUI can be suspended for the browser
    // handshake.
    if let Some(WizardFieldKind::OAuth { provider }) = wd.fields.get(app.focus).map(|f| &f.kind) {
        match key.code {
            KeyCode::Char(' ') => {
                return PageAction::RunAuth((*provider).to_string());
            }
            // Up/Down handled by the generic block below.
            _ => {}
        }
    }

    // Choice / MultiChoice / RemoteMultiChoice / Bool consume Up/Down/
    // Space to navigate their option list. Tab / Shift-Tab moves between
    // fields (handled above). Bool is rendered as a two-row "yes / no"
    // list for consistency with Choice — the user picks with Space
    // rather than toggling.
    let focused_is_options = matches!(
        wd.fields.get(app.focus).map(|f| &f.kind),
        Some(WizardFieldKind::Choice { .. })
            | Some(WizardFieldKind::MultiChoice { .. })
            | Some(WizardFieldKind::RemoteMultiChoice { .. })
            | Some(WizardFieldKind::Bool { .. })
    );
    if focused_is_options {
        return handle_options_key(key, app, &widget_id, &wd);
    }

    match key.code {
        // Up/Down navigates between fields for Text / Number / Bool /
        // TextList / Path — fields without an inner option list. Routed
        // through move_focus so in-flight TextList/Lookup edits commit
        // and the text buffer re-seeds from the new field's value.
        KeyCode::Down => {
            move_focus(app, &widget_id, &wd, 1, field_count);
            PageAction::Stay
        }
        KeyCode::Up => {
            move_focus(app, &widget_id, &wd, -1, field_count);
            PageAction::Stay
        }
        // Number gets ±step via ←/→. Vim-style h/l aliases only fire on
        // Number fields — for Text / TextList / Path they'd otherwise
        // eat the literal letters the user is trying to type (a Gallery
        // path like `/Users/you/...` would lose every `h` and `l`).
        // Bool already lives in `handle_options_key` above.
        KeyCode::Left => {
            adjust_focused(app, &widget_id, &wd, false);
            PageAction::Stay
        }
        KeyCode::Right => {
            adjust_focused(app, &widget_id, &wd, true);
            PageAction::Stay
        }
        KeyCode::Char('h') | KeyCode::Char('l')
            if matches!(
                wd.fields.get(app.focus).map(|f| &f.kind),
                Some(WizardFieldKind::Number { .. })
            ) =>
        {
            adjust_focused(app, &widget_id, &wd, key.code == KeyCode::Char('l'));
            PageAction::Stay
        }
        KeyCode::Char(' ') => {
            // Space toggles Bool fields; falls through to type-into-text
            // for Text / TextList / Path so a real space character still
            // works in free-form fields.
            if let Some(field) = wd.fields.get(app.focus) {
                if matches!(field.kind, WizardFieldKind::Bool { .. }) {
                    adjust_focused(app, &widget_id, &wd, true);
                    return PageAction::Stay;
                }
            }
            type_into_focused(app, &widget_id, &wd, ' ');
            PageAction::Stay
        }
        KeyCode::Char(c) => {
            type_into_focused(app, &widget_id, &wd, c);
            PageAction::Stay
        }
        KeyCode::Backspace => {
            backspace_focused(app, &widget_id, &wd);
            PageAction::Stay
        }
        KeyCode::Enter => {
            // Enter behaves like Tab inside any field — moves to the
            // next focus slot (commits in-flight edits via move_focus).
            // The page advance is reserved for Enter on the
            // [ Save & Next ] button row, handled above.
            move_focus(app, &widget_id, &wd, 1, focus_total);
            PageAction::Stay
        }
        _ => PageAction::Stay,
    }
}

/// Key handling for Choice / MultiChoice. Up/Down navigates the option
/// rows; Space picks (Choice) or toggles (MultiChoice). Enter advances
/// the page (subject to the required-fields gate). The selected row
/// index is stored in `app.lookup_offset` (shared with the Lookup
/// dropdown handler).
fn handle_options_key(
    key: KeyEvent,
    app: &mut WizardApp,
    widget_id: &str,
    wd: &WizardDescriptor,
) -> PageAction {
    let Some(field) = wd.fields.get(app.focus) else {
        return PageAction::Stay;
    };
    let opts_len = option_row_count_for(app, &field.kind);
    match key.code {
        KeyCode::Down => {
            if opts_len > 0 {
                app.lookup_offset = (app.lookup_offset + 1).min(opts_len - 1);
            }
            PageAction::Stay
        }
        KeyCode::Up => {
            app.lookup_offset = app.lookup_offset.saturating_sub(1);
            PageAction::Stay
        }
        KeyCode::Char(' ') => {
            commit_option_selection(app, widget_id, field);
            PageAction::Stay
        }
        KeyCode::Enter => {
            // Enter on a Choice / MultiChoice / Bool field acts like
            // Tab — it commits whatever option is highlighted (so the
            // user's last cursor position isn't silently discarded) and
            // moves focus to the next field / the [ Save & Next ]
            // button. To advance the page, the user lands on the
            // button (the trailing focus slot) and presses Enter there.
            commit_option_selection(app, widget_id, field);
            move_focus(app, widget_id, wd, 1, wd.focus_total());
            PageAction::Stay
        }
        _ => PageAction::Stay,
    }
}

/// How many selectable rows the field's list has. Bool is rendered as a
/// 2-row yes/no list; Choice and MultiChoice count their options
/// directly; RemoteMultiChoice falls back to its `defaults` count when
/// the source hasn't been fetched yet so the keyboard cursor stays
/// usable even before authorization; everything else is 0.
fn option_row_count_for(app: &WizardApp, kind: &WizardFieldKind) -> usize {
    match kind {
        WizardFieldKind::Bool { .. } => 2,
        WizardFieldKind::Choice { options, .. } | WizardFieldKind::MultiChoice { options, .. } => {
            options.len()
        }
        WizardFieldKind::RemoteMultiChoice { source, defaults } => app
            .remote_options
            .get(*source)
            .map(|opts| opts.len())
            .unwrap_or(defaults.len()),
        _ => 0,
    }
}

fn commit_option_selection(app: &mut WizardApp, widget_id: &str, field: &WizardField) {
    match &field.kind {
        WizardFieldKind::Bool { .. } => {
            // Row 0 = yes, row 1 = no.
            let value = app.lookup_offset == 0;
            app.state
                .widget_set(widget_id, field.key, WizardValue::Bool(value));
        }
        WizardFieldKind::Choice { options, .. } => {
            if let Some(opt) = options.get(app.lookup_offset) {
                app.state.widget_set(
                    widget_id,
                    field.key,
                    WizardValue::Choice(opt.value.to_string()),
                );
            }
        }
        WizardFieldKind::MultiChoice { options, .. } => {
            let Some(opt) = options.get(app.lookup_offset) else {
                return;
            };
            let mut current = match current_value(app, widget_id, field) {
                WizardValue::MultiChoice(v) => v,
                _ => Vec::new(),
            };
            if let Some(pos) = current.iter().position(|s| s == opt.value) {
                current.remove(pos);
            } else {
                current.push(opt.value.to_string());
            }
            app.state
                .widget_set(widget_id, field.key, WizardValue::MultiChoice(current));
        }
        WizardFieldKind::RemoteMultiChoice { source, defaults } => {
            // Mirror MultiChoice but pull the option list from the
            // session's remote cache. When the cache is empty we toggle
            // against the descriptor's `defaults` list, so the picker
            // still functions before authorization (e.g. the user can
            // pre-select INBOX before granting OAuth).
            let resolved: Vec<(String, String)> = match app.remote_options.get(*source) {
                Some(opts) => opts.clone(),
                None => defaults
                    .iter()
                    .map(|s| ((*s).to_string(), (*s).to_string()))
                    .collect(),
            };
            let Some((value, _)) = resolved.get(app.lookup_offset) else {
                return;
            };
            let value = value.clone();
            let mut current = match current_value(app, widget_id, field) {
                WizardValue::MultiChoice(v) => v,
                _ => Vec::new(),
            };
            if let Some(pos) = current.iter().position(|s| *s == value) {
                current.remove(pos);
            } else {
                current.push(value);
            }
            app.state
                .widget_set(widget_id, field.key, WizardValue::MultiChoice(current));
        }
        _ => {}
    }
}

/// Lookup dropdown key handling. Always: ↑/↓/PgUp/PgDn navigate, Space
/// commits the highlighted row, Tab/Shift-Tab leaves the field, Enter
/// advances the page. Other characters append to the filter; Backspace
/// trims it. This separation (Space commits, Enter advances) keeps the
/// semantics identical across Choice / MultiChoice / Lookup.
fn handle_lookup_key(
    key: KeyEvent,
    app: &mut WizardApp,
    widget_id: &str,
    wd: &WizardDescriptor,
) -> PageAction {
    let Some(field) = wd.fields.get(app.focus) else {
        return PageAction::Stay;
    };
    let opts = filtered_lookup_options(field, &app.text_buffer);

    match key.code {
        KeyCode::Down => {
            if !opts.is_empty() {
                app.lookup_offset = (app.lookup_offset + 1).min(opts.len() - 1);
            }
            PageAction::Stay
        }
        KeyCode::Up => {
            app.lookup_offset = app.lookup_offset.saturating_sub(1);
            PageAction::Stay
        }
        KeyCode::PageDown => {
            if !opts.is_empty() {
                app.lookup_offset = (app.lookup_offset + 10).min(opts.len() - 1);
            }
            PageAction::Stay
        }
        KeyCode::PageUp => {
            app.lookup_offset = app.lookup_offset.saturating_sub(10);
            PageAction::Stay
        }
        KeyCode::Backspace => {
            app.text_buffer.pop();
            app.lookup_offset = 0;
            PageAction::Stay
        }
        KeyCode::Char(' ') => {
            // Space commits the highlighted option. Clear any in-flight
            // filter (the value is set; further navigation should be over
            // the full list, not the filtered subset) and reposition the
            // cursor to land on the just-committed value in that full list.
            if let Some((value, _)) = opts.get(app.lookup_offset) {
                let value = value.to_string();
                app.state
                    .widget_set(widget_id, field.key, WizardValue::Choice(value.clone()));
                app.text_buffer.clear();
                app.lookup_offset = position_in_unfiltered(field, &value);
            }
            PageAction::Stay
        }
        KeyCode::Char(c) if !c.is_control() => {
            app.text_buffer.push(c);
            app.lookup_offset = 0;
            PageAction::Stay
        }
        KeyCode::Enter => {
            // Enter on a Lookup commits the highlighted filter row
            // (same as Space — preserves the user's filter+cursor
            // work) then moves focus forward like Tab does. Page
            // advance is reserved for Enter on the trailing
            // [ Save & Next ] button.
            if let Some((value, _)) = opts.get(app.lookup_offset) {
                let value = value.to_string();
                app.state
                    .widget_set(widget_id, field.key, WizardValue::Choice(value.clone()));
                app.text_buffer.clear();
                app.lookup_offset = position_in_unfiltered(field, &value);
            }
            move_focus(app, widget_id, wd, 1, wd.focus_total());
            PageAction::Stay
        }
        _ => PageAction::Stay,
    }
}

/// Position of `value` inside the focused field's unfiltered option list.
/// Used after Space-commit on a Lookup so the cursor stays on the just-
/// committed row rather than snapping to 0.
fn position_in_unfiltered(field: &WizardField, value: &str) -> usize {
    filtered_lookup_options(field, "")
        .iter()
        .position(|(v, _)| *v == value)
        .unwrap_or(0)
}

/// Cursor index that should be displayed for the currently-focused field.
/// Choice / MultiChoice / Lookup: look up the current value in the option
/// list and return its index so the highlight lands on it. Everything else
/// stays at 0.
fn current_value_index(app: &WizardApp, widget_id: &str, wd: &WizardDescriptor) -> usize {
    let Some(field) = wd.fields.get(app.focus) else {
        return 0;
    };
    match &field.kind {
        WizardFieldKind::Choice { options, .. } => {
            let cur = current_choice(app, widget_id, field);
            options.iter().position(|o| o.value == cur).unwrap_or(0)
        }
        WizardFieldKind::Lookup { .. } => {
            let cur = current_choice(app, widget_id, field);
            position_in_unfiltered(field, &cur)
        }
        WizardFieldKind::Bool { .. } => {
            // Row 0 = yes, row 1 = no.
            match current_value(app, widget_id, field) {
                WizardValue::Bool(true) => 0,
                _ => 1,
            }
        }
        // MultiChoice has many active rows simultaneously; just keep the
        // last cursor position rather than guessing one.
        _ => 0,
    }
}

fn advance_or_gate(app: &mut WizardApp, widget_id: &str, wd: &WizardDescriptor) -> PageAction {
    if let Some(missing) = first_missing_required(app, widget_id, wd) {
        app.feedback = Some(format!(
            "Required field \"{missing}\" needs a value before continuing."
        ));
        return PageAction::Stay;
    }
    PageAction::Advance
}

fn current_choice(app: &WizardApp, widget_id: &str, field: &WizardField) -> String {
    match current_value(app, widget_id, field) {
        WizardValue::Choice(s) => s,
        _ => String::new(),
    }
}

/// Build the dropdown view: blank entry first (when configured), then any
/// `(value, label)` whose value or label contains the filter
/// case-insensitively. An empty filter passes everything through.
fn filtered_lookup_options<'a>(field: &'a WizardField, filter: &str) -> Vec<(&'a str, &'a str)> {
    let WizardFieldKind::Lookup {
        options,
        allow_blank,
        blank_label,
        ..
    } = &field.kind
    else {
        return Vec::new();
    };
    let needle = filter.to_ascii_lowercase();
    let matches = |s: &str| -> bool {
        if needle.is_empty() {
            return true;
        }
        s.to_ascii_lowercase().contains(&needle)
    };

    let mut out: Vec<(&str, &str)> = Vec::new();
    if *allow_blank && matches(blank_label) {
        out.push(("", *blank_label));
    }
    for (value, label) in options {
        if matches(value) || matches(label) {
            out.push((value, label));
        }
    }
    out
}

fn first_missing_required(
    app: &WizardApp,
    widget_id: &str,
    wd: &WizardDescriptor,
) -> Option<String> {
    wd.fields.iter().find_map(|f| {
        if !f.required {
            return None;
        }
        let v = current_value(app, widget_id, f);
        if v.is_empty() {
            Some(f.label.to_string())
        } else {
            None
        }
    })
}

fn current_value(app: &WizardApp, widget_id: &str, field: &WizardField) -> WizardValue {
    app.state
        .widget_get(widget_id, field.key)
        .cloned()
        .unwrap_or_else(|| field.kind.initial_value())
}

/// ←/→ adjustment for Bool (toggle) and Number (±step). Choice /
/// MultiChoice / Lookup are handled by their dedicated dispatchers; this
/// helper short-circuits for everything else.
fn adjust_focused(app: &mut WizardApp, widget_id: &str, wd: &WizardDescriptor, forward: bool) {
    let Some(field) = wd.fields.get(app.focus) else {
        return;
    };
    let cur = current_value(app, widget_id, field);
    let next = match (&field.kind, cur) {
        (WizardFieldKind::Bool { .. }, WizardValue::Bool(b)) => WizardValue::Bool(!b),
        (
            WizardFieldKind::Number {
                default,
                integer,
                range,
            },
            WizardValue::Number(n),
        ) => {
            let step = if *integer { 1.0 } else { 0.1 };
            let mut next = if forward { n + step } else { n - step };
            if let Some((lo, hi)) = range {
                next = next.clamp(*lo, *hi);
            }
            // Snap-back to default when the user pushes past 0 without a
            // configured range — purely cosmetic.
            if range.is_none() && next < 0.0 && default.unwrap_or(0.0) >= 0.0 {
                next = 0.0;
            }
            WizardValue::Number(next)
        }
        (_, v) => v,
    };
    app.state.widget_set(widget_id, field.key, next);
}

fn type_into_focused(app: &mut WizardApp, widget_id: &str, wd: &WizardDescriptor, c: char) {
    let Some(field) = wd.fields.get(app.focus) else {
        return;
    };
    // TextList edits go to the raw buffer — splitting on every keystroke
    // would strip the trailing comma/space the user just typed.
    if matches!(field.kind, WizardFieldKind::TextList { .. }) {
        app.text_buffer.push(c);
        return;
    }
    let cur = current_value(app, widget_id, field);
    let next = match (&field.kind, cur) {
        (WizardFieldKind::Text { .. }, WizardValue::Text(mut s)) => {
            s.push(c);
            WizardValue::Text(s)
        }
        (WizardFieldKind::Path { .. }, WizardValue::Path(mut s)) => {
            s.push(c);
            WizardValue::Path(s)
        }
        (WizardFieldKind::Number { integer, .. }, WizardValue::Number(_)) => {
            // Accumulate digits in the transient text buffer and reparse
            // on every change so users can type "30" cleanly.
            app.text_buffer.push(c);
            let parsed = app.text_buffer.parse::<f64>().unwrap_or(0.0);
            let v = if *integer { parsed.trunc() } else { parsed };
            WizardValue::Number(v)
        }
        (_, v) => v,
    };
    app.state.widget_set(widget_id, field.key, next);
}

fn backspace_focused(app: &mut WizardApp, widget_id: &str, wd: &WizardDescriptor) {
    let Some(field) = wd.fields.get(app.focus) else {
        return;
    };
    if matches!(field.kind, WizardFieldKind::TextList { .. }) {
        app.text_buffer.pop();
        return;
    }
    let cur = current_value(app, widget_id, field);
    let next = match (&field.kind, cur) {
        (WizardFieldKind::Text { .. }, WizardValue::Text(mut s)) => {
            s.pop();
            WizardValue::Text(s)
        }
        (WizardFieldKind::Path { .. }, WizardValue::Path(mut s)) => {
            s.pop();
            WizardValue::Path(s)
        }
        (WizardFieldKind::Number { .. }, WizardValue::Number(_)) => {
            app.text_buffer.pop();
            let parsed = app.text_buffer.parse::<f64>().unwrap_or(0.0);
            WizardValue::Number(parsed)
        }
        (_, v) => v,
    };
    app.state.widget_set(widget_id, field.key, next);
}

/// Shift focus by `step` (positive = forward, negative = back), wrapping
/// at the field boundaries, and resync transient state so the new field
/// renders correctly. Used by Tab/Shift-Tab and by ↑/↓ on non-option
/// fields — both navigation modes need to commit any in-flight TextList
/// or Lookup edits before leaving the field and re-seed the text buffer
/// from the next field's stored value (otherwise the previous field's
/// buffer would bleed through into the new field's display).
fn move_focus(
    app: &mut WizardApp,
    widget_id: &str,
    wd: &WizardDescriptor,
    step: isize,
    field_count: usize,
) {
    commit_inflight_edits(app, widget_id, wd);
    let n = field_count as isize;
    app.focus = ((app.focus as isize + step).rem_euclid(n)) as usize;
    app.text_buffer.clear();
    app.lookup_offset = current_value_index(app, widget_id, wd);
    populate_textlist_buffer(app, widget_id, wd);
}

/// Populate `app.text_buffer` from the focused TextList field's current
/// value so the user sees their existing entries when they start
/// editing. No-op for other field kinds.
fn populate_textlist_buffer(app: &mut WizardApp, widget_id: &str, wd: &WizardDescriptor) {
    let Some(field) = wd.fields.get(app.focus) else {
        return;
    };
    let WizardFieldKind::TextList { separator, .. } = &field.kind else {
        return;
    };
    let WizardValue::TextList(list) = current_value(app, widget_id, field) else {
        return;
    };
    let sep = match separator {
        crate::wizard::descriptor::Separator::Comma => ", ",
        crate::wizard::descriptor::Separator::Newline => "\n",
    };
    app.text_buffer = list.join(sep);
}

/// Commit any in-flight edits on the focused field before it loses
/// focus. Called from Tab / Shift-Tab / Enter / Esc so the user's
/// intermediate state (raw TextList text, active Lookup filter+cursor)
/// is preserved rather than discarded.
///
/// - TextList: split the raw text buffer into entries and store them.
/// - Lookup with a non-empty filter: commit the highlighted row from
///   the filtered list. Without this, typing "vancouver" + ↑/↓ to
///   position + Tab would lose the selection because the filter
///   buffer was discarded on field-leave.
/// - Everything else: no-op.
fn commit_inflight_edits(app: &mut WizardApp, widget_id: &str, wd: &WizardDescriptor) {
    let Some(field) = wd.fields.get(app.focus) else {
        return;
    };
    match &field.kind {
        WizardFieldKind::TextList { separator, .. } => {
            let parts = split_list(&app.text_buffer, *separator);
            app.state
                .widget_set(widget_id, field.key, WizardValue::TextList(parts));
        }
        WizardFieldKind::Lookup { .. } => {
            // If the user is actively filtering, take the currently
            // highlighted row as the commit. An empty filter means they
            // haven't started narrowing yet — leave the existing value
            // alone.
            if app.text_buffer.is_empty() {
                return;
            }
            let opts = filtered_lookup_options(field, &app.text_buffer);
            if let Some((value, _)) = opts.get(app.lookup_offset) {
                app.state.widget_set(
                    widget_id,
                    field.key,
                    WizardValue::Choice((*value).to_string()),
                );
            }
        }
        _ => {}
    }
}

fn split_list(joined: &str, separator: crate::wizard::descriptor::Separator) -> Vec<String> {
    use crate::wizard::descriptor::Separator;
    let parts: Vec<&str> = match separator {
        Separator::Comma => joined.split(',').collect(),
        Separator::Newline => joined.lines().collect(),
    };
    parts
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn render(
    frame: &mut Frame,
    area: Rect,
    app: &WizardApp,
    cell_idx: usize,
    child_idx: Option<usize>,
) {
    let Some((kind, widget_id)) = resolve_target(app, cell_idx, child_idx) else {
        return;
    };
    let Some(desc) = registry::find(&kind) else {
        frame.render_widget(
            Paragraph::new(format!("Unknown widget kind: {}", kind))
                .block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    };
    let wd = (desc.wizard)();

    // Stack-child pages add a "(stack child N of M)" suffix so the
    // user can see they're inside a stack walkthrough; single-widget
    // pages keep the unadorned widget id title.
    let title = match child_idx {
        Some(k) => {
            let total = app
                .state
                .assignments
                .get(cell_idx)
                .map(|a| a.stack_children.len())
                .unwrap_or(0);
            format!(
                " Configure {} (cell {} stack child {} of {}) ",
                widget_id,
                cell_idx + 1,
                k + 1,
                total
            )
        }
        None => format!(" Configure {} ", widget_id),
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = style::pad_inner(block.inner(area));
    frame.render_widget(block, area);

    // Three-column split: form, 2-cell visual gap, layout preview. The
    // gap keeps the preview border from butting against the form's
    // values + dropdown rows. The preview highlights the PARENT cell
    // regardless of whether we're on a stack-child page — the layout
    // grid has no per-child slot to highlight.
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(60),
            Constraint::Length(2),
            Constraint::Min(20),
        ])
        .split(inner);
    super::preview::render(
        frame,
        cols[2],
        &app.state.layout,
        &app.state.assignments,
        Some(cell_idx),
    );

    // Header gets an extra row so the blurb has a blank line of breathing
    // room before the first input field. Without it, blurbs like "Time
    // display with optional secondary…" butted directly into field 1.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(1)])
        .split(cols[0]);

    let header = Paragraph::new(vec![
        Line::from(Span::styled(
            wd.display_name.to_string(),
            style::section_header(),
        )),
        Line::from(Span::styled(wd.blurb.to_string(), style::blurb())),
        Line::from(""),
    ])
    .wrap(Wrap { trim: false });
    frame.render_widget(header, rows[0]);

    if wd.fields.is_empty() {
        let body = Paragraph::new(vec![
            Line::from(""),
            Line::from(
                "No wizard-managed fields yet. Edit the widget's TOML for \
                 advanced configuration. Press Enter to continue.",
            ),
        ])
        .wrap(Wrap { trim: false });
        frame.render_widget(body, rows[1]);
        return;
    }

    render_fields(frame, rows[1], app, &widget_id, &wd);
}

// Indentation conventions inside the body block. Each field section is
// numbered ("1.", "2.", …) flush-left; the value, dropdown rows, and
// help text indent under it so the eye can follow the hierarchy.
const FIELD_BODY_INDENT: &str = "      "; // 6 spaces — under "1.  "
const HELP_INDENT: &str = "       "; // 7 spaces — slight in-set
const DROPDOWN_INDENT: &str = "       ";

fn render_fields(
    frame: &mut Frame,
    area: Rect,
    app: &WizardApp,
    widget_id: &str,
    wd: &WizardDescriptor,
) {
    let mut lines: Vec<Line> = Vec::new();
    for (i, field) in wd.fields.iter().enumerate() {
        let focused = i == app.focus;
        let mut label_spans: Vec<Span> = Vec::new();
        let label_prefix = format!("{}. ", i + 1);
        let label_style = if focused {
            style::label_focused()
        } else {
            style::label()
        };
        label_spans.push(Span::styled(label_prefix, label_style));
        label_spans.push(Span::styled(field.label.to_string(), label_style));
        if field.required {
            label_spans.push(Span::raw(" "));
            label_spans.push(Span::styled("*", style::required()));
        }
        lines.push(Line::from(label_spans));

        // Body — either a list of options (Choice / MultiChoice / Lookup
        // when focused) or a single value line.
        render_field_body(&mut lines, app, widget_id, field, focused);

        if focused {
            lines.push(Line::from(Span::styled(
                format!("{HELP_INDENT}{}", field.help),
                style::help_text(),
            )));
        }
        // Visual gap between fields so labels + help don't run together.
        lines.push(Line::from(""));
    }
    // Trailing [ Save & Next ] button — the explicit page-advance
    // affordance, distinct from per-field Enter (which acts as Tab).
    let on_button = app.focus == wd.fields.len();
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
            "    Enter advances to the next page (Tab/↑ to return to fields).".to_string(),
            style::help_text(),
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_field_body(
    lines: &mut Vec<Line<'static>>,
    app: &WizardApp,
    widget_id: &str,
    field: &WizardField,
    focused: bool,
) {
    match &field.kind {
        WizardFieldKind::Choice { options, .. } => {
            render_choice_list(lines, app, widget_id, field, options, focused);
        }
        WizardFieldKind::MultiChoice { options, .. } => {
            render_multichoice_list(lines, app, widget_id, field, options, focused);
        }
        WizardFieldKind::RemoteMultiChoice { source, defaults } => {
            render_remote_multichoice_list(lines, app, widget_id, field, source, defaults, focused);
        }
        WizardFieldKind::Bool { .. } => {
            render_bool_list(lines, app, widget_id, field, focused);
        }
        WizardFieldKind::OAuth { provider } => {
            render_oauth_status(lines, app, provider, focused);
        }
        WizardFieldKind::Lookup { .. } if focused => {
            append_lookup_dropdown(lines, app, field);
        }
        WizardFieldKind::TextList { .. } if focused => {
            // While editing, the raw text buffer is the source of truth —
            // it carries the un-split trailing commas and spaces the user
            // just typed. Display it directly so editing feels responsive.
            // Append a blinking cursor so the user sees this field is
            // typeable; the cursor lands at the end of any pre-seeded
            // defaults (e.g. Stocks' default tickers) so further typing
            // extends the list.
            let style_for_value = style::value_focused();
            let mut spans: Vec<Span> = vec![Span::raw(FIELD_BODY_INDENT)];
            if !app.text_buffer.is_empty() {
                spans.push(Span::styled(app.text_buffer.clone(), style_for_value));
            }
            spans.push(cursor_span());
            lines.push(Line::from(spans));
        }
        _ => {
            let v = current_value(app, widget_id, field);
            lines.push(render_value(&v, focused, field));
        }
    }
}

/// Render the current authorization status for an OAuth provider as a
/// short value line: whether tokens are already on disk, plus the
/// session's last attempt outcome if the user has triggered the flow
/// during this wizard run. Press Space (while focused) to (re)run the
/// flow — the actual TUI suspend lives in the app loop.
fn render_oauth_status(
    lines: &mut Vec<Line<'static>>,
    app: &WizardApp,
    provider: &str,
    focused: bool,
) {
    // Token-on-disk check is the canonical source of truth — the user
    // may have run `glint --auth <provider>` from another terminal
    // outside this wizard session.
    let has_token = provider_has_token(provider);
    let session_status = app.state.auth_status.get(provider);

    let value_style = if focused {
        style::value_focused()
    } else {
        style::value_idle()
    };

    let summary = match (has_token, session_status) {
        (true, Some(crate::wizard::state::AuthStatus::Failed { message })) => {
            format!("Token on disk · last attempt this session failed: {message}")
        }
        (true, _) => "Authorized — token already on disk.".to_string(),
        (false, Some(crate::wizard::state::AuthStatus::Failed { message })) => {
            format!("Not authorized — last attempt failed: {message}")
        }
        (false, Some(crate::wizard::state::AuthStatus::Authorized)) => {
            // Race: state says authorized but the token store can't
            // find it. Surface honestly so the user knows to retry.
            "Authorization recorded but token not found on disk — retry recommended.".to_string()
        }
        (false, _) => "Not authorized.".to_string(),
    };
    lines.push(Line::from(vec![
        Span::raw(FIELD_BODY_INDENT),
        Span::styled(summary, value_style),
    ]));
    if focused {
        let hint = if has_token {
            "Press Space to re-authorize (e.g. after revoking access)."
        } else {
            "Press Space to authorize — glint opens a browser tab and waits for the redirect."
        };
        lines.push(Line::from(vec![
            Span::raw(FIELD_BODY_INDENT),
            Span::styled(hint, style::help_text()),
        ]));
    }
}

/// `true` when the provider's credentials directory holds a usable
/// token file for the given OAuth provider. Doesn't validate the token
/// — that's the provider's job at fetch time.
fn provider_has_token(provider: &str) -> bool {
    match provider {
        "google" => crate::auth::google::store::GoogleToken::load()
            .map(|t| t.is_some())
            .unwrap_or(false),
        "microsoft" => crate::auth::microsoft::store::MicrosoftToken::load()
            .map(|t| t.is_some())
            .unwrap_or(false),
        _ => false,
    }
}

/// Two-row "( ) yes / ( ) no" list. Row 0 is `yes`; row 1 is `no` — kept
/// in sync with `option_row_count` and `commit_option_selection`.
fn render_bool_list(
    lines: &mut Vec<Line<'static>>,
    app: &WizardApp,
    widget_id: &str,
    field: &WizardField,
    focused: bool,
) {
    let cur = matches!(
        current_value(app, widget_id, field),
        WizardValue::Bool(true)
    );
    let highlight = if focused {
        Some(app.lookup_offset.min(1))
    } else {
        None
    };
    for (i, (value, label)) in [(true, "yes"), (false, "no")].iter().enumerate() {
        let is_active = *value == cur;
        let is_highlighted = highlight == Some(i);
        let marker = if is_active { "(•)" } else { "( )" };
        let marker_style = if is_active {
            style::marker_active()
        } else {
            style::marker_idle()
        };
        let label_style = if is_highlighted {
            style::option_selected()
        } else {
            style::option_idle()
        };
        lines.push(Line::from(vec![
            Span::raw(FIELD_BODY_INDENT),
            Span::styled(marker.to_string(), marker_style),
            Span::raw(" "),
            Span::styled(label.to_string(), label_style),
        ]));
    }
}

/// Vertical radio list. The user's current selection shows a filled
/// `(•)`; the focused (highlighted) row gets reverse-video so users see
/// where Space will pick.
fn render_choice_list(
    lines: &mut Vec<Line<'static>>,
    app: &WizardApp,
    widget_id: &str,
    field: &WizardField,
    options: &[ChoiceOption],
    focused: bool,
) {
    let current = current_choice(app, widget_id, field);
    let highlight = if focused {
        app.lookup_offset.min(options.len().saturating_sub(1))
    } else {
        usize::MAX
    };
    for (i, opt) in options.iter().enumerate() {
        let is_active = opt.value == current;
        let is_highlighted = i == highlight;
        let marker = if is_active { "(•)" } else { "( )" };
        let marker_style = if is_active {
            style::marker_active()
        } else {
            style::marker_idle()
        };
        let label_style = if is_highlighted {
            style::option_selected()
        } else {
            style::option_idle()
        };
        let mut spans = vec![
            Span::raw(FIELD_BODY_INDENT),
            Span::styled(marker.to_string(), marker_style),
            Span::raw(" "),
            Span::styled(opt.label.to_string(), label_style),
        ];
        if let Some(help) = opt.help {
            spans.push(Span::styled(format!("  — {help}"), style::help_text()));
        }
        lines.push(Line::from(spans));
    }
}

/// Variant of [`render_multichoice_list`] for [`WizardFieldKind::RemoteMultiChoice`]:
/// options come from the session's `remote_options` cache (populated
/// after OAuth). Falls back to the descriptor's `defaults` list when
/// the cache is empty, and renders a one-line hint so the user knows
/// the picker is in pre-fetch / fallback mode.
fn render_remote_multichoice_list(
    lines: &mut Vec<Line<'static>>,
    app: &WizardApp,
    widget_id: &str,
    field: &WizardField,
    source: &'static str,
    defaults: &[&'static str],
    focused: bool,
) {
    let selected: Vec<String> = match current_value(app, widget_id, field) {
        WizardValue::MultiChoice(v) => v,
        _ => Vec::new(),
    };
    let cached = app.remote_options.get(source);
    let options: Vec<(String, String)> = match cached {
        Some(opts) => opts.clone(),
        None => defaults
            .iter()
            .map(|s| ((*s).to_string(), (*s).to_string()))
            .collect(),
    };
    if options.is_empty() {
        lines.push(Line::from(vec![
            Span::raw(FIELD_BODY_INDENT),
            Span::styled(
                "(no items to pick — authorize this provider first)".to_string(),
                style::help_text(),
            ),
        ]));
        return;
    }
    let total = options.len();
    let highlight = if focused {
        app.lookup_offset.min(total.saturating_sub(1))
    } else {
        usize::MAX
    };
    if cached.is_none() {
        lines.push(Line::from(vec![
            Span::raw(FIELD_BODY_INDENT),
            Span::styled(
                "(showing defaults — list refreshes after you authorize)".to_string(),
                style::help_text(),
            ),
        ]));
    }
    let (start, end) = visible_window(total, highlight, focused);
    for i in start..end {
        let (value, label) = &options[i];
        let is_active = selected.iter().any(|s| s == value);
        let is_highlighted = i == highlight;
        let marker = if is_active { "[x]" } else { "[ ]" };
        let marker_style = if is_active {
            style::marker_active()
        } else {
            style::marker_idle()
        };
        let label_style = if is_highlighted {
            style::option_selected()
        } else {
            style::option_idle()
        };
        lines.push(Line::from(vec![
            Span::raw(FIELD_BODY_INDENT),
            Span::styled(marker.to_string(), marker_style),
            Span::raw(" "),
            Span::styled(label.clone(), label_style),
        ]));
    }
    if let Some(footer) = window_footer(total, start, end) {
        lines.push(Line::from(Span::styled(
            format!("{FIELD_BODY_INDENT}{footer}"),
            style::help_text(),
        )));
    }
}

/// Vertical checkbox list. Each option shows `[x]` when selected,
/// `[ ]` otherwise; the focused row is highlighted independently.
/// Long lists are scrolled around the highlighted row to keep the
/// page height stable (so the trailing fields + [ Save & Next ]
/// button stay visible even with 60+ Gmail labels in view).
fn render_multichoice_list(
    lines: &mut Vec<Line<'static>>,
    app: &WizardApp,
    widget_id: &str,
    field: &WizardField,
    options: &[ChoiceOption],
    focused: bool,
) {
    let selected: Vec<String> = match current_value(app, widget_id, field) {
        WizardValue::MultiChoice(v) => v,
        _ => Vec::new(),
    };
    let total = options.len();
    let highlight = if focused {
        app.lookup_offset.min(total.saturating_sub(1))
    } else {
        usize::MAX
    };
    let (start, end) = visible_window(total, highlight, focused);
    for i in start..end {
        let opt = &options[i];
        let is_active = selected.iter().any(|s| s == opt.value);
        let is_highlighted = i == highlight;
        let marker = if is_active { "[x]" } else { "[ ]" };
        let marker_style = if is_active {
            style::marker_active()
        } else {
            style::marker_idle()
        };
        let label_style = if is_highlighted {
            style::option_selected()
        } else {
            style::option_idle()
        };
        let mut spans = vec![
            Span::raw(FIELD_BODY_INDENT),
            Span::styled(marker.to_string(), marker_style),
            Span::raw(" "),
            Span::styled(opt.label.to_string(), label_style),
        ];
        if let Some(help) = opt.help {
            spans.push(Span::styled(format!("  — {help}"), style::help_text()));
        }
        lines.push(Line::from(spans));
    }
    if let Some(footer) = window_footer(total, start, end) {
        lines.push(Line::from(Span::styled(
            format!("{FIELD_BODY_INDENT}{footer}"),
            style::help_text(),
        )));
    }
}

/// Bounded window over an option list, centred on `highlight`. When
/// the field isn't focused we still show a short preview (top of list)
/// so the user can see what's in there without focusing it. Mirrors
/// the Lookup dropdown's pattern so the visual rhythm is consistent.
const VISIBLE_OPTION_ROWS: usize = 10;
fn visible_window(total: usize, highlight: usize, focused: bool) -> (usize, usize) {
    if total <= VISIBLE_OPTION_ROWS {
        return (0, total);
    }
    if !focused {
        // Non-focused fields just show the first N rows. The user has
        // to focus the field to scroll — keeps non-focused noise low.
        return (0, VISIBLE_OPTION_ROWS);
    }
    let half = VISIBLE_OPTION_ROWS / 2;
    let start = highlight
        .saturating_sub(half)
        .min(total.saturating_sub(VISIBLE_OPTION_ROWS));
    let end = (start + VISIBLE_OPTION_ROWS).min(total);
    (start, end)
}

/// Footer line summarising the hidden rows, when any are off-screen.
/// `None` ⇒ everything's visible; no footer needed.
fn window_footer(total: usize, start: usize, end: usize) -> Option<String> {
    if total <= VISIBLE_OPTION_ROWS {
        return None;
    }
    let above = start;
    let below = total.saturating_sub(end);
    if above == 0 && below == 0 {
        return None;
    }
    Some(format!(
        "↕ {above} above · {below} below ({total} total — ↑/↓ to scroll, Space to toggle)"
    ))
}

/// Append the dropdown view (search prompt + visible window of filtered
/// options) to the focused field's section.
fn append_lookup_dropdown(lines: &mut Vec<Line<'static>>, app: &WizardApp, field: &WizardField) {
    let opts = filtered_lookup_options(field, &app.text_buffer);
    let total = opts.len();
    let prompt = if app.text_buffer.is_empty() {
        format!("{DROPDOWN_INDENT}filter: (type to narrow)")
    } else {
        format!("{DROPDOWN_INDENT}filter: {}", app.text_buffer)
    };
    lines.push(Line::from(Span::styled(prompt, style::help_text())));

    const VISIBLE: usize = 8;
    if opts.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("{DROPDOWN_INDENT}  (no matches)"),
            style::help_text(),
        )));
        return;
    }
    let selected = app.lookup_offset.min(total.saturating_sub(1));
    let start = selected
        .saturating_sub(VISIBLE / 2)
        .min(total.saturating_sub(VISIBLE).max(0));
    let end = (start + VISIBLE).min(total);
    for (i, (_value, label)) in opts[start..end].iter().enumerate() {
        let row_idx = start + i;
        let marker = if row_idx == selected { "▶ " } else { "  " };
        let row_style = if row_idx == selected {
            style::option_selected()
        } else {
            style::option_idle()
        };
        lines.push(Line::from(vec![
            Span::raw(DROPDOWN_INDENT.to_string()),
            Span::styled(marker.to_string(), row_style),
            Span::styled(label.to_string(), row_style),
        ]));
    }
    let footer = if total > VISIBLE {
        format!(
            "{DROPDOWN_INDENT}({} more — ↑/↓ or PgUp/PgDn to scroll, Space to select)",
            total - VISIBLE
        )
    } else {
        format!("{DROPDOWN_INDENT}(Space to select, Tab to next field)")
    };
    lines.push(Line::from(Span::styled(footer, style::help_text())));
}

fn render_value(v: &WizardValue, focused: bool, field: &WizardField) -> Line<'static> {
    let style_for_value = if focused {
        style::value_focused()
    } else {
        style::value_idle()
    };
    // Text-like fields get an inline blinking cursor while focused so the
    // user sees they can type. For an empty field this replaces the
    // "(empty)" placeholder entirely with just the cursor. The cursor
    // sits at the end of any existing content.
    let is_typeable = matches!(
        field.kind,
        WizardFieldKind::Text { .. }
            | WizardFieldKind::Path { .. }
            | WizardFieldKind::Number { .. }
    );
    if focused && is_typeable {
        let mut spans: Vec<Span> = vec![Span::raw(FIELD_BODY_INDENT.to_string())];
        let body = match v {
            WizardValue::Text(s) | WizardValue::Path(s) => s.clone(),
            WizardValue::Number(n) => format!("{n}"),
            _ => String::new(),
        };
        if !body.is_empty() {
            spans.push(Span::styled(body, style_for_value));
        }
        spans.push(cursor_span());
        return Line::from(spans);
    }
    let text = match v {
        WizardValue::Text(s) | WizardValue::Choice(s) | WizardValue::Path(s) => {
            if s.is_empty() {
                "(empty)".to_string()
            } else {
                s.clone()
            }
        }
        WizardValue::Number(n) => format!("{n}"),
        WizardValue::Bool(b) => if *b { "yes" } else { "no" }.to_string(),
        WizardValue::MultiChoice(v) | WizardValue::TextList(v) => {
            if v.is_empty() {
                "(none)".to_string()
            } else {
                v.join(", ")
            }
        }
    };
    Line::from(vec![
        Span::raw(FIELD_BODY_INDENT.to_string()),
        Span::styled(text, style_for_value),
    ])
}

/// Reverse-video blinking block painted at the end of a focused
/// text-typeable field. Uses a single space cell with the cursor style
/// from the palette (REVERSED + SLOW_BLINK) so it animates with the
/// terminal's cursor blink rate.
pub(crate) fn cursor_span() -> Span<'static> {
    Span::styled("█".to_string(), style::cursor())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wizard::descriptor::{WizardField, WizardFieldKind};

    fn lookup_field(options: Vec<(&'static str, &'static str)>, allow_blank: bool) -> WizardField {
        WizardField {
            key: "tz",
            label: "Timezone",
            help: "",
            required: false,
            kind: WizardFieldKind::Lookup {
                options,
                default: None,
                allow_blank,
                blank_label: "(system local time)",
            },
            validate: None,
        }
    }

    #[test]
    fn filter_passes_everything_through_when_empty() {
        let f = lookup_field(
            vec![
                ("America/Vancouver", "America/Vancouver"),
                ("Europe/London", "Europe/London"),
            ],
            false,
        );
        let out = filtered_lookup_options(&f, "");
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn filter_matches_case_insensitively_on_value_or_label() {
        let f = lookup_field(
            vec![
                ("America/Vancouver", "America/Vancouver"),
                ("Europe/London", "Europe/London"),
                ("Asia/Tokyo", "Asia/Tokyo"),
            ],
            false,
        );
        let out = filtered_lookup_options(&f, "lond");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "Europe/London");
        // Case-insensitive match.
        let out = filtered_lookup_options(&f, "TOKYO");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "Asia/Tokyo");
    }

    #[test]
    fn tab_between_textlist_fields_preserves_each_default() {
        // Reproduces the bug report: tabbing into the stocks watchlist
        // shouldn't show the index tickers. Each TextList field's
        // populate path must read its own descriptor default.
        use crate::wizard::descriptor::{Separator, WizardDescriptor};
        use crate::wizard::pages::Page;
        use crate::wizard::state::{CellAssignment, WizardState};

        let wd = WizardDescriptor {
            display_name: "X",
            blurb: "",
            load_from_toml: None,
            render_toml: None,
            fields: vec![
                WizardField {
                    key: "indices",
                    label: "Index tickers",
                    help: "",
                    required: false,
                    kind: WizardFieldKind::TextList {
                        default: vec!["^DJI".into(), "^GSPC".into()],
                        separator: Separator::Comma,
                    },
                    validate: None,
                },
                WizardField {
                    key: "watchlist",
                    label: "Watchlist tickers",
                    help: "",
                    required: false,
                    kind: WizardFieldKind::TextList {
                        default: vec!["AAPL".into(), "MSFT".into()],
                        separator: Separator::Comma,
                    },
                    validate: None,
                },
            ],
        };

        let mut state = WizardState::default();
        state.assignments.push(CellAssignment {
            cell_index: 0,
            kind: "stocks".into(),
            instance: "main".into(),
            stack_children: Vec::new(),
        });
        let mut app = WizardApp::new(state);
        app.page = Page::Widget(0);
        app.focus = 0;

        // Simulate on_enter: seed text_buffer for focus=0.
        populate_textlist_buffer(&mut app, "stocks@main", &wd);
        assert_eq!(app.text_buffer, "^DJI, ^GSPC");

        // Simulate Tab: commit current, advance focus, re-populate.
        commit_inflight_edits(&mut app, "stocks@main", &wd);
        app.focus = 1;
        app.text_buffer.clear();
        populate_textlist_buffer(&mut app, "stocks@main", &wd);
        assert_eq!(
            app.text_buffer, "AAPL, MSFT",
            "watchlist field should populate from its OWN default, not the indices default"
        );
    }

    #[test]
    fn arrow_down_between_textlist_fields_resets_buffer() {
        // The original bug: ↓ arrow moved focus between fields without
        // committing the prior TextList buffer or re-populating from the
        // new field, so the indices buffer bled into the watchlist
        // display. Tab handled this; arrows didn't.
        use crate::wizard::descriptor::{Separator, WizardDescriptor};
        use crate::wizard::state::{CellAssignment, WizardState};
        use crossterm::event::{KeyCode, KeyEvent};

        let wd_fn: fn() -> WizardDescriptor = || WizardDescriptor {
            display_name: "Stocks-ish",
            blurb: "",
            load_from_toml: None,
            render_toml: None,
            fields: vec![
                WizardField {
                    key: "indices",
                    label: "Index tickers",
                    help: "",
                    required: false,
                    kind: WizardFieldKind::TextList {
                        default: vec!["^DJI".into(), "^GSPC".into()],
                        separator: Separator::Comma,
                    },
                    validate: None,
                },
                WizardField {
                    key: "watchlist",
                    label: "Watchlist tickers",
                    help: "",
                    required: false,
                    kind: WizardFieldKind::TextList {
                        default: vec!["AAPL".into(), "MSFT".into()],
                        separator: Separator::Comma,
                    },
                    validate: None,
                },
            ],
        };

        let mut state = WizardState::default();
        // Stand in for stocks: register a temporary kind under the same
        // assignment slot. The actual `handle_key` path looks up the
        // descriptor via the registry, so we exercise `move_focus`
        // directly here.
        state.assignments.push(CellAssignment {
            cell_index: 0,
            kind: "stocks".into(),
            instance: "main".into(),
            stack_children: Vec::new(),
        });
        let mut app = WizardApp::new(state);
        app.focus = 0;
        let wd = wd_fn();
        populate_textlist_buffer(&mut app, "stocks", &wd);
        assert_eq!(app.text_buffer, "^DJI, ^GSPC");

        // ↓ arrow — must reset the buffer to the next field's default.
        move_focus(&mut app, "stocks", &wd, 1, wd.fields.len());
        assert_eq!(app.focus, 1);
        assert_eq!(
            app.text_buffer, "AAPL, MSFT",
            "↓ between TextList fields must re-seed buffer from new field"
        );

        // And wrapping back via ↑ should likewise re-seed.
        move_focus(&mut app, "stocks", &wd, -1, wd.fields.len());
        assert_eq!(app.focus, 0);
        assert_eq!(app.text_buffer, "^DJI, ^GSPC");

        // Silence dead_code on the closure builder used only here.
        let _ = KeyEvent::from(KeyCode::Down);
    }

    #[cfg(feature = "widget-stocks")]
    #[test]
    fn stocks_descriptor_watchlist_default_is_stock_tickers_not_indices() {
        // Direct guard against the user-reported bug: the stocks
        // descriptor's watchlist field must default to stock tickers,
        // and tabbing from indices into watchlist must surface them.
        use crate::wizard::state::{CellAssignment, WizardState};

        let wd = crate::widgets::stocks::wizard_descriptor();
        // Sanity: field order indices=0, watchlist=1.
        assert_eq!(wd.fields[0].key, "indices");
        assert_eq!(wd.fields[1].key, "watchlist");

        let indices_default = match &wd.fields[0].kind {
            WizardFieldKind::TextList { default, .. } => default.clone(),
            _ => panic!("indices not a TextList"),
        };
        let watchlist_default = match &wd.fields[1].kind {
            WizardFieldKind::TextList { default, .. } => default.clone(),
            _ => panic!("watchlist not a TextList"),
        };
        assert!(
            !watchlist_default.iter().any(|s| s.starts_with('^')),
            "watchlist default unexpectedly contains index ticker(s): {watchlist_default:?}"
        );
        assert_ne!(
            indices_default, watchlist_default,
            "indices and watchlist defaults should differ"
        );

        // End-to-end Tab simulation through the page helpers.
        let mut state = WizardState::default();
        state.assignments.push(CellAssignment {
            cell_index: 0,
            kind: "stocks".into(),
            instance: "main".into(),
            stack_children: Vec::new(),
        });
        let mut app = WizardApp::new(state);
        app.focus = 0;

        populate_textlist_buffer(&mut app, "stocks", &wd);
        assert_eq!(app.text_buffer, indices_default.join(", "));

        commit_inflight_edits(&mut app, "stocks", &wd);
        app.focus = 1;
        app.text_buffer.clear();
        populate_textlist_buffer(&mut app, "stocks", &wd);
        assert_eq!(
            app.text_buffer,
            watchlist_default.join(", "),
            "watchlist field populated with wrong defaults"
        );
    }

    #[test]
    fn visible_window_caps_focused_long_list_around_highlight() {
        // 50 options, highlight near top: window starts at 0 and shows
        // VISIBLE_OPTION_ROWS rows.
        let (s, e) = visible_window(50, 1, true);
        assert_eq!(e - s, VISIBLE_OPTION_ROWS);
        assert_eq!(s, 0);

        // Highlight in the middle: window centred around it.
        let (s, e) = visible_window(50, 25, true);
        assert_eq!(e - s, VISIBLE_OPTION_ROWS);
        assert!(s <= 25 && 25 < e);

        // Highlight at the end: window clamps to the tail.
        let (s, e) = visible_window(50, 49, true);
        assert_eq!(e, 50);
        assert_eq!(e - s, VISIBLE_OPTION_ROWS);
    }

    #[test]
    fn visible_window_shows_everything_when_under_cap() {
        let (s, e) = visible_window(5, 2, true);
        assert_eq!((s, e), (0, 5));
    }

    #[test]
    fn visible_window_non_focused_shows_top_preview_only() {
        let (s, e) = visible_window(50, 25, false);
        assert_eq!((s, e), (0, VISIBLE_OPTION_ROWS));
    }

    #[test]
    fn window_footer_announces_hidden_rows() {
        // Focused, window centred: should report both above and below counts.
        let (s, e) = visible_window(50, 25, true);
        let footer = window_footer(50, s, e).expect("expected footer");
        assert!(footer.contains("above"));
        assert!(footer.contains("below"));
        assert!(footer.contains("50 total"));
    }

    #[test]
    fn window_footer_none_when_everything_visible() {
        assert!(window_footer(5, 0, 5).is_none());
    }

    #[test]
    fn blank_entry_only_appears_when_allow_blank_is_set_and_label_matches() {
        let f = lookup_field(vec![("America/Vancouver", "America/Vancouver")], true);
        let out = filtered_lookup_options(&f, "");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, ""); // blank first
                                  // Filter that matches the blank label keeps the blank entry.
        let out = filtered_lookup_options(&f, "local");
        assert!(out.iter().any(|(v, _)| v.is_empty()));
        // Filter that matches only a real value drops the blank entry.
        let out = filtered_lookup_options(&f, "vancouver");
        assert!(out.iter().all(|(v, _)| !v.is_empty()));
    }
}
