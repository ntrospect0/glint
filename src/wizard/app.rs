// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! TUI shell for the wizard. Owns the event loop, page dispatch, progress
//! header, and footer hint bar. Pages are stateless renderers driven by
//! [`WizardState`]; transient UI state (focus index, in-flight text input)
//! lives here on the [`WizardApp`].

#![allow(dead_code)] // some surface lands in subsequent pages.

use std::io;

use anyhow::{Context, Result};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame, Terminal,
};

use super::{
    flow,
    pages::{Page, PageAction},
    state::{AuthStatus, WizardState},
    storage, style,
};

/// Result of one wizard run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardOutcome {
    /// User clicked Complete and Save — final TOMLs were written.
    Completed,
    /// User quit mid-flow (Ctrl+C / Esc on the welcome page). State buffer
    /// persists; next `--setup` offers Resume.
    Quit,
}

/// Sub-state for the multi-phase Layout page (count picker → preset picker).
/// Lives on the app rather than on `WizardState` because it's transient
/// UI bookkeeping — losing it on a quit-and-resume is harmless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutPhase {
    PickCount,
    PickPreset,
}

impl Default for LayoutPhase {
    fn default() -> Self {
        Self::PickCount
    }
}

/// In-memory state for the running wizard. The `WizardState` field is the
/// persisted buffer; everything else is transient TUI bookkeeping.
pub struct WizardApp {
    pub state: WizardState,
    /// Current page in the flow.
    pub page: Page,
    /// History of visited pages so Back can pop.
    pub history: Vec<Page>,
    /// Index of the focused interactive element on the current page. Each
    /// page defines what indices mean.
    pub focus: usize,
    /// Per-page transient text buffer (the actively typed string). Cleared
    /// on page transitions. For `Lookup` fields this is the user's filter
    /// query — typing narrows the dropdown.
    pub text_buffer: String,
    /// Selected row inside a `Lookup` dropdown (index into the filtered
    /// option list). Reset when focus changes between fields or pages.
    pub lookup_offset: usize,
    /// Sub-state for the Layout page (count picker → preset picker).
    pub layout_phase: LayoutPhase,
    /// Inline validation / error message displayed under the page body.
    /// Cleared on the next key press unless replaced.
    pub feedback: Option<String>,
    /// Available color schemes (value, display label), sourced from the
    /// user's `colorschemes.toml`. Loaded once at wizard startup so the
    /// Global page's theme picker reflects whatever the user actually has
    /// on disk — including any custom schemes they've added. Ordered with
    /// `default` first, then the rest alphabetically.
    pub themes: Vec<(String, String)>,
    /// Transient text buffers for the OAuth credentials capture page,
    /// keyed by the credential field name (`client_id`, `client_secret`,
    /// `tenant`). Cleared whenever we leave the OAuthSetup page. Lives
    /// on the app rather than `WizardState` because real OAuth secrets
    /// only land in `credentials/` once the user confirms — we don't
    /// want partial typing leaking into the resume buffer.
    pub oauth_capture: std::collections::HashMap<String, String>,
    /// Runtime-fetched option lists for [`WizardFieldKind::RemoteMultiChoice`]
    /// fields, keyed by the field's `source`. Populated after a
    /// successful OAuth flow (Gmail labels, Outlook folders) so the
    /// next render of the corresponding widget page can present them as
    /// checkboxes. Cleared on wizard exit — these are session-scoped.
    pub remote_options: std::collections::HashMap<String, Vec<(String, String)>>,
}

impl WizardApp {
    pub fn new(state: WizardState) -> Self {
        let page = flow::start_page(&state);
        // Seed the wizard's color palette: whatever theme the user already
        // picked (resumed state) wins, otherwise we boot on the wizard's
        // default scheme so even the first frame looks "themed" rather
        // than ANSI-default.
        let initial_scheme = match state.global_get("theme") {
            Some(super::descriptor::WizardValue::Choice(s)) if !s.is_empty() => s.clone(),
            _ => style::DEFAULT_SCHEME.to_string(),
        };
        style::set_active_scheme(&initial_scheme);
        Self {
            state,
            page,
            history: Vec::new(),
            focus: 0,
            text_buffer: String::new(),
            lookup_offset: 0,
            layout_phase: LayoutPhase::default(),
            feedback: None,
            themes: load_available_themes(),
            oauth_capture: std::collections::HashMap::new(),
            remote_options: std::collections::HashMap::new(),
        }
    }
}

/// Run the new TUI wizard. Returns `Completed` once the user finishes the
/// confirmation page; `Quit` if they bail out early (state file kept so
/// the next `--setup` offers Resume).
pub fn run_wizard() -> Result<WizardOutcome> {
    // Load the resume buffer if present (None on version mismatch /
    // corruption / no prior run), then ALWAYS backfill from disk.
    //
    // `hydrate_from_disk` is additive — every field it seeds is guarded by a
    // "does the state already have this?" check ("resume values win"), so
    // running it over a resumed buffer only fills the gaps. This is what makes
    // a re-run of `--setup` surface current on-disk values (e.g. an existing
    // API key, the configured theme) as defaults, and — crucially — stops a
    // stale or partial buffer from *masking* real config it happens to lack.
    let mut state = storage::load()?.unwrap_or_default();
    super::hydrate::hydrate_from_disk(&mut state);
    let mut app = WizardApp::new(state);
    super::pages::on_enter(&mut app);

    let mut terminal = enter_tui().context("failed to initialize wizard terminal")?;
    let _guard = TuiGuard;

    loop {
        terminal.draw(|frame| render(frame, &app))?;

        let evt = match event::read() {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(error = %err, "wizard event read failed");
                continue;
            }
        };

        let Event::Key(key) = evt else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        // Global escape hatches.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            persist(&app.state);
            return Ok(WizardOutcome::Quit);
        }

        let action = super::pages::dispatch_key(key, &mut app);
        match action {
            PageAction::Stay => {}
            PageAction::Advance => {
                let page_id = app.page.id().to_string();
                app.state.mark_completed(&page_id);
                app.state.last_page = Some(page_id);
                persist(&app.state);
                let next = flow::next_page(&app.page, &app.state);
                match next {
                    Some(next_page) => {
                        let prev = std::mem::replace(&mut app.page, next_page);
                        app.history.push(prev);
                        app.focus = 0;
                        app.text_buffer.clear();
                        app.lookup_offset = 0;
                        app.layout_phase = LayoutPhase::default();
                        app.feedback = None;
                        super::pages::on_enter(&mut app);
                    }
                    None => {
                        // No next page → finalize and exit.
                        super::finalize::write_all(&app.state)?;
                        storage::clear()?;
                        return Ok(WizardOutcome::Completed);
                    }
                }
            }
            PageAction::Back => {
                // History captures the user's actual forward path. On a
                // mid-flow resume the stack is empty — fall back to the
                // flow's logical prev_page so Esc still navigates back.
                //
                // When leaving the AssignStack breakout (Esc/cancel),
                // restore focus to the cell the user was configuring
                // instead of jumping to Cell 1.
                let restore_focus = match &app.page {
                    Page::AssignStack { cell_index } => Some(*cell_index),
                    _ => None,
                };
                let target = app
                    .history
                    .pop()
                    .or_else(|| flow::prev_page(&app.page, &app.state));
                if let Some(prev) = target {
                    app.page = prev;
                    app.focus = restore_focus.unwrap_or(0);
                    app.text_buffer.clear();
                    app.lookup_offset = 0;
                    app.layout_phase = LayoutPhase::default();
                    app.feedback = None;
                    super::pages::on_enter(&mut app);
                }
            }
            PageAction::Quit => {
                persist(&app.state);
                return Ok(WizardOutcome::Quit);
            }
            PageAction::RunAuth(provider_name) => {
                // If the user hasn't provided OAuth client credentials
                // yet (or has only the placeholder template), route to
                // the inline OAuthSetup page to collect them, rather
                // than failing with a "missing file" error.
                if crate::auth::registry::needs_credential_capture(&provider_name)
                    && !matches!(app.page, Page::OAuthSetup { .. })
                {
                    let prev = std::mem::replace(
                        &mut app.page,
                        Page::OAuthSetup {
                            provider: provider_name.clone(),
                        },
                    );
                    app.history.push(prev);
                    app.focus = 0;
                    app.text_buffer.clear();
                    app.lookup_offset = 0;
                    app.feedback = None;
                    super::pages::on_enter(&mut app);
                    continue;
                }

                // Credentials look valid — tear down the TUI so the
                // OAuth flow's browser launcher and loopback HTTP server
                // can use the real terminal.
                let _ = disable_raw_mode();
                let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);

                let result = run_oauth_for_provider(&provider_name);
                let status = match &result {
                    Ok(()) => AuthStatus::Authorized,
                    Err(err) => AuthStatus::Failed {
                        message: err.to_string(),
                    },
                };
                app.state.auth_status.insert(provider_name.clone(), status);
                persist(&app.state);
                app.feedback = Some(match &result {
                    Ok(()) => format!("{provider_name}: authorization complete."),
                    Err(err) => format!("{provider_name}: {err}"),
                });

                // After a successful auth, pre-fetch runtime option lists
                // (Gmail labels, Outlook folders, …) so the next widget
                // page render can paint RemoteMultiChoice fields with
                // real values instead of "Authorize first" placeholders.
                // Non-fatal if it fails — the picker still shows
                // defaults / placeholder.
                if result.is_ok() {
                    fetch_remote_options_for_provider(&provider_name, &mut app);
                }

                // Rebuild the TUI to continue the wizard.
                terminal = enter_tui()?;

                // If the flow was invoked from the OAuthSetup page,
                // pop back to the originating widget page now that the
                // browser handshake is done. Cancellation / failure
                // also returns — keeps the user from getting stuck on
                // the setup page after a retry.
                if matches!(app.page, Page::OAuthSetup { .. }) {
                    if let Some(prev) = app.history.pop() {
                        app.page = prev;
                        app.oauth_capture.clear();
                        super::pages::on_enter(&mut app);
                    }
                }
            }
            PageAction::OpenAssignStack(cell_index) => {
                // Push current Assign page onto history; switch to the
                // AssignStack sub-page for this cell. Pop back happens
                // when the sub-page returns PageAction::Back.
                let prev = std::mem::replace(&mut app.page, Page::AssignStack { cell_index });
                app.history.push(prev);
                app.focus = 0;
                app.text_buffer.clear();
                app.lookup_offset = 0;
                app.feedback = None;
                super::pages::on_enter(&mut app);
            }
            PageAction::AssignStackDone { cell_index } => {
                // Stack saved — pop back to Assign and advance focus
                // to the next cell, mirroring the single-widget Enter
                // path in assign.rs (which also advances after a pick).
                // `focus_total = cell_count + 1` includes the trailing
                // [Save & Next] button as the wrap target, same as the
                // regular advance.
                let target = app
                    .history
                    .pop()
                    .or_else(|| flow::prev_page(&app.page, &app.state));
                if let Some(prev) = target {
                    app.page = prev;
                    let cell_count = app.state.assignments.len();
                    let focus_total = cell_count + 1;
                    app.focus = if focus_total > 0 {
                        (cell_index + 1) % focus_total
                    } else {
                        0
                    };
                    app.text_buffer.clear();
                    app.lookup_offset = 0;
                    app.layout_phase = LayoutPhase::default();
                    app.feedback = None;
                    super::pages::on_enter(&mut app);
                }
            }
        }
    }
}

/// Drive a single OAuth provider's authorization flow from inside the
/// wizard. We're already running inside the tokio runtime that `main`
/// established (the wizard is invoked via `runtime.block_on`), so
/// `Handle::current()` resolves; `block_in_place` lets us turn the
/// async flow into a synchronous call without nesting runtimes.
///
/// Before kicking off the flow we ensure the provider's
/// `<provider>_oauth_client.toml` template exists in `credentials/`.
/// On a fresh install this lets the user press Space on the wizard's
/// Authorize field, see a clear "edit credentials/foo.toml" message,
/// edit the file in another terminal, and retry — without having to
/// quit and re-run `--setup`.
fn run_oauth_for_provider(provider_name: &str) -> Result<()> {
    crate::auth::registry::ensure_credentials_template(provider_name)?;
    let provider = crate::auth::registry::find(provider_name).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown auth provider {provider_name:?} (known: {})",
            crate::auth::registry::names_csv()
        )
    })?;
    // The wizard only ever authenticates the default account; extra
    // accounts are added via `glint --auth <provider>:<label>`.
    let handle = tokio::runtime::Handle::current();
    tokio::task::block_in_place(|| {
        handle.block_on((provider.run)(crate::auth::DEFAULT_ACCOUNT))
    })
}

/// Pre-fetch any remote option lists the provider exposes via
/// [`AuthProvider::post_auth_refresh`] (e.g. Gmail labels, Outlook
/// folders) and cache them on the app for widget-page `RemoteMultiChoice`
/// fields. Per-provider; failures log and leave the cache empty so the
/// picker falls back to its `defaults` list.
fn fetch_remote_options_for_provider(provider_name: &str, app: &mut WizardApp) {
    let Some(provider) = crate::auth::registry::find(provider_name) else {
        return;
    };
    let Some(refresh) = provider.post_auth_refresh else {
        return;
    };
    let handle = tokio::runtime::Handle::current();
    let result = tokio::task::block_in_place(|| handle.block_on(refresh()));
    match result {
        Ok((key, opts)) => {
            app.remote_options.insert(key.to_string(), opts);
        }
        Err(err) => {
            tracing::warn!(
                provider = provider_name,
                error = %err,
                "wizard: post-auth remote-option refresh failed"
            );
        }
    }
}

/// Best-effort state save. Failures log + continue — losing the resume
/// buffer is annoying but not fatal.
fn persist(state: &WizardState) {
    if let Err(err) = storage::save(state) {
        tracing::warn!(error = %err, "failed to save wizard state");
    }
}

/// Read `colorschemes.toml` and surface its schemes as `(value, label)`
/// pairs for the Global page's theme picker. `default` is always offered
/// (and pinned first) so a brand-new file with no extra schemes still
/// gives the user something to pick. Falls back to a single `default`
/// entry if the file is missing or unreadable.
fn load_available_themes() -> Vec<(String, String)> {
    let names = match crate::theme::load_schemes_file() {
        Ok(file) => file.schemes.into_keys().collect::<Vec<_>>(),
        Err(err) => {
            tracing::warn!(error = %err, "wizard could not read colorschemes.toml — offering only 'default'");
            Vec::new()
        }
    };
    let mut rest: Vec<String> = names.into_iter().filter(|n| n != "default").collect();
    rest.sort();
    let mut out = Vec::with_capacity(rest.len() + 1);
    out.push((
        "default".to_string(),
        "Default — built-in palette".to_string(),
    ));
    for name in rest {
        let label = humanize_scheme_name(&name);
        out.push((name, label));
    }
    out
}

/// Turn a snake_case scheme key into a Title-Case display label
/// (`solarized_light` → `Solarized Light`). Keeps the wizard listing
/// readable without forcing scheme authors to declare a display name
/// explicitly.
fn humanize_scheme_name(key: &str) -> String {
    key.split('_')
        .filter(|s| !s.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_scheme_name_title_cases_snake() {
        assert_eq!(humanize_scheme_name("nord"), "Nord");
        assert_eq!(humanize_scheme_name("gruvbox_dark"), "Gruvbox Dark");
        assert_eq!(
            humanize_scheme_name("solarized_light_v2"),
            "Solarized Light V2"
        );
        assert_eq!(humanize_scheme_name(""), "");
    }
}

fn render(frame: &mut Frame, app: &WizardApp) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4), // header (title + progress bar)
            Constraint::Min(5),    // body
            Constraint::Length(3), // footer
        ])
        .split(area);

    render_header(frame, chunks[0], app);
    super::pages::render_body(frame, chunks[1], app);
    render_footer(frame, chunks[2], app);
}

fn render_header(frame: &mut Frame, area: Rect, app: &WizardApp) {
    let current = flow::current_step(&app.page, &app.state);
    let total = flow::total_steps(&app.state);
    let (filled, empty) = style::progress_chars(current, total);
    let pct = if total == 0 {
        0
    } else {
        (current.min(total) * 100) / total
    };

    let lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled(" glint setup ", style::section_header()),
            Span::raw(" — "),
            Span::styled(
                app.page.title(&app.state),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(filled, style::progress_filled()),
            Span::styled(empty, style::progress_empty()),
            Span::raw("  "),
            Span::styled(format!("step {current}/{total}"), style::key_hint_desc()),
            Span::raw("  "),
            Span::styled(format!("({pct}%)"), style::key_hint_desc()),
        ]),
    ];
    let header = Paragraph::new(lines).block(Block::default().borders(Borders::BOTTOM));
    frame.render_widget(header, area);
}

fn render_footer(frame: &mut Frame, area: Rect, app: &WizardApp) {
    let mut lines: Vec<Line> = Vec::with_capacity(2);
    if let Some(msg) = app.feedback.as_deref() {
        lines.push(Line::from(Span::styled(msg.to_string(), style::error())));
    }
    // Compact key hint row — page can override via app.feedback for
    // contextual messages.
    lines.push(Line::from(vec![
        Span::styled("↑/↓", style::key_hint()),
        Span::styled(" within field  ", style::key_hint_desc()),
        Span::styled("Tab/Enter", style::key_hint()),
        Span::styled(" next field  ", style::key_hint_desc()),
        Span::styled("Space", style::key_hint()),
        Span::styled(" pick  ", style::key_hint_desc()),
        Span::styled("Enter on [Save & Next]", style::key_hint()),
        Span::styled(" advance page  ", style::key_hint_desc()),
        Span::styled("Esc", style::key_hint()),
        Span::styled(" back  ", style::key_hint_desc()),
        Span::styled("Ctrl-C", style::key_hint()),
        Span::styled(" quit (resume later)", style::key_hint_desc()),
    ]));
    let footer = Paragraph::new(lines).block(Block::default().borders(Borders::TOP));
    frame.render_widget(footer, area);
}

type Tui = Terminal<CrosstermBackend<io::Stdout>>;

fn enter_tui() -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

/// Restores the terminal on drop so a panic still leaves the user's shell sane.
struct TuiGuard;

impl Drop for TuiGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
    }
}

/// Convenience: handle a key event that should accept the focused button.
/// Pages use this to keep their key handlers tight.
pub fn is_activation_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Enter | KeyCode::Char(' '))
}
