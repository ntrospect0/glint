// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Minimal HTML → plain text converter for email bodies. Hand-rolled rather
//! than pulling in `scraper` / `html2text` because real-world emails are wildly
//! malformed (Outlook ships angle brackets unescaped, marketing emails
//! cram inline styles, etc.) and a tolerant state-machine is faster + more
//! predictable than a full parser.
//!
//! Coverage:
//!   - tags stripped (in-tag vs out-of-tag state)
//!   - <br>, <br/>, <p>, </p>, <div>, </div> become \n
//!   - <script> and <style> blocks (including their contents) get dropped
//!   - common entities decoded: &amp; &lt; &gt; &quot; &apos; &#39; &nbsp; &copy; …
//!   - numeric entities (decimal &#N; and hex &#xNN;) decoded
//!   - runs of whitespace collapsed to a single space; multiple blank lines
//!     collapsed to at most one

/// Convert an HTML string into plain text suitable for terminal display.
/// Best-effort: malformed input doesn't error, it just produces a best
/// approximation.
pub fn html_to_text(html: &str) -> String {
    // Step 1: drop <script>…</script> and <style>…</style> wholesale, so
    // their bodies don't leak into the output. Case-insensitive match.
    let stripped_scripts = strip_block(html, "script");
    let stripped = strip_block(&stripped_scripts, "style");

    // Step 2: walk the resulting bytes, tracking whether we're inside a tag.
    // When we hit a line-break-worthy tag, push `\n`. Everything else outside
    // a tag goes through verbatim (decoded for entities). Whitespace is
    // normalized at the end so quote-printed line wraps don't survive.
    let bytes = stripped.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0usize;
    let mut in_tag = false;
    let mut tag_buf = String::new();
    while i < bytes.len() {
        let b = bytes[i];
        if !in_tag {
            if b == b'<' {
                in_tag = true;
                tag_buf.clear();
                i += 1;
                continue;
            }
            if b == b'&' {
                // Find the next ';' within a short window; longer than 8 and
                // it's not really an entity. Pass through as-is on no match.
                let end = stripped[i + 1..]
                    .find([';', ' ', '<', '&'])
                    .map(|n| i + 1 + n);
                if let Some(end_idx) = end {
                    if bytes.get(end_idx) == Some(&b';') {
                        let entity = &stripped[i + 1..end_idx];
                        out.push_str(&decode_entity(entity));
                        i = end_idx + 1;
                        continue;
                    }
                }
                out.push('&');
                i += 1;
                continue;
            }
            // HTML treats source newlines and tabs as inline whitespace, so
            // collapse them to a space here. The only \n's that survive into
            // `out` are the ones the tag handler injects below.
            if b == b'\n' || b == b'\r' || b == b'\t' {
                out.push(' ');
            } else {
                out.push(b as char);
            }
            i += 1;
        } else {
            if b == b'>' {
                in_tag = false;
                let lname = tag_name(&tag_buf);
                if matches!(lname.as_str(), "br" | "/p" | "/div" | "p" | "div" | "tr" | "/tr" | "li" | "/li") {
                    out.push('\n');
                }
                tag_buf.clear();
                i += 1;
                continue;
            }
            // Track tag content so we can recognize the name + slash.
            tag_buf.push(b as char);
            i += 1;
        }
    }

    collapse_whitespace(&out)
}

/// Lowercased tag name (e.g. " BR / " → "br"; "/P" → "/p"). Strips the
/// trailing self-closing slash so `<br/>` and `<br>` are equivalent.
fn tag_name(raw: &str) -> String {
    let trimmed = raw.trim();
    let head = trimmed.split_whitespace().next().unwrap_or("");
    let head = head.trim_end_matches('/');
    head.to_ascii_lowercase()
}

/// Drop every occurrence of `<name>…</name>` (case-insensitive) including the
/// surrounding tags. Used to nuke `<script>` and `<style>` blocks before
/// the main tag-strip walk so their contents don't leak into the output.
fn strip_block(html: &str, name: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let open = format!("<{name}");
    let close = format!("</{name}");
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;
    while cursor < html.len() {
        match lower[cursor..].find(&open) {
            None => {
                out.push_str(&html[cursor..]);
                break;
            }
            Some(rel) => {
                let abs_open = cursor + rel;
                out.push_str(&html[cursor..abs_open]);
                // Find the closing `>` of the opening tag, then the matching
                // `</name…>`. If either is missing, bail and emit verbatim.
                let after_open = match html[abs_open..].find('>') {
                    Some(p) => abs_open + p + 1,
                    None => {
                        out.push_str(&html[abs_open..]);
                        break;
                    }
                };
                match lower[after_open..].find(&close) {
                    None => {
                        // Unclosed — drop the rest entirely to avoid leaking
                        // raw <script> content.
                        break;
                    }
                    Some(c) => {
                        let close_open = after_open + c;
                        match html[close_open..].find('>') {
                            Some(p) => {
                                cursor = close_open + p + 1;
                            }
                            None => break,
                        }
                    }
                }
            }
        }
    }
    out
}

fn decode_entity(name: &str) -> String {
    // Numeric: &#N; (decimal) or &#xNN; (hex).
    if let Some(rest) = name.strip_prefix('#') {
        let (radix, digits) = if let Some(hex) = rest.strip_prefix('x').or_else(|| rest.strip_prefix('X')) {
            (16, hex)
        } else {
            (10, rest)
        };
        if let Ok(code) = u32::from_str_radix(digits, radix) {
            if let Some(c) = char::from_u32(code) {
                return c.to_string();
            }
        }
        return String::new();
    }
    match name {
        "amp" => "&".into(),
        "lt" => "<".into(),
        "gt" => ">".into(),
        "quot" => "\"".into(),
        "apos" => "'".into(),
        "nbsp" => " ".into(),
        "copy" => "©".into(),
        "reg" => "®".into(),
        "trade" => "™".into(),
        "hellip" => "…".into(),
        "mdash" => "—".into(),
        "ndash" => "–".into(),
        "rsquo" | "lsquo" => "'".into(),
        "rdquo" | "ldquo" => "\"".into(),
        _ => format!("&{name};"),
    }
}

/// Collapse runs of inline whitespace to a single space, and runs of blank
/// lines to at most one blank line. Preserves explicit \n's because the
/// tag-strip pass emits them at paragraph boundaries.
fn collapse_whitespace(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut blank_run = 0u32;
    for line in input.lines() {
        let trimmed = line
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if trimmed.is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out.push('\n');
            }
        } else {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&trimmed);
            blank_run = 0;
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_simple_tags() {
        let html = "<p>hello <b>world</b></p>";
        let plain = html_to_text(html);
        assert_eq!(plain, "hello world");
    }

    #[test]
    fn decodes_named_entities() {
        let html = "Tom &amp; Jerry &lt;3 &quot;quoted&quot; &nbsp;space";
        let plain = html_to_text(html);
        assert_eq!(plain, "Tom & Jerry <3 \"quoted\" space");
    }

    #[test]
    fn decodes_numeric_entities() {
        // &#39; = apostrophe, &#xA9; = ©, &#8212; = em-dash
        let html = "it&#39;s &#xA9; 2026 &#8212; ok";
        let plain = html_to_text(html);
        assert_eq!(plain, "it's © 2026 — ok");
    }

    #[test]
    fn br_and_p_become_newlines() {
        let html = "line one<br>line two<br/>line three</p>line four";
        let plain = html_to_text(html);
        // <br> and <br/> both insert newlines; </p> too.
        let lines: Vec<&str> = plain.lines().collect();
        assert_eq!(lines, vec!["line one", "line two", "line three", "line four"]);
    }

    #[test]
    fn drops_script_and_style_blocks() {
        let html = "before<script>alert('x');</script>after<style>body{color:red}</style>tail";
        let plain = html_to_text(html);
        // Critical: the alert() text + CSS body must not leak through.
        assert!(!plain.contains("alert"));
        assert!(!plain.contains("color:red"));
        assert!(plain.contains("before"));
        assert!(plain.contains("after"));
        assert!(plain.contains("tail"));
    }

    #[test]
    fn collapses_excessive_whitespace() {
        let html = "<p>hello   \n\n  world</p>\n\n\n<p>next</p>";
        let plain = html_to_text(html);
        assert_eq!(plain, "hello world\nnext");
    }

    #[test]
    fn handles_unclosed_tags_gracefully() {
        // Malformed input shouldn't panic — just produce best-effort output.
        let html = "<p>unfinished";
        let plain = html_to_text(html);
        assert_eq!(plain, "unfinished");
    }

    #[test]
    fn passes_through_loose_ampersand() {
        // No semicolon → not an entity; render verbatim.
        let html = "A & B (a & b)";
        let plain = html_to_text(html);
        assert_eq!(plain, "A & B (a & b)");
    }
}
