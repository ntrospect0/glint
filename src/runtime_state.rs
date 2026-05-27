// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Runtime state — tiny user-state file separate from config.toml.
//!
//! Holds things that change as the user *uses* glint (rather than
//! things the user explicitly *configures*). Today: which tab is
//! visible inside each stack. Lives at
//! `~/.config/glint/.runtime_state.toml` (dot-prefixed to keep it
//! out of casual `ls` listings alongside the user-authored TOMLs).
//!
//! Failures are non-fatal in both directions:
//! - **Load**: missing / unreadable / version-mismatched file → start
//!   with empty state (every stack defaults to tab 0).
//! - **Save**: error logged via `tracing::warn!`; the dashboard keeps
//!   running.
//!
//! See `docs/stack-spec.md` §5.

#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::config_dir;

/// Bump when the on-disk shape changes incompatibly. Old files are
/// silently discarded (no migration in v1).
pub const RUNTIME_STATE_VERSION: u32 = 1;

/// File name (relative to the config dir). Dot-prefixed.
pub const RUNTIME_STATE_FILENAME: &str = ".runtime_state.toml";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeState {
    #[serde(default = "default_version")]
    pub version: u32,
    /// Per-stack tab index, keyed by the stack's synthetic id
    /// (`stack:<child1>+<child2>+…`). Missing entries default to 0.
    #[serde(default)]
    pub stacks: HashMap<String, StackEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackEntry {
    pub active_tab: usize,
}

fn default_version() -> u32 {
    RUNTIME_STATE_VERSION
}

/// Absolute path to the state file under `$XDG_CONFIG_HOME/glint/`.
pub fn state_path() -> Result<PathBuf> {
    Ok(config_dir()?.join(RUNTIME_STATE_FILENAME))
}

/// Read the persisted runtime state. Returns the default (empty) on
/// missing file, unreadable file, version mismatch, or parse error —
/// never propagates to the caller.
pub fn load() -> RuntimeState {
    let path = match state_path() {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(error = %err, "could not resolve runtime-state path; using defaults");
            return RuntimeState::default();
        }
    };
    if !path.exists() {
        return RuntimeState::default();
    }
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(err) => {
            tracing::warn!(error = %err, "could not read runtime-state file; using defaults");
            return RuntimeState::default();
        }
    };
    match toml::from_str::<RuntimeState>(&text) {
        Ok(state) if state.version == RUNTIME_STATE_VERSION => state,
        Ok(state) => {
            tracing::info!(
                saw = state.version,
                expected = RUNTIME_STATE_VERSION,
                "runtime-state version mismatch; starting fresh"
            );
            RuntimeState::default()
        }
        Err(err) => {
            tracing::warn!(error = %err, "runtime-state parse failed; using defaults");
            RuntimeState::default()
        }
    }
}

/// Atomic write to `~/.config/glint/.runtime_state.toml`. Writes via
/// a sibling temp file + rename so a crash mid-write can't corrupt an
/// existing state file. Errors log + return — callers should not
/// abort on a failed save.
pub fn save(state: &RuntimeState) -> Result<()> {
    let path = state_path()?;
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("runtime-state path has no parent"))?;
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let body = toml::to_string_pretty(state).context("runtime-state serialize failed")?;
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| {
        format!("rename {} → {}", tmp.display(), path.display())
    })?;
    Ok(())
}

impl RuntimeState {
    /// Compact map view of just the (stack_id → active_tab) pairs.
    /// Used by the runtime-state-dirty check in `app.rs`.
    pub fn snapshot(&self) -> HashMap<String, usize> {
        self.stacks
            .iter()
            .map(|(id, entry)| (id.clone(), entry.active_tab))
            .collect()
    }

    /// Build a `RuntimeState` from a snapshot map (the inverse of
    /// `snapshot`). Used to construct the value passed to `save`.
    pub fn from_snapshot(snap: &HashMap<String, usize>) -> Self {
        Self {
            version: RUNTIME_STATE_VERSION,
            stacks: snap
                .iter()
                .map(|(id, active)| {
                    (
                        id.clone(),
                        StackEntry {
                            active_tab: *active,
                        },
                    )
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trips() {
        let mut state = RuntimeState::default();
        state.stacks.insert(
            "stack:a+b".into(),
            StackEntry { active_tab: 1 },
        );
        state.stacks.insert(
            "stack:c+d+e".into(),
            StackEntry { active_tab: 2 },
        );
        let snap = state.snapshot();
        assert_eq!(snap.get("stack:a+b"), Some(&1));
        assert_eq!(snap.get("stack:c+d+e"), Some(&2));
        let back = RuntimeState::from_snapshot(&snap);
        assert_eq!(back.stacks.len(), 2);
        assert_eq!(
            back.stacks.get("stack:a+b").map(|e| e.active_tab),
            Some(1)
        );
    }

    #[test]
    fn default_state_is_empty() {
        let s = RuntimeState::default();
        assert_eq!(s.version, 0);
        assert!(s.stacks.is_empty());
    }

    #[test]
    fn serde_round_trips() {
        let mut state = RuntimeState {
            version: RUNTIME_STATE_VERSION,
            stacks: HashMap::new(),
        };
        state.stacks.insert(
            "stack:clock+weather".into(),
            StackEntry { active_tab: 1 },
        );
        let text = toml::to_string_pretty(&state).unwrap();
        let parsed: RuntimeState = toml::from_str(&text).unwrap();
        assert_eq!(parsed.version, RUNTIME_STATE_VERSION);
        assert_eq!(parsed.stacks.len(), 1);
        assert_eq!(
            parsed
                .stacks
                .get("stack:clock+weather")
                .map(|e| e.active_tab),
            Some(1)
        );
    }
}
