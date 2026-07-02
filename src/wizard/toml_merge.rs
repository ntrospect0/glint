// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Surgical TOML edits that preserve structure the wizard doesn't
//! manage.
//!
//! When a widget's wizard descriptor only exposes a handful of
//! top-level scalars but the on-disk TOML carries additional hand-edited
//! sections (arrays of tables, color overrides, comments), a full
//! re-render at finalize would clobber the user's curation. Instead we
//! update only the scalar keys we know about and leave everything else
//! verbatim.
//!
//! Text-based rather than TOML-AST based: pulling in `toml_edit` would
//! add a heavyweight dep just to keep comments. The operations here are
//! narrow enough to handle with line scanning.

#![allow(dead_code)]

/// Replace any top-level `<key> = ...` assignment with `<key> =
/// <new_value>` in `text`, preserving everything else (comments,
/// table headers, array-of-tables blocks, surrounding whitespace).
/// Missing keys are inserted before the first table/array-of-tables
/// header so they land in the "top-level scalars" zone every TOML file
/// has at the start. Operates only on key assignments that appear
/// before the first table header — assignments inside tables aren't
/// touched.
///
/// `new_value` must already be valid TOML on its own (`"foo"`, `42`,
/// `true`, etc.). The caller is responsible for quoting strings.
pub fn merge_top_level_scalars(text: &str, updates: &[(&str, String)]) -> String {
    // Split into (head, tail): head is everything before the first
    // table or array-of-tables header line; tail is from that header
    // onward. Scalars live in head; arrays/tables live in tail.
    let (head_end, tail_start) = first_table_header_offset(text);
    let head = &text[..head_end];
    let tail = &text[tail_start..];

    let mut head_out = head.to_string();
    let mut pending_inserts: Vec<(&str, &String)> = Vec::new();
    for (key, value) in updates {
        if replace_first_assignment(&mut head_out, key, value) {
            continue;
        }
        pending_inserts.push((*key, value));
    }

    // Anything we didn't find a slot to replace gets inserted at the
    // end of the head block, before the first table header. Ensure a
    // newline boundary so the new line doesn't accidentally merge with
    // a trailing comment.
    if !pending_inserts.is_empty() {
        // Ensure a newline boundary so the new line doesn't merge with
        // a trailing comment. Skip when head is empty so we don't emit
        // a leading blank line into an otherwise empty file.
        if !head_out.is_empty() && !head_out.ends_with('\n') {
            head_out.push('\n');
        }
        for (key, value) in pending_inserts {
            head_out.push_str(&format!("{key} = {value}\n"));
        }
    }

    let mut out = head_out;
    out.push_str(tail);
    out
}

/// Return `(end_of_head, start_of_tail)` — usually identical, except
/// that `start_of_tail` is positioned at the table header itself so
/// inserts can land before it. When there's no table header at all,
/// both equal `text.len()`.
fn first_table_header_offset(text: &str) -> (usize, usize) {
    let mut search_from = 0;
    while search_from < text.len() {
        let Some(rel) = text[search_from..].find('\n') else {
            // No more newlines — the rest of `text` is one final line.
            // If it starts a table header, that's where head ends.
            let line = &text[search_from..];
            let trimmed = line.trim_start();
            if trimmed.starts_with('[') {
                return (search_from, search_from);
            }
            return (text.len(), text.len());
        };
        let line_start = search_from;
        let line_end = search_from + rel + 1; // include the '\n'
        let line = &text[line_start..line_end];
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            return (line_start, line_start);
        }
        search_from = line_end;
    }
    (text.len(), text.len())
}

/// In-place replacement of the first `<key> = ...` line in `head`.
/// Returns true if found (and replaced). False otherwise so the caller
/// can fall back to insertion.
fn replace_first_assignment(head: &mut String, key: &str, new_value: &str) -> bool {
    let bytes = head.as_str();
    let mut search_from = 0;
    while search_from < bytes.len() {
        let line_end = bytes[search_from..]
            .find('\n')
            .map(|p| search_from + p + 1)
            .unwrap_or(bytes.len());
        let line = &bytes[search_from..line_end];
        let trimmed = line.trim_start();
        if line_assigns_top_level_key(trimmed, key) {
            // Preserve indentation (rare in TOML but cheap to keep).
            let indent_len = line.len() - trimmed.len();
            let indent = &bytes[search_from..search_from + indent_len];
            let replacement = format!("{indent}{key} = {new_value}\n");
            head.replace_range(search_from..line_end, &replacement);
            return true;
        }
        search_from = line_end;
    }
    false
}

/// `true` when `trimmed` is exactly `<key> [=] ...`, distinguishing
/// `key = ...` from a longer key starting with the same prefix
/// (e.g. searching for `widget` shouldn't match `widget_title`).
fn line_assigns_top_level_key(trimmed: &str, key: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix(key) else {
        return false;
    };
    let rest = rest.trim_start();
    rest.starts_with('=')
}

/// Remove every `[[<header>]]` block from `text`, returning the
/// stripped version. Used when the caller needs to replace an entire
/// array-of-tables in place — they strip the existing blocks and
/// append fresh ones. Blocks here span from the `[[<header>]]` line
/// up to the next `[[` or `[` table header, or end-of-file.
pub fn strip_array_of_tables_blocks(text: &str, header: &str) -> String {
    let needle = format!("[[{header}]]");
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    while cursor < text.len() {
        let line_end = text[cursor..]
            .find('\n')
            .map(|p| cursor + p + 1)
            .unwrap_or(text.len());
        let line = &text[cursor..line_end];
        if line.trim_start().starts_with(&needle) {
            // Skip this block: scan forward until the next table
            // header (any `[` line) or end of file.
            cursor = line_end;
            while cursor < text.len() {
                let next_end = text[cursor..]
                    .find('\n')
                    .map(|p| cursor + p + 1)
                    .unwrap_or(text.len());
                let next_line = &text[cursor..next_end];
                if next_line.trim_start().starts_with('[') {
                    break;
                }
                cursor = next_end;
            }
            continue;
        }
        out.push_str(line);
        cursor = line_end;
    }
    // Collapse runs of blank lines created by the strip down to one
    // blank line, so the output doesn't drift apart visually each run.
    collapse_double_blanks(&out)
}

fn collapse_double_blanks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0;
    for line in text.split_inclusive('\n') {
        let is_blank = line.trim().is_empty();
        if is_blank {
            blank_run += 1;
            if blank_run <= 1 {
                out.push_str(line);
            }
        } else {
            blank_run = 0;
            out.push_str(line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_gradient_write_back_replaces_and_inserts() {
        // default profile: gradient present (with a trailing comment) plus a
        // sibling scalar and a table — value is replaced, siblings/table kept.
        let with_key =
            "timezone = \"local\"\ngradient = \"hue_shift\"  # normal | subtle\n\n[colors]\nfg = \"#fff\"\n";
        let out = merge_top_level_scalars(&with_key, &[("gradient", "\"glow\"".into())]);
        assert!(out.contains("gradient = \"glow\""));
        assert!(!out.contains("hue_shift"));
        assert!(out.contains("timezone = \"local\""));
        assert!(out.contains("[colors]"));

        // ipad profile: no gradient key — it's inserted into the top-level
        // scalar zone (before any table header).
        let without_key = "timezone = \"local\"\n\n[colors]\nfg = \"#fff\"\n";
        let out = merge_top_level_scalars(&without_key, &[("gradient", "\"glow\"".into())]);
        assert!(out.contains("gradient = \"glow\""));
        let gi = out.find("gradient").unwrap();
        let ci = out.find("[colors]").unwrap();
        assert!(gi < ci, "gradient must land before the [colors] table");
    }

    #[test]
    fn replaces_existing_scalar_in_place() {
        let text =
            "poll_interval_secs = 900\nshow_topic_labels = true\n\n[[feeds]]\nlabel = \"BBC\"\n";
        let out = merge_top_level_scalars(text, &[("poll_interval_secs", "300".into())]);
        assert!(out.contains("poll_interval_secs = 300"));
        assert!(!out.contains("poll_interval_secs = 900"));
        // Tail untouched.
        assert!(out.contains("[[feeds]]"));
        assert!(out.contains("label = \"BBC\""));
    }

    #[test]
    fn inserts_missing_scalar_before_first_table() {
        let text = "# leading comment\n\n[[feeds]]\nlabel = \"X\"\n";
        let out = merge_top_level_scalars(text, &[("summarize_with_llm", "false".into())]);
        let summarize_pos = out.find("summarize_with_llm").unwrap();
        let feeds_pos = out.find("[[feeds]]").unwrap();
        assert!(
            summarize_pos < feeds_pos,
            "new scalar should land before the first table header"
        );
        // Comment + array preserved.
        assert!(out.contains("# leading comment"));
        assert!(out.contains("label = \"X\""));
    }

    #[test]
    fn preserves_arrays_and_comments_verbatim() {
        let text = "poll_interval_secs = 900\n\n# user comment\n[[feeds]]\nlabel = \"BBC\"\nurl = \"https://example/rss\"\n\n[[topics]]\nlabel = \"Tech\"\nkeywords = [\"rust\"]\n";
        let out = merge_top_level_scalars(
            text,
            &[
                ("poll_interval_secs", "60".into()),
                ("show_topic_labels", "true".into()),
            ],
        );
        assert!(out.contains("# user comment"));
        assert!(out.contains("[[feeds]]"));
        assert!(out.contains("url = \"https://example/rss\""));
        assert!(out.contains("[[topics]]"));
        assert!(out.contains("keywords = [\"rust\"]"));
    }

    #[test]
    fn does_not_touch_assignments_inside_tables() {
        // A `label = ...` inside [[feeds]] must NOT be rewritten when
        // the caller asks to update a different scalar.
        let text = "poll_interval_secs = 900\n\n[[feeds]]\nlabel = \"BBC\"\n";
        let out = merge_top_level_scalars(text, &[("poll_interval_secs", "300".into())]);
        assert!(out.contains("label = \"BBC\""));
    }

    #[test]
    fn key_prefix_does_not_collide_with_longer_keys() {
        // Pretend a TOML had both `show = true` and `show_topic_labels = true`.
        // Replacing `show` must not touch `show_topic_labels`.
        let text = "show = true\nshow_topic_labels = true\n";
        let out = merge_top_level_scalars(text, &[("show", "false".into())]);
        assert!(out.contains("show = false"));
        assert!(out.contains("show_topic_labels = true"));
    }

    #[test]
    fn empty_input_just_emits_the_inserts() {
        let out = merge_top_level_scalars("", &[("k", "1".into())]);
        assert_eq!(out, "k = 1\n");
    }
}
