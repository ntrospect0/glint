//! Wizard descriptor — the declarative contract each widget exports so the
//! wizard driver can render its setup page without hard-coded per-widget
//! logic.
//!
//! Widgets attach a `wizard: fn() -> WizardDescriptor` field to their
//! `WidgetDescriptor` (see `widgets::registry`). The wizard's generic
//! per-widget page reads the returned descriptor and renders one input per
//! [`WizardField`].

// Constructors for many of these types are exercised only through the
// wizard driver, which the compiler can't see from this module alone.
#![allow(dead_code)]

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Full description of one widget's setup page.
#[derive(Debug, Clone)]
pub struct WizardDescriptor {
    /// Display name shown in the page header (e.g. `"Clock"`, `"News"`).
    pub display_name: &'static str,

    /// One-sentence summary rendered under the page title. Should explain
    /// what the widget does and link to the relevant config-file follow-ups.
    pub blurb: &'static str,

    /// Ordered list of inputs presented to the user.
    pub fields: Vec<WizardField>,

    /// Hydrate the wizard's value map from a widget's existing TOML on
    /// re-run. Receives the parsed TOML document; returns the values to
    /// pre-populate into the wizard's state. `None` ⇒ the wizard pulls
    /// scalar values straight from the TOML using each field's `key`.
    pub load_from_toml: Option<fn(&toml::Value) -> HashMap<String, WizardValue>>,

    /// Custom TOML body renderer. Called at finalize time with the
    /// user's answers (keyed by `WizardField.key`) and the existing
    /// on-disk file contents (`None` on first install). Returns the
    /// full TOML body to write. Widgets that wholly own their schema
    /// can ignore the second argument; widgets with structured arrays
    /// the wizard doesn't manage (e.g. news's `[[feeds]]`) should merge
    /// into the existing text so hand-edits and bulk-data blocks
    /// survive `--setup` re-runs. `None` ⇒ finalizer falls back to a
    /// field-by-field default that emits one top-level assignment per
    /// field — sufficient for widgets with flat config.
    pub render_toml: Option<fn(&HashMap<String, WizardValue>, Option<&str>) -> String>,
}

/// One input on a widget's setup page.
#[derive(Debug, Clone)]
pub struct WizardField {
    /// TOML key written when the wizard finalises. Also the lookup key into
    /// the per-widget value map in `WizardState`.
    pub key: &'static str,

    /// Label displayed above (or beside) the input.
    pub label: &'static str,

    /// 1–3 line explainer rendered below the field while it has focus.
    /// Should describe what the parameter means and how it changes the
    /// widget's behaviour — not just restate the field name.
    pub help: &'static str,

    /// `true` ⇒ Save+Continue is blocked until the user provides a value.
    /// Required fields without a value at completion time also block the
    /// final Confirm page.
    pub required: bool,

    /// Input type and its associated defaults / options.
    pub kind: WizardFieldKind,

    /// Optional cross-validator. Runs on every value change. Returning
    /// `Err(msg)` renders `msg` as an inline error and prevents
    /// Save+Continue until corrected. Pure type/format checks (e.g.
    /// "must be a number") are handled by the field kind itself; this slot
    /// is for higher-level invariants (e.g. "latitude must be between
    /// -90 and 90", "API key must start with sk-ant-").
    pub validate: Option<fn(&WizardValue) -> Result<(), String>>,
}

/// Supported input types. Each variant carries the metadata the renderer
/// needs to draw a fully self-describing input.
#[derive(Debug, Clone)]
pub enum WizardFieldKind {
    /// Single-line free-form text.
    Text {
        default: Option<String>,
        placeholder: Option<&'static str>,
    },

    /// Numeric input. `integer = true` rejects decimal points; `range`
    /// bounds the accepted value (inclusive on both ends).
    Number {
        default: Option<f64>,
        range: Option<(f64, f64)>,
        integer: bool,
    },

    /// Yes / No toggle.
    Bool { default: bool },

    /// Single-select from a list of options (radio-button semantics).
    Choice {
        options: Vec<ChoiceOption>,
        default: Option<&'static str>,
    },

    /// Multi-select from a list of options (checkbox semantics). Defaults
    /// list the options that start out checked — used for the
    /// "ship-with-defaults, let the user deselect" pattern (news feeds,
    /// stock indices, etc.).
    MultiChoice {
        options: Vec<ChoiceOption>,
        defaults: Vec<&'static str>,
    },

    /// Free-form text list. The wizard splits the user's input on
    /// `separator` characters (typically newlines or commas) and trims each
    /// element. No hard validation — invalid entries fail at widget load
    /// time and the user can fix them in the TOML.
    TextList {
        default: Vec<String>,
        separator: Separator,
    },

    /// Filesystem path. `mode` is a hint to the renderer / validator about
    /// whether to accept a single file, a directory, or a glob pattern.
    Path {
        mode: PathMode,
        default: Option<String>,
    },

    /// OAuth provider. The wizard inserts a synthetic OAuth page after the
    /// widget's main page when this field is present and selected.
    OAuth { provider: &'static str },

    /// Multi-select whose options aren't known at compile time — they're
    /// fetched at runtime (e.g. Gmail labels, Outlook folders, calendar
    /// IDs from the user's account) and stashed in
    /// [`crate::wizard::app::WizardApp::remote_options`] keyed by `source`.
    /// Stored as `WizardValue::MultiChoice` so the existing
    /// load/render/finalize plumbing works without a parallel value type.
    ///
    /// `defaults` is consulted on the first render when no options are
    /// cached yet — handy for falling back to "INBOX" while a fetch is
    /// in flight or before authorization.
    RemoteMultiChoice {
        source: &'static str,
        defaults: Vec<&'static str>,
    },

    /// Single-select from a large option list. Same result as `Choice` but
    /// the renderer drops a type-to-filter dropdown instead of an
    /// inline ←/→ cycler — appropriate when the options run into the
    /// hundreds (IANA timezones, country codes, color names). The value is
    /// stored as `WizardValue::Choice(String)`.
    ///
    /// `options` carries `(value, label)` pairs. `value` is what lands in
    /// the TOML; `label` is what the user reads (often identical). When
    /// `allow_blank` is `true`, the dropdown leads with a `blank_label`
    /// entry whose value is the empty string — useful for optional fields
    /// like "(system local time)".
    Lookup {
        options: Vec<(&'static str, &'static str)>,
        default: Option<&'static str>,
        allow_blank: bool,
        blank_label: &'static str,
    },
}

#[derive(Debug, Clone)]
pub struct ChoiceOption {
    /// Value written to TOML when selected.
    pub value: &'static str,
    /// User-visible label.
    pub label: &'static str,
    /// Optional per-option help rendered when this option is focused.
    pub help: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Separator {
    /// Each line is one list entry.
    Newline,
    /// Commas split entries; whitespace around each entry is trimmed.
    Comma,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathMode {
    File,
    Dir,
    Glob,
}

/// Runtime value held by the wizard for one field. Mirrors `WizardFieldKind`
/// but carries the user's actual input rather than the field schema.
///
/// `Serialize`/`Deserialize` so the wizard's in-flight state can persist to
/// `.wizard_state.toml` across runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum WizardValue {
    Text(String),
    Number(f64),
    Bool(bool),
    Choice(String),
    MultiChoice(Vec<String>),
    TextList(Vec<String>),
    Path(String),
}

impl WizardValue {
    /// `true` when the user hasn't supplied any content (used to enforce
    /// `required`). Empty strings, empty lists, and the multi-choice all-off
    /// state all count as empty; numbers + bools never do.
    pub fn is_empty(&self) -> bool {
        match self {
            WizardValue::Text(s) | WizardValue::Choice(s) | WizardValue::Path(s) => {
                s.trim().is_empty()
            }
            WizardValue::MultiChoice(v) | WizardValue::TextList(v) => v.is_empty(),
            WizardValue::Number(_) | WizardValue::Bool(_) => false,
        }
    }
}

impl WizardFieldKind {
    /// Produce the initial `WizardValue` for this field when no existing
    /// value is loaded from TOML.
    pub fn initial_value(&self) -> WizardValue {
        match self {
            WizardFieldKind::Text { default, .. } => {
                WizardValue::Text(default.clone().unwrap_or_default())
            }
            WizardFieldKind::Number { default, .. } => {
                WizardValue::Number(default.unwrap_or(0.0))
            }
            WizardFieldKind::Bool { default } => WizardValue::Bool(*default),
            WizardFieldKind::Choice { default, .. } => {
                WizardValue::Choice(default.map(str::to_string).unwrap_or_default())
            }
            WizardFieldKind::MultiChoice { defaults, .. } => {
                WizardValue::MultiChoice(defaults.iter().map(|s| s.to_string()).collect())
            }
            WizardFieldKind::TextList { default, .. } => WizardValue::TextList(default.clone()),
            WizardFieldKind::Path { default, .. } => {
                WizardValue::Path(default.clone().unwrap_or_default())
            }
            // OAuth values aren't really values — the OAuth page tracks
            // status separately. We park a placeholder here.
            WizardFieldKind::OAuth { provider } => WizardValue::Choice((*provider).to_string()),
            WizardFieldKind::Lookup { default, .. } => {
                WizardValue::Choice(default.map(str::to_string).unwrap_or_default())
            }
            WizardFieldKind::RemoteMultiChoice { defaults, .. } => {
                WizardValue::MultiChoice(defaults.iter().map(|s| s.to_string()).collect())
            }
        }
    }
}

/// Builder convenience: an empty descriptor for widgets that haven't yet
/// been migrated to the wizard schema. Renders a "deferred to TOML" page
/// pointing the user at the widget's config file.
pub const fn defer_to_toml_descriptor(
    display_name: &'static str,
    blurb: &'static str,
) -> WizardDescriptor {
    WizardDescriptor {
        display_name,
        blurb,
        fields: Vec::new(),
        load_from_toml: None,
        render_toml: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_value_uses_defaults() {
        let f = WizardFieldKind::Text {
            default: Some("hi".into()),
            placeholder: None,
        };
        assert_eq!(f.initial_value(), WizardValue::Text("hi".into()));

        let f = WizardFieldKind::Number {
            default: Some(42.0),
            range: None,
            integer: true,
        };
        assert_eq!(f.initial_value(), WizardValue::Number(42.0));

        let f = WizardFieldKind::Bool { default: true };
        assert_eq!(f.initial_value(), WizardValue::Bool(true));

        let f = WizardFieldKind::Choice {
            options: vec![],
            default: Some("x"),
        };
        assert_eq!(f.initial_value(), WizardValue::Choice("x".into()));

        let f = WizardFieldKind::MultiChoice {
            options: vec![],
            defaults: vec!["a", "b"],
        };
        assert_eq!(
            f.initial_value(),
            WizardValue::MultiChoice(vec!["a".into(), "b".into()])
        );
    }

    #[test]
    fn is_empty_recognises_blank_values() {
        assert!(WizardValue::Text("".into()).is_empty());
        assert!(WizardValue::Text("   ".into()).is_empty());
        assert!(!WizardValue::Text("ok".into()).is_empty());

        assert!(WizardValue::MultiChoice(vec![]).is_empty());
        assert!(!WizardValue::MultiChoice(vec!["x".into()]).is_empty());

        assert!(WizardValue::TextList(vec![]).is_empty());
        assert!(!WizardValue::TextList(vec!["x".into()]).is_empty());

        // Numbers / bools always count as supplied — the user picked them
        // by accepting the default if nothing else.
        assert!(!WizardValue::Number(0.0).is_empty());
        assert!(!WizardValue::Bool(false).is_empty());
    }

    #[test]
    fn defer_to_toml_descriptor_has_no_fields() {
        let d = defer_to_toml_descriptor("Foo", "blurb");
        assert_eq!(d.display_name, "Foo");
        assert!(d.fields.is_empty());
    }
}
