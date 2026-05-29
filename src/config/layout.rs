// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct LayoutConfig {
    #[serde(default = "default_columns")]
    pub columns: Vec<u16>,

    #[serde(default = "default_rows")]
    pub rows: Vec<u16>,

    #[serde(default = "default_cells")]
    pub cells: Vec<GridCell>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GridCell {
    /// Single-widget cell: `widget = "clock"`. Mutually exclusive with
    /// `widgets` — exactly one of the two must be present. The render
    /// path checks `is_stack()` to decide which one to read.
    #[serde(default)]
    pub widget: Option<String>,
    /// Stack cell: `widgets = ["clock", "weather", "stocks"]`. Up to 3
    /// widgets share one cell; only one is visible at a time. Empty
    /// strings are dropped at parse time so the stack is always N
    /// contiguous widgets. After dropping empties, a stack of size 1
    /// degrades to a regular cell (no tab strip).
    #[serde(default)]
    pub widgets: Option<Vec<String>>,
    pub col: usize,
    pub row: usize,
    #[serde(default = "default_span")]
    pub col_span: usize,
    #[serde(default = "default_span")]
    pub row_span: usize,
}

/// Cap on stack size. Cells with more than `MAX_STACK_WIDGETS`
/// entries in `widgets` are truncated (with a warning logged)
/// rather than rejected, so a hand-edited config doesn't fail to
/// launch over an obvious typo.
pub const MAX_STACK_WIDGETS: usize = 3;

impl GridCell {
    /// `true` when this cell holds 2+ widgets in a stack. A `widgets`
    /// field that resolves to a single non-empty entry (or zero
    /// entries) does NOT count — those degrade to single-widget cells
    /// per the spec.
    pub fn is_stack(&self) -> bool {
        self.stack_widget_refs().len() >= 2
    }

    /// Normalised list of widget refs in this stack (gap-stripped,
    /// capped to MAX_STACK_WIDGETS). Empty when the cell isn't a
    /// stack (or when the stack lost all its widgets to gap-stripping
    /// — the caller falls back to the single-widget path).
    pub fn stack_widget_refs(&self) -> Vec<String> {
        match &self.widgets {
            None => Vec::new(),
            Some(list) => {
                let mut out: Vec<String> = list
                    .iter()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if out.len() > MAX_STACK_WIDGETS {
                    tracing::warn!(
                        cell_widgets = ?list,
                        kept = MAX_STACK_WIDGETS,
                        "stack cell has more than {} widgets; truncating",
                        MAX_STACK_WIDGETS
                    );
                    out.truncate(MAX_STACK_WIDGETS);
                }
                out
            }
        }
    }

    /// The single widget id for non-stack cells. For stack cells with
    /// only one effective widget after gap-stripping, returns that
    /// widget. Returns `None` only for malformed cells (both fields
    /// missing, or widgets field with no non-empty entries and no
    /// fallback `widget`).
    pub fn primary_widget(&self) -> Option<String> {
        let stack = self.stack_widget_refs();
        if stack.len() == 1 {
            return Some(stack.into_iter().next().unwrap());
        }
        self.widget
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// Identity of the widget instance this cell renders, used to look
    /// up the matching `Box<dyn Widget>` in the `WidgetManager`. For
    /// stacks this is a synthetic id built from the children, joined
    /// by `+`, prefixed `stack:`. For single-widget cells it's just
    /// the widget id (`"clock"` or `"clock@home"`).
    pub fn render_target_id(&self) -> Option<String> {
        if self.is_stack() {
            let joined = self.stack_widget_refs().join("+");
            Some(format!("stack:{joined}"))
        } else {
            self.primary_widget()
        }
    }
}

fn default_span() -> usize {
    1
}

fn default_columns() -> Vec<u16> {
    vec![40, 60]
}

fn default_rows() -> Vec<u16> {
    vec![35, 35, 30]
}

fn default_cells() -> Vec<GridCell> {
    vec![
        GridCell {
            widget: Some("clock".into()),
            widgets: None,
            col: 0,
            row: 0,
            col_span: 1,
            row_span: 1,
        },
        GridCell {
            widget: Some("calendar".into()),
            widgets: None,
            col: 1,
            row: 0,
            col_span: 1,
            row_span: 1,
        },
        GridCell {
            widget: Some("weather".into()),
            widgets: None,
            col: 0,
            row: 1,
            col_span: 1,
            row_span: 1,
        },
        GridCell {
            widget: Some("news".into()),
            widgets: None,
            col: 1,
            row: 1,
            col_span: 1,
            row_span: 1,
        },
        GridCell {
            widget: Some("stocks".into()),
            widgets: None,
            col: 0,
            row: 2,
            col_span: 2,
            row_span: 1,
        },
    ]
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            columns: default_columns(),
            rows: default_rows(),
            cells: default_cells(),
        }
    }
}

/// A grid cell paired with the screen `Rect` it covers.
#[derive(Debug, Clone)]
pub struct ResolvedCell<'a> {
    pub cell: &'a GridCell,
    pub area: Rect,
}

impl LayoutConfig {
    /// Resolve the grid to a list of `Rect`s for each cell, in the order
    /// cells are declared. Cells outside the grid bounds are skipped.
    pub fn resolve<'a>(&'a self, area: Rect) -> Vec<ResolvedCell<'a>> {
        if self.columns.is_empty() || self.rows.is_empty() {
            return Vec::new();
        }

        let col_constraints: Vec<Constraint> = weights_to_constraints(&self.columns);
        let row_constraints: Vec<Constraint> = weights_to_constraints(&self.rows);

        let col_slices = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(col_constraints)
            .split(area);

        let row_slices = Layout::default()
            .direction(Direction::Vertical)
            .constraints(row_constraints)
            .split(area);

        let n_cols = self.columns.len();
        let n_rows = self.rows.len();

        let mut out = Vec::with_capacity(self.cells.len());
        for cell in &self.cells {
            if cell.col >= n_cols || cell.row >= n_rows {
                continue;
            }
            let col_end = (cell.col + cell.col_span.max(1) - 1).min(n_cols - 1);
            let row_end = (cell.row + cell.row_span.max(1) - 1).min(n_rows - 1);

            let x = col_slices[cell.col].x;
            let y = row_slices[cell.row].y;
            let width = col_slices[col_end].x + col_slices[col_end].width - x;
            let height = row_slices[row_end].y + row_slices[row_end].height - y;

            out.push(ResolvedCell {
                cell,
                area: Rect {
                    x,
                    y,
                    width,
                    height,
                },
            });
        }
        out
    }
}

fn weights_to_constraints(weights: &[u16]) -> Vec<Constraint> {
    if weights.is_empty() {
        return vec![Constraint::Percentage(100)];
    }
    weights
        .iter()
        .map(|w| Constraint::Ratio(u32::from(*w), weights.iter().map(|x| u32::from(*x)).sum()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_layout_has_five_cells() {
        let layout = LayoutConfig::default();
        assert_eq!(layout.cells.len(), 5);
        let widgets: Vec<String> = layout
            .cells
            .iter()
            .map(|c| c.primary_widget().unwrap_or_default())
            .collect();
        assert_eq!(
            widgets,
            vec!["clock", "calendar", "weather", "news", "stocks"]
        );
    }

    #[test]
    fn resolve_fills_area() {
        let layout = LayoutConfig::default();
        let area = Rect::new(0, 0, 100, 40);
        let resolved = layout.resolve(area);
        assert_eq!(resolved.len(), 5);

        // Top row: clock left, calendar right.
        assert_eq!(resolved[0].cell.primary_widget().as_deref(), Some("clock"));
        assert_eq!(resolved[0].area.x, 0);
        assert_eq!(resolved[0].area.y, 0);
        assert_eq!(
            resolved[1].cell.primary_widget().as_deref(),
            Some("calendar")
        );
        assert_eq!(resolved[1].area.y, 0);
        assert!(resolved[1].area.x > 0);

        // Middle row: weather left, news right.
        assert_eq!(
            resolved[2].cell.primary_widget().as_deref(),
            Some("weather")
        );
        assert_eq!(resolved[3].cell.primary_widget().as_deref(), Some("news"));
        assert_eq!(resolved[2].area.y, resolved[3].area.y);
        assert!(resolved[2].area.y > resolved[0].area.y);

        // Bottom row: stocks spans both columns.
        assert_eq!(resolved[4].cell.primary_widget().as_deref(), Some("stocks"));
        assert_eq!(resolved[4].area.x, 0);
        assert_eq!(resolved[4].area.width, 100);
        assert!(resolved[4].area.y > resolved[2].area.y);
    }

    #[test]
    fn resolve_skips_out_of_bounds_cells() {
        let mut layout = LayoutConfig::default();
        layout.cells.push(GridCell {
            widget: Some("ghost".into()),
            widgets: None,
            col: 5,
            row: 5,
            col_span: 1,
            row_span: 1,
        });
        let resolved = layout.resolve(Rect::new(0, 0, 100, 40));
        assert!(resolved
            .iter()
            .all(|r| r.cell.primary_widget().as_deref() != Some("ghost")));
    }

    fn stack_cell(widgets: &[&str]) -> GridCell {
        GridCell {
            widget: None,
            widgets: Some(widgets.iter().map(|s| (*s).into()).collect()),
            col: 0,
            row: 0,
            col_span: 1,
            row_span: 1,
        }
    }

    #[test]
    fn stack_cell_is_stack_when_two_or_more_widgets() {
        assert!(stack_cell(&["clock", "weather"]).is_stack());
        assert!(stack_cell(&["clock", "weather", "stocks"]).is_stack());
    }

    #[test]
    fn stack_cell_with_single_widget_degrades_to_non_stack() {
        // Per spec §1: a single-element widgets array renders as a
        // normal cell — no tab strip, no rotation keys.
        let cell = stack_cell(&["clock"]);
        assert!(!cell.is_stack());
        assert_eq!(cell.primary_widget().as_deref(), Some("clock"));
    }

    #[test]
    fn stack_cell_drops_empty_entries() {
        let cell = stack_cell(&["", "clock", "", "weather", ""]);
        assert_eq!(
            cell.stack_widget_refs(),
            vec!["clock".to_string(), "weather".to_string()]
        );
    }

    #[test]
    fn stack_cell_truncates_beyond_max_widgets() {
        let cell = stack_cell(&["a", "b", "c", "d", "e"]);
        assert_eq!(cell.stack_widget_refs().len(), MAX_STACK_WIDGETS);
    }

    #[test]
    fn render_target_id_uses_stack_prefix_for_multi_widget_cells() {
        let cell = stack_cell(&["clock", "weather", "stocks"]);
        assert_eq!(
            cell.render_target_id().as_deref(),
            Some("stack:clock+weather+stocks")
        );
        let single = GridCell {
            widget: Some("clock".into()),
            widgets: None,
            col: 0,
            row: 0,
            col_span: 1,
            row_span: 1,
        };
        assert_eq!(single.render_target_id().as_deref(), Some("clock"));
    }
}
