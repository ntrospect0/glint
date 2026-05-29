// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Confirmation page. Summary of what's about to be written + the
//! "Complete and Save" trigger. Points the user at the relevant TOMLs for
//! further hand-tuning.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use super::PageAction;
use crate::wizard::{app::WizardApp, state::LayoutChoice, style};

pub fn handle_key(key: KeyEvent, _app: &mut WizardApp) -> PageAction {
    match key.code {
        KeyCode::Enter | KeyCode::Char(' ') => PageAction::Advance,
        KeyCode::Esc => PageAction::Back,
        _ => PageAction::Stay,
    }
}

pub fn render(frame: &mut Frame, area: Rect, app: &WizardApp) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Confirm — review and save ");
    let outer_inner = style::pad_inner(block.inner(area));
    frame.render_widget(block, area);

    // Two-column split: textual summary on the left, layout preview on
    // the right. The preview echoes each cell's assigned widget so the
    // user sees the dashboard shape they're about to write before they
    // press Save.
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(60),
            Constraint::Length(2),
            Constraint::Min(20),
        ])
        .split(outer_inner);
    let inner = cols[0];
    super::preview::render(
        frame,
        cols[2],
        &app.state.layout,
        &app.state.assignments,
        None,
    );

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "Files that will be written or updated:",
        style::section_header(),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from("  ~/.config/glint/config.toml"));
    for assignment in &app.state.assignments {
        let stem = if assignment.instance == "main" {
            assignment.kind.clone()
        } else {
            format!("{}@{}", assignment.kind, assignment.instance)
        };
        lines.push(Line::from(format!("  ~/.config/glint/{stem}.toml")));
    }
    let picked_provider_name = match app.state.global_get("llm_provider") {
        Some(crate::wizard::descriptor::WizardValue::Choice(s)) => s.clone(),
        _ => crate::llm::PROVIDERS
            .first()
            .map(|p| p.name.to_string())
            .unwrap_or_default(),
    };
    let picked_provider = crate::llm::find_provider(&picked_provider_name);
    lines.push(Line::from("  ~/.config/glint/llm.toml"));
    if let Some(def) = picked_provider {
        let key_state = format!("llm_api_key__{}", def.name);
        let typed_key = match app.state.global_get(&key_state) {
            Some(crate::wizard::descriptor::WizardValue::Text(s)) => s.trim().to_string(),
            _ => String::new(),
        };
        if !typed_key.is_empty() {
            lines.push(Line::from(format!(
                "  ~/.config/glint/credentials/{}",
                def.credentials_filename
            )));
        }
    }
    lines.push(Line::from(""));

    lines.push(Line::from(Span::styled(
        "Summary:",
        style::section_header(),
    )));
    lines.push(Line::from(""));
    let layout_summary = match &app.state.layout {
        LayoutChoice::Preset { name } => format!("  Layout         : {name} preset"),
        LayoutChoice::KeepExisting => "  Layout         : keep existing".into(),
    };
    lines.push(Line::from(layout_summary));
    lines.push(Line::from(format!(
        "  Widgets        : {}",
        app.state.assignments.len()
    )));
    lines.push(Line::from(""));

    lines.push(Line::from(Span::styled(
        "Hand-tuning beyond what the wizard sets:",
        style::section_header(),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(
        "  Custom layout pane sizes        → [layout] in config.toml",
    ));
    lines.push(Line::from(
        "  Custom RSS feed URLs            → [[feeds]] in news.toml",
    ));
    lines.push(Line::from(
        "  Adjust topic keyword lists      → [[topics]] keywords in news.toml",
    ));
    lines.push(Line::from(
        "  Pick which calendars to show    → calendar_ids = [...] per [[providers]] in calendar.toml",
    ));
    lines.push(Line::from(
        "  Mailbox folders / Gmail labels  → folders = [...] in email.toml",
    ));
    lines.push(Line::from(
        "  CalDAV server + credentials     → credentials/caldav.toml",
    ));
    lines.push(Line::from(
        "  Multi-instance widgets          → e.g. clock@home.toml + a layout cell pointing to clock@home",
    ));
    lines.push(Line::from(
        "  Gallery image directories       → images = [...] in gallery.toml",
    ));
    lines.push(Line::from(
        "  Per-widget color overrides      → [colors] block in each widget's TOML",
    ));
    lines.push(Line::from(
        "  Define more color schemes       → colorschemes.toml",
    ));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Further reading:",
        style::section_header(),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(
        "  INSTRUCTIONS.md — step-by-step walkthrough for Google Cloud, Azure,",
    ));
    lines.push(Line::from(
        "                    CalDAV (iCloud/Fastmail), LLM provider key setup, plus a",
    ));
    lines.push(Line::from(
        "                    troubleshooting guide. Read this when re-authorizing or",
    ));
    lines.push(Line::from("                    setting up a new mailbox."));
    lines.push(Line::from(
        "  README.md       — install, keybindings, color schemes, multi-instance",
    ));
    lines.push(Line::from("                    widgets, layout overview."));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Tip: every <widget>.toml is plain TOML — open it in your editor, save, and \
         the next time glint launches (or :reload at runtime) the change takes effect.",
        style::blurb(),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("[ Save & Start Glint ]", style::page_button_focused()),
    ]));
    lines.push(Line::from(Span::styled(
        "    Enter activates · Esc to go back · Ctrl-C to bail (state is preserved).".to_string(),
        style::help_text(),
    )));

    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(para, inner);
}
