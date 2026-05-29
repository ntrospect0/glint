// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Configuration schema for the calendar widget — TOML on-disk shape,
//! defaults, the wizard descriptor, and the wizard-side TOML
//! render/load helpers. No render/state code here; this is the
//! data-and-schema layer the rest of the widget reads from.

use std::collections::HashMap;

use chrono::Weekday;
use serde::{Deserialize, Serialize};

use super::local;
use crate::theme::ColorScheme;
use crate::ui::big_digits;

pub(super) const VIEW_TABS: &[(CalendarView, &str)] = &[
    (CalendarView::Day, "day"),
    (CalendarView::Week, "week"),
    (CalendarView::Month, "month"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CalendarView {
    #[default]
    Day,
    Week,
    Month,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    #[default]
    Local,
    Google,
    #[serde(alias = "apple", alias = "icloud")]
    Caldav,
    #[serde(alias = "microsoft", alias = "ms365")]
    Outlook,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CalendarConfig {
    #[serde(default)]
    pub default_view: CalendarView,

    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    /// Calendar sources. Empty = local-only (use `[[events]]` below).
    #[serde(default)]
    pub providers: Vec<ProviderEntry>,

    /// Fallback URLs for any `caldav` entry without explicit `calendar_ids`.
    #[serde(default)]
    pub caldav: CalDavConfig,

    /// Events for the built-in local provider.
    #[serde(default)]
    pub events: Vec<local::RawEvent>,

    /// ANSI palette cycled across calendars in `[[providers]]` order. Names
    /// like `red`, `light_blue`. Wraps when more calendars than colors.
    #[serde(default)]
    pub color_palette: Vec<String>,

    /// Per-calendar overrides keyed by `"<source>:<calendar_id>"`
    /// (e.g. `"google:primary"`). Wins over the palette sequence.
    #[serde(default)]
    pub calendar_colors: HashMap<String, String>,

    /// Big-digit gradient for the day-of-month numeral in Day view.
    /// `g` cycles. Only applies to today — anchor/preview days stay solid.
    #[serde(default)]
    pub gradient: big_digits::Gradient,

    /// Per-widget overrides layered on the app theme. Distinct from
    /// `calendar_colors`, which colors per-provider event blocks.
    #[serde(default)]
    pub colors: ColorScheme,

    /// `Shift+<letter>` focus shortcuts; falls back to `['c', 'd', 'a', 'l', 'e', 'n', 'r']`.
    #[serde(default)]
    pub shortcuts: Vec<char>,

    /// Which weekday starts the week in Week + Month views. Defaults to
    /// Sunday (US convention); ISO/Europe users typically set
    /// `first_day_of_week = "monday"`. Any chrono-recognized lowercase
    /// weekday name works (sunday/monday/tuesday/...). Invalid values
    /// fall back to Sunday with a `serde` parse error logged.
    #[serde(default)]
    pub first_day_of_week: FirstDayOfWeek,
}

/// Configurable first-day-of-week. Defaults to Sunday. Serialized as
/// a lowercase weekday name (`"sunday"`, `"monday"`, …) so the TOML
/// reads naturally.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FirstDayOfWeek {
    #[default]
    Sunday,
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
}

impl FirstDayOfWeek {
    pub fn as_weekday(self) -> Weekday {
        match self {
            FirstDayOfWeek::Sunday => Weekday::Sun,
            FirstDayOfWeek::Monday => Weekday::Mon,
            FirstDayOfWeek::Tuesday => Weekday::Tue,
            FirstDayOfWeek::Wednesday => Weekday::Wed,
            FirstDayOfWeek::Thursday => Weekday::Thu,
            FirstDayOfWeek::Friday => Weekday::Fri,
            FirstDayOfWeek::Saturday => Weekday::Sat,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderEntry {
    pub kind: ProviderKind,
    /// Google IDs, Outlook IDs, or CalDAV URLs. Empty = the provider's default
    /// (Google `"primary"`, Outlook default, every CalDAV calendar).
    #[serde(default)]
    pub calendar_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CalDavConfig {
    /// Explicit calendar URLs. Empty = walk the CalDAV principal chain
    /// (current-user-principal → calendar-home-set → calendars) to discover.
    #[serde(default)]
    pub calendars: Vec<String>,
}

fn default_poll_interval() -> u64 {
    60
}

impl Default for CalendarConfig {
    fn default() -> Self {
        Self {
            default_view: CalendarView::default(),
            poll_interval_secs: default_poll_interval(),
            providers: Vec::new(),
            caldav: CalDavConfig::default(),
            events: Vec::new(),
            color_palette: Vec::new(),
            calendar_colors: HashMap::new(),
            gradient: big_digits::Gradient::default(),
            colors: ColorScheme::default(),
            shortcuts: Vec::new(),
            first_day_of_week: FirstDayOfWeek::default(),
        }
    }
}

pub const KIND: &str = "calendar";

/// Wizard descriptor. Covers the core common knobs (refresh interval +
/// per-provider OAuth handoff); structured data like
/// [[providers]] / [[events]] / `[calendar_colors]` lives in
/// calendar.toml and is preserved across `--setup` re-runs.
pub fn wizard_descriptor() -> crate::wizard::descriptor::WizardDescriptor {
    use crate::wizard::descriptor::{ChoiceOption, WizardDescriptor, WizardField, WizardFieldKind};
    WizardDescriptor {
        display_name: "Calendar",
        blurb: "Day / week / month agenda views across Google, Outlook, \
                CalDAV, and a built-in local provider. Tick the calendars \
                you'd like to pull from; the wizard runs the OAuth \
                handshakes; per-calendar IDs + CalDAV details live in \
                calendar.toml for hand-tuning.",
        load_from_toml: Some(load_calendar_from_toml),
        render_toml: Some(render_calendar_toml),
        fields: vec![
            WizardField {
                key: "sources",
                label: "Calendar sources",
                help: "Each ticked source becomes a [[providers]] block in \
                       calendar.toml. Google + Outlook need their OAuth \
                       handshake (next two fields). CalDAV credentials \
                       live in credentials/caldav.toml. Local needs no \
                       setup — uses [[events]] entries in calendar.toml.",
                required: false,
                kind: WizardFieldKind::MultiChoice {
                    options: vec![
                        ChoiceOption {
                            value: "google",
                            label: "Google Calendar",
                            help: None,
                        },
                        ChoiceOption {
                            value: "outlook",
                            label: "Outlook (Microsoft 365)",
                            help: None,
                        },
                        ChoiceOption {
                            value: "caldav",
                            label: "CalDAV (iCloud, Fastmail, Nextcloud, …)",
                            help: None,
                        },
                        ChoiceOption {
                            value: "local",
                            label: "Local events (defined in calendar.toml)",
                            help: None,
                        },
                    ],
                    defaults: vec!["local"],
                },
                validate: None,
            },
            WizardField {
                key: "poll_interval_secs",
                label: "Refresh interval (seconds)",
                help: "How often the calendar re-fetches events from each \
                       configured provider. 60–300s is usual.",
                required: true,
                kind: WizardFieldKind::Number {
                    default: Some(60.0),
                    range: Some((30.0, 3600.0)),
                    integer: true,
                },
                validate: None,
            },
            WizardField {
                key: "authorize_google",
                label: "Authorize Google Calendar",
                help: "Required if you want calendar.toml to include \
                       a [[providers]] block with kind = \"google\". Opens \
                       a browser to console.cloud.google.com for the OAuth \
                       consent, then captures the token on a loopback port.",
                required: false,
                kind: WizardFieldKind::OAuth { provider: "google" },
                validate: None,
            },
            WizardField {
                key: "authorize_microsoft",
                label: "Authorize Microsoft (Outlook calendar)",
                help: "Required for an Outlook calendar provider. Opens a \
                       browser to login.microsoftonline.com; if you haven't \
                       set up an Azure app yet, see \
                       credentials/microsoft_oauth_client.toml.",
                required: false,
                kind: WizardFieldKind::OAuth {
                    provider: "microsoft",
                },
                validate: None,
            },
        ],
    }
}

pub(super) fn load_calendar_from_toml(
    doc: &toml::Value,
) -> HashMap<String, crate::wizard::descriptor::WizardValue> {
    use crate::wizard::descriptor::WizardValue;
    let mut out = HashMap::new();
    if let Some(n) = doc.get("poll_interval_secs").and_then(|v| v.as_integer()) {
        out.insert("poll_interval_secs".into(), WizardValue::Number(n as f64));
    }
    // Derive the MultiChoice from existing [[providers]] blocks. We
    // accept the same aliases the runtime deserializer does
    // (apple/icloud → caldav, microsoft/ms365 → outlook) so a
    // hand-edited file round-trips cleanly.
    if let Some(arr) = doc.get("providers").and_then(|v| v.as_array()) {
        let mut sources: Vec<String> = Vec::new();
        for entry in arr {
            if let Some(kind) = entry.get("kind").and_then(|v| v.as_str()) {
                let canonical = match kind {
                    "google" => "google",
                    "outlook" | "microsoft" | "ms365" => "outlook",
                    "caldav" | "apple" | "icloud" => "caldav",
                    "local" => "local",
                    _ => continue,
                };
                if !sources.iter().any(|s| s == canonical) {
                    sources.push(canonical.to_string());
                }
            }
        }
        if !sources.is_empty() {
            out.insert("sources".into(), WizardValue::MultiChoice(sources));
        }
    }
    out
}

pub(super) fn render_calendar_toml(
    values: &HashMap<String, crate::wizard::descriptor::WizardValue>,
    existing: Option<&str>,
) -> String {
    use crate::wizard::descriptor::WizardValue;

    let scalars: Vec<(&str, String)> = vec![(
        "poll_interval_secs",
        match values.get("poll_interval_secs") {
            Some(WizardValue::Number(n)) => format!("{}", *n as i64),
            _ => "60".into(),
        },
    )];

    // Build [[providers]] blocks. For each selected source, reuse the
    // user's existing block (preserving calendar_ids etc.) when one
    // exists, else emit a minimal default.
    let selected_kinds: Vec<String> = match values.get("sources") {
        Some(WizardValue::MultiChoice(items)) => items.clone(),
        _ => vec!["local".into()],
    };
    let existing_blocks: HashMap<String, String> =
        existing_provider_blocks_by_kind(existing.unwrap_or(""));

    let mut provider_blocks = String::new();
    for kind in &selected_kinds {
        if let Some(block) = existing_blocks.get(kind) {
            provider_blocks.push_str("\n");
            provider_blocks.push_str(block);
        } else {
            provider_blocks.push_str(&format!("\n[[providers]]\nkind = \"{kind}\"\n"));
        }
    }

    let base: std::borrow::Cow<str> = match existing {
        Some(text) => std::borrow::Cow::Borrowed(text),
        None => std::borrow::Cow::Borrowed(crate::config::DEFAULT_CALENDAR_TOML),
    };
    let stripped = crate::wizard::toml_merge::strip_array_of_tables_blocks(&base, "providers");
    let merged = crate::wizard::toml_merge::merge_top_level_scalars(&stripped, &scalars);

    let mut out = merged;
    if !out.ends_with("\n\n") {
        if out.ends_with('\n') {
            out.push('\n');
        } else {
            out.push_str("\n\n");
        }
    }
    out.push_str(provider_blocks.trim_start_matches('\n'));
    out
}

/// Pull each existing `[[providers]]` block out of the text, keyed by
/// canonicalised kind (apple/icloud → caldav, etc.) so a re-render can
/// preserve the user's `calendar_ids` lists when they keep that
/// source ticked.
fn existing_provider_blocks_by_kind(text: &str) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    let Ok(doc) = toml::from_str::<toml::Value>(text) else {
        return out;
    };
    let Some(arr) = doc.get("providers").and_then(|v| v.as_array()) else {
        return out;
    };
    for entry in arr {
        let Some(kind) = entry.get("kind").and_then(|v| v.as_str()) else {
            continue;
        };
        let canonical = match kind {
            "google" => "google",
            "outlook" | "microsoft" | "ms365" => "outlook",
            "caldav" | "apple" | "icloud" => "caldav",
            "local" => "local",
            _ => continue,
        };
        // Re-emit the block from the parsed Value so we don't have to
        // line-scan the source for boundaries. Manual emit keeps the
        // output predictable (kind first, then calendar_ids).
        let mut block = String::from("[[providers]]\n");
        block.push_str(&format!("kind = \"{canonical}\"\n"));
        if let Some(ids) = entry.get("calendar_ids").and_then(|v| v.as_array()) {
            let items: Vec<String> = ids
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| format!("\"{}\"", s.replace('"', "\\\"")))
                .collect();
            if !items.is_empty() {
                block.push_str(&format!("calendar_ids = [{}]\n", items.join(", ")));
            }
        }
        out.insert(canonical.to_string(), block);
    }
    out
}
