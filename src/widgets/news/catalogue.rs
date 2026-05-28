// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Curated topic + feed catalogue surfaced by the news widget's
//! `--setup` wizard.
//!
//! The catalogue lives in
//! [`src/widgets/news/catalogue.toml`](catalogue.toml) and is embedded
//! into the binary at build time via `include_str!`. Editing the TOML
//! requires a rebuild; users can also override the catalogue at
//! runtime by editing their `news.toml` (the wizard preserves
//! hand-added `[[topics]]` and `[[feeds]]` blocks across re-runs).
//!
//! The catalogue is *not* a runtime concept — it exists purely to
//! seed the wizard's checkbox lists and to recognize "is this URL one
//! of ours or custom?" during TOML renderer's preservation pass.
//! Actual topic-tagging at runtime uses whatever the user's
//! `news.toml` carries, not what's in this file.
//!
//! A future phase will additionally scan
//! `~/.config/glint/news.catalogue.toml` (or similar) so power users
//! can extend the wizard's built-in options without recompiling.

use serde::Deserialize;

/// Parsed shape of `catalogue.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct Catalogue {
    #[serde(default)]
    pub topics: Vec<CatalogueTopic>,
    #[serde(default)]
    pub feeds: Vec<CatalogueFeed>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CatalogueTopic {
    pub label: String,
    #[serde(default)]
    pub keywords: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CatalogueFeed {
    pub label: String,
    pub url: String,
}

const CATALOGUE_TOML: &str = include_str!("catalogue.toml");

/// Parse the embedded catalogue. Panics on a malformed embedded TOML
/// — that's a programmer error caught by the test below, not user
/// data. Cheap enough to call from the wizard step every time;
/// nothing on the render hot path consults it.
pub fn load() -> Catalogue {
    toml::from_str::<Catalogue>(CATALOGUE_TOML)
        .unwrap_or_else(|err| panic!("news/catalogue.toml: parse failed: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_parses() {
        let c = load();
        assert!(!c.topics.is_empty(), "expected at least one topic");
        assert!(!c.feeds.is_empty(), "expected at least one feed");
    }

    #[test]
    fn topic_labels_are_unique_and_non_empty() {
        let mut seen = std::collections::HashSet::new();
        for t in load().topics {
            assert!(!t.label.is_empty(), "topic label must not be empty");
            assert!(
                seen.insert(t.label.clone()),
                "duplicate topic label: {}",
                t.label
            );
        }
    }

    #[test]
    fn every_topic_has_at_least_one_keyword() {
        for t in load().topics {
            assert!(
                !t.keywords.is_empty(),
                "topic {} has no keywords — empty list never matches anything",
                t.label
            );
        }
    }

    #[test]
    fn feed_urls_are_unique_and_non_empty() {
        let mut seen = std::collections::HashSet::new();
        for f in load().feeds {
            assert!(!f.label.is_empty(), "feed label must not be empty");
            assert!(!f.url.is_empty(), "feed url must not be empty");
            assert!(
                seen.insert(f.url.clone()),
                "duplicate feed url: {}",
                f.url
            );
        }
    }
}
