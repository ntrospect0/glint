// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Starter templates surfaced by the feeds widget's `--setup` wizard.
//!
//! Each template TOML lives at `src/widgets/feeds/templates/<id>.toml`
//! and is embedded into the binary at build time via `include_str!`.
//! At wizard time we parse the embedded TOMLs into [`Template`] values
//! and offer them as Choice options. The selected template's catalogue
//! is then copied into the freshly-generated `feeds@<instance>.toml`.
//!
//! Templates are *not* a runtime concept — the widget itself reads the
//! per-instance `[[feeds]]` blocks directly. Templates exist purely to
//! give the wizard something to seed a fresh file from.
//!
//! Adding a new built-in template: drop a new TOML in
//! `src/widgets/feeds/templates/`, add a matching `include_str!` line
//! to [`BUILTIN_TEMPLATES`] below, and ship a release. A future phase
//! will additionally scan `~/.config/glint/templates/` so power users
//! can add custom templates without recompiling.

use serde::Deserialize;

/// One starter template parsed from a TOML file.
#[derive(Debug, Clone, Deserialize)]
pub struct Template {
    /// Stable id matching the wizard's Choice option value. Lowercase
    /// ASCII, no whitespace. Surfaced to users as the wizard radio's
    /// `label` (via `display_name`), not the `value`.
    pub id: String,

    /// Human-readable name. Becomes the generated instance TOML's
    /// `display_name = "..."` value (which the runtime uses for the
    /// title bar, dashboard label, and LLM summarizer prompt).
    pub display_name: String,

    /// Preferred `Shift+<letter>` shortcut letters seeded into the
    /// generated instance TOML's `shortcuts = [...]` line.
    pub default_shortcut_prefs: Vec<char>,

    /// Per-instance command aliases seeded into the generated
    /// instance TOML's `commands = [...]` line. The runtime
    /// recognizes `:<alias>`, `:<alias>-summary`, and
    /// `:<alias>-refresh` for each alias. Empty for source
    /// templates that don't ship a short name (e.g. the WSJ
    /// template ships `["wsj"]`; an "Empty" template would have
    /// no defaults).
    #[serde(default)]
    pub default_commands: Vec<String>,

    /// Every topical feed this source publishes. `default = true`
    /// entries are written as live `[[feeds]]` blocks; the rest are
    /// written commented-out so the user can uncomment to enable.
    #[serde(default)]
    pub feeds: Vec<TemplateFeed>,
}

/// One catalogue entry inside a [`Template`].
#[derive(Debug, Clone, Deserialize)]
pub struct TemplateFeed {
    pub topic: String,
    pub url: String,
    /// Whether to write this feed live (`true`) or commented out
    /// (`false`) when the wizard generates a fresh instance TOML.
    #[serde(default)]
    pub default: bool,
}

/// Every compiled-in template, in wizard display order. Each entry is
/// `(id, raw_toml)`; we parse on demand rather than at static-init time
/// so a malformed template only crashes the wizard step, not the whole
/// binary.
const BUILTIN_TEMPLATES: &[(&str, &str)] = &[
    ("wsj", include_str!("templates/wsj.toml")),
    (
        "marketwatch",
        include_str!("templates/marketwatch.toml"),
    ),
];

/// Parse every built-in template. Panics on a malformed embedded TOML
/// — those failures are programmer errors caught by the test below,
/// not user-supplied bad data.
pub fn all() -> Vec<Template> {
    BUILTIN_TEMPLATES
        .iter()
        .map(|(id, raw)| {
            toml::from_str::<Template>(raw)
                .unwrap_or_else(|err| panic!("templates/{id}.toml: parse failed: {err}"))
        })
        .collect()
}

/// Look up a single template by id (case-insensitive). Returns
/// `None` for unknown ids so the wizard can fall back to WSJ rather
/// than panic on a stale Choice value.
pub fn by_id(id: &str) -> Option<Template> {
    all().into_iter().find(|t| t.id.eq_ignore_ascii_case(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_template_parses() {
        let parsed = all();
        assert_eq!(
            parsed.len(),
            BUILTIN_TEMPLATES.len(),
            "every embedded template should parse"
        );
    }

    #[test]
    fn template_ids_are_lowercase_unique_and_non_empty() {
        let mut seen = std::collections::HashSet::new();
        for t in all() {
            assert!(!t.id.is_empty(), "template id must not be empty");
            assert_eq!(
                t.id,
                t.id.to_ascii_lowercase(),
                "template id must be lowercase: {}",
                t.id
            );
            assert!(seen.insert(t.id.clone()), "duplicate template id: {}", t.id);
        }
    }

    #[test]
    fn template_id_matches_file_slug() {
        for (slug, _) in BUILTIN_TEMPLATES {
            let t = by_id(slug).unwrap_or_else(|| {
                panic!("by_id({slug}) returned None — template id doesn't match filename")
            });
            assert_eq!(
                t.id, *slug,
                "templates/{slug}.toml declares id = {:?}; should match its filename",
                t.id
            );
        }
    }

    #[test]
    fn templates_have_at_least_one_default_feed() {
        for t in all() {
            assert!(
                t.feeds.iter().any(|f| f.default),
                "template {} should have at least one default feed",
                t.id
            );
        }
    }

    #[test]
    fn no_duplicate_topics_within_a_template() {
        for t in all() {
            let mut seen = std::collections::HashSet::new();
            for feed in &t.feeds {
                assert!(
                    seen.insert(feed.topic.clone()),
                    "template {}: duplicate topic {:?}",
                    t.id,
                    feed.topic
                );
            }
        }
    }

    #[test]
    fn by_id_is_case_insensitive() {
        assert!(by_id("wsj").is_some());
        assert!(by_id("WSJ").is_some());
        assert!(by_id("Wsj").is_some());
        assert!(by_id("marketwatch").is_some());
        assert!(by_id("MarketWatch").is_some());
        assert!(by_id("definitely-not-a-source").is_none());
    }
}
