// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Shared card-grid layout helper.
//!
//! [`CardGrid`] computes a grid of capped-width cards inside a given [`Rect`].
//! Cards are placed at a fixed stride (`card_w + gap`); the visible group for
//! each row is horizontally centered within the area. Supports two scrolling
//! shapes:
//!
//! - **Pinned home** (`pin_home = true`): item 0 is always the leftmost card
//!   regardless of `scroll_offset`; non-home items scroll behind it. Used by
//!   the weather widget.
//! - **Unpinned scroll** (`pin_home = false`): all items scroll together; the
//!   window advances by `scroll_offset`. Used by the clock world-time grid.
//!
//! # Zero-dependency guarantee
//!
//! This helper touches only [`ratatui::layout::Rect`]. It never takes a
//! [`ratatui::Frame`] or any widget data — the geometry is the only output,
//! keeping the module unit-testable without a terminal backend.

use ratatui::layout::Rect;

/// Layout parameters for a horizontally-centered card grid.
///
/// Fill the fields and call [`CardGrid::layout`] to get the positioned
/// card rectangles for the current frame.
pub struct CardGrid {
    /// Area available to the grid (typically the inner area of a bordered
    /// widget — pass `block.inner(area)` or equivalent).
    pub area: Rect,

    /// Maximum card width. Cards are never wider than this, but may be
    /// narrower when `area.width < card_max_w`. Pass [`u16::MAX`] to let
    /// the available width dictate the width entirely.
    pub card_max_w: u16,

    /// Minimum card width. When `area.width` is smaller than this value the
    /// card is clamped to `area.width` anyway (the grid always renders at
    /// least one card). Pass `0` for no minimum.
    pub card_min_w: u16,

    /// Height (rows) of a single card row. When a single full-height strip
    /// is needed (weather), pass `area.height`. For multi-row grids (clock),
    /// pass the fixed per-card height.
    pub cell_h: u16,

    /// Gap (columns) between adjacent card borders. Typically `1`.
    pub gap: u16,

    /// Total number of items to place. `0` returns an empty layout.
    pub item_count: usize,

    /// Current scroll offset. Semantics differ by [`pin_home`]:
    /// - `pin_home = true`: number of steps the non-home window has
    ///   advanced past item 1. Item 0 is always visible.
    /// - `pin_home = false`: index of the first visible item.
    ///
    /// Clamped internally to `max_scroll`; callers do not need to pre-clamp.
    ///
    /// [`pin_home`]: CardGrid::pin_home
    pub scroll_offset: usize,

    /// When `true`, item 0 ("home") is always the leftmost card in the
    /// first row regardless of `scroll_offset`. Non-home items (1 .. n)
    /// scroll behind it.
    ///
    /// When `false`, all items scroll together: the visible window starts
    /// at `scroll_offset` and advances one item per step.
    pub pin_home: bool,
}

/// Output of [`CardGrid::layout`].
pub struct CardGridLayout {
    /// `(item_index, card_rect)` for every card that fits in the area,
    /// in slot order (left-to-right, top-to-bottom).
    ///
    /// The rects already incorporate the centered outer margin for each row.
    /// Callers iterate this slice and render each card at the given rect.
    pub cells: Vec<(usize, Rect)>,

    /// Maximum valid `scroll_offset` for the current `item_count` and area.
    ///
    /// `0` means all items fit without scrolling; widgets should return
    /// `EventResult::Ignored` for scroll keys when `max_scroll == 0`.
    pub max_scroll: usize,
}

impl CardGrid {
    /// Compute the card grid layout for the current frame.
    pub fn layout(&self) -> CardGridLayout {
        if self.item_count == 0 || self.area.width == 0 || self.area.height == 0 {
            return CardGridLayout { cells: Vec::new(), max_scroll: 0 };
        }

        // Resolved card width: cap at max, enforce min (but never wider than
        // the available area so at least one card can always render).
        let card_w = self
            .card_max_w
            .min(self.area.width)
            .max(self.card_min_w.min(self.area.width));

        let stride = card_w + self.gap;

        // Cards per row: N cards occupy N*card_w + (N-1)*gap columns.
        // Equivalently N ≤ (width + gap) / stride.
        let cols = ((self.area.width + self.gap) / stride).max(1) as usize;

        // Row bands that fit vertically.
        let cell_h = self.cell_h.max(1);
        let rows = (self.area.height / cell_h).max(1) as usize;

        // Total slot capacity.
        let capacity = cols * rows;

        // max_scroll and clamped offset depend on pin_home.
        let (max_scroll, offset) = if self.pin_home {
            // Home occupies one slot; non-home capacity = capacity - 1.
            let non_home_capacity = capacity.saturating_sub(1);
            let non_home_count = self.item_count.saturating_sub(1);
            let ms = non_home_count.saturating_sub(non_home_capacity);
            (ms, self.scroll_offset.min(ms))
        } else {
            let ms = self.item_count.saturating_sub(capacity);
            (ms, self.scroll_offset.min(ms))
        };

        // Determine how many items are actually visible.
        let visible_count = if self.pin_home {
            // 1 home + however many non-home fit, capped at item_count.
            let non_home_visible = capacity.saturating_sub(1).min(self.item_count.saturating_sub(1));
            (1 + non_home_visible).min(self.item_count)
        } else {
            capacity.min(self.item_count.saturating_sub(offset))
        };

        let mut cells: Vec<(usize, Rect)> = Vec::with_capacity(visible_count);

        for slot in 0..visible_count {
            let item_idx = if self.pin_home {
                if slot == 0 {
                    0
                } else {
                    offset + slot
                }
            } else {
                offset + slot
            };

            if item_idx >= self.item_count {
                break;
            }

            let row = slot / cols;
            let col_in_row = slot % cols;

            // Count how many cards are in this row to compute centering.
            let row_start = row * cols;
            let row_card_count = visible_count.saturating_sub(row_start).min(cols);

            let group_w = (row_card_count as u16) * card_w
                + (row_card_count.saturating_sub(1) as u16) * self.gap;
            let outer_margin = self.area.width.saturating_sub(group_w) / 2;

            let x = self.area.x + outer_margin + (col_in_row as u16) * stride;
            let y = self.area.y + (row as u16) * cell_h;

            let w = card_w.min(self.area.right().saturating_sub(x));
            let h = cell_h.min(self.area.bottom().saturating_sub(y));
            if w == 0 || h == 0 {
                break;
            }

            cells.push((item_idx, Rect::new(x, y, w, h)));
        }

        CardGridLayout { cells, max_scroll }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(w: u16, h: u16) -> Rect {
        Rect::new(0, 0, w, h)
    }

    // -----------------------------------------------------------------------
    // Pinned-home strip (weather shape): single row, home always slot 0.
    // -----------------------------------------------------------------------

    /// All items fit → max_scroll 0, home is always index 0.
    #[test]
    fn pinned_all_fit_max_scroll_zero() {
        // 150 wide, 10 high, card_max=48, gap=1, 3 cities.
        // stride=49, cols=(150+1)/49=3, capacity=3, all fit.
        let g = CardGrid {
            area: area(150, 10),
            card_max_w: 48,
            card_min_w: 0,
            cell_h: 10,
            gap: 1,
            item_count: 3,
            scroll_offset: 0,
            pin_home: true,
        };
        let l = g.layout();
        assert_eq!(l.max_scroll, 0, "all items fit → no scrolling");
        assert_eq!(l.cells.len(), 3);
        assert_eq!(l.cells[0].0, 0, "first cell must be home (index 0)");
        assert_eq!(l.cells[1].0, 1);
        assert_eq!(l.cells[2].0, 2);
    }

    /// Home is always index 0 even with a non-zero offset.
    #[test]
    fn pinned_home_is_always_index_zero() {
        let g = CardGrid {
            area: area(200, 10),
            card_max_w: 48,
            card_min_w: 0,
            cell_h: 10,
            gap: 1,
            item_count: 10,
            scroll_offset: 3,
            pin_home: true,
        };
        let l = g.layout();
        assert_eq!(l.cells[0].0, 0, "home must be index 0 regardless of offset");
    }

    /// Overflow: non-home window advances; home stays at slot 0.
    #[test]
    fn pinned_overflow_home_pinned_window_advances() {
        // 200 wide, 10 high, card_max=48, gap=1 → stride=49,
        // cols=(200+1)/49=4, capacity=4.
        // 10 items: non-home capacity=3, non-home count=9, max_scroll=6.
        let g = CardGrid {
            area: area(200, 10),
            card_max_w: 48,
            card_min_w: 0,
            cell_h: 10,
            gap: 1,
            item_count: 10,
            scroll_offset: 2,
            pin_home: true,
        };
        let l = g.layout();
        assert_eq!(l.max_scroll, 6);
        assert_eq!(l.cells.len(), 4);
        assert_eq!(l.cells[0].0, 0);       // home pinned
        assert_eq!(l.cells[1].0, 3);       // offset(2) + 1
        assert_eq!(l.cells[2].0, 4);       // offset(2) + 2
        assert_eq!(l.cells[3].0, 5);       // offset(2) + 3
    }

    /// Scroll offset is clamped to max_scroll.
    #[test]
    fn pinned_offset_clamped_to_max_scroll() {
        // 200 wide, card_max=48, gap=1 → cols=4, capacity=4.
        // 5 items: non-home count=4, non-home capacity=3, max_scroll=1.
        let g = CardGrid {
            area: area(200, 10),
            card_max_w: 48,
            card_min_w: 0,
            cell_h: 10,
            gap: 1,
            item_count: 5,
            scroll_offset: 99,
            pin_home: true,
        };
        let l = g.layout();
        assert_eq!(l.max_scroll, 1);
        assert_eq!(l.cells[0].0, 0);
        assert_eq!(l.cells[1].0, 2); // clamped offset=1 → 1+1
        assert_eq!(l.cells[2].0, 3);
        assert_eq!(l.cells[3].0, 4);
    }

    /// Cards are horizontally centered: outer margins absorb leftover width.
    #[test]
    fn pinned_group_is_centered() {
        // 200 wide, card_max=48, gap=1, 4 items.
        // cols=4 (stride=49, (200+1)/49=4), all fit (max_scroll=0).
        // group_w = 4*48 + 3*1 = 195, outer_margin = (200-195)/2 = 2.
        let g = CardGrid {
            area: area(200, 10),
            card_max_w: 48,
            card_min_w: 0,
            cell_h: 10,
            gap: 1,
            item_count: 4,
            scroll_offset: 0,
            pin_home: true,
        };
        let l = g.layout();
        assert_eq!(l.cells[0].1.x, 2, "first card should start at outer_margin=2");
        assert_eq!(l.cells[1].1.x, 2 + 49, "second card at margin + stride");
        assert_eq!(l.cells[2].1.x, 2 + 98);
        assert_eq!(l.cells[3].1.x, 2 + 147);
    }

    /// Card width is capped at card_max_w; never fills the whole area when cap < area.
    #[test]
    fn pinned_card_width_is_capped() {
        let g = CardGrid {
            area: area(200, 10),
            card_max_w: 48,
            card_min_w: 0,
            cell_h: 10,
            gap: 1,
            item_count: 3,
            scroll_offset: 0,
            pin_home: true,
        };
        let l = g.layout();
        for (_, rect) in &l.cells {
            assert_eq!(rect.width, 48, "card width must be capped at card_max_w");
        }
    }

    // -----------------------------------------------------------------------
    // Unpinned multi-row grid (clock shape): all items scroll together,
    // group centered per row, partial last row centered to its own count.
    // -----------------------------------------------------------------------

    /// All items fit in multiple rows → max_scroll 0.
    #[test]
    fn unpinned_multi_row_all_fit() {
        // 200 wide, 30 high, card_max=40, card_min=29, cell_h=10, gap=1.
        // stride=41, cols=(200+1)/41=4, rows=30/10=3, capacity=12.
        // 8 items: max_scroll = 8.saturating_sub(12) = 0.
        let g = CardGrid {
            area: area(200, 30),
            card_max_w: 40,
            card_min_w: 29,
            cell_h: 10,
            gap: 1,
            item_count: 8,
            scroll_offset: 0,
            pin_home: false,
        };
        let l = g.layout();
        assert_eq!(l.max_scroll, 0);
        assert_eq!(l.cells.len(), 8);
        assert_eq!(l.cells[0].0, 0);
        assert_eq!(l.cells[7].0, 7);
    }

    /// Scroll advances the visible window for unpinned grids.
    #[test]
    fn unpinned_scroll_advances_window() {
        // capacity=12, 15 items → max_scroll=3.
        let g = CardGrid {
            area: area(200, 30),
            card_max_w: 40,
            card_min_w: 29,
            cell_h: 10,
            gap: 1,
            item_count: 15,
            scroll_offset: 2,
            pin_home: false,
        };
        let l = g.layout();
        assert_eq!(l.max_scroll, 3);
        assert_eq!(l.cells[0].0, 2, "first slot should show item at offset");
        assert_eq!(l.cells[1].0, 3);
        assert_eq!(l.cells[11].0, 13);
    }

    /// Per-row centering: partial last row uses its own card count.
    #[test]
    fn unpinned_partial_last_row_centered() {
        // 200 wide, 20 high, card_max=40, gap=1 → cols=4, rows=2, capacity=8.
        // 6 items: row 0 has 4 cards, row 1 has 2 cards.
        // Row 1 centering: group_w = 2*40 + 1 = 81, outer_margin = (200-81)/2 = 59.
        let g = CardGrid {
            area: area(200, 20),
            card_max_w: 40,
            card_min_w: 0,
            cell_h: 10,
            gap: 1,
            item_count: 6,
            scroll_offset: 0,
            pin_home: false,
        };
        let l = g.layout();
        assert_eq!(l.cells.len(), 6);
        // Row 0: 4 cards centered for a full row.
        // group_w = 4*40 + 3 = 163, margin = (200-163)/2 = 18.
        assert_eq!(l.cells[0].1.x, 18, "row 0 first card at outer_margin=18");
        assert_eq!(l.cells[0].1.y, 0);
        // Row 1: 2 cards, narrower group → larger margin.
        assert_eq!(l.cells[4].1.y, 10, "row 1 should start at y=cell_h");
        assert_eq!(l.cells[4].1.x, 59, "row 1 first card centered for 2-card group");
        assert_eq!(l.cells[5].1.x, 59 + 41, "row 1 second card at margin + stride");
    }

    /// Row-y coordinates increment by cell_h.
    #[test]
    fn unpinned_row_y_increments_by_cell_h() {
        let g = CardGrid {
            area: area(200, 30),
            card_max_w: 40,
            card_min_w: 0,
            cell_h: 10,
            gap: 1,
            item_count: 12,
            scroll_offset: 0,
            pin_home: false,
        };
        let l = g.layout();
        let (_, r0) = l.cells[0];
        let (_, r4) = l.cells[4];
        let (_, r8) = l.cells[8];
        assert_eq!(r0.y, 0);
        assert_eq!(r4.y, 10);
        assert_eq!(r8.y, 20);
    }

    // -----------------------------------------------------------------------
    // Edge cases: item_count 0, 1, zero area.
    // -----------------------------------------------------------------------

    #[test]
    fn item_count_zero_produces_empty_layout() {
        let g = CardGrid {
            area: area(200, 20),
            card_max_w: 48,
            card_min_w: 0,
            cell_h: 10,
            gap: 1,
            item_count: 0,
            scroll_offset: 0,
            pin_home: true,
        };
        let l = g.layout();
        assert!(l.cells.is_empty());
        assert_eq!(l.max_scroll, 0);
    }

    #[test]
    fn item_count_one_shows_only_home() {
        let g = CardGrid {
            area: area(200, 20),
            card_max_w: 48,
            card_min_w: 0,
            cell_h: 10,
            gap: 1,
            item_count: 1,
            scroll_offset: 0,
            pin_home: true,
        };
        let l = g.layout();
        assert_eq!(l.cells.len(), 1);
        assert_eq!(l.cells[0].0, 0);
        assert_eq!(l.max_scroll, 0);
    }

    #[test]
    fn zero_area_produces_empty_layout() {
        let g = CardGrid {
            area: area(0, 0),
            card_max_w: 48,
            card_min_w: 0,
            cell_h: 10,
            gap: 1,
            item_count: 5,
            scroll_offset: 0,
            pin_home: true,
        };
        let l = g.layout();
        assert!(l.cells.is_empty());
        assert_eq!(l.max_scroll, 0);
    }

    // -----------------------------------------------------------------------
    // Cell rects are within area bounds.
    // -----------------------------------------------------------------------

    #[test]
    fn cell_rects_within_area_bounds() {
        let a = area(200, 30);
        let g = CardGrid {
            area: a,
            card_max_w: 40,
            card_min_w: 29,
            cell_h: 10,
            gap: 1,
            item_count: 9,
            scroll_offset: 0,
            pin_home: false,
        };
        let l = g.layout();
        for (_, rect) in &l.cells {
            assert!(rect.x + rect.width <= a.right(), "cell overflows right");
            assert!(rect.y + rect.height <= a.bottom(), "cell overflows bottom");
        }
    }

    // -----------------------------------------------------------------------
    // Weather-exact reproduction: verify the slot index mapping matches
    // weather's render_full_grid arithmetic for the pinned-home case.
    // -----------------------------------------------------------------------

    /// Reproduce weather's `full_grid_fit` result: same n_visible and max_scroll.
    #[test]
    fn weather_exact_n_visible_and_max_scroll() {
        // inner_width=200, num_cities=5 → n_visible=4, max_scroll=1.
        // card_max=48, gap=1: stride=49, cols=(200+1)/49=4, max_scroll=(5-1)-3=1.
        let g = CardGrid {
            area: Rect::new(0, 0, 200, 20),
            card_max_w: 48,
            card_min_w: 0,
            cell_h: 20,
            gap: 1,
            item_count: 5,
            scroll_offset: 0,
            pin_home: true,
        };
        let l = g.layout();
        assert_eq!(l.cells.len(), 4, "n_visible should be 4");
        assert_eq!(l.max_scroll, 1, "max_scroll should be 1");
    }

    /// Reproduce weather's outer_margin centering for a 4-card group in 200 cols.
    #[test]
    fn weather_exact_outer_margin() {
        // group_w = 4*48 + 3*1 = 195, outer_margin = (200-195)/2 = 2.
        let g = CardGrid {
            area: Rect::new(0, 0, 200, 20),
            card_max_w: 48,
            card_min_w: 0,
            cell_h: 20,
            gap: 1,
            item_count: 4,
            scroll_offset: 0,
            pin_home: true,
        };
        let l = g.layout();
        assert_eq!(l.cells[0].1.x, 2, "home card x should be outer_margin=2");
        assert_eq!(l.cells[0].1.width, 48);
    }

    /// With scroll_offset=1, home stays at 0, non-home window shifts.
    #[test]
    fn weather_scroll_offset_shifts_non_home_window() {
        let g = CardGrid {
            area: Rect::new(0, 0, 200, 20),
            card_max_w: 48,
            card_min_w: 0,
            cell_h: 20,
            gap: 1,
            item_count: 5,
            scroll_offset: 1,
            pin_home: true,
        };
        let l = g.layout();
        assert_eq!(l.cells[0].0, 0, "home always item 0");
        assert_eq!(l.cells[1].0, 2, "slot 1 → offset(1)+1=2");
        assert_eq!(l.cells[2].0, 3);
        assert_eq!(l.cells[3].0, 4);
    }

    // -----------------------------------------------------------------------
    // Clock-exact reproduction: verify unpinned per-row centering matches
    // clock's render_full_world_clock_grid arithmetic.
    // -----------------------------------------------------------------------

    /// Clock's full-capacity scenario: max_scroll=0 when all items fit.
    #[test]
    fn clock_exact_max_scroll_zero_when_all_fit() {
        // area=200×30, card_max=40, card_min=29, cell_h=10, gap=1.
        // stride=41, cols=4, rows=3, capacity=12. 8 items → max_scroll=0.
        let g = CardGrid {
            area: Rect::new(0, 0, 200, 30),
            card_max_w: 40,
            card_min_w: 29,
            cell_h: 10,
            gap: 1,
            item_count: 8,
            scroll_offset: 0,
            pin_home: false,
        };
        let l = g.layout();
        assert_eq!(l.max_scroll, 0);
    }

    /// Clock row centering: full row has margin=(200-163)/2=18, partial row centers differently.
    #[test]
    fn clock_exact_row_centering() {
        // area=200×20, card_max=40, gap=1 → cols=4, rows=2, capacity=8.
        // 5 items: row 0 has 4, row 1 has 1.
        // Row 0: group_w=4*40+3=163, margin=(200-163)/2=18.
        // Row 1: group_w=1*40+0=40, margin=(200-40)/2=80.
        let g = CardGrid {
            area: Rect::new(0, 0, 200, 20),
            card_max_w: 40,
            card_min_w: 0,
            cell_h: 10,
            gap: 1,
            item_count: 5,
            scroll_offset: 0,
            pin_home: false,
        };
        let l = g.layout();
        assert_eq!(l.cells[0].1.x, 18, "row 0 first card at margin=18");
        assert_eq!(l.cells[4].1.x, 80, "row 1 single card centered at margin=80");
        assert_eq!(l.cells[4].1.y, 10, "row 1 at y=cell_h");
    }
}
