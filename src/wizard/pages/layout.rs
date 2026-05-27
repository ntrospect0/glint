// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Layout page. Two-phase: first asks for the number of panes the user
//! wants, then surfaces the preset layouts that match that count. The
//! preset list is curated rather than fully customisable — power users
//! who want a bespoke grid edit `config.toml` directly (the confirm page
//! points them at the right block).

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use super::PageAction;
use crate::wizard::{
    app::{LayoutPhase, WizardApp},
    state::{CellAssignment, LayoutChoice, WizardState},
    style,
};

/// A named layout offered on this page.
pub struct Preset {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    /// Number of cells the assign page will produce.
    pub cells: usize,
    /// ASCII preview rendered next to the description on the preset
    /// picker. Static art; the dynamic per-cell preview rendered on the
    /// Assign + per-widget pages uses [`grid_def`] instead.
    pub ascii: &'static [&'static str],
    /// Grid dimensions for the dynamic preview.
    pub grid_cols: usize,
    pub grid_rows: usize,
    /// One `(col, row, col_span, row_span)` per cell, in registration
    /// order. The Assign page assigns widgets to cells using this same
    /// order, so cell 0 in `grid_def` matches `assignments[0]`.
    pub grid_def: &'static [(usize, usize, usize, usize)],
}

pub const PRESETS: &[Preset] = &[
    Preset {
        id: "single",
        label: "Single pane (full screen)",
        description: "One widget filling the entire terminal. Useful for a focused tool view.",
        cells: 1,
        ascii: &[
            "┌──────────────┐",
            "│              │",
            "│              │",
            "│              │",
            "└──────────────┘",
        ],
        grid_cols: 1,
        grid_rows: 1,
        grid_def: &[(0, 0, 1, 1)],
    },
    Preset {
        id: "split_vertical",
        label: "Two columns",
        description: "Side-by-side full-height panes — good for a feed + tool combo.",
        cells: 2,
        ascii: &[
            "┌──────┬───────┐",
            "│      │       │",
            "│      │       │",
            "│      │       │",
            "└──────┴───────┘",
        ],
        grid_cols: 2,
        grid_rows: 1,
        grid_def: &[(0, 0, 1, 1), (1, 0, 1, 1)],
    },
    Preset {
        id: "split_horizontal",
        label: "Two rows",
        description: "Stacked full-width panes — top headline + bottom detail.",
        cells: 2,
        ascii: &[
            "┌──────────────┐",
            "│              │",
            "├──────────────┤",
            "│              │",
            "└──────────────┘",
        ],
        grid_cols: 1,
        grid_rows: 2,
        grid_def: &[(0, 0, 1, 1), (0, 1, 1, 1)],
    },
    Preset {
        id: "three_column",
        label: "Three columns",
        description: "Three equal vertical strips — full-height panels.",
        cells: 3,
        ascii: &[
            "┌────┬─────┬───┐",
            "│    │     │   │",
            "│    │     │   │",
            "└────┴─────┴───┘",
        ],
        grid_cols: 3,
        grid_rows: 1,
        grid_def: &[(0, 0, 1, 1), (1, 0, 1, 1), (2, 0, 1, 1)],
    },
    Preset {
        id: "three_row",
        label: "Three rows",
        description: "Three stacked full-width panes.",
        cells: 3,
        ascii: &[
            "┌──────────────┐",
            "│              │",
            "├──────────────┤",
            "│              │",
            "├──────────────┤",
            "│              │",
            "└──────────────┘",
        ],
        grid_cols: 1,
        grid_rows: 3,
        grid_def: &[(0, 0, 1, 1), (0, 1, 1, 1), (0, 2, 1, 1)],
    },
    Preset {
        id: "sidebar_main",
        label: "Sidebar + main",
        description: "Narrow side panel with two stacked cells; large main area on the right.",
        cells: 3,
        ascii: &[
            "┌────┬─────────┐",
            "│    │         │",
            "├────┤         │",
            "│    │         │",
            "└────┴─────────┘",
        ],
        grid_cols: 2,
        grid_rows: 2,
        grid_def: &[(0, 0, 1, 1), (0, 1, 1, 1), (1, 0, 1, 2)],
    },
    Preset {
        id: "main_sidebar",
        label: "Main + sidebar",
        description: "Large main pane on the left, two stacked side cells on the right — mirror of Sidebar + main.",
        cells: 3,
        ascii: &[
            "┌─────────┬────┐",
            "│         │    │",
            "│         ├────┤",
            "│         │    │",
            "└─────────┴────┘",
        ],
        grid_cols: 2,
        grid_rows: 2,
        grid_def: &[(0, 0, 1, 2), (1, 0, 1, 1), (1, 1, 1, 1)],
    },
    Preset {
        id: "two_by_two",
        label: "2 × 2 grid",
        description: "Four equal cells. Compact, balanced.",
        cells: 4,
        ascii: &[
            "┌──────┬───────┐",
            "│      │       │",
            "├──────┼───────┤",
            "│      │       │",
            "└──────┴───────┘",
        ],
        grid_cols: 2,
        grid_rows: 2,
        grid_def: &[
            (0, 0, 1, 1),
            (1, 0, 1, 1),
            (0, 1, 1, 1),
            (1, 1, 1, 1),
        ],
    },
    Preset {
        id: "four_row",
        label: "Four rows",
        description: "Four full-width stacked panes.",
        cells: 4,
        ascii: &[
            "┌──────────────┐",
            "├──────────────┤",
            "├──────────────┤",
            "├──────────────┤",
            "└──────────────┘",
        ],
        grid_cols: 1,
        grid_rows: 4,
        grid_def: &[
            (0, 0, 1, 1),
            (0, 1, 1, 1),
            (0, 2, 1, 1),
            (0, 3, 1, 1),
        ],
    },
    Preset {
        id: "four_column",
        label: "Four columns",
        description: "Four equal vertical strips — full-height panels.",
        cells: 4,
        ascii: &[
            "┌──┬──┬──┬──┐",
            "│  │  │  │  │",
            "│  │  │  │  │",
            "└──┴──┴──┴──┘",
        ],
        grid_cols: 4,
        grid_rows: 1,
        grid_def: &[
            (0, 0, 1, 1),
            (1, 0, 1, 1),
            (2, 0, 1, 1),
            (3, 0, 1, 1),
        ],
    },
    Preset {
        id: "sidebar_three_stack",
        label: "Sidebar + 3 stacked",
        description: "Tall sidebar with three stacked cells on the right.",
        cells: 4,
        ascii: &[
            "┌────┬─────────┐",
            "│    ├─────────┤",
            "│    ├─────────┤",
            "└────┴─────────┘",
        ],
        grid_cols: 2,
        grid_rows: 3,
        grid_def: &[
            (0, 0, 1, 3),
            (1, 0, 1, 1),
            (1, 1, 1, 1),
            (1, 2, 1, 1),
        ],
    },
    Preset {
        id: "three_stack_sidebar",
        label: "3 stacked + sidebar",
        description: "Three full-width cells stacked on the left, tall sidebar on the right — mirror of Sidebar + 3 stacked.",
        cells: 4,
        ascii: &[
            "┌─────────┬────┐",
            "├─────────┤    │",
            "├─────────┤    │",
            "└─────────┴────┘",
        ],
        grid_cols: 2,
        grid_rows: 3,
        grid_def: &[
            (0, 0, 1, 1),
            (0, 1, 1, 1),
            (0, 2, 1, 1),
            (1, 0, 1, 3),
        ],
    },
    Preset {
        id: "dual_sidebar_stack",
        label: "Sidebar + 2 stacked + sidebar",
        description: "Two tall sidebars flanking a pair of stacked middle cells.",
        cells: 4,
        ascii: &[
            "┌────┬──────┬────┐",
            "│    │      │    │",
            "│    ├──────┤    │",
            "│    │      │    │",
            "└────┴──────┴────┘",
        ],
        grid_cols: 3,
        grid_rows: 2,
        grid_def: &[
            (0, 0, 1, 2),
            (1, 0, 1, 1),
            (1, 1, 1, 1),
            (2, 0, 1, 2),
        ],
    },
    Preset {
        id: "magazine",
        label: "Magazine (5 cells)",
        description: "Two rows on top, full-width stocks-style row at the bottom — glint's default seed.",
        cells: 5,
        ascii: &[
            "┌────┬─────────┐",
            "│    │         │",
            "├────┼─────────┤",
            "│    │         │",
            "├────┴─────────┤",
            "│              │",
            "└──────────────┘",
        ],
        grid_cols: 2,
        grid_rows: 3,
        grid_def: &[
            (0, 0, 1, 1),
            (1, 0, 1, 1),
            (0, 1, 1, 1),
            (1, 1, 1, 1),
            (0, 2, 2, 1),
        ],
    },
    Preset {
        id: "five_column",
        label: "Five columns",
        description: "Five equal vertical strips — full-height panels.",
        cells: 5,
        ascii: &[
            "┌──┬──┬──┬──┬──┐",
            "│  │  │  │  │  │",
            "└──┴──┴──┴──┴──┘",
        ],
        grid_cols: 5,
        grid_rows: 1,
        grid_def: &[
            (0, 0, 1, 1),
            (1, 0, 1, 1),
            (2, 0, 1, 1),
            (3, 0, 1, 1),
            (4, 0, 1, 1),
        ],
    },
    Preset {
        id: "hero_two_by_two",
        label: "Hero + 2 × 2 grid",
        description: "Full-width hero on top with four equal cells in a grid below.",
        cells: 5,
        ascii: &[
            "┌──────────────┐",
            "├──────┬───────┤",
            "├──────┼───────┤",
            "└──────┴───────┘",
        ],
        grid_cols: 2,
        grid_rows: 3,
        grid_def: &[
            (0, 0, 2, 1),
            (0, 1, 1, 1),
            (1, 1, 1, 1),
            (0, 2, 1, 1),
            (1, 2, 1, 1),
        ],
    },
    Preset {
        id: "five_row",
        label: "Five rows",
        description: "Five full-width stacked panes.",
        cells: 5,
        ascii: &[
            "┌──────────────┐",
            "├──────────────┤",
            "├──────────────┤",
            "├──────────────┤",
            "├──────────────┤",
            "└──────────────┘",
        ],
        grid_cols: 1,
        grid_rows: 5,
        grid_def: &[
            (0, 0, 1, 1),
            (0, 1, 1, 1),
            (0, 2, 1, 1),
            (0, 3, 1, 1),
            (0, 4, 1, 1),
        ],
    },
    Preset {
        id: "sidebar_quad",
        label: "Sidebar + 2 × 2",
        description: "Tall sidebar with a 2 × 2 grid of four cells on the right.",
        cells: 5,
        ascii: &[
            "┌────┬────┬────┐",
            "│    │    │    │",
            "│    ├────┼────┤",
            "│    │    │    │",
            "└────┴────┴────┘",
        ],
        grid_cols: 3,
        grid_rows: 2,
        grid_def: &[
            (0, 0, 1, 2),
            (1, 0, 1, 1),
            (2, 0, 1, 1),
            (1, 1, 1, 1),
            (2, 1, 1, 1),
        ],
    },
    Preset {
        id: "hero_four_column",
        label: "Hero + four columns",
        description: "Full-width hero on top with four equal columns beneath.",
        cells: 5,
        ascii: &[
            "┌───────────────┐",
            "├───┬───┬───┬───┤",
            "│   │   │   │   │",
            "└───┴───┴───┴───┘",
        ],
        grid_cols: 4,
        grid_rows: 2,
        grid_def: &[
            (0, 0, 4, 1),
            (0, 1, 1, 1),
            (1, 1, 1, 1),
            (2, 1, 1, 1),
            (3, 1, 1, 1),
        ],
    },
    Preset {
        id: "two_by_three",
        label: "2 × 3 grid",
        description: "Two columns, three rows — six equal cells.",
        cells: 6,
        ascii: &[
            "┌──────┬───────┐",
            "├──────┼───────┤",
            "├──────┼───────┤",
            "└──────┴───────┘",
        ],
        grid_cols: 2,
        grid_rows: 3,
        grid_def: &[
            (0, 0, 1, 1),
            (1, 0, 1, 1),
            (0, 1, 1, 1),
            (1, 1, 1, 1),
            (0, 2, 1, 1),
            (1, 2, 1, 1),
        ],
    },
    Preset {
        id: "three_by_two",
        label: "3 × 2 grid",
        description: "Three columns, two rows — six equal cells.",
        cells: 6,
        ascii: &[
            "┌────┬────┬────┐",
            "├────┼────┼────┤",
            "└────┴────┴────┘",
        ],
        grid_cols: 3,
        grid_rows: 2,
        grid_def: &[
            (0, 0, 1, 1),
            (1, 0, 1, 1),
            (2, 0, 1, 1),
            (0, 1, 1, 1),
            (1, 1, 1, 1),
            (2, 1, 1, 1),
        ],
    },
    Preset {
        id: "hero_five_column",
        label: "Hero + five columns",
        description: "Full-width hero on top with five equal columns beneath.",
        cells: 6,
        ascii: &[
            "┌──────────────┐",
            "├──┬──┬──┬──┬──┤",
            "│  │  │  │  │  │",
            "└──┴──┴──┴──┴──┘",
        ],
        grid_cols: 5,
        grid_rows: 2,
        grid_def: &[
            (0, 0, 5, 1),
            (0, 1, 1, 1),
            (1, 1, 1, 1),
            (2, 1, 1, 1),
            (3, 1, 1, 1),
            (4, 1, 1, 1),
        ],
    },
    Preset {
        id: "header_quad_footer",
        label: "Header + 2 × 2 + footer",
        description: "Full-width header, 2 × 2 grid in the middle, full-width footer.",
        cells: 6,
        ascii: &[
            "┌──────────────┐",
            "├──────┬───────┤",
            "├──────┼───────┤",
            "├──────┴───────┤",
            "└──────────────┘",
        ],
        grid_cols: 2,
        grid_rows: 4,
        grid_def: &[
            (0, 0, 2, 1),
            (0, 1, 1, 1),
            (1, 1, 1, 1),
            (0, 2, 1, 1),
            (1, 2, 1, 1),
            (0, 3, 2, 1),
        ],
    },
    Preset {
        id: "sidebar_hero_quad",
        label: "Sidebar + hero + 2 × 2",
        description: "Tall sidebar with a hero band atop a 2 × 2 grid on its right.",
        cells: 6,
        ascii: &[
            "┌────┬─────────┐",
            "│    │         │",
            "│    ├────┬────┤",
            "│    ├────┼────┤",
            "└────┴────┴────┘",
        ],
        grid_cols: 3,
        grid_rows: 3,
        grid_def: &[
            (0, 0, 1, 3),
            (1, 0, 2, 1),
            (1, 1, 1, 1),
            (2, 1, 1, 1),
            (1, 2, 1, 1),
            (2, 2, 1, 1),
        ],
    },
    Preset {
        id: "column_quad_column",
        label: "Column + 2 × 2 + column",
        description: "Two full-height side columns bracketing a 2 × 2 grid.",
        cells: 6,
        ascii: &[
            "┌───┬─────┬─────┬───┐",
            "│   │     │     │   │",
            "│   ├─────┼─────┤   │",
            "│   │     │     │   │",
            "└───┴─────┴─────┴───┘",
        ],
        grid_cols: 4,
        grid_rows: 2,
        grid_def: &[
            (0, 0, 1, 2),
            (1, 0, 1, 1),
            (2, 0, 1, 1),
            (1, 1, 1, 1),
            (2, 1, 1, 1),
            (3, 0, 1, 2),
        ],
    },
    Preset {
        id: "quad_two_columns",
        label: "2 × 2 + two columns",
        description: "2 × 2 grid on the left, two full-height columns on the right.",
        cells: 6,
        ascii: &[
            "┌─────┬─────┬───┬───┐",
            "│     │     │   │   │",
            "├─────┼─────┤   │   │",
            "│     │     │   │   │",
            "└─────┴─────┴───┴───┘",
        ],
        grid_cols: 4,
        grid_rows: 2,
        grid_def: &[
            (0, 0, 1, 1),
            (1, 0, 1, 1),
            (0, 1, 1, 1),
            (1, 1, 1, 1),
            (2, 0, 1, 2),
            (3, 0, 1, 2),
        ],
    },
    Preset {
        id: "two_columns_quad",
        label: "Two columns + 2 × 2",
        description: "Two full-height columns on the left, 2 × 2 grid on the right.",
        cells: 6,
        ascii: &[
            "┌───┬───┬─────┬─────┐",
            "│   │   │     │     │",
            "│   │   ├─────┼─────┤",
            "│   │   │     │     │",
            "└───┴───┴─────┴─────┘",
        ],
        grid_cols: 4,
        grid_rows: 2,
        grid_def: &[
            (0, 0, 1, 2),
            (1, 0, 1, 2),
            (2, 0, 1, 1),
            (3, 0, 1, 1),
            (2, 1, 1, 1),
            (3, 1, 1, 1),
        ],
    },
    Preset {
        id: "hero_two_rows",
        label: "Hero + two rows of three",
        description: "Full-width hero on top with two rows of three cells beneath.",
        cells: 7,
        ascii: &[
            "┌──────────────┐",
            "├────┬────┬────┤",
            "├────┼────┼────┤",
            "└────┴────┴────┘",
        ],
        grid_cols: 3,
        grid_rows: 3,
        grid_def: &[
            (0, 0, 3, 1),
            (0, 1, 1, 1),
            (1, 1, 1, 1),
            (2, 1, 1, 1),
            (0, 2, 1, 1),
            (1, 2, 1, 1),
            (2, 2, 1, 1),
        ],
    },
    Preset {
        id: "sidebar_three_by_two",
        label: "Sidebar + 3 × 2",
        description: "Tall sidebar on the left with a 3 × 2 grid of six cells to its right.",
        cells: 7,
        ascii: &[
            "┌────┬───┬───┬───┐",
            "│    ├───┼───┼───┤",
            "└────┴───┴───┴───┘",
        ],
        grid_cols: 4,
        grid_rows: 2,
        grid_def: &[
            (0, 0, 1, 2),
            (1, 0, 1, 1),
            (2, 0, 1, 1),
            (3, 0, 1, 1),
            (1, 1, 1, 1),
            (2, 1, 1, 1),
            (3, 1, 1, 1),
        ],
    },
    Preset {
        id: "magazine_seven",
        label: "Magazine + footer (7 cells)",
        description: "Three rows of paired left/right cells with a full-width footer underneath.",
        cells: 7,
        ascii: &[
            "┌────┬─────────┐",
            "├────┼─────────┤",
            "├────┼─────────┤",
            "├────┴─────────┤",
            "└──────────────┘",
        ],
        grid_cols: 2,
        grid_rows: 4,
        grid_def: &[
            (0, 0, 1, 1),
            (1, 0, 1, 1),
            (0, 1, 1, 1),
            (1, 1, 1, 1),
            (0, 2, 1, 1),
            (1, 2, 1, 1),
            (0, 3, 2, 1),
        ],
    },
    Preset {
        id: "header_five_footer",
        label: "Header + 5 columns + footer",
        description: "Full-width header, five equal columns in the middle, full-width footer.",
        cells: 7,
        ascii: &[
            "┌──────────────┐",
            "├──┬──┬──┬──┬──┤",
            "│  │  │  │  │  │",
            "├──┴──┴──┴──┴──┤",
            "└──────────────┘",
        ],
        grid_cols: 5,
        grid_rows: 3,
        grid_def: &[
            (0, 0, 5, 1),
            (0, 1, 1, 1),
            (1, 1, 1, 1),
            (2, 1, 1, 1),
            (3, 1, 1, 1),
            (4, 1, 1, 1),
            (0, 2, 5, 1),
        ],
    },
    Preset {
        id: "header_four_dual_footer",
        label: "Header + 4 columns + 2 wide footers",
        description: "Full-width header, four columns in the middle, two wide footers each spanning two columns.",
        cells: 7,
        ascii: &[
            "┌───────────────────┐",
            "├────┬────┬────┬────┤",
            "│    │    │    │    │",
            "├────┴────┼────┴────┤",
            "└─────────┴─────────┘",
        ],
        grid_cols: 4,
        grid_rows: 3,
        grid_def: &[
            (0, 0, 4, 1),
            (0, 1, 1, 1),
            (1, 1, 1, 1),
            (2, 1, 1, 1),
            (3, 1, 1, 1),
            (0, 2, 2, 1),
            (2, 2, 2, 1),
        ],
    },
    Preset {
        id: "dashboard_seven",
        label: "Dashboard (2 × 3 + hero footer)",
        description: "Two rows of three cells with a full-width hero footer below — summary-at-the-bottom style.",
        cells: 7,
        ascii: &[
            "┌────┬────┬────┐",
            "├────┼────┼────┤",
            "├────┴────┴────┤",
            "└──────────────┘",
        ],
        grid_cols: 3,
        grid_rows: 3,
        grid_def: &[
            (0, 0, 1, 1),
            (1, 0, 1, 1),
            (2, 0, 1, 1),
            (0, 1, 1, 1),
            (1, 1, 1, 1),
            (2, 1, 1, 1),
            (0, 2, 3, 1),
        ],
    },
    Preset {
        id: "four_by_two",
        label: "4 × 2 grid",
        description: "Four columns, two rows — eight equal cells.",
        cells: 8,
        ascii: &[
            "┌───┬───┬───┬───┐",
            "├───┼───┼───┼───┤",
            "└───┴───┴───┴───┘",
        ],
        grid_cols: 4,
        grid_rows: 2,
        grid_def: &[
            (0, 0, 1, 1),
            (1, 0, 1, 1),
            (2, 0, 1, 1),
            (3, 0, 1, 1),
            (0, 1, 1, 1),
            (1, 1, 1, 1),
            (2, 1, 1, 1),
            (3, 1, 1, 1),
        ],
    },
    Preset {
        id: "two_by_four",
        label: "2 × 4 grid",
        description: "Two columns, four rows — eight equal cells.",
        cells: 8,
        ascii: &[
            "┌──────┬───────┐",
            "├──────┼───────┤",
            "├──────┼───────┤",
            "├──────┼───────┤",
            "└──────┴───────┘",
        ],
        grid_cols: 2,
        grid_rows: 4,
        grid_def: &[
            (0, 0, 1, 1),
            (1, 0, 1, 1),
            (0, 1, 1, 1),
            (1, 1, 1, 1),
            (0, 2, 1, 1),
            (1, 2, 1, 1),
            (0, 3, 1, 1),
            (1, 3, 1, 1),
        ],
    },
    Preset {
        id: "sidebar_3x2_footer",
        label: "Sidebar + 3 × 2 + footer",
        description: "Tall sidebar, a 3 × 2 grid of six cells to its right, and a full-width footer beneath.",
        cells: 8,
        ascii: &[
            "┌────┬───┬───┬───┐",
            "│    ├───┼───┼───┤",
            "├────┴───┴───┴───┤",
            "└────────────────┘",
        ],
        grid_cols: 4,
        grid_rows: 3,
        grid_def: &[
            (0, 0, 1, 2),
            (1, 0, 1, 1),
            (2, 0, 1, 1),
            (3, 0, 1, 1),
            (1, 1, 1, 1),
            (2, 1, 1, 1),
            (3, 1, 1, 1),
            (0, 2, 4, 1),
        ],
    },
    Preset {
        id: "dual_band_three",
        label: "Stripes (1 + 3 + 1 + 3)",
        description: "Alternating full-width band and three-column row, twice — two summary bars over two trios of cells.",
        cells: 8,
        ascii: &[
            "┌──────────────┐",
            "├────┬────┬────┤",
            "├────┴────┴────┤",
            "├────┬────┬────┤",
            "└────┴────┴────┘",
        ],
        grid_cols: 3,
        grid_rows: 4,
        grid_def: &[
            (0, 0, 3, 1),
            (0, 1, 1, 1),
            (1, 1, 1, 1),
            (2, 1, 1, 1),
            (0, 2, 3, 1),
            (0, 3, 1, 1),
            (1, 3, 1, 1),
            (2, 3, 1, 1),
        ],
    },
    Preset {
        id: "header_grid_footer",
        label: "Header + 2 × 3 + footer",
        description: "Full-width header, six cells as two rows of three, full-width footer.",
        cells: 8,
        ascii: &[
            "┌──────────────┐",
            "├────┬────┬────┤",
            "├────┼────┼────┤",
            "├────┴────┴────┤",
            "└──────────────┘",
        ],
        grid_cols: 3,
        grid_rows: 4,
        grid_def: &[
            (0, 0, 3, 1),
            (0, 1, 1, 1),
            (1, 1, 1, 1),
            (2, 1, 1, 1),
            (0, 2, 1, 1),
            (1, 2, 1, 1),
            (2, 2, 1, 1),
            (0, 3, 3, 1),
        ],
    },
    Preset {
        id: "header_sidebar_grid",
        label: "Header + sidebar + 2 × 3",
        description: "Full-width header on top with a tall sidebar and 2 × 3 grid beneath.",
        cells: 8,
        ascii: &[
            "┌──────────────────┐",
            "├────┬──────┬──────┤",
            "│    ├──────┼──────┤",
            "│    ├──────┼──────┤",
            "└────┴──────┴──────┘",
        ],
        grid_cols: 3,
        grid_rows: 4,
        grid_def: &[
            (0, 0, 3, 1),
            (0, 1, 1, 3),
            (1, 1, 1, 1),
            (2, 1, 1, 1),
            (1, 2, 1, 1),
            (2, 2, 1, 1),
            (1, 3, 1, 1),
            (2, 3, 1, 1),
        ],
    },
    Preset {
        id: "sidebar_grid_footer",
        label: "Sidebar + 2 × 3 + footer",
        description: "Tall sidebar with a 2 × 3 grid of six cells to its right, plus a full-width footer.",
        cells: 8,
        ascii: &[
            "┌────┬──────┬──────┐",
            "│    ├──────┼──────┤",
            "│    ├──────┼──────┤",
            "├────┴──────┴──────┤",
            "└──────────────────┘",
        ],
        grid_cols: 3,
        grid_rows: 4,
        grid_def: &[
            (0, 0, 1, 3),
            (1, 0, 1, 1),
            (2, 0, 1, 1),
            (1, 1, 1, 1),
            (2, 1, 1, 1),
            (1, 2, 1, 1),
            (2, 2, 1, 1),
            (0, 3, 3, 1),
        ],
    },
];

const MIN_PANES: usize = 1;
const MAX_PANES: usize = 8;

pub fn handle_key(key: KeyEvent, app: &mut WizardApp) -> PageAction {
    match app.layout_phase {
        LayoutPhase::PickCount => handle_count_key(key, app),
        LayoutPhase::PickPreset => handle_preset_key(key, app),
    }
}

fn handle_count_key(key: KeyEvent, app: &mut WizardApp) -> PageAction {
    let cur = current_count(app).unwrap_or(default_count(app));
    match key.code {
        KeyCode::Left | KeyCode::Char('h') | KeyCode::Up | KeyCode::Char('k') => {
            let next = if cur > MIN_PANES { cur - 1 } else { MAX_PANES };
            app.text_buffer = next.to_string();
            PageAction::Stay
        }
        KeyCode::Right | KeyCode::Char('l') | KeyCode::Down | KeyCode::Char('j') => {
            let next = if cur < MAX_PANES { cur + 1 } else { MIN_PANES };
            app.text_buffer = next.to_string();
            PageAction::Stay
        }
        KeyCode::Char(c) if c.is_ascii_digit() => {
            let d = (c as u8 - b'0') as usize;
            if (MIN_PANES..=MAX_PANES).contains(&d) {
                app.text_buffer = d.to_string();
            }
            PageAction::Stay
        }
        // KeepExisting option — `k` jumps straight to "use existing
        // layout" and skips preset selection.
        KeyCode::Char('e') | KeyCode::Char('E') => {
            commit_keep_existing(app);
            PageAction::Advance
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let count = current_count(app).unwrap_or(default_count(app));
            app.text_buffer = count.to_string();
            app.layout_phase = LayoutPhase::PickPreset;
            app.focus = 0;
            PageAction::Stay
        }
        KeyCode::Esc => PageAction::Back,
        _ => PageAction::Stay,
    }
}

fn handle_preset_key(key: KeyEvent, app: &mut WizardApp) -> PageAction {
    let count = current_count(app).unwrap_or(default_count(app));
    let matching: Vec<&Preset> = presets_for(count).collect();
    if matching.is_empty() {
        // No matching preset — only Back / "Keep existing" should work.
        return match key.code {
            KeyCode::Esc => {
                app.layout_phase = LayoutPhase::PickCount;
                app.focus = 0;
                PageAction::Stay
            }
            KeyCode::Char('e') | KeyCode::Char('E') => {
                commit_keep_existing(app);
                PageAction::Advance
            }
            _ => PageAction::Stay,
        };
    }
    let n = matching.len();
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            app.focus = (app.focus + n - 1) % n;
            PageAction::Stay
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.focus = (app.focus + 1) % n;
            PageAction::Stay
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let preset = matching[app.focus.min(n - 1)];
            commit_preset(app, preset);
            PageAction::Advance
        }
        KeyCode::Esc => {
            // Back from preset selection returns to count picker, not the
            // global page — the user usually just wants to bump the count.
            app.layout_phase = LayoutPhase::PickCount;
            app.focus = 0;
            PageAction::Stay
        }
        _ => PageAction::Stay,
    }
}

fn commit_preset(app: &mut WizardApp, preset: &Preset) {
    let choice = LayoutChoice::Preset {
        name: preset.id.into(),
    };
    seed_assignments(&mut app.state, &choice);
    app.state.layout = choice;
}

fn commit_keep_existing(app: &mut WizardApp) {
    app.state.layout = LayoutChoice::KeepExisting;
}

/// Pre-fill the assignments list with empty slots for the chosen preset so
/// the Assign page can iterate cells without computing them itself.
fn seed_assignments(state: &mut WizardState, choice: &LayoutChoice) {
    let want = match choice {
        LayoutChoice::Preset { name } => PRESETS
            .iter()
            .find(|p| p.id == name)
            .map(|p| p.cells)
            .unwrap_or(0),
        LayoutChoice::KeepExisting => return,
    };
    state.assignments.truncate(want);
    while state.assignments.len() < want {
        state.assignments.push(CellAssignment {
            cell_index: state.assignments.len(),
            kind: String::new(),
            instance: "main".into(),
            stack_children: Vec::new(),
        });
    }
}

fn current_count(app: &WizardApp) -> Option<usize> {
    app.text_buffer
        .parse::<usize>()
        .ok()
        .filter(|n| (MIN_PANES..=MAX_PANES).contains(n))
}

fn default_count(app: &WizardApp) -> usize {
    // On re-entry, default to whatever count the user previously chose.
    // For KeepExisting, use the assignment count we hydrated from
    // config.toml so the count picker starts on the user's actual layout
    // size rather than an arbitrary fallback.
    match &app.state.layout {
        LayoutChoice::Preset { name } => PRESETS
            .iter()
            .find(|p| p.id == name)
            .map(|p| p.cells)
            .unwrap_or(4),
        LayoutChoice::KeepExisting => app.state.assignments.len().clamp(MIN_PANES, MAX_PANES),
    }
}

fn presets_for(count: usize) -> impl Iterator<Item = &'static Preset> {
    PRESETS.iter().filter(move |p| p.cells == count)
}

pub fn render(frame: &mut Frame, area: Rect, app: &WizardApp) {
    let title = match app.layout_phase {
        LayoutPhase::PickCount => " Layout — step 1 of 2: how many panes? ",
        LayoutPhase::PickPreset => " Layout — step 2 of 2: choose a preset ",
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = style::pad_inner(block.inner(area));
    frame.render_widget(block, area);

    match app.layout_phase {
        LayoutPhase::PickCount => render_count_picker(frame, inner, app),
        LayoutPhase::PickPreset => render_preset_picker(frame, inner, app),
    }
}

fn render_count_picker(frame: &mut Frame, area: Rect, app: &WizardApp) {
    let count = current_count(app).unwrap_or(default_count(app));
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "How many widgets do you want on the dashboard?",
        style::section_header(),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(
            "  Each pane holds one widget. Pick a count from {} to {}; the next \
             step offers preset grid shapes that fit it.",
            MIN_PANES, MAX_PANES
        ),
        style::blurb(),
    )));
    lines.push(Line::from(""));

    // Big count display.
    lines.push(Line::from(vec![
        Span::raw("      "),
        Span::styled("◀ ", style::key_hint()),
        Span::styled(
            format!("{count} pane{} ", if count == 1 { "" } else { "s" }),
            style::value_focused(),
        ),
        Span::styled("▶", style::key_hint()),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("       Press 1–{MAX_PANES} or use ←/→ ↑/↓ to adjust. Enter to continue."),
        style::help_text(),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Already happy with your existing layout?",
        style::label(),
    )));
    lines.push(Line::from(vec![
        Span::raw("      "),
        Span::styled("[E]", style::key_hint()),
        Span::raw(" "),
        Span::styled(
            "Keep existing — leaves the [layout] block in config.toml untouched.",
            style::value_idle(),
        ),
    ]));

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_preset_picker(frame: &mut Frame, area: Rect, app: &WizardApp) {
    let count = current_count(app).unwrap_or(default_count(app));
    let matching: Vec<&Preset> = presets_for(count).collect();

    if matching.is_empty() {
        let lines = vec![
            Line::from(Span::styled(
                format!("No presets for {count} panes."),
                style::error(),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  Press Esc to go back and pick a different count, or [E] to \
                 keep your existing layout.",
                style::help_text(),
            )),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
        return;
    }

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    render_preset_list(frame, cols[0], &matching, app.focus);
    render_preset_preview(frame, cols[1], matching[app.focus.min(matching.len() - 1)]);
}

fn render_preset_list(frame: &mut Frame, area: Rect, matching: &[&Preset], focus: usize) {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("Pick a {}-pane layout:", matching[0].cells),
        style::section_header(),
    )));
    lines.push(Line::from(""));
    for (i, preset) in matching.iter().enumerate() {
        let focused = i == focus;
        let marker = if focused { "▶ " } else { "  " };
        let label_style = if focused {
            style::option_selected()
        } else {
            style::option_idle()
        };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(marker.to_string(), label_style),
            Span::styled(preset.label.to_string(), label_style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  ↑/↓ to highlight · Enter to pick · Esc to change count",
        style::help_text(),
    )));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_preset_preview(frame: &mut Frame, area: Rect, preset: &Preset) {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        preset.label.to_string(),
        style::section_header(),
    )));
    lines.push(Line::from(""));
    for row in preset.ascii {
        lines.push(Line::from(Span::styled(
            row.to_string(),
            style::value_idle(),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        preset.description.to_string(),
        style::blurb(),
    )));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_pane_count_has_at_least_two_presets() {
        // 1-pane is the natural exception — there's only one geometry for
        // a single full-screen pane. Every other supported count should
        // give the user a real choice.
        for count in 2..=MAX_PANES {
            let n = presets_for(count).count();
            assert!(
                n >= 2,
                "pane count {count} only has {n} preset(s); need at least 2"
            );
        }
    }

    #[test]
    fn preset_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for p in PRESETS {
            assert!(seen.insert(p.id), "duplicate preset id: {}", p.id);
        }
    }

    #[test]
    fn preset_grid_def_matches_cell_count() {
        for p in PRESETS {
            assert_eq!(
                p.grid_def.len(),
                p.cells,
                "preset {} declares {} cells but grid_def has {} entries",
                p.id,
                p.cells,
                p.grid_def.len()
            );
        }
    }
}
