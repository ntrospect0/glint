// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0
//
// Shared transient-status primitive for widgets.

//! Transient status feedback with a TTL.
//!
//! Widgets that surface short-lived "Added AAPL to watchlist" /
//! "Save failed" / "Copied to clipboard" messages in their footer
//! all need the same shape: a value, the instant it was set, and a
//! TTL after which it should vanish. Before this module the pattern
//! was open-coded as `Option<(String, Instant)>` in `feeds`, `forex`,
//! and `stocks`, with each widget hand-rolling the elapsed-check.
//!
//! [`TimedFeedback`] is generic over the value type. The most common
//! shape is `Option<TimedFeedback<String>>` for a status message,
//! but `Option<TimedFeedback<usize>>` works equally well for things
//! like the forex widget's "row index that was copied to clipboard"
//! pulse marker.
//!
//! See `docs/widget-sdk.md` § Transient status (TimedFeedback).

#![allow(dead_code)] // some accessors are SDK surface for future widgets.

use std::time::{Duration, Instant};

/// A value with a TTL — surfaced to the user for `ttl` time after
/// it's set, then automatically considered expired.
#[derive(Debug, Clone)]
pub struct TimedFeedback<T> {
    value: T,
    set_at: Instant,
    ttl: Duration,
}

impl<T> TimedFeedback<T> {
    /// Construct a new feedback entry that stays live for `ttl`
    /// past now. Most call sites pass a widget-defined constant
    /// like `STATUS_TTL = Duration::from_millis(2500)`.
    pub fn new(value: T, ttl: Duration) -> Self {
        Self {
            value,
            set_at: Instant::now(),
            ttl,
        }
    }

    /// `true` once `ttl` has elapsed since construction.
    pub fn is_expired(&self) -> bool {
        self.set_at.elapsed() >= self.ttl
    }

    /// Borrow the inner value without checking expiry — useful when
    /// the caller has already decided to show it (e.g. inside a
    /// branch guarded by `live_value`). For "show if not expired"
    /// reads, prefer [`live_value`].
    pub fn value(&self) -> &T {
        &self.value
    }
}

/// Read an `Option<TimedFeedback<T>>` slot, auto-clearing it when
/// the entry has expired. Returns `Some(&T)` while the feedback is
/// still live, `None` otherwise. The slot is cleared as a side
/// effect on expiry so the next render's "is anything live?" check
/// is a simple `is_some`.
///
/// Use this from `render` (or any place that wants the *current*
/// value): it's the canonical "drain expired and return what's
/// left" read.
pub fn live_value<T>(slot: &mut Option<TimedFeedback<T>>) -> Option<&T> {
    let expired = slot.as_ref().is_some_and(|f| f.is_expired());
    if expired {
        *slot = None;
    }
    slot.as_ref().map(|f| f.value())
}

/// Drain the slot iff the entry has expired. Returns `true` when
/// a drain actually happened (i.e. the display state changed and
/// the widget should redraw). Useful in tick paths where the
/// widget wants to mark its dirty flag exactly when the chrome
/// needs to revert.
pub fn drain_if_expired<T>(slot: &mut Option<TimedFeedback<T>>) -> bool {
    if slot.as_ref().is_some_and(|f| f.is_expired()) {
        *slot = None;
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn fresh_feedback_is_not_expired() {
        let f = TimedFeedback::new("hello", Duration::from_secs(60));
        assert!(!f.is_expired());
        assert_eq!(*f.value(), "hello");
    }

    #[test]
    fn expires_after_ttl_elapses() {
        let f = TimedFeedback::new(42usize, Duration::from_millis(20));
        assert!(!f.is_expired());
        sleep(Duration::from_millis(30));
        assert!(f.is_expired());
    }

    #[test]
    fn live_value_returns_some_then_clears_on_expiry() {
        let mut slot = Some(TimedFeedback::new("alive", Duration::from_millis(10)));
        assert_eq!(live_value(&mut slot).copied(), Some("alive"));
        sleep(Duration::from_millis(20));
        assert!(live_value(&mut slot).is_none());
        assert!(slot.is_none(), "expired slot should be drained");
    }

    #[test]
    fn live_value_returns_none_for_empty_slot() {
        let mut slot: Option<TimedFeedback<String>> = None;
        assert!(live_value(&mut slot).is_none());
    }

    #[test]
    fn drain_if_expired_reports_true_only_when_it_actually_drained() {
        let mut slot = Some(TimedFeedback::new(7u32, Duration::from_millis(10)));
        assert!(!drain_if_expired(&mut slot), "fresh entry shouldn't drain");
        assert!(slot.is_some());
        sleep(Duration::from_millis(20));
        assert!(drain_if_expired(&mut slot));
        assert!(slot.is_none());
        assert!(!drain_if_expired(&mut slot), "empty slot is a no-op");
    }

    #[test]
    fn generic_over_value_type() {
        // Same shape works for usize (forex's row-pulse marker),
        // (usize, Instant) tuples, custom enums, etc.
        let mut slot: Option<TimedFeedback<(usize, char)>> =
            Some(TimedFeedback::new((3, '✓'), Duration::from_secs(1)));
        let v = live_value(&mut slot);
        assert_eq!(v, Some(&(3, '✓')));
    }
}
