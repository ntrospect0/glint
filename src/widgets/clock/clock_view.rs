// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Clock-mode rendering and helpers — the big-digit time face, the
//! optional ticker line, the date row, and the scrollable World
//! Clocks block beneath. Also owns the transient `:time <location>`
//! override (geocoding spawn, clear, snapshot) since those mutate
//! state that only the clock view reads.
//!
//! Per-mode key handling lives in `state.rs` (`handle_key_clock_mode`)
//! so that the mode tab strip and tick-state machine can sit next to
//! the cross-mode pieces they coordinate.

use chrono::{DateTime, Datelike, Local, TimeZone, Timelike};
use chrono_tz::Tz;
use ratatui::{
    layout::{Alignment, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};

use crate::ui::{big_digits, CardGrid};
use crate::widgets::ViewTier;

use super::{ClockWidget, EventResult};

impl ClockWidget {
    /// `(Option<(full_label, _city, tz)>, searching)`. The full label
    /// is what the title row shows; callers wanting just the city
    /// name pull the second tuple element directly.
    pub(super) fn snapshot_transient(&self) -> (Option<(String, String, Tz)>, bool) {
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
            .map(|(_, _, tz)| *tz)
            .or(self.tz)
    }

    pub(super) fn lookup_location(&self, query: &str) {
        {
            let mut st = self.state.lock().expect("clock state poisoned");
            st.transient_searching = true;
            // Setting an override prepends Local + the override onto the
            // world-clocks list, so any prior scroll offset no longer points
            // at the same entry — reset to the top for predictability.
            // Same reasoning for the selection cursor: indices shift, so
            // drop the cursor rather than leave it on a now-stale row.
            st.world_clock_scroll = 0;
            st.world_clock_selected = None;
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
                            st.transient_tz = Some((loc.label.clone(), loc.city.clone(), tz));
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

    pub(super) fn clear_transient(&self) {
        let mut st = self.state.lock().expect("clock state poisoned");
        st.transient_tz = None;
        // Same reasoning as `lookup_location` — the list shape changes back,
        // so reset the offset + selection rather than leave them pointing
        // somewhere stale.
        st.world_clock_scroll = 0;
        st.world_clock_selected = None;
    }

    /// Move the world-clocks view by `delta` rows (negative = up). Returns
    /// `Handled` only when scrolling is actually possible — when the full
    /// list already fits, ↑/↓ and mouse-wheel fall through so the event can
    /// reach a higher-level handler.
    pub(super) fn scroll_world_clocks(&self, delta: i32) -> EventResult {
        let mut st = self.state.lock().expect("clock state poisoned");
        if st.world_clock_max_scroll == 0 {
            return EventResult::Ignored;
        }
        let max = st.world_clock_max_scroll;
        let next = (st.world_clock_scroll as i32 + delta).clamp(0, max as i32);
        st.world_clock_scroll = next as usize;
        EventResult::Handled
    }

    /// Inclusive `(min, max)` range of selectable absolute indices
    /// into [`world_clock_entries`]. Primary (idx 0) and the
    /// `:time`/`:clock` transient row (idx 1 when present) are
    /// excluded — only configured secondary timezones are
    /// navigable. `None` when there are no secondaries to land on.
    fn selectable_world_clock_range(&self) -> Option<(usize, usize)> {
        if self.secondaries.is_empty() {
            return None;
        }
        let has_transient = self
            .state
            .lock()
            .expect("clock state poisoned")
            .transient_tz
            .is_some();
        let start = if has_transient { 2 } else { 1 };
        let end = start + self.secondaries.len() - 1;
        Some((start, end))
    }

    /// j/k handler. First press materializes the selector at the
    /// first secondary row; subsequent presses move it within the
    /// selectable range. Render auto-scrolls the list to keep the
    /// selected row visible.
    pub(super) fn move_world_clock_selection(&self, delta: i32) -> EventResult {
        let Some((min, max)) = self.selectable_world_clock_range() else {
            return EventResult::Ignored;
        };
        let mut st = self.state.lock().expect("clock state poisoned");
        let next = match st.world_clock_selected {
            None => min,
            Some(cur) => (cur as i32 + delta).clamp(min as i32, max as i32) as usize,
        };
        // Re-clamp in case the secondaries list shrank since the
        // last navigation (e.g. user just removed a row); a stale
        // index outside the new range would otherwise highlight the
        // wrong row on the next render.
        let clamped = next.clamp(min, max);
        st.world_clock_selected = Some(clamped);
        EventResult::Handled
    }

    /// `-` handler. Opens the confirm modal for the selected
    /// secondary row. No-op when no row is selected or the
    /// selected index isn't a secondary (defense in depth — the
    /// key handler should already have routed `-` only to
    /// secondaries).
    pub(super) fn request_remove_selected(&self) -> EventResult {
        let Some((min, max)) = self.selectable_world_clock_range() else {
            return EventResult::Ignored;
        };
        let sel = match self
            .state
            .lock()
            .expect("clock state poisoned")
            .world_clock_selected
        {
            Some(i) if i >= min && i <= max => i,
            _ => return EventResult::Ignored,
        };
        let sec_idx = sel - min;
        let (label, tz) = match self.secondaries.get(sec_idx) {
            Some(s) => (s.0.clone(), s.1.name().to_string()),
            None => return EventResult::Ignored,
        };
        self.state
            .lock()
            .expect("clock state poisoned")
            .confirm_remove = Some((label, tz));
        EventResult::Handled
    }

    /// Apply a pending remove (set by [`Self::request_remove_selected`]).
    /// Mutates `config.secondary_timezones`, rebuilds the parsed
    /// `secondaries` list, persists the change to clock.toml, and
    /// re-clamps the selection / scroll so the surviving rows stay
    /// reachable.
    pub(super) fn confirm_remove_world_clock(&mut self) {
        let target_tz = match self
            .state
            .lock()
            .expect("clock state poisoned")
            .confirm_remove
            .clone()
        {
            Some((_, tz)) => tz,
            None => return,
        };
        let before = self.config.secondary_timezones.len();
        self.config
            .secondary_timezones
            .retain(|s| s.timezone != target_tz);
        let removed = self.config.secondary_timezones.len() < before;
        // Always clear the modal slot; if nothing matched we still
        // want the user dismissed back to the main view rather than
        // stuck looking at a confirm overlay for a missing row.
        self.state
            .lock()
            .expect("clock state poisoned")
            .confirm_remove = None;
        if !removed {
            return;
        }
        self.secondaries.retain(|(_, tz)| tz.name() != target_tz);
        self.persist_secondary_timezones();
        // Re-clamp the selection. After removal the secondaries
        // list is shorter; if the cursor was past the new end we
        // pin it to the last surviving row; if it was the only row
        // we clear the cursor entirely.
        let new_range = self.selectable_world_clock_range();
        let mut st = self.state.lock().expect("clock state poisoned");
        st.world_clock_selected = match (new_range, st.world_clock_selected) {
            (Some((min, max)), Some(cur)) => Some(cur.clamp(min, max)),
            (Some((min, _)), None) => Some(min),
            (None, _) => None,
        };
        // World-clock list shrank, so any cached max-scroll is stale.
        // Render re-derives it on next frame; this clamp keeps the
        // intermediate state self-consistent.
        st.world_clock_scroll = 0;
        st.world_clock_max_scroll = 0;
    }

    pub(super) fn cancel_remove_world_clock(&self) {
        self.state
            .lock()
            .expect("clock state poisoned")
            .confirm_remove = None;
    }

    /// `+` handler. Adds the active `:time`/`:clock` transient
    /// lookup to the permanent secondary list, persists, clears
    /// the transient (the row now lives in the secondaries block),
    /// and lands the selector on the freshly-added entry.
    pub(super) fn add_transient_to_world_clocks(&mut self) -> EventResult {
        let (city, tz) = {
            let st = self.state.lock().expect("clock state poisoned");
            match &st.transient_tz {
                Some((_full, city, tz)) => (city.clone(), *tz),
                None => return EventResult::Ignored,
            }
        };
        let tz_name = tz.name().to_string();
        // Skip if the same IANA zone is already in the list — adding
        // a duplicate would leave the secondaries list with two rows
        // that tick in lockstep and confuse the user.
        if self
            .config
            .secondary_timezones
            .iter()
            .any(|s| s.timezone == tz_name)
        {
            // Still clear the transient so `:clock <city>` on an
            // already-tracked city resolves to "you're done" rather
            // than a stuck lookup row.
            let mut st = self.state.lock().expect("clock state poisoned");
            st.transient_tz = None;
            return EventResult::Handled;
        }
        self.config
            .secondary_timezones
            .push(super::config::SecondaryTimezone {
                label: city.clone(),
                timezone: tz_name.clone(),
            });
        self.secondaries.push((city, tz));
        self.persist_secondary_timezones();
        let new_range = self.selectable_world_clock_range();
        let mut st = self.state.lock().expect("clock state poisoned");
        st.transient_tz = None;
        // Selection lands on the newly-added (now last) entry so
        // the user sees a clear confirmation that the add stuck.
        if let Some((_, max)) = new_range {
            st.world_clock_selected = Some(max);
        }
        st.world_clock_scroll = 0;
        st.world_clock_max_scroll = 0;
        EventResult::Handled
    }

    /// Returns (HH:MM[:SS], AM/PM, date) for the effective primary timezone.
    pub(super) fn render_strings(&self, now_utc: DateTime<chrono::Utc>) -> (String, String, String) {
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

    pub(super) fn ticker_string(&self, now_utc: DateTime<chrono::Utc>) -> String {
        match self.effective_tz() {
            Some(tz) => format_ticker(now_utc.with_timezone(&tz), self.config.hour_format),
            None => format_ticker(now_utc.with_timezone(&Local), self.config.hour_format),
        }
    }

    /// Returns `(label, "HH:MM", "Wkd Mon DD")` triples for the Full-tier
    /// big-digit grid.
    ///
    /// Only the *secondary* world-clock zones are returned — the primary
    /// (home) zone is already shown by the big-digit face at the top of the
    /// widget and must not be repeated as a grid cell. Each triple carries
    /// the zone's own local day-of-week and date so the user can tell at a
    /// glance when a secondary is on a different calendar day than home.
    pub(super) fn world_clock_secondary_grid_entries(
        &self,
    ) -> Vec<(String, String, String, &'static str)> {
        let now = chrono::Utc::now();
        let mut out: Vec<(String, String, String, &'static str)> =
            Vec::with_capacity(self.secondaries.len());
        for (label, tz) in &self.secondaries {
            let t = now.with_timezone(tz);
            let hhmm = format!("{:02}:{:02}", t.hour(), t.minute());
            let day_date = format!(
                "{} {} {}",
                weekday_name(t.weekday()),
                month_name(t.month()),
                t.day()
            );
            out.push((label.clone(), hhmm, day_date, day_night_icon(t.hour())));
        }
        out
    }

    /// Returns (label, "HH:MM Wkd Mon DD") pairs for the World Clocks block.
    /// Primary timezone leads, then any configured secondaries. Each entry
    /// carries its own local date so the user can tell when a clock is on a
    /// different calendar day than local time without having to do timezone
    /// arithmetic in their head.
    pub(super) fn world_clock_entries(&self) -> Vec<(String, String)> {
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
            // World-clocks list uses the short city name (second
            // element of the transient triple). Full label stays
            // available to the title-bar metadata path. Embedded
            // commas in compound city names (e.g. "Washington, D.C.")
            // are preserved by the geocoder and pass through here.
            Some((_full_label, city, tz)) => {
                let t = now.with_timezone(&tz);
                (city, format_clock_entry(&t))
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

    /// Body renderer for the Clock mode — the original big-digit time
    /// + ticker + date + world clocks layout, factored out of the
    /// top-level `render` so the new tab strip + mode dispatch can
    /// share the chrome (title row, border, mode tabs).
    ///
    /// At `ViewTier::Full` the world-clocks block is replaced by a
    /// multi-column side-by-side grid so all configured zones are
    /// visible at once without scrolling. At every other tier the
    /// existing scrollable vertical list renders unchanged.
    pub(super) fn render_clock_body(
        &self,
        frame: &mut Frame,
        inner: Rect,
        transient: Option<&(String, String, Tz)>,
        tier: ViewTier,
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

        // ── Full-tier: large-digit multi-TZ grid ──────────────────────
        //
        // At Full, split the body into a top rect (big digits + ticker +
        // date, rendered as before) and a bottom rect (the PinnedGrid of
        // per-zone large-digit clocks). The scrollable list path is
        // completely bypassed; world_clock_max_scroll is set by the grid
        // itself based on how many zones fit. Scroll keys delegate to
        // scroll_world_clocks(), which returns Ignored when max_scroll == 0.
        if tier == ViewTier::Full {
            let top_h = lines.len() as u16;
            let top_rect = Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: top_h.min(inner.height),
            };
            frame.render_widget(
                Paragraph::new(lines).alignment(Alignment::Center),
                top_rect,
            );
            // One blank row between the primary face's day/date line and the
            // timezone card grid below.
            const TOP_GRID_GAP: u16 = 1;
            if inner.height > top_h + TOP_GRID_GAP {
                let grid_rect = Rect {
                    x: inner.x,
                    y: inner.y + top_h + TOP_GRID_GAP,
                    width: inner.width,
                    height: inner.height - top_h - TOP_GRID_GAP,
                };
                let entries = self.world_clock_secondary_grid_entries();
                self.render_full_world_clock_grid(frame, grid_rect, &entries);
            }
            return;
        }

        // World clocks block — show as many entries as fit, scroll the rest
        // with ↑/↓ and mouse-wheel. Primary timezone leads so the user can
        // see local time alongside the rest of the world. The contextual
        // footer (revert/add/remove hints — see the hint-build block
        // below) eats the bottom row, so the available height for the
        // body shrinks by 1 in that case — factor that into the fit
        // calculation, otherwise the last clock entry would be clipped
        // by the footer.
        let clocks = self.world_clock_entries();
        let selection_active = self
            .state
            .lock()
            .expect("clock state poisoned")
            .world_clock_selected
            .is_some();
        let show_footer = transient.is_some() || selection_active;
        let body_h = if show_footer {
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
                let (scroll, selected) = {
                    let mut st = self.state.lock().expect("clock state poisoned");
                    st.world_clock_max_scroll = max_scroll;
                    // Auto-scroll: if the selected row would land
                    // outside the visible window, slide the window
                    // so it's in view. j/k driven movement happens
                    // in the key handler — this is just the
                    // viewport-adjust half that needs the render-
                    // time geometry.
                    if let Some(sel) = st.world_clock_selected {
                        if sel < st.world_clock_scroll {
                            st.world_clock_scroll = sel;
                        } else if sel >= st.world_clock_scroll + visible_count {
                            st.world_clock_scroll = sel + 1 - visible_count;
                        }
                    }
                    if st.world_clock_scroll > max_scroll {
                        st.world_clock_scroll = max_scroll;
                    }
                    (st.world_clock_scroll, st.world_clock_selected)
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

                // Reserve enough width for the longest time column
                // (icon + HH:MM + weekday + month + day, ~18 cells).
                // The label column gets whatever inner.width leaves
                // over after the 2-cell gap and 2-cell selector
                // prefix; long city names get truncated with an
                // ellipsis so the time always stays on screen.
                let time_w = clocks
                    .iter()
                    .map(|(_, t)| t.chars().count())
                    .max()
                    .unwrap_or(0);
                let widest_label = clocks
                    .iter()
                    .map(|(l, _)| l.chars().count())
                    .max()
                    .unwrap_or(0);
                // 2 cells for the leading "▸ " / "  " selector
                // marker, 2 cells of gap between label and time.
                let available_label_w =
                    (inner.width as usize).saturating_sub(time_w + 4);
                let max_label = widest_label.min(available_label_w).max(1);
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
                    let prefix = if selected == Some(idx) { "▸ " } else { "  " };
                    let display_label = crate::text::truncate(label, max_label);
                    let line = format!(
                        "{}{:<width$}  {}",
                        prefix,
                        display_label,
                        time_str,
                        width = max_label
                    );
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

        // Build the footer hint from whatever contextual shortcuts
        // are currently meaningful: revert / add when a `:time`
        // lookup is active, remove when the selector cursor is
        // on a secondary row. Empty when neither applies, in which
        // case the body claims the full inner area.
        let mut hints: Vec<&str> = Vec::with_capacity(3);
        if transient.is_some() {
            hints.push("x revert to Local");
            hints.push("+ add timezone");
        }
        if selection_active {
            hints.push("- remove");
        }
        if !show_footer {
            let body = Paragraph::new(lines).alignment(Alignment::Center);
            frame.render_widget(body, inner);
        } else {
            let hint = Line::from(Span::styled(hints.join(" · "), self.theme.text_dim));
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
        }
    }

    /// Render secondary world-clock zones as a capped-width large-digit grid at Full tier.
    ///
    /// The home/primary zone is intentionally excluded — it is already displayed
    /// by the big-digit face at the top of the widget. Only the configured
    /// secondary zones appear here.
    ///
    /// Each cell is rendered as a bordered card (all four sides). Inside the card:
    /// - Row 0: top padding
    /// - Rows 1–5: big-digit "HH:MM" (5 glyph rows via `big_digits::render_styled`)
    /// - Row 6: city label (dim)
    /// - Row 7: day-of-week + date (e.g. "Tue Jul 8", dim)
    ///
    /// Card geometry:
    /// - max card width: `CARD_MAX_W` (40 cols) — never stretched beyond this.
    /// - min card width: `CELL_MIN_STRIDE` (29 cols) — keeps big-digit block intact.
    /// - height: 8 content rows + 2 border rows = 10 total
    /// - 1-col gap between adjacent cards; group is horizontally centered.
    ///
    /// Layout is delegated to [`crate::ui::CardGrid`] (unpinned, multi-row).
    /// A partial last row is centered to its own card count.
    fn render_full_world_clock_grid(
        &self,
        frame: &mut Frame,
        area: Rect,
        entries: &[(String, String, String, &'static str)],
    ) {
        if area.height == 0 || entries.is_empty() {
            let mut st = self.state.lock().expect("clock state poisoned");
            st.world_clock_max_scroll = 0;
            st.world_clock_scroll = 0;
            return;
        }

        // "HH:MM" glyph width: 5 chars × 3 cols + 4 single-space separators = 19.
        const GLYPH_COLS: u16 = 19;
        // Preferred inner padding on each side of the glyph block.
        const PAD: u16 = 4;
        // Card border consumes 2 cols (left + right) and 2 rows (top + bottom).
        const BORDER: u16 = 2;
        // Minimum card width: inner glyph+pad space + border cols.
        const CELL_MIN_STRIDE: u16 = GLYPH_COLS + PAD * 2 + BORDER;
        // Maximum card width — cards never stretch beyond this.
        const CARD_MAX_W: u16 = 40;
        // 1-col gap between adjacent card borders.
        const INTER_CARD_GAP: u16 = 1;
        // Cell height: 2 border rows + 1 top-pad + 5 glyph rows + 1 blank
        // spacer + 1 label + 1 date.
        const CELL_ROWS: u16 = BORDER + 1 + big_digits::GLYPH_HEIGHT as u16 + 1 + 2;

        // Read the current scroll offset from state.
        let scroll_offset = {
            let st = self.state.lock().expect("clock state poisoned");
            st.world_clock_scroll
        };

        // Compute card positions via the shared CardGrid primitive.
        // pin_home = false: all entries scroll together (the home/primary zone
        // is already shown by the big-digit face above — only secondaries here).
        let grid_layout = CardGrid {
            area,
            card_max_w: CARD_MAX_W,
            card_min_w: CELL_MIN_STRIDE,
            cell_h: CELL_ROWS,
            gap: INTER_CARD_GAP,
            item_count: entries.len(),
            scroll_offset,
            pin_home: false,
        }
        .layout();

        // Persist max_scroll and clamped offset back to state.
        {
            let mut st = self.state.lock().expect("clock state poisoned");
            st.world_clock_max_scroll = grid_layout.max_scroll;
            let clamped = scroll_offset.min(grid_layout.max_scroll);
            st.world_clock_scroll = clamped;
        }

        if grid_layout.cells.is_empty() {
            return;
        }

        let gradient = self.state.lock().expect("clock state poisoned").gradient;
        let cell_style = self.theme.text_plain;

        for &(item_idx, cell_rect) in &grid_layout.cells {
            let entry = &entries[item_idx];
            if cell_rect.width == 0 || cell_rect.height == 0 {
                continue;
            }

            let (label, hhmm, day_date, glyph) = entry;

            // Draw the card border and obtain the inner content rect.
            let card = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(self.theme.border_style(false));
            let inner = card.inner(cell_rect);
            frame.render_widget(card, cell_rect);

            if inner.width == 0 || inner.height == 0 {
                continue;
            }

            // Row 0 (inner): top padding — empty line so glyphs breathe from the border.
            // Rows 1–5: big-digit time, centered.
            let digit_lines = big_digits::render_styled(hhmm, gradient, cell_style);
            for (row_offset, dline) in digit_lines.into_iter().enumerate() {
                let y = inner.y + 1 + row_offset as u16;
                if y >= inner.bottom() {
                    break;
                }
                frame.render_widget(
                    Paragraph::new(dline).alignment(Alignment::Center),
                    Rect::new(inner.x, y, inner.width, 1),
                );
            }

            // One blank spacer row between the digits and the label, then the
            // city label (centered, dim).
            let label_y = inner.y + 1 + big_digits::GLYPH_HEIGHT as u16 + 1;
            if label_y < inner.bottom() {
                // City name with the zone's day/night glyph to its right.
                // Reserve 2 cols (" ☀") so the glyph survives truncation.
                let city = crate::text::truncate(
                    label,
                    (inner.width as usize).saturating_sub(2),
                );
                let label_span = Span::styled(
                    format!("{city} {glyph}"),
                    self.theme.text_dim,
                );
                frame.render_widget(
                    Paragraph::new(Line::from(label_span)).alignment(Alignment::Center),
                    Rect::new(inner.x, label_y, inner.width, 1),
                );
            }

            // Row 7 (inner): day + date, centered and dim.
            let date_y = label_y + 1;
            if date_y < inner.bottom() {
                let date_span = Span::styled(
                    crate::text::truncate(day_date, inner.width as usize),
                    self.theme.text_dim,
                );
                frame.render_widget(
                    Paragraph::new(Line::from(date_span)).alignment(Alignment::Center),
                    Rect::new(inner.x, date_y, inner.width, 1),
                );
            }
        }
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
pub(super) fn day_night_icon(hour: u32) -> &'static str {
    if (6..=17).contains(&hour) {
        "☀"
    } else {
        "☾"
    }
}

/// Convert an IANA timezone name like "America/Vancouver" into a friendly
/// label ("Vancouver"). Underscores become spaces.
pub(super) fn city_from_tz_name(name: &str) -> String {
    name.rsplit('/').next().unwrap_or(name).replace('_', " ")
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
