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
    pub widget: String,
    pub col: usize,
    pub row: usize,
    #[serde(default = "default_span")]
    pub col_span: usize,
    #[serde(default = "default_span")]
    pub row_span: usize,
}

fn default_span() -> usize {
    1
}

fn default_columns() -> Vec<u16> {
    vec![45, 30, 25]
}

fn default_rows() -> Vec<u16> {
    vec![45, 55]
}

fn default_cells() -> Vec<GridCell> {
    vec![
        GridCell {
            widget: "stocks".into(),
            col: 0,
            row: 0,
            col_span: 1,
            row_span: 2,
        },
        GridCell {
            widget: "clock".into(),
            col: 1,
            row: 0,
            col_span: 1,
            row_span: 1,
        },
        GridCell {
            widget: "weather".into(),
            col: 1,
            row: 1,
            col_span: 1,
            row_span: 1,
        },
        GridCell {
            widget: "calendar".into(),
            col: 2,
            row: 0,
            col_span: 1,
            row_span: 1,
        },
        GridCell {
            widget: "news".into(),
            col: 2,
            row: 1,
            col_span: 1,
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
    weights.iter().map(|w| Constraint::Ratio(u32::from(*w), weights.iter().map(|x| u32::from(*x)).sum())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_layout_has_five_cells() {
        let layout = LayoutConfig::default();
        assert_eq!(layout.cells.len(), 5);
        let widgets: Vec<&str> = layout.cells.iter().map(|c| c.widget.as_str()).collect();
        assert_eq!(widgets, vec!["stocks", "clock", "weather", "calendar", "news"]);
    }

    #[test]
    fn resolve_fills_area() {
        let layout = LayoutConfig::default();
        let area = Rect::new(0, 0, 100, 40);
        let resolved = layout.resolve(area);
        assert_eq!(resolved.len(), 5);

        // Stocks spans both rows in col 0.
        assert_eq!(resolved[0].cell.widget, "stocks");
        assert_eq!(resolved[0].area.x, 0);
        assert_eq!(resolved[0].area.y, 0);
        assert_eq!(resolved[0].area.height, 40);

        // Clock above weather in col 1.
        assert_eq!(resolved[1].cell.widget, "clock");
        assert_eq!(resolved[2].cell.widget, "weather");
        assert_eq!(
            resolved[1].area.y + resolved[1].area.height,
            resolved[2].area.y
        );

        // Calendar above news in col 2.
        assert_eq!(resolved[3].cell.widget, "calendar");
        assert_eq!(resolved[4].cell.widget, "news");
        assert_eq!(
            resolved[3].area.y + resolved[3].area.height,
            resolved[4].area.y
        );
    }

    #[test]
    fn resolve_skips_out_of_bounds_cells() {
        let mut layout = LayoutConfig::default();
        layout.cells.push(GridCell {
            widget: "ghost".into(),
            col: 5,
            row: 5,
            col_span: 1,
            row_span: 1,
        });
        let resolved = layout.resolve(Rect::new(0, 0, 100, 40));
        assert!(resolved.iter().all(|r| r.cell.widget != "ghost"));
    }
}
