// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Migration / cleanup prompt — shown ahead of the Profile Manager when a
//! flat (pre-profiles) config, or its leftover duplicates, are present.
//!
//! Two states, detected from disk:
//!
//! - **Not migrated** (flat config, no `profiles/default/`): offer to migrate.
//!   *Migrate* copies the flat config into `profiles/default/`, removes the
//!   flat duplicates, and unlocks multi-profile management. *Keep flat* stays
//!   single-default (edits the flat config directly; no Manager).
//! - **Migrated with leftovers** (`profiles/default/` exists AND flat files
//!   still at the root): offer to remove the now-dead duplicates.
//!
//! Destructive steps run only on explicit consent, and only after the flat
//! config has been copied into `profiles/default/`.

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

/// `profiles/default/` exists → already migrated (any flat files are leftovers).
pub fn already_migrated() -> bool {
    config::glint_root()
        .map(|r| r.join("profiles").join(config::DEFAULT_PROFILE).exists())
        .unwrap_or(false)
}

pub fn handle_key(key: KeyEvent, app: &mut WizardApp) -> PageAction {
    let migrated = already_migrated();
    match key.code {
        // Migrate a flat config into the profiles layout, then remove the
        // flat duplicates.
        KeyCode::Char('m' | 'M') if !migrated => {
            let result = config::migrate::migrate_to_profiles()
                .and_then(|_| config::migrate::remove_flat_originals());
            match result {
                Ok(_) => {
                    app.feedback =
                        Some("Migrated flat config into profiles/default/.".into());
                    PageAction::EnterManager
                }
                Err(err) => {
                    app.feedback = Some(format!("Migration failed: {err}"));
                    PageAction::Stay
                }
            }
        }
        // Remove leftover flat duplicates (already migrated).
        KeyCode::Char('r' | 'R') if migrated => match config::migrate::remove_flat_originals() {
            Ok(n) => {
                app.feedback = Some(format!("Removed {n} leftover flat file(s)."));
                PageAction::EnterManager
            }
            Err(err) => {
                app.feedback = Some(format!("Cleanup failed: {err}"));
                PageAction::Stay
            }
        },
        // Keep as-is. Not migrated → single-default flat mode (Welcome).
        // Already migrated → keep the leftovers and go to the Manager.
        KeyCode::Char('k' | 'K') => {
            if migrated {
                PageAction::EnterManager
            } else {
                PageAction::Advance
            }
        }
        KeyCode::Esc => PageAction::Quit,
        _ => PageAction::Stay,
    }
}

pub fn render(frame: &mut Frame, area: Rect, app: &WizardApp) {
    let migrated = already_migrated();
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));

    if !migrated {
        lines.push(Line::from(Span::styled(
            "Legacy flat configuration detected.",
            style::section_header(),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Your config lives in the old flat layout at ~/.config/glint/. \
             Migrating moves it into profiles/default/ and unlocks multiple \
             profiles you can create, clone, and switch between. Keeping it \
             stays single-profile — you'll just edit the default config here.",
            style::blurb(),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  [M]", style::key_hint()),
            Span::styled("igrate  ", style::label()),
            Span::styled(
                "— move into profiles/default/ and enable multiple profiles",
                style::value_idle(),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  [K]", style::key_hint()),
            Span::styled("eep flat", style::label()),
            Span::styled(
                "  — stay single-profile (edit the default config only)",
                style::value_idle(),
            ),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            "Leftover flat config files found.",
            style::section_header(),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "You've migrated to profiles/default/, but duplicate flat files \
             remain at ~/.config/glint/. glint no longer reads them — they're \
             now just clutter that can cause confusion. They're safe to remove \
             (the live copies are in profiles/default/).",
            style::blurb(),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  [R]", style::key_hint()),
            Span::styled("emove  ", style::label()),
            Span::styled("— delete the leftover flat duplicates", style::value_idle()),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  [K]", style::key_hint()),
            Span::styled("eep    ", style::label()),
            Span::styled("— leave them in place for now", style::value_idle()),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Esc quits. Nothing is removed until you choose.",
        style::help_text(),
    )));

    if let Some(msg) = &app.feedback {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("  {msg}"),
            style::required(),
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
