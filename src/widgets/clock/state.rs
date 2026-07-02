// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Cross-mode state machinery for the clock widget.
//!
//! Owns the [`Mode`] selector (Clock / Stopwatch / Timer), the
//! [`ClockState`] mutex-protected struct (which holds the per-mode
//! slices plus dirty-tracking caches), and the three methods that
//! touch all three modes at once: the bottom tab strip, the mode
//! switch path, and the per-tick state advance.
//!
//! Per-mode rendering + key handling lives in `stopwatch.rs`,
//! `timer.rs`, and `clock_view.rs` — those files just consume the
//! relevant slice of `ClockState` through the mutex.

use std::time::{Duration, SystemTime};

use chrono_tz::Tz;
use crossterm::event::{KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Alignment, Rect},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::ui::big_digits;

use super::stopwatch::StopwatchState;
use super::timer::{alarm_burst_index, alarm_flash_on, edit_blink_on, TimerPhase, TimerState};
use super::{ClockWidget, EventResult};

// ─── mode infrastructure ──────────────────────────────────────────
//
// The clock widget hosts three modes — plain Clock, Stopwatch, and
// Timer — selected via a 1-row tab strip pinned to the bottom of the
// widget. Keyboard `c` / `s` / `t` and a left-click on the tab strip
// both switch modes. The big-digit display, color treatment, and
// per-mode keybindings change with the active mode; everything else
// (border, title row, app-level shortcut letter) stays consistent.
//
// The tab strip is a one-row footer (1 cell from the bottom of
// `inner`), modeled after Calendar's Day/Week/Month strip. When the
// pane is too short to spare a row for it, mode tabs hide and the
// body uses the full inner area — matching the rest of the widget's
// "graceful degrade at narrow sizes" pattern.

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Clock,
    Stopwatch,
    Timer,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Self::Clock => "Clock",
            Self::Stopwatch => "Stopwatch",
            Self::Timer => "Timer",
        }
    }
    fn all() -> &'static [Mode] {
        &[Mode::Clock, Mode::Stopwatch, Mode::Timer]
    }

    /// Lowercase persistence key for the runtime-state file.
    pub(super) fn persist_key(self) -> &'static str {
        match self {
            Self::Clock => "clock",
            Self::Stopwatch => "stopwatch",
            Self::Timer => "timer",
        }
    }

    /// Inverse of `persist_key`. Case-insensitive so the file can
    /// be hand-edited without surprises. Unknown values yield `None`
    /// and the caller falls back to the default.
    pub(super) fn from_persist_key(key: &str) -> Option<Self> {
        match key.to_ascii_lowercase().as_str() {
            "clock" => Some(Self::Clock),
            "stopwatch" => Some(Self::Stopwatch),
            "timer" => Some(Self::Timer),
            _ => None,
        }
    }
}

#[derive(Default)]
pub(super) struct ClockState {
    /// Override pinned by `:time <location>`. When Some, the big-digit display
    /// renders in that timezone and is tinted purple to make the override
    /// state unmistakable.
    /// Triple of `(full label, short city, tz)`. The full label
    /// (`"<city>, <admin>, <country>"`) feeds the title-bar metadata
    /// row; the short city ("Tokyo", "Washington, D.C.") feeds the
    /// world-clocks list so very long admin/country chains don't
    /// push the time column off screen. The city portion preserves
    /// any embedded commas the geocoder returned.
    pub(super) transient_tz: Option<(String, String, Tz)>,
    /// True while a `:time <location>` geocoding request is in flight.
    pub(super) transient_searching: bool,
    /// Currently active big-digit gradient. Seeded from config at startup; the
    /// user can cycle through variants by pressing `g`.
    pub(super) gradient: big_digits::Gradient,
    /// First-visible world-clock index when the cell is too short to show the
    /// whole list. ↑/↓ and mouse-wheel adjust this; render clamps it against
    /// `world_clock_max_scroll` so handlers don't need to know the cell size.
    pub(super) world_clock_scroll: usize,
    /// Largest valid value for `world_clock_scroll` given the most recent
    /// render's available height. Cached here so the key/mouse handlers can
    /// clamp without re-deriving the layout. `0` when the full list fits (or
    /// when the world-clocks block isn't shown at all).
    pub(super) world_clock_max_scroll: usize,
    /// Currently selected world-clock row (absolute index into the
    /// `world_clock_entries` list). `None` = no selection visible —
    /// the first j/k press lands the cursor on the first secondary
    /// row; subsequent presses move it. Cleared when the widget
    /// loses focus.
    pub(super) world_clock_selected: Option<usize>,
    /// `(display_label, iana_tz)` for the secondary timezone the
    /// user has asked to remove. Set when `-` is pressed on a valid
    /// row; cleared on confirm/cancel. Render branches on this to
    /// show the confirm modal.
    pub(super) confirm_remove: Option<(String, String)>,
    /// Currently visible mode (Clock/Stopwatch/Timer). Switched via c/s/t
    /// or by clicking the bottom tab strip.
    pub(super) mode: Mode,
    pub(super) stopwatch: StopwatchState,
    pub(super) timer: TimerState,
    /// Per-tab `(label, abs_x_start, abs_x_end_exclusive, abs_y)`
    /// hit-test rects captured by `render_mode_tabs` so a left-click
    /// in the bottom strip routes to the right mode.
    pub(super) mode_tab_rects: Vec<(Mode, u16, u16, u16)>,
    /// Set true the frame after the timer alarm fires; the next
    /// `take_focus_request` poll drains it. Decouples "user observed
    /// the alarm" from "widget got promoted to the front" so the
    /// promotion happens exactly once.
    pub(super) pending_focus_grab: bool,
    /// Last observed alarm-flash polarity. Lets the tick path detect
    /// flips without storing a counter (the polarity is purely a
    /// function of `fired_at` + wall clock; we just need to remember
    /// what we last rendered). `None` = we're not in the Fired phase.
    pub(super) last_alarm_phase: Option<bool>,
    /// Index of the alarm burst we last beeped on. Bursts are
    /// detected by integer-dividing elapsed-since-fired by the cycle
    /// period — independent of how often the tick samples the flash
    /// phase, so we emit beeps reliably even when the visual flip
    /// rate is finer than the tick rate.
    pub(super) last_alarm_burst_index: Option<u128>,
    /// Last observed edit-mode blink phase (true = "on" half-second).
    /// Used by `tick_mode_state` to mark dirty on each blink flip so
    /// the focused field's visual pulse actually paints. `None` when
    /// not in edit mode.
    pub(super) last_edit_blink: Option<bool>,
    /// Last whole-second elapsed value the stopwatch was rendered at.
    /// Stopwatch redraws are anchored to *elapsed* seconds crossing
    /// a boundary, not to wall-clock seconds — that keeps the
    /// HH:MM:SS display ticking at a steady 1 Hz cadence regardless
    /// of when the user pressed Space. `None` when not running.
    pub(super) last_stopwatch_secs: Option<u64>,
    /// Last whole-second remaining value the timer was rendered at.
    /// Same idea as `last_stopwatch_secs`: the display refresh is
    /// driven by the remaining countdown crossing a second boundary,
    /// not by wall-clock seconds. `None` when not running.
    pub(super) last_timer_secs: Option<u64>,
    /// First-visible lap index in the stopwatch lap list. ↑/↓/j/k
    /// and mouse-wheel adjust this; render clamps it against
    /// `last_laps_max_scroll` so key handlers don't need to know the
    /// pane size.
    pub(super) laps_scroll: usize,
    /// Highest valid `laps_scroll` given the current laps list +
    /// pane height. Cached by render so the key handler can clamp
    /// without re-deriving the layout. 0 when everything fits.
    pub(super) last_laps_max_scroll: usize,
}

impl ClockWidget {
    pub(super) fn render_mode_tabs(&self, frame: &mut Frame, area: Rect, active: Mode) {
        // Build the label list first so we know the total rendered
        // width before laying out hit-test rects. We render the strip
        // center-aligned (looks balanced under the big-digit body),
        // and a left-anchored accumulator would put the clickable
        // ranges to the LEFT of where the labels actually paint —
        // exactly the bug a user would see as "tabs seem clickable
        // but their click locations are way off to the left of where
        // they ought to be". Compute the centered start column once,
        // then walk from there.
        //
        // Labels render lowercase. On *inactive* tabs the first
        // letter takes the scheme's selection-highlight style
        // (`theme.text_selected`) to surface the c/s/t keyboard
        // shortcuts (the rest of the label stays text_dim). On the
        // *active* tab the whole label runs in text_selected and the
        // shortcut accent blends in — the hint is redundant when the
        // user is already there. Pulling from the scheme means user
        // theme overrides flow through; selection-highlight is also
        // visually distinct from the `text_shortcut` red used for the
        // app-level `Shift+<letter>` widget-focus shortcuts.
        const SEP: &str = "  ";
        let shortcut_style = self.theme.text_selected;
        let labels: Vec<String> = Mode::all()
            .iter()
            .map(|m| m.label().to_ascii_lowercase())
            .collect();
        let total_width: u16 = labels
            .iter()
            .map(|l| l.chars().count() as u16 + 2)
            .sum::<u16>()
            + (labels.len().saturating_sub(1) as u16) * SEP.len() as u16;
        let start_x = area
            .x
            .saturating_add(area.width.saturating_sub(total_width) / 2);

        let mut spans: Vec<Span> = Vec::with_capacity(Mode::all().len() * 4 + 2);
        let mut hits: Vec<(Mode, u16, u16, u16)> = Vec::with_capacity(Mode::all().len());
        let mut x = start_x;
        for (i, (mode, label)) in Mode::all().iter().zip(labels.iter()).enumerate() {
            if i > 0 {
                spans.push(Span::raw(SEP));
                x = x.saturating_add(SEP.len() as u16);
            }
            let is_active = *mode == active;
            let base = if is_active {
                self.theme.text_selected
            } else {
                self.theme.text_dim
            };
            // Drop the yellow shortcut accent on the active tab — when
            // a tab is selected its whole label runs in text_selected.
            let first_style = if is_active { base } else { shortcut_style };
            let tab_w = label.chars().count() as u16 + 2;
            spans.push(Span::styled("[", base));
            let mut chars = label.chars();
            if let Some(first) = chars.next() {
                spans.push(Span::styled(first.to_string(), first_style));
            }
            let rest: String = chars.collect();
            if !rest.is_empty() {
                spans.push(Span::styled(rest, base));
            }
            spans.push(Span::styled("]", base));
            hits.push((*mode, x, x.saturating_add(tab_w), area.y));
            x = x.saturating_add(tab_w);
        }
        self.state.lock().expect("clock state poisoned").mode_tab_rects = hits;
        frame.render_widget(
            Paragraph::new(Line::from(spans)).alignment(Alignment::Center),
            area,
        );
    }

    pub(super) fn switch_mode(&self, target: Mode) {
        let mode_changed = {
            let mut st = self.state.lock().expect("clock state poisoned");
            // Bailing out of Timer mode while editing implicitly
            // cancels the edit — restore the phase the user was in
            // before they pressed `e`. Without this, switching to
            // Clock/Stopwatch and back would leave the timer stuck in
            // edit mode showing a stale buffer. Same as pressing Esc.
            if st.mode == Mode::Timer && target != Mode::Timer {
                if let TimerPhase::Editing { prior_phase } = st.timer.phase.clone() {
                    st.timer.phase = *prior_phase;
                }
            }
            let prev = st.mode;
            st.mode = target;
            prev != target
        };
        if mode_changed {
            self.persist_clock_state();
        }
    }

    /// Per-tick state advance for Stopwatch / Timer. Returns
    /// `(needs_dirty, emit_bel)`:
    ///   - `needs_dirty` is true when the visible state changed in a
    ///     way the second-change check above didn't already catch
    ///     (e.g. a timer crossed into Fired, or an alarm flash flipped
    ///     polarity).
    ///   - `emit_bel` is true on each on-phase flip of the alarm so
    ///     the audio cadence matches the visual one.
    /// Caller holds no state lock during this call; this method takes
    /// it internally.
    pub(super) fn tick_mode_state(&self, second_changed: bool) -> (bool, bool) {
        let mut st = self.state.lock().expect("clock state poisoned");

        // Clock view ticker line refreshes once per wall-clock second.
        // Only paid for in Clock mode — Stopwatch / Timer don't show
        // wall-clock time, so wasting a render per second there is
        // exactly the bug the prior version had.
        let clock_dirty = st.mode == Mode::Clock && second_changed;

        // Stopwatch: redraw is anchored to the *elapsed*-time second
        // boundary rather than the wall-clock second. Without this,
        // a stopwatch started off-boundary stutters — `00` shows for
        // up to ~1.75 s before "catching up". Checking the elapsed
        // integer-seconds value on each tick and marking dirty only
        // when it changes gives a steady 1 Hz refresh anchored to
        // start_at, with the display lagging by at most one tick
        // (~250 ms).
        let stopwatch_dirty = if st.mode == Mode::Stopwatch && st.stopwatch.running() {
            let secs = st.stopwatch.elapsed().as_secs();
            let changed = st.last_stopwatch_secs != Some(secs);
            st.last_stopwatch_secs = Some(secs);
            changed
        } else {
            // Reset cache so a future start triggers a redraw on the
            // first tick instead of comparing against a stale value.
            st.last_stopwatch_secs = None;
            false
        };

        // Timer: same idea — drive dirty off the *remaining*-time
        // second boundary so the countdown ticks evenly regardless of
        // when the user pressed Space. Plus we still need to detect
        // Running → Fired crossover and the alarm-flash polarity.
        let mut emit_bel = false;
        let mut timer_dirty = false;
        match st.timer.phase.clone() {
            TimerPhase::Running { end_at } => {
                let now = SystemTime::now();
                if now >= end_at {
                    st.timer.phase = TimerPhase::Fired { fired_at: now };
                    st.pending_focus_grab = true;
                    st.last_timer_secs = None;
                    timer_dirty = true;
                    emit_bel = true;
                } else if st.mode == Mode::Timer {
                    let remaining = end_at.duration_since(now).unwrap_or(Duration::ZERO);
                    let secs = remaining.as_secs();
                    if st.last_timer_secs != Some(secs) {
                        st.last_timer_secs = Some(secs);
                        timer_dirty = true;
                    }
                }
            }
            TimerPhase::Fired { fired_at } => {
                // Reset countdown cache so a future Reset/restart
                // starts fresh.
                st.last_timer_secs = None;
                // Always redraw during the alarm so each visual flip
                // lands on a frame. With the flash-gap matched to the
                // tick rate every flip is naturally covered, but the
                // unconditional dirty also forwards through any
                // upstream skip-frame optimization safely.
                timer_dirty = true;
                let on_now = alarm_flash_on(fired_at);
                st.last_alarm_phase = Some(on_now);
                // Beep cadence is driven by *burst entry*, not by
                // every flash flip. One beep-emission per burst, with
                // 3 BEL chars packed into a single write so the
                // terminal plays them as a rapid triplet.
                let burst_index = alarm_burst_index(fired_at);
                if st.last_alarm_burst_index != Some(burst_index) {
                    st.last_alarm_burst_index = Some(burst_index);
                    emit_bel = true;
                }
            }
            TimerPhase::Editing { .. } => {
                // Edit-mode blink. Track the wall-clock-anchored phase
                // and mark dirty on each flip so the focused HH/MM/SS
                // field's visual pulse actually paints. We also clear
                // any leftover Fired / Running tracking caches so a
                // future restart starts fresh.
                st.last_alarm_phase = None;
                st.last_alarm_burst_index = None;
                st.last_timer_secs = None;
                let on_now = edit_blink_on();
                if st.last_edit_blink != Some(on_now) {
                    st.last_edit_blink = Some(on_now);
                    timer_dirty = true;
                }
            }
            _ => {
                st.last_alarm_phase = None;
                st.last_alarm_burst_index = None;
                st.last_timer_secs = None;
                st.last_edit_blink = None;
            }
        }

        (clock_dirty || stopwatch_dirty || timer_dirty, emit_bel)
    }

    pub(super) fn handle_key_clock_mode(&mut self, key: KeyEvent) -> EventResult {
        if key.modifiers != KeyModifiers::NONE && key.modifiers != KeyModifiers::SHIFT {
            return EventResult::Ignored;
        }
        match key.code {
            crossterm::event::KeyCode::Char('x') => {
                self.clear_transient();
                EventResult::Handled
            }
            crossterm::event::KeyCode::Char('g') => {
                {
                    let mut st = self.state.lock().expect("clock state poisoned");
                    st.gradient = st.gradient.next();
                }
                // Write the choice back to clock.toml so it survives a restart
                // (config stays authoritative — no runtime-state shadowing).
                self.persist_gradient();
                EventResult::Handled
            }
            // Up / Down arrows and vim-style k / j move the
            // selection cursor across secondary world-clock rows.
            // First press lands on the first secondary; clamps at
            // edges. Mouse wheel still raw-scrolls the list, so a
            // user who just wants to peek at hidden rows without
            // engaging a selection has that option.
            crossterm::event::KeyCode::Up | crossterm::event::KeyCode::Char('k') => {
                self.move_world_clock_selection(-1)
            }
            crossterm::event::KeyCode::Down | crossterm::event::KeyCode::Char('j') => {
                self.move_world_clock_selection(1)
            }
            // `-` while a secondary row is selected prompts for
            // confirmation of removal; rejected silently when no
            // row is selected (or the selected entry isn't a
            // secondary — primary / transient rows can't be
            // removed here, see clock_view::request_remove_selected).
            crossterm::event::KeyCode::Char('-') => self.request_remove_selected(),
            // `+` adds the active `:time`/`:clock` lookup to the
            // permanent secondary list and persists. No-op when no
            // lookup is active.
            crossterm::event::KeyCode::Char('+') => self.add_transient_to_world_clocks(),
            // Esc drops the selection cursor — the user's "I'm
            // done navigating" signal. Falls through (Ignored) when
            // no row is selected so Esc remains available to
            // higher-level handlers (e.g. command bar dismissal).
            crossterm::event::KeyCode::Esc => {
                let mut st = self.state.lock().expect("clock state poisoned");
                if st.world_clock_selected.is_some() {
                    st.world_clock_selected = None;
                    EventResult::Handled
                } else {
                    EventResult::Ignored
                }
            }
            _ => EventResult::Ignored,
        }
    }
}
