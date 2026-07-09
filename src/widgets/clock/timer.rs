// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Timer mode for the clock widget: countdown phase machine, the
//! HH:MM:SS edit buffer, the alarm flash + edit-blink phase
//! functions, and the timer renderer + key handlers.
//!
//! `alarm_flash_on` and `edit_blink_on` are referenced from the
//! cross-mode tick state machine (`state.rs`) too, so they live
//! `pub(super)` rather than getting duplicated.

use std::time::{Duration, SystemTime};

use chrono::{DateTime, Local};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Alignment, Rect},
    style::Modifier,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::ui::big_digits;
use crate::widgets::ViewTier;

use super::stopwatch::format_hms;
use super::{ClockWidget, EventResult};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum EditField {
    #[default]
    Hours,
    Minutes,
    Seconds,
}

impl EditField {
    fn next(self) -> Self {
        match self {
            Self::Hours => Self::Minutes,
            Self::Minutes => Self::Seconds,
            Self::Seconds => Self::Hours,
        }
    }
    fn prev(self) -> Self {
        match self {
            Self::Hours => Self::Seconds,
            Self::Minutes => Self::Hours,
            Self::Seconds => Self::Minutes,
        }
    }
}

/// Timer phase machine. `duration` (carried on the parent `TimerState`)
/// is the last-committed countdown target; phase tracks where in the
/// run/pause/fired lifecycle we are.
#[derive(Debug, Clone)]
pub(super) enum TimerPhase {
    /// Set duration, ready to start. The default phase.
    Idle,
    /// User is editing the duration in the HH:MM:SS field grid.
    /// `prior` is the phase we restore to on Esc — only Idle or
    /// Paused, since Running/Fired get coerced through pause first.
    Editing { prior_phase: Box<TimerPhase> },
    /// Counting down; remaining = `end_at - now`.
    Running { end_at: SystemTime },
    /// Stopped mid-countdown.
    Paused { remaining: Duration },
    /// Reached zero and the alarm is firing. Stays here until
    /// the user acknowledges (Space / Enter / click on body).
    Fired { fired_at: SystemTime },
}

impl Default for TimerPhase {
    fn default() -> Self {
        Self::Idle
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct TimerEditBuffer {
    pub(super) hh: u8,
    pub(super) mm: u8,
    pub(super) ss: u8,
    pub(super) field: EditField,
    /// Counts digits typed in the current field for calculator-style
    /// auto-advance. Resets to 0 on field switch.
    digits_typed: u8,
}

impl TimerEditBuffer {
    fn from_duration(d: Duration) -> Self {
        let total = d.as_secs();
        Self {
            hh: ((total / 3600).min(99)) as u8,
            mm: ((total % 3600 / 60).min(59)) as u8,
            ss: ((total % 60).min(59)) as u8,
            field: EditField::Hours,
            digits_typed: 0,
        }
    }
    fn to_duration(&self) -> Duration {
        Duration::from_secs(
            self.hh as u64 * 3600 + self.mm as u64 * 60 + self.ss as u64,
        )
    }
    /// Inject one digit into the focused field. Semantics depend on
    /// how many digits have landed in the current field already:
    /// - **First digit** *replaces* the field. Without this, editing
    ///   a paused timer at 0:30:00 and typing `5` in the MM field
    ///   would give `mm = (30 * 10 + 5) % 100 = 5` (after a u8
    ///   overflow wraparound, no less) — confusing, since the user
    ///   was starting fresh in that field. Replace-on-first matches
    ///   what every calculator-style time picker does.
    /// - **Subsequent digits** shift-left, calculator-style.
    /// Auto-advances to the next field once two digits have landed.
    /// Arithmetic widens to `u16` so a seeded `hh = 95` plus a typed
    /// digit can't overflow.
    fn push_digit(&mut self, digit: u8) {
        let (max_val, current): (u8, u8) = match self.field {
            EditField::Hours => (99, self.hh),
            EditField::Minutes => (59, self.mm),
            EditField::Seconds => (59, self.ss),
        };
        let next: u8 = if self.digits_typed == 0 {
            digit
        } else {
            ((current as u16 * 10 + digit as u16) % 100) as u8
        };
        let clamped = next.min(max_val);
        match self.field {
            EditField::Hours => self.hh = clamped,
            EditField::Minutes => self.mm = clamped,
            EditField::Seconds => self.ss = clamped,
        }
        self.digits_typed = self.digits_typed.saturating_add(1);
        if self.digits_typed >= 2 {
            self.field = self.field.next();
            self.digits_typed = 0;
        }
    }
    /// Up/Down arrow: ±1 on the focused field, clamped. Does not
    /// affect digits_typed (so subsequent digit input restarts
    /// calculator entry on this field).
    fn bump(&mut self, delta: i32) {
        let (max_val, current) = match self.field {
            EditField::Hours => (99i32, self.hh as i32),
            EditField::Minutes => (59i32, self.mm as i32),
            EditField::Seconds => (59i32, self.ss as i32),
        };
        let next = (current + delta).clamp(0, max_val) as u8;
        match self.field {
            EditField::Hours => self.hh = next,
            EditField::Minutes => self.mm = next,
            EditField::Seconds => self.ss = next,
        }
        self.digits_typed = 0;
    }
    fn switch_field(&mut self, field: EditField) {
        self.field = field;
        self.digits_typed = 0;
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct TimerState {
    /// Last-committed duration. Persisted across restarts.
    pub(super) duration: Duration,
    pub(super) phase: TimerPhase,
    /// Edit buffer; only meaningful while `phase == Editing`.
    pub(super) edit: TimerEditBuffer,
}

impl TimerState {
    pub(super) fn is_editing(&self) -> bool {
        matches!(self.phase, TimerPhase::Editing { .. })
    }
    /// Remaining time on the countdown clock, used by render.
    /// Returns the committed duration when idle, the live remaining
    /// when running (clamped to zero on crossover), and the paused
    /// snapshot when paused.
    pub(super) fn display_remaining(&self) -> Duration {
        match &self.phase {
            TimerPhase::Idle => self.duration,
            TimerPhase::Editing { prior_phase } => match prior_phase.as_ref() {
                TimerPhase::Paused { remaining } => *remaining,
                _ => self.duration,
            },
            TimerPhase::Running { end_at } => SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .ok()
                .and_then(|_| end_at.duration_since(SystemTime::now()).ok())
                .unwrap_or(Duration::ZERO),
            TimerPhase::Paused { remaining } => *remaining,
            TimerPhase::Fired { .. } => Duration::ZERO,
        }
    }
}

// ─── alarm cadence constants ──────────────────────────────────────
//
// The alarm fires in bursts of 3 rapid visual+audio flips, then quiet,
// then repeat — chosen to be attention-grabbing without being
// distressing. Each burst writes 3 BEL chars to stdout (the terminal
// decides whether to beep / flash / dock-bounce) and toggles the big-
// digit color between two highlight styles. Tuning lives here so a
// future config-driven adjustment is a small refactor.

const ALARM_BEEPS_PER_BURST: u32 = 3;
// Flash flip rate is held at the app tick rate (250 ms). Going faster
// just means the per-tick render misses flips, so the visual flashes
// degrade to "one flicker per cycle." Matching the tick lets every
// flip land on a render.
const ALARM_FLASH_GAP: Duration = Duration::from_millis(250);
const ALARM_BURST_GAP: Duration = Duration::from_millis(1500);

/// Integer index of the alarm burst currently running. Stays the
/// same across the flips inside one burst, then increments at the
/// start of the next burst — `tick_mode_state` uses it to BEL once
/// per burst rather than once per flip.
pub(super) fn alarm_burst_index(fired_at: SystemTime) -> u128 {
    let elapsed = SystemTime::now()
        .duration_since(fired_at)
        .unwrap_or(Duration::ZERO);
    let cycle_period =
        ALARM_FLASH_GAP * ALARM_BEEPS_PER_BURST * 2 + ALARM_BURST_GAP;
    elapsed.as_nanos() / cycle_period.as_nanos()
}

/// Visual flash phase for the timer's Fired state, derived purely
/// from wall-clock elapsed since the alarm fired. No counter to
/// maintain in state: any tick that lands in the on-phase paints
/// the alarm highlight; off-phase ticks paint the resting style.
/// The pattern is `ALARM_BEEPS_PER_BURST` flips at `ALARM_FLASH_GAP`
/// spacing, then `ALARM_BURST_GAP` of quiet, repeated.
pub(super) fn alarm_flash_on(fired_at: SystemTime) -> bool {
    let elapsed = SystemTime::now()
        .duration_since(fired_at)
        .unwrap_or(Duration::ZERO);
    let burst_period =
        ALARM_FLASH_GAP * ALARM_BEEPS_PER_BURST * 2 + ALARM_BURST_GAP;
    let in_cycle = Duration::from_nanos((elapsed.as_nanos() % burst_period.as_nanos()) as u64);
    let burst_window = ALARM_FLASH_GAP * ALARM_BEEPS_PER_BURST * 2;
    if in_cycle >= burst_window {
        return false;
    }
    // Inside the burst window: alternate every ALARM_FLASH_GAP.
    let slot = (in_cycle.as_nanos() / ALARM_FLASH_GAP.as_nanos()) as u32;
    slot % 2 == 0
}

/// Edit-mode blink phase for the focused HH/MM/SS field. ~1 Hz
/// pulse (500 ms on, 500 ms off) keyed off the wall clock so any
/// number of clock widgets stay in lockstep. Returning a single bool
/// rather than tracking a counter means no state to manage —
/// `tick_mode_state` just observes flips and re-renders.
const EDIT_BLINK_HALF_PERIOD: Duration = Duration::from_millis(500);

pub(super) fn edit_blink_on() -> bool {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let half = EDIT_BLINK_HALF_PERIOD.as_nanos();
    if half == 0 {
        return true;
    }
    (nanos / half) % 2 == 0
}

impl ClockWidget {
    pub(super) fn render_timer_body(&self, frame: &mut Frame, inner: Rect, tier: ViewTier) {
        let (phase, remaining, edit_snapshot, gradient, duration) = {
            let st = self.state.lock().expect("clock state poisoned");
            (
                st.timer.phase.clone(),
                st.timer.display_remaining(),
                st.timer.edit.clone(),
                st.gradient,
                st.timer.duration,
            )
        };

        // Color/style key:
        //   Running           → lookup color   (text.selected)
        //   Paused / Idle     → home color     (text.focused)
        //   Editing           → home color, edit caret highlights focused field
        //   Fired (blinking)  → wall-clock-driven flip between text.selected
        //                       and text_shortcut for an alarm-y "alert" feel
        let big_style = match &phase {
            TimerPhase::Running { .. } => self.theme.text_selected,
            TimerPhase::Fired { fired_at } => {
                if alarm_flash_on(*fired_at) {
                    self.theme.text_shortcut
                } else {
                    self.theme.text_selected
                }
            }
            _ => self.theme.text_focused,
        };

        // Edit mode renders each HH / MM / SS field with its own
        // style so the focused field can pulse (call site below).
        // All other phases go through the normal styled-render path.
        let big_lines = if matches!(phase, TimerPhase::Editing { .. }) {
            self.render_timer_edit_digits(&edit_snapshot)
        } else {
            big_digits::render_styled(&format_hms(remaining), gradient, big_style)
        };

        let mut lines: Vec<Line<'_>> = Vec::new();
        lines.push(Line::from(""));
        for line in big_lines {
            lines.push(line);
        }

        // ── Full-tier: burn-down progress bar ──────────────────────────
        //
        // At Full, insert a horizontal progress bar + human-readable
        // detail line below the big digits and above the phase hints.
        // The bar shows elapsed fraction = (duration − remaining) /
        // duration, clipped to [0, 1]. Only shown for phases where the
        // fraction is meaningful (Running, Paused, Fired) and only when
        // duration > 0 to avoid division by zero.
        //
        // The "remaining" text in the detail row is the test-visible
        // signal that distinguishes Full-tier rendering from Standard:
        // standard/expanded hints do not include the word "remaining".
        if tier == ViewTier::Full
            && duration > Duration::ZERO
            && !matches!(phase, TimerPhase::Idle | TimerPhase::Editing { .. })
        {
            let elapsed_frac = {
                let elapsed_secs =
                    duration.as_secs_f64() - remaining.as_secs_f64();
                (elapsed_secs / duration.as_secs_f64()).clamp(0.0, 1.0)
            };
            const BAR_INNER: usize = 40;
            let filled = (elapsed_frac * BAR_INNER as f64).round() as usize;
            let empty = BAR_INNER - filled;
            let bar_str = format!(
                "[{}{}] {:3.0}%",
                "█".repeat(filled),
                "░".repeat(empty),
                elapsed_frac * 100.0,
            );
            let detail_str = format_timer_detail(&phase, remaining);
            if (lines.len() as u16) + 2 < inner.height {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    bar_str,
                    self.theme.text_focused,
                )));
                if (lines.len() as u16) < inner.height {
                    lines.push(Line::from(Span::styled(
                        detail_str,
                        self.theme.text_dim,
                    )));
                }
            }
        }

        // Phase-specific annotation line that lives right under the
        // big digits. Empty for Running / Idle / Fired so the layout
        // doesn't shift around at those transitions.
        // Running-phase footer has its own fixed layout (specced
        // per-line), so it's built inline below. All other phases use
        // the generic "optional label row, blank, optional spacer,
        // help hint" stack.
        if matches!(phase, TimerPhase::Running { .. }) {
            // Order per spec:
            //   <big digits already pushed>
            //   <blank>
            //   help message
            //   <blank>
            //   Starting time HH:MM:SS
            //   <blank>
            //   Timer alerts only while
            //   glint is running
            // Each row is pushed only when the pane is tall enough to
            // hold it — the running widget can render in a sliver
            // and still show the big digits.
            let dim = self.theme.text_dim;
            let italic = dim.add_modifier(Modifier::ITALIC);
            let rows: [(String, ratatui::style::Style); 7] = [
                (String::new(), dim),
                ("Space pause · r reset · e edit".to_string(), dim),
                (String::new(), dim),
                (format!("Starting time   {}", format_hms(duration)), dim),
                (String::new(), dim),
                ("Timer alerts only while".to_string(), italic),
                ("glint is running".to_string(), italic),
            ];
            for (text, style) in rows {
                if (lines.len() as u16) >= inner.height {
                    break;
                }
                if text.is_empty() {
                    lines.push(Line::from(""));
                } else {
                    lines.push(Line::from(Span::styled(text, style)));
                }
            }
        } else {
            //   Editing:  "HH    MM    SS" — labels aligned under
            //             the two-digit columns so the user can see
            //             which field is being edited without parsing
            //             the help line below.
            let label_line: Option<Line<'_>> = match &phase {
                TimerPhase::Editing { .. } => Some(self.timer_edit_field_labels()),
                _ => None,
            };
            if let Some(line) = label_line {
                lines.push(line);
            }

            // Footer hint. Spec calls for the digit-keys and
            // arrow-bump hints to be dropped — keyboard newcomers
            // usually try those intuitively, and the [HH] / [MM] /
            // [SS] tag is replaced by the live label row above.
            let hint = match &phase {
                TimerPhase::Idle => "Space start · e edit · r reset".to_string(),
                TimerPhase::Editing { .. } => {
                    "←/→/h/l field   ↵ set   Esc cancel".to_string()
                }
                TimerPhase::Running { .. } => unreachable!(),
                TimerPhase::Paused { remaining } => {
                    // "Paused at the configured duration" really
                    // means "not started yet" — Space will fire the
                    // first countdown, so the hint reads "start"
                    // rather than the mid-run "resume".
                    if *remaining == duration {
                        "Space start · r reset · e edit".to_string()
                    } else {
                        "Space resume · r reset · e edit".to_string()
                    }
                }
                TimerPhase::Fired { .. } => "Time's up! Space / Enter / click to dismiss".to_string(),
            };

            // Push a separator blank line before the hint when
            // there's a label line above it — gives visual breathing
            // room between the label row and the keyboard hint.
            let block_height_estimate = lines.len() as u16;
            if inner.height > block_height_estimate {
                lines.push(Line::from(""));
                if matches!(phase, TimerPhase::Editing { .. })
                    && inner.height > block_height_estimate + 1
                {
                    lines.push(Line::from(""));
                }
                let hint_style = if matches!(phase, TimerPhase::Fired { .. }) {
                    self.theme.text_shortcut
                } else {
                    self.theme.text_dim
                };
                lines.push(Line::from(Span::styled(hint, hint_style)));
            }
        }

        let body = Paragraph::new(lines).alignment(Alignment::Center);
        frame.render_widget(body, inner);
    }

    /// Build the `HH    MM    SS` label row that sits directly under
    /// the big-digit edit display. Spacing matches the big-digit cell
    /// columns (8 glyphs × 3 cells + 7 inter-glyph separators = 31
    /// cells per row) so each label centers under its 2-digit field.
    fn timer_edit_field_labels(&self) -> Line<'static> {
        // Big-digit HH:MM:SS column anchors:
        //   HH center cells: 3   (cols 2-3)
        //   MM center cells: 15  (cols 14-15)
        //   SS center cells: 27  (cols 26-27)
        // Width 31 = 2 pad + "HH" + 10 pad + "MM" + 10 pad + "SS" + 3 pad.
        let row = "  HH          MM          SS   ";
        Line::from(Span::styled(row.to_string(), self.theme.text_dim))
    }

    /// Build the big-digit HH:MM:SS lines for the timer's edit phase
    /// with per-field styling. The focused field pulses at ~1 Hz
    /// between the home style and a dim style so the user can see at
    /// a glance which pair of digits their next keystroke will edit.
    /// Non-focused fields and the colon separators render in the
    /// quiet home style.
    fn render_timer_edit_digits(&self, edit: &TimerEditBuffer) -> Vec<Line<'static>> {
        // Composite the 8-char string `HH:MM:SS` once, then walk it
        // character-by-character to assemble each glyph row. The
        // (idx → field) map below is the source of truth for which
        // glyphs belong to which field; colons (idx 2, 5) stay quiet
        // regardless of focus.
        let chars: Vec<char> = format!(
            "{:02}:{:02}:{:02}",
            edit.hh, edit.mm, edit.ss
        )
        .chars()
        .collect();
        let focused = edit.field;
        let blink_on = edit_blink_on();
        let quiet = self.theme.text_focused;
        let blink_off = self.theme.text_dim;
        let focused_style = if blink_on { quiet } else { blink_off };

        let mut lines: Vec<Line<'static>> = Vec::with_capacity(big_digits::GLYPH_HEIGHT);
        for row in 0..big_digits::GLYPH_HEIGHT {
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(chars.len() * 2);
            for (i, ch) in chars.iter().enumerate() {
                if i > 0 {
                    // Match big_digits' inter-glyph 1-cell spacer.
                    spans.push(Span::raw(" "));
                }
                let glyph_row = big_digits::glyph(*ch)
                    .map(|g| g[row].to_string())
                    .unwrap_or_default();
                let style = match i {
                    0 | 1 if focused == EditField::Hours => focused_style,
                    3 | 4 if focused == EditField::Minutes => focused_style,
                    6 | 7 if focused == EditField::Seconds => focused_style,
                    _ => quiet,
                };
                spans.push(Span::styled(glyph_row, style));
            }
            lines.push(Line::from(spans));
        }
        lines
    }

    pub(super) fn handle_key_timer_mode(&mut self, key: KeyEvent) -> EventResult {
        // Editing mode owns its own keymap (digits, arrows, Enter,
        // Esc, field nav). Check it first so digit keys etc. don't
        // collide with the run/pause keymap below.
        let (is_editing, current_phase) = {
            let st = self.state.lock().expect("clock state poisoned");
            (st.timer.is_editing(), st.timer.phase.clone())
        };
        if is_editing {
            return self.handle_key_timer_edit(key);
        }

        if key.modifiers != KeyModifiers::NONE {
            return EventResult::Ignored;
        }
        match key.code {
            // Space transitions through the run lifecycle: Idle/Paused
            // → Running; Running → Paused; Fired → Idle (acknowledge
            // the alarm). Refuses to start with a zero-duration timer
            // so the user has a chance to set one first.
            KeyCode::Char(' ') | KeyCode::Enter => {
                self.timer_space_or_enter(&current_phase);
                self.persist_clock_state();
                EventResult::Handled
            }
            // `e` opens the edit modal. Pauses a running timer first
            // (preserving its remaining) so the duration the user
            // edits is the natural "left over" reading at that
            // moment — matches kitchen-timer conventions.
            KeyCode::Char('e') => {
                self.timer_enter_edit(&current_phase);
                // No persist here — entering edit is a transient UI
                // state, not a checkpoint worth saving. We persist on
                // Enter (commit) or Esc (revert) instead.
                EventResult::Handled
            }
            // `r` resets to the last-committed duration in the
            // Paused phase. Active alarm gets acknowledged + reset in
            // one keystroke.
            KeyCode::Char('r') => {
                self.timer_reset();
                self.persist_clock_state();
                EventResult::Handled
            }
            _ => EventResult::Ignored,
        }
    }

    /// Space / Enter dispatch for the Timer mode (outside edit). Each
    /// phase has one outgoing transition; centralising the matrix
    /// here keeps the key handler small and makes the transitions
    /// easy to audit.
    fn timer_space_or_enter(&self, phase: &TimerPhase) {
        let mut st = self.state.lock().expect("clock state poisoned");
        match phase {
            TimerPhase::Idle => {
                if st.timer.duration == Duration::ZERO {
                    // Don't silently start a zero-length timer — it would
                    // fire on the same frame and confuse the user.
                    return;
                }
                let end_at = SystemTime::now() + st.timer.duration;
                st.timer.phase = TimerPhase::Running { end_at };
            }
            TimerPhase::Running { end_at } => {
                let remaining = end_at
                    .duration_since(SystemTime::now())
                    .unwrap_or(Duration::ZERO);
                st.timer.phase = TimerPhase::Paused { remaining };
            }
            TimerPhase::Paused { remaining } => {
                if *remaining == Duration::ZERO {
                    st.timer.phase = TimerPhase::Idle;
                    return;
                }
                let end_at = SystemTime::now() + *remaining;
                st.timer.phase = TimerPhase::Running { end_at };
            }
            TimerPhase::Fired { .. } => {
                // Acknowledge: back to Idle at the committed duration.
                st.timer.phase = TimerPhase::Idle;
            }
            TimerPhase::Editing { .. } => {} // handled by edit-mode dispatcher
        }
    }

    fn timer_enter_edit(&self, phase: &TimerPhase) {
        let mut st = self.state.lock().expect("clock state poisoned");
        // Snapshot the current remaining (or the committed duration
        // when there's no remaining-in-flight) into the edit buffer,
        // and remember what to revert to on Esc.
        let seed = match phase {
            TimerPhase::Running { end_at } => {
                let remaining = end_at
                    .duration_since(SystemTime::now())
                    .unwrap_or(Duration::ZERO);
                // Convert the in-flight Running into a Paused so Esc
                // restores a sensible state (you weren't "running" in
                // any meaningful sense while editing).
                let prior = TimerPhase::Paused { remaining };
                (remaining, prior)
            }
            TimerPhase::Paused { remaining } => {
                (*remaining, TimerPhase::Paused { remaining: *remaining })
            }
            TimerPhase::Fired { .. } | TimerPhase::Idle => {
                (st.timer.duration, TimerPhase::Idle)
            }
            TimerPhase::Editing { .. } => return, // already editing
        };
        st.timer.edit = TimerEditBuffer::from_duration(seed.0);
        st.timer.phase = TimerPhase::Editing {
            prior_phase: Box::new(seed.1),
        };
    }

    fn timer_reset(&self) {
        let mut st = self.state.lock().expect("clock state poisoned");
        let duration = st.timer.duration;
        st.timer.phase = if duration == Duration::ZERO {
            TimerPhase::Idle
        } else {
            TimerPhase::Paused {
                remaining: duration,
            }
        };
    }

    /// In-edit-mode keymap. Calculator-style digit entry +
    /// up/down bumps + tab/h/l/←/→ field nav, with Enter to commit
    /// and Esc to revert.
    fn handle_key_timer_edit(&self, key: KeyEvent) -> EventResult {
        // Tab is "shift-aware" via its dedicated key code; allow
        // SHIFT for shift-tab and bare keys otherwise.
        let bare = key.modifiers == KeyModifiers::NONE;
        let shifted = key.modifiers == KeyModifiers::SHIFT;
        if !bare && !shifted {
            return EventResult::Ignored;
        }
        match key.code {
            KeyCode::Esc if bare => {
                {
                    let mut st = self.state.lock().expect("clock state poisoned");
                    if let TimerPhase::Editing { prior_phase } = st.timer.phase.clone() {
                        st.timer.phase = *prior_phase;
                    }
                }
                // Persist the reverted phase so a quit-while-editing
                // doesn't leak an inconsistent "still editing" state
                // into the next launch.
                self.persist_clock_state();
                EventResult::Handled
            }
            KeyCode::Enter if bare => {
                {
                    let mut st = self.state.lock().expect("clock state poisoned");
                    let d = st.timer.edit.to_duration();
                    st.timer.duration = d;
                    st.timer.phase = if d == Duration::ZERO {
                        TimerPhase::Idle
                    } else {
                        TimerPhase::Paused { remaining: d }
                    };
                }
                // Persist the new duration + phase so it survives a
                // restart. Best-effort: the save helper logs on
                // failure and the in-memory state is already updated,
                // so a transient disk error doesn't break the user's
                // session.
                self.persist_clock_state();
                EventResult::Handled
            }
            KeyCode::Tab if bare => {
                let mut st = self.state.lock().expect("clock state poisoned");
                let next = st.timer.edit.field.next();
                st.timer.edit.switch_field(next);
                EventResult::Handled
            }
            KeyCode::BackTab if (bare || shifted) => {
                let mut st = self.state.lock().expect("clock state poisoned");
                let prev = st.timer.edit.field.prev();
                st.timer.edit.switch_field(prev);
                EventResult::Handled
            }
            KeyCode::Char('h') | KeyCode::Left if bare => {
                let mut st = self.state.lock().expect("clock state poisoned");
                let prev = st.timer.edit.field.prev();
                st.timer.edit.switch_field(prev);
                EventResult::Handled
            }
            KeyCode::Char('l') | KeyCode::Right if bare => {
                let mut st = self.state.lock().expect("clock state poisoned");
                let next = st.timer.edit.field.next();
                st.timer.edit.switch_field(next);
                EventResult::Handled
            }
            KeyCode::Up if bare => {
                let mut st = self.state.lock().expect("clock state poisoned");
                st.timer.edit.bump(1);
                EventResult::Handled
            }
            KeyCode::Down if bare => {
                let mut st = self.state.lock().expect("clock state poisoned");
                st.timer.edit.bump(-1);
                EventResult::Handled
            }
            KeyCode::Char(c) if bare && c.is_ascii_digit() => {
                let mut st = self.state.lock().expect("clock state poisoned");
                let digit = (c as u8) - b'0';
                st.timer.edit.push_digit(digit);
                EventResult::Handled
            }
            _ => EventResult::Ignored,
        }
    }
}

/// Build the human-readable remaining-time string for the Full-tier
/// burn-down bar's detail row. Format varies by phase:
/// - Running: "Xm Ys remaining · fires at HH:MM"
/// - Paused:  "Xm Ys remaining (paused)"
/// - Fired:   "Elapsed"
/// - Other phases are not reached (caller guards on Running/Paused/Fired).
fn format_timer_detail(phase: &TimerPhase, remaining: Duration) -> String {
    let rem = format_remaining_human(remaining);
    match phase {
        TimerPhase::Running { end_at } => {
            let fires_at = DateTime::<Local>::from(*end_at);
            format!("{rem} remaining · fires at {}", fires_at.format("%H:%M"))
        }
        TimerPhase::Paused { .. } => {
            format!("{rem} remaining (paused)")
        }
        TimerPhase::Fired { .. } => "Elapsed".to_string(),
        _ => String::new(),
    }
}

/// Format a `Duration` as a compact human-readable string suitable for
/// the burn-down bar's detail row. Examples: "42s", "5m 23s", "1h 05m".
fn format_remaining_human(d: Duration) -> String {
    let total = d.as_secs();
    if total >= 3600 {
        let h = total / 3600;
        let m = (total % 3600) / 60;
        format!("{h}h {m:02}m")
    } else if total >= 60 {
        let m = total / 60;
        let s = total % 60;
        format!("{m}m {s:02}s")
    } else {
        format!("{total}s")
    }
}
