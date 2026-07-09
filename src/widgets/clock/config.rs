// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Configuration schema for the clock widget — TOML on-disk shape,
//! defaults, the wizard descriptor, and the wizard-side TOML
//! render/load helpers. No render/state code here; this is the
//! data-and-schema layer the rest of the widget reads from.

use serde::Deserialize;

use crate::text::toml_quote;
use crate::theme::ColorScheme;
use crate::ui::big_digits;

/// Loaded from `~/.config/glint/clock.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct ClockConfig {
    /// IANA timezone for the primary clock. `None` = system local time.
    #[serde(default)]
    pub timezone: Option<String>,

    #[serde(default)]
    pub show_seconds: bool,

    /// Small ticking `HH:MM:SS` line below the big digits.
    #[serde(default = "default_show_seconds_ticker")]
    pub show_seconds_ticker: bool,

    #[serde(default = "default_show_date")]
    pub show_date: bool,

    /// `"12h"` or `"24h"`.
    #[serde(
        default = "default_hour_format",
        deserialize_with = "deserialize_hour_format"
    )]
    pub hour_format: u8,

    /// World clocks rendered below the primary display when the cell is tall enough.
    #[serde(default)]
    pub secondary_timezones: Vec<SecondaryTimezone>,

    /// Big-digit gradient style. `g` cycles at runtime.
    #[serde(default)]
    pub gradient: big_digits::Gradient,

    /// Per-widget overrides layered on the app theme.
    #[serde(default)]
    pub colors: ColorScheme,

    /// `Shift+<letter>` focus shortcuts; falls back to `['c', 'l', 'o', 'k']`.
    #[serde(default)]
    pub shortcuts: Vec<char>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SecondaryTimezone {
    pub label: String,
    /// IANA timezone identifier (e.g. `"America/New_York"`).
    pub timezone: String,
}

fn default_show_seconds_ticker() -> bool {
    true
}
fn default_show_date() -> bool {
    true
}
fn default_hour_format() -> u8 {
    24
}

/// Parse `"12h"` / `"24h"` into the corresponding integer.
fn deserialize_hour_format<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let s = String::deserialize(deserializer)?;
    match s.trim().to_lowercase().as_str() {
        "12h" => Ok(12),
        "24h" => Ok(24),
        other => Err(D::Error::custom(format!(
            "unknown hour_format {other:?}, expected \"12h\" or \"24h\""
        ))),
    }
}

impl Default for ClockConfig {
    fn default() -> Self {
        Self {
            timezone: None,
            show_seconds: false,
            show_seconds_ticker: default_show_seconds_ticker(),
            show_date: default_show_date(),
            hour_format: default_hour_format(),
            secondary_timezones: Vec::new(),
            gradient: big_digits::Gradient::default(),
            colors: ColorScheme::default(),
            shortcuts: Vec::new(),
        }
    }
}

/// Registry kind string for the clock widget. Single source of truth — used
/// by the widget descriptor, the config file resolver, and the wizard.
pub const KIND: &str = "clock";

/// Wizard descriptor for the clock widget. Serves as the reference
/// implementation other widgets follow when they migrate from
/// `defer_to_toml_descriptor` to a real schema.
pub fn wizard_descriptor() -> crate::wizard::descriptor::WizardDescriptor {
    use crate::wizard::descriptor::{ChoiceOption, WizardDescriptor, WizardField, WizardFieldKind};

    // Helper for the three optional secondary-timezone fields. Each is a
    // Lookup over the same IANA list with allow_blank so the user can
    // leave any slot empty.
    fn secondary_field(key: &'static str, label: &'static str) -> WizardField {
        WizardField {
            key,
            label,
            help: "Optional. Type to filter the IANA zone list (e.g. \
                   \"tokyo\", \"london\"). Space picks the highlighted row; \
                   Tab moves to the next field. Pick \"(none)\" to skip this \
                   slot. For more than three world clocks, hand-edit \
                   [[secondary_timezones]] in clock.toml after setup.",
            required: false,
            kind: WizardFieldKind::Lookup {
                options: iana_timezone_options(),
                default: None,
                allow_blank: true,
                blank_label: "(none)",
            },
            validate: None,
        }
    }

    WizardDescriptor {
        display_name: "Clock",
        blurb: "Time display with optional secondary world clocks. The wizard \
                covers the basics; gradient styles and additional secondary \
                zones live in clock.toml for hand-tuning.",
        load_from_toml: Some(load_clock_from_toml),
        render_toml: Some(render_clock_toml),
        fields: vec![
            WizardField {
                key: "timezone",
                label: "Primary timezone",
                help: "Type to filter (e.g. \"vancouver\", \"tokyo\"). ↑/↓ \
                       navigates; PgUp/PgDn jumps by 10. Space picks the \
                       highlighted row. Pick \"(system local time)\" to \
                       follow the host clock.",
                required: false,
                kind: WizardFieldKind::Lookup {
                    options: iana_timezone_options(),
                    default: None,
                    allow_blank: true,
                    blank_label: "(system local time)",
                },
                validate: None,
            },
            WizardField {
                key: "hour_format",
                label: "Hour format",
                help: "\"12h\" — am/pm. \"24h\" — military time.",
                required: true,
                kind: WizardFieldKind::Choice {
                    options: vec![
                        ChoiceOption {
                            value: "24h",
                            label: "24-hour",
                            help: None,
                        },
                        ChoiceOption {
                            value: "12h",
                            label: "12-hour (am/pm)",
                            help: None,
                        },
                    ],
                    default: Some("24h"),
                },
                validate: None,
            },
            WizardField {
                key: "show_seconds",
                label: "Show seconds in the big digits",
                help: "Adds :SS to the block-digit display. The small ticking \
                       line below the big digits always shows seconds.",
                required: false,
                kind: WizardFieldKind::Bool { default: false },
                validate: None,
            },
            WizardField {
                key: "show_date",
                label: "Show the date row",
                help: "Renders today's date under the big digits.",
                required: false,
                kind: WizardFieldKind::Bool { default: true },
                validate: None,
            },
            secondary_field("secondary_tz_1", "Secondary world clock 1"),
            secondary_field("secondary_tz_2", "Secondary world clock 2"),
            secondary_field("secondary_tz_3", "Secondary world clock 3"),
        ],
    }
}

/// Every IANA zone the host's chrono-tz database knows about, formatted as
/// `(value, label)` pairs for the wizard's `Lookup` dropdown. Both halves
/// of each tuple are the canonical name (`"America/Los_Angeles"`) — the
/// dropdown's filter matches against the label, which means the user can
/// type either the continent or the city.
fn iana_timezone_options() -> Vec<(&'static str, &'static str)> {
    chrono_tz::TZ_VARIANTS
        .iter()
        .map(|tz| {
            let name = tz.name();
            (name, name)
        })
        .collect()
}

/// Render the clock widget's TOML from wizard values. We render
/// `secondary_timezones` as repeated `[[secondary_timezones]]` tables to
/// match the existing `ClockConfig` deserialiser; labels are derived from
/// the city portion of the IANA name.
pub(super) fn render_clock_toml(
    values: &std::collections::HashMap<String, crate::wizard::descriptor::WizardValue>,
    _existing: Option<&str>,
) -> String {
    use crate::wizard::descriptor::WizardValue;
    let mut out = String::new();
    out.push_str(
        "# Generated by `glint --setup`. Hand-edit freely; the wizard\n\
         # preserves advanced keys it doesn't manage (e.g. [colors], gradient).\n\n",
    );
    // Timezone field is a Lookup → WizardValue::Choice; accept Text
    // as a fallback in case a custom descriptor wires it differently.
    let tz = match values.get("timezone") {
        Some(WizardValue::Choice(s)) | Some(WizardValue::Text(s)) => s.trim(),
        _ => "",
    };
    if !tz.is_empty() {
        out.push_str(&format!("timezone = {}\n", toml_quote(tz)));
    }
    if let Some(WizardValue::Choice(hf)) = values.get("hour_format") {
        out.push_str(&format!("hour_format = {}\n", toml_quote(hf)));
    }
    if let Some(WizardValue::Bool(b)) = values.get("show_seconds") {
        out.push_str(&format!("show_seconds = {b}\n"));
    }
    if let Some(WizardValue::Bool(b)) = values.get("show_date") {
        out.push_str(&format!("show_date = {b}\n"));
    }
    out.push_str("show_seconds_ticker = true\n");

    // Up to three optional secondary world clocks, each in its own Lookup
    // field. Empty / unset slots are skipped; the user reaches for clock.toml
    // directly when they want more than three.
    for key in ["secondary_tz_1", "secondary_tz_2", "secondary_tz_3"] {
        let zone = match values.get(key) {
            Some(WizardValue::Choice(s)) | Some(WizardValue::Text(s)) => s.trim(),
            _ => "",
        };
        if zone.is_empty() {
            continue;
        }
        let label = label_from_iana_zone(zone);
        out.push_str("\n[[secondary_timezones]]\n");
        out.push_str(&format!("label = {}\n", toml_quote(&label)));
        out.push_str(&format!("timezone = {}\n", toml_quote(zone)));
    }
    out
}

/// Derive a friendly label from an IANA zone like `"America/New_York"` →
/// `"New York"`. Falls back to the full zone when there's no `/`.
pub(super) fn label_from_iana_zone(zone: &str) -> String {
    let tail = zone.rsplit('/').next().unwrap_or(zone);
    tail.replace('_', " ")
}

/// Inverse of [`render_clock_toml`]: parse a clock TOML and surface the
/// scalar fields plus the first three `[[secondary_timezones]]` entries
/// into the wizard's three Lookup slots. Additional entries beyond the
/// third are intentionally ignored — the user can hand-edit clock.toml
/// for more — and the wizard's render path will preserve only the three
/// it knows about, so users with custom clocks should not lose them
/// silently. (Hydration only seeds keys; the user is then expected to
/// confirm and re-finalize through the wizard.)
fn load_clock_from_toml(
    doc: &toml::Value,
) -> std::collections::HashMap<String, crate::wizard::descriptor::WizardValue> {
    use crate::wizard::descriptor::WizardValue;
    let mut out = std::collections::HashMap::new();
    if let Some(s) = doc.get("timezone").and_then(|v| v.as_str()) {
        out.insert("timezone".into(), WizardValue::Choice(s.into()));
    }
    if let Some(s) = doc.get("hour_format").and_then(|v| v.as_str()) {
        out.insert("hour_format".into(), WizardValue::Choice(s.into()));
    }
    if let Some(b) = doc.get("show_seconds").and_then(|v| v.as_bool()) {
        out.insert("show_seconds".into(), WizardValue::Bool(b));
    }
    if let Some(b) = doc.get("show_date").and_then(|v| v.as_bool()) {
        out.insert("show_date".into(), WizardValue::Bool(b));
    }
    if let Some(arr) = doc.get("secondary_timezones").and_then(|v| v.as_array()) {
        for (i, entry) in arr.iter().take(3).enumerate() {
            let Some(zone) = entry.get("timezone").and_then(|v| v.as_str()) else {
                continue;
            };
            let key = match i {
                0 => "secondary_tz_1",
                1 => "secondary_tz_2",
                _ => "secondary_tz_3",
            };
            out.insert(key.into(), WizardValue::Choice(zone.into()));
        }
    }
    out
}
