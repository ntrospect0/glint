// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Persistence for in-flight wizard state.
//!
//! The wizard buffers everything in [`WizardState`] until the user confirms
//! at the end. We serialise to `.wizard_state.toml` (under the config dir)
//! after every page advance so an unexpected exit can be resumed.
//!
//! This module owns the file layout, atomic write, and load semantics. On
//! mismatched schema versions we silently report "no resume" rather than
//! risking a stale state file fighting a real install.

#![allow(dead_code)] // consumed by the wizard driver.

use std::{fs, io::Write, path::PathBuf};

use anyhow::{Context, Result};

use crate::config;

use super::state::WizardState;

/// File name (relative to the config dir) holding the in-flight buffer.
/// Dot-prefixed so it doesn't appear in casual `ls` listings of the config
/// dir alongside the user's hand-edited TOMLs.
pub const STATE_FILENAME: &str = ".wizard_state.toml";

/// Schema version this build writes. Loaders compare against the version in
/// any state file they read and discard the file on mismatch.
pub const STATE_VERSION: u32 = 1;

/// Absolute path to the state file under `$XDG_CONFIG_HOME/glint/`.
pub fn state_path() -> Result<PathBuf> {
    Ok(config::config_dir()?.join(STATE_FILENAME))
}

/// `true` when a resume buffer exists on disk. Used by the welcome page to
/// decide whether to surface the `[Resume]` option.
pub fn state_exists() -> bool {
    state_path().map(|p| p.exists()).unwrap_or(false)
}

/// Load the buffered state. Returns `None` when there's no state file or
/// when the version is unrecognised — in either case the wizard starts
/// fresh rather than trying to recover from a stale snapshot.
pub fn load() -> Result<Option<WizardState>> {
    let path = state_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let state: WizardState = match toml::from_str(&contents) {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(
                file = %path.display(),
                error = %err,
                "wizard state file unreadable; starting fresh"
            );
            return Ok(None);
        }
    };
    if state.version != STATE_VERSION {
        tracing::info!(
            file = %path.display(),
            saw = state.version,
            expected = STATE_VERSION,
            "wizard state file is from a different version; starting fresh"
        );
        return Ok(None);
    }
    Ok(Some(state))
}

/// Atomically persist `state` to disk. The write goes to a sibling temp
/// file and renames in place so a crash mid-write can't corrupt an existing
/// buffer.
pub fn save(state: &WizardState) -> Result<()> {
    let path = state_path()?;
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("state path has no parent: {}", path.display()))?;
    fs::create_dir_all(dir)
        .with_context(|| format!("failed to create state dir {}", dir.display()))?;
    let serialised = toml::to_string_pretty(state).context("wizard state serialise failed")?;
    let tmp = path.with_extension("toml.tmp");
    {
        let mut f =
            fs::File::create(&tmp).with_context(|| format!("failed to open {}", tmp.display()))?;
        f.write_all(serialised.as_bytes())
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        f.sync_all().ok();
    }
    fs::rename(&tmp, &path).with_context(|| {
        format!("failed to rename {} → {}", tmp.display(), path.display())
    })?;
    Ok(())
}

/// Remove the state file. Called on successful Complete + Save so the next
/// `--setup` run starts cleanly. Missing files are not an error.
pub fn clear() -> Result<()> {
    let path = state_path()?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to remove {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wizard::descriptor::WizardValue;
    use crate::wizard::state::{CellAssignment, LayoutChoice};
    use std::sync::Mutex;

    /// XDG_CONFIG_HOME is process-wide, so the storage tests have to
    /// serialise. Cheap mutex; ignored tests, only opted-in.
    static XDG_LOCK: Mutex<()> = Mutex::new(());

    /// Override the config dir for the duration of the test by setting
    /// XDG_CONFIG_HOME. Returns a `(dir, guard)` pair — drop the guard at
    /// end of test to release the env var for the next test.
    fn with_xdg(test_name: &str) -> (PathBuf, std::sync::MutexGuard<'static, ()>) {
        let guard = XDG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "glint-wizard-storage-{}-{}-{}",
            test_name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        (dir, guard)
    }

    #[test]
    #[ignore = "mutates the process-wide XDG_CONFIG_HOME — opt in with --ignored"]
    fn save_load_round_trip() {
        let (_dir, _guard) = with_xdg("round_trip");
        let mut st = WizardState::default();
        st.global_set("theme", WizardValue::Choice("nord".into()));
        st.widget_set("clock", "show_seconds", WizardValue::Bool(true));
        st.layout = LayoutChoice::KeepExisting;
        st.assignments.push(CellAssignment {
            cell_index: 0,
            kind: "clock".into(),
            instance: "main".into(),
            stack_children: Vec::new(),
        });
        save(&st).unwrap();
        assert!(state_exists());
        let loaded = load().unwrap().expect("state should be present");
        assert_eq!(loaded.global, st.global);
        assert_eq!(loaded.widget_values, st.widget_values);
        assert_eq!(loaded.layout, st.layout);
        assert_eq!(loaded.assignments, st.assignments);
    }

    #[test]
    #[ignore = "mutates the process-wide XDG_CONFIG_HOME — opt in with --ignored"]
    fn load_returns_none_for_missing_file() {
        let (_dir, _guard) = with_xdg("missing");
        assert!(!state_exists());
        assert!(load().unwrap().is_none());
    }

    #[test]
    #[ignore = "mutates the process-wide XDG_CONFIG_HOME — opt in with --ignored"]
    fn load_returns_none_on_version_mismatch() {
        let (dir, _guard) = with_xdg("version_mismatch");
        std::fs::create_dir_all(dir.join("glint")).unwrap();
        let path = dir.join("glint").join(STATE_FILENAME);
        std::fs::write(&path, "version = 9999\n").unwrap();
        assert!(load().unwrap().is_none());
    }

    #[test]
    #[ignore = "mutates the process-wide XDG_CONFIG_HOME — opt in with --ignored"]
    fn clear_is_idempotent() {
        let (_dir, _guard) = with_xdg("clear_idempotent");
        // Clearing a non-existent state file is fine.
        clear().unwrap();
        save(&WizardState::default()).unwrap();
        clear().unwrap();
        assert!(!state_exists());
        clear().unwrap();
    }
}
