// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Global config page. Theme + mouse_scroll + LLM provider are rendered
//! as vertical option lists (consistent with the per-widget Choice
//! fields); the API key is a free-form text input that binds to the
//! currently picked provider.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use super::PageAction;
use crate::wizard::{app::WizardApp, descriptor::WizardValue, style};

const FIELD_THEME: usize = 0;
const FIELD_MOUSE_SCROLL: usize = 1;
const FIELD_LLM_PROVIDER: usize = 2;
const FIELD_LLM_KEY: usize = 3;
const FIELD_COUNT: usize = 4;
/// Focus slot for the trailing [ Save & Next ] button. Tab cycles
/// fields → button → first field; Enter on the button advances the
/// page (matching the convention introduced in widget.rs).
const FOCUS_NEXT_BUTTON: usize = FIELD_COUNT;
const FOCUS_TOTAL: usize = FIELD_COUNT + 1;

/// Mouse-scroll choices are a fixed pair — no on-disk equivalent like
/// the theme list has.
const MOUSE_SCROLLS: &[(&str, &str)] = &[
    ("natural", "Natural — wheel-up scrolls up"),
    ("inverted", "Inverted — wheel-up scrolls down"),
];

/// State key under which we cache each provider's typed API key while
/// the wizard is open — so switching the picker doesn't lose mid-typed
/// input. Finalize only writes the active provider's key.
fn llm_key_state_key(provider: &str) -> String {
    format!("llm_api_key__{provider}")
}

/// Provider whose key the API-key field currently edits — the one the
/// user has selected in the picker, falling back to the first registered
/// provider.
fn active_llm_provider(app: &WizardApp) -> &'static str {
    let configured = match app.state.global_get("llm_provider") {
        Some(WizardValue::Choice(s)) => s.clone(),
        _ => String::new(),
    };
    crate::llm::PROVIDERS
        .iter()
        .find(|p| p.name == configured)
        .or_else(|| crate::llm::PROVIDERS.first())
        .map(|p| p.name)
        .unwrap_or("")
}

pub fn handle_key(key: KeyEvent, app: &mut WizardApp) -> PageAction {
    // Field-navigation keys + global escape hatch are handled the same
    // way regardless of which field has focus.
    match key.code {
        KeyCode::Tab => {
            app.focus = (app.focus + 1) % FOCUS_TOTAL;
            app.text_buffer.clear();
            app.lookup_offset = current_value_index(app);
            return PageAction::Stay;
        }
        KeyCode::BackTab => {
            app.focus = (app.focus + FOCUS_TOTAL - 1) % FOCUS_TOTAL;
            app.text_buffer.clear();
            app.lookup_offset = current_value_index(app);
            return PageAction::Stay;
        }
        KeyCode::Esc => return PageAction::Back,
        _ => {}
    }
    // The trailing [ Save & Next ] button consumes only ↑/↓/Enter.
    // j/k mirror ↑/↓ to match the vim-style navigation used elsewhere
    // in the wizard (and called out in feedback when this page was the
    // outlier).
    if app.focus == FOCUS_NEXT_BUTTON {
        return match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                app.focus = FIELD_LLM_KEY;
                PageAction::Stay
            }
            KeyCode::Down | KeyCode::Char('j') => {
                app.focus = FIELD_THEME;
                app.lookup_offset = current_value_index(app);
                PageAction::Stay
            }
            KeyCode::Enter | KeyCode::Char(' ') => PageAction::Advance,
            _ => PageAction::Stay,
        };
    }
    match app.focus {
        FIELD_LLM_KEY => {
            let state_key = llm_key_state_key(active_llm_provider(app));
            handle_text_key(key, app, &state_key)
        }
        _ => handle_choice_key(key, app),
    }
}

fn handle_choice_key(key: KeyEvent, app: &mut WizardApp) -> PageAction {
    let options = options_for_focus(app, app.focus);
    let n = options.len();
    match key.code {
        KeyCode::Down | KeyCode::Char('j') => {
            if n > 0 {
                app.lookup_offset = (app.lookup_offset + 1).min(n - 1);
            }
            PageAction::Stay
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.lookup_offset = app.lookup_offset.saturating_sub(1);
            PageAction::Stay
        }
        KeyCode::Char(' ') => {
            if let Some((value, _)) = options.get(app.lookup_offset) {
                let value = value.to_string();
                let state_key = state_key_for_focus(app.focus);
                app.state
                    .global_set(state_key, WizardValue::Choice(value.clone()));
                // Live-apply the new color scheme so the rest of the
                // wizard repaints in the user's chosen palette.
                if state_key == "theme" {
                    style::set_active_scheme(&value);
                }
            }
            PageAction::Stay
        }
        KeyCode::Enter => {
            // Enter commits the highlighted option (so the user's
            // cursor work isn't dropped) and moves focus forward like
            // Tab. The page advance lives on the trailing
            // [ Save & Next ] button.
            if let Some((value, _)) = options.get(app.lookup_offset) {
                let value = value.to_string();
                let state_key = state_key_for_focus(app.focus);
                app.state
                    .global_set(state_key, WizardValue::Choice(value.clone()));
                if state_key == "theme" {
                    style::set_active_scheme(&value);
                }
            }
            app.focus = (app.focus + 1) % FOCUS_TOTAL;
            app.text_buffer.clear();
            app.lookup_offset = current_value_index(app);
            PageAction::Stay
        }
        _ => PageAction::Stay,
    }
}

fn handle_text_key(key: KeyEvent, app: &mut WizardApp, state_key: &str) -> PageAction {
    match key.code {
        KeyCode::Char(c) => {
            let mut cur = current_text(app, state_key);
            cur.push(c);
            app.state.global_set(state_key, WizardValue::Text(cur));
            PageAction::Stay
        }
        KeyCode::Backspace => {
            let mut cur = current_text(app, state_key);
            cur.pop();
            app.state.global_set(state_key, WizardValue::Text(cur));
            PageAction::Stay
        }
        KeyCode::Enter => {
            // Enter inside the API-key field = move to next focus, not
            // advance. Page advance lives on [ Save & Next ].
            app.focus = (app.focus + 1) % FOCUS_TOTAL;
            app.text_buffer.clear();
            app.lookup_offset = current_value_index(app);
            PageAction::Stay
        }
        _ => PageAction::Stay,
    }
}

/// Borrow the option list for the given field. Theme options come from
/// `app.themes` (read from `colorschemes.toml` at wizard startup);
/// everything else is a static const.
fn options_for_focus<'a>(app: &'a WizardApp, focus: usize) -> Vec<(&'a str, &'a str)> {
    match focus {
        FIELD_THEME => app
            .themes
            .iter()
            .map(|(v, l)| (v.as_str(), l.as_str()))
            .collect(),
        FIELD_MOUSE_SCROLL => MOUSE_SCROLLS.iter().map(|(v, l)| (*v, *l)).collect(),
        FIELD_LLM_PROVIDER => crate::llm::PROVIDERS
            .iter()
            .map(|p| (p.name, p.display_name))
            .collect(),
        _ => Vec::new(),
    }
}

fn state_key_for_focus(focus: usize) -> &'static str {
    match focus {
        FIELD_THEME => "theme",
        FIELD_MOUSE_SCROLL => "mouse_scroll",
        FIELD_LLM_PROVIDER => "llm_provider",
        _ => "",
    }
}

fn current_value_index(app: &WizardApp) -> usize {
    let options = options_for_focus(app, app.focus);
    if options.is_empty() {
        return 0;
    }
    let key = state_key_for_focus(app.focus);
    let cur = current_choice(app, key, options[0].0);
    options.iter().position(|(v, _)| *v == cur).unwrap_or(0)
}

fn current_choice(app: &WizardApp, key: &str, default: &str) -> String {
    match app.state.global_get(key) {
        Some(WizardValue::Choice(s)) => s.clone(),
        _ => default.to_string(),
    }
}

fn current_text(app: &WizardApp, key: &str) -> String {
    match app.state.global_get(key) {
        Some(WizardValue::Text(s)) => s.clone(),
        _ => String::new(),
    }
}

pub fn render(frame: &mut Frame, area: Rect, app: &WizardApp) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Global settings ");
    let inner = style::pad_inner(block.inner(area));
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "Global settings",
        style::section_header(),
    )));
    lines.push(Line::from(Span::styled(
        "  Apply across the whole dashboard — palette, mouse, optional LLM key.",
        style::blurb(),
    )));
    lines.push(Line::from(""));

    let theme_options: Vec<(&str, &str)> = app
        .themes
        .iter()
        .map(|(v, l)| (v.as_str(), l.as_str()))
        .collect();
    render_choice_field(
        &mut lines,
        app,
        1,
        FIELD_THEME,
        "Color scheme",
        &theme_options,
    );
    lines.push(Line::from(""));
    let mouse_options: Vec<(&str, &str)> = MOUSE_SCROLLS.iter().map(|(v, l)| (*v, *l)).collect();
    render_choice_field(
        &mut lines,
        app,
        2,
        FIELD_MOUSE_SCROLL,
        "Mouse scroll direction",
        &mouse_options,
    );
    lines.push(Line::from(""));
    let provider_options: Vec<(&str, &str)> = crate::llm::PROVIDERS
        .iter()
        .map(|p| (p.name, p.display_name))
        .collect();
    render_choice_field(
        &mut lines,
        app,
        3,
        FIELD_LLM_PROVIDER,
        "LLM provider",
        &provider_options,
    );
    lines.push(Line::from(""));
    let active = active_llm_provider(app);
    let key_label = match crate::llm::find_provider(active) {
        Some(def) => format!("{} API key", def.display_name),
        None => "LLM API key".to_string(),
    };
    render_text_field(&mut lines, app, 4, FIELD_LLM_KEY, &key_label);
    lines.push(Line::from(""));

    let on_button = app.focus == FOCUS_NEXT_BUTTON;
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

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn render_choice_field(
    lines: &mut Vec<Line<'static>>,
    app: &WizardApp,
    number: usize,
    field_idx: usize,
    label: &str,
    options: &[(&str, &str)],
) {
    let focused = app.focus == field_idx;
    let label_style = if focused {
        style::label_focused()
    } else {
        style::label()
    };
    lines.push(Line::from(vec![
        Span::styled(format!("{number}. "), label_style),
        Span::styled(label.to_string(), label_style),
    ]));

    let key = state_key_for_focus(field_idx);
    let cur = current_choice(app, key, options.first().map(|(v, _)| *v).unwrap_or(""));
    let highlight = if focused {
        Some(app.lookup_offset.min(options.len().saturating_sub(1)))
    } else {
        None
    };
    for (i, (value, opt_label)) in options.iter().enumerate() {
        let is_active = *value == cur;
        let is_highlighted = highlight == Some(i);
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
            Span::styled(opt_label.to_string(), row_style),
        ]));
    }
    if focused {
        lines.push(Line::from(Span::styled(
            "      ↑/↓ navigates, Space picks, Tab moves to the next field.".to_string(),
            style::help_text(),
        )));
    }
}

fn render_text_field(
    lines: &mut Vec<Line<'static>>,
    app: &WizardApp,
    number: usize,
    field_idx: usize,
    label: &str,
) {
    let focused = app.focus == field_idx;
    let label_style = if focused {
        style::label_focused()
    } else {
        style::label()
    };
    lines.push(Line::from(vec![
        Span::styled(format!("{number}. "), label_style),
        Span::styled(label.to_string(), label_style),
    ]));
    let state_key = llm_key_state_key(active_llm_provider(app));
    let raw_key = current_text(app, &state_key);
    let value_style = if focused {
        style::value_focused()
    } else {
        style::value_idle()
    };
    // When focused, replace the static "(not set)" placeholder with a
    // blinking cursor so the user sees the field is typeable. The
    // existing key is still masked so it doesn't leak to onlookers.
    if focused {
        let mut spans: Vec<Span> = vec![Span::raw("      ")];
        if !raw_key.is_empty() {
            spans.push(Span::styled(mask_api_key(&raw_key), value_style));
        }
        spans.push(Span::styled("█".to_string(), style::cursor()));
        lines.push(Line::from(spans));
    } else {
        let key_display = mask_api_key(&raw_key);
        lines.push(Line::from(vec![
            Span::raw("      "),
            Span::styled(key_display, value_style),
        ]));
    }
    if focused {
        let hint = match crate::llm::find_provider(active_llm_provider(app)) {
            Some(def) => format!(
                "      Optional. Required for LLM-backed features. Get a key at {}. \
                 Type or backspace to edit; leave blank to skip.",
                def.key_portal_url,
            ),
            None => "      Optional. Required for LLM-backed features. \
                     Type or backspace to edit; leave blank to skip."
                .to_string(),
        };
        lines.push(Line::from(Span::styled(hint, style::help_text())));
    }
}

fn mask_api_key(s: &str) -> String {
    if s.is_empty() {
        return String::from("(not set)");
    }
    if s.len() <= 8 {
        return "*".repeat(s.len());
    }
    format!("{}…{}", &s[..4], "*".repeat(s.len() - 4))
}
