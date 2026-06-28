// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Per-calendar color assignment. Resolves `(source, calendar_id)`
//! pairs to a Ratatui `Color` using (in order): explicit overrides
//! from `[calendar_colors]`, palette slots for calendars declared
//! in `[[providers]]`, then a stable hash for unexpected runtime
//! arrivals (rare — happens with CalDAV auto-discovery).

use std::collections::HashMap;

use ratatui::style::Color;

use super::config::{CalendarConfig, ProviderEntry, ProviderKind};
use crate::theme::parse_color;

/// Built-in palette cycled across calendars when the user hasn't supplied
/// their own `color_palette` in calendar.toml. Eight slots so up to eight
/// calendars get unique colors before the sequence repeats.
pub(super) const DEFAULT_PALETTE: [Color; 8] = [
    Color::LightBlue,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightMagenta,
    Color::LightCyan,
    Color::LightRed,
    Color::Blue,
    Color::Green,
];

pub(super) struct CalendarColors {
    palette: Vec<Color>,
    /// Explicit per-calendar overrides keyed by `(source, calendar_id)`.
    overrides: HashMap<(String, String), Color>,
    /// Pre-computed palette index for each calendar declared in config.
    assigned: HashMap<(String, String), usize>,
}

impl CalendarColors {
    pub(super) fn build(config: &CalendarConfig) -> Self {
        // Parse the user palette; fall back to defaults when entries are
        // empty or unrecognized rather than silently dropping the calendar's
        // distinct color.
        let palette: Vec<Color> = if config.color_palette.is_empty() {
            DEFAULT_PALETTE.to_vec()
        } else {
            let mut parsed: Vec<Color> = config
                .color_palette
                .iter()
                .filter_map(|s| parse_color(s))
                .collect();
            if parsed.is_empty() {
                parsed = DEFAULT_PALETTE.to_vec();
            }
            parsed
        };

        // Per-calendar overrides. Keys take the form "source:calendar_id".
        // Anything we can't parse is logged once and dropped so the rest of
        // the map still applies.
        let mut overrides: HashMap<(String, String), Color> = HashMap::new();
        for (key, value) in &config.calendar_colors {
            let Some((source, calendar)) = key.split_once(':') else {
                tracing::warn!(
                    key = %key,
                    "calendar_colors key missing 'source:' prefix — expected e.g. \"google:primary\""
                );
                continue;
            };
            let Some(color) = parse_color(value) else {
                tracing::warn!(key = %key, value = %value, "unrecognized color name");
                continue;
            };
            overrides.insert((source.to_string(), calendar.to_string()), color);
        }

        // Walk `[[providers]]` in order and assign each declared calendar
        // the next palette index. Overrides don't consume a slot, so the
        // next non-overridden calendar still gets palette[0].
        let entries: Vec<ProviderEntry> = config.providers.clone();

        let mut assigned: HashMap<(String, String), usize> = HashMap::new();
        let mut next_idx: usize = 0;
        for entry in &entries {
            let source = entry.source_label();
            let ids: Vec<String> = if entry.calendar_ids.is_empty() {
                // An empty list means "the provider's default calendar".
                // Each provider names that default slightly differently, but
                // for color purposes we just need a stable key.
                vec!["primary".to_string()]
            } else {
                entry.calendar_ids.clone()
            };
            for id in ids {
                let key = (source.to_string(), id);
                if overrides.contains_key(&key) || assigned.contains_key(&key) {
                    continue;
                }
                assigned.insert(key, next_idx);
                next_idx += 1;
            }
        }

        Self {
            palette,
            overrides,
            assigned,
        }
    }

    pub(super) fn resolve(&self, source: &str, calendar: &str) -> Color {
        let key = (source.to_string(), calendar.to_string());
        if let Some(c) = self.overrides.get(&key) {
            return *c;
        }
        if let Some(idx) = self.assigned.get(&key) {
            return self.palette[idx % self.palette.len()];
        }
        // Unknown calendar — hash the composite key into the palette so at
        // least same-name events stay one color across renders.
        let mut hash: u32 = 5381;
        for b in source
            .bytes()
            .chain(b":".iter().copied())
            .chain(calendar.bytes())
        {
            hash = hash.wrapping_mul(33).wrapping_add(u32::from(b));
        }
        self.palette[(hash as usize) % self.palette.len()]
    }
}

pub(super) fn provider_kind_label(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Local => "local",
        ProviderKind::Google => "google",
        ProviderKind::Outlook => "outlook",
        ProviderKind::Caldav => "caldav",
    }
}
