// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Interactive setup wizard for glint. Invoked by `glint --setup` and by
//! the first-run UX in `main.rs` when no `config.toml` is present.
//!
//! The wizard is a TUI app of its own (ratatui + crossterm) that buffers
//! every answer in [`state::WizardState`] until the user confirms on the
//! final page. The actual TOML files are written in a single transaction
//! at that point (see [`finalize`]). A mid-flow exit (`Ctrl-C` / `Esc` on
//! the welcome page) persists the buffer to `.wizard_state.toml`; the next
//! `--setup` offers `[Resume]` from the welcome page.
//!
//! ## Architecture
//!
//! - [`descriptor`] — declarative schema each widget exports via its
//!   `WidgetDescriptor.wizard` field. The wizard's generic per-widget page
//!   is driven entirely by the returned [`descriptor::WizardDescriptor`].
//! - [`state`] — the in-flight buffer that survives across pages.
//! - [`storage`] — atomic save/load of the resume buffer.
//! - [`flow`] — pure sequencing functions (next/prev/start page).
//! - [`pages`] — one module per page; each is a stateless renderer +
//!   key handler driven by [`app::WizardApp`].
//! - [`finalize`] — commits the buffer to the real TOML files.
//! - [`app`] — owns the event loop and the transient TUI bookkeeping.

pub mod app;
pub mod descriptor;
pub mod finalize;
pub mod flow;
pub mod hydrate;
pub mod pages;
pub mod state;
pub mod storage;
pub mod style;
pub mod toml_merge;

use anyhow::Result;

/// Public entry point. Runs the wizard; returns `Ok(())` regardless of
/// whether the user completed or quit mid-flow (state file is preserved
/// across quits so the next call resumes).
pub fn run() -> Result<()> {
    let _ = app::run_wizard()?;
    Ok(())
}
