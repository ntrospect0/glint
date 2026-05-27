use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use serde::Deserialize;

/// 3-wide × 5-tall block-character font used by the clock and calendar widgets
/// to render numeric values (digits 0-9, `:`) at a glance.
pub const GLYPH_HEIGHT: usize = 5;
#[cfg(test)]
const GLYPH_WIDTH: usize = 3;

/// Visual style applied to big-digit output. `Normal` paints `█` with the
/// caller-supplied style. The other variants substitute each `█` with a `▀`
/// half-block whose fg = top half color and bg = bottom half color, giving
/// 10 color stops across the 5 glyph rows for a smooth top-to-bottom
/// gradient. Gradient endpoints are derived from the scheme's accent color
/// at render time — the variant chooses the *shape* of the ramp
/// (brightness fade, hue rotation, near-white top, neutral fade) but the
/// color comes from the active color scheme. Change scheme → big digits
/// follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gradient {
    #[default]
    Normal,
    /// Brightness fade within the accent hue: lighter top, darker bottom.
    Subtle,
    /// Brightness + hue rotation: accent on top, rotated ~30° on the bottom
    /// (cyan→blue, yellow→orange, etc.).
    HueShift,
    /// Near-white top tinted with the accent, accent on the bottom.
    Glow,
    /// Accent on top, faded toward neutral dark gray on the bottom.
    Fade,
}

impl Gradient {
    pub const ALL: [Gradient; 5] = [
        Gradient::Normal,
        Gradient::Subtle,
        Gradient::HueShift,
        Gradient::Glow,
        Gradient::Fade,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Gradient::Normal => "normal",
            Gradient::Subtle => "subtle",
            Gradient::HueShift => "hue-shift",
            Gradient::Glow => "glow",
            Gradient::Fade => "fade",
        }
    }

    pub fn next(self) -> Gradient {
        let i = Self::ALL.iter().position(|g| *g == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }
}

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

/// Render `s` as styled lines for inclusion in a Ratatui `Paragraph`.
///
/// `Normal` mode paints `█` with `style` directly so the terminal bg shows
/// through. Pass the scheme role you want the digits to follow
/// (`text.focused` for the clock, `text.selected` for the calendar today
/// numeral, etc.) and the digits will restyle when the user runs
/// `:scheme`.
///
/// Gradient modes use `style.fg` as the seed for a scheme-derived palette:
/// the variant picks the *shape* of the ramp and the seed color picks the
/// hue. `style.fg = None` falls back to a neutral off-white so the digits
/// don't disappear on schemes that omit `text.focused`.
pub fn render_styled(s: &str, gradient: Gradient, style: Style) -> Vec<Line<'static>> {
    let rows = render(s);

    if matches!(gradient, Gradient::Normal) {
        return rows
            .into_iter()
            .map(|row| Line::from(Span::styled(row, style)))
            .collect();
    }

    let base = color_to_rgb(style.fg.unwrap_or(Color::Gray));
    let stops = palette(gradient, base);
    let mut lines = Vec::with_capacity(rows.len());
    for (r, row) in rows.iter().enumerate() {
        let top = stops[r * 2];
        let bot = stops[r * 2 + 1];
        let half_block_style = Style::default()
            .fg(top)
            .bg(bot)
            .add_modifier(Modifier::BOLD);
        // Each cell is rendered as its own Span so we don't bleed the
        // half-block bg color into adjacent empty cells. Empty cells stay
        // unstyled so the terminal background shows through normally.
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(row.chars().count());
        for ch in row.chars() {
            if ch == '█' {
                spans.push(Span::styled("▀".to_string(), half_block_style));
            } else {
                spans.push(Span::raw(" ".to_string()));
            }
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// Build a 10-stop gradient palette from a single accent color. Each
/// variant chooses the *shape* of the ramp; the input color picks the
/// hue. Endpoints aim to roughly match the visual feel of the old
/// hardcoded palettes when seeded with cyan/yellow/magenta.
fn palette(gradient: Gradient, base: (u8, u8, u8)) -> [Color; 10] {
    let (top, bot) = match gradient {
        Gradient::Normal => unreachable!("Normal handled before palette()"),
        // Lighter top, darker bottom — same hue family.
        Gradient::Subtle => (lighten(base, 0.30), darken(base, 0.45)),
        // Bright top, hue-rotated darker bottom. Matches the original feel:
        // cyan→blue, yellow→orange, magenta→purple.
        Gradient::HueShift => (
            lighten(base, 0.25),
            darken(shift_hue(base, -30.0), 0.30),
        ),
        // Near-white tinted top, accent below.
        Gradient::Glow => (lighten(base, 0.70), base),
        // Accent top, neutral dark gray bottom.
        Gradient::Fade => (base, (80, 80, 88)),
    };
    let mut out = [Color::Reset; 10];
    for (i, slot) in out.iter_mut().enumerate() {
        let t = i as f32 / 9.0;
        *slot = lerp_rgb_color(top, bot, t);
    }
    out
}

/// Best-effort ANSI/Rgb → (u8, u8, u8). ANSI colors map to terminal-ish RGB
/// approximations chosen to be close to what the old hardcoded palettes
/// assumed (e.g. LightCyan → 160/240/255 to match the previous Subtle top).
/// `Color::Reset` and palette-indexed colors fall through to a neutral gray
/// so the gradient still renders something legible.
fn color_to_rgb(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Black => (0, 0, 0),
        Color::Red => (200, 40, 40),
        Color::Green => (40, 200, 40),
        Color::Yellow => (220, 200, 40),
        Color::Blue => (40, 90, 220),
        Color::Magenta => (220, 40, 220),
        Color::Cyan => (40, 220, 220),
        Color::White => (220, 220, 220),
        Color::Gray => (180, 180, 180),
        Color::DarkGray => (90, 90, 90),
        Color::LightRed => (255, 90, 90),
        Color::LightGreen => (90, 255, 90),
        Color::LightYellow => (255, 250, 180),
        Color::LightBlue => (90, 150, 255),
        Color::LightMagenta => (255, 150, 255),
        Color::LightCyan => (160, 240, 255),
        // Color::Reset, Color::Indexed(_), and any future variants land
        // here. Neutral gray keeps gradients legible.
        _ => (200, 200, 200),
    }
}

fn lighten(rgb: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    lerp_rgb(rgb, (255, 255, 255), t.clamp(0.0, 1.0))
}

fn darken(rgb: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    lerp_rgb(rgb, (0, 0, 0), t.clamp(0.0, 1.0))
}

fn lerp_rgb(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let mix = |x: u8, y: u8| -> u8 {
        (x as f32 + (y as f32 - x as f32) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    (mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

fn lerp_rgb_color(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> Color {
    let (r, g, b) = lerp_rgb(a, b, t);
    Color::Rgb(r, g, b)
}

/// Rotate `rgb`'s hue by `degrees` (positive = toward red→yellow→green,
/// negative = the other way around the color wheel) via HSL. Saturation and
/// lightness are preserved. Used by `HueShift` to derive a "related but
/// shifted" bottom endpoint.
fn shift_hue(rgb: (u8, u8, u8), degrees: f32) -> (u8, u8, u8) {
    let (h, s, l) = rgb_to_hsl(rgb);
    let h = (h + degrees).rem_euclid(360.0);
    hsl_to_rgb(h, s, l)
}

fn rgb_to_hsl(rgb: (u8, u8, u8)) -> (f32, f32, f32) {
    let r = rgb.0 as f32 / 255.0;
    let g = rgb.1 as f32 / 255.0;
    let b = rgb.2 as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let delta = max - min;
    if delta < 1e-6 {
        return (0.0, 0.0, l);
    }
    let s = if l < 0.5 {
        delta / (max + min)
    } else {
        delta / (2.0 - max - min)
    };
    let h = if (r - max).abs() < 1e-6 {
        ((g - b) / delta).rem_euclid(6.0)
    } else if (g - max).abs() < 1e-6 {
        (b - r) / delta + 2.0
    } else {
        (r - g) / delta + 4.0
    } * 60.0;
    let h = if h < 0.0 { h + 360.0 } else { h };
    (h, s, l)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    if s < 1e-6 {
        let v = (l * 255.0).round().clamp(0.0, 255.0) as u8;
        return (v, v, v);
    }
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let h_seg = h / 60.0;
    let x = c * (1.0 - (h_seg.rem_euclid(2.0) - 1.0).abs());
    let (r, g, b) = if h_seg < 1.0 {
        (c, x, 0.0)
    } else if h_seg < 2.0 {
        (x, c, 0.0)
    } else if h_seg < 3.0 {
        (0.0, c, x)
    } else if h_seg < 4.0 {
        (0.0, x, c)
    } else if h_seg < 5.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    let m = l - c / 2.0;
    (
        ((r + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((g + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((b + m) * 255.0).round().clamp(0.0, 255.0) as u8,
    )
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

    #[test]
    fn gradient_next_cycles_through_all_variants() {
        let mut seen = std::collections::HashSet::new();
        let mut g = Gradient::default();
        for _ in 0..Gradient::ALL.len() {
            seen.insert(g);
            g = g.next();
        }
        assert_eq!(seen.len(), Gradient::ALL.len());
        assert_eq!(g, Gradient::default(), "cycle should wrap back to start");
    }

    #[test]
    fn render_styled_normal_emits_full_blocks() {
        let lines = render_styled("1", Gradient::Normal, Style::default());
        assert_eq!(lines.len(), GLYPH_HEIGHT);
        // Each line contains the single full-block span from the existing
        // render() output — no half-blocks should appear in Normal mode.
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.clone()))
            .collect();
        assert!(joined.contains('█'));
        assert!(!joined.contains('▀'));
    }

    #[test]
    fn render_styled_gradient_uses_half_blocks() {
        let lines = render_styled(
            "8",
            Gradient::Subtle,
            Style::default().fg(Color::LightCyan),
        );
        assert_eq!(lines.len(), GLYPH_HEIGHT);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.clone()))
            .collect();
        assert!(joined.contains('▀'), "gradient render must use ▀");
        assert!(!joined.contains('█'), "gradient render must not use █");
    }

    #[test]
    fn gradient_palette_seeds_from_accent_color() {
        // Two different seeds should yield two different palettes.
        // (Regression: prior hardcoded palette ignored the seed.)
        let cyan = palette(Gradient::Subtle, (160, 240, 255));
        let yellow = palette(Gradient::Subtle, (255, 250, 180));
        assert_ne!(
            cyan, yellow,
            "different accent seeds must produce different palettes"
        );
        // And same seed → identical palettes, run to run.
        assert_eq!(cyan, palette(Gradient::Subtle, (160, 240, 255)));
    }

    #[test]
    fn shift_hue_rotates_around_color_wheel() {
        // Red (0°) shifted +120° lands on green; -120° lands on blue.
        // (Tolerance ±2 because of float-rounding through HSL.)
        let red = (255, 0, 0);
        let g = shift_hue(red, 120.0);
        assert!(g.1 > 200 && g.0 < 30 && g.2 < 30, "got {g:?}");
        let b = shift_hue(red, -120.0);
        assert!(b.2 > 200 && b.0 < 30 && b.1 < 30, "got {b:?}");
    }

    #[test]
    fn lighten_and_darken_endpoints() {
        let red = (200, 40, 40);
        let lighter = lighten(red, 1.0);
        let darker = darken(red, 1.0);
        assert_eq!(lighter, (255, 255, 255));
        assert_eq!(darker, (0, 0, 0));
        // Zero-blend is a no-op.
        assert_eq!(lighten(red, 0.0), red);
        assert_eq!(darken(red, 0.0), red);
    }
}
