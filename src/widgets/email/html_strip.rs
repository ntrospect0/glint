// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! HTML → plain text for email bodies, backed by the `html2text` renderer
//! (the html5ever tree builder). Runs entirely in-process — no network.
//!
//! html2text tolerates the wildly malformed HTML real emails ship (unescaped
//! angle brackets, unclosed tags, MSO conditional comments, inline styles) and
//! handles what the previous hand-rolled stripper missed:
//!   - comments (`<!-- … -->`, incl. `<!--[if mso]> … <![endif]-->`) dropped
//!     wholesale, so their embedded tables / CSS can't leak into the body
//!   - `<script>` / `<style>` blocks dropped
//!   - the full HTML entity set decoded (not just a hand-picked dozen)
//!   - lists, links, blockquotes, and headings given readable structure
//!
//! We render at a wide width so paragraphs stay on one logical line; the read
//! pane re-wraps them to its own width. A post-pass strips zero-width / control
//! junk (invisible preheader spacers) and caps runs of blank lines.

/// Render width handed to html2text. Deliberately wide so it keeps each
/// paragraph on a single logical line (the read pane wraps to its own width);
/// bounded so pathological input can't blow up column math.
const RENDER_WIDTH: usize = 10_000;

/// Convert an HTML string into plain text suitable for terminal display.
/// Best-effort: malformed input doesn't error — on the rare render failure we
/// fall back to the raw string so the user still sees *something*.
pub fn html_to_text(html: &str) -> String {
    let rendered = html2text::config::plain()
        .no_table_borders() // layout tables shouldn't draw ASCII borders
        .string_from_read(html.as_bytes(), RENDER_WIDTH)
        .unwrap_or_else(|_| html.to_string());
    normalize(&rendered)
}

/// Strip invisible junk (zero-width spaces, soft hyphens, BOMs, stray control
/// chars) that newsletters stuff into preheaders, and collapse runs of blank
/// lines to at most one.
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank_run = 0u32;
    for line in s.lines() {
        let cleaned: String = line
            .chars()
            .filter(|c| {
                !matches!(
                    *c,
                    '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}' | '\u{00AD}'
                ) && (*c == '\t' || !c.is_control())
            })
            .collect();
        let trimmed = cleaned.trim_end();
        if trimmed.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out.push('\n');
            }
        } else {
            out.push_str(trimmed);
            out.push('\n');
            blank_run = 0;
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tags_to_visible_text() {
        assert_eq!(html_to_text("<p>hello <b>world</b></p>"), "hello world");
    }

    #[test]
    fn decodes_the_full_entity_set() {
        let plain = html_to_text(
            "Tom &amp; Jerry &lt;3 it&#39;s &#xA9; 2026 &#8212; 5&deg; &bull; &euro;10 &rarr; &frac12;",
        );
        assert!(plain.contains("Tom & Jerry"));
        assert!(plain.contains("<3") && plain.contains("it's"));
        // Entities the old hand-picked table rendered literally as `&deg;` etc.
        for want in ['©', '—', '°', '•', '€', '→', '½'] {
            assert!(plain.contains(want), "missing {want:?} in {plain:?}");
        }
    }

    #[test]
    fn drops_script_and_style_blocks() {
        let plain = html_to_text(
            "before<script>alert('x');</script>after<style>body{color:red}</style>tail",
        );
        assert_eq!(plain, "beforeaftertail");
    }

    #[test]
    fn drops_comments_including_mso_conditionals() {
        // The old stripper bailed at the first `>` inside the comment and leaked
        // the embedded table + CSS; html2text drops the whole comment.
        let html = "start<!--[if gte mso 9]><table><tr><td>OUTLOOKJUNK</td></tr></table>\
                    <style>.x{color:red}</style><![endif]-->end";
        let plain = html_to_text(html);
        assert!(!plain.contains("OUTLOOKJUNK"), "MSO comment leaked: {plain:?}");
        assert!(!plain.contains("color:red"));
        assert!(plain.contains("start") && plain.contains("end"));
    }

    #[test]
    fn br_and_block_tags_break_lines() {
        let plain = html_to_text("line one<br>line two<br/>line three</p><p>line four");
        let lines: Vec<&str> = plain.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines, vec!["line one", "line two", "line three", "line four"]);
    }

    #[test]
    fn formats_lists_and_links() {
        let plain = html_to_text(
            "<ul><li>first</li><li>second</li></ul><a href=\"https://example.com\">a link</a>",
        );
        assert!(plain.contains("first") && plain.contains("second"));
        assert!(plain.contains("a link"));
        // The link's target is surfaced (footnote-style) rather than dropped.
        assert!(plain.contains("https://example.com"));
    }

    #[test]
    fn strips_zero_width_preheader_junk() {
        let plain = html_to_text("Real text\u{200B}\u{200C}\u{FEFF}\u{00AD} here");
        assert_eq!(plain, "Real text here");
        assert!(!plain
            .chars()
            .any(|c| matches!(c, '\u{200B}' | '\u{200C}' | '\u{FEFF}' | '\u{00AD}')));
    }

    #[test]
    fn malformed_input_does_not_panic() {
        assert_eq!(html_to_text("<p>unfinished"), "unfinished");
        // Garbage must not panic.
        let _ = html_to_text("<<<>&&;<table><tr><td>&#xZZ;");
    }
}
