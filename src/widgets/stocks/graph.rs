//! Braille-dot graph renderer.
//!
//! Each U+2800–U+28FF braille char represents an 8-pixel cell laid out as
//! 4 rows × 2 cols of "sub-pixels":
//!
//! ```text
//! dot 1 (0x01)  dot 4 (0x08)
//! dot 2 (0x02)  dot 5 (0x10)
//! dot 3 (0x04)  dot 6 (0x20)
//! dot 7 (0x40)  dot 8 (0x80)
//! ```
//!
//! So one char gives us 4× vertical and 2× horizontal resolution vs. plain
//! ASCII, which is plenty for compact intraday price charts.

const BRAILLE_BASE: u32 = 0x2800;
const SUB_ROWS_PER_CHAR: usize = 4;
const SUB_COLS_PER_CHAR: usize = 2;

/// Bit positions of the 8 sub-pixels inside the braille code point, indexed by
/// `(row, col)` where row 0 is the top.
const BIT_AT: [[u32; SUB_COLS_PER_CHAR]; SUB_ROWS_PER_CHAR] = [
    [0x01, 0x08],
    [0x02, 0x10],
    [0x04, 0x20],
    [0x40, 0x80],
];

/// Plot the y-value series `points` into `rows × cols` chars (one char = 4×2
/// sub-pixels). `(min, max)` is the visible y-range; values outside the range
/// are clamped. Returns `rows` strings, each `cols` chars wide.
#[allow(clippy::needless_range_loop)] // index-driven on purpose: we set bitmap[y][x] each iter.
pub fn render_series(points: &[f64], rows: u16, cols: u16, min: f64, max: f64) -> Vec<String> {
    let rows = rows as usize;
    let cols = cols as usize;
    if rows == 0 || cols == 0 || points.len() < 2 {
        return vec![String::new(); rows];
    }

    let sub_rows = rows * SUB_ROWS_PER_CHAR;
    let sub_cols = cols * SUB_COLS_PER_CHAR;
    let span = (max - min).max(f64::MIN_POSITIVE);

    // Bitmap of sub-pixels we want lit.
    let mut bitmap = vec![vec![false; sub_cols]; sub_rows];

    let n = points.len();
    let mut prev_y: Option<usize> = None;
    for x in 0..sub_cols {
        // Map x ∈ [0, sub_cols) onto the input series via nearest-neighbor.
        let idx = if sub_cols == 1 {
            0
        } else {
            (x * (n - 1)) / (sub_cols - 1)
        };
        let v = points[idx];
        // y=0 at top → high values render at top. Standard chart convention.
        let frac = ((v - min) / span).clamp(0.0, 1.0);
        let y = ((1.0 - frac) * (sub_rows as f64 - 1.0)).round() as usize;
        let y = y.min(sub_rows - 1);
        bitmap[y][x] = true;
        // Draw a connecting line between adjacent samples so the trace looks
        // continuous instead of dotty when steep.
        if let Some(py) = prev_y {
            let (mut a, b) = if py <= y { (py, y) } else { (y, py) };
            if b - a > 1 {
                a += 1;
                while a < b {
                    bitmap[a][x] = true;
                    a += 1;
                }
            }
        }
        prev_y = Some(y);
    }

    // Collapse the bitmap into rows of braille chars.
    let mut out: Vec<String> = Vec::with_capacity(rows);
    for r in 0..rows {
        let mut row = String::with_capacity(cols);
        for c in 0..cols {
            let mut bits: u32 = 0;
            for (sub_r, bit_row) in BIT_AT.iter().enumerate() {
                for (sub_c, bit) in bit_row.iter().enumerate() {
                    let y = r * SUB_ROWS_PER_CHAR + sub_r;
                    let x = c * SUB_COLS_PER_CHAR + sub_c;
                    if y < sub_rows && x < sub_cols && bitmap[y][x] {
                        bits |= *bit;
                    }
                }
            }
            // U+2800 alone (no dots) renders as zero-width on some fonts, so
            // emit a space for fully-blank cells to keep column widths stable.
            let ch = if bits == 0 {
                ' '
            } else {
                char::from_u32(BRAILLE_BASE + bits).unwrap_or(' ')
            };
            row.push(ch);
        }
        out.push(row);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_or_tiny_series_returns_blank_rows() {
        assert_eq!(render_series(&[], 3, 10, 0.0, 1.0).len(), 3);
        assert_eq!(render_series(&[1.0], 3, 10, 0.0, 1.0).len(), 3);
    }

    #[test]
    fn render_series_produces_requested_dimensions() {
        let points: Vec<f64> = (0..50).map(|i| (i as f64).sin()).collect();
        let out = render_series(&points, 4, 20, -1.0, 1.0);
        assert_eq!(out.len(), 4);
        for row in &out {
            assert_eq!(row.chars().count(), 20);
        }
    }

    #[test]
    fn flat_series_at_max_value_renders_at_top_row() {
        let points = vec![10.0; 30];
        let out = render_series(&points, 3, 10, 0.0, 10.0);
        // Top row should have braille glyphs, lower rows should be empty
        // (just spaces).
        assert!(out[0].chars().any(|c| c != ' '));
        assert!(out[2].chars().all(|c| c == ' '));
    }

    #[test]
    fn flat_series_at_min_value_renders_at_bottom_row() {
        let points = vec![0.0; 30];
        let out = render_series(&points, 3, 10, 0.0, 10.0);
        assert!(out[2].chars().any(|c| c != ' '));
        assert!(out[0].chars().all(|c| c == ' '));
    }
}
