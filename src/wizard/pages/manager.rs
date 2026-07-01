// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Profile Manager — the wizard's front page for a bare `glint --setup`.
//!
//! Lists the profiles and lets the user pick one to configure; picking a
//! profile re-targets the wizard at it (via a config-dir override) and drops
//! into the normal flow. Create / clone / rename / delete are available on the
//! CLI (surfaced as hints here); interactive management is a follow-up.
//!
//! `app.focus` is the selected row (index into the profile list).

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

/// Profiles on disk (always includes `default`). Never errors out the UI.
fn profiles() -> Vec<String> {
    config::profiles::list().unwrap_or_else(|_| vec![config::DEFAULT_PROFILE.to_string()])
}

pub fn on_enter(app: &mut WizardApp) {
    // Pre-select the active profile so the highlighted row matches context.
    let list = profiles();
    let active = config::active_profile();
    app.focus = list.iter().position(|n| n == active).unwrap_or(0);
}

pub fn handle_key(key: KeyEvent, app: &mut WizardApp) -> PageAction {
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
        KeyCode::Enter | KeyCode::Char(' ') => {
            let name = list
                .get(app.focus)
                .cloned()
                .unwrap_or_else(|| config::DEFAULT_PROFILE.to_string());
            PageAction::EnterProfileEdit(name)
        }
        KeyCode::Esc | KeyCode::Char('q') => PageAction::Quit,
        _ => PageAction::Stay,
    }
}

pub fn render(frame: &mut Frame, area: Rect, app: &WizardApp) {
    let list = profiles();
    let active = config::active_profile();

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Pick a profile to configure.",
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
        let selected = i == app.focus;
        let marker = if selected { "▸ " } else { "  " };
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
        let name_style = if selected {
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
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Manage profiles from the CLI:",
        style::label(),
    )));
    for hint in [
        "glint --new-profile <name>               create a new profile",
        "glint --new-profile <name> --from <src>  clone a profile's config",
        "glint --rename-profile OLD:NEW           rename",
        "glint --delete-profile <name>            delete",
    ] {
        lines.push(Line::from(Span::styled(
            format!("    {hint}"),
            style::help_text(),
        )));
    }

    let block = Block::default().borders(Borders::ALL).title(" Profiles ");
    let inner = style::pad_inner(block.inner(area));
    frame.render_widget(block, area);
    let body = Paragraph::new(lines)
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: false });
    frame.render_widget(body, inner);
}
