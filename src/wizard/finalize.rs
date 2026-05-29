// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Transactional commit of wizard state to the real TOML files.
//!
//! Called once, after the user presses `Complete and Save` on the confirm
//! page. Walks the in-memory [`WizardState`] and produces:
//!
//! - `~/.config/glint/config.toml`             — `[global]` + `[layout]`
//! - `~/.config/glint/<kind>[@<instance>].toml` — one per assigned widget
//! - `~/.config/glint/llm.toml` — when the picked provider differs from
//!   what's on disk
//! - `~/.config/glint/credentials/<provider>_key.toml` for the active
//!   provider (if a key was entered)
//!
//! Widgets whose descriptor has no fields keep their existing TOML
//! untouched when the file is present; otherwise they're seeded from
//! the `DEFAULT_*_TOML` constants in `config::mod`. Widgets with
//! fields render their TOML from the captured wizard values.

#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::cache::Cache;
use crate::credentials;
use crate::config;
use crate::widgets::registry;

use super::descriptor::{WizardDescriptor, WizardField, WizardFieldKind, WizardValue};
use super::pages::layout as layout_page;
use super::state::{LayoutChoice, WizardState};

pub fn write_all(state: &WizardState) -> Result<()> {
    let dir = config::config_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;

    write_main_config(state, &dir)?;
    for assignment in &state.assignments {
        if !assignment.kind.is_empty() {
            // Single-widget cell.
            write_widget_config(state, &assignment.widget_id(), &assignment.kind, &dir)?;
        } else if !assignment.stack_children.is_empty() {
            // Stack cell: write a TOML per child so the user's
            // wizard-collected values for each stack child end up on
            // disk. Without this every stack child would silently fall
            // back to the seeded default TOML.
            for child in &assignment.stack_children {
                if child.kind.is_empty() {
                    continue;
                }
                write_widget_config(state, &child.widget_id(), &child.kind, &dir)?;
            }
        }
        // else: truly empty cell — nothing to write.
    }
    write_llm_settings(state, &dir)?;
    flush_runtime_caches();
    Ok(())
}

/// Drop every cached widget payload (news bodies, calendar events, email
/// inboxes, image thumbs, etc.) on the floor at the end of finalize. If
/// the user just switched email provider from Outlook to Gmail, the
/// Outlook inbox sitting in `~/.cache/glint/email/main/` would otherwise
/// flash on the first frame before the Gmail provider replaces it. Same
/// hazard for any provider/account swap inside a widget. Best-effort —
/// configs are already written and the wizard has succeeded; a cache
/// clear failure just means widgets refetch slower on next launch.
fn flush_runtime_caches() {
    match Cache::open_default() {
        Ok(cache) => {
            if let Err(err) = cache.clear_all() {
                tracing::warn!(
                    error = %err,
                    "wizard: failed to flush cache after finalize"
                );
            }
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                "wizard: failed to open cache for post-finalize flush"
            );
        }
    }
    // Stack active-tab indices (runtime_state.toml) are also tied to
    // the previous layout — clearing the file means each stack starts
    // on its first child after a wizard run instead of restoring a
    // potentially-stale active tab from a different layout.
    if let Err(err) = crate::runtime_state::clear() {
        tracing::warn!(
            error = %err,
            "wizard: failed to clear runtime_state.toml after finalize"
        );
    }
}

fn write_main_config(state: &WizardState, dir: &Path) -> Result<()> {
    // KeepExisting → leave config.toml's [layout] alone (we still rewrite
    // [global] to honour wizard answers). Re-read existing file when
    // possible so unrelated keys survive.
    let existing = fs::read_to_string(dir.join("config.toml")).ok();
    let layout_section = match &state.layout {
        LayoutChoice::Preset { name } => {
            Some(substitute_assignments(render_preset_layout(name), state))
        }
        // Keep-existing: preserve the user's [layout] block (column
        // widths, row heights, comments, custom spans) but apply any
        // widget reassignments / empty-cell marks the user made on the
        // Assign page. Assignments and existing `[[layout.cells]]`
        // blocks are paired in file order — the same order hydration
        // uses, so re-saves round-trip cleanly. If there's no existing
        // file at all, fall back to the magazine preset.
        LayoutChoice::KeepExisting => existing
            .as_deref()
            .and_then(extract_layout_section)
            .map(|layout| apply_assignments_to_existing_layout(&layout, state))
            .or_else(|| {
                Some(substitute_assignments(
                    render_preset_layout("magazine"),
                    state,
                ))
            }),
    };

    let mut out = String::new();
    out.push_str("version = 1\n\n");
    out.push_str("[global]\n");
    out.push_str(&format!(
        "theme = {}\n",
        toml_string(&choice_or(state.global_get("theme"), "default"))
    ));
    out.push_str("command_key = \":\"\n");
    out.push_str("refresh_all_on_focus = true\n");
    out.push_str("log_level = \"info\"\n");
    out.push_str(&format!(
        "mouse_scroll = {}\n",
        toml_string(&choice_or(state.global_get("mouse_scroll"), "natural",))
    ));
    out.push_str("show_status_bar = true\n");
    out.push('\n');
    if let Some(layout) = layout_section {
        out.push_str(&layout);
    }
    atomic_write(&dir.join("config.toml"), &out)
}

fn write_widget_config(state: &WizardState, widget_id: &str, kind: &str, dir: &Path) -> Result<()> {
    let Some(desc) = registry::find(kind) else {
        return Ok(());
    };
    let wd = (desc.wizard)();
    let path = dir.join(format!("{widget_id}.toml"));

    if wd.fields.is_empty() {
        // Defer-to-TOML: seed from the in-tree default if the file
        // doesn't exist yet; otherwise leave the user's edits alone.
        if path.exists() {
            return Ok(());
        }
        if let Some(seed) = default_seed_for(kind) {
            return atomic_write(&path, seed);
        }
        return Ok(());
    }

    let body = if let Some(custom) = wd.render_toml {
        // Custom renderers receive the flat key → value map and the
        // existing on-disk text (when present) — used by widgets whose
        // config shape can't be expressed as one top-level assignment
        // per field, and by widgets that need to merge into hand-edited
        // arrays the wizard doesn't manage (e.g. news's [[feeds]]).
        let mut values: HashMap<String, WizardValue> = HashMap::new();
        for f in &wd.fields {
            let v = state
                .widget_get(widget_id, f.key)
                .cloned()
                .unwrap_or_else(|| f.kind.initial_value());
            values.insert(f.key.to_string(), v);
        }
        let existing = fs::read_to_string(&path).ok();
        custom(&values, existing.as_deref())
    } else {
        render_wizard_toml(state, widget_id, &wd)
    };
    atomic_write(&path, &body)
}

fn render_wizard_toml(state: &WizardState, widget_id: &str, wd: &WizardDescriptor) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Generated by `glint --setup`. Hand-edit freely; the wizard preserves\n\
         # advanced keys it doesn't manage.\n\n",
    ));
    for field in &wd.fields {
        let v = state
            .widget_get(widget_id, field.key)
            .cloned()
            .unwrap_or_else(|| field.kind.initial_value());
        out.push_str(&render_field_assignment(field, &v));
    }
    out
}

fn render_field_assignment(field: &WizardField, value: &WizardValue) -> String {
    let key = field.key;
    match value {
        WizardValue::Text(s) | WizardValue::Choice(s) | WizardValue::Path(s) => {
            format!("{key} = {}\n", toml_string(s))
        }
        WizardValue::Number(n) => {
            if matches!(field.kind, WizardFieldKind::Number { integer: true, .. }) {
                format!("{key} = {}\n", *n as i64)
            } else {
                format!("{key} = {}\n", n)
            }
        }
        WizardValue::Bool(b) => format!("{key} = {b}\n"),
        WizardValue::MultiChoice(items) | WizardValue::TextList(items) => {
            let mut line = format!("{key} = [");
            line.push_str(
                &items
                    .iter()
                    .map(|s| toml_string(s))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            line.push_str("]\n");
            line
        }
    }
}

/// Quote-and-escape a string for use as a TOML value. Conservative: we use
/// basic strings and escape `"` + `\` + control chars; that's enough for
/// the values the wizard collects (URLs, labels, IANA TZ names, paths).
fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn choice_or(v: Option<&WizardValue>, fallback: &str) -> String {
    match v {
        Some(WizardValue::Choice(s)) | Some(WizardValue::Text(s)) => s.clone(),
        _ => fallback.to_string(),
    }
}

fn picked_provider(state: &WizardState) -> &'static crate::llm::LlmProviderDef {
    let name = match state.global_get("llm_provider") {
        Some(WizardValue::Choice(s)) => s.as_str(),
        _ => "",
    };
    crate::llm::find_provider(name)
        .or_else(|| crate::llm::PROVIDERS.first())
        .expect("at least one LLM provider must be registered")
}

fn key_for_provider(state: &WizardState, provider: &str) -> Option<String> {
    let key = format!("llm_api_key__{provider}");
    match state.global_get(&key) {
        Some(WizardValue::Text(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

/// Write the active provider's API key (if entered) and rewrite
/// `llm.toml`'s `[provider]` block to point at the picked provider.
/// Other providers' on-disk credentials files are left untouched so the
/// user doesn't lose unrelated keys by switching.
fn write_llm_settings(state: &WizardState, dir: &Path) -> Result<()> {
    let provider = picked_provider(state);
    if let Some(key) = key_for_provider(state, provider.name) {
        write_provider_key(provider, &key)?;
    }
    write_llm_toml(provider, dir)?;
    Ok(())
}

fn write_provider_key(provider: &crate::llm::LlmProviderDef, key: &str) -> Result<()> {
    let dir = credentials::dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(provider.credentials_filename);
    let body = format!(
        "# {} API key. Generated by `glint --setup`.\n\
         # Get one at {}.\n\
         api_key = {}\n",
        provider.display_name,
        provider.key_portal_url,
        toml_string(key),
    );
    atomic_write(&path, &body)
}

fn write_llm_toml(provider: &crate::llm::LlmProviderDef, dir: &Path) -> Result<()> {
    let path = dir.join("llm.toml");
    let existing = fs::read_to_string(&path).ok();
    let body = render_llm_toml(provider, existing.as_deref());
    atomic_write(&path, &body)
}

/// Render `llm.toml` for the picked provider. If a previous `llm.toml`
/// exists, preserve the user's `enabled`, `[limits]`, and any
/// `[provider].model` / `api_base` / `max_tokens` that already point at
/// the *same* provider — switching providers resets those to the
/// registry defaults (the old values referenced a different API).
fn render_llm_toml(provider: &crate::llm::LlmProviderDef, existing: Option<&str>) -> String {
    let prev = existing.and_then(|s| toml::from_str::<toml::Value>(s).ok());
    let prev_block = prev.as_ref().and_then(|d| d.get("provider"));
    let prev_limits = prev.as_ref().and_then(|d| d.get("limits"));
    // Carry forward the previous [provider] field only when the user
    // is still on the same provider; otherwise reset to the registry
    // default (the prior value referenced a different API).
    let same_provider = prev_block
        .and_then(|p| p.get("name"))
        .and_then(|v| v.as_str())
        == Some(provider.name);
    let carried_str = |key: &str| -> Option<String> {
        if !same_provider {
            return None;
        }
        prev_block?.get(key)?.as_str().map(str::to_string)
    };
    let carried_u32 = |key: &str| -> Option<u32> {
        if !same_provider {
            return None;
        }
        prev_block?.get(key)?.as_integer().map(|n| n as u32)
    };
    let enabled = prev
        .as_ref()
        .and_then(|d| d.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let model = carried_str("model").unwrap_or_else(|| provider.default_model.to_string());
    let api_base = carried_str("api_base").unwrap_or_else(|| provider.default_api_base.to_string());
    let max_tokens = carried_u32("max_tokens").unwrap_or(provider.default_max_tokens);
    let rpm = prev_limits
        .and_then(|l| l.get("max_requests_per_minute"))
        .and_then(|v| v.as_integer())
        .map(|n| n as u32)
        .unwrap_or(20);
    let cache_capacity = prev_limits
        .and_then(|l| l.get("cache_capacity"))
        .and_then(|v| v.as_integer())
        .map(|n| n as u64)
        .unwrap_or(1024);
    format!(
        "# Generated by `glint --setup`. Edit freely; `--setup` re-runs\n\
         # rewrite only the keys the wizard manages.\n\
         enabled = {enabled}\n\
         \n\
         [provider]\n\
         name = {name}\n\
         model = {model}\n\
         api_base = {api_base}\n\
         max_tokens = {max_tokens}\n\
         \n\
         [limits]\n\
         max_requests_per_minute = {rpm}\n\
         cache_capacity = {cache_capacity}\n",
        name = toml_string(provider.name),
        model = toml_string(&model),
        api_base = toml_string(&api_base),
    )
}

fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let tmp = path.with_extension("toml.wizard.tmp");
    fs::write(&tmp, contents).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} → {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Render the `[layout]` block for a named preset by walking the
/// preset's declared grid definition. Equal fractions for columns and
/// rows; users wanting custom sizing edit `config.toml` afterward.
/// Falls back to the first preset (`single`) if the name doesn't match,
/// which keeps a working dashboard rather than corrupting the file.
fn render_preset_layout(name: &str) -> String {
    let preset = layout_page::all_presets()
        .iter()
        .find(|p| p.id == name)
        .unwrap_or(&layout_page::all_presets()[0]);

    let mut out = String::new();
    out.push_str("[layout]\n");
    out.push_str(&format!(
        "columns = [{}]\n",
        equal_split_percentages(preset.grid_cols)
    ));
    out.push_str(&format!(
        "rows = [{}]\n",
        equal_split_percentages(preset.grid_rows)
    ));
    out.push('\n');

    for (i, (col, row, col_span, row_span)) in preset.grid_def.iter().enumerate() {
        out.push_str("[[layout.cells]]\n");
        out.push_str(&format!("widget = \"<cell-{i}>\"\n"));
        out.push_str(&format!("col = {col}\n"));
        out.push_str(&format!("row = {row}\n"));
        if *col_span > 1 {
            out.push_str(&format!("col_span = {col_span}\n"));
        }
        if *row_span > 1 {
            out.push_str(&format!("row_span = {row_span}\n"));
        }
        out.push('\n');
    }
    out
}

/// Distribute 100% across `n` slots as integer percentages summing to
/// exactly 100, comma-separated. Extra percentage points (when 100 isn't
/// divisible by n) are added to the leading slots — visually
/// indistinguishable but keeps the sum exact for downstream consumers
/// that may validate the row/column total.
fn equal_split_percentages(n: usize) -> String {
    if n == 0 {
        return "100".to_string();
    }
    let base = 100 / n;
    let mut remainder = 100 - base * n;
    (0..n)
        .map(|_| {
            let mut x = base;
            if remainder > 0 {
                x += 1;
                remainder -= 1;
            }
            x.to_string()
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Substitute `<cell-N>` placeholders in the rendered layout with the
/// actual widget ids the user assigned. Empty cells get their entire
/// `[[layout.cells]]` block stripped out so unassigned panes simply
/// don't render (rather than being silently filled with a fallback
/// widget).
fn substitute_assignments(layout: String, state: &WizardState) -> String {
    let mut out = layout;
    for assignment in &state.assignments {
        let placeholder = format!("<cell-{}>", assignment.cell_index);
        if assignment.is_stack() {
            // Replace `widget = "<cell-N>"` with
            // `widgets = ["child1", "child2", ...]`. Search for the
            // full single-quoted assignment so we don't accidentally
            // touch a line that contains `<cell-N>` as a substring.
            let single_line = format!("widget = \"{placeholder}\"");
            if let Some(idx) = out.find(&single_line) {
                let ids = assignment
                    .stack_children
                    .iter()
                    .map(|c| format!("\"{}\"", c.widget_id()))
                    .collect::<Vec<_>>()
                    .join(", ");
                let replacement = format!("widgets = [{ids}]");
                out.replace_range(idx..idx + single_line.len(), &replacement);
            } else {
                // `render_preset_layout` always emits the cell's
                // widget on its own line. If we can't find it, the
                // preset rendering and the placeholder-replacement
                // logic have drifted — surface that in dev. In
                // release fall back gracefully so the dashboard
                // still has a working cell.
                debug_assert!(
                    false,
                    "preset cell `{placeholder}` had no single-line widget form"
                );
                out = out.replace(&placeholder, &assignment.widget_id());
            }
        } else if assignment.kind.is_empty() {
            out = strip_cell_block(&out, &placeholder);
        } else {
            out = out.replace(&placeholder, &assignment.widget_id());
        }
    }
    // Any placeholders the wizard didn't touch (e.g. preset has more
    // cells than the assignments vec) are dropped so the dashboard
    // doesn't try to render with an unresolved widget kind.
    let mut leftover = 0;
    loop {
        let token = format!("<cell-{leftover}>");
        if !out.contains(&token) {
            break;
        }
        out = strip_cell_block(&out, &token);
        leftover += 1;
    }
    out
}

/// Remove the full `[[layout.cells]] ... \n\n` block that contains the
/// given placeholder. Leaves the surrounding text untouched. If the
/// block can't be located (defensive — shouldn't happen), the
/// placeholder itself is replaced with an empty string so the
/// substitution pass terminates.
fn strip_cell_block(layout: &str, placeholder: &str) -> String {
    let Some(token_pos) = layout.find(placeholder) else {
        return layout.to_string();
    };
    // Walk backward to the `[[layout.cells]]` header that introduces
    // this block.
    let block_start = layout[..token_pos]
        .rfind("[[layout.cells]]")
        .unwrap_or(token_pos);
    // Walk forward to the next blank line OR the next `[[layout.cells]]`
    // header OR end-of-string — whichever comes first.
    let after_block = block_start + "[[layout.cells]]".len();
    let next_break = layout[after_block..]
        .find("\n\n")
        .map(|p| after_block + p + 2)
        .or_else(|| {
            layout[after_block..]
                .find("[[layout.cells]]")
                .map(|p| after_block + p)
        })
        .unwrap_or(layout.len());
    let mut out = String::with_capacity(layout.len());
    out.push_str(&layout[..block_start]);
    out.push_str(&layout[next_break..]);
    out
}

fn extract_layout_section(existing: &str) -> Option<String> {
    let start = existing.find("[layout]")?;
    Some(existing[start..].to_string())
}

/// Apply the wizard's assignment list to a `[layout]` block the user
/// chose to keep intact. Walks the existing `[[layout.cells]]` blocks in
/// file order and, for each one, either:
///
/// - rewrites the `widget = "..."` line to point at `assignments[i].widget_id()`, or
/// - strips the entire block when the user marked that cell empty on
///   the Assign page (assignments[i].kind.is_empty()).
///
/// Everything *outside* the cell blocks — `columns`, `rows`, comments,
/// custom keys we don't manage — is preserved verbatim. Pairing is
/// strictly by file order, which matches the order hydration uses to
/// populate `state.assignments`; a user who hand-reorders blocks
/// between wizard runs would see the wizard slots map to the new file
/// order, which is the usual TOML convention.
fn apply_assignments_to_existing_layout(layout: &str, state: &WizardState) -> String {
    let blocks = layout_cell_block_ranges(layout);
    let mut out = String::with_capacity(layout.len());
    let mut cursor = 0;
    for (i, (start, end)) in blocks.iter().enumerate() {
        // Push the inter-block text (header, columns/rows, blank lines,
        // comments) unchanged.
        out.push_str(&layout[cursor..*start]);
        cursor = *end;
        match state.assignments.get(i) {
            Some(a) if a.is_stack() => {
                // Rewrite `widget = "..."` as `widgets = [...]` for
                // stack cells. The block's other keys (col, row,
                // col_span, comments) stay intact.
                let block_text = &layout[*start..*end];
                let ids = a
                    .stack_children
                    .iter()
                    .map(|c| format!("\"{}\"", c.widget_id()))
                    .collect::<Vec<_>>()
                    .join(", ");
                let rewritten = rewrite_widget_line_to_widgets_array(block_text, &ids);
                out.push_str(&rewritten);
            }
            Some(a) if a.kind.is_empty() && a.stack_children.is_empty() => {
                // Strip — push nothing for this block (its trailing
                // whitespace, which lives inside the block range, also
                // disappears so we don't leave double-blank gaps).
            }
            Some(a) => {
                let block_text = &layout[*start..*end];
                let rewritten = rewrite_widget_line(block_text, &a.widget_id());
                out.push_str(&rewritten);
            }
            None => {
                // Fewer assignments than file has blocks — defensive:
                // leave the orphan block alone. Shouldn't happen under
                // normal hydrate→wizard→finalize flow, but better than
                // silently dropping the user's data on resume edge cases.
                out.push_str(&layout[*start..*end]);
            }
        }
    }
    out.push_str(&layout[cursor..]);
    out
}

/// Locate every `[[layout.cells]]` block in `layout`, returning
/// `(start, end)` byte ranges. Each block runs from its header up to
/// (but not including) the next header — so trailing blank lines /
/// comments after the last `widget = ...` line stay associated with
/// their block, which is what we want for the strip-empty path.
fn layout_cell_block_ranges(layout: &str) -> Vec<(usize, usize)> {
    const HEADER: &str = "[[layout.cells]]";
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut search_from = 0;
    while let Some(rel) = layout[search_from..].find(HEADER) {
        let start = search_from + rel;
        let after = start + HEADER.len();
        let end = layout[after..]
            .find(HEADER)
            .map(|p| after + p)
            .unwrap_or(layout.len());
        ranges.push((start, end));
        search_from = end;
    }
    ranges
}

/// Replace the first `widget = "..."` line inside `block` with one
/// pointing at `new_widget`. Leading whitespace on the matched line is
/// preserved so the existing indentation style survives. If the block
/// somehow has no `widget` line (unusual — would be invalid layout
/// TOML), one is appended at the end so the cell still gets rendered.
fn rewrite_widget_line(block: &str, new_widget: &str) -> String {
    rewrite_cell_widget_line(block, &["widget"], |leading| {
        format!("{leading}widget = {}\n", toml_string(new_widget))
    })
}

/// Like `rewrite_widget_line` but emits `widgets = [...]` instead of
/// the scalar form. Used for stack cells. `widgets_array` is the
/// already-formatted comma-separated body (e.g. `"clock", "weather"`).
fn rewrite_widget_line_to_widgets_array(block: &str, widgets_array: &str) -> String {
    rewrite_cell_widget_line(block, &["widget", "widgets"], |leading| {
        format!("{leading}widgets = [{widgets_array}]\n")
    })
}

/// Rewrite the first cell-key line in `block` (matching any of
/// `match_keys`) by replacing it with `render(leading_whitespace)`.
/// Appends the rendered line if no match exists, so callers don't
/// have to special-case the "fresh cell" path.
fn rewrite_cell_widget_line(
    block: &str,
    match_keys: &[&str],
    render: impl Fn(&str) -> String,
) -> String {
    let mut out = String::with_capacity(block.len() + 64);
    let mut replaced = false;
    for line in block.split_inclusive('\n') {
        if !replaced {
            let trimmed = line.trim_start();
            let matches = match_keys
                .iter()
                .any(|k| trimmed.starts_with(k) && line_assigns_key(k, trimmed));
            if matches {
                let leading = &line[..line.len() - trimmed.len()];
                out.push_str(&render(leading));
                replaced = true;
                continue;
            }
        }
        out.push_str(line);
    }
    if !replaced {
        out.push_str(&render(""));
    }
    out
}

/// `true` when `trimmed` looks like a TOML key assignment of the form
/// `<key> = ...` (allowing whitespace around `=`). Guards against
/// accidentally matching keys whose names start with `widget` (e.g.
/// `widget_title = "..."`) when we're searching for the `widget` line.
fn line_assigns_key(key: &str, trimmed: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix(key) else {
        return false;
    };
    let rest = rest.trim_start();
    rest.starts_with('=')
}

fn default_seed_for(kind: &str) -> Option<&'static str> {
    match kind {
        "clock" => Some(config::DEFAULT_CLOCK_TOML),
        "weather" => Some(config::DEFAULT_WEATHER_TOML),
        "news" => Some(config::DEFAULT_NEWS_TOML),
        "stocks" => Some(config::DEFAULT_STOCKS_TOML),
        "calendar" => Some(config::DEFAULT_CALENDAR_TOML),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wizard::state::CellAssignment;

    fn state_for(preset_id: &str, assignments: Vec<(usize, &str)>) -> WizardState {
        let mut state = WizardState::default();
        state.layout = LayoutChoice::Preset {
            name: preset_id.to_string(),
        };
        state.assignments = assignments
            .into_iter()
            .map(|(cell_index, kind)| CellAssignment {
                cell_index,
                kind: kind.to_string(),
                instance: "main".to_string(),
                stack_children: Vec::new(),
            })
            .collect();
        state
    }

    /// Count rendered `[[layout.cells]]` blocks. Asserts that the
    /// emitted layout has exactly the number of cells the user picked
    /// (plus their per-preset shape) — guards against the regression
    /// where single-pane setups landed a 5-cell magazine grid.
    fn cell_block_count(layout: &str) -> usize {
        layout.matches("[[layout.cells]]").count()
    }

    #[test]
    fn stack_assignment_emits_widgets_array_in_preset_layout() {
        // Build a state with a stack assignment in cell 0; render the
        // single preset; verify the output contains
        // `widgets = ["clock", "weather"]` instead of `widget = "..."`.
        let mut state = state_for("single", vec![(0, "stocks")]);
        state.assignments[0].kind = String::new();
        state.assignments[0].stack_children = vec![
            crate::wizard::state::StackChild {
                kind: "clock".into(),
                instance: "main".into(),
            },
            crate::wizard::state::StackChild {
                kind: "weather".into(),
                instance: "main".into(),
            },
        ];
        let layout = substitute_assignments(render_preset_layout("single"), &state);
        assert!(
            layout.contains("widgets = [\"clock\", \"weather\"]"),
            "expected widgets array in layout:\n{layout}"
        );
        assert!(
            !layout.contains("widget = \"<cell-0>\""),
            "placeholder should be consumed:\n{layout}"
        );
    }

    #[test]
    fn keep_existing_rewrites_stack_assignment_as_widgets_array() {
        // Pre-existing single-widget cell becomes a 2-widget stack.
        let existing = "[layout]\n\
                        columns = [50, 50]\n\
                        rows = [100]\n\
                        \n\
                        [[layout.cells]]\n\
                        widget = \"clock\"\n\
                        col = 0\n\
                        row = 0\n\
                        \n\
                        [[layout.cells]]\n\
                        widget = \"weather\"\n\
                        col = 1\n\
                        row = 0\n";
        let mut state = state_for("magazine", vec![(0, "clock"), (1, "weather")]);
        // Turn cell 0 into a stack.
        state.assignments[0].kind = String::new();
        state.assignments[0].stack_children = vec![
            crate::wizard::state::StackChild {
                kind: "clock".into(),
                instance: "main".into(),
            },
            crate::wizard::state::StackChild {
                kind: "stocks".into(),
                instance: "main".into(),
            },
        ];
        let out = apply_assignments_to_existing_layout(existing, &state);
        assert!(out.contains("widgets = [\"clock\", \"stocks\"]"));
        // Other cell stays a single-widget cell.
        assert!(out.contains("widget = \"weather\""));
        // Cell metadata preserved.
        assert!(out.contains("col = 0"));
    }

    #[test]
    fn single_preset_emits_exactly_one_cell() {
        let state = state_for("single", vec![(0, "stocks")]);
        let layout = substitute_assignments(render_preset_layout("single"), &state);
        assert_eq!(
            cell_block_count(&layout),
            1,
            "single-pane preset should emit one cell; got:\n{layout}"
        );
        assert!(
            layout.contains("widget = \"stocks\""),
            "stocks assignment missing from rendered layout:\n{layout}"
        );
    }

    #[test]
    fn every_preset_emits_cell_count_matching_its_definition() {
        // Walks every preset in the picker. Each one, fully populated
        // with concrete widget kinds, should emit exactly `preset.cells`
        // `[[layout.cells]]` blocks — no more, no less.
        for preset in layout_page::all_presets() {
            let assignments: Vec<(usize, &str)> = (0..preset.cells).map(|i| (i, "clock")).collect();
            let state = state_for(&preset.id, assignments);
            let layout = substitute_assignments(render_preset_layout(&preset.id), &state);
            assert_eq!(
                cell_block_count(&layout),
                preset.cells,
                "preset {} should emit {} cells; got:\n{}",
                preset.id,
                preset.cells,
                layout
            );
        }
    }

    #[test]
    fn equal_split_sums_to_exactly_one_hundred() {
        for n in 1..=8 {
            let parts: Vec<usize> = equal_split_percentages(n)
                .split(", ")
                .map(|s| s.parse().unwrap())
                .collect();
            assert_eq!(parts.len(), n);
            assert_eq!(parts.iter().sum::<usize>(), 100, "n={n}");
        }
    }

    #[test]
    fn keep_existing_rewrites_widget_lines_per_assignment() {
        // User had clock + weather on disk; they re-ran --setup and
        // changed cell 0's widget to stocks via the Assign page while
        // keeping the existing layout. Finalize should rewrite the
        // first cell's widget line while preserving everything else.
        let existing = "[layout]\n\
                        columns = [50, 50]\n\
                        rows = [100]\n\
                        \n\
                        [[layout.cells]]\n\
                        widget = \"clock\"\n\
                        col = 0\n\
                        row = 0\n\
                        \n\
                        [[layout.cells]]\n\
                        widget = \"weather\"\n\
                        col = 1\n\
                        row = 0\n";
        let state = state_for(
            "magazine", // irrelevant — we're testing the KeepExisting path
            vec![(0, "stocks"), (1, "weather")],
        );
        let out = apply_assignments_to_existing_layout(existing, &state);
        assert!(out.contains("widget = \"stocks\""));
        assert!(out.contains("widget = \"weather\""));
        assert!(!out.contains("widget = \"clock\""));
        // Layout structure untouched.
        assert!(out.contains("columns = [50, 50]"));
        assert!(out.contains("rows = [100]"));
        // Cell metadata untouched.
        assert!(out.contains("col = 0"));
        assert!(out.contains("col = 1"));
        assert_eq!(cell_block_count(&out), 2);
    }

    #[test]
    fn keep_existing_strips_block_for_empty_assignment() {
        let existing = "[layout]\n\
                        columns = [50, 50]\n\
                        rows = [100]\n\
                        \n\
                        [[layout.cells]]\n\
                        widget = \"clock\"\n\
                        col = 0\n\
                        row = 0\n\
                        \n\
                        [[layout.cells]]\n\
                        widget = \"weather\"\n\
                        col = 1\n\
                        row = 0\n";
        let state = state_for("magazine", vec![(0, "clock"), (1, "")]);
        let out = apply_assignments_to_existing_layout(existing, &state);
        assert_eq!(cell_block_count(&out), 1);
        assert!(out.contains("widget = \"clock\""));
        assert!(!out.contains("widget = \"weather\""));
    }

    #[test]
    fn keep_existing_preserves_custom_keys_inside_cell_blocks() {
        // User had hand-added `col_span = 2` and a comment to a cell.
        // Wizard rewrite of the widget line must leave those alone.
        let existing = "[layout]\n\
                        columns = [50, 50]\n\
                        rows = [50, 50]\n\
                        \n\
                        [[layout.cells]]\n\
                        # user-added comment\n\
                        widget = \"news\"\n\
                        col = 0\n\
                        row = 1\n\
                        col_span = 2\n";
        let state = state_for("magazine", vec![(0, "stocks")]);
        let out = apply_assignments_to_existing_layout(existing, &state);
        assert!(out.contains("# user-added comment"));
        assert!(out.contains("col_span = 2"));
        assert!(out.contains("widget = \"stocks\""));
    }

    #[test]
    fn line_assigns_key_distinguishes_widget_from_widget_title() {
        // The `widget` matcher must not mistake `widget_title = "..."`
        // for the cell's widget assignment.
        assert!(line_assigns_key("widget", "widget = \"clock\""));
        assert!(line_assigns_key("widget", "widget  =  \"clock\""));
        assert!(!line_assigns_key("widget", "widget_title = \"x\""));
        assert!(!line_assigns_key("widget", "widget.something = 1"));
    }

    #[test]
    fn empty_assignments_strip_their_cell_blocks() {
        // User picks a 3-cell preset but leaves cell 1 empty. Final
        // layout must drop cell 1's block entirely.
        let state = state_for("three_column", vec![(0, "clock"), (1, ""), (2, "weather")]);
        let layout = substitute_assignments(render_preset_layout("three_column"), &state);
        assert_eq!(cell_block_count(&layout), 2);
        assert!(layout.contains("widget = \"clock\""));
        assert!(layout.contains("widget = \"weather\""));
    }

    fn provider_def(name: &str) -> &'static crate::llm::LlmProviderDef {
        crate::llm::find_provider(name)
            .unwrap_or_else(|| panic!("test requires registered LLM provider {name:?}"))
    }

    #[test]
    fn switching_llm_provider_resets_model_and_api_base_to_registry_defaults() {
        // User was on anthropic with a custom model; they re-run --setup
        // and pick openai. The rewritten [provider] block should track
        // openai's defaults — keeping anthropic's model name there would
        // 400 the first request.
        let prev = "enabled = true\n\
                    \n\
                    [provider]\n\
                    name = \"anthropic\"\n\
                    model = \"claude-opus-4-7\"\n\
                    api_base = \"https://api.anthropic.com\"\n\
                    max_tokens = 2048\n\
                    \n\
                    [limits]\n\
                    max_requests_per_minute = 40\n\
                    cache_capacity = 2048\n";
        let openai = provider_def("openai");
        let rendered = render_llm_toml(openai, Some(prev));
        assert!(rendered.contains("name = \"openai\""));
        assert!(rendered.contains(&format!("model = \"{}\"", openai.default_model)));
        assert!(rendered.contains(&format!("api_base = \"{}\"", openai.default_api_base)));
        // Limits + enabled are provider-agnostic — preserve them.
        assert!(rendered.contains("enabled = true"));
        assert!(rendered.contains("max_requests_per_minute = 40"));
        assert!(rendered.contains("cache_capacity = 2048"));
    }

    #[test]
    fn restaying_on_same_llm_provider_preserves_custom_model() {
        // Same provider as before → custom model survives the rewrite.
        let prev = "enabled = true\n\
                    \n\
                    [provider]\n\
                    name = \"anthropic\"\n\
                    model = \"claude-opus-4-7\"\n\
                    api_base = \"https://api.anthropic.com\"\n\
                    max_tokens = 2048\n";
        let rendered = render_llm_toml(provider_def("anthropic"), Some(prev));
        assert!(rendered.contains("model = \"claude-opus-4-7\""));
        assert!(rendered.contains("max_tokens = 2048"));
    }

    #[test]
    fn first_run_llm_toml_uses_registry_defaults() {
        let rendered = render_llm_toml(provider_def("openai"), None);
        let openai = provider_def("openai");
        assert!(rendered.contains("name = \"openai\""));
        assert!(rendered.contains(&format!("model = \"{}\"", openai.default_model)));
        assert!(rendered.contains(&format!("max_tokens = {}", openai.default_max_tokens)));
        assert!(rendered.contains("max_requests_per_minute = 20"));
        assert!(rendered.contains("cache_capacity = 1024"));
    }
}
