// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono_tz::Tz;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::Rect,
    style::Modifier,
    widgets::{Block, BorderType, Borders},
    Frame,
};
use crate::theme::Theme;
use crate::ui::{apply_title_row, MetadataEmphasis};

use super::{AppContext, EventResult, FocusRequest, Widget};

mod clock_view;
mod config;
mod persistence;
mod state;
mod stopwatch;
mod timer;

pub use config::{wizard_descriptor, ClockConfig, KIND};
use state::{ClockState, Mode};
use timer::{alarm_flash_on, TimerPhase};

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
    /// Wall-clock millis-since-epoch of the most recent `render()`
    /// call. Read by `update()` to detect when this widget is on a
    /// hidden stack tab — render() isn't called on hidden children,
    /// so a stale timestamp means we're invisible and per-second
    /// dirty flips can be suppressed (otherwise an invisible clock
    /// face would force a full-dashboard redraw at 1 Hz). Atomic so
    /// the `&self` render path can write without locking.
    last_render_at_millis: AtomicI64,
    /// Display-state dirty flag — see Widget::take_dirty. True at
    /// construction so the initial render lands.
    dirty: bool,
}

/// How fresh `last_render_at_millis` must be for the widget to count
/// as "currently visible." Stack widgets call `update()` on hidden
/// children every `stack_hidden_poll_ratio` ticks (default 20 ×
/// 250 ms = 5 s); a visible clock renders at least once per second
/// (it's the dashboard's 1 Hz dirty source). 1500 ms catches the
/// visible case while comfortably excluding the hidden case.
const HIDDEN_RENDER_THRESHOLD_MS: i64 = 1500;

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
        let mut state = ClockState {
            gradient: config.gradient,
            ..ClockState::default()
        };
        persistence::hydrate_state(&mut state, &id);
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
            last_render_at_millis: AtomicI64::new(0),
            dirty: true,
        }
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
        let now_millis = chrono::Utc::now().timestamp_millis();
        let now_secs = now_millis / 1000;
        let second_changed = self.last_tick_second != Some(now_secs);
        if second_changed {
            self.last_tick_second = Some(now_secs);
        }
        // Detect "I'm on a hidden stack tab." Stack children only
        // render when active; if the last render was more than
        // HIDDEN_RENDER_THRESHOLD_MS ago, suppress per-second dirty
        // flips so an invisible clock face doesn't force a
        // full-dashboard redraw at 1 Hz. Timer-fire / focus-grab
        // signals still flow via `pending_focus_grab` (polled
        // separately by the app), and the next user-triggered tab
        // switch forces a redraw regardless, so visibility catches
        // up immediately when the clock comes back to the front.
        let last_render = self.last_render_at_millis.load(Ordering::Relaxed);
        let visible = last_render > 0 && (now_millis - last_render) < HIDDEN_RENDER_THRESHOLD_MS;

        // Single state-lock for the per-tick work — combining the
        // mode check, stopwatch/timer phase advance, and dirty
        // signaling halves the per-tick lock count at idle (one lock
        // instead of two). `tick_mode_state` already covers running
        // counters and the alarm flash; here we just need to ALSO
        // refresh on a wall-clock second change when the visible
        // surface depends on it (the Clock view's ticker line; no
        // other mode does).
        let (need_dirty, emit_bel) = self.tick_mode_state(second_changed);
        if need_dirty && visible {
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
        // Stamp the visibility tracker so update() can tell whether
        // this widget is being drawn. Stack widgets skip render() for
        // hidden children, so a stale value means "not visible."
        self.last_render_at_millis
            .store(chrono::Utc::now().timestamp_millis(), Ordering::Relaxed);
        // Losing focus drops the world-clock selection cursor — the
        // user has navigated away and the highlighted row would
        // otherwise linger across tabs. Also dismiss any open
        // confirm-remove modal: a focus shift mid-prompt is a clear
        // cancel signal, and re-opening the prompt later would
        // surprise the user.
        if !focused {
            let mut st = self.state.lock().expect("clock state poisoned");
            st.world_clock_selected = None;
            st.confirm_remove = None;
        }
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
                if let Some((label, _city, _tz)) = &transient {
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

        // ViewTier deviation: clock uses ViewTier::from_rect for gating
        // the Full-tier rich views, but keeps its own inline geometry
        // constants for the big-digit layout (which drives the per-mode
        // thresholds). The tier is derived from the outer area (borders
        // included), consistent with the convention in every other widget.
        let tier = crate::widgets::ViewTier::from_rect(area);
        match mode {
            Mode::Clock => self.render_clock_body(frame, body, transient.as_ref(), tier),
            Mode::Stopwatch => self.render_stopwatch_body(frame, body, tier),
            Mode::Timer => self.render_timer_body(frame, body, tier),
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

        // Confirm-remove overlay paints last so it sits above the
        // clock body. Only mounted when a removal is pending.
        let pending = self
            .state
            .lock()
            .expect("clock state poisoned")
            .confirm_remove
            .clone();
        if let Some((label, _tz)) = pending {
            crate::ui::modal::render(
                frame,
                area,
                &self.theme,
                crate::ui::modal::ConfirmModal {
                    title: " Remove world clock? ",
                    target: &label,
                    hint: None,
                    max_width: 48,
                },
            );
        }
    }
    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        // Confirm-remove modal eats every keystroke while open: `y`
        // commits the removal, anything else cancels and dismisses.
        // Runs before any other dispatch so mode-switch letters
        // don't bypass the prompt.
        if self
            .state
            .lock()
            .expect("clock state poisoned")
            .confirm_remove
            .is_some()
        {
            match crate::ui::modal::dispatch_key(key) {
                crate::ui::modal::ConfirmChoice::Confirm => self.confirm_remove_world_clock(),
                crate::ui::modal::ConfirmChoice::Cancel => self.cancel_remove_world_clock(),
            }
            return EventResult::Handled;
        }

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
            ("↑/↓ / j/k", "select world clock (auto-scrolls)"),
            ("-", "remove selected world clock"),
            ("+", "add `:time` / `:clock` lookup to world clocks"),
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
        // Snapshot the assigned `Shift+<letter>` shortcut before
        // we wholesale-replace `self`. `assign_shortcuts` runs once
        // at startup, so a config-watcher-triggered apply_config
        // (e.g. after `+`/`-` rewrites clock.toml) would otherwise
        // drop the C-highlight from the title bar with no path to
        // restore it.
        let shortcut = self.shortcut;
        *self = Self::with_config(instance, new_config, app_theme);
        self.shortcut = shortcut;
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
        if let Some((label, _city, _tz)) = transient {
            return Some(format!("{label} (lookup)"));
        }
        if searching {
            return Some("looking up…".to_string());
        }
        self.tz.map(|tz| tz.to_string())
    }
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
mod tests;
