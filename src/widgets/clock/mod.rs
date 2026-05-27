// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Datelike, Local, TimeZone, Timelike};
use chrono_tz::Tz;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};
use serde::Deserialize;

use crate::theme::{ColorScheme, Theme};
use crate::ui::{apply_title_row, big_digits, MetadataEmphasis};

use super::{AppContext, EventResult, FocusRequest, Widget};

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
}

// ─── stopwatch state ──────────────────────────────────────────────

/// Stopwatch model: closed-form elapsed = `accumulated` + (now -
/// `started_at` when running). Storing the wall-clock start instant
/// rather than a per-tick counter means a running stopwatch stays
/// accurate even when the widget isn't being redrawn (the widget is
/// stack-hidden, the terminal was backgrounded, etc.). Persisting
/// across restarts works the same way — `SystemTime` survives a
/// serde round-trip; an `Instant` would not.
/// Hard cap on recorded laps. Past this the `l` key no-ops — keeps
/// the list bounded for persistence + render cost and matches
/// kitchen-stopwatch convention.
const MAX_LAPS: usize = 99;

#[derive(Debug, Clone, Default)]
struct StopwatchState {
    /// Time accrued in prior runs (start→stop→start cycles add up
    /// here). Reset to zero on `r`.
    accumulated: Duration,
    /// `Some(start)` when running; `None` when paused/stopped.
    started_at: Option<SystemTime>,
    /// Lap markers captured by `l`. Each is the *total elapsed*
    /// reading at the moment the user pressed `l` (not a delta from
    /// the previous lap) — matches how physical stopwatches display
    /// laps as cumulative timestamps. Cleared on `r`; preserved on
    /// stop, restart, and across app shutdown.
    laps: Vec<Duration>,
}

impl StopwatchState {
    fn elapsed(&self) -> Duration {
        match self.started_at {
            Some(start) => {
                self.accumulated
                    + SystemTime::now()
                        .duration_since(start)
                        .unwrap_or(Duration::ZERO)
            }
            None => self.accumulated,
        }
    }
    fn running(&self) -> bool {
        self.started_at.is_some()
    }
    fn toggle(&mut self) {
        match self.started_at {
            Some(start) => {
                // Stopping: roll the live span into accumulated and
                // null out the start instant.
                self.accumulated += SystemTime::now()
                    .duration_since(start)
                    .unwrap_or(Duration::ZERO);
                self.started_at = None;
            }
            None => {
                self.started_at = Some(SystemTime::now());
            }
        }
    }
    /// `r`: zero out elapsed and drop all recorded laps. Preserves
    /// the running flag — if the stopwatch was running it keeps
    /// running from 00:00:00, per spec. Reset is the *only* path
    /// that clears laps; stop, restart, and app shutdown all
    /// preserve them.
    fn reset(&mut self) {
        self.accumulated = Duration::ZERO;
        self.laps.clear();
        if self.started_at.is_some() {
            self.started_at = Some(SystemTime::now());
        }
    }

    /// Record a lap at the current elapsed reading. Returns `true`
    /// when accepted; `false` when the stopwatch isn't running or
    /// the per-session cap is reached.
    fn record_lap(&mut self) -> bool {
        if !self.running() || self.laps.len() >= MAX_LAPS {
            return false;
        }
        self.laps.push(self.elapsed());
        true
    }
}

// ─── timer state ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum EditField {
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
enum TimerPhase {
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
struct TimerEditBuffer {
    hh: u8,
    mm: u8,
    ss: u8,
    field: EditField,
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
struct TimerState {
    /// Last-committed duration. Persisted across restarts.
    duration: Duration,
    phase: TimerPhase,
    /// Edit buffer; only meaningful while `phase == Editing`.
    edit: TimerEditBuffer,
}

impl TimerState {
    fn is_editing(&self) -> bool {
        matches!(self.phase, TimerPhase::Editing { .. })
    }
    /// Remaining time on the countdown clock, used by render.
    /// Returns the committed duration when idle, the live remaining
    /// when running (clamped to zero on crossover), and the paused
    /// snapshot when paused.
    fn display_remaining(&self) -> Duration {
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

/// Loaded from `~/.config/glint/clock.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct ClockConfig {
    /// IANA timezone for the primary clock. `None` = system local time.
    #[serde(default)]
    pub timezone: Option<String>,

    #[serde(default)]
    pub show_seconds: bool,

    /// Small ticking `HH:MM:SS` line below the big digits.
    #[serde(default = "default_show_seconds_ticker")]
    pub show_seconds_ticker: bool,

    #[serde(default = "default_show_date")]
    pub show_date: bool,

    /// `"12h"` or `"24h"`.
    #[serde(
        default = "default_hour_format",
        deserialize_with = "deserialize_hour_format"
    )]
    pub hour_format: u8,

    /// World clocks rendered below the primary display when the cell is tall enough.
    #[serde(default)]
    pub secondary_timezones: Vec<SecondaryTimezone>,

    /// Big-digit gradient style. `g` cycles at runtime.
    #[serde(default)]
    pub gradient: big_digits::Gradient,

    /// Per-widget overrides layered on the app theme.
    #[serde(default)]
    pub colors: ColorScheme,

    /// `Shift+<letter>` focus shortcuts; falls back to `['c', 'l', 'o', 'k']`.
    #[serde(default)]
    pub shortcuts: Vec<char>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SecondaryTimezone {
    pub label: String,
    /// IANA timezone identifier (e.g. `"America/New_York"`).
    pub timezone: String,
}

fn default_show_seconds_ticker() -> bool {
    true
}
fn default_show_date() -> bool {
    true
}
fn default_hour_format() -> u8 {
    24
}

/// Parse `"12h"` / `"24h"` into the corresponding integer.
fn deserialize_hour_format<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let s = String::deserialize(deserializer)?;
    match s.trim().to_lowercase().as_str() {
        "12h" => Ok(12),
        "24h" => Ok(24),
        other => Err(D::Error::custom(format!(
            "unknown hour_format {other:?}, expected \"12h\" or \"24h\""
        ))),
    }
}

impl Default for ClockConfig {
    fn default() -> Self {
        Self {
            timezone: None,
            show_seconds: false,
            show_seconds_ticker: default_show_seconds_ticker(),
            show_date: default_show_date(),
            hour_format: default_hour_format(),
            secondary_timezones: Vec::new(),
            gradient: big_digits::Gradient::default(),
            colors: ColorScheme::default(),
            shortcuts: Vec::new(),
        }
    }
}

#[derive(Default)]
struct ClockState {
    /// Override pinned by `:time <location>`. When Some, the big-digit display
    /// renders in that timezone and is tinted purple to make the override
    /// state unmistakable.
    transient_tz: Option<(String, Tz)>,
    /// True while a `:time <location>` geocoding request is in flight.
    transient_searching: bool,
    /// Currently active big-digit gradient. Seeded from config at startup; the
    /// user can cycle through variants by pressing `g`.
    gradient: big_digits::Gradient,
    /// First-visible world-clock index when the cell is too short to show the
    /// whole list. ↑/↓ and mouse-wheel adjust this; render clamps it against
    /// `world_clock_max_scroll` so handlers don't need to know the cell size.
    world_clock_scroll: usize,
    /// Largest valid value for `world_clock_scroll` given the most recent
    /// render's available height. Cached here so the key/mouse handlers can
    /// clamp without re-deriving the layout. `0` when the full list fits (or
    /// when the world-clocks block isn't shown at all).
    world_clock_max_scroll: usize,
    /// Currently visible mode (Clock/Stopwatch/Timer). Switched via c/s/t
    /// or by clicking the bottom tab strip.
    mode: Mode,
    stopwatch: StopwatchState,
    timer: TimerState,
    /// Per-tab `(label, abs_x_start, abs_x_end_exclusive, abs_y)`
    /// hit-test rects captured by `render_mode_tabs` so a left-click
    /// in the bottom strip routes to the right mode.
    mode_tab_rects: Vec<(Mode, u16, u16, u16)>,
    /// Set true the frame after the timer alarm fires; the next
    /// `take_focus_request` poll drains it. Decouples "user observed
    /// the alarm" from "widget got promoted to the front" so the
    /// promotion happens exactly once.
    pending_focus_grab: bool,
    /// Last observed alarm-flash polarity. Lets the tick path detect
    /// flips without storing a counter (the polarity is purely a
    /// function of `fired_at` + wall clock; we just need to remember
    /// what we last rendered). `None` = we're not in the Fired phase.
    last_alarm_phase: Option<bool>,
    /// Index of the alarm burst we last beeped on. Bursts are
    /// detected by integer-dividing elapsed-since-fired by the cycle
    /// period — independent of how often the tick samples the flash
    /// phase, so we emit beeps reliably even when the visual flip
    /// rate is finer than the tick rate.
    last_alarm_burst_index: Option<u128>,
    /// Last observed edit-mode blink phase (true = "on" half-second).
    /// Used by `tick_mode_state` to mark dirty on each blink flip so
    /// the focused field's visual pulse actually paints. `None` when
    /// not in edit mode.
    last_edit_blink: Option<bool>,
    /// Last whole-second elapsed value the stopwatch was rendered at.
    /// Stopwatch redraws are anchored to *elapsed* seconds crossing
    /// a boundary, not to wall-clock seconds — that keeps the
    /// HH:MM:SS display ticking at a steady 1 Hz cadence regardless
    /// of when the user pressed Space. `None` when not running.
    last_stopwatch_secs: Option<u64>,
    /// Last whole-second remaining value the timer was rendered at.
    /// Same idea as `last_stopwatch_secs`: the display refresh is
    /// driven by the remaining countdown crossing a second boundary,
    /// not by wall-clock seconds. `None` when not running.
    last_timer_secs: Option<u64>,
    /// First-visible lap index in the stopwatch lap list. ↑/↓/j/k
    /// and mouse-wheel adjust this; render clamps it against
    /// `last_laps_max_scroll` so key handlers don't need to know the
    /// pane size.
    laps_scroll: usize,
    /// Highest valid `laps_scroll` given the current laps list +
    /// pane height. Cached by render so the key handler can clamp
    /// without re-deriving the layout. 0 when everything fits.
    last_laps_max_scroll: usize,
}

pub struct ClockWidget {
    id: String,
    instance: String,
    /// Cached `Clock` / `Clock (instance)` label so `display_name()` can
    /// return a `&str` without per-call allocation.
    display_name_cache: String,
    config: ClockConfig,
    tz: Option<Tz>,
    /// Parsed secondary timezones — entries with invalid IANA names get dropped
    /// at construction time and a warning logged.
    secondaries: Vec<(String, Tz)>,
    state: Arc<Mutex<ClockState>>,
    /// App-level theme; kept so live config reloads can rebuild `theme`
    /// from updated `colors` overrides.
    app_theme: Arc<Theme>,
    /// Merged theme (app + widget overrides). Rebuilt on `apply_config`.
    theme: Theme,
    /// Letter assigned by the app for `Shift+<letter>` focus, painted in
    /// the title via `text.shortcut`. `None` = no shortcut claimed.
    shortcut: Option<char>,
    /// Effective shortcut preference list (TOML override or built-in).
    shortcut_prefs: Vec<char>,
    /// Last whole-second the display was confirmed against — the ticker
    /// row prints seconds even when `show_seconds = false`, so the
    /// display changes at 1Hz and `take_dirty` needs to detect that
    /// without redrawing on every 250ms tick.
    last_tick_second: Option<i64>,
    /// Display-state dirty flag — see Widget::take_dirty. True at
    /// construction so the initial render lands.
    dirty: bool,
}

impl Default for ClockWidget {
    fn default() -> Self {
        Self::with_config(
            "main".to_string(),
            ClockConfig::default(),
            Arc::new(Theme::builtin_defaults()),
        )
    }
}

impl ClockWidget {
    pub fn with_config(instance: String, config: ClockConfig, app_theme: Arc<Theme>) -> Self {
        let tz = config
            .timezone
            .as_deref()
            .and_then(|name| name.parse::<Tz>().ok());
        let mut secondaries = Vec::with_capacity(config.secondary_timezones.len());
        for st in &config.secondary_timezones {
            match st.timezone.parse::<Tz>() {
                Ok(t) => secondaries.push((st.label.clone(), t)),
                Err(_) => {
                    tracing::warn!(label = %st.label, timezone = %st.timezone, "invalid IANA timezone, skipping");
                }
            }
        }
        let id = if instance == "main" {
            "clock".to_string()
        } else {
            format!("clock@{instance}")
        };

        // Seed mutable state from runtime_state — preserves the
        // user's timer/stopwatch progress across quit/restart so a
        // running stopwatch keeps ticking and a configured timer
        // doesn't have to be retyped. Looked up by widget id so
        // `clock` and `clock@home` keep independent values.
        let persisted = crate::runtime_state::load();
        let mut state = ClockState {
            gradient: config.gradient,
            ..ClockState::default()
        };
        if let Some(entry) = persisted.clocks.get(&id) {
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
        }
        let theme = app_theme.with_overrides(&config.colors);
        let shortcut_prefs = if config.shortcuts.is_empty() {
            vec!['c', 'l', 'o', 'k']
        } else {
            config.shortcuts.clone()
        };
        let display_name_cache = if instance == "main" {
            "Clock".to_string()
        } else {
            format!("Clock ({instance})")
        };
        Self {
            id,
            instance,
            display_name_cache,
            config,
            tz,
            secondaries,
            state: Arc::new(Mutex::new(state)),
            app_theme,
            theme,
            shortcut: None,
            shortcut_prefs,
            last_tick_second: None,
            dirty: true,
        }
    }

    fn snapshot_transient(&self) -> (Option<(String, Tz)>, bool) {
        let st = self.state.lock().expect("clock state poisoned");
        (st.transient_tz.clone(), st.transient_searching)
    }

    /// Effective primary timezone — transient override beats configured tz
    /// beats system local.
    fn effective_tz(&self) -> Option<Tz> {
        self.state
            .lock()
            .expect("clock state poisoned")
            .transient_tz
            .as_ref()
            .map(|(_, tz)| *tz)
            .or(self.tz)
    }

    fn lookup_location(&self, query: &str) {
        {
            let mut st = self.state.lock().expect("clock state poisoned");
            st.transient_searching = true;
            // Setting an override prepends Local + the override onto the
            // world-clocks list, so any prior scroll offset no longer points
            // at the same entry — reset to the top for predictability.
            st.world_clock_scroll = 0;
        }
        let state = self.state.clone();
        let query = query.to_string();
        tokio::spawn(async move {
            let result = crate::geolocation::by_name(&query).await;
            let mut st = state.lock().expect("clock state poisoned");
            st.transient_searching = false;
            match result {
                Ok(loc) => {
                    let Some(tz_name) = loc.timezone.as_deref() else {
                        tracing::warn!(query = %query, "geocoding succeeded but returned no timezone");
                        return;
                    };
                    match tz_name.parse::<Tz>() {
                        Ok(tz) => {
                            st.transient_tz = Some((loc.label.clone(), tz));
                        }
                        Err(_) => {
                            tracing::warn!(query = %query, tz = %tz_name, "unrecognized IANA timezone");
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(query = %query, error = %err, "clock geocoding failed");
                }
            }
        });
    }

    fn clear_transient(&self) {
        let mut st = self.state.lock().expect("clock state poisoned");
        st.transient_tz = None;
        // Same reasoning as `lookup_location` — the list shape changes back,
        // so reset the offset rather than leave it pointing somewhere stale.
        st.world_clock_scroll = 0;
    }

    /// Move the world-clocks view by `delta` rows (negative = up). Returns
    /// `Handled` only when scrolling is actually possible — when the full
    /// list already fits, ↑/↓ and mouse-wheel fall through so the event can
    /// reach a higher-level handler.
    fn scroll_world_clocks(&self, delta: i32) -> EventResult {
        let mut st = self.state.lock().expect("clock state poisoned");
        if st.world_clock_max_scroll == 0 {
            return EventResult::Ignored;
        }
        let max = st.world_clock_max_scroll;
        let next = (st.world_clock_scroll as i32 + delta).clamp(0, max as i32);
        st.world_clock_scroll = next as usize;
        EventResult::Handled
    }

    /// Returns (HH:MM[:SS], AM/PM, date) for the effective primary timezone.
    fn render_strings(&self, now_utc: DateTime<chrono::Utc>) -> (String, String, String) {
        match self.effective_tz() {
            Some(tz) => self.format_parts(now_utc.with_timezone(&tz)),
            None => self.format_parts(now_utc.with_timezone(&Local)),
        }
    }

    fn format_parts<T: TimeZone>(&self, dt: DateTime<T>) -> (String, String, String)
    where
        T::Offset: std::fmt::Display,
    {
        let (hour_disp, ampm) = if self.config.hour_format == 12 {
            let h = dt.hour();
            let (h12, suffix) = match h {
                0 => (12, "AM"),
                1..=11 => (h, "AM"),
                12 => (12, "PM"),
                _ => (h - 12, "PM"),
            };
            (h12, suffix.to_string())
        } else {
            (dt.hour(), String::new())
        };

        let time = if self.config.show_seconds {
            format!("{:02}:{:02}:{:02}", hour_disp, dt.minute(), dt.second())
        } else {
            format!("{:02}:{:02}", hour_disp, dt.minute())
        };

        let date = if self.config.show_date {
            format!(
                "{} {} {}, {}",
                weekday_name(dt.weekday()),
                month_name(dt.month()),
                dt.day(),
                dt.year()
            )
        } else {
            String::new()
        };

        (time, ampm, date)
    }

    fn ticker_string(&self, now_utc: DateTime<chrono::Utc>) -> String {
        match self.effective_tz() {
            Some(tz) => format_ticker(now_utc.with_timezone(&tz), self.config.hour_format),
            None => format_ticker(now_utc.with_timezone(&Local), self.config.hour_format),
        }
    }

    /// Returns (label, "HH:MM Wkd Mon DD") pairs for the World Clocks block.
    /// Primary timezone leads, then any configured secondaries. Each entry
    /// carries its own local date so the user can tell when a clock is on a
    /// different calendar day than local time without having to do timezone
    /// arithmetic in their head.
    fn world_clock_entries(&self) -> Vec<(String, String)> {
        let now = chrono::Utc::now();
        let mut out: Vec<(String, String)> = Vec::with_capacity(self.secondaries.len() + 2);
        let transient = self
            .state
            .lock()
            .expect("clock state poisoned")
            .transient_tz
            .clone();

        // When a `:time <location>` override is active the big-digit display
        // is showing that override, so pin Local to the top of the World
        // Clocks list — otherwise the user has no easy way to see their
        // actual local time at a glance.
        if transient.is_some() {
            let local_now = now.with_timezone(&Local);
            out.push(("Local".to_string(), format_clock_entry(&local_now)));
        }

        let (primary_label, primary_str) = match transient {
            Some((label, tz)) => {
                let t = now.with_timezone(&tz);
                (label, format_clock_entry(&t))
            }
            None => match self.tz {
                Some(tz) => {
                    let t = now.with_timezone(&tz);
                    (city_from_tz_name(tz.name()), format_clock_entry(&t))
                }
                None => {
                    let t = now.with_timezone(&Local);
                    ("Local".to_string(), format_clock_entry(&t))
                }
            },
        };
        out.push((primary_label, primary_str));
        for (label, tz) in &self.secondaries {
            let t = now.with_timezone(tz);
            out.push((label.clone(), format_clock_entry(&t)));
        }
        out
    }
}

fn format_clock_entry<T: TimeZone>(t: &DateTime<T>) -> String
where
    T::Offset: std::fmt::Display,
{
    format!(
        "{} {:02}:{:02} {} {} {}",
        day_night_icon(t.hour()),
        t.hour(),
        t.minute(),
        weekday_name(t.weekday()),
        month_name(t.month()),
        t.day()
    )
}

/// Simple day/night marker keyed off local hour-of-day. Use 06:00–17:59 as
/// "day"; outside that window is "night". Not astronomically accurate but
/// good enough as a glance signal alongside the time.
fn day_night_icon(hour: u32) -> &'static str {
    if (6..=17).contains(&hour) {
        "☀"
    } else {
        "☾"
    }
}

/// Convert an IANA timezone name like "America/Vancouver" into a friendly
/// label ("Vancouver"). Underscores become spaces.
fn city_from_tz_name(name: &str) -> String {
    name.rsplit('/').next().unwrap_or(name).replace('_', " ")
}

/// Format a `Duration` as `HH:MM:SS`, hours capped to 99 so the big-
/// digit renderer always sees a 2-character hour field. A 100h+
/// stopwatch saturates at "99:59:59" rather than blowing the layout.
fn format_hms(d: Duration) -> String {
    let total = d.as_secs();
    let h = (total / 3600).min(99);
    let m = (total % 3600) / 60;
    let s = total % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

/// `HH:MM:SS.mmm` — same hours-cap policy as `format_hms`, plus a
/// zero-padded millisecond suffix for fixed-width row alignment.
/// Used by the stopwatch lap list where sub-second precision is the
/// whole point of recording.
fn format_hms_ms(d: Duration) -> String {
    let total = d.as_secs();
    let h = (total / 3600).min(99);
    let m = (total % 3600) / 60;
    let s = total % 60;
    let ms = d.subsec_millis();
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

/// Visual flash phase for the timer's Fired state, derived purely
/// from wall-clock elapsed since the alarm fired. No counter to
/// maintain in state: any tick that lands in the on-phase paints
/// the alarm highlight; off-phase ticks paint the resting style.
/// The pattern is `ALARM_BEEPS_PER_BURST` flips at `ALARM_FLASH_GAP`
/// spacing, then `ALARM_BURST_GAP` of quiet, repeated.
fn alarm_flash_on(fired_at: SystemTime) -> bool {
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

/// Edit-mode blink phase for the focused HH/MM/SS field. ~1 Hz
/// pulse (500 ms on, 500 ms off) keyed off the wall clock so any
/// number of clock widgets stay in lockstep. Returning a single bool
/// rather than tracking a counter means no state to manage — `tick_mode_state`
/// just observes flips and re-renders.
const EDIT_BLINK_HALF_PERIOD: Duration = Duration::from_millis(500);

fn edit_blink_on() -> bool {
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

fn format_ticker<T: TimeZone>(t: DateTime<T>, hour_format: u8) -> String
where
    T::Offset: std::fmt::Display,
{
    let hour = t.hour();
    if hour_format == 12 {
        let (h12, suffix) = match hour {
            0 => (12, "AM"),
            1..=11 => (hour, "AM"),
            12 => (12, "PM"),
            _ => (hour - 12, "PM"),
        };
        format!("{:02}:{:02}:{:02} {}", h12, t.minute(), t.second(), suffix)
    } else {
        format!("{:02}:{:02}:{:02}", hour, t.minute(), t.second())
    }
}

fn weekday_name(w: chrono::Weekday) -> &'static str {
    use chrono::Weekday::*;
    match w {
        Mon => "Mon",
        Tue => "Tue",
        Wed => "Wed",
        Thu => "Thu",
        Fri => "Fri",
        Sat => "Sat",
        Sun => "Sun",
    }
}

fn month_name(m: u32) -> &'static str {
    match m {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => "???",
    }
}

#[async_trait]
impl Widget for ClockWidget {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> &str {
        "clock"
    }

    fn instance(&self) -> &str {
        &self.instance
    }

    fn display_name(&self) -> &str {
        &self.display_name_cache
    }

    async fn update(&mut self, _ctx: &AppContext) -> Result<()> {
        let now_secs = chrono::Utc::now().timestamp();
        let second_changed = self.last_tick_second != Some(now_secs);
        if second_changed {
            self.last_tick_second = Some(now_secs);
        }

        // Single state-lock for the per-tick work — combining the
        // mode check, stopwatch/timer phase advance, and dirty
        // signaling halves the per-tick lock count at idle (one lock
        // instead of two). `tick_mode_state` already covers running
        // counters and the alarm flash; here we just need to ALSO
        // refresh on a wall-clock second change when the visible
        // surface depends on it (the Clock view's ticker line; no
        // other mode does).
        let (need_dirty, emit_bel) = self.tick_mode_state(second_changed);
        if need_dirty {
            self.dirty = true;
        }
        if emit_bel {
            // Three BEL chars packed into one write — terminals that
            // honor the bell will play three quick beeps; terminals
            // that dedupe back-to-back BEL will collapse them into
            // one. Best-effort either way; the visual flash carries
            // the attention load regardless of what the terminal
            // chooses to do with the audio.
            use std::io::Write;
            let _ = std::io::stdout().write_all(b"\x07\x07\x07");
            let _ = std::io::stdout().flush();
        }
        Ok(())
    }

    fn take_dirty(&mut self) -> bool {
        std::mem::replace(&mut self.dirty, false)
    }

    fn take_focus_request(&mut self) -> Option<FocusRequest> {
        let mut st = self.state.lock().expect("clock state poisoned");
        if std::mem::replace(&mut st.pending_focus_grab, false) {
            Some(FocusRequest {
                widget_id: self.id.clone(),
            })
        } else {
            None
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, focused: bool) {
        let (transient, searching) = self.snapshot_transient();
        let mode = self.state.lock().expect("clock state poisoned").mode;
        let base = if self.instance == "main" {
            "Clock".to_string()
        } else {
            format!("Clock ({})", self.instance)
        };
        // Italics carry the "this is a transient override" signal at
        // any width — same convention as the weather widget. Drop the
        // `(lookup)` suffix (it'd be the first thing tail-truncation
        // ate anyway) and let `MetadataEmphasis::Emphasized` do the
        // styling. Both the resolved-override and in-flight-lookup
        // states get italics since both are non-default.
        //
        // In Stopwatch/Timer mode the title metadata is the mode name
        // itself (Stopwatch / Timer) since :time overrides only affect
        // the Clock view.
        let metadata = match mode {
            Mode::Clock => {
                if let Some((label, _)) = &transient {
                    Some(label.clone())
                } else if searching {
                    Some("looking up…".to_string())
                } else {
                    self.tz.map(|tz| tz.to_string())
                }
            }
            Mode::Stopwatch => Some("Stopwatch".to_string()),
            Mode::Timer => Some("Timer".to_string()),
        };
        let emphasis = if mode == Mode::Clock && (transient.is_some() || searching) {
            MetadataEmphasis::Emphasized
        } else {
            MetadataEmphasis::Default
        };
        // Alarm-aware border: while the timer is in the Fired phase
        // and we're on the "on" half of the flash cycle, paint the
        // border in the alert (text_shortcut) style — bold and
        // accent-colored — so the whole widget rim flashes in step
        // with the digit color flip. Off-half ticks paint the resting
        // focused-border style, so the rim visibly *pulses* rather
        // than staying static while the body alone flashes.
        let alarm_on = match self
            .state
            .lock()
            .expect("clock state poisoned")
            .timer
            .phase
        {
            TimerPhase::Fired { fired_at } => Some(alarm_flash_on(fired_at)),
            _ => None,
        };
        let border_style = match alarm_on {
            Some(true) => self.theme.text_shortcut.add_modifier(Modifier::BOLD),
            Some(false) => self.theme.border_focused,
            None => self.theme.border_style(focused),
        };
        let block = apply_title_row(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(border_style),
            focused,
            &base,
            metadata.as_deref(),
            emphasis,
            self.shortcut,
            &self.theme,
            area.width,
        );

        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Reserve the bottom row for the mode-tabs strip when the
        // widget has at least 2 rows of inner space. Below that we
        // skip the strip entirely so a sliver-sized clock cell still
        // shows time rather than only tab labels.
        let (body, tabs_area) = if inner.height >= 2 {
            let body = Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: inner.height - 1,
            };
            let tabs = Rect {
                x: inner.x,
                y: inner.y + inner.height - 1,
                width: inner.width,
                height: 1,
            };
            (body, Some(tabs))
        } else {
            (inner, None)
        };

        match mode {
            Mode::Clock => self.render_clock_body(frame, body, transient.as_ref()),
            Mode::Stopwatch => self.render_stopwatch_body(frame, body),
            Mode::Timer => self.render_timer_body(frame, body),
        }

        if let Some(tabs) = tabs_area {
            self.render_mode_tabs(frame, tabs, mode);
        } else {
            self.state
                .lock()
                .expect("clock state poisoned")
                .mode_tab_rects
                .clear();
        }
    }
    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        // Uppercase ASCII letters are reserved for the app-wide
        // `Shift+<letter>` focus-jump dispatcher — never consume them
        // here regardless of mode.
        if let KeyCode::Char(c) = key.code {
            if c.is_ascii_uppercase() {
                return EventResult::Ignored;
            }
        }

        // Mode switching: c / s / t works in every mode, even Timer's
        // edit mode (the edit buffer reverts implicitly via the next
        // commit cycle). Letters are bare (no modifier) — guard
        // against accidental Ctrl-c etc. matching.
        if key.modifiers == KeyModifiers::NONE {
            match key.code {
                KeyCode::Char('c') => {
                    self.switch_mode(Mode::Clock);
                    return EventResult::Handled;
                }
                KeyCode::Char('s') => {
                    self.switch_mode(Mode::Stopwatch);
                    return EventResult::Handled;
                }
                KeyCode::Char('t') => {
                    self.switch_mode(Mode::Timer);
                    return EventResult::Handled;
                }
                _ => {}
            }
        }

        // Mode-specific dispatch. Stopwatch + Timer keys live in
        // dedicated handlers; Clock mode keeps the original behavior
        // (timezone override clear, gradient cycle, world-clock
        // scroll).
        let mode = self.state.lock().expect("clock state poisoned").mode;
        match mode {
            Mode::Clock => self.handle_key_clock_mode(key),
            Mode::Stopwatch => self.handle_key_stopwatch_mode(key),
            Mode::Timer => self.handle_key_timer_mode(key),
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, _area: Rect) -> EventResult {
        // Scroll routing is mode-aware: in Stopwatch mode the wheel
        // moves the lap list (when there is one to scroll), and in
        // Clock mode it moves the world-clocks list — keeps the
        // gesture obvious without forcing the user to land the
        // cursor precisely on a sub-region.
        let mode = self.state.lock().expect("clock state poisoned").mode;
        match mouse.kind {
            MouseEventKind::ScrollUp => match mode {
                Mode::Stopwatch => self.scroll_laps(-1),
                _ => self.scroll_world_clocks(-1),
            },
            MouseEventKind::ScrollDown => match mode {
                Mode::Stopwatch => self.scroll_laps(1),
                _ => self.scroll_world_clocks(1),
            },
            MouseEventKind::Down(MouseButton::Left) => {
                // Tab-strip hit-test takes priority over the body —
                // the strip is one row at the bottom. We snapshot the
                // cached rects (filled by render_mode_tabs the previous
                // frame) and dispatch on a hit.
                let rects = {
                    let st = self.state.lock().expect("clock state poisoned");
                    st.mode_tab_rects.clone()
                };
                for (mode, x0, x1, y) in rects {
                    if mouse.row == y && mouse.column >= x0 && mouse.column < x1 {
                        self.switch_mode(mode);
                        return EventResult::Handled;
                    }
                }
                EventResult::Ignored
            }
            _ => EventResult::Ignored,
        }
    }

    fn handle_command(&mut self, cmd: &str, args: &[&str]) -> Result<bool> {
        match cmd {
            "time" | "t" | "clock" => {
                if args.is_empty() {
                    anyhow::bail!("usage: :time <city or country>");
                }
                let query = args.join(" ");
                self.lookup_location(&query);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn keybindings(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("↑ / ↓ / scroll", "scroll world clocks (when truncated)"),
            ("g", "cycle digit gradient style"),
            ("x", "clear :time lookup (return to local time)"),
            (":time <city>", "switch primary clock to that location"),
            (":clock <city>", "alias for :time"),
        ]
    }

    fn config(&self) -> serde_json::Value {
        serde_json::json!({
            "timezone": self.config.timezone,
            "show_seconds": self.config.show_seconds,
            "show_seconds_ticker": self.config.show_seconds_ticker,
            "show_date": self.config.show_date,
            "hour_format": self.config.hour_format,
            "secondary_timezones": self.config.secondary_timezones.iter().map(|s| {
                serde_json::json!({"label": s.label, "timezone": s.timezone})
            }).collect::<Vec<_>>(),
            "gradient": self.config.gradient.label(),
        })
    }

    fn apply_config(&mut self, config: serde_json::Value) -> Result<()> {
        let new_config: ClockConfig =
            serde_json::from_value(config).context("invalid clock config payload")?;
        let app_theme = self.app_theme.clone();
        let instance = self.instance.clone();
        *self = Self::with_config(instance, new_config, app_theme);
        Ok(())
    }

    fn set_app_theme(&mut self, theme: Arc<Theme>) {
        self.theme = theme.with_overrides(&self.config.colors);
        self.app_theme = theme;
    }

    fn shortcut_preferences(&self) -> &[char] {
        &self.shortcut_prefs
    }

    fn set_shortcut(&mut self, shortcut: Option<char>) {
        self.shortcut = shortcut;
    }

    fn shortcut(&self) -> Option<char> {
        self.shortcut
    }

    fn title_metadata(&self) -> Option<String> {
        let (transient, searching) = self.snapshot_transient();
        if let Some((label, _)) = transient {
            return Some(format!("{label} (lookup)"));
        }
        if searching {
            return Some("looking up…".to_string());
        }
        self.tz.map(|tz| tz.to_string())
    }
}

// Helpers for the mode infrastructure live in a separate inherent
// `impl ClockWidget` block — Rust requires non-trait methods outside
// `impl Widget for ClockWidget`. Multiple inherent impls are allowed,
// so the per-mode rendering, the tab strip helper, and the per-mode
// key handlers stay clustered together rather than scattered across
// the file. The trait impl itself stays a single block above.
impl ClockWidget {
    /// Body renderer for the Clock mode — the original big-digit time
    /// + ticker + date + world clocks layout, factored out of the
    /// top-level `render` so the new tab strip + mode dispatch can
    /// share the chrome (title row, border, mode tabs).
    fn render_clock_body(
        &self,
        frame: &mut Frame,
        inner: Rect,
        transient: Option<&(String, Tz)>,
    ) {
        let now = chrono::Utc::now();
        let (time, ampm, date) = self.render_strings(now);

        // Big-digit color seed: `text.focused` from the active scheme by
        // default; `text.selected` while a `:time <location>` override is
        // active so the user can't miss that they're not on home base. The
        // gradient (subtle / hue_shift / glow / fade) derives its full
        // 10-stop palette from this seed, so the digits restyle on
        // `:scheme` regardless of the gradient mode chosen.
        let big_style = if transient.is_some() {
            self.theme.text_selected
        } else {
            self.theme.text_focused
        };
        let gradient = self.state.lock().expect("clock state poisoned").gradient;
        let big_lines = big_digits::render_styled(&time, gradient, big_style);

        let mut lines: Vec<Line<'_>> = Vec::new();
        // Top padding so the big digits don't kiss the border.
        lines.push(Line::from(""));
        for line in big_lines {
            lines.push(line);
        }

        if self.config.show_seconds_ticker {
            // Blank line between the big-digit clock and the HH:MM:SS ticker
            // beneath it — gives the ticker some breathing room from the
            // glyphs above.
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                self.ticker_string(now),
                self.theme.text_dim,
            )));
        }

        if !ampm.is_empty() {
            lines.push(Line::from(Span::styled(ampm, self.theme.text_dim)));
        }
        if !date.is_empty() {
            // No blank line above the date — the ticker and the day-date sit
            // together as one block of secondary info beneath the clock.
            lines.push(Line::from(date));
        }

        // World clocks block — show as many entries as fit, scroll the rest
        // with ↑/↓ and mouse-wheel. Primary timezone leads so the user can
        // see local time alongside the rest of the world. The transient
        // footer (when a `:time` override is active) eats the bottom row, so
        // the available height for the body shrinks by 1 in that case —
        // factor that into the fit calculation, otherwise the last clock
        // entry would be clipped by the footer.
        let clocks = self.world_clock_entries();
        let body_h = if transient.is_some() {
            inner.height.saturating_sub(1)
        } else {
            inner.height
        };
        if !clocks.is_empty() {
            // Block overhead is the blank pad + the "── World Clocks ──"
            // header. Below that, every remaining row holds one entry.
            const HEADER_ROWS: u16 = 2;
            let avail_rows = (body_h as i32) - (lines.len() as i32) - (HEADER_ROWS as i32);
            let avail_clocks = avail_rows.max(0) as usize;
            if avail_clocks >= 1 {
                let visible_count = avail_clocks.min(clocks.len());
                let max_scroll = clocks.len().saturating_sub(visible_count);
                let scroll = {
                    let mut st = self.state.lock().expect("clock state poisoned");
                    st.world_clock_max_scroll = max_scroll;
                    if st.world_clock_scroll > max_scroll {
                        st.world_clock_scroll = max_scroll;
                    }
                    st.world_clock_scroll
                };
                let visible_end = scroll + visible_count;
                let has_above = scroll > 0;
                let has_below = visible_end < clocks.len();

                lines.push(Line::from(""));
                // Chevrons surface which directions still have hidden rows.
                // Header is centered by the surrounding Paragraph so the
                // width drift between states is barely perceptible.
                let header_text = match (has_above, has_below) {
                    (false, false) => "── World Clocks ──",
                    (true, false) => "── World Clocks ↑ ──",
                    (false, true) => "── World Clocks ↓ ──",
                    (true, true) => "── World Clocks ↑↓ ──",
                };
                lines.push(Line::from(Span::styled(
                    header_text.to_string(),
                    self.theme.text_dim,
                )));

                let max_label = clocks
                    .iter()
                    .map(|(l, _)| l.chars().count())
                    .max()
                    .unwrap_or(0);
                // Local — and whichever entry the big-digit display is showing
                // — get colored so the user can see at a glance which row
                // matches the big clock. Local picks up `text.focused` from
                // the active scheme; the `:time` override row picks up
                // `text.selected` so it's distinct from Local but still
                // theme-driven.
                let local_highlight_style = self.theme.text_focused;
                let override_highlight_style = self.theme.text_selected;
                let has_override = transient.is_some();
                for (idx, (label, time_str)) in
                    clocks.iter().enumerate().skip(scroll).take(visible_count)
                {
                    // Highlight is keyed off the *absolute* index in the full
                    // list (not the visible window) so the colored row keeps
                    // its identity as the user scrolls past it.
                    let style = if has_override {
                        match idx {
                            0 => local_highlight_style,
                            1 => override_highlight_style,
                            _ => Style::default(),
                        }
                    } else if idx == 0 {
                        local_highlight_style
                    } else {
                        Style::default()
                    };
                    let line = format!("{:<width$}  {}", label, time_str, width = max_label);
                    lines.push(Line::from(Span::styled(line, style)));
                }
            } else {
                // No room — make sure stale max_scroll doesn't let ↑/↓ shift
                // an invisible offset that re-clamps oddly when the cell
                // grows again.
                let mut st = self.state.lock().expect("clock state poisoned");
                st.world_clock_max_scroll = 0;
                st.world_clock_scroll = 0;
            }
        }

        // When a `:time <city>` override is active, append a footer hint
        // pinned to the bottom of the cell so the user has an obvious
        // escape route back to Local time.
        if transient.is_some() {
            let hint = Line::from(Span::styled("x: revert to Local", self.theme.text_dim));
            let body = Paragraph::new(lines).alignment(Alignment::Center);
            let body_h = inner.height.saturating_sub(1);
            let body_area = Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: body_h,
            };
            let hint_area = Rect {
                x: inner.x,
                y: inner.y + body_h,
                width: inner.width,
                height: 1,
            };
            frame.render_widget(body, body_area);
            frame.render_widget(Paragraph::new(hint).alignment(Alignment::Center), hint_area);
        } else {
            let body = Paragraph::new(lines).alignment(Alignment::Center);
            frame.render_widget(body, inner);
        }
    }

    // ─── stopwatch render ──────────────────────────────────────────

    fn render_stopwatch_body(&self, frame: &mut Frame, inner: Rect) {
        let (running, elapsed, gradient, laps, laps_scroll) = {
            let st = self.state.lock().expect("clock state poisoned");
            (
                st.stopwatch.running(),
                st.stopwatch.elapsed(),
                st.gradient,
                st.stopwatch.laps.clone(),
                st.laps_scroll,
            )
        };
        // Running: lookup-time colors (text.selected) so the user can
        // see at a glance that the stopwatch is live. Paused: home
        // colors (text.focused) — matches the resting clock.
        let big_style = if running {
            self.theme.text_selected
        } else {
            self.theme.text_focused
        };
        let hms = format_hms(elapsed);
        let big_lines = big_digits::render_styled(&hms, gradient, big_style);

        // ── Top section: big digits + (frac if paused) + blank + help ──
        let mut top_lines: Vec<Line<'static>> = Vec::new();
        top_lines.push(Line::from(""));
        for line in big_lines {
            top_lines.push(line);
        }

        // Fractional-second suffix only when paused. While running we
        // don't tick fractional seconds on screen (would force a
        // sub-second redraw cycle for a number that's hard to read
        // anyway); the wall-clock-anchored `elapsed` calculation
        // means a stop-and-restart still picks up the exact paused
        // moment without it ever being rendered.
        if !running {
            let frac_ms = elapsed.subsec_millis();
            top_lines.push(Line::from(Span::styled(
                format!(".{frac_ms:03}"),
                self.theme.text_dim,
            )));
        }

        // Help line. `l lap` only advertised while running — the key
        // is ignored otherwise so leaving it in the hint would just
        // confuse a paused user.
        if inner.height > top_lines.len() as u16 + 1 {
            top_lines.push(Line::from(""));
            let hint = if running {
                "Space stop · l lap · r reset"
            } else {
                "Space start · r reset"
            };
            top_lines.push(Line::from(Span::styled(hint, self.theme.text_dim)));
        }

        let top_h = top_lines.len() as u16;
        let top_rect = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: top_h.min(inner.height),
        };
        frame.render_widget(
            Paragraph::new(top_lines).alignment(Alignment::Center),
            top_rect,
        );

        // ── Lap list — separate sub-rect so it can scroll without
        // pushing the big digits off the top. We reserve:
        //   - 1 blank row between help and the laps list
        //   - 1 blank row at the bottom of `inner` (above the mode
        //     tab strip, which lives outside `inner`)
        // and use everything in between for the scrollable list.
        let (laps_max_scroll, applied_scroll) =
            if laps.is_empty() || inner.height <= top_h + 2 {
                (0usize, 0usize)
            } else {
                let laps_rect = Rect {
                    x: inner.x,
                    y: inner.y + top_h + 1,
                    width: inner.width,
                    height: inner.height - top_h - 2,
                };
                // Pre-compute the maximum scroll for this pane so we
                // can clamp `laps_scroll` BEFORE rendering — handles
                // the after-restart "scroll to end" sentinel and any
                // pane-shrink event that would otherwise leave the
                // user scrolled past the end with a blank list. Max
                // scroll = however far we can advance while keeping
                // one lap row visible (the bottom one) plus the
                // `↑ N more` cue.
                let pane_h = laps_rect.height as usize;
                let total = laps.len();
                let max_scroll = if total > pane_h {
                    total - (pane_h - 1)
                } else {
                    0
                };
                let clamped = laps_scroll.min(max_scroll);
                self.render_stopwatch_laps(frame, laps_rect, &laps, clamped);
                (max_scroll, clamped)
            };
        let mut st = self.state.lock().expect("clock state poisoned");
        st.last_laps_max_scroll = laps_max_scroll;
        st.laps_scroll = applied_scroll;
    }

    /// Render the scrollable lap list inside `area`. The caller is
    /// responsible for clamping `scroll` against the pane height
    /// (see `render_stopwatch_body` for the max-scroll math).
    /// Lays out top-to-bottom, reserving one row each for `↑ N more`
    /// / `↓ N more` cues when content overflows. Laps are formatted
    /// `Lap NN - HH:MM:SS.mmm (HH:MM:SS.mmm)`, with the most-recent
    /// entry painted in `text.focused` to draw the eye to fresh data.
    fn render_stopwatch_laps(
        &self,
        frame: &mut Frame,
        area: Rect,
        laps: &[Duration],
        scroll: usize,
    ) {
        let total = laps.len();
        let pane_h = area.height as usize;
        if pane_h == 0 || total == 0 {
            return;
        }
        // Cue decisions. Computed in the right order so the bottom
        // cue is only drawn when entries *actually* hide past the
        // visible window — earlier draft drew "↓ 1 more" even when
        // the last lap fit, leaving the freshest split concealed by
        // its own cue.
        let has_above = scroll > 0;
        let rows_after_top = pane_h.saturating_sub(if has_above { 1 } else { 0 });
        let remaining = total.saturating_sub(scroll);
        let (has_below, visible_count) = if remaining <= rows_after_top {
            // All remaining laps fit without sacrificing a row to a
            // bottom cue. This is the after-`l`-bump-scroll path:
            // the latest lap sits at the bottom of the visible
            // window and there's no "↓ N more" hiding it.
            (false, remaining)
        } else {
            // At least one lap is hidden below — reserve a row for
            // the `↓ N more` cue.
            (true, rows_after_top.saturating_sub(1))
        };

        let mut lines: Vec<Line<'static>> = Vec::with_capacity(pane_h);
        if has_above {
            let hidden = scroll;
            lines.push(Line::from(Span::styled(
                format!("↑ {hidden} more"),
                self.theme.text_dim,
            )));
        }
        let end = (scroll + visible_count).min(total);
        for i in scroll..end {
            // Lap numbers are 1-indexed for human display; cap the
            // width at 2 so `Lap 01` and `Lap 99` line up.
            let num = i + 1;
            // Gap from the previous lap. Lap 01's "previous" is the
            // stopwatch start (Duration::ZERO), so its gap equals
            // the lap time — same as a physical stopwatch shows.
            let prev = if i == 0 { Duration::ZERO } else { laps[i - 1] };
            let gap = laps[i].checked_sub(prev).unwrap_or(Duration::ZERO);
            let main_style = if i + 1 == total {
                self.theme.text_focused
            } else {
                self.theme.text_dim
            };
            // Two spans so the parenthetical (gap from previous)
            // stays in `text.dim` regardless of whether this is the
            // most-recent row. Keeps the visual emphasis on the
            // cumulative time the user pressed `l` against.
            lines.push(Line::from(vec![
                Span::styled(
                    format!("Lap {num:02} - {}", format_hms_ms(laps[i])),
                    main_style,
                ),
                Span::styled(
                    format!(" ({})", format_hms_ms(gap)),
                    self.theme.text_dim,
                ),
            ]));
        }
        if has_below {
            let hidden = total - end;
            lines.push(Line::from(Span::styled(
                format!("↓ {hidden} more"),
                self.theme.text_dim,
            )));
        }

        frame.render_widget(
            Paragraph::new(lines).alignment(Alignment::Center),
            area,
        );
    }

    // ─── timer render ──────────────────────────────────────────────

    fn render_timer_body(&self, frame: &mut Frame, inner: Rect) {
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

    // ─── mode tabs (bottom strip) ──────────────────────────────────

    fn render_mode_tabs(&self, frame: &mut Frame, area: Rect, active: Mode) {
        // Build the label list first so we know the total rendered
        // width before laying out hit-test rects. We render the strip
        // center-aligned (looks balanced under the big-digit body),
        // and a left-anchored accumulator would put the clickable
        // ranges to the LEFT of where the labels actually paint —
        // exactly the bug a user would see as "tabs seem clickable
        // but their click locations are way off to the left of where
        // they ought to be". Compute the centered start column once,
        // then walk from there.
        const SEP: &str = "  ";
        let labels: Vec<String> = Mode::all()
            .iter()
            .map(|m| format!("[{}]", m.label()))
            .collect();
        let total_width: u16 = labels.iter().map(|l| l.chars().count() as u16).sum::<u16>()
            + (labels.len().saturating_sub(1) as u16) * SEP.len() as u16;
        let start_x = area
            .x
            .saturating_add(area.width.saturating_sub(total_width) / 2);

        let mut spans: Vec<Span> = Vec::with_capacity(Mode::all().len() * 2);
        let mut hits: Vec<(Mode, u16, u16, u16)> = Vec::with_capacity(Mode::all().len());
        let mut x = start_x;
        for (i, (mode, label)) in Mode::all().iter().zip(labels.iter()).enumerate() {
            if i > 0 {
                spans.push(Span::raw(SEP));
                x = x.saturating_add(SEP.len() as u16);
            }
            let width = label.chars().count() as u16;
            let style = if *mode == active {
                self.theme.text_selected
            } else {
                self.theme.text_dim
            };
            spans.push(Span::styled(label.clone(), style));
            hits.push((*mode, x, x.saturating_add(width), area.y));
            x = x.saturating_add(width);
        }
        self.state.lock().expect("clock state poisoned").mode_tab_rects = hits;
        frame.render_widget(
            Paragraph::new(Line::from(spans)).alignment(Alignment::Center),
            area,
        );
    }

    // ─── mode + alarm helpers ──────────────────────────────────────

    /// Snapshot the *full* in-memory stopwatch+timer state into the
    /// runtime-state file. Called from every key handler that changes
    /// stopwatch or timer state, so quitting at any moment preserves
    /// progress and a restart picks up where we left off (running
    /// stopwatches keep ticking, paused timers stay paused, etc.).
    /// Stack tab indices live in the same file; load-modify-save
    /// round-trips them so other widgets' persisted state isn't
    /// wiped on each clock-state save.
    fn persist_clock_state(&self) {
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

        drop(st);
        if let Err(err) = crate::runtime_state::save(&state) {
            tracing::warn!(error = %err, "failed to persist clock state");
        }
    }

    fn switch_mode(&self, target: Mode) {
        let mut reverted_edit = false;
        {
            let mut st = self.state.lock().expect("clock state poisoned");
            // Bailing out of Timer mode while editing implicitly
            // cancels the edit — restore the phase the user was in
            // before they pressed `e`. Without this, switching to
            // Clock/Stopwatch and back would leave the timer stuck in
            // edit mode showing a stale buffer. Same as pressing Esc.
            if st.mode == Mode::Timer && target != Mode::Timer {
                if let TimerPhase::Editing { prior_phase } = st.timer.phase.clone() {
                    st.timer.phase = *prior_phase;
                    reverted_edit = true;
                }
            }
            st.mode = target;
        }
        if reverted_edit {
            self.persist_clock_state();
        }
    }

    fn handle_key_clock_mode(&mut self, key: KeyEvent) -> EventResult {
        if key.modifiers != KeyModifiers::NONE && key.modifiers != KeyModifiers::SHIFT {
            return EventResult::Ignored;
        }
        match key.code {
            KeyCode::Char('x') => {
                self.clear_transient();
                EventResult::Handled
            }
            KeyCode::Char('g') => {
                let mut st = self.state.lock().expect("clock state poisoned");
                st.gradient = st.gradient.next();
                EventResult::Handled
            }
            KeyCode::Up => self.scroll_world_clocks(-1),
            KeyCode::Down => self.scroll_world_clocks(1),
            _ => EventResult::Ignored,
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
    fn tick_mode_state(&self, second_changed: bool) -> (bool, bool) {
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
                let elapsed = SystemTime::now()
                    .duration_since(fired_at)
                    .unwrap_or(Duration::ZERO);
                let cycle_period =
                    ALARM_FLASH_GAP * ALARM_BEEPS_PER_BURST * 2 + ALARM_BURST_GAP;
                let burst_index = elapsed.as_nanos() / cycle_period.as_nanos();
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

    fn handle_key_stopwatch_mode(&mut self, key: KeyEvent) -> EventResult {
        // Reject any modifier (Shift, Ctrl, etc.) so Ctrl-C and the
        // app-wide focus-jump dispatcher still see them.
        if key.modifiers != KeyModifiers::NONE {
            return EventResult::Ignored;
        }
        match key.code {
            // Space toggles run/stop. The closed-form `elapsed` model
            // means a stop captures `accumulated` and a restart starts
            // a fresh `started_at` — pause-resume is exact, no
            // sub-second drift.
            KeyCode::Char(' ') => {
                {
                    let mut st = self.state.lock().expect("clock state poisoned");
                    st.stopwatch.toggle();
                }
                self.persist_clock_state();
                EventResult::Handled
            }
            // `r` zeros the elapsed counter. If the stopwatch was
            // running, it keeps running from 00:00:00 — preserves the
            // "restart-without-stopping" gesture from the spec.
            KeyCode::Char('r') => {
                {
                    let mut st = self.state.lock().expect("clock state poisoned");
                    st.stopwatch.reset();
                    // Reset also clears the laps scroll — there's
                    // nothing to scroll through anymore, and stale
                    // scroll position could leave the list looking
                    // empty when laps are eventually recorded again.
                    st.laps_scroll = 0;
                }
                self.persist_clock_state();
                EventResult::Handled
            }
            // `l` records a lap at the current elapsed reading.
            // Silently no-ops when the stopwatch isn't running or the
            // 99-lap cap has been reached — feedback-free is the
            // right call since both states are obvious from the
            // visible UI (paused-style digits, full list).
            KeyCode::Char('l') => {
                let recorded = {
                    let mut st = self.state.lock().expect("clock state poisoned");
                    let ok = st.stopwatch.record_lap();
                    if ok {
                        // Auto-scroll to the bottom so a new lap is
                        // always visible. Sentinel value: the next
                        // render clamps it down to the actual max
                        // scroll for the current pane height. Same
                        // trick the post-restart path uses.
                        st.laps_scroll = st.stopwatch.laps.len();
                    }
                    ok
                };
                if recorded {
                    self.persist_clock_state();
                }
                EventResult::Handled
            }
            // Scroll the laps list. Up/Down arrows + j/k both work,
            // matching the WSJ / news / notes navigation convention.
            // Returns Handled only when scroll actually changed, so a
            // press at the limit falls through to the global dispatcher.
            KeyCode::Up | KeyCode::Char('k') => self.scroll_laps(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_laps(1),
            _ => EventResult::Ignored,
        }
    }

    /// Adjust the laps scroll offset by `delta`. Clamps against the
    /// max scroll cached by the most recent render so the handler
    /// doesn't need to re-derive the pane layout. Returns `Handled`
    /// only when the offset actually moved.
    fn scroll_laps(&self, delta: i32) -> EventResult {
        let mut st = self.state.lock().expect("clock state poisoned");
        if st.last_laps_max_scroll == 0 {
            return EventResult::Ignored;
        }
        let max = st.last_laps_max_scroll as i32;
        let cur = st.laps_scroll as i32;
        let next = (cur + delta).clamp(0, max);
        if next == cur {
            return EventResult::Ignored;
        }
        st.laps_scroll = next as usize;
        EventResult::Handled
    }

    fn handle_key_timer_mode(&mut self, key: KeyEvent) -> EventResult {
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

/// Registry kind string for the clock widget. Single source of truth — used
/// by the widget descriptor, the config file resolver, and the wizard.
pub const KIND: &str = "clock";

/// Wizard descriptor for the clock widget. Serves as the reference
/// implementation other widgets follow when they migrate from
/// `defer_to_toml_descriptor` to a real schema.
pub fn wizard_descriptor() -> crate::wizard::descriptor::WizardDescriptor {
    use crate::wizard::descriptor::{ChoiceOption, WizardDescriptor, WizardField, WizardFieldKind};

    // Helper for the three optional secondary-timezone fields. Each is a
    // Lookup over the same IANA list with allow_blank so the user can
    // leave any slot empty.
    fn secondary_field(key: &'static str, label: &'static str) -> WizardField {
        WizardField {
            key,
            label,
            help: "Optional. Type to filter the IANA zone list (e.g. \
                   \"tokyo\", \"london\"). Space picks the highlighted row; \
                   Tab moves to the next field. Pick \"(none)\" to skip this \
                   slot. For more than three world clocks, hand-edit \
                   [[secondary_timezones]] in clock.toml after setup.",
            required: false,
            kind: WizardFieldKind::Lookup {
                options: iana_timezone_options(),
                default: None,
                allow_blank: true,
                blank_label: "(none)",
            },
            validate: None,
        }
    }

    WizardDescriptor {
        display_name: "Clock",
        blurb: "Time display with optional secondary world clocks. The wizard \
                covers the basics; gradient styles and additional secondary \
                zones live in clock.toml for hand-tuning.",
        load_from_toml: Some(load_clock_from_toml),
        render_toml: Some(render_clock_toml),
        fields: vec![
            WizardField {
                key: "timezone",
                label: "Primary timezone",
                help: "Type to filter (e.g. \"vancouver\", \"tokyo\"). ↑/↓ \
                       navigates; PgUp/PgDn jumps by 10. Space picks the \
                       highlighted row. Pick \"(system local time)\" to \
                       follow the host clock.",
                required: false,
                kind: WizardFieldKind::Lookup {
                    options: iana_timezone_options(),
                    default: None,
                    allow_blank: true,
                    blank_label: "(system local time)",
                },
                validate: None,
            },
            WizardField {
                key: "hour_format",
                label: "Hour format",
                help: "\"12h\" — am/pm. \"24h\" — military time.",
                required: true,
                kind: WizardFieldKind::Choice {
                    options: vec![
                        ChoiceOption {
                            value: "24h",
                            label: "24-hour",
                            help: None,
                        },
                        ChoiceOption {
                            value: "12h",
                            label: "12-hour (am/pm)",
                            help: None,
                        },
                    ],
                    default: Some("24h"),
                },
                validate: None,
            },
            WizardField {
                key: "show_seconds",
                label: "Show seconds in the big digits",
                help: "Adds :SS to the block-digit display. The small ticking \
                       line below the big digits always shows seconds.",
                required: false,
                kind: WizardFieldKind::Bool { default: false },
                validate: None,
            },
            WizardField {
                key: "show_date",
                label: "Show the date row",
                help: "Renders today's date under the big digits.",
                required: false,
                kind: WizardFieldKind::Bool { default: true },
                validate: None,
            },
            secondary_field("secondary_tz_1", "Secondary world clock 1"),
            secondary_field("secondary_tz_2", "Secondary world clock 2"),
            secondary_field("secondary_tz_3", "Secondary world clock 3"),
        ],
    }
}

/// Every IANA zone the host's chrono-tz database knows about, formatted as
/// `(value, label)` pairs for the wizard's `Lookup` dropdown. Both halves
/// of each tuple are the canonical name (`"America/Los_Angeles"`) — the
/// dropdown's filter matches against the label, which means the user can
/// type either the continent or the city.
fn iana_timezone_options() -> Vec<(&'static str, &'static str)> {
    chrono_tz::TZ_VARIANTS
        .iter()
        .map(|tz| {
            let name = tz.name();
            (name, name)
        })
        .collect()
}

/// Render the clock widget's TOML from wizard values. We render
/// `secondary_timezones` as repeated `[[secondary_timezones]]` tables to
/// match the existing `ClockConfig` deserialiser; labels are derived from
/// the city portion of the IANA name.
fn render_clock_toml(
    values: &std::collections::HashMap<String, crate::wizard::descriptor::WizardValue>,
    _existing: Option<&str>,
) -> String {
    use crate::wizard::descriptor::WizardValue;
    let mut out = String::new();
    out.push_str(
        "# Generated by `glint --setup`. Hand-edit freely; the wizard\n\
         # preserves advanced keys it doesn't manage (e.g. [colors], gradient).\n\n",
    );
    // Timezone field is a Lookup → WizardValue::Choice; accept Text
    // as a fallback in case a custom descriptor wires it differently.
    let tz = match values.get("timezone") {
        Some(WizardValue::Choice(s)) | Some(WizardValue::Text(s)) => s.trim(),
        _ => "",
    };
    if !tz.is_empty() {
        out.push_str(&format!("timezone = {}\n", toml_quote(tz)));
    }
    if let Some(WizardValue::Choice(hf)) = values.get("hour_format") {
        out.push_str(&format!("hour_format = {}\n", toml_quote(hf)));
    }
    if let Some(WizardValue::Bool(b)) = values.get("show_seconds") {
        out.push_str(&format!("show_seconds = {b}\n"));
    }
    if let Some(WizardValue::Bool(b)) = values.get("show_date") {
        out.push_str(&format!("show_date = {b}\n"));
    }
    out.push_str("show_seconds_ticker = true\n");

    // Up to three optional secondary world clocks, each in its own Lookup
    // field. Empty / unset slots are skipped; the user reaches for clock.toml
    // directly when they want more than three.
    for key in ["secondary_tz_1", "secondary_tz_2", "secondary_tz_3"] {
        let zone = match values.get(key) {
            Some(WizardValue::Choice(s)) | Some(WizardValue::Text(s)) => s.trim(),
            _ => "",
        };
        if zone.is_empty() {
            continue;
        }
        let label = label_from_iana_zone(zone);
        out.push_str("\n[[secondary_timezones]]\n");
        out.push_str(&format!("label = {}\n", toml_quote(&label)));
        out.push_str(&format!("timezone = {}\n", toml_quote(zone)));
    }
    out
}

/// Derive a friendly label from an IANA zone like `"America/New_York"` →
/// `"New York"`. Falls back to the full zone when there's no `/`.
fn label_from_iana_zone(zone: &str) -> String {
    let tail = zone.rsplit('/').next().unwrap_or(zone);
    tail.replace('_', " ")
}

/// Inverse of [`render_clock_toml`]: parse a clock TOML and surface the
/// scalar fields plus the first three `[[secondary_timezones]]` entries
/// into the wizard's three Lookup slots. Additional entries beyond the
/// third are intentionally ignored — the user can hand-edit clock.toml
/// for more — and the wizard's render path will preserve only the three
/// it knows about, so users with custom clocks should not lose them
/// silently. (Hydration only seeds keys; the user is then expected to
/// confirm and re-finalize through the wizard.)
fn load_clock_from_toml(
    doc: &toml::Value,
) -> std::collections::HashMap<String, crate::wizard::descriptor::WizardValue> {
    use crate::wizard::descriptor::WizardValue;
    let mut out = std::collections::HashMap::new();
    if let Some(s) = doc.get("timezone").and_then(|v| v.as_str()) {
        out.insert("timezone".into(), WizardValue::Choice(s.into()));
    }
    if let Some(s) = doc.get("hour_format").and_then(|v| v.as_str()) {
        out.insert("hour_format".into(), WizardValue::Choice(s.into()));
    }
    if let Some(b) = doc.get("show_seconds").and_then(|v| v.as_bool()) {
        out.insert("show_seconds".into(), WizardValue::Bool(b));
    }
    if let Some(b) = doc.get("show_date").and_then(|v| v.as_bool()) {
        out.insert("show_date".into(), WizardValue::Bool(b));
    }
    if let Some(arr) = doc.get("secondary_timezones").and_then(|v| v.as_array()) {
        for (i, entry) in arr.iter().take(3).enumerate() {
            let Some(zone) = entry.get("timezone").and_then(|v| v.as_str()) else {
                continue;
            };
            let key = match i {
                0 => "secondary_tz_1",
                1 => "secondary_tz_2",
                _ => "secondary_tz_3",
            };
            out.insert(key.into(), WizardValue::Choice(zone.into()));
        }
    }
    out
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

/// Registry factory. Reads the on-disk TOML for this instance and constructs
/// the widget with the dependencies it needs from `WidgetCtx`.
pub fn build(ctx: &super::WidgetCtx) -> Box<dyn super::Widget> {
    let cfg: ClockConfig =
        crate::config::load_widget_toml_for_instance(KIND, &ctx.instance).unwrap_or_default();
    Box::new(ClockWidget::with_config(
        ctx.instance.clone(),
        cfg,
        ctx.theme.clone(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn build_widget(cfg: ClockConfig) -> ClockWidget {
        ClockWidget::with_config("main".to_string(), cfg, Arc::new(Theme::builtin_defaults()))
    }

    #[test]
    fn label_from_iana_zone_strips_underscores_and_continent() {
        assert_eq!(label_from_iana_zone("America/New_York"), "New York");
        assert_eq!(label_from_iana_zone("Asia/Tokyo"), "Tokyo");
        assert_eq!(label_from_iana_zone("UTC"), "UTC");
        assert_eq!(label_from_iana_zone("Pacific/Auckland"), "Auckland");
    }

    #[test]
    fn render_clock_toml_emits_secondary_zone_tables() {
        use crate::wizard::descriptor::WizardValue;
        let mut values: std::collections::HashMap<String, WizardValue> = Default::default();
        values.insert(
            "timezone".into(),
            WizardValue::Choice("America/Vancouver".into()),
        );
        values.insert("hour_format".into(), WizardValue::Choice("24h".into()));
        values.insert("show_seconds".into(), WizardValue::Bool(false));
        values.insert("show_date".into(), WizardValue::Bool(true));
        // Two filled secondary-zone slots, one left blank.
        values.insert(
            "secondary_tz_1".into(),
            WizardValue::Choice("America/New_York".into()),
        );
        values.insert(
            "secondary_tz_2".into(),
            WizardValue::Choice("Europe/London".into()),
        );
        values.insert("secondary_tz_3".into(), WizardValue::Choice("".into()));
        let body = render_clock_toml(&values, None);
        assert!(body.contains("timezone = \"America/Vancouver\""));
        assert!(body.contains("hour_format = \"24h\""));
        assert!(body.contains("[[secondary_timezones]]"));
        assert!(body.contains("label = \"New York\""));
        assert!(body.contains("timezone = \"America/New_York\""));
        assert!(body.contains("label = \"London\""));
        // Round-trips through the existing deserialiser; the empty slot is
        // omitted entirely.
        let parsed: ClockConfig = toml::from_str(&body).expect("wizard-rendered clock.toml parses");
        assert_eq!(parsed.timezone.as_deref(), Some("America/Vancouver"));
        assert_eq!(parsed.hour_format, 24);
        assert_eq!(parsed.secondary_timezones.len(), 2);
    }

    #[test]
    fn twelve_hour_format_renders_midnight_as_12_am() {
        let cfg = ClockConfig {
            timezone: Some("UTC".into()),
            show_seconds: false,
            show_seconds_ticker: false,
            show_date: false,
            hour_format: 12,
            secondary_timezones: Vec::new(),
            gradient: big_digits::Gradient::default(),
            colors: ColorScheme::default(),
            shortcuts: Vec::new(),
        };
        let widget = build_widget(cfg);
        let midnight_utc = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap();
        let (time, ampm, date) = widget.render_strings(midnight_utc);
        assert_eq!(time, "12:00");
        assert_eq!(ampm, "AM");
        assert!(date.is_empty());
    }

    #[test]
    fn twenty_four_hour_format_zero_pads() {
        let cfg = ClockConfig {
            timezone: Some("UTC".into()),
            show_seconds: true,
            show_seconds_ticker: false,
            show_date: false,
            hour_format: 24,
            secondary_timezones: Vec::new(),
            gradient: big_digits::Gradient::default(),
            colors: ColorScheme::default(),
            shortcuts: Vec::new(),
        };
        let widget = build_widget(cfg);
        let t = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 9, 5, 7).unwrap();
        let (time, ampm, _) = widget.render_strings(t);
        assert_eq!(time, "09:05:07");
        assert_eq!(ampm, "");
    }

    #[test]
    fn ticker_includes_seconds_in_primary_timezone() {
        let cfg = ClockConfig {
            timezone: Some("UTC".into()),
            show_seconds: false,
            show_seconds_ticker: true,
            show_date: false,
            hour_format: 24,
            secondary_timezones: Vec::new(),
            gradient: big_digits::Gradient::default(),
            colors: ColorScheme::default(),
            shortcuts: Vec::new(),
        };
        let w = build_widget(cfg);
        let t = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 9, 5, 42).unwrap();
        assert_eq!(w.ticker_string(t), "09:05:42");
    }

    #[test]
    fn city_from_tz_name_strips_region_and_underscores() {
        assert_eq!(city_from_tz_name("America/New_York"), "New York");
        assert_eq!(city_from_tz_name("Europe/London"), "London");
        assert_eq!(city_from_tz_name("Asia/Tokyo"), "Tokyo");
        assert_eq!(city_from_tz_name("UTC"), "UTC");
    }

    #[test]
    fn world_clock_entries_pin_local_during_time_override() {
        use chrono_tz::Tz;
        let cfg = ClockConfig {
            secondary_timezones: vec![SecondaryTimezone {
                label: "Tokyo".into(),
                timezone: "Asia/Tokyo".into(),
            }],
            ..ClockConfig::default()
        };
        let w = build_widget(cfg);
        {
            let mut st = w.state.lock().unwrap();
            st.transient_tz = Some(("Berlin".into(), "Europe/Berlin".parse::<Tz>().unwrap()));
        }
        let entries = w.world_clock_entries();
        assert_eq!(entries.len(), 3, "Local + override + 1 secondary");
        assert_eq!(entries[0].0, "Local");
        assert_eq!(entries[1].0, "Berlin");
        assert_eq!(entries[2].0, "Tokyo");
    }

    #[test]
    fn world_clock_entries_lead_with_primary() {
        let cfg = ClockConfig {
            timezone: Some("America/Vancouver".into()),
            secondary_timezones: vec![SecondaryTimezone {
                label: "Tokyo".into(),
                timezone: "Asia/Tokyo".into(),
            }],
            ..ClockConfig::default()
        };
        let w = build_widget(cfg);
        let entries = w.world_clock_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "Vancouver");
        assert_eq!(entries[1].0, "Tokyo");
    }

    #[test]
    fn world_clock_entries_include_icon_time_and_date() {
        let cfg = ClockConfig {
            timezone: Some("America/Vancouver".into()),
            secondary_timezones: vec![SecondaryTimezone {
                label: "Tokyo".into(),
                timezone: "Asia/Tokyo".into(),
            }],
            ..ClockConfig::default()
        };
        let w = build_widget(cfg);
        let entries = w.world_clock_entries();
        for (_label, formatted) in &entries {
            // Format: "<icon> HH:MM Wkd Mon DD"
            let parts: Vec<&str> = formatted.split_whitespace().collect();
            assert_eq!(parts.len(), 5, "unexpected format: {formatted:?}");
            assert!(parts[0] == "☀" || parts[0] == "☾");
            // HH:MM
            assert_eq!(parts[1].chars().nth(2), Some(':'));
            // Weekday abbreviation
            assert!(
                ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].contains(&parts[2]),
                "unexpected weekday: {:?}",
                parts[2]
            );
            // Month abbreviation
            assert!(
                [
                    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov",
                    "Dec"
                ]
                .contains(&parts[3]),
                "unexpected month: {:?}",
                parts[3]
            );
            // Day-of-month is a positive integer
            assert!(parts[4].parse::<u32>().is_ok());
        }
    }

    #[test]
    fn day_night_icon_boundaries() {
        assert_eq!(day_night_icon(5), "☾");
        assert_eq!(day_night_icon(6), "☀");
        assert_eq!(day_night_icon(12), "☀");
        assert_eq!(day_night_icon(17), "☀");
        assert_eq!(day_night_icon(18), "☾");
        assert_eq!(day_night_icon(23), "☾");
        assert_eq!(day_night_icon(0), "☾");
    }

    #[test]
    fn scroll_world_clocks_clamps_and_passes_through_when_full_list_fits() {
        let w = build_widget(ClockConfig::default());

        // max_scroll == 0 means the whole list fits, so ↑/↓ events should
        // fall through (Ignored) rather than silently swallow the keypress.
        assert_eq!(w.scroll_world_clocks(-1), EventResult::Ignored);
        assert_eq!(w.scroll_world_clocks(1), EventResult::Ignored);
        assert_eq!(w.state.lock().unwrap().world_clock_scroll, 0);

        // Simulate a render that left 3 entries hidden below the fold.
        {
            let mut st = w.state.lock().unwrap();
            st.world_clock_max_scroll = 3;
        }
        // Scroll down advances; can't go past max_scroll.
        assert_eq!(w.scroll_world_clocks(1), EventResult::Handled);
        assert_eq!(w.state.lock().unwrap().world_clock_scroll, 1);
        for _ in 0..10 {
            w.scroll_world_clocks(1);
        }
        assert_eq!(
            w.state.lock().unwrap().world_clock_scroll,
            3,
            "scroll must clamp at max_scroll"
        );
        // Scroll up walks back; can't go below 0.
        assert_eq!(w.scroll_world_clocks(-1), EventResult::Handled);
        assert_eq!(w.state.lock().unwrap().world_clock_scroll, 2);
        for _ in 0..10 {
            w.scroll_world_clocks(-1);
        }
        assert_eq!(
            w.state.lock().unwrap().world_clock_scroll,
            0,
            "scroll must clamp at 0"
        );
    }

    #[test]
    fn invalid_secondary_timezones_are_dropped() {
        let cfg = ClockConfig {
            secondary_timezones: vec![
                SecondaryTimezone {
                    label: "New York".into(),
                    timezone: "America/New_York".into(),
                },
                SecondaryTimezone {
                    label: "Bogus".into(),
                    timezone: "Not/A_Real_TZ".into(),
                },
            ],
            ..ClockConfig::default()
        };
        let w = build_widget(cfg);
        assert_eq!(w.secondaries.len(), 1);
        assert_eq!(w.secondaries[0].0, "New York");
    }
}
