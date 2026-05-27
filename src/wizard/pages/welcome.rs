//! Welcome page. First-run users get an orientation blurb; users with a
//! `.wizard_state.toml` from a prior run get a `[Resume]` / `[Start fresh]`
//! choice instead.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Alignment, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use super::PageAction;
use crate::wizard::{app::WizardApp, hydrate, state::WizardState, storage, style};

pub fn handle_key(key: KeyEvent, app: &mut WizardApp) -> PageAction {
    match key.code {
        KeyCode::Enter | KeyCode::Char(' ') => PageAction::Advance,
        KeyCode::Esc => PageAction::Quit,
        KeyCode::Char('r' | 'R') if storage::state_exists() => {
            // Reload the resume buffer in case the user picked Resume but
            // we'd already nuked the in-memory state on Start Fresh.
            match storage::load() {
                Ok(Some(state)) => {
                    app.state = state;
                    PageAction::Advance
                }
                _ => PageAction::Advance,
            }
        }
        KeyCode::Char('n' | 'N') => {
            // Start fresh — discard the in-flight resume buffer in memory
            // (Complete will clear the on-disk copy; we leave it for now
            // in case the user bails). We DO re-hydrate from the user's
            // existing TOMLs so "new session" still surfaces their actual
            // config as defaults — the alternative (truly blank state)
            // would force them to retype values they already have on disk.
            let mut fresh = WizardState::new();
            hydrate::hydrate_from_disk(&mut fresh);
            app.state = fresh;
            PageAction::Advance
        }
        _ => PageAction::Stay,
    }
}

pub fn render(frame: &mut Frame, area: Rect, app: &WizardApp) {
    let has_resume = storage::state_exists();
    // "Existing config" = any state we picked up from disk during
    // hydration (global keys, assignments, or widget values). Distinct
    // from `has_resume`, which is about an interrupted prior wizard run.
    let has_existing_config = !app.state.global.is_empty()
        || !app.state.assignments.is_empty()
        || !app.state.widget_values.is_empty();
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Welcome to the glint setup wizard.",
        style::section_header(),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "This wizard walks you through glint's global settings, grid layout, \
         and per-widget configuration. Your answers are buffered and only \
         written to disk on the final confirmation page — you can quit \
         mid-flow (Ctrl-C) and resume later.",
        style::blurb(),
    )));
    lines.push(Line::from(""));
    if has_existing_config && !has_resume {
        lines.push(Line::from(Span::styled(
            "Existing configuration detected — your current values will \
             be pre-filled at each step. Press Enter to step through and \
             modify what you'd like to change.",
            style::value_idle(),
        )));
        lines.push(Line::from(""));
    }
    if has_resume {
        lines.push(Line::from(Span::styled(
            "A previous session was interrupted.",
            style::required(),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  [R]", style::key_hint()),
            Span::styled("esume       ", style::label()),
            Span::styled("— continue where you left off", style::value_idle()),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  [N]", style::key_hint()),
            Span::styled("ew session  ", style::label()),
            Span::styled(
                "— start fresh (your previous answers are discarded)",
                style::value_idle(),
            ),
        ]));
    }
    // The Welcome page is a single-action page; render an explicit
    // button so the affordance matches every other wizard page (Enter
    // activates the highlighted button rather than implicitly advancing).
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("[ Begin Setup ]", style::page_button_focused()),
    ]));
    lines.push(Line::from(Span::styled(
        "    Enter activates · Esc quits".to_string(),
        style::help_text(),
    )));

    let body = Paragraph::new(lines)
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title(" Welcome "));
    frame.render_widget(body, area);
}
