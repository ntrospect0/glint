// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Size-tier helper for responsive widget rendering.
//!
//! `ViewTier::from_rect` classifies a widget's outer `Rect` into one of four
//! display tiers. Widgets call this at the top of their `render()` body and
//! branch on the result — the richer layout paths (split-panes, additional
//! data columns, full reading panes) activate when the tier warrants it.
//!
//! # Community SDK surface
//!
//! `ViewTier` is part of the public widget SDK. Once glint is open-sourced,
//! community widget authors will import this type and match on its variants.
//! **The variant names (`Compact`, `Standard`, `Expanded`, `Full`) and the
//! threshold constant names are a one-way door at OSS release** — renaming
//! them is a breaking change for downstream widgets. Do not publish
//! `ViewTier` as provisional.
//!
//! # When to use `from_rect` vs. per-widget constants
//!
//! Use `from_rect` for the common split-pane pattern shared by email, feeds,
//! news, stocks, forex, and resources — six widgets whose `Expanded` tier
//! fires at the same ~65-col threshold for horizontal list+detail splits.
//! This consistency is the point: a user watching a terminal resize sees all
//! six widgets' split layouts appear at the same width in the same frame.
//!
//! Use per-widget inline constants instead for widgets whose layout geometry
//! dictates its own thresholds (calendar's month grid, clock's large-digit
//! widths). Document any deviation in the widget's module doc-comment with
//! the prefix `// ViewTier deviation:` so a convention-sweep grep finds all
//! deviations in one pass.

use ratatui::layout::Rect;

// Width is the dominant axis: split-pane promotion is a horizontal decision.
// Height is only consulted to gate `Full` — a wide-but-shallow cell stays
// `Expanded` rather than upgrading to a full dashboard layout.

/// How many content rows are available inside a bordered widget.
///
/// Subtracts 2 for a 1-cell border on each vertical edge. Returns 0 if the
/// area is too small to contain a border. This is a named, grep-able
/// convention for the `area.height - 2` idiom — the value is trivial;
/// the naming is the point.
///
/// Available to widget authors; the built-ins ended up computing inner
/// dimensions via [`crate::ui::grid::CardGrid`] or inline, so this stays
/// provided-but-unused (hence `#[allow(dead_code)]`).
#[allow(dead_code)]
pub(crate) fn inner_rows(area: Rect) -> u16 {
    area.height.saturating_sub(2)
}

/// How many content columns are available inside a bordered widget.
///
/// Subtracts 2 for a 1-cell border on each horizontal edge. Returns 0 if
/// the area is too small to contain a border. Pair with `inner_rows` when
/// computing cell counts for grid layouts.
///
/// Available to widget authors; see [`inner_rows`] on why it's currently
/// provided-but-unused by the built-ins.
#[allow(dead_code)]
pub(crate) fn inner_cols(area: Rect) -> u16 {
    area.width.saturating_sub(2)
}

/// Split `total` rows between two sections A and B.
///
/// A gets `min(total, budget_a)` rows; B gets the remainder. Both values
/// are non-negative. Use this for the "up-to-N-rows for one section,
/// remainder for another" pattern (e.g., sparkline budget + process list).
///
/// ```text
/// row_split(22, 10) == (10, 12)  // sparkline capped, list gets rest
/// row_split(5, 10)  == (5, 0)    // budget exceeds total; B gets nothing
/// row_split(10, 10) == (10, 0)   // at-budget; B gets nothing
/// row_split(0, 10)  == (0, 0)    // nothing to split
/// ```
pub(crate) fn row_split(total: u16, budget_a: u16) -> (u16, u16) {
    let a = total.min(budget_a);
    (a, total - a)
}

/// Upper bound for the `Compact` tier (inclusive on the `Compact` side).
/// Any rect narrower than `COMPACT_MAX_W + 1` columns resolves to `Compact`.
pub const COMPACT_MAX_W: u16 = 31;

/// Minimum width for the `Expanded` tier. Rects between `COMPACT_MAX_W + 1`
/// and `EXPANDED_MIN_W - 1` columns resolve to `Standard`.
pub const EXPANDED_MIN_W: u16 = 65;

/// Minimum width for the `Full` tier. The height axis must also satisfy
/// `FULL_MIN_H` — a wide-but-shallow cell stays `Expanded`.
pub const FULL_MIN_W: u16 = 105;

/// Minimum height for the `Full` tier. A cell that is wide enough but
/// shallower than this resolves to `Expanded`, not `Full`.
pub const FULL_MIN_H: u16 = 30;

/// Four display tiers used by responsive widgets to pick their render path.
///
/// Ordered: `Compact < Standard < Expanded < Full`. The derived `PartialOrd` /
/// `Ord` let callers write `tier >= ViewTier::Expanded` without explicit
/// pattern matching.
///
/// | Tier | Rough footprint | What fits |
/// |---|---|---|
/// | `Compact` | ~20 × ~10 | One primary signal; maximum compression. |
/// | `Standard` | ~40 × ~18 | Fully functional: chart, scrollable list, multi-day calendar. This is most widgets today. |
/// | `Expanded` | ~80 × ~24 | Split-pane layouts viable; additional data columns. |
/// | `Full` | ~120 × ~36 | Dashboard-filling: reading panes, multi-column grids, all context visible without scrolling. |
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ViewTier {
    /// ~20 cols × ~10 rows. One primary signal; maximum compression. This is
    /// most widgets when placed in a tight grid cell.
    Compact,
    /// ~40 cols × ~18 rows. The current comfortable rendering: chart,
    /// scrollable list, multi-day view. The widget is fully functional at
    /// this tier.
    Standard,
    /// ~80 cols × ~24 rows. Split-pane layouts become viable: list on the
    /// left, detail on the right (or top/bottom). Additional data columns
    /// that don't fit at `Standard`. Achievable in a standard-width terminal
    /// when one widget is given a large share of the layout.
    Expanded,
    /// ~120 cols × ~36 rows. Dashboard-filling views: full reading panes,
    /// multi-column data grids, all context visible without scrolling. The
    /// typical Focus Zoom target and a large dedicated layout cell on a wide
    /// display.
    Full,
}

impl ViewTier {
    /// Classify `area` (the outer `Rect` that `Widget::render` receives,
    /// borders included) into the appropriate display tier.
    ///
    /// Width is the primary axis. Height is only consulted to gate `Full`:
    /// a cell that is wide enough but too shallow (< `FULL_MIN_H` rows) stays
    /// `Expanded`. A height below 8 rows forces `Compact` regardless of width
    /// — there is not enough room for a usable split layout.
    ///
    /// For `area` rects that come from the Focus Zoom overlay, the tier
    /// reflects the zoom frame's size. The widget receives this rect exactly
    /// as it would receive any large layout-cell rect — it has no signal that
    /// zoom is involved and should not try to detect it.
    pub fn from_rect(area: Rect) -> Self {
        if area.height < 8 {
            return Self::Compact;
        }
        if area.width >= FULL_MIN_W && area.height >= FULL_MIN_H {
            Self::Full
        } else if area.width >= EXPANDED_MIN_W {
            Self::Expanded
        } else if area.width > COMPACT_MAX_W {
            Self::Standard
        } else {
            Self::Compact
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(w: u16, h: u16) -> Rect {
        Rect::new(0, 0, w, h)
    }

    // --- Compact ---

    #[test]
    fn zero_rect_is_compact() {
        assert_eq!(ViewTier::from_rect(r(0, 0)), ViewTier::Compact);
    }

    #[test]
    fn tiny_rect_is_compact() {
        assert_eq!(ViewTier::from_rect(r(10, 5)), ViewTier::Compact);
    }

    #[test]
    fn very_short_but_wide_rect_is_compact() {
        // Height < 8 forces Compact regardless of width.
        assert_eq!(ViewTier::from_rect(r(200, 7)), ViewTier::Compact);
        assert_eq!(ViewTier::from_rect(r(200, 0)), ViewTier::Compact);
    }

    #[test]
    fn compact_max_width_boundary() {
        // Exactly COMPACT_MAX_W cols with sufficient height → Compact.
        assert_eq!(ViewTier::from_rect(r(COMPACT_MAX_W, 10)), ViewTier::Compact);
        // One column wider → Standard.
        assert_eq!(
            ViewTier::from_rect(r(COMPACT_MAX_W + 1, 10)),
            ViewTier::Standard
        );
    }

    // --- Standard ---

    #[test]
    fn mid_standard_rect() {
        // 50 × 20 is well inside Standard (32–64 cols).
        assert_eq!(ViewTier::from_rect(r(50, 20)), ViewTier::Standard);
    }

    // --- Standard → Expanded boundary ---

    #[test]
    fn one_below_expanded_is_standard() {
        assert_eq!(
            ViewTier::from_rect(r(EXPANDED_MIN_W - 1, 20)),
            ViewTier::Standard
        );
    }

    #[test]
    fn exactly_expanded_min_width_is_expanded() {
        assert_eq!(
            ViewTier::from_rect(r(EXPANDED_MIN_W, 20)),
            ViewTier::Expanded
        );
    }

    // --- Expanded → Full boundary ---

    #[test]
    fn full_requires_both_width_and_height() {
        // Wide enough but one row too short → Expanded.
        assert_eq!(
            ViewTier::from_rect(r(FULL_MIN_W, FULL_MIN_H - 1)),
            ViewTier::Expanded
        );
        // One col too narrow but tall enough → Expanded.
        assert_eq!(
            ViewTier::from_rect(r(FULL_MIN_W - 1, FULL_MIN_H)),
            ViewTier::Expanded
        );
        // Both thresholds met → Full.
        assert_eq!(
            ViewTier::from_rect(r(FULL_MIN_W, FULL_MIN_H)),
            ViewTier::Full
        );
    }

    // --- Full ---

    #[test]
    fn very_large_rect_is_full() {
        assert_eq!(ViewTier::from_rect(r(200, 60)), ViewTier::Full);
    }

    // --- Ordering ---

    #[test]
    fn ord_allows_tier_comparison() {
        assert!(ViewTier::Compact < ViewTier::Standard);
        assert!(ViewTier::Standard < ViewTier::Expanded);
        assert!(ViewTier::Expanded < ViewTier::Full);
        assert!(ViewTier::Expanded >= ViewTier::Expanded);
        assert!(ViewTier::Full > ViewTier::Compact);
    }

    // --- inner_rows / inner_cols ---

    #[test]
    fn inner_rows_subtracts_border() {
        assert_eq!(inner_rows(r(80, 30)), 28);
        assert_eq!(inner_rows(r(80, 2)), 0); // border exactly uses all rows
        assert_eq!(inner_rows(r(80, 1)), 0); // saturating_sub
        assert_eq!(inner_rows(r(80, 0)), 0);
    }

    #[test]
    fn inner_cols_subtracts_border() {
        assert_eq!(inner_cols(r(80, 30)), 78);
        assert_eq!(inner_cols(r(2, 30)), 0);
        assert_eq!(inner_cols(r(1, 30)), 0);
        assert_eq!(inner_cols(r(0, 30)), 0);
    }

    // --- row_split ---

    #[test]
    fn row_split_budget_below_total() {
        // A takes its full budget; B gets the remainder.
        assert_eq!(row_split(22, 10), (10, 12));
    }

    #[test]
    fn row_split_budget_equals_total() {
        // A takes everything; B gets nothing.
        assert_eq!(row_split(10, 10), (10, 0));
    }

    #[test]
    fn row_split_budget_above_total() {
        // A is capped at total; B gets nothing.
        assert_eq!(row_split(5, 10), (5, 0));
    }

    #[test]
    fn row_split_total_zero() {
        // Nothing to split; both get 0.
        assert_eq!(row_split(0, 10), (0, 0));
        assert_eq!(row_split(0, 0), (0, 0));
    }
}
