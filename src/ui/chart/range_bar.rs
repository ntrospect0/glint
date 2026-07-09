// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! 52-week range bar renderer shared by widgets that display a price
//! relative to its annual high/low window. Provider-agnostic: all values
//! must be caller-scaled to the desired display unit before calling.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// Build a compact 52-week range bar for the given `total_width`, returned
/// as a styled [`Line`].
///
/// Format: `52w  <low> ├──────●──────────────┤ <high>`
///
/// Rendered as four spans:
/// - `"52w "` — with `label_style`
/// - the low value — with `value_style`
/// - `" ├──●──┤ "` bar run including the marker — with `bar_style`
/// - the high value — with `value_style`
///
/// `●` sits at the column proportional to where `current` falls between
/// `low` and `high`. All three values must already be scaled to the
/// desired display unit by the caller (e.g. forex multiplies by
/// `primary_unit` before passing). Geometry and characters are identical
/// to the previous `String`-returning version; only styling and return
/// type differ.
pub(crate) fn range_bar_line(
    total_width: usize,
    low: f64,
    high: f64,
    current: f64,
    label_style: Style,
    value_style: Style,
    bar_style: Style,
) -> Line<'static> {
    let low_s = format!("{:.4}", low);
    let high_s = format!("{:.4}", high);
    // overhead = "52w " (4) + low_s + " ├" (2) + "┤ " (2) + high_s
    let overhead = 4 + low_s.chars().count() + 4 + high_s.chars().count();
    if total_width <= overhead + 1 {
        // Narrower than the fixed frame — just emit the label.
        return Line::from(Span::styled(
            format!("{:<width$}", "52w —", width = total_width),
            label_style,
        ));
    }
    let bar_w = total_width - overhead;
    let frac = if high > low {
        ((current - low) / (high - low)).clamp(0.0, 1.0)
    } else {
        0.5
    };
    let marker_pos =
        ((frac * bar_w.saturating_sub(1) as f64).round() as usize).min(bar_w.saturating_sub(1));
    let bar_interior: String = (0..bar_w)
        .map(|i| if i == marker_pos { '●' } else { '─' })
        .collect();
    Line::from(vec![
        Span::styled("52w ".to_string(), label_style),
        Span::styled(low_s, value_style),
        Span::styled(format!(" ├{}┤ ", bar_interior), bar_style),
        Span::styled(high_s, value_style),
    ])
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;

    use super::*;

    #[test]
    fn range_bar_spans_carry_correct_styles() {
        let label_style = Style::default().fg(Color::DarkGray);
        let value_style = Style::default().fg(Color::White);
        let bar_style = Style::default().fg(Color::LightCyan);

        let line = range_bar_line(50, 0.8800, 1.0500, 0.9237, label_style, value_style, bar_style);

        let spans = &line.spans;
        assert_eq!(spans.len(), 4, "expected 4 spans: label, low, bar, high");
        assert!(spans[0].content.starts_with("52w"), "span 0 must be the label");
        assert_eq!(spans[0].style, label_style, "label span must carry label_style");
        assert!(spans[1].content.contains("0.8800"), "span 1 must hold the low value");
        assert_eq!(spans[1].style, value_style, "low value span must carry value_style");
        assert!(
            spans[2].content.contains('●'),
            "span 2 (bar) must contain the marker ●"
        );
        assert_eq!(spans[2].style, bar_style, "bar span must carry bar_style");
        assert!(spans[3].content.contains("1.0500"), "span 3 must hold the high value");
        assert_eq!(spans[3].style, value_style, "high value span must carry value_style");
    }

    #[test]
    fn range_bar_total_char_count_matches_total_width() {
        let s = Style::default();
        let line = range_bar_line(40, 0.8800, 1.0500, 0.9237, s, s, s);
        let total: usize = line.spans.iter().map(|sp| sp.content.chars().count()).sum();
        assert_eq!(total, 40, "span chars must sum to total_width");
    }

    #[test]
    fn range_bar_narrow_fallback_emits_label_span_only() {
        let label_style = Style::default().fg(Color::DarkGray);
        let s = Style::default();
        // total_width = 5 is too narrow for the bar frame.
        let line = range_bar_line(5, 0.88, 1.05, 0.92, label_style, s, s);
        assert_eq!(line.spans.len(), 1, "narrow fallback must be a single span");
        assert_eq!(line.spans[0].style, label_style);
        assert!(line.spans[0].content.starts_with("52w"));
    }

    #[test]
    fn range_bar_marker_at_low_end() {
        let s = Style::default();
        let line = range_bar_line(40, 0.88, 1.05, 0.88, s, s, s);
        let bar_span = &line.spans[2];
        // Marker at position 0 of the interior → first char after '├' is '●'.
        let interior: Vec<char> = bar_span.content.chars().collect();
        // interior[0] = ' ', interior[1] = '├', interior[2] = first bar char
        assert_eq!(interior[2], '●', "marker must be at the left end when current == low");
    }
}
