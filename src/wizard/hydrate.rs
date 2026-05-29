// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Pre-seed a fresh [`WizardState`] from the user's existing on-disk
//! configs. Runs once at wizard start when there's no `.wizard_state.toml`
//! resume buffer — re-running `--setup` with prior configs should surface
//! the user's current values as defaults, not start from zero.
//!
//! Inverse of [`super::finalize`]: finalize takes state → disk; hydrate
//! takes disk → state. Both modules walk the same widget registry +
//! descriptor schema so they stay in sync as fields evolve.
//!
//! Read failures and parse errors are non-fatal — hydration is a
//! best-effort UX improvement, never the only source of truth. If a file
//! is unreadable we log + skip rather than abort the wizard.

#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config;
use crate::widgets::registry;

use super::descriptor::{WizardDescriptor, WizardField, WizardFieldKind, WizardValue};
use super::state::{CellAssignment, LayoutChoice, WizardState};

/// Walk the config dir and overlay everything we can recover onto
/// `state`. Existing values in `state` (e.g. from a partial resume) win
/// — hydration never clobbers in-progress edits.
pub fn hydrate_from_disk(state: &mut WizardState) {
    let Ok(dir) = config::config_dir() else {
        return;
    };
    if !dir.exists() {
        return;
    }

    let main_toml = parse_toml_file(&dir.join("config.toml"));
    if let Some(doc) = main_toml.as_ref() {
        hydrate_global(state, doc);
        hydrate_assignments_from_layout(state, doc);
    }
    hydrate_widget_values(state, &dir);
    hydrate_llm_settings(state, &dir);
}

/// Read + parse a single TOML file. `None` on any failure (missing file,
/// I/O error, parse error). Errors are logged but not propagated.
fn parse_toml_file(path: &Path) -> Option<toml::Value> {
    if !path.exists() {
        return None;
    }
    match fs::read_to_string(path) {
        Ok(text) => match toml::from_str::<toml::Value>(&text) {
            Ok(v) => Some(v),
            Err(err) => {
                tracing::warn!(
                    file = %path.display(),
                    error = %err,
                    "hydrate: TOML parse failed; skipping"
                );
                None
            }
        },
        Err(err) => {
            tracing::warn!(
                file = %path.display(),
                error = %err,
                "hydrate: read failed; skipping"
            );
            None
        }
    }
}

/// Seed `state.global` from `[global]` in config.toml. Only keys the
/// wizard's Global page knows about are copied — anything else stays in
/// the user's TOML untouched (finalize re-renders only the keys it
/// manages).
fn hydrate_global(state: &mut WizardState, doc: &toml::Value) {
    let Some(global) = doc.get("global").and_then(|v| v.as_table()) else {
        return;
    };
    for key in ["theme", "mouse_scroll"] {
        if state.global.contains_key(key) {
            continue;
        }
        if let Some(s) = global.get(key).and_then(|v| v.as_str()) {
            state
                .global
                .insert(key.to_string(), WizardValue::Choice(s.to_string()));
        }
    }
}

/// Read `[[layout.cells]]` and turn each `widget = "<id>"` entry into a
/// `CellAssignment`. The on-disk layout shape isn't (yet) reverse-mapped
/// back to a wizard preset, so we set [`LayoutChoice::KeepExisting`] —
/// finalize will leave the `[layout]` block alone unless the user
/// explicitly picks a new preset in the wizard's Layout page.
///
/// Skipped entirely if the wizard state already has assignments (i.e.
/// the user is mid-flow and has navigated the Layout page).
fn hydrate_assignments_from_layout(state: &mut WizardState, doc: &toml::Value) {
    if !state.assignments.is_empty() {
        return;
    }
    let Some(cells) = doc
        .get("layout")
        .and_then(|v| v.get("cells"))
        .and_then(|v| v.as_array())
    else {
        return;
    };
    let mut next_cell = 0usize;
    for cell in cells {
        // Stack cell: `widgets = [...]`. Take precedence over the
        // scalar `widget` field if both are present (which would be
        // invalid TOML but we're defensive).
        if let Some(arr) = cell.get("widgets").and_then(|v| v.as_array()) {
            let mut children: Vec<crate::wizard::state::StackChild> = Vec::new();
            for entry in arr {
                let Some(s) = entry.as_str() else {
                    continue;
                };
                let s = s.trim();
                if s.is_empty() {
                    continue;
                }
                let (kind, instance) = parse_widget_id(s);
                if registry::find(&kind).is_none() {
                    continue;
                }
                children.push(crate::wizard::state::StackChild { kind, instance });
            }
            if children.len() >= 2 {
                state.assignments.push(CellAssignment {
                    cell_index: next_cell,
                    kind: String::new(),
                    instance: "main".into(),
                    stack_children: children,
                });
                next_cell += 1;
                continue;
            }
            // Single-element widgets array degrades to non-stack;
            // fall through to the scalar path so the cell still
            // registers correctly.
            if let Some(only) = children.into_iter().next() {
                state.assignments.push(CellAssignment {
                    cell_index: next_cell,
                    kind: only.kind,
                    instance: only.instance,
                    stack_children: Vec::new(),
                });
                next_cell += 1;
                continue;
            }
        }
        let Some(widget_id) = cell.get("widget").and_then(|v| v.as_str()) else {
            continue;
        };
        let (kind, instance) = parse_widget_id(widget_id);
        // Drop entries that reference widget kinds we don't know about —
        // they'd render as "Unknown widget" pages and confuse the user.
        if registry::find(&kind).is_none() {
            continue;
        }
        state.assignments.push(CellAssignment {
            cell_index: next_cell,
            kind,
            instance,
            stack_children: Vec::new(),
        });
        next_cell += 1;
    }
    if !state.assignments.is_empty() {
        state.layout = LayoutChoice::KeepExisting;
    }
}

/// `clock` → (`"clock"`, `"main"`); `clock@home` → (`"clock"`, `"home"`).
/// The `main` sentinel mirrors what [`super::state::CellAssignment::widget_id`]
/// emits for the default instance.
fn parse_widget_id(s: &str) -> (String, String) {
    match s.split_once('@') {
        Some((kind, instance)) => (kind.to_string(), instance.to_string()),
        None => (s.to_string(), "main".to_string()),
    }
}

/// For each widget known to the registry, look for its `<kind>.toml`
/// (and any `<kind>@<instance>.toml` siblings the user has on disk),
/// parse, and seed `state.widget_values` per the widget's descriptor.
///
/// Uses each descriptor's `load_from_toml` callback when defined; falls
/// back to a generic field-by-key auto-loader otherwise (covers every
/// widget whose TOML shape is flat scalars + arrays — i.e. everything
/// except clock's `[[secondary_timezones]]`).
fn hydrate_widget_values(state: &mut WizardState, dir: &Path) {
    for desc in crate::widgets::registry::WIDGETS {
        let wd = (desc.wizard)();
        if wd.fields.is_empty() {
            continue; // defer-to-TOML widgets have no wizard fields to seed
        }
        for (widget_id, path) in widget_toml_paths(desc.kind, dir) {
            let Some(doc) = parse_toml_file(&path) else {
                continue;
            };
            let values = match wd.load_from_toml {
                Some(loader) => loader(&doc),
                None => auto_load_from_toml(&wd, &doc),
            };
            for (key, value) in values {
                // Resume values win; hydration only fills gaps.
                if state.widget_get(&widget_id, key.as_str()).is_some() {
                    continue;
                }
                // Promote `&'static str` lookup-key into the state via
                // widget_set — it accepts a `&str` for the key, so we
                // intern via a leaked Box only when needed. Cheaper to
                // just clone the key once.
                state.widget_set(&widget_id, key.as_str(), value);
            }
        }
    }
}

/// Find every `<kind>[@<instance>].toml` for the given widget kind in
/// `dir`. Returns `(widget_id, path)` pairs — `widget_id` matches the
/// key used in `state.widget_values`.
fn widget_toml_paths(kind: &str, dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    let default_path = dir.join(format!("{kind}.toml"));
    if default_path.exists() {
        out.push((kind.to_string(), default_path));
    }
    // Scan the directory for `<kind>@<instance>.toml`. dir.read_dir
    // missing is non-fatal; just means there are no instance variants.
    if let Ok(entries) = fs::read_dir(dir) {
        let prefix = format!("{kind}@");
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let Some(rest) = name.strip_prefix(&prefix) else {
                continue;
            };
            let Some(instance) = rest.strip_suffix(".toml") else {
                continue;
            };
            if instance.is_empty() {
                continue;
            }
            out.push((format!("{kind}@{instance}"), entry.path()));
        }
    }
    out
}

/// Generic per-field loader. Walks the descriptor's fields and pulls a
/// value for each one out of the parsed TOML, using the field's `key`
/// as a top-level TOML key. Field kinds map to TOML types as follows:
///
/// | Field kind                | TOML expectation        |
/// |---------------------------|-------------------------|
/// | Text / Path / Choice / Lookup | string              |
/// | Number                    | integer or float        |
/// | Bool                      | bool                    |
/// | MultiChoice / TextList    | array of strings        |
/// | OAuth                     | (skipped — status only) |
///
/// Missing keys are silently skipped — the wizard falls back to each
/// field's own `default` for anything we don't seed.
pub fn auto_load_from_toml(
    wd: &WizardDescriptor,
    doc: &toml::Value,
) -> HashMap<String, WizardValue> {
    let mut out = HashMap::new();
    for field in &wd.fields {
        if let Some(value) = extract_field_value(field, doc) {
            out.insert(field.key.to_string(), value);
        }
    }
    out
}

fn extract_field_value(field: &WizardField, doc: &toml::Value) -> Option<WizardValue> {
    let raw = doc.get(field.key)?;
    match &field.kind {
        WizardFieldKind::Text { .. } | WizardFieldKind::Path { .. } => {
            raw.as_str().map(|s| match field.kind {
                WizardFieldKind::Path { .. } => WizardValue::Path(s.to_string()),
                _ => WizardValue::Text(s.to_string()),
            })
        }
        WizardFieldKind::Choice { .. } | WizardFieldKind::Lookup { .. } => {
            raw.as_str().map(|s| WizardValue::Choice(s.to_string()))
        }
        WizardFieldKind::Number { .. } => {
            // TOML can store the number as either an int or a float; we
            // accept either and promote to f64 to match the wizard's
            // internal representation.
            if let Some(n) = raw.as_integer() {
                Some(WizardValue::Number(n as f64))
            } else {
                raw.as_float().map(WizardValue::Number)
            }
        }
        WizardFieldKind::Bool { .. } => raw.as_bool().map(WizardValue::Bool),
        WizardFieldKind::MultiChoice { .. }
        | WizardFieldKind::RemoteMultiChoice { .. }
        | WizardFieldKind::TextList { .. } => {
            let arr = raw.as_array()?;
            let items: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
            Some(match field.kind {
                WizardFieldKind::TextList { .. } => WizardValue::TextList(items),
                _ => WizardValue::MultiChoice(items),
            })
        }
        WizardFieldKind::OAuth { .. } => None,
    }
}

/// Hydrate the LLM provider choice from `llm.toml` and each registered
/// provider's API key from its credentials file. We ignore placeholder
/// strings `init_default_config` writes so the wizard doesn't display
/// "REPLACE_WITH_YOUR_KEY" as the current value. Per-provider keys are
/// stored under `llm_api_key__<provider>` so switching the wizard's
/// picker reveals whichever key was already on disk.
fn hydrate_llm_settings(state: &mut WizardState, config_dir: &Path) {
    if !state.global.contains_key("llm_provider") {
        if let Some(doc) = parse_toml_file(&config_dir.join("llm.toml")) {
            if let Some(name) = doc
                .get("provider")
                .and_then(|p| p.get("name"))
                .and_then(|v| v.as_str())
            {
                if crate::llm::find_provider(name.trim()).is_some() {
                    state.global.insert(
                        "llm_provider".to_string(),
                        WizardValue::Choice(name.trim().to_string()),
                    );
                }
            }
        }
    }
    let Ok(creds_dir) = crate::credentials::dir() else {
        return;
    };
    for def in crate::llm::PROVIDERS {
        let state_key = format!("llm_api_key__{}", def.name);
        if state.global.contains_key(&state_key) {
            continue;
        }
        let path = creds_dir.join(def.credentials_filename);
        let Some(doc) = parse_toml_file(&path) else {
            continue;
        };
        let Some(key) = doc.get("api_key").and_then(|v| v.as_str()) else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || key.starts_with("REPLACE_WITH_") {
            continue;
        }
        state
            .global
            .insert(state_key, WizardValue::Text(key.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wizard::descriptor::{Separator, WizardDescriptor};

    fn flat_descriptor() -> WizardDescriptor {
        WizardDescriptor {
            display_name: "Test",
            blurb: "",
            load_from_toml: None,
            render_toml: None,
            fields: vec![
                WizardField {
                    key: "label",
                    label: "L",
                    help: "",
                    required: false,
                    kind: WizardFieldKind::Text {
                        default: None,
                        placeholder: None,
                    },
                    validate: None,
                },
                WizardField {
                    key: "poll_interval_secs",
                    label: "P",
                    help: "",
                    required: false,
                    kind: WizardFieldKind::Number {
                        default: None,
                        range: None,
                        integer: true,
                    },
                    validate: None,
                },
                WizardField {
                    key: "enabled",
                    label: "E",
                    help: "",
                    required: false,
                    kind: WizardFieldKind::Bool { default: false },
                    validate: None,
                },
                WizardField {
                    key: "watchlist",
                    label: "W",
                    help: "",
                    required: false,
                    kind: WizardFieldKind::TextList {
                        default: vec![],
                        separator: Separator::Comma,
                    },
                    validate: None,
                },
            ],
        }
    }

    #[test]
    fn auto_load_pulls_scalars_and_lists_by_field_key() {
        let doc: toml::Value = toml::from_str(
            r#"label = "Richmond"
poll_interval_secs = 600
enabled = true
watchlist = ["AAPL", "MSFT"]
"#,
        )
        .unwrap();
        let wd = flat_descriptor();
        let out = auto_load_from_toml(&wd, &doc);
        assert_eq!(
            out.get("label"),
            Some(&WizardValue::Text("Richmond".into()))
        );
        assert_eq!(
            out.get("poll_interval_secs"),
            Some(&WizardValue::Number(600.0))
        );
        assert_eq!(out.get("enabled"), Some(&WizardValue::Bool(true)));
        assert_eq!(
            out.get("watchlist"),
            Some(&WizardValue::TextList(vec!["AAPL".into(), "MSFT".into()]))
        );
    }

    #[test]
    fn auto_load_skips_missing_keys() {
        let doc: toml::Value = toml::from_str("label = \"x\"\n").unwrap();
        let out = auto_load_from_toml(&flat_descriptor(), &doc);
        assert_eq!(out.len(), 1);
        assert!(out.contains_key("label"));
    }

    #[test]
    fn auto_load_accepts_integer_or_float_for_number_fields() {
        let doc: toml::Value = toml::from_str("poll_interval_secs = 15.5\n").unwrap();
        let out = auto_load_from_toml(&flat_descriptor(), &doc);
        assert_eq!(
            out.get("poll_interval_secs"),
            Some(&WizardValue::Number(15.5))
        );
    }

    #[test]
    fn parse_widget_id_splits_on_at() {
        assert_eq!(parse_widget_id("clock"), ("clock".into(), "main".into()));
        assert_eq!(
            parse_widget_id("clock@home"),
            ("clock".into(), "home".into())
        );
        assert_eq!(
            parse_widget_id("email@gmail"),
            ("email".into(), "gmail".into())
        );
    }

    #[test]
    fn clock_descriptor_load_round_trips_secondary_timezones() {
        // End-to-end check that re-running --setup with an existing
        // clock.toml on disk would surface the user's three secondary
        // world clocks back into the wizard's three Lookup slots.
        let clock_toml = r#"timezone = "America/Vancouver"
hour_format = "24h"
show_seconds = true
show_date = false

[[secondary_timezones]]
label = "New York"
timezone = "America/New_York"

[[secondary_timezones]]
label = "London"
timezone = "Europe/London"

[[secondary_timezones]]
label = "Tokyo"
timezone = "Asia/Tokyo"
"#;
        let doc: toml::Value = toml::from_str(clock_toml).unwrap();
        let desc = crate::widgets::registry::find("clock").expect("clock in registry");
        let wd = (desc.wizard)();
        let loader = wd
            .load_from_toml
            .expect("clock should declare load_from_toml");
        let out = loader(&doc);

        assert_eq!(
            out.get("timezone"),
            Some(&WizardValue::Choice("America/Vancouver".into()))
        );
        assert_eq!(
            out.get("hour_format"),
            Some(&WizardValue::Choice("24h".into()))
        );
        assert_eq!(out.get("show_seconds"), Some(&WizardValue::Bool(true)));
        assert_eq!(out.get("show_date"), Some(&WizardValue::Bool(false)));
        assert_eq!(
            out.get("secondary_tz_1"),
            Some(&WizardValue::Choice("America/New_York".into()))
        );
        assert_eq!(
            out.get("secondary_tz_2"),
            Some(&WizardValue::Choice("Europe/London".into()))
        );
        assert_eq!(
            out.get("secondary_tz_3"),
            Some(&WizardValue::Choice("Asia/Tokyo".into()))
        );
    }

    #[test]
    fn stocks_descriptor_auto_loads_indices_and_watchlist() {
        // The stocks widget has no custom load_from_toml — exercise the
        // auto-loader path against its real descriptor + a realistic TOML.
        let stocks_toml = r#"indices = ["^DJI", "^GSPC"]
watchlist = ["AAPL", "MSFT", "NVDA"]
poll_interval_secs = 30
default_display_mode = "percent"
default_period = "1d"
"#;
        let doc: toml::Value = toml::from_str(stocks_toml).unwrap();
        let desc = crate::widgets::registry::find("stocks").expect("stocks in registry");
        let wd = (desc.wizard)();
        let out = match wd.load_from_toml {
            Some(loader) => loader(&doc),
            None => auto_load_from_toml(&wd, &doc),
        };
        assert_eq!(
            out.get("indices"),
            Some(&WizardValue::TextList(vec!["^DJI".into(), "^GSPC".into()]))
        );
        assert_eq!(
            out.get("watchlist"),
            Some(&WizardValue::TextList(vec![
                "AAPL".into(),
                "MSFT".into(),
                "NVDA".into()
            ]))
        );
        assert_eq!(
            out.get("poll_interval_secs"),
            Some(&WizardValue::Number(30.0))
        );
    }

    #[test]
    fn news_descriptor_round_trips_scalar_fields() {
        // News uses a custom load_from_toml — confirm it surfaces the
        // four wizard-managed scalars and ignores the [[feeds]] /
        // [[topics]] arrays it doesn't touch.
        let news_toml = r#"poll_interval_secs = 600
show_topic_labels = false
summarize_with_llm = true
horizontal_scroll_filters = true

[[feeds]]
label = "Hacker News"
url = "https://hnrss.org/frontpage"
"#;
        let doc: toml::Value = toml::from_str(news_toml).unwrap();
        let desc = crate::widgets::registry::find("news").expect("news in registry");
        let wd = (desc.wizard)();
        let loader = wd
            .load_from_toml
            .expect("news should declare load_from_toml");
        let out = loader(&doc);
        assert_eq!(
            out.get("poll_interval_secs"),
            Some(&WizardValue::Number(600.0))
        );
        assert_eq!(
            out.get("show_topic_labels"),
            Some(&WizardValue::Bool(false))
        );
        assert_eq!(
            out.get("summarize_with_llm"),
            Some(&WizardValue::Bool(true))
        );
        assert_eq!(
            out.get("horizontal_scroll_filters"),
            Some(&WizardValue::Bool(true))
        );
    }

    #[test]
    fn news_render_preserves_existing_feeds_array() {
        // Critical: re-running --setup must not drop the user's
        // curated [[feeds]] list.
        let original = r#"poll_interval_secs = 900
show_topic_labels = true

[[feeds]]
label = "Custom Feed"
url = "https://example.com/rss"
"#;
        let mut values = std::collections::HashMap::new();
        values.insert("poll_interval_secs".into(), WizardValue::Number(120.0));
        values.insert("show_topic_labels".into(), WizardValue::Bool(false));
        values.insert("summarize_with_llm".into(), WizardValue::Bool(true));
        values.insert("horizontal_scroll_filters".into(), WizardValue::Bool(false));

        let desc = crate::widgets::registry::find("news").unwrap();
        let wd = (desc.wizard)();
        let renderer = wd.render_toml.expect("news has render_toml");
        let out = renderer(&values, Some(original));
        assert!(out.contains("poll_interval_secs = 120"));
        assert!(out.contains("show_topic_labels = false"));
        // The user's curated feed survives.
        assert!(out.contains("label = \"Custom Feed\""));
        assert!(out.contains("url = \"https://example.com/rss\""));
    }

    #[test]
    fn news_feed_multichoice_round_trips_catalogue_and_custom_feeds() {
        // User had two catalogue feeds + one custom URL. The wizard
        // should:
        //   - surface only the catalogue feeds in the MultiChoice load
        //   - preserve the custom feed across a render
        let original = r#"poll_interval_secs = 900

[[feeds]]
label = "Hacker News"
url = "https://hnrss.org/frontpage"

[[feeds]]
label = "BBC News"
url = "http://feeds.bbci.co.uk/news/rss.xml"

[[feeds]]
label = "My Private Feed"
url = "https://example.com/private.xml"
"#;
        let doc: toml::Value = toml::from_str(original).unwrap();
        let desc = crate::widgets::registry::find("news").unwrap();
        let wd = (desc.wizard)();
        let loader = wd.load_from_toml.expect("news load_from_toml");
        let loaded = loader(&doc);

        // Catalogue URLs surface, private URL doesn't.
        let selected = match loaded.get("feeds") {
            Some(WizardValue::MultiChoice(items)) => items.clone(),
            other => panic!("expected MultiChoice, got {other:?}"),
        };
        assert!(selected.contains(&"https://hnrss.org/frontpage".to_string()));
        assert!(selected.contains(&"http://feeds.bbci.co.uk/news/rss.xml".to_string()));
        assert!(!selected
            .iter()
            .any(|s| s.contains("example.com/private.xml")));

        // Render should re-emit both catalogue feeds AND the private one.
        let mut values = loaded;
        values.insert("poll_interval_secs".into(), WizardValue::Number(600.0));
        let renderer = wd.render_toml.expect("news render_toml");
        let rendered = renderer(&values, Some(original));
        assert!(rendered.contains("https://hnrss.org/frontpage"));
        assert!(rendered.contains("http://feeds.bbci.co.uk/news/rss.xml"));
        assert!(
            rendered.contains("https://example.com/private.xml"),
            "custom feed should be preserved across re-render:\n{rendered}"
        );
        assert!(rendered.contains("poll_interval_secs = 600"));
    }

    #[test]
    fn news_render_deselecting_catalogue_feed_drops_it_without_touching_custom() {
        let original = r#"poll_interval_secs = 900

[[feeds]]
label = "Hacker News"
url = "https://hnrss.org/frontpage"

[[feeds]]
label = "My Private Feed"
url = "https://example.com/private.xml"
"#;
        // Simulate the user deselecting Hacker News.
        let mut values = std::collections::HashMap::new();
        values.insert("feeds".into(), WizardValue::MultiChoice(vec![]));
        values.insert("poll_interval_secs".into(), WizardValue::Number(900.0));
        values.insert("show_topic_labels".into(), WizardValue::Bool(true));
        values.insert("summarize_with_llm".into(), WizardValue::Bool(true));
        values.insert("horizontal_scroll_filters".into(), WizardValue::Bool(false));
        let desc = crate::widgets::registry::find("news").unwrap();
        let wd = (desc.wizard)();
        let renderer = wd.render_toml.unwrap();
        let rendered = renderer(&values, Some(original));
        assert!(!rendered.contains("https://hnrss.org/frontpage"));
        assert!(rendered.contains("https://example.com/private.xml"));
    }

    #[test]
    fn news_topics_multichoice_round_trips_with_keyword_preservation() {
        // Existing file has Tech (with the user's edited keyword list)
        // and a custom topic outside the catalogue. The wizard should:
        //   - load only the catalogue topic into the MultiChoice
        //   - preserve the user's Tech keyword list across re-render
        //   - keep the custom topic verbatim
        let original = r#"poll_interval_secs = 900

[[topics]]
label = "Tech"
keywords = ["Rust", "Haskell", "Lisp"]

[[topics]]
label = "MyCustomTopic"
keywords = ["my-key"]
"#;
        let doc: toml::Value = toml::from_str(original).unwrap();
        let desc = crate::widgets::registry::find("news").unwrap();
        let wd = (desc.wizard)();
        let loader = wd.load_from_toml.unwrap();
        let loaded = loader(&doc);
        let topics = match loaded.get("topics") {
            Some(WizardValue::MultiChoice(items)) => items.clone(),
            other => panic!("expected MultiChoice, got {other:?}"),
        };
        assert!(topics.contains(&"Tech".to_string()));
        assert!(!topics.iter().any(|s| s == "MyCustomTopic"));

        // Render: user kept Tech ticked. Their custom keywords should
        // survive; the custom topic should be re-emitted intact.
        let mut values = loaded;
        values.insert("poll_interval_secs".into(), WizardValue::Number(900.0));
        values.insert("show_topic_labels".into(), WizardValue::Bool(true));
        values.insert("summarize_with_llm".into(), WizardValue::Bool(true));
        values.insert("horizontal_scroll_filters".into(), WizardValue::Bool(false));
        values.insert("feeds".into(), WizardValue::MultiChoice(vec![]));
        let renderer = wd.render_toml.unwrap();
        let rendered = renderer(&values, Some(original));

        assert!(rendered.contains("label = \"Tech\""));
        assert!(
            rendered.contains("\"Rust\""),
            "user's edited Tech keywords lost:\n{rendered}"
        );
        assert!(rendered.contains("\"Haskell\""));
        assert!(rendered.contains("label = \"MyCustomTopic\""));
        assert!(rendered.contains("\"my-key\""));
    }

    #[test]
    fn calendar_sources_multichoice_round_trips_providers() {
        let original = r#"poll_interval_secs = 60

[[providers]]
kind = "google"
calendar_ids = ["primary", "team@example.com"]

[[providers]]
kind = "local"
"#;
        let doc: toml::Value = toml::from_str(original).unwrap();
        let desc = crate::widgets::registry::find("calendar").unwrap();
        let wd = (desc.wizard)();
        let loader = wd.load_from_toml.unwrap();
        let loaded = loader(&doc);
        let sources = match loaded.get("sources") {
            Some(WizardValue::MultiChoice(items)) => items.clone(),
            other => panic!("expected MultiChoice, got {other:?}"),
        };
        assert!(sources.contains(&"google".to_string()));
        assert!(sources.contains(&"local".to_string()));

        // Render with same selection → preserves calendar_ids.
        let mut values = loaded;
        values.insert("poll_interval_secs".into(), WizardValue::Number(120.0));
        let renderer = wd.render_toml.unwrap();
        let rendered = renderer(&values, Some(original));
        assert!(rendered.contains("kind = \"google\""));
        assert!(rendered.contains("primary"));
        assert!(rendered.contains("team@example.com"));
        assert!(rendered.contains("kind = \"local\""));
        assert!(rendered.contains("poll_interval_secs = 120"));
    }

    #[test]
    fn email_folders_loaded_from_toml_as_multichoice() {
        let original = r#"provider = "gmail"
folders = ["INBOX", "SENT", "Bills/Utilities"]
"#;
        let doc: toml::Value = toml::from_str(original).unwrap();
        let desc = crate::widgets::registry::find("email").unwrap();
        let wd = (desc.wizard)();
        let loader = wd.load_from_toml.unwrap();
        let loaded = loader(&doc);
        let folders = match loaded.get("folders") {
            Some(WizardValue::MultiChoice(items)) => items.clone(),
            other => panic!("expected MultiChoice, got {other:?}"),
        };
        assert_eq!(folders, vec!["INBOX", "SENT", "Bills/Utilities"]);
    }

    #[test]
    fn email_render_emits_folders_array_from_multichoice() {
        let mut values = std::collections::HashMap::new();
        values.insert("provider".into(), WizardValue::Choice("gmail".into()));
        values.insert("latest_days".into(), WizardValue::Number(7.0));
        values.insert("refresh_minutes".into(), WizardValue::Number(5.0));
        values.insert("summarize_with_llm".into(), WizardValue::Bool(false));
        values.insert(
            "folders".into(),
            WizardValue::MultiChoice(vec!["INBOX".into(), "STARRED".into()]),
        );
        let desc = crate::widgets::registry::find("email").unwrap();
        let wd = (desc.wizard)();
        let renderer = wd.render_toml.unwrap();
        let out = renderer(&values, None);
        assert!(out.contains("provider = \"gmail\""));
        assert!(
            out.contains("folders = [\"INBOX\", \"STARRED\"]"),
            "folders array missing from output:\n{out}"
        );
    }

    #[test]
    fn email_descriptor_exposes_remote_multichoice_folders() {
        // Guard against the folder picker getting accidentally
        // downgraded back to a static field type.
        use crate::wizard::descriptor::WizardFieldKind;
        let desc = crate::widgets::registry::find("email").unwrap();
        let wd = (desc.wizard)();
        let folders_field = wd
            .fields
            .iter()
            .find(|f| f.key == "folders")
            .expect("folders field present");
        match &folders_field.kind {
            WizardFieldKind::RemoteMultiChoice { source, defaults } => {
                assert_eq!(*source, "email_folders");
                assert!(defaults.contains(&"INBOX"));
            }
            other => panic!("folders field should be RemoteMultiChoice; got {other:?}"),
        }
    }

    #[test]
    fn calendar_email_descriptors_expose_oauth_fields() {
        // Sanity guard: both widgets must surface an OAuth trigger
        // for each provider we wired up so the wizard can run them.
        for kind in ["calendar", "email"] {
            let desc = crate::widgets::registry::find(kind).unwrap();
            let wd = (desc.wizard)();
            let providers: Vec<&str> = wd
                .fields
                .iter()
                .filter_map(|f| match &f.kind {
                    WizardFieldKind::OAuth { provider } => Some(*provider),
                    _ => None,
                })
                .collect();
            assert!(
                providers.contains(&"google"),
                "{kind} should expose Google OAuth"
            );
            assert!(
                providers.contains(&"microsoft"),
                "{kind} should expose Microsoft OAuth"
            );
        }
    }

    #[test]
    fn registered_llm_providers_carry_wizard_metadata() {
        // The Global page builds its picker + credential-write paths
        // from `LlmProviderDef` metadata. Anything missing here breaks
        // the wizard, so guard the contract.
        for def in crate::llm::PROVIDERS {
            assert!(
                !def.display_name.is_empty(),
                "{} missing display_name",
                def.name
            );
            assert!(
                def.credentials_filename.ends_with(".toml"),
                "{} credentials_filename must be a .toml path: {:?}",
                def.name,
                def.credentials_filename
            );
            assert!(
                def.key_portal_url.starts_with("http"),
                "{} key_portal_url must be a URL: {:?}",
                def.name,
                def.key_portal_url
            );
        }
    }
}
