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

#![allow(dead_code)] // wizard-finalize hook + entry types kept ahead of new persistence call sites.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::config_dir;

/// Bump when the on-disk shape changes incompatibly. Old files are
/// silently discarded (no migration). Version 2 added the
/// `stocks` / `forex` sections and the `clocks.mode` field; older
/// version-1 files are still parsed as an empty state because
/// `serde(default)` accepts the missing sections — but the version
/// check rejects them so we don't accidentally load a v1 file with
/// out-of-date schema assumptions baked in.
pub const RUNTIME_STATE_VERSION: u32 = 2;

/// File name (relative to the config dir). Dot-prefixed.
pub const RUNTIME_STATE_FILENAME: &str = ".runtime_state.toml";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeState {
    #[serde(default = "default_version")]
    pub version: u32,
    /// Per-stack tab index, keyed by the stack's synthetic id
    /// (`stack:<child1>+<child2>+…`). Missing entries default to 0.
    #[serde(default)]
    pub stacks: HashMap<String, StackEntry>,
    /// Per-clock-instance widget state — survives restart so a set
    /// timer duration isn't lost. Keyed by the widget id (`"clock"`,
    /// `"clock@home"`, …). Missing entries default to empty.
    #[serde(default)]
    pub clocks: HashMap<String, ClockEntry>,
    /// Per-stocks-instance widget state — keeps the user's selected
    /// ticker and active period across restarts. Keyed by the widget
    /// id (`"stocks"`, `"stocks@watch"`, …).
    #[serde(default)]
    pub stocks: HashMap<String, StocksEntry>,
    /// Per-forex-instance widget state — keeps the user's selected
    /// currency / crypto and active period across restarts. Keyed by
    /// the widget id (`"forex"`, `"forex@crypto"`, …).
    #[serde(default)]
    pub forex: HashMap<String, ForexEntry>,
    /// Per-notes-instance widget state — remembers which note the
    /// user was viewing so a relaunch reopens the same one rather
    /// than always landing on the most-recently-edited note. Keyed
    /// by the widget id (`"notes"`, `"notes@work"`, …).
    #[serde(default)]
    pub notes: HashMap<String, NotesEntry>,
}

// Manual `Default` rather than `derive` so a freshly-constructed
// `RuntimeState` already carries the current schema version. Without
// this, `RuntimeState::default()` produced `version: 0`, which
// `load()` then rejected as a version mismatch — round-tripping a
// blank state through disk would silently throw it away.
impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            version: RUNTIME_STATE_VERSION,
            stacks: HashMap::new(),
            clocks: HashMap::new(),
            stocks: HashMap::new(),
            forex: HashMap::new(),
            notes: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackEntry {
    pub active_tab: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClockEntry {
    /// Last-committed timer duration in whole seconds. `None` when
    /// the user has never set a timer for this clock instance.
    #[serde(default)]
    pub timer_duration_secs: Option<u64>,
    /// Stopwatch accumulated time in milliseconds (sum of prior
    /// start→stop runs). Restored as the paused value on next load.
    #[serde(default)]
    pub stopwatch_accumulated_ms: Option<u64>,
    /// Unix-epoch milliseconds at which the stopwatch was last
    /// started, if it was running when the app quit. On load the
    /// widget computes elapsed = accumulated + (now - started) and
    /// the stopwatch keeps ticking from where it left off.
    /// `None` = stopwatch was paused (or never started).
    #[serde(default)]
    pub stopwatch_started_at_unix_ms: Option<i64>,
    /// Unix-epoch milliseconds at which a running timer is scheduled
    /// to fire, if the timer was Running when the app quit. On load,
    /// if this time is in the future → Running; in the past → Fired.
    #[serde(default)]
    pub timer_running_end_unix_ms: Option<i64>,
    /// Remaining time in milliseconds for a paused timer. Mutually
    /// exclusive with `timer_running_end_unix_ms`.
    #[serde(default)]
    pub timer_paused_remaining_ms: Option<u64>,
    /// Recorded stopwatch lap times, in milliseconds, in the order
    /// the user pressed `l`. Cleared on stopwatch reset; preserved
    /// across stop/restart and app shutdown.
    #[serde(default)]
    pub stopwatch_laps_ms: Vec<u64>,
    /// Active mode at the time of last save (`"clock"`, `"stopwatch"`,
    /// `"timer"`). `None` falls back to the configured default mode
    /// on next launch. We persist as a string rather than the
    /// widget's `Mode` enum so the runtime-state file stays decoupled
    /// from the widget crate's type churn.
    #[serde(default)]
    pub mode: Option<String>,
    /// Big-digit gradient style at the time of last save. `None` falls back
    /// to the configured `gradient` in `clock.toml` on next launch, so the
    /// `g` cycle survives restarts without shadowing an unset config.
    #[serde(default)]
    pub gradient: Option<crate::ui::big_digits::Gradient>,
}

/// Per-stocks-instance persisted state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StocksEntry {
    /// Ticker the user had highlighted when glint exited
    /// (e.g. `"AAPL"`, `"^GSPC"`). `None` defaults to the first
    /// row on next launch. Restored only when the symbol is still
    /// in the configured indices / watchlist — drops the entry
    /// silently otherwise.
    #[serde(default)]
    pub selected_symbol: Option<String>,
    /// Active chart period label (`"1d"`, `"1w"`, `"1m"`, `"6m"`,
    /// `"ytd"`, `"1y"`, `"3y"`, `"5y"`, `"10y"`). Stored as a string
    /// so the file stays decoupled from the widget's `Period` enum;
    /// unknown values fall back to the configured `default_period`.
    #[serde(default)]
    pub period: Option<String>,
}

/// Per-notes-instance persisted state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NotesEntry {
    /// Stable note id (the on-disk filename stem) the user had open
    /// at exit. `None` when the store was empty. Restored only when
    /// a note with the same id still exists — otherwise the widget
    /// falls back to the most-recently-edited note (index 0).
    #[serde(default)]
    pub active_note_id: Option<String>,
}

/// Per-forex-instance persisted state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ForexEntry {
    /// Currency / crypto code the user had highlighted at exit
    /// (e.g. `"EUR"`, `"BTC"`, `"USD"`). `None` defaults to the
    /// primary on next launch. Restored only when the code is still
    /// in the configured watchlist / crypto_watchlist.
    #[serde(default)]
    pub selected_code: Option<String>,
    /// Active chart period label. Same shape as `StocksEntry.period`.
    #[serde(default)]
    pub period: Option<String>,
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

/// Remove `runtime_state.toml`. The wizard calls this at finalize so a
/// fresh layout doesn't inherit stack active-tab indices keyed by
/// IDs that no longer exist. Idempotent — missing file is success.
pub fn clear() -> Result<()> {
    let path = state_path()?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("remove {}", path.display())),
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
    fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} → {}", tmp.display(), path.display()))?;
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
            clocks: HashMap::new(),
            stocks: HashMap::new(),
            forex: HashMap::new(),
            notes: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_gradient_round_trips_through_toml() {
        // Every gradient must survive a serialize → deserialize cycle, so the
        // `g` toggle persists across restarts (regression: ClockEntry had no
        // gradient field and the choice was lost).
        for g in crate::ui::big_digits::Gradient::ALL {
            let entry = ClockEntry {
                gradient: Some(g),
                ..Default::default()
            };
            let toml = toml::to_string(&entry).unwrap();
            let back: ClockEntry = toml::from_str(&toml).unwrap();
            assert_eq!(back.gradient, Some(g), "gradient {g:?} did not round-trip");
        }
    }

    #[test]
    fn snapshot_round_trips() {
        let mut state = RuntimeState::default();
        state
            .stacks
            .insert("stack:a+b".into(), StackEntry { active_tab: 1 });
        state
            .stacks
            .insert("stack:c+d+e".into(), StackEntry { active_tab: 2 });
        let snap = state.snapshot();
        assert_eq!(snap.get("stack:a+b"), Some(&1));
        assert_eq!(snap.get("stack:c+d+e"), Some(&2));
        let back = RuntimeState::from_snapshot(&snap);
        assert_eq!(back.stacks.len(), 2);
        assert_eq!(back.stacks.get("stack:a+b").map(|e| e.active_tab), Some(1));
    }

    #[test]
    #[ignore = "mutates the process-wide XDG_CONFIG_HOME — opt in with --ignored"]
    fn clear_is_idempotent_when_file_missing() {
        // Clear must succeed even when there's no state file yet — the
        // wizard's post-finalize hook calls it unconditionally on every
        // run, including the first.
        let dir = std::env::temp_dir().join(format!(
            "glint-runtime-state-clear-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        // No file present → still Ok.
        assert!(clear().is_ok(), "clear on missing file must succeed");
        // Create one, clear it, verify it's gone.
        let mut state = RuntimeState::default();
        state
            .stacks
            .insert("stack:a+b".into(), StackEntry { active_tab: 1 });
        save(&state).unwrap();
        let path = state_path().unwrap();
        assert!(path.exists(), "save should have written the file");
        clear().unwrap();
        assert!(!path.exists(), "clear should have removed the file");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn default_state_carries_current_version_and_is_empty() {
        let s = RuntimeState::default();
        assert_eq!(s.version, RUNTIME_STATE_VERSION);
        assert!(s.stacks.is_empty());
        assert!(s.stocks.is_empty());
        assert!(s.forex.is_empty());
        assert!(s.clocks.is_empty());
        assert!(s.notes.is_empty());
    }

    #[test]
    fn notes_entry_round_trips_through_toml() {
        let mut state = RuntimeState::default();
        state.notes.insert(
            "notes".into(),
            NotesEntry {
                active_note_id: Some("note-1719847200000".into()),
            },
        );
        let text = toml::to_string_pretty(&state).unwrap();
        let parsed: RuntimeState = toml::from_str(&text).unwrap();
        assert_eq!(
            parsed
                .notes
                .get("notes")
                .and_then(|e| e.active_note_id.as_deref()),
            Some("note-1719847200000")
        );
    }

    #[test]
    fn stocks_and_forex_entries_round_trip_through_toml() {
        let mut state = RuntimeState::default();
        state.stocks.insert(
            "stocks".into(),
            StocksEntry {
                selected_symbol: Some("NVDA".into()),
                period: Some("1W".into()),
            },
        );
        state.forex.insert(
            "forex".into(),
            ForexEntry {
                selected_code: Some("EUR".into()),
                period: Some("1M".into()),
            },
        );
        let text = toml::to_string_pretty(&state).unwrap();
        let parsed: RuntimeState = toml::from_str(&text).unwrap();
        assert_eq!(
            parsed.stocks.get("stocks").and_then(|e| e.selected_symbol.as_deref()),
            Some("NVDA")
        );
        assert_eq!(
            parsed.stocks.get("stocks").and_then(|e| e.period.as_deref()),
            Some("1W")
        );
        assert_eq!(
            parsed.forex.get("forex").and_then(|e| e.selected_code.as_deref()),
            Some("EUR")
        );
        assert_eq!(
            parsed.forex.get("forex").and_then(|e| e.period.as_deref()),
            Some("1M")
        );
    }

    #[test]
    fn clock_mode_round_trips_through_toml() {
        let mut state = RuntimeState::default();
        state.clocks.insert(
            "clock".into(),
            ClockEntry {
                mode: Some("stopwatch".into()),
                ..Default::default()
            },
        );
        let text = toml::to_string_pretty(&state).unwrap();
        let parsed: RuntimeState = toml::from_str(&text).unwrap();
        assert_eq!(
            parsed.clocks.get("clock").and_then(|e| e.mode.as_deref()),
            Some("stopwatch")
        );
    }

    #[test]
    fn serde_round_trips() {
        let mut state = RuntimeState {
            version: RUNTIME_STATE_VERSION,
            stacks: HashMap::new(),
            clocks: HashMap::new(),
            stocks: HashMap::new(),
            forex: HashMap::new(),
            notes: HashMap::new(),
        };
        state
            .stacks
            .insert("stack:clock+weather".into(), StackEntry { active_tab: 1 });
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
