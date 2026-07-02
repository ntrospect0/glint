// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Profile Manager — the wizard's front page for a bare `glint --setup`.
//!
//! Lists the profiles and manages them interactively: pick one to configure,
//! or create / clone / rename / delete. The mutations call the same tested
//! `config::profiles` ops the CLI uses.
//!
//! `app.focus` is the selected row (index into the profile list);
//! `app.manager_mode` is the sub-mode (list ↔ name entry ↔ delete confirm);
//! name entry types into `app.text_buffer`.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Alignment, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use super::PageAction;
use crate::config;
use crate::wizard::{app::WizardApp, style};

/// Sub-mode of the Manager page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Mode {
    /// Browsing the profile list.
    #[default]
    List,
    /// Typing a name for a create / clone / rename (name in `app.text_buffer`).
    Naming(NameAction),
    /// Confirming deletion of the named profile.
    ConfirmDelete(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameAction {
    Create,
    Clone { from: String },
    Rename { old: String },
}

/// Profiles on disk (always includes `default`). Never errors out the UI.
fn profiles() -> Vec<String> {
    config::profiles::list().unwrap_or_else(|_| vec![config::DEFAULT_PROFILE.to_string()])
}

fn focus_on(app: &mut WizardApp, name: &str) {
    let list = profiles();
    app.focus = list.iter().position(|n| n == name).unwrap_or(0);
}

pub fn on_enter(app: &mut WizardApp) {
    app.manager_mode = Mode::List;
    app.text_buffer.clear();
    let list = profiles();
    let active = config::active_profile();
    app.focus = list.iter().position(|n| n == active).unwrap_or(0);
}

pub fn handle_key(key: KeyEvent, app: &mut WizardApp) -> PageAction {
    match app.manager_mode.clone() {
        Mode::List => list_key(key, app),
        Mode::Naming(action) => naming_key(key, app, action),
        Mode::ConfirmDelete(name) => confirm_key(key, app, name),
    }
}

fn selected(app: &WizardApp) -> String {
    profiles()
        .get(app.focus)
        .cloned()
        .unwrap_or_else(|| config::DEFAULT_PROFILE.to_string())
}

fn list_key(key: KeyEvent, app: &mut WizardApp) -> PageAction {
    let list = profiles();
    let len = list.len().max(1);
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            app.focus = (app.focus + len - 1) % len;
            PageAction::Stay
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.focus = (app.focus + 1) % len;
            PageAction::Stay
        }
        KeyCode::Enter | KeyCode::Char(' ') => PageAction::EnterProfileEdit(selected(app)),
        KeyCode::Char('n') => {
            app.text_buffer.clear();
            app.manager_mode = Mode::Naming(NameAction::Create);
            PageAction::Stay
        }
        KeyCode::Char('c') => {
            app.text_buffer.clear();
            app.manager_mode = Mode::Naming(NameAction::Clone { from: selected(app) });
            PageAction::Stay
        }
        KeyCode::Char('r') => {
            let name = selected(app);
            if name == config::DEFAULT_PROFILE {
                app.feedback = Some("The default profile can't be renamed.".into());
            } else {
                app.text_buffer = name.clone();
                app.manager_mode = Mode::Naming(NameAction::Rename { old: name });
            }
            PageAction::Stay
        }
        KeyCode::Char('d') => {
            let name = selected(app);
            if name == config::DEFAULT_PROFILE {
                app.feedback = Some("The default profile can't be deleted.".into());
            } else if name == config::active_profile() {
                app.feedback = Some(format!("Can't delete the active profile {name:?}."));
            } else {
                app.manager_mode = Mode::ConfirmDelete(name);
            }
            PageAction::Stay
        }
        KeyCode::Esc | KeyCode::Char('q') => PageAction::Quit,
        _ => PageAction::Stay,
    }
}

fn naming_key(key: KeyEvent, app: &mut WizardApp, action: NameAction) -> PageAction {
    match key.code {
        KeyCode::Char(c) => {
            app.text_buffer.push(c);
            PageAction::Stay
        }
        KeyCode::Backspace => {
            app.text_buffer.pop();
            PageAction::Stay
        }
        KeyCode::Esc => {
            app.text_buffer.clear();
            app.manager_mode = Mode::List;
            PageAction::Stay
        }
        KeyCode::Enter => {
            let name = app.text_buffer.trim().to_string();
            let result = match &action {
                NameAction::Create => config::profiles::create(&name, None),
                NameAction::Clone { from } => config::profiles::create(&name, Some(from)),
                NameAction::Rename { old } => config::profiles::rename(old, &name),
            };
            match result {
                Ok(()) => {
                    app.feedback = Some(match &action {
                        NameAction::Create => format!("Created profile {name:?}."),
                        NameAction::Clone { from } => {
                            format!("Cloned {from:?} → {name:?} (re-authorize its accounts).")
                        }
                        NameAction::Rename { old } => format!("Renamed {old:?} → {name:?}."),
                    });
                    app.text_buffer.clear();
                    app.manager_mode = Mode::List;
                    focus_on(app, &name);
                }
                Err(err) => {
                    // Stay in Naming so the user can fix the name.
                    app.feedback = Some(format!("Error: {err}"));
                }
            }
            PageAction::Stay
        }
        _ => PageAction::Stay,
    }
}

fn confirm_key(key: KeyEvent, app: &mut WizardApp, name: String) -> PageAction {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            match config::profiles::delete(&name) {
                Ok(()) => app.feedback = Some(format!("Deleted profile {name:?}.")),
                Err(err) => app.feedback = Some(format!("Error: {err}")),
            }
            app.manager_mode = Mode::List;
            let len = profiles().len().max(1);
            app.focus = app.focus.min(len - 1);
            PageAction::Stay
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            app.manager_mode = Mode::List;
            PageAction::Stay
        }
        _ => PageAction::Stay,
    }
}

pub fn render(frame: &mut Frame, area: Rect, app: &WizardApp) {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));

    match &app.manager_mode {
        Mode::List => render_list(app, &mut lines),
        Mode::Naming(action) => render_naming(app, action, &mut lines),
        Mode::ConfirmDelete(name) => render_confirm(name, &mut lines),
    }

    if let Some(msg) = &app.feedback {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(format!("  {msg}"), style::required())));
    }

    let block = Block::default().borders(Borders::ALL).title(" Profiles ");
    let inner = style::pad_inner(block.inner(area));
    frame.render_widget(block, area);
    let body = Paragraph::new(lines)
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: false });
    frame.render_widget(body, inner);
}

fn render_list(app: &WizardApp, lines: &mut Vec<Line>) {
    let list = profiles();
    let active = config::active_profile();

    lines.push(Line::from(Span::styled(
        "Pick a profile to configure, or manage them.",
        style::section_header(),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Each profile is an isolated dashboard — its own layout, widgets, \
         theme, and accounts. The colorscheme library and OAuth app \
         registrations are shared across all profiles.",
        style::blurb(),
    )));
    lines.push(Line::from(""));

    for (i, name) in list.iter().enumerate() {
        let is_sel = i == app.focus;
        let marker = if is_sel { "▸ " } else { "  " };
        let mut tags: Vec<&str> = Vec::new();
        if name == config::DEFAULT_PROFILE {
            tags.push("default");
        }
        if name.as_str() == active {
            tags.push("active");
        }
        let tag = if tags.is_empty() {
            String::new()
        } else {
            format!("   ({})", tags.join(", "))
        };
        let name_style = if is_sel {
            style::page_button_focused()
        } else {
            style::value_idle()
        };
        lines.push(Line::from(vec![
            Span::raw(format!("  {marker}")),
            Span::styled(name.clone(), name_style),
            Span::styled(tag, style::help_text()),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  ↑/↓ select · Enter configure · Esc quit",
        style::help_text(),
    )));
    lines.push(Line::from(Span::styled(
        "  n new · c clone · r rename · d delete",
        style::help_text(),
    )));
}

fn render_naming(app: &WizardApp, action: &NameAction, lines: &mut Vec<Line>) {
    let prompt = match action {
        NameAction::Create => "New profile name:".to_string(),
        NameAction::Clone { from } => format!("Clone {from:?} — new profile name:"),
        NameAction::Rename { old } => format!("Rename {old:?} — new name:"),
    };
    lines.push(Line::from(Span::styled(
        format!("  {prompt}"),
        style::section_header(),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw("      "),
        Span::styled(app.text_buffer.clone(), style::value_focused()),
        Span::styled("█", style::cursor()),
    ]));
    lines.push(Line::from(""));
    if let NameAction::Clone { .. } = action {
        lines.push(Line::from(Span::styled(
            "  Config is copied; credentials are NOT — re-authorize the clone.",
            style::value_idle(),
        )));
    }
    lines.push(Line::from(Span::styled(
        "  Letters, digits, - or _ · Enter confirm · Esc cancel",
        style::help_text(),
    )));
}

fn render_confirm(name: &str, lines: &mut Vec<Line>) {
    lines.push(Line::from(Span::styled(
        format!("  Delete profile {name:?}?"),
        style::section_header(),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  This permanently removes its config, credentials, and cache.",
        style::required(),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  [y]", style::key_hint()),
        Span::styled(" delete     ", style::label()),
        Span::styled("[N]", style::key_hint()),
        Span::styled(" cancel", style::label()),
    ]));
}
