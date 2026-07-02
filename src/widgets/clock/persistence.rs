// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Runtime-state persistence for the clock widget.
//!
//! [`hydrate_state`] is called from `ClockWidget::with_config` to
//! seed the in-memory state from the on-disk runtime file (so a
//! relaunch picks up a running stopwatch, paused timer, last-active
//! mode, etc.). [`ClockWidget::persist_clock_state`] writes the same
//! shape back out on every state-changing keystroke.
//!
//! The two `unix_ms` helpers carry timestamps across the
//! `SystemTime` / `i64` boundary that the runtime-state file uses
//! (chrono doesn't expose `SystemTime` directly; we deal in millis).

use std::time::{Duration, SystemTime};

use super::state::{ClockState, Mode};
use super::stopwatch::MAX_LAPS;
use super::timer::TimerPhase;
use super::ClockWidget;

/// Convert a Unix-epoch millisecond timestamp to a `SystemTime`.
/// Handles negative inputs (pre-1970) by subtracting from EPOCH; in
/// practice all callers pass positive values.
fn unix_ms_to_systemtime(ms: i64) -> SystemTime {
    if ms >= 0 {
        SystemTime::UNIX_EPOCH + Duration::from_millis(ms as u64)
    } else {
        SystemTime::UNIX_EPOCH - Duration::from_millis((-ms) as u64)
    }
}

/// Convert a `SystemTime` to Unix-epoch milliseconds. Negative result
/// indicates pre-1970 (won't happen in practice for app state).
fn systemtime_to_unix_ms(t: SystemTime) -> i64 {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    }
}

/// Seed a fresh [`ClockState`] from the persisted entry for this
/// widget id. Preserves the user's stopwatch / timer progress across
/// quit/restart so a running stopwatch keeps ticking and a configured
/// timer doesn't have to be retyped. Called from
/// `ClockWidget::with_config` after `ClockState::default()` has
/// applied config-derived seeds (gradient, etc.).
pub(super) fn hydrate_state(state: &mut ClockState, id: &str) {
    let persisted = crate::runtime_state::load();
    let Some(entry) = persisted.clocks.get(id) else {
        return;
    };

    // ── Stopwatch ──
    // Reconstruct accumulated + started_at from the persisted
    // millisecond fields. A `Some(started_at_ms)` means the
    // stopwatch was running when we quit — elapsed picks up
    // from `now`.
    state.stopwatch.accumulated =
        Duration::from_millis(entry.stopwatch_accumulated_ms.unwrap_or(0));
    if let Some(ms) = entry.stopwatch_started_at_unix_ms {
        state.stopwatch.started_at = Some(unix_ms_to_systemtime(ms));
    }
    // Restore lap times. Clamped to MAX_LAPS in case a
    // future code path produced a longer list (defensive).
    state.stopwatch.laps = entry
        .stopwatch_laps_ms
        .iter()
        .take(MAX_LAPS)
        .map(|ms| Duration::from_millis(*ms))
        .collect();
    // Sentinel: "scroll to the end of the list on first
    // render." The first `render_stopwatch_body` call clamps
    // this against the actual pane-height-derived max so the
    // latest lap lands in view rather than being scrolled
    // past. Without this seed we'd default to scroll=0 and
    // the user would have to manually page down after every
    // restart to find their most recent split.
    state.laps_scroll = state.stopwatch.laps.len();

    // ── Timer ──
    // Three-way priority: committed duration → paused remaining → running end.
    if let Some(secs) = entry.timer_duration_secs {
        state.timer.duration = Duration::from_secs(secs);
    }
    if let Some(end_ms) = entry.timer_running_end_unix_ms {
        let end_at = unix_ms_to_systemtime(end_ms);
        state.timer.phase = if SystemTime::now() >= end_at {
            // We were running and the deadline already passed
            // while glint was shut down — restore directly
            // into Fired so the alarm fires on first tick.
            TimerPhase::Fired { fired_at: end_at }
        } else {
            TimerPhase::Running { end_at }
        };
    } else if let Some(remaining_ms) = entry.timer_paused_remaining_ms {
        state.timer.phase = TimerPhase::Paused {
            remaining: Duration::from_millis(remaining_ms),
        };
    } else if let Some(secs) = entry.timer_duration_secs {
        if secs > 0 {
            state.timer.phase = TimerPhase::Paused {
                remaining: Duration::from_secs(secs),
            };
        }
    }
    // Restore last-active mode so a relaunch lands the user
    // back on the view they were using (Clock / Stopwatch /
    // Timer). Unknown values fall back to `Mode::default()`,
    // which `ClockState::default()` already set.
    if let Some(mode_key) = entry.mode.as_deref() {
        if let Some(m) = Mode::from_persist_key(mode_key) {
            state.mode = m;
        }
    }
    // Restore the big-digit gradient chosen via `g`. Absent (never toggled)
    // ⇒ keep the config-seeded value already in `state`.
    if let Some(gradient) = entry.gradient {
        state.gradient = gradient;
    }
}

impl ClockWidget {
    /// Snapshot the *full* in-memory stopwatch+timer state into the
    /// runtime-state file. Called from every key handler that changes
    /// stopwatch or timer state, so quitting at any moment preserves
    /// progress and a restart picks up where we left off (running
    /// stopwatches keep ticking, paused timers stay paused, etc.).
    /// Stack tab indices live in the same file; load-modify-save
    /// round-trips them so other widgets' persisted state isn't
    /// wiped on each clock-state save.
    pub(super) fn persist_clock_state(&self) {
        let mut state = crate::runtime_state::load();
        let entry = state.clocks.entry(self.id.clone()).or_default();

        let st = self.state.lock().expect("clock state poisoned");

        // Timer duration is independent of phase and persists either
        // way — that's the value the user typed and `r` resets to.
        entry.timer_duration_secs = if st.timer.duration == Duration::ZERO {
            None
        } else {
            Some(st.timer.duration.as_secs())
        };

        // Timer phase — only Running and Paused carry resumable
        // state. Idle/Editing/Fired all reduce to "no live timer";
        // Fired is treated as Idle so a long-quit doesn't immediately
        // re-fire the alarm on next launch (the user can press Space
        // to restart it).
        entry.timer_running_end_unix_ms = None;
        entry.timer_paused_remaining_ms = None;
        match &st.timer.phase {
            TimerPhase::Running { end_at } => {
                entry.timer_running_end_unix_ms = Some(systemtime_to_unix_ms(*end_at));
            }
            TimerPhase::Paused { remaining } => {
                entry.timer_paused_remaining_ms = Some(remaining.as_millis() as u64);
            }
            _ => {}
        }

        // Stopwatch — accumulated always, started_at iff running.
        entry.stopwatch_accumulated_ms = if st.stopwatch.accumulated == Duration::ZERO
            && st.stopwatch.started_at.is_none()
        {
            None
        } else {
            Some(st.stopwatch.accumulated.as_millis() as u64)
        };
        entry.stopwatch_started_at_unix_ms = st
            .stopwatch
            .started_at
            .map(systemtime_to_unix_ms);

        // Persist the recorded laps so they survive a restart for
        // the duration of this stopwatch session (i.e. until the
        // user presses `r`, which clears them in memory and the
        // empty vec then overwrites the persisted list).
        entry.stopwatch_laps_ms = st
            .stopwatch
            .laps
            .iter()
            .map(|d| d.as_millis() as u64)
            .collect();

        // Active mode — restored on next launch so the user lands
        // back on the Clock / Stopwatch / Timer view they were
        // using.
        entry.mode = Some(st.mode.persist_key().to_string());

        // Big-digit gradient style cycled with `g`.
        entry.gradient = Some(st.gradient);

        drop(st);
        if let Err(err) = crate::runtime_state::save(&state) {
            tracing::warn!(error = %err, "failed to persist clock state");
        }
    }

    /// Rewrite the `[[secondary_timezones]]` blocks in this
    /// instance's clock.toml to match `self.config.secondary_timezones`.
    /// Strips every existing entry and re-emits the current list so
    /// add/remove from the world-clock UI round-trips through disk
    /// — comments and unrelated scalars are preserved by the merge
    /// helper.
    pub(super) fn persist_secondary_timezones(&self) {
        use std::fmt::Write as _;
        let stem = crate::widgets::widget_config_stem(super::config::KIND, &self.instance);
        let path = match crate::config::config_dir() {
            Ok(d) => d.join(format!("{stem}.toml")),
            Err(err) => {
                tracing::warn!(error = %err, "clock: could not resolve config dir");
                return;
            }
        };
        let original = if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(err) => {
                    tracing::warn!(error = %err, "clock: failed to read {}", path.display());
                    return;
                }
            }
        } else {
            String::new()
        };
        // Strip the existing entries and append the fresh list. The
        // merge helper preserves comments + sibling scalars so users
        // who hand-edited the file keep their notes.
        let mut updated =
            crate::wizard::toml_merge::strip_array_of_tables_blocks(&original, "secondary_timezones");
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        for sz in &self.config.secondary_timezones {
            if !updated.is_empty() && !updated.ends_with("\n\n") {
                updated.push('\n');
            }
            let _ = writeln!(updated, "[[secondary_timezones]]");
            let _ = writeln!(updated, "label = {}", toml_quote(&sz.label));
            let _ = writeln!(updated, "timezone = {}", toml_quote(&sz.timezone));
        }
        if let Some(parent) = path.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                tracing::warn!(error = %err, "clock: failed to mkdir {}", parent.display());
                return;
            }
        }
        let tmp = path.with_extension("toml.tmp");
        if let Err(err) = std::fs::write(&tmp, &updated) {
            tracing::warn!(error = %err, "clock: failed to write {}", tmp.display());
            return;
        }
        if let Err(err) = std::fs::rename(&tmp, &path) {
            tracing::warn!(error = %err, "clock: failed to rename into place at {}", path.display());
        }
    }
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
