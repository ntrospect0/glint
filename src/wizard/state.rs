// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! In-flight wizard state. Buffered across pages until the user clicks
//! `Complete and Save` on the confirmation page, at which point we write
//! every collected value into the real TOML files in a single transaction.
//!
//! The state is also serialised to disk after every page advance so a
//! mid-flow `Ctrl+C` can be resumed via the welcome page's `[Resume]`
//! option on the next `glint --setup`. See [`crate::wizard::storage`].

#![allow(dead_code)] // consumed once the TUI driver lands.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::descriptor::WizardValue;

/// Identifies the user's chosen layout. `Preset(name)` looks up one of the
/// hard-coded grids in [`crate::wizard::pages::layout`]; `KeepExisting`
/// means "don't overwrite the layout block in config.toml on Complete" and
/// is the default on re-runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum LayoutChoice {
    /// Named preset (`"two_by_two"`, `"three_column"`, …). Resolved by
    /// the layout-page module to a concrete `LayoutConfig`.
    Preset { name: String },
    /// Carry forward whatever `[layout]` is already in `config.toml`.
    /// Only meaningful on re-run.
    KeepExisting,
}

impl Default for LayoutChoice {
    fn default() -> Self {
        // First-run default — the layout page will surface the actual
        // preset list and let the user pick. Picking this as a starting
        // point ensures we never silently keep a stale layout if the
        // user explicitly opted into a wizard refresh.
        Self::Preset {
            name: "two_by_two".into(),
        }
    }
}

/// One cell-to-widget assignment recorded on the Assign page.
///
/// `cell_index` matches the position of the cell in the resolved layout's
/// `cells` vector. `kind` is the widget kind from the registry (`"clock"`,
/// `"stocks"`); `instance` is `"main"` by default or a user-chosen suffix
/// for multi-instance setups (`"home"`, `"office"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellAssignment {
    pub cell_index: usize,
    pub kind: String,
    pub instance: String,
    /// Stack children when this cell is a stack. Empty for
    /// single-widget cells. Stored as `(kind, instance)` pairs so the
    /// finalize path can emit `widgets = ["clock", "weather@home"]`.
    /// Per spec §1, contains 2–3 entries when non-empty; a
    /// single-element list is collapsed to a non-stack cell at
    /// commit time.
    #[serde(default)]
    pub stack_children: Vec<StackChild>,
}

/// One entry in a stack cell's `widgets` array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackChild {
    pub kind: String,
    pub instance: String,
}

impl StackChild {
    pub fn widget_id(&self) -> String {
        if self.instance == "main" {
            self.kind.clone()
        } else {
            format!("{}@{}", self.kind, self.instance)
        }
    }
}

impl CellAssignment {
    /// Canonical widget id (`"clock"` or `"clock@home"`) used as the key
    /// into [`WizardState::widget_values`]. For stack cells this is
    /// `stack:<child1>+<child2>+...` matching `GridCell::render_target_id`.
    pub fn widget_id(&self) -> String {
        if !self.stack_children.is_empty() {
            let joined = self
                .stack_children
                .iter()
                .map(|c| c.widget_id())
                .collect::<Vec<_>>()
                .join("+");
            return format!("stack:{joined}");
        }
        if self.instance == "main" {
            self.kind.clone()
        } else {
            format!("{}@{}", self.kind, self.instance)
        }
    }

    pub fn is_stack(&self) -> bool {
        self.stack_children.len() >= 2
    }
}

/// Tracks the outcome of an OAuth step for a single provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AuthStatus {
    /// User authorized in the wizard; the auth registry's flow wrote the
    /// token to credentials/. The `provider` name is the key.
    Authorized,
    /// User picked `Skip — set up later`. The confirmation page surfaces
    /// the command to run (`glint --auth <provider>`).
    Deferred,
    /// Flow ran but failed. The user can retry; if they Skip after a
    /// failure we record the last error message for the confirmation
    /// page to display.
    Failed { message: String },
}

/// The full wizard buffer — everything the user has answered up to (but
/// not including) the Complete-and-Save step. Serialised to disk as
/// `~/.config/glint/.wizard_state.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WizardState {
    /// Schema version of this state file. Bump when we make incompatible
    /// changes; loaders fall back to "start fresh" on mismatch so a stale
    /// state file never corrupts a real install.
    #[serde(default = "default_state_version")]
    pub version: u32,

    /// Global config values keyed by [`crate::wizard::pages::global`]
    /// field keys (theme, mouse_scroll, log_level, llm_api_key, …).
    #[serde(default)]
    pub global: HashMap<String, WizardValue>,

    /// Layout choice for the run.
    #[serde(default)]
    pub layout: LayoutChoice,

    /// Cell → widget mapping. Populated on the Assign page.
    #[serde(default)]
    pub assignments: Vec<CellAssignment>,

    /// Per-widget field values, keyed by widget id (`"clock"`,
    /// `"clock@home"`).
    #[serde(default)]
    pub widget_values: HashMap<String, HashMap<String, WizardValue>>,

    /// OAuth status per provider name (`"google"`, `"microsoft"`).
    #[serde(default)]
    pub auth_status: HashMap<String, AuthStatus>,

    /// Page indices the user has completed at least once. Used for the
    /// progress indicator and to enable Back navigation.
    #[serde(default)]
    pub completed_pages: Vec<String>,

    /// Stable identifier for the page the user was on at last save —
    /// resume jumps directly there. `None` ⇒ no resume point.
    #[serde(default)]
    pub last_page: Option<String>,
}

fn default_state_version() -> u32 {
    1
}

// Manual `Default` so a freshly-constructed `WizardState` already
// carries the current schema version. The derived `Default` produced
// `version: 0`, which `storage::load` would then reject as a version
// mismatch.
impl Default for WizardState {
    fn default() -> Self {
        Self {
            version: default_state_version(),
            global: HashMap::new(),
            layout: LayoutChoice::default(),
            assignments: Vec::new(),
            widget_values: HashMap::new(),
            auth_status: HashMap::new(),
            completed_pages: Vec::new(),
            last_page: None,
        }
    }
}

impl WizardState {
    /// Fetch a global config value by key.
    pub fn global_get(&self, key: &str) -> Option<&WizardValue> {
        self.global.get(key)
    }

    /// Set a global config value by key.
    pub fn global_set(&mut self, key: &str, value: WizardValue) {
        self.global.insert(key.to_string(), value);
    }

    /// Fetch a per-widget value by widget id + field key.
    pub fn widget_get(&self, widget_id: &str, key: &str) -> Option<&WizardValue> {
        self.widget_values.get(widget_id)?.get(key)
    }

    /// Set a per-widget value, creating the inner map if absent.
    pub fn widget_set(&mut self, widget_id: &str, key: &str, value: WizardValue) {
        self.widget_values
            .entry(widget_id.to_string())
            .or_default()
            .insert(key.to_string(), value);
    }

    /// Mark a page as completed (idempotent — duplicates are skipped so the
    /// vec doubles as an ordered "history" without bloating).
    pub fn mark_completed(&mut self, page_id: &str) {
        if !self.completed_pages.iter().any(|p| p == page_id) {
            self.completed_pages.push(page_id.to_string());
        }
    }

    pub fn is_completed(&self, page_id: &str) -> bool {
        self.completed_pages.iter().any(|p| p == page_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_assignment_widget_id_handles_main_vs_named() {
        let main = CellAssignment {
            cell_index: 0,
            kind: "clock".into(),
            instance: "main".into(),
            stack_children: Vec::new(),
        };
        assert_eq!(main.widget_id(), "clock");

        let named = CellAssignment {
            cell_index: 1,
            kind: "clock".into(),
            instance: "home".into(),
            stack_children: Vec::new(),
        };
        assert_eq!(named.widget_id(), "clock@home");
    }

    #[test]
    fn cell_assignment_with_stack_children_yields_stack_widget_id() {
        let stack = CellAssignment {
            cell_index: 0,
            kind: String::new(),
            instance: "main".into(),
            stack_children: vec![
                StackChild { kind: "clock".into(), instance: "main".into() },
                StackChild { kind: "weather".into(), instance: "main".into() },
            ],
        };
        assert!(stack.is_stack());
        assert_eq!(stack.widget_id(), "stack:clock+weather");
    }

    #[test]
    fn widget_get_set_round_trips() {
        let mut st = WizardState::default();
        st.widget_set("clock", "hour_format", WizardValue::Choice("24h".into()));
        assert_eq!(
            st.widget_get("clock", "hour_format"),
            Some(&WizardValue::Choice("24h".into()))
        );
        assert_eq!(st.widget_get("clock", "missing"), None);
        assert_eq!(st.widget_get("missing-widget", "key"), None);
    }

    #[test]
    fn global_get_set_round_trips() {
        let mut st = WizardState::default();
        st.global_set("theme", WizardValue::Choice("nord".into()));
        assert_eq!(
            st.global_get("theme"),
            Some(&WizardValue::Choice("nord".into()))
        );
    }

    #[test]
    fn mark_completed_is_idempotent() {
        let mut st = WizardState::default();
        st.mark_completed("layout");
        st.mark_completed("layout");
        st.mark_completed("global");
        assert_eq!(st.completed_pages, vec!["layout", "global"]);
        assert!(st.is_completed("layout"));
        assert!(!st.is_completed("confirm"));
    }
}
