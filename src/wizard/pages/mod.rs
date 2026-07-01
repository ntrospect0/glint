// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Wizard pages. Each page is a thin module with a `render` + `handle_key`
//! function. The [`Page`] enum holds zero per-page state — transient UI
//! state lives on [`crate::wizard::app::WizardApp`]; persistent data lives
//! in [`crate::wizard::state::WizardState`].

#![allow(dead_code)]

pub mod assign;
pub mod assign_stack;
pub mod confirm;
pub mod global;
pub mod layout;
pub mod manager;
pub mod oauth_setup;
pub mod preview;
pub mod welcome;
pub mod widget;

use crossterm::event::KeyEvent;
use ratatui::{layout::Rect, Frame};

use super::app::WizardApp;

/// Identifier for a page in the wizard flow. Variants without payload have
/// a fixed position; `Widget(i)` indexes into `WizardState.assignments`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Page {
    /// Profile Manager — the front page for a bare `--setup`, listing
    /// profiles to pick one to configure. Not part of the linear flow.
    Manager,
    Welcome,
    Global,
    Layout,
    Assign,
    Widget(usize),
    /// Out-of-band credential capture before running an OAuth flow.
    /// Pushed onto history when the user hits Space on a widget page's
    /// OAuth field and the provider's `credentials/<x>_oauth_client.toml`
    /// is still a placeholder. Pops back to the originating Widget page
    /// once the user saves + authorizes (or cancels).
    OAuthSetup {
        provider: String,
    },
    /// Stack-cell composition picker. Pushed onto history when the
    /// user picks "Stack" on the Assign page; walks them through
    /// slot 1 / slot 2 / slot 3. Pops back to Assign on save or
    /// cancel.
    AssignStack {
        cell_index: usize,
    },
    /// Per-child configuration page for a stack cell. After visiting
    /// every single-widget cell's Widget page, the wizard walks
    /// through each stack cell's children one by one — same UI as
    /// Widget(i), but the kind / widget_id come from
    /// `assignments[cell_index].stack_children[child_index]`.
    StackChild {
        cell_index: usize,
        child_index: usize,
    },
    Confirm,
}

impl Page {
    /// Stable page id used in state files (resume) and completion tracking.
    pub fn id(&self) -> String {
        match self {
            Page::Manager => "manager".into(),
            Page::Welcome => "welcome".into(),
            Page::Global => "global".into(),
            Page::Layout => "layout".into(),
            Page::Assign => "assign".into(),
            Page::Widget(i) => format!("widget-{i}"),
            Page::OAuthSetup { provider } => format!("oauth-setup-{provider}"),
            Page::AssignStack { cell_index } => format!("assign-stack-{cell_index}"),
            Page::StackChild {
                cell_index,
                child_index,
            } => {
                format!("stack-child-{cell_index}-{child_index}")
            }
            Page::Confirm => "confirm".into(),
        }
    }

    /// Human title shown in the wizard header.
    pub fn title(&self, state: &super::state::WizardState) -> String {
        match self {
            Page::Manager => "Profiles".into(),
            Page::Welcome => "Welcome".into(),
            Page::Global => "Global settings".into(),
            Page::Layout => "Layout".into(),
            Page::Assign => "Assign widgets".into(),
            Page::Widget(i) => match state.assignments.get(*i) {
                Some(a) => format!("Configure {}", a.widget_id()),
                None => "Widget".into(),
            },
            Page::OAuthSetup { provider } => format!("Authorize {provider}"),
            Page::AssignStack { cell_index } => {
                format!("Configure stack — cell {}", cell_index + 1)
            }
            Page::StackChild {
                cell_index,
                child_index,
            } => {
                match state
                    .assignments
                    .get(*cell_index)
                    .and_then(|a| a.stack_children.get(*child_index))
                {
                    Some(child) => {
                        format!(
                            "Configure {} (cell {} stack)",
                            child.widget_id(),
                            cell_index + 1
                        )
                    }
                    None => "Configure stack child".into(),
                }
            }
            Page::Confirm => "Confirm".into(),
        }
    }
}

/// Outcome of a page's `handle_key`. The app loop interprets this to drive
/// navigation; pages don't mutate `WizardApp.page` directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageAction {
    /// Stay on the current page (default).
    Stay,
    /// Advance to the next page (validation gated by the page itself).
    Advance,
    /// Back to the previous page in history.
    Back,
    /// Save state and exit the wizard without finalising.
    Quit,
    /// Suspend the TUI, run an OAuth flow for the named provider (which
    /// will open a browser and listen on a loopback port), then resume.
    /// The resulting [`super::state::AuthStatus`] is written into
    /// `app.state.auth_status[provider]` by the app loop.
    RunAuth(String),
    /// Push the AssignStack page onto history for the given cell. The
    /// app loop performs the navigation; the assign page emits this
    /// when the user picks "Stack" as the cell kind.
    OpenAssignStack(usize),
    /// Commit the in-progress stack for `cell_index` and pop back to
    /// the Assign page with focus advanced to the next cell. Mirrors
    /// the single-widget Enter behavior on Assign (which also advances
    /// focus after committing) so the user doesn't have to manually
    /// Tab back through cells they've already configured.
    AssignStackDone { cell_index: usize },
    /// From the Profile Manager: re-target the wizard at the named profile
    /// (re-hydrate from its config) and drop into the normal flow. The app
    /// loop performs the retarget + navigation.
    EnterProfileEdit(String),
}

/// Dispatch a key event to the active page's `handle_key`.
pub fn dispatch_key(key: KeyEvent, app: &mut WizardApp) -> PageAction {
    // Clear stale feedback on every key — pages that want to keep a
    // message visible refresh it inside their handler.
    app.feedback = None;
    match app.page.clone() {
        Page::Manager => manager::handle_key(key, app),
        Page::Welcome => welcome::handle_key(key, app),
        Page::Global => global::handle_key(key, app),
        Page::Layout => layout::handle_key(key, app),
        Page::Assign => assign::handle_key(key, app),
        Page::Widget(i) => widget::handle_key(key, app, i, None),
        Page::OAuthSetup { provider } => oauth_setup::handle_key(key, app, &provider),
        Page::AssignStack { cell_index } => assign_stack::handle_key(key, app, cell_index),
        Page::StackChild {
            cell_index,
            child_index,
        } => widget::handle_key(key, app, cell_index, Some(child_index)),
        Page::Confirm => confirm::handle_key(key, app),
    }
}

/// Render the active page's body into `area`.
pub fn render_body(frame: &mut Frame, area: Rect, app: &WizardApp) {
    match &app.page {
        Page::Manager => manager::render(frame, area, app),
        Page::Welcome => welcome::render(frame, area, app),
        Page::Global => global::render(frame, area, app),
        Page::Layout => layout::render(frame, area, app),
        Page::Assign => assign::render(frame, area, app),
        Page::Widget(i) => widget::render(frame, area, app, *i, None),
        Page::OAuthSetup { provider } => oauth_setup::render(frame, area, app, provider),
        Page::AssignStack { cell_index } => assign_stack::render(frame, area, app, *cell_index),
        Page::StackChild {
            cell_index,
            child_index,
        } => widget::render(frame, area, app, *cell_index, Some(*child_index)),
        Page::Confirm => confirm::render(frame, area, app),
    }
}

/// Called by the app loop after every page transition (Advance, Back, or
/// initial start). Lets pages set up transient TUI state — currently the
/// only consumer is the widget page, which seeds its TextList buffer from
/// the focused field's stored value so users see their existing entries
/// when editing.
pub fn on_enter(app: &mut WizardApp) {
    match app.page.clone() {
        Page::Manager => manager::on_enter(app),
        Page::Widget(i) => widget::on_enter(app, i, None),
        Page::Assign => assign::on_enter(app),
        Page::OAuthSetup { provider } => oauth_setup::on_enter(app, &provider),
        Page::AssignStack { cell_index } => assign_stack::on_enter(app, cell_index),
        Page::StackChild {
            cell_index,
            child_index,
        } => widget::on_enter(app, cell_index, Some(child_index)),
        _ => {}
    }
}
