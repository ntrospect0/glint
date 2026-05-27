// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Periodic-fetch debounce primitive.
//!
//! Widgets that periodically pull data (stocks quotes, RSS feeds,
//! weather, calendar, email, …) all need the same three things:
//!
//! 1. An interval that says "how often do we want to refresh?"
//! 2. A monotonic timestamp of the last attempt so we can ask
//!    "is it time again?"
//! 3. A way to seed (2) from a cached entry so a freshly-launched
//!    glint with an on-disk cache doesn't fire a refresh in the
//!    first 250 ms tick.
//!
//! This module supplies that primitive once. The [`Widget`] trait
//! exposes a `poll_tracker()` hook so the platform can also see a
//! widget's current polling state — used today for tracing, useful
//! later for tick-deadline scheduling.
//!
//! Widgets that need radically different timing (push streams, IDLE
//! connections, manual triggers) simply don't construct a
//! `PollTracker` and don't implement the trait hook. The platform
//! treats them as "not polling" and stays out of their way.
//!
//! ## See also
//! `docs/widget-sdk.md` — the developer-facing capability writeup.

#![allow(dead_code)] // PollSnapshot + next_due_at are forward-looking platform surface

use std::{
    hash::{Hash, Hasher},
    time::{Duration, Instant},
};

/// Deterministic per-widget offset for `apply_jitter`. Returns a value
/// in `[0, max_jitter.as_millis())`. Uses `DefaultHasher` because we
/// only need *spread*, not cryptographic randomness — two widgets with
/// different ids should land on different phases, that's all.
fn jitter_offset_millis(key: &str, max_jitter: Duration) -> u64 {
    let span = max_jitter.as_millis() as u64;
    if span == 0 {
        return 0;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish() % span
}

/// Debounce tracker for periodic widget refreshes.
///
/// Holds a configured interval plus the monotonic timestamp of the
/// most recent attempt. Construct with [`PollTracker::new`], gate
/// refreshes with [`is_due`](Self::is_due), and call
/// [`mark_attempted`](Self::mark_attempted) whenever you spawn a
/// fetch (success-or-failure — the next attempt is `interval` after
/// *the attempt*, not after the response).
#[derive(Debug, Clone)]
pub struct PollTracker {
    interval: Duration,
    last_attempt: Option<Instant>,
}

/// Read-only view of a [`PollTracker`] for platform-side consumers.
///
/// Returned by [`Widget::poll_snapshot`](crate::widgets::Widget::poll_snapshot)
/// so the platform can inspect polling state without taking a
/// reference into a widget's internal `Mutex<State>`. Mutation stays
/// with the widget — the platform reads, the widget writes.
#[derive(Debug, Clone, Copy)]
pub struct PollSnapshot {
    /// Would the next call to `is_due()` return true?
    pub is_due: bool,
    /// Monotonic instant of the next scheduled refresh, or `None`
    /// when the widget is already overdue. Reserved for future
    /// event-loop wake-up scheduling.
    pub next_due_at: Option<Instant>,
    pub interval: Duration,
}

impl Default for PollTracker {
    /// 60-second placeholder interval. Widgets always overwrite via
    /// [`PollTracker::new`] at construction; this exists only so
    /// state structs that `derive(Default)` continue to compile.
    fn default() -> Self {
        Self::new(Duration::from_secs(60))
    }
}

impl PollTracker {
    /// Build a tracker with the given refresh interval. The first
    /// call to [`is_due`](Self::is_due) returns `true` so the widget
    /// fires its first fetch as soon as the event loop ticks.
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last_attempt: None,
        }
    }

    /// Seed `last_attempt` so a cache-hydrated entry doesn't trigger
    /// an immediate refresh on launch. Pass the cache entry's age
    /// (typically `entry.age()`); we clamp it to the interval so a
    /// long-stale cache lands as "due right now" rather than as a
    /// future time.
    pub fn seed_from_cache_age(&mut self, age: Duration) {
        let age = age.min(self.interval);
        self.last_attempt = Instant::now().checked_sub(age);
    }

    /// Defer the next `is_due` from "fire right now" to "fire `jitter`
    /// seconds from now", where `jitter` is a deterministic hash of
    /// `key` modulo `min(interval / 2, 30 s)`. No-op when the tracker
    /// isn't currently due (a recently-attempted or fresh-cache-seeded
    /// tracker already has a future deadline — we don't overwrite it).
    ///
    /// **Why this exists.** Widgets that all share a 60 s poll
    /// interval and construct around the same time fire their first
    /// network request together, which compounds CPU + allocator
    /// pressure into a coordinated "refresh storm" once per minute.
    /// Spreading the *first* fire across the configured jitter window
    /// preserves that spread on every subsequent cycle — the per-fire
    /// cadence is unchanged, but no two widgets share a phase.
    ///
    /// Pass a stable identifier (widget id, including its instance
    /// suffix) so the offset is deterministic across restarts — the
    /// same dashboard always lands on the same phase, which matters
    /// for users debugging cadence with `--trace`.
    pub fn apply_jitter(&mut self, key: &str) {
        if !self.is_due() {
            return;
        }
        let max_jitter = std::cmp::min(self.interval / 2, Duration::from_secs(30));
        if max_jitter.is_zero() {
            return;
        }
        let jitter = Duration::from_millis(jitter_offset_millis(key, max_jitter));
        // `interval - jitter` = how far in the past to claim the last
        // attempt was, so the next `is_due` fires `jitter` from now.
        let lookback = self.interval.saturating_sub(jitter);
        self.last_attempt = Instant::now().checked_sub(lookback);
    }

    /// `true` when no attempt has been recorded yet *or* the
    /// configured interval has elapsed since the last attempt.
    pub fn is_due(&self) -> bool {
        match self.last_attempt {
            None => true,
            Some(t) => t.elapsed() >= self.interval,
        }
    }

    /// Stamp the current time as the latest attempt. Call this when
    /// the widget kicks off a fetch — success or failure — so a
    /// failed request doesn't spin-retry every tick.
    pub fn mark_attempted(&mut self) {
        self.last_attempt = Some(Instant::now());
    }

    /// Force the next [`is_due`](Self::is_due) call to return `true`.
    /// Used by user-triggered refreshes (`r` key, `:reload` command)
    /// where the user explicitly wants the cadence overridden.
    pub fn mark_dirty(&mut self) {
        self.last_attempt = None;
    }

    /// `true` once any fetch attempt has been recorded since the
    /// tracker was constructed (or its last [`mark_dirty`](Self::mark_dirty)).
    /// Distinct from [`is_due`](Self::is_due): widgets use this to
    /// answer "have I ever tried to load data?", which renderers
    /// branch on (e.g., "show 'No items'" vs "show 'Loading…'").
    pub fn has_attempted(&self) -> bool {
        self.last_attempt.is_some()
    }

    /// Current refresh interval.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Update the refresh interval. Used by widgets that hot-reload
    /// their config via `apply_config`.
    pub fn set_interval(&mut self, interval: Duration) {
        self.interval = interval;
    }

    /// Monotonic instant the tracker would next return `true` from
    /// [`is_due`](Self::is_due). `None` when an attempt is already
    /// overdue (the widget should fetch now). Exposed for future
    /// platform-side scheduling — e.g., the event loop could wake
    /// at the earliest `next_due_at()` across all widgets rather
    /// than running a blanket 250 ms tick. Not consumed yet.
    pub fn next_due_at(&self) -> Option<Instant> {
        let last = self.last_attempt?;
        let next = last.checked_add(self.interval)?;
        if next <= Instant::now() {
            None
        } else {
            Some(next)
        }
    }

    /// Owned copy of the tracker's read-only state for platform-side
    /// observers (tracing, scheduler). See [`PollSnapshot`].
    pub fn snapshot(&self) -> PollSnapshot {
        PollSnapshot {
            is_due: self.is_due(),
            next_due_at: self.next_due_at(),
            interval: self.interval,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn fresh_tracker_is_due_immediately() {
        let p = PollTracker::new(Duration::from_secs(60));
        assert!(
            p.is_due(),
            "first call should fire so widgets refresh on boot"
        );
    }

    #[test]
    fn mark_attempted_resets_due_until_interval_elapses() {
        let mut p = PollTracker::new(Duration::from_millis(50));
        p.mark_attempted();
        assert!(!p.is_due());
        sleep(Duration::from_millis(60));
        assert!(p.is_due());
    }

    #[test]
    fn mark_dirty_forces_due_regardless_of_recent_attempt() {
        let mut p = PollTracker::new(Duration::from_secs(60));
        p.mark_attempted();
        assert!(!p.is_due());
        p.mark_dirty();
        assert!(p.is_due());
    }

    #[test]
    fn seed_from_cache_age_clamps_to_interval() {
        // Cache older than the interval → effectively zero — next
        // is_due returns true immediately. Without the clamp the
        // computed last_attempt could underflow into the future on
        // platforms where Instant arithmetic checks bounds.
        let mut p = PollTracker::new(Duration::from_secs(60));
        p.seed_from_cache_age(Duration::from_secs(3600));
        assert!(p.is_due(), "stale cache should not delay first fetch");
    }

    #[test]
    fn seed_from_recent_cache_delays_first_due() {
        let mut p = PollTracker::new(Duration::from_secs(60));
        // Cache is 1s old — well within the 60s interval, so we
        // shouldn't be due yet.
        p.seed_from_cache_age(Duration::from_secs(1));
        assert!(!p.is_due());
    }

    #[test]
    fn set_interval_takes_effect_for_next_check() {
        let mut p = PollTracker::new(Duration::from_secs(3600));
        p.mark_attempted();
        assert!(!p.is_due());
        p.set_interval(Duration::from_millis(1));
        sleep(Duration::from_millis(5));
        assert!(p.is_due(), "shorter interval should now be due");
    }

    #[test]
    fn next_due_at_is_none_when_overdue_or_unattempted() {
        let mut p = PollTracker::new(Duration::from_secs(60));
        assert!(p.next_due_at().is_none(), "no attempt yet ⇒ due now");
        p.set_interval(Duration::from_millis(1));
        p.mark_attempted();
        sleep(Duration::from_millis(5));
        assert!(p.next_due_at().is_none(), "elapsed past interval ⇒ due now");
    }

    #[test]
    fn next_due_at_returns_future_instant_while_in_window() {
        let mut p = PollTracker::new(Duration::from_secs(60));
        p.mark_attempted();
        let due = p.next_due_at().expect("should have a future deadline");
        assert!(due > Instant::now());
    }

    #[test]
    fn apply_jitter_defers_first_due_for_unattempted_tracker() {
        // A fresh tracker is due immediately; jitter should shift it
        // so is_due now returns false (because the first fire is
        // scheduled some seconds into the future).
        let mut p = PollTracker::new(Duration::from_secs(60));
        assert!(p.is_due(), "precondition: fresh tracker is due now");
        p.apply_jitter("widget-a@main");
        assert!(
            !p.is_due(),
            "jittered first fire should land in the future, not at t=0"
        );
    }

    #[test]
    fn apply_jitter_is_deterministic_for_same_key() {
        // Same key → same offset. Two trackers built back-to-back
        // can't compare `last_attempt` directly (each one captured its
        // own `Instant::now()` baseline) — but the offset *within* the
        // interval window is what determines phase, so verify those
        // are within a millisecond of each other.
        let mut p1 = PollTracker::new(Duration::from_secs(60));
        let mut p2 = PollTracker::new(Duration::from_secs(60));
        p1.apply_jitter("widget-a@main");
        p2.apply_jitter("widget-a@main");
        let elapsed_diff = p1
            .last_attempt
            .unwrap()
            .elapsed()
            .abs_diff(p2.last_attempt.unwrap().elapsed());
        assert!(
            elapsed_diff < Duration::from_millis(5),
            "same key should land on same phase (off by {elapsed_diff:?})"
        );
    }

    #[test]
    fn apply_jitter_spreads_different_keys() {
        // Different keys should (with very high probability) land on
        // different phases — otherwise the whole point is moot. We
        // assert *some* difference across a handful of widget ids;
        // a stable PRG over short strings makes a collision here
        // genuinely unexpected.
        let mut p1 = PollTracker::new(Duration::from_secs(60));
        let mut p2 = PollTracker::new(Duration::from_secs(60));
        let mut p3 = PollTracker::new(Duration::from_secs(60));
        p1.apply_jitter("news@main");
        p2.apply_jitter("wsj@main");
        p3.apply_jitter("email@main");
        let ts = [p1.last_attempt, p2.last_attempt, p3.last_attempt];
        let unique = ts.iter().collect::<std::collections::HashSet<_>>().len();
        assert!(
            unique > 1,
            "different widget ids should pick different phases"
        );
    }

    #[test]
    fn apply_jitter_does_not_override_recently_attempted_tracker() {
        // If the tracker was just attempted (or has a fresh cache),
        // applying jitter is a no-op — we shouldn't push a future
        // deadline backwards just because we got around to seeding.
        let mut p = PollTracker::new(Duration::from_secs(60));
        p.mark_attempted();
        let before = p.last_attempt;
        p.apply_jitter("widget@main");
        assert_eq!(p.last_attempt, before, "jitter must respect prior attempt");
    }
}
