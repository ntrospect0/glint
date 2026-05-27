/// 3-wide × 5-tall block-character font used by the clock and calendar widgets
/// to render numeric values (digits 0-9, `:`) at a glance.
pub const GLYPH_HEIGHT: usize = 5;
#[cfg(test)]
const GLYPH_WIDTH: usize = 3;

pub fn glyph(c: char) -> Option<[&'static str; GLYPH_HEIGHT]> {
    Some(match c {
        '0' => ["███", "█ █", "█ █", "█ █", "███"],
        '1' => ["  █", "  █", "  █", "  █", "  █"],
        '2' => ["███", "  █", "███", "█  ", "███"],
        '3' => ["███", "  █", "███", "  █", "███"],
        '4' => ["█ █", "█ █", "███", "  █", "  █"],
        '5' => ["███", "█  ", "███", "  █", "███"],
        '6' => ["███", "█  ", "███", "█ █", "███"],
        '7' => ["███", "  █", "  █", "  █", "  █"],
        '8' => ["███", "█ █", "███", "█ █", "███"],
        '9' => ["███", "█ █", "███", "  █", "███"],
        ':' => ["   ", " █ ", "   ", " █ ", "   "],
        _ => return None,
    })
}

/// Render an arbitrary string of digits/colons as five rows of glyphs. Unknown
/// characters are silently skipped.
pub fn render(s: &str) -> Vec<String> {
    let mut rows: Vec<String> = vec![String::new(); GLYPH_HEIGHT];
    let mut first = true;
    for ch in s.chars() {
        let Some(g) = glyph(ch) else { continue };
        for (row_idx, row) in rows.iter_mut().enumerate() {
            if !first {
                row.push(' ');
            }
            row.push_str(g[row_idx]);
        }
        first = false;
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_five_rows_of_correct_width() {
        let rows = render("12:34");
        assert_eq!(rows.len(), GLYPH_HEIGHT);
        for row in &rows {
            // 5 glyphs × 3 wide + 4 single-space separators = 19.
            assert_eq!(row.chars().count(), 5 * GLYPH_WIDTH + 4);
        }
    }

    #[test]
    fn covers_all_clock_chars() {
        for ch in "0123456789:".chars() {
            assert!(glyph(ch).is_some());
        }
    }

    #[test]
    fn unknown_chars_are_skipped() {
        let rows = render("1x2");
        assert_eq!(rows[0].chars().count(), 2 * GLYPH_WIDTH + 1);
    }
}
