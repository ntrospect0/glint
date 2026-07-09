// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0
//
// Shared text utilities for widgets.

//! Plain-text helpers used by multiple widgets: Unicode-width-aware
//! truncation + padding, word-wrap with a per-call max-line cap, and a
//! tolerant HTML sanitiser for RSS/Atom `<description>` blobs.
//!
//! These were previously duplicated across `email`, `news`, `feeds`, and
//! `resources` with subtle differences (some used `chars().count()`
//! instead of Unicode display width — wrong for CJK and emoji — and
//! the wrap implementations disagreed on paragraph handling and on
//! how to ellipsise an overflowed final line). Consolidating fixes
//! the latent correctness bugs and gives every future widget one
//! canonical implementation to reach for.
//!
//! See `docs/widget-sdk.md` § Text utilities.

#![allow(dead_code)] // some exports are SDK surface for future widgets.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Truncate `s` so its terminal display width fits inside `max`
/// cells, appending `…` (1 cell) when truncation happens. Honest
/// about Unicode: a wide char (2 cells) that would land at the
/// last position gets dropped so the result is exactly `max` cells.
pub fn truncate(s: &str, max: usize) -> String {
    if UnicodeWidthStr::width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let budget = max.saturating_sub(1);
    let mut used: usize = 0;
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// Pad `s` to exactly `width` terminal cells, truncating if it
/// already exceeds the budget. Padding is ASCII spaces (1 cell
/// each) so cell-width and char-count agree on the appended slice.
pub fn pad_or_truncate(s: &str, width: usize) -> String {
    let s = truncate(s, width);
    let visible = UnicodeWidthStr::width(s.as_str());
    if visible < width {
        format!("{s}{}", " ".repeat(width - visible))
    } else {
        s
    }
}

/// Word-wrap `text` to lines of at most `max_width` *cells* each,
/// producing at most `max_lines` lines. If `preserve_paragraphs` is
/// true, `\n` characters in `text` are treated as paragraph
/// boundaries — each paragraph wraps independently and the lines
/// are concatenated; otherwise the entire `text` is treated as one
/// flow.
///
/// Single words longer than `max_width` get split mid-word: the
/// first `max_width` chars become their own line, the remainder
/// continues on the next line.
///
/// When the input would need more than `max_lines` lines to render
/// fully, the final emitted line is truncated and ends with `…`.
pub fn wrap(
    text: &str,
    max_width: usize,
    max_lines: usize,
    preserve_paragraphs: bool,
) -> Vec<String> {
    if max_width == 0 || max_lines == 0 {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::with_capacity(max_lines.min(8));
    let paragraphs: Vec<&str> = if preserve_paragraphs {
        text.lines().collect()
    } else {
        vec![text]
    };
    let mut consumed_all = true;
    'outer: for paragraph in paragraphs {
        let words: Vec<&str> = paragraph.split_whitespace().collect();
        if words.is_empty() {
            continue;
        }
        let mut current = String::new();
        let mut idx = 0;
        while idx < words.len() {
            let word = words[idx];
            let word_w = UnicodeWidthStr::width(word);
            if word_w > max_width {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                    if out.len() >= max_lines {
                        consumed_all = idx == words.len();
                        break 'outer;
                    }
                }
                // Mid-break the oversized word: split into
                // max_width-sized chunks until the whole word is
                // emitted or the row budget runs out. Without this
                // tail-continuation the wrap would silently drop
                // everything past the first chunk, which is exactly
                // the kind of latent bug consolidating these helpers
                // is meant to extinguish.
                let mut remaining = word;
                while !remaining.is_empty() {
                    let (chunk, rest) = take_cells_split(remaining, max_width);
                    out.push(chunk);
                    remaining = rest;
                    if out.len() >= max_lines {
                        consumed_all = remaining.is_empty() && idx + 1 == words.len();
                        break 'outer;
                    }
                }
                idx += 1;
                continue;
            }
            let prospective = if current.is_empty() {
                word_w
            } else {
                UnicodeWidthStr::width(current.as_str()) + 1 + word_w
            };
            if prospective > max_width {
                out.push(std::mem::take(&mut current));
                if out.len() >= max_lines {
                    consumed_all = idx == words.len();
                    break 'outer;
                }
                current = word.to_string();
                idx += 1;
            } else {
                if !current.is_empty() {
                    current.push(' ');
                }
                current.push_str(word);
                idx += 1;
            }
        }
        if !current.is_empty() {
            if out.len() < max_lines {
                out.push(current);
            } else {
                consumed_all = false;
                break;
            }
        }
    }
    if !consumed_all {
        if let Some(last) = out.last_mut() {
            while UnicodeWidthStr::width(last.as_str()) + 1 > max_width && !last.is_empty() {
                last.pop();
            }
            while last.ends_with(' ') {
                last.pop();
            }
            last.push('…');
        }
    }
    out
}

/// Split `s` at the cell-width boundary `cells`: returns
/// `(taken, rest)` where `taken` is exactly the prefix that fits in
/// `cells` columns (wide chars that would overflow are dropped) and
/// `rest` is the remaining slice for the caller to continue with.
fn take_cells_split(s: &str, cells: usize) -> (String, &str) {
    let mut taken = String::with_capacity(s.len());
    let mut used: usize = 0;
    let mut byte_idx = 0usize;
    for (i, ch) in s.char_indices() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > cells {
            byte_idx = i;
            return (taken, &s[byte_idx..]);
        }
        taken.push(ch);
        used += w;
        byte_idx = i + ch.len_utf8();
    }
    (taken, &s[byte_idx..])
}

/// Strip rudimentary HTML tags and decode the common named +
/// numeric entities so an RSS / Atom `<description>` blob renders
/// readably. Tolerant — unknown entities are passed through
/// verbatim rather than dropped, and runs of whitespace are
/// collapsed to a single space.
///
/// Not a full HTML parser. For real document parsing (article
/// bodies, etc.) widgets should use `html5ever` directly. This is
/// for short summary snippets where correctness on edge cases
/// matters less than robustness against unknown markup.
pub fn sanitize_html(raw: &str) -> String {
    decode_entities(&strip_tags(raw))
}

/// Wrap `s` in TOML basic-string double-quotes with correct escaping:
/// `\` → `\\`, `"` → `\"`, and C0 control characters (U+0000–U+001F)
/// as `\uXXXX` escape sequences. Suitable for writing TOML values that
/// must survive a round-trip through the parser (timezone names, feed
/// labels, OAuth fields, etc.).
pub fn toml_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn strip_tags(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_tag = false;
    let mut prev_was_space = false;
    for ch in raw.chars() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
                if !prev_was_space {
                    out.push(' ');
                    prev_was_space = true;
                }
            }
            continue;
        }
        if ch == '<' {
            in_tag = true;
            continue;
        }
        if ch.is_whitespace() {
            if !prev_was_space {
                out.push(' ');
                prev_was_space = true;
            }
        } else {
            out.push(ch);
            prev_was_space = false;
        }
    }
    out.trim().to_string()
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '&' {
            out.push(c);
            continue;
        }
        let mut buf = String::new();
        let mut closed = false;
        for _ in 0..10 {
            match chars.peek() {
                Some(';') => {
                    chars.next();
                    closed = true;
                    break;
                }
                Some(&nc) if nc.is_ascii_alphanumeric() || nc == '#' => {
                    buf.push(nc);
                    chars.next();
                }
                _ => break,
            }
        }
        if !closed {
            out.push('&');
            out.push_str(&buf);
            continue;
        }
        match lookup_entity(&buf) {
            Some(ch) => out.push(ch),
            None => {
                out.push('&');
                out.push_str(&buf);
                out.push(';');
            }
        }
    }
    out
}

fn lookup_entity(entity: &str) -> Option<char> {
    if let Some(rest) = entity.strip_prefix('#') {
        let (radix, digits) = if let Some(hex) = rest.strip_prefix(['x', 'X']) {
            (16, hex)
        } else {
            (10, rest)
        };
        let n = u32::from_str_radix(digits, radix).ok()?;
        return char::from_u32(n);
    }
    Some(match entity {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "hellip" => '…',
        "mdash" => '—',
        "ndash" => '–',
        "lsquo" => '\u{2018}',
        "rsquo" => '\u{2019}',
        "ldquo" => '\u{201C}',
        "rdquo" => '\u{201D}',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── truncate ───────────────────────────────────────────────

    #[test]
    fn truncate_short_passes_through() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_at_boundary_adds_ellipsis() {
        let out = truncate("hello world", 8);
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 8);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_respects_unicode_width() {
        // 4 wide characters = 8 cells. truncate to 5 cells → 4 cells of
        // content + 1 cell ellipsis = 5 cells total.
        let out = truncate("漢字漢字", 5);
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 5);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_zero_budget_returns_empty() {
        assert_eq!(truncate("hello", 0), "");
    }

    // ── pad_or_truncate ────────────────────────────────────────

    #[test]
    fn pad_or_truncate_pads_short_strings() {
        let out = pad_or_truncate("hi", 5);
        assert_eq!(out, "hi   ");
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 5);
    }

    #[test]
    fn pad_or_truncate_truncates_long_strings() {
        let out = pad_or_truncate("hello world", 5);
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 5);
        assert!(out.ends_with('…'));
    }

    // ── wrap ───────────────────────────────────────────────────

    #[test]
    fn wrap_short_returns_single_line() {
        assert_eq!(wrap("hello", 20, 3, false), vec!["hello"]);
    }

    #[test]
    fn wrap_word_boundary() {
        let out = wrap("the quick brown fox jumps", 10, 3, false);
        for line in &out {
            assert!(UnicodeWidthStr::width(line.as_str()) <= 10);
        }
    }

    #[test]
    fn wrap_caps_at_max_lines_with_ellipsis() {
        let out = wrap(
            "one two three four five six seven eight nine ten eleven",
            8,
            3,
            false,
        );
        assert_eq!(out.len(), 3);
        assert!(out[2].ends_with('…'));
    }

    #[test]
    fn wrap_handles_too_long_word_by_breaking() {
        let out = wrap("verylongunbreakableword", 5, 2, false);
        assert_eq!(UnicodeWidthStr::width(out[0].as_str()), 5);
    }

    #[test]
    fn wrap_preserves_paragraphs_when_requested() {
        // Two paragraphs separated by `\n`. With preserve_paragraphs,
        // each wraps independently → first paragraph's last line
        // shouldn't merge with the second's first word.
        let out = wrap("hello world\nsecond line", 20, 5, true);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], "hello world");
        assert_eq!(out[1], "second line");
    }

    #[test]
    fn wrap_without_preserve_paragraphs_treats_newlines_as_whitespace() {
        let out = wrap("hello world\nsecond line", 50, 3, false);
        // With preserve_paragraphs=false, the whole input is one
        // flow → all words on one line.
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn wrap_empty_input_yields_empty() {
        assert!(wrap("", 10, 3, false).is_empty());
        assert!(wrap("   ", 10, 3, false).is_empty());
    }

    #[test]
    fn wrap_zero_budget_yields_empty() {
        assert!(wrap("hello", 0, 3, false).is_empty());
        assert!(wrap("hello", 10, 0, false).is_empty());
    }

    // ── sanitize_html ──────────────────────────────────────────

    #[test]
    fn sanitize_html_strips_simple_tags() {
        let out = sanitize_html("<p>Hello <b>world</b></p>");
        assert_eq!(out, "Hello world");
    }

    #[test]
    fn sanitize_html_decodes_named_entities() {
        let out = sanitize_html("Apple &amp; orange &mdash; sweet&nbsp;fruit");
        assert_eq!(out, "Apple & orange — sweet fruit");
    }

    #[test]
    fn sanitize_html_decodes_numeric_entities() {
        let out = sanitize_html("hello&#8217;world &#x2014; test");
        assert_eq!(out, "hello\u{2019}world — test");
    }

    #[test]
    fn sanitize_html_passes_unknown_entities_verbatim() {
        // Unknown entity (no such thing as `&zzz;`) stays in place
        // so we don't accidentally garble text that wasn't HTML.
        let out = sanitize_html("a &zzz; b");
        assert!(out.contains("&zzz;"));
    }

    #[test]
    fn sanitize_html_collapses_whitespace() {
        let out = sanitize_html("<p>line one</p>\n\n<p>line  two</p>");
        assert_eq!(out, "line one line two");
    }

    // ── toml_quote ─────────────────────────────────────────────

    #[test]
    fn toml_quote_plain_string() {
        assert_eq!(toml_quote("hello"), "\"hello\"");
    }

    #[test]
    fn toml_quote_escapes_double_quote() {
        assert_eq!(toml_quote(r#"say "hi""#), r#""say \"hi\"""#);
    }

    #[test]
    fn toml_quote_escapes_backslash() {
        assert_eq!(toml_quote(r"C:\Users"), r#""C:\\Users""#);
    }

    #[test]
    fn toml_quote_escapes_newline_as_unicode() {
        assert_eq!(toml_quote("line1\nline2"), "\"line1\\u000aline2\"");
    }

    #[test]
    fn toml_quote_escapes_tab_as_unicode() {
        assert_eq!(toml_quote("a\tb"), "\"a\\u0009b\"");
    }

    #[test]
    fn toml_quote_escapes_cr_as_unicode() {
        assert_eq!(toml_quote("a\rb"), "\"a\\u000db\"");
    }

    #[test]
    fn toml_quote_empty_string() {
        assert_eq!(toml_quote(""), "\"\"");
    }

    #[test]
    fn toml_quote_unicode_passthrough() {
        assert_eq!(toml_quote("München"), "\"München\"");
    }
}
