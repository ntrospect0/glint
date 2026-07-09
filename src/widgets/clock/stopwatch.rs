// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Stopwatch mode for the clock widget: the closed-form elapsed
//! counter, the lap list, the render + key handlers for the
//! Stopwatch tab. The shared widget-level state (mode, gradient,
//! etc.) lives on `ClockState` in `mod.rs`; this file owns the
//! stopwatch-specific slice (`StopwatchState`) and the methods that
//! consume it.
//!
//! `format_hms` and `format_hms_ms` are shared with the Timer
//! renderer, so they live here `pub(super)` rather than getting
//! duplicated.

use std::time::{Duration, SystemTime};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Alignment, Rect},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::ui::big_digits;
use crate::widgets::ViewTier;

use super::{ClockWidget, EventResult};

/// Hard cap on recorded laps. Past this the `l` key no-ops — keeps
/// the list bounded for persistence + render cost and matches
/// kitchen-stopwatch convention.
pub(super) const MAX_LAPS: usize = 99;

/// Stopwatch model: closed-form elapsed = `accumulated` + (now -
/// `started_at` when running). Storing the wall-clock start instant
/// rather than a per-tick counter means a running stopwatch stays
/// accurate even when the widget isn't being redrawn (the widget is
/// stack-hidden, the terminal was backgrounded, etc.). Persisting
/// across restarts works the same way — `SystemTime` survives a
/// serde round-trip; an `Instant` would not.
#[derive(Debug, Clone, Default)]
pub(super) struct StopwatchState {
    /// Time accrued in prior runs (start→stop→start cycles add up
    /// here). Reset to zero on `r`.
    pub(super) accumulated: Duration,
    /// `Some(start)` when running; `None` when paused/stopped.
    pub(super) started_at: Option<SystemTime>,
    /// Lap markers captured by `l`. Each is the *total elapsed*
    /// reading at the moment the user pressed `l` (not a delta from
    /// the previous lap) — matches how physical stopwatches display
    /// laps as cumulative timestamps. Cleared on `r`; preserved on
    /// stop, restart, and across app shutdown.
    pub(super) laps: Vec<Duration>,
}

impl StopwatchState {
    pub(super) fn elapsed(&self) -> Duration {
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
    pub(super) fn running(&self) -> bool {
        self.started_at.is_some()
    }
    pub(super) fn toggle(&mut self) {
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
    pub(super) fn reset(&mut self) {
        self.accumulated = Duration::ZERO;
        self.laps.clear();
        if self.started_at.is_some() {
            self.started_at = Some(SystemTime::now());
        }
    }

    /// Record a lap at the current elapsed reading. Returns `true`
    /// when accepted; `false` when the stopwatch isn't running or
    /// the per-session cap is reached.
    pub(super) fn record_lap(&mut self) -> bool {
        if !self.running() || self.laps.len() >= MAX_LAPS {
            return false;
        }
        self.laps.push(self.elapsed());
        true
    }
}

/// Format a `Duration` as `HH:MM:SS`, hours capped to 99 so the big-
/// digit renderer always sees a 2-character hour field. A 100h+
/// stopwatch saturates at "99:59:59" rather than blowing the layout.
pub(super) fn format_hms(d: Duration) -> String {
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
pub(super) fn format_hms_ms(d: Duration) -> String {
    let total = d.as_secs();
    let h = (total / 3600).min(99);
    let m = (total % 3600) / 60;
    let s = total % 60;
    let ms = d.subsec_millis();
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

impl ClockWidget {
    pub(super) fn render_stopwatch_body(&self, frame: &mut Frame, inner: Rect, tier: ViewTier) {
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
        //
        // At Full tier, the compact scrollable list is replaced by a
        // structured table with a header row and fastest/slowest
        // styling. The scroll offset is not used in the table (it
        // auto-shows the most recent laps), so last_laps_max_scroll
        // is set to 0 — j/k return Ignored at Full tier, which is
        // correct since the table is not user-scrollable.
        let (laps_max_scroll, applied_scroll) =
            if inner.height <= top_h + 2 {
                (0usize, 0usize)
            } else {
                let laps_rect = Rect {
                    x: inner.x,
                    y: inner.y + top_h + 1,
                    width: inner.width,
                    height: inner.height - top_h - 2,
                };
                if tier == ViewTier::Full {
                    // Full-tier: structured table regardless of lap count
                    // (shows header + "no laps recorded" when empty).
                    self.render_stopwatch_lap_table(frame, laps_rect, &laps);
                    (0usize, 0usize)
                } else if laps.is_empty() {
                    (0usize, 0usize)
                } else {
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
                }
            };
        let mut st = self.state.lock().expect("clock state poisoned");
        st.last_laps_max_scroll = laps_max_scroll;
        st.laps_scroll = applied_scroll;
    }

    /// Render the Full-tier structured lap table inside `area`.
    ///
    /// Shows a column header row ("Lap  Split  Total") followed by lap
    /// rows formatted as `" NN  HH:MM:SS.mmm  HH:MM:SS.mmm"`. The
    /// fastest lap (minimum split) is painted in `text_focused` (bright);
    /// the slowest (maximum split, only meaningful with ≥ 2 laps) is
    /// painted in `text_dim`. All other laps render at default style.
    ///
    /// When there are more laps than vertical rows, the most recent laps
    /// are shown (the table auto-tails), with a "↑ N earlier laps" cue
    /// in the first row. This matches the behavior a developer expects
    /// from a live timing tool — the freshest split is always visible.
    fn render_stopwatch_lap_table(
        &self,
        frame: &mut Frame,
        area: Rect,
        laps: &[Duration],
    ) {
        if area.height == 0 {
            return;
        }
        let pane_h = area.height as usize;

        // Compute per-lap splits (gap from the previous lap, or from zero
        // for the first lap) and find fastest/slowest indices.
        let splits: Vec<Duration> = (0..laps.len())
            .map(|i| {
                let prev = if i == 0 { Duration::ZERO } else { laps[i - 1] };
                laps[i].checked_sub(prev).unwrap_or(Duration::ZERO)
            })
            .collect();
        let fastest_idx = splits
            .iter()
            .enumerate()
            .min_by_key(|(_, d)| d.as_nanos())
            .map(|(i, _)| i);
        // Slowest is only meaningful when there are ≥ 2 laps; with a
        // single lap fastest == slowest and marking it both ways is noisy.
        let slowest_idx = if splits.len() >= 2 {
            splits
                .iter()
                .enumerate()
                .max_by_key(|(_, d)| d.as_nanos())
                .map(|(i, _)| i)
        } else {
            None
        };

        let mut lines: Vec<Line<'static>> = Vec::with_capacity(pane_h);

        // Header row — always rendered regardless of lap count so the
        // user sees the column structure even before the first lap.
        lines.push(Line::from(Span::styled(
            " Lap  Split            Total".to_string(),
            self.theme.text_dim,
        )));

        if laps.is_empty() {
            if pane_h > 1 {
                lines.push(Line::from(Span::styled(
                    " no laps recorded".to_string(),
                    self.theme.text_dim,
                )));
            }
        } else {
            // Rows available after the header. If more laps than fit,
            // show the most recent (tail) with a "↑ N earlier" cue.
            let avail = pane_h.saturating_sub(1);
            let total = laps.len();
            let (start, show_above_cue) = if total <= avail {
                (0, false)
            } else {
                // Reserve one row for the "↑ N earlier" cue.
                let start = total - avail.saturating_sub(1);
                (start, true)
            };

            if show_above_cue {
                lines.push(Line::from(Span::styled(
                    format!("  ↑ {} earlier laps", start),
                    self.theme.text_dim,
                )));
            }

            for i in start..total {
                if lines.len() >= pane_h {
                    break;
                }
                let num = i + 1;
                let split = splits[i];
                let total_elapsed = laps[i];
                let row_text = format!(
                    " {:>2}   {}   {}",
                    num,
                    format_hms_ms(split),
                    format_hms_ms(total_elapsed),
                );
                let style = if fastest_idx == Some(i) {
                    self.theme.text_focused
                } else if slowest_idx == Some(i) {
                    self.theme.text_dim
                } else {
                    Style::default()
                };
                lines.push(Line::from(Span::styled(row_text, style)));
            }
        }

        // Center the table as a block under the (centered) stopwatch time:
        // find the widest line and place the whole column set in a centered
        // sub-rect, left-aligned inside it so the columns stay aligned.
        let block_width = lines
            .iter()
            .map(|l| l.width() as u16)
            .max()
            .unwrap_or(0)
            .min(area.width);
        let x = area.x + area.width.saturating_sub(block_width) / 2;
        let centered = Rect { x, y: area.y, width: block_width, height: area.height };
        frame.render_widget(Paragraph::new(lines).alignment(Alignment::Left), centered);
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

    pub(super) fn handle_key_stopwatch_mode(&mut self, key: KeyEvent) -> EventResult {
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
    pub(super) fn scroll_laps(&self, delta: i32) -> EventResult {
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
}
