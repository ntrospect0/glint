// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Mutex-protected state for the calendar widget plus the methods
//! that touch it. Per-view rendering reads a snapshot of this
//! state through `snapshot_events`; key handlers and the polling
//! tick mutate it through the helpers here.

use std::sync::{
    atomic::Ordering,
    Arc,
};
use std::time::{Duration, Instant};

use chrono::{DateTime, Datelike, Duration as ChronoDuration, Local, NaiveDate};
use ratatui::layout::Rect;

use super::config::CalendarView;
use super::nav::{
    content_rect_for, first_of_next_month, local_midnight, start_of_month, start_of_week,
    WebTarget,
};
use super::provider::Event;
use super::CalendarWidget;
use super::AUTO_ROLL_FOCUSED_IDLE;
use super::STATUS_TTL;
use crate::ui::big_digits;
use crate::ui::status::{live_value, TimedFeedback};

pub(super) const CACHE_KEY_EVENTS: &str = "events";

/// Minimum gap between two horizontal-scroll-driven anchor jumps. A typical
/// trackpad flick (~300ms of ~30 events) should produce one navigation
/// step; slow deliberate scroll still feeds steady steps.
pub(super) const HORIZONTAL_SCROLL_COOLDOWN: Duration = Duration::from_millis(200);

/// Window during which horizontal-scroll events are dropped after any
/// vertical scroll. macOS trackpads emit a few micro horizontal events
/// per vertical gesture — those are jitter, not intent. Generous enough
/// that the entire vertical gesture is covered, tight enough that a
/// deliberate horizontal flick after the user clearly stops vertical
/// scrolling still gets through.
pub(super) const VERTICAL_AXIS_LOCK_WINDOW: Duration = Duration::from_millis(700);

/// Whether the day-rollover snap should fire now. Pure so the focus/
/// idle gating is unit-testable without a wall clock:
/// - `today <= rollover_date` → `false` (no new day; or the clock ran
///   backward, which the caller resyncs separately).
/// - focused and active within [`AUTO_ROLL_FOCUSED_IDLE`] → `false`
///   (defer so the view doesn't jump mid-interaction).
/// - otherwise → `true`.
///
/// What to do *when* it fires (snap to today vs. leave a past view
/// frozen) is the caller's call — see `maybe_auto_roll`.
pub(super) fn auto_roll_due(
    today: NaiveDate,
    rollover_date: NaiveDate,
    focused: bool,
    idle: Duration,
) -> bool {
    if today <= rollover_date {
        return false;
    }
    !(focused && idle < AUTO_ROLL_FOCUSED_IDLE)
}

#[derive(Default)]
pub(super) struct CalendarState {
    /// `Arc<Event>` so the per-render `Vec::clone()` is O(N) atomic
    /// increments instead of O(N) deep Event copies. With month view
    /// holding 100+ events × 5 Strings each, this drops ~500 String
    /// allocations per dashboard redraw to ~100 refcount bumps.
    pub(super) events: Vec<Arc<Event>>,
    pub(super) last_error: Option<String>,
    pub(super) poll: crate::polling::PollTracker,
    pub(super) inflight: bool,
    /// `(start, end)` of the data span the most recent successful
    /// fetch covered — the *fetch* range, which is wider than the
    /// displayed range thanks to the buffer added in `fetch_range`.
    /// Navigation that lands inside this span doesn't trigger a
    /// refetch (`mark_dirty_if_uncovered`), so scrolling through
    /// recently-visited days is instant.
    pub(super) last_fetched_range: Option<(DateTime<Local>, DateTime<Local>)>,
    /// Active big-digit gradient. Seeded from config; user cycles with `g`.
    pub(super) gradient: big_digits::Gradient,
    /// Row offset for the day's agenda list (Day view body + Month
    /// view's selected-day footer). ↑/↓ keys and mouse wheel drive this;
    /// reset to 0 whenever the anchor day or view changes so the user
    /// doesn't get stranded mid-list after navigating.
    pub(super) agenda_scroll: u16,
    /// Last-known maximum scroll offset for the agenda — written by
    /// render after measuring lines vs viewport, read by the scroll
    /// handler so it can clamp without re-running the layout.
    pub(super) agenda_scroll_max: u16,
    /// False until the agenda has been auto-scrolled once for the
    /// current (anchor, view) pair. Render flips this to true after
    /// it places the viewport on "now or later" events; manual
    /// scrolling (keys / wheel) also sets it so render stops fighting
    /// the user. Inverted (done-flag vs pending-flag) so the
    /// `#[derive(Default)]` default of `false` means "needs an
    /// autoscroll pass on the next render", which matches the right
    /// behavior on first construction without a manual override.
    pub(super) agenda_autoscroll_done: bool,
    /// Per-column scroll offset for Week view — index 0 is the leftmost
    /// day (Sunday), index 6 the rightmost (Saturday). Wheel scrolling
    /// over a specific day in Week view drives one entry; columns are
    /// independent so scrolling Monday doesn't shift Friday's events.
    pub(super) week_col_scroll: [u16; 7],
    /// Last-known maximum scroll for each Week-view day column,
    /// written by render after laying out each column's events vs the
    /// available height. The wheel handler clamps against these so
    /// scrolling past the end of one column doesn't accumulate state
    /// that survives a re-render.
    pub(super) week_col_scroll_max: [u16; 7],
    /// Display-state dirty bit drained by `take_dirty`. Set true by
    /// every async-task / tick-time mutation site so the main loop's
    /// dirty-flag gate triggers a redraw.
    pub(super) dirty: bool,
    /// Transient title-bar status (e.g. open-failed warning, "no
    /// web-viewable calendar" notice). Cleared after `STATUS_TTL`.
    pub(super) status: Option<TimedFeedback<String>>,
    /// Open-in-browser picker. `Some(targets)` when the user pressed
    /// `o` and more than one provider is configured — render shows a
    /// numbered modal; the next 1–N keypress opens the chosen URL,
    /// any other key cancels.
    pub(super) open_picker: Option<Vec<WebTarget>>,
}

impl CalendarWidget {
    pub(super) fn is_due(&self) -> bool {
        let st = self.state.lock().expect("calendar state poisoned");
        if st.inflight {
            return false;
        }
        st.poll.is_due()
    }

    /// Has the events vec settled to the current view's range? `false`
    /// while a refresh is in flight or pending (`mark_dirty` clears
    /// the tracker); render uses this to decide between showing
    /// "No events." and leaving the agenda blank during the brief
    /// window where the events vec is from the previously-fetched
    /// range and might be spuriously empty for the new anchor.
    pub(super) fn agenda_data_loaded(&self) -> bool {
        let st = self.state.lock().expect("calendar state poisoned");
        !st.inflight && st.poll.has_attempted()
    }

    /// Range we ask the provider for — `current_range` widened by a
    /// per-view buffer so adjacent days / weeks / months land in the
    /// same fetch and navigation within that buffer is instant.
    /// Without the buffer, every `h` / `l` press in Day view forced
    /// a refetch (Day's `current_range` is only the anchor + preview
    /// = 2 days), so even days the user *just* visited weren't in
    /// `state.events` anymore.
    pub(super) fn fetch_range(&self) -> (DateTime<Local>, DateTime<Local>) {
        let (start, end) = self.current_range();
        let buffer = match self.view {
            // ±2 weeks lets the user step through a month of days
            // without re-hitting the provider.
            CalendarView::Day => ChronoDuration::days(14),
            // Same buffer; a full extra month each side fits in one fetch.
            CalendarView::Week => ChronoDuration::days(14),
            // ±1 month so Month-view's ←/→ across adjacent months is
            // also covered.
            CalendarView::Month => ChronoDuration::days(31),
        };
        (start - buffer, end + buffer)
    }

    /// True when the most recent successful fetch covers the current
    /// display range. Used by `mark_dirty_if_uncovered` to skip the
    /// forced refresh that fires on every anchor / view change — most
    /// navigations within the fetch buffer are already covered.
    pub(super) fn current_range_covered(&self) -> bool {
        let (display_start, display_end) = self.current_range();
        let st = self.state.lock().expect("calendar state poisoned");
        match st.last_fetched_range {
            Some((s, e)) => display_start >= s && display_end <= e,
            None => false,
        }
    }

    /// Like `mark_dirty`, but only fires when the new display range
    /// isn't already covered by the last successful fetch. Anchor and
    /// view changes route through this so navigation within the fetch
    /// buffer doesn't trigger spurious refetches. Background polling
    /// still catches stale data on its own cadence.
    pub(super) fn mark_dirty_if_uncovered(&self) {
        if self.current_range_covered() {
            return;
        }
        let mut st = self.state.lock().expect("calendar state poisoned");
        st.poll.mark_dirty();
    }

    pub(super) fn current_range(&self) -> (DateTime<Local>, DateTime<Local>) {
        let (start, end) = match self.view {
            // Fetch two days when in Day view: the wide layout previews the
            // *next* day next to the anchor, and we don't want it to render
            // "No events" just because the next day's events weren't fetched.
            CalendarView::Day => (self.anchor, self.anchor + ChronoDuration::days(2)),
            CalendarView::Week => {
                let s = start_of_week(self.anchor, self.first_day_of_week);
                (s, s + ChronoDuration::days(7))
            }
            CalendarView::Month => {
                let s = start_of_month(self.anchor);
                let e = first_of_next_month(self.anchor);
                (s, e)
            }
        };
        (
            local_midnight(start).expect("midnight is valid"),
            local_midnight(end).expect("midnight is valid"),
        )
    }

    pub(super) fn spawn_refresh(&self) {
        let (start, end) = self.fetch_range();
        {
            let mut st = self.state.lock().expect("calendar state poisoned");
            st.inflight = true;
            st.poll.mark_attempted();
            st.dirty = true;
        }
        let provider = self.provider.clone();
        let state = self.state.clone();
        let cache = self.cache.clone();
        tokio::spawn(async move {
            let result = provider.fetch_range(start, end).await;
            let mut st = state.lock().expect("calendar state poisoned");
            st.inflight = false;
            st.dirty = true;
            match result {
                Ok(events) => {
                    if let Err(err) = cache.store(CACHE_KEY_EVENTS, &events) {
                        tracing::warn!(error = %err, "calendar cache store failed");
                    }
                    st.events = events.into_iter().map(Arc::new).collect();
                    st.last_fetched_range = Some((start, end));
                    st.last_error = None;
                }
                Err(err) => {
                    tracing::warn!(error = %err, "calendar fetch failed");
                    st.last_error = Some(err.to_string());
                }
            }
        });
    }

    /// Reset the agenda scroll offset to the top and re-arm the
    /// auto-scroll-to-now pass. Called whenever the anchor day or
    /// the view mode changes so the user doesn't land mid-list after
    /// navigating to a different day — the next render will then
    /// re-position to the first event at-or-after the current time
    /// (when the displayed day is today).
    pub(super) fn reset_agenda_scroll(&self) {
        let mut st = self.state.lock().expect("calendar state poisoned");
        st.agenda_scroll = 0;
        st.agenda_scroll_max = 0;
        st.agenda_autoscroll_done = false;
        // Per-day Week-view offsets reset alongside the shared agenda
        // scroll — navigating to a different week means the previous
        // week's column-by-column scroll positions are no longer
        // meaningful for the new dates.
        st.week_col_scroll = [0; 7];
        st.week_col_scroll_max = [0; 7];
    }

    /// Does the currently-shown range cover today? Day view checks
    /// anchor-equals-today; Week view checks today falls inside the
    /// Sun..=Sat window containing the anchor; Month view checks
    /// today's calendar month matches the anchor's. Drives the
    /// `[Today]` button's lit-vs-dim styling so the user can tell
    /// at a glance whether jumping to today would change the view.
    pub(super) fn current_view_contains_today(&self) -> bool {
        let today = Local::now().date_naive();
        match self.view {
            CalendarView::Day => self.anchor == today,
            CalendarView::Week => {
                let start = start_of_week(self.anchor, self.first_day_of_week);
                let end = start + ChronoDuration::days(6);
                today >= start && today <= end
            }
            CalendarView::Month => {
                today.year() == self.anchor.year() && today.month() == self.anchor.month()
            }
        }
    }

    /// Accept-or-drop gate for ScrollLeft/Right. Returns `true` only when
    /// the event represents real horizontal intent: a vertical scroll
    /// hasn't fired recently (axis-lock filters out the micro horizontal
    /// events macOS trackpads emit alongside any vertical gesture) AND
    /// enough time has elapsed since the last accepted horizontal scroll
    /// to take another step (burst-debounce — a trackpad flick emits
    /// ~30 events in 300ms; without this gate one flick would skip 20+
    /// days at once).
    pub(super) fn consume_horizontal_scroll(&mut self) -> bool {
        let now = Instant::now();
        if let Some(prev_vert) = self.last_vertical_scroll {
            if now.duration_since(prev_vert) < VERTICAL_AXIS_LOCK_WINDOW {
                return false;
            }
        }
        if let Some(prev) = self.last_horizontal_scroll {
            if now.duration_since(prev) < HORIZONTAL_SCROLL_COOLDOWN {
                return false;
            }
        }
        self.last_horizontal_scroll = Some(now);
        true
    }

    /// Distance one ←/→ keystroke advances the anchor by. View-
    /// dependent: Day → 1 day, Week → 7 days, Month → ~30 days.
    pub(super) fn nav_step(&self) -> ChronoDuration {
        match self.view {
            CalendarView::Day => ChronoDuration::days(1),
            CalendarView::Week => ChronoDuration::days(7),
            CalendarView::Month => ChronoDuration::days(30),
        }
    }

    /// Adjust the agenda scroll by `delta` rows, clamping against the
    /// last-rendered maximum. Up/Down keys and mouse wheel route here.
    /// Also marks the autoscroll as done so render doesn't fight the
    /// user's manual position on the next tick.
    pub(super) fn scroll_agenda(&self, delta: i32) {
        let mut st = self.state.lock().expect("calendar state poisoned");
        let max = st.agenda_scroll_max as i32;
        let next = (st.agenda_scroll as i32 + delta).clamp(0, max);
        st.agenda_scroll = next as u16;
        st.agenda_autoscroll_done = true;
    }

    /// Week-view per-column scroll. Routes a wheel event over a specific
    /// day-of-week to that column's scroll offset so scrolling Monday
    /// doesn't shift Friday. `mouse_col` is the absolute terminal column;
    /// `area` is the cell rect handed to `handle_mouse`. Out-of-bounds
    /// clicks (e.g. inside the title bar) are silently dropped.
    pub(super) fn scroll_week_col(&self, mouse_col: u16, area: Rect, delta: i32) {
        if area.width < 2 || area.height < 2 {
            return;
        }
        let inner = Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2);
        let content = content_rect_for(CalendarView::Week, inner);
        if content.width == 0 || mouse_col < content.x || mouse_col >= content.x + content.width {
            return;
        }
        let dow =
            (((mouse_col - content.x) as u32 * 7) / content.width.max(1) as u32).min(6) as usize;
        let mut st = self.state.lock().expect("calendar state poisoned");
        let max = st.week_col_scroll_max[dow] as i32;
        let next = (st.week_col_scroll[dow] as i32 + delta).clamp(0, max);
        st.week_col_scroll[dow] = next as u16;
    }

    /// Keep an unattended calendar from going stale as the clock crosses
    /// midnight. Called every tick from `update`.
    ///
    /// When the local date moves past [`rollover_date`](CalendarWidget) —
    /// midnight, or several midnights after the machine slept — the
    /// anchor snaps home to the new today regardless of where it was
    /// (today, a future date, or a past date the user had navigated to),
    /// so an always-on dashboard returns to the live day. The snap is
    /// immediate when the widget is unfocused; when it's focused we hold
    /// off until [`AUTO_ROLL_FOCUSED_IDLE`] of no key/mouse activity so the
    /// view never jumps out from under someone actively using it. The
    /// gating decision lives in the pure [`auto_roll_due`] helper.
    ///
    /// `rollover_date` advances only here and on user reposition (see
    /// `handle_key` / `handle_mouse`), so a view the user repositioned
    /// *as of today* is left alone until the next unattended midnight.
    pub(super) fn maybe_auto_roll(&mut self) {
        let today = Local::now().date_naive();
        let focused = self.is_focused.load(Ordering::Relaxed);
        if !auto_roll_due(today, self.rollover_date, focused, self.last_activity.elapsed()) {
            // Resync the baseline downward if the clock moved backward (a
            // timezone shift / NTP correction) so we don't snap on the
            // rebound; otherwise leave everything untouched.
            if today < self.rollover_date {
                self.rollover_date = today;
            }
            return;
        }
        self.rollover_date = today;
        if self.anchor != today {
            self.anchor = today;
            self.reset_agenda_scroll();
            self.mark_dirty_if_uncovered();
            // Display dirty bit so the new day repaints — `update` runs
            // on a tick, otherwise gated out by `take_dirty`.
            self.state.lock().expect("calendar state poisoned").dirty = true;
        }
    }

    pub(super) fn snapshot_events(&self) -> Vec<Arc<Event>> {
        let st = self.state.lock().expect("calendar state poisoned");
        st.events.clone()
    }

    pub(super) fn set_status(&self, msg: impl Into<String>) {
        let mut st = self.state.lock().expect("calendar state poisoned");
        st.status = Some(TimedFeedback::new(msg.into(), STATUS_TTL));
        st.dirty = true;
        drop(st);
        self.feedback_pending.store(true, Ordering::Relaxed);
    }

    pub(super) fn live_status(&self) -> Option<String> {
        let mut st = self.state.lock().expect("calendar state poisoned");
        live_value(&mut st.status).cloned()
    }
}
