//! Widget registry — the single source of truth for which widgets exist.
//!
//! ## Adding a widget
//!
//! 1. Implement [`Widget`] under `src/widgets/<name>/`.
//! 2. Export `pub const KIND: &str` and `pub fn build(&WidgetCtx) -> Box<dyn Widget>`.
//! 3. Add a `widget-<name>` feature in `Cargo.toml` and gate the module in
//!    `widgets::mod` on it.
//! 4. Append a `WidgetDescriptor` to [`WIDGETS`] below.
//!
//! No edits to `app.rs`, `main.rs`, or the wizard driver required.
//! Registration, first-run defaults, and auth prompts all walk `WIDGETS`.

use crate::auth::registry::AuthRequirement;
use crate::wizard::descriptor::WizardDescriptor;

use super::{Widget, WidgetCtx, WidgetFactory};

/// Static description of a widget kind.
pub struct WidgetDescriptor {
    /// Stable kind string used in `layout.toml` cells and `<kind>.toml`
    /// config filenames. Must match the widget module's `KIND` constant.
    pub kind: &'static str,

    /// Factory that reads the widget's TOML and constructs an instance.
    pub factory: WidgetFactory,

    /// Whether this widget appears in the empty-layout fallback grid. Set
    /// to `false` for auxiliary widgets that the user should opt into via
    /// the wizard or by editing `config.toml`.
    pub default_in_first_run: bool,

    /// OAuth providers this widget may call. Widgets with multiple backends
    /// (calendar's google / microsoft / caldav / local) list every provider
    /// they could need; the user picks one at wizard time. Fully offline
    /// widgets leave this empty.
    #[allow(dead_code)] // surfaced by the wizard's auth-prompt step.
    pub auth_requirements: &'static [AuthRequirement],

    /// Wizard descriptor — the declarative setup-page schema the wizard
    /// driver uses to render this widget's configuration UI. Widgets that
    /// haven't been migrated to the schema yet return a `defer_to_toml`
    /// descriptor that points the user at the widget's TOML file.
    #[allow(dead_code)] // consumed by the wizard driver.
    pub wizard: fn() -> WizardDescriptor,
}

/// The full set of widgets compiled into this build. Order is significant
/// — it sets the empty-layout fallback registration order and the wizard's
/// step ordering.
pub const WIDGETS: &[WidgetDescriptor] = &[
    #[cfg(feature = "widget-stocks")]
    WidgetDescriptor {
        kind: super::stocks::KIND,
        factory: super::stocks::build,
        default_in_first_run: true,
        auth_requirements: &[],
        wizard: super::stocks::wizard_descriptor,
    },
    #[cfg(feature = "widget-forex")]
    WidgetDescriptor {
        kind: super::forex::KIND,
        factory: super::forex::build,
        default_in_first_run: false,
        auth_requirements: &[],
        wizard: super::forex::wizard_descriptor,
    },
    #[cfg(feature = "widget-clock")]
    WidgetDescriptor {
        kind: super::clock::KIND,
        factory: super::clock::build,
        default_in_first_run: true,
        auth_requirements: &[],
        wizard: super::clock::wizard_descriptor,
    },
    #[cfg(feature = "widget-weather")]
    WidgetDescriptor {
        kind: super::weather::KIND,
        factory: super::weather::build,
        default_in_first_run: true,
        auth_requirements: &[],
        wizard: super::weather::wizard_descriptor,
    },
    #[cfg(feature = "widget-calendar")]
    WidgetDescriptor {
        kind: super::calendar::KIND,
        factory: super::calendar::build,
        default_in_first_run: true,
        auth_requirements: &[
            AuthRequirement {
                provider: "google",
                scope_hints: &["calendar.readonly"],
            },
            AuthRequirement {
                provider: "microsoft",
                scope_hints: &["Calendars.Read"],
            },
        ],
        wizard: super::calendar::wizard_descriptor,
    },
    #[cfg(feature = "widget-news")]
    WidgetDescriptor {
        kind: super::news::KIND,
        factory: super::news::build,
        default_in_first_run: true,
        auth_requirements: &[],
        wizard: super::news::wizard_descriptor,
    },
    #[cfg(feature = "widget-email")]
    WidgetDescriptor {
        kind: super::email::KIND,
        factory: super::email::build,
        default_in_first_run: false,
        auth_requirements: &[
            AuthRequirement {
                provider: "google",
                scope_hints: &["gmail.readonly"],
            },
            AuthRequirement {
                provider: "microsoft",
                scope_hints: &["Mail.Read"],
            },
        ],
        wizard: super::email::wizard_descriptor,
    },
    #[cfg(feature = "widget-resources")]
    WidgetDescriptor {
        kind: super::resources::KIND,
        factory: super::resources::build,
        default_in_first_run: false,
        auth_requirements: &[],
        wizard: super::resources::wizard_descriptor,
    },
    #[cfg(feature = "widget-gallery")]
    WidgetDescriptor {
        kind: super::gallery::KIND,
        factory: super::gallery::build,
        default_in_first_run: false,
        auth_requirements: &[],
        wizard: super::gallery::wizard_descriptor,
    },
    #[cfg(feature = "widget-sticky")]
    WidgetDescriptor {
        kind: super::sticky::KIND,
        factory: super::sticky::build,
        default_in_first_run: false,
        auth_requirements: &[],
        wizard: super::sticky::wizard_descriptor,
    },
];

/// Look up a widget descriptor by kind string. `None` when the kind isn't
/// compiled in or doesn't exist.
pub fn find(kind: &str) -> Option<&'static WidgetDescriptor> {
    WIDGETS.iter().find(|d| d.kind == kind)
}

/// Build a widget for `(kind, instance)` via the registry. `make_ctx`
/// produces the [`WidgetCtx`] stamped with the supplied instance. Returns
/// `None` for unknown kinds so callers can warn and skip on layout typos.
pub fn build_for(
    kind: &str,
    instance: &str,
    make_ctx: impl FnOnce(String) -> WidgetCtx,
) -> Option<Box<dyn Widget>> {
    let desc = find(kind)?;
    let ctx = make_ctx(instance.to_string());
    Some((desc.factory)(&ctx))
}

/// Kinds that seed the empty-layout fallback grid.
pub fn default_kinds() -> impl Iterator<Item = &'static str> {
    WIDGETS
        .iter()
        .filter(|d| d.default_in_first_run)
        .map(|d| d.kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn kinds_are_unique_and_non_empty() {
        let mut seen: HashSet<&'static str> = HashSet::new();
        for desc in WIDGETS {
            assert!(!desc.kind.is_empty(), "widget kind must not be empty");
            assert!(
                seen.insert(desc.kind),
                "duplicate widget kind in registry: {}",
                desc.kind
            );
        }
    }

    #[test]
    fn find_returns_descriptor_for_each_kind() {
        for desc in WIDGETS {
            let found = find(desc.kind).unwrap_or_else(|| panic!("find({}) returned None", desc.kind));
            assert_eq!(found.kind, desc.kind);
        }
        assert!(find("definitely-not-a-real-widget").is_none());
    }

    /// Five-widget smoke test for the default-features dashboard. Mirrors
    /// the seed layout in `config::DEFAULT_CONFIG_TOML` — if either drifts,
    /// the empty-config first-run experience breaks.
    #[cfg(all(
        feature = "widget-clock",
        feature = "widget-weather",
        feature = "widget-calendar",
        feature = "widget-news",
        feature = "widget-stocks",
    ))]
    #[test]
    fn core_widgets_are_present() {
        for kind in ["clock", "weather", "calendar", "news", "stocks"] {
            assert!(find(kind).is_some(), "core widget {kind} missing from registry");
        }
    }
}
