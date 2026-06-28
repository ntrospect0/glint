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
    /// Account label for same-provider multi-account (e.g. a work Outlook
    /// alongside a personal one). Omitted ⇒ the `"default"` account. The
    /// label names which `…_oauth_token.<account>.toml` to use and, when
    /// non-default, becomes this entry's `source` so colors don't collide.
    /// Only Google and Outlook are account-aware today.
    #[serde(default)]
    pub account: Option<String>,
    /// Google IDs, Outlook IDs, or CalDAV URLs. Empty = the provider's default
    /// (Google `"primary"`, Outlook default, every CalDAV calendar).
    #[serde(default)]
    pub calendar_ids: Vec<String>,
}

impl ProviderEntry {
    /// Token-storage account label — the explicit `account`, or `"default"`.
    pub(super) fn account_label(&self) -> &str {
        self.account.as_deref().unwrap_or(crate::auth::DEFAULT_ACCOUNT)
    }

    /// Identity used for the cell title + color keys. The default account
    /// reads as the provider kind (`"outlook"`) so existing single-account
    /// configs and `calendar_colors` keys are unaffected; a named account is
    /// provider-namespaced as `kind/account` (`"outlook/work"`) so it stays
    /// grouped under its provider and never collides with a same-label
    /// account of a different kind. `/` (not `:`) because `:` already
    /// separates source from calendar in `calendar_colors` keys.
    pub(super) fn source_label(&self) -> String {
        let kind = super::colors::provider_kind_label(self.kind);
        match &self.account {
            Some(a) if a != crate::auth::DEFAULT_ACCOUNT => format!("{kind}/{a}"),
            _ => kind.to_string(),
        }
    }
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

    // Build [[providers]] blocks. The wizard's `sources` toggle is keyed by
    // kind, but a kind can have several blocks (multi-account — e.g. work +
    // personal Outlook hand-added to calendar.toml). So we *preserve* rather
    // than rebuild: every existing block of a still-selected kind is kept
    // verbatim (account label and all); unticked kinds drop out; a freshly
    // ticked kind with no existing block gets one default block.
    let selected_kinds: Vec<String> = match values.get("sources") {
        Some(WizardValue::MultiChoice(items)) => items.clone(),
        _ => vec!["local".into()],
    };
    let existing_blocks: HashMap<String, Vec<String>> =
        existing_provider_blocks_by_kind(existing.unwrap_or(""));

    let mut provider_blocks = String::new();
    for kind in &selected_kinds {
        match existing_blocks.get(kind) {
            Some(blocks) => {
                for block in blocks {
                    provider_blocks.push_str("\n");
                    provider_blocks.push_str(block);
                }
            }
            None => {
                provider_blocks.push_str(&format!("\n[[providers]]\nkind = \"{kind}\"\n"));
            }
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

/// Pull the existing `[[providers]]` blocks out of the text, grouped by
/// canonicalised kind (apple/icloud → caldav, etc.) and kept in file order.
/// A kind maps to *all* its blocks so a re-render preserves multiple
/// same-kind accounts (a hand-added second Outlook survives the wizard),
/// each retaining its `account` label and `calendar_ids`.
fn existing_provider_blocks_by_kind(text: &str) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
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
        // Re-emit each block from the parsed Value so we don't have to
        // line-scan the source for boundaries. Manual emit keeps the output
        // predictable (kind, then account, then calendar_ids) and carries
        // forward the fields that encode user intent.
        let mut block = String::from("[[providers]]\n");
        block.push_str(&format!("kind = \"{canonical}\"\n"));
        if let Some(account) = entry.get("account").and_then(|v| v.as_str()) {
            block.push_str(&format!(
                "account = \"{}\"\n",
                account.replace('"', "\\\"")
            ));
        }
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
        out.entry(canonical.to_string()).or_default().push(block);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wizard::descriptor::WizardValue;

    // A hand-edited calendar.toml with two Outlook accounts: the default
    // plus a hand-added "work" account with its own calendar_ids.
    const TWO_OUTLOOK: &str = "\
poll_interval_secs = 90

[[providers]]
kind = \"outlook\"

[[providers]]
kind = \"outlook\"
account = \"work\"
calendar_ids = [\"AAMk-work-cal\"]
";

    #[test]
    fn multi_account_outlook_survives_wizard_save() {
        let doc: toml::Value = toml::from_str(TWO_OUTLOOK).unwrap();
        let values = load_calendar_from_toml(&doc);

        // The wizard toggle collapses both blocks to a single "outlook" tick…
        match values.get("sources") {
            Some(WizardValue::MultiChoice(items)) => {
                assert_eq!(items, &vec!["outlook".to_string()]);
            }
            other => panic!("expected sources MultiChoice, got {other:?}"),
        }

        // …but a re-render preserves BOTH blocks, including the hand-added
        // work account and its calendar_ids.
        let rendered = render_calendar_toml(&values, Some(TWO_OUTLOOK));
        let parsed: toml::Value = toml::from_str(&rendered).unwrap();
        let providers = parsed
            .get("providers")
            .and_then(|v| v.as_array())
            .expect("providers array");
        assert_eq!(
            providers.len(),
            2,
            "both outlook blocks must survive a wizard save:\n{rendered}"
        );
        let accounts: Vec<Option<&str>> = providers
            .iter()
            .map(|p| p.get("account").and_then(|v| v.as_str()))
            .collect();
        assert!(accounts.contains(&None), "default account block missing");
        assert!(
            accounts.contains(&Some("work")),
            "work account block missing"
        );
        // The work block keeps its calendar_ids.
        let work = providers
            .iter()
            .find(|p| p.get("account").and_then(|v| v.as_str()) == Some("work"))
            .unwrap();
        assert_eq!(
            work.get("calendar_ids")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str()),
            Some("AAMk-work-cal")
        );
        assert_eq!(
            parsed.get("poll_interval_secs").and_then(|v| v.as_integer()),
            Some(90)
        );
    }

    #[test]
    fn unticking_outlook_drops_all_its_accounts() {
        let doc: toml::Value = toml::from_str(TWO_OUTLOOK).unwrap();
        let mut values = load_calendar_from_toml(&doc);
        // User unticks Outlook in the wizard, leaving only local.
        values.insert(
            "sources".into(),
            WizardValue::MultiChoice(vec!["local".into()]),
        );
        let rendered = render_calendar_toml(&values, Some(TWO_OUTLOOK));
        let parsed: toml::Value = toml::from_str(&rendered).unwrap();
        let providers = parsed
            .get("providers")
            .and_then(|v| v.as_array())
            .expect("providers array");
        for p in providers {
            assert_ne!(
                p.get("kind").and_then(|v| v.as_str()),
                Some("outlook"),
                "unticked outlook should leave no outlook blocks:\n{rendered}"
            );
        }
    }

    #[test]
    fn source_label_defaults_to_kind_but_names_account() {
        let default = ProviderEntry {
            kind: ProviderKind::Outlook,
            account: None,
            calendar_ids: vec![],
        };
        assert_eq!(default.source_label(), "outlook");
        assert_eq!(default.account_label(), "default");

        // An explicit account = "default" still colors as the kind.
        let explicit_default = ProviderEntry {
            kind: ProviderKind::Outlook,
            account: Some("default".into()),
            calendar_ids: vec![],
        };
        assert_eq!(explicit_default.source_label(), "outlook");

        // A named account is provider-namespaced for its source/color key,
        // but its *token* account label stays the bare string.
        let work = ProviderEntry {
            kind: ProviderKind::Outlook,
            account: Some("work".into()),
            calendar_ids: vec![],
        };
        assert_eq!(work.source_label(), "outlook/work");
        assert_eq!(work.account_label(), "work");

        // Same label under a different provider gets a distinct source.
        let g_work = ProviderEntry {
            kind: ProviderKind::Google,
            account: Some("work".into()),
            calendar_ids: vec![],
        };
        assert_eq!(g_work.source_label(), "google/work");
        assert_ne!(g_work.source_label(), work.source_label());
    }
}
