// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Inline OAuth credentials capture page.
//!
//! Triggered when the user hits Space on a widget page's OAuth field
//! and the provider's `credentials/<x>_oauth_client.toml` is still the
//! placeholder template. Collects the user's Client ID (and Secret,
//! for Google) directly in the wizard, writes them to disk with 0600
//! permissions, then re-fires the OAuth flow which now succeeds.
//!
//! Page lifecycle:
//!
//! - `on_enter`: pre-populates the input buffers from any non-placeholder
//!   values already on disk so users can edit-and-resubmit.
//! - Tab / Shift-Tab: cycles between input fields and the [Save &
//!   Authorize] button.
//! - Char / Backspace: edits the focused buffer.
//! - Enter on input: same as Tab — advances focus.
//! - Enter on Save button: writes the credentials file + emits
//!   `PageAction::RunAuth(provider)`. The app loop's RunAuth handler
//!   will see valid credentials this time and proceed to the browser
//!   flow, then pop history back to the widget page.
//! - Esc: cancels — pops history back to the widget page without
//!   writing anything.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use super::PageAction;
use crate::auth::registry::{CredentialsSpec, SetupSchema};
use crate::wizard::{app::WizardApp, style};

/// Resolve a provider's credentials spec via the auth registry. Returns
/// `None` if the provider isn't registered or has no inline setup form.
fn schema_for(provider: &str) -> Option<(&'static CredentialsSpec, &'static SetupSchema)> {
    let spec = crate::auth::registry::find(provider)?.credentials?;
    let setup = spec.setup_schema?;
    Some((spec, setup))
}

/// Number of focusable rows on the form = field count + 1 ("Save"
/// button). The cancel hint is keyboard-only (Esc) so it doesn't take
/// a focus slot.
fn focus_count(setup: &SetupSchema) -> usize {
    setup.fields.len() + 1
}

fn save_button_index(setup: &SetupSchema) -> usize {
    setup.fields.len()
}

/// Seed input buffers from any existing credentials file so the user
/// can edit (rather than re-type) when their Client ID was already set
/// but the Secret got typo'd. Placeholder template values are skipped.
pub fn on_enter(app: &mut WizardApp, provider: &str) {
    app.oauth_capture.clear();
    app.focus = 0;
    let Some((spec, setup)) = schema_for(provider) else {
        return;
    };
    let Ok(dir) = crate::auth::credentials_dir() else {
        return;
    };
    let path = dir.join(spec.filename);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(doc) = toml::from_str::<toml::Value>(&text) else {
        return;
    };
    for field in setup.fields {
        if let Some(s) = doc.get(field.key).and_then(|v| v.as_str()) {
            if s.is_empty() || s.starts_with("REPLACE_WITH_") {
                continue;
            }
            app.oauth_capture
                .insert(field.key.to_string(), s.to_string());
        }
    }
}

pub fn handle_key(key: KeyEvent, app: &mut WizardApp, provider: &str) -> PageAction {
    let Some((spec, setup)) = schema_for(provider) else {
        return PageAction::Back;
    };
    let total = focus_count(setup);
    match key.code {
        KeyCode::Tab => {
            app.focus = (app.focus + 1) % total;
            PageAction::Stay
        }
        KeyCode::BackTab => {
            app.focus = (app.focus + total - 1) % total;
            PageAction::Stay
        }
        KeyCode::Up => {
            app.focus = (app.focus + total - 1) % total;
            PageAction::Stay
        }
        KeyCode::Down => {
            app.focus = (app.focus + 1) % total;
            PageAction::Stay
        }
        KeyCode::Esc => PageAction::Back,
        KeyCode::Enter => {
            if app.focus == save_button_index(setup) {
                match save_and_authorize(app, spec, setup, provider) {
                    Ok(()) => PageAction::RunAuth(provider.to_string()),
                    Err(err) => {
                        app.feedback = Some(format!("Could not save credentials: {err}"));
                        PageAction::Stay
                    }
                }
            } else {
                app.focus = (app.focus + 1) % total;
                PageAction::Stay
            }
        }
        KeyCode::Char(c) => {
            if app.focus < setup.fields.len() {
                let key = setup.fields[app.focus].key;
                app.oauth_capture
                    .entry(key.to_string())
                    .or_default()
                    .push(c);
            }
            PageAction::Stay
        }
        KeyCode::Backspace => {
            if app.focus < setup.fields.len() {
                let key = setup.fields[app.focus].key;
                if let Some(buf) = app.oauth_capture.get_mut(key) {
                    buf.pop();
                }
            }
            PageAction::Stay
        }
        _ => PageAction::Stay,
    }
}

/// Write the captured credentials to `credentials/<filename>` with
/// 0600 perms (Unix). Bails if any field is empty so the user gets a
/// clear "fill in X" message before the OAuth flow tries to load
/// placeholder data.
fn save_and_authorize(
    app: &WizardApp,
    spec: &CredentialsSpec,
    setup: &SetupSchema,
    provider: &str,
) -> anyhow::Result<()> {
    use std::fmt::Write as _;
    let mut body = String::new();
    let _ = writeln!(
        body,
        "# Generated by `glint --setup`. Edit freely; the wizard\n\
         # only rewrites the {n} field{plural} below.\n",
        n = setup.fields.len(),
        plural = if setup.fields.len() == 1 { "" } else { "s" },
    );
    for field in setup.fields {
        let value = app
            .oauth_capture
            .get(field.key)
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if value.is_empty() {
            anyhow::bail!("missing value for {}", field.label);
        }
        writeln!(body, "{} = {}", field.key, toml_quote(&value)).ok();
    }
    for line in setup.extra_lines {
        writeln!(body, "{line}").ok();
    }

    let dir = crate::auth::credentials_dir()?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(spec.filename);
    std::fs::write(&path, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    tracing::info!(
        provider = %provider,
        path = %path.display(),
        "wizard wrote OAuth client credentials"
    );
    Ok(())
}

fn toml_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn render(frame: &mut Frame, area: Rect, app: &WizardApp, provider: &str) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Authorize {provider} "));
    let inner = style::pad_inner(block.inner(area));
    frame.render_widget(block, area);

    let Some((_, setup)) = schema_for(provider) else {
        let para = Paragraph::new(format!("Unknown OAuth provider: {provider}"))
            .wrap(Wrap { trim: false });
        frame.render_widget(para, inner);
        return;
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("Set up your {} OAuth client", setup.short_name),
        style::section_header(),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("  Console: {}", setup.portal_url),
        style::value_idle(),
    )));
    lines.push(Line::from(""));
    // Multi-line hints: split on '\n' so ratatui paints each step on
    // its own row. Without this, the entire hint would render as a
    // single wrapped paragraph and the numbered list loses structure.
    for hint_line in setup.hint.split('\n') {
        lines.push(Line::from(Span::styled(
            format!("  {hint_line}"),
            style::blurb(),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  More detail (screenshots, account-type choices, scope reference): \
         see INSTRUCTIONS.md in the glint repo.",
        style::help_text(),
    )));
    lines.push(Line::from(""));

    for (i, field) in setup.fields.iter().enumerate() {
        let focused = i == app.focus;
        let label_style = if focused {
            style::label_focused()
        } else {
            style::label()
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{}. ", i + 1), label_style),
            Span::styled(field.label.to_string(), label_style),
        ]));
        let value = app
            .oauth_capture
            .get(field.key)
            .cloned()
            .unwrap_or_default();
        let displayed = if field.secret {
            mask_secret(&value)
        } else if value.is_empty() {
            "(empty — type your value)".to_string()
        } else {
            value
        };
        let value_style = if focused {
            style::value_focused()
        } else {
            style::value_idle()
        };
        lines.push(Line::from(vec![
            Span::raw("      "),
            Span::styled(displayed, value_style),
        ]));
        lines.push(Line::from(""));
    }

    // Submit button row.
    let save_idx = save_button_index(setup);
    let save_focused = app.focus == save_idx;
    let save_style = if save_focused {
        style::option_selected()
    } else {
        style::option_idle()
    };
    lines.push(Line::from(vec![
        Span::raw("      "),
        Span::styled("[ Save & Authorize ]", save_style),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Tab cycles fields · type to edit · Enter on [Save] writes credentials \
         and opens your browser · Esc cancels.",
        style::help_text(),
    )));

    if let Some(msg) = &app.feedback {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(format!("  {msg}"), style::error())));
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn mask_secret(s: &str) -> String {
    if s.is_empty() {
        return "(empty — type your value)".to_string();
    }
    if s.len() <= 6 {
        return "*".repeat(s.len());
    }
    format!("{}…{}", &s[..3], "*".repeat(s.len().saturating_sub(3)))
}
