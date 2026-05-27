// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

#[cfg(feature = "widget-calendar")]
pub mod calendar;
#[cfg(feature = "widget-clock")]
pub mod clock;
#[cfg(feature = "widget-email")]
pub mod email;
#[cfg(feature = "widget-forex")]
pub mod forex;
#[cfg(feature = "widget-gallery")]
pub mod gallery;
#[cfg(feature = "widget-news")]
pub mod news;
#[cfg(feature = "widget-notes")]
pub mod notes;
pub mod registry;
#[cfg(feature = "widget-resources")]
pub mod resources;
pub mod stack;
#[cfg(feature = "widget-stocks")]
pub mod stocks;
#[cfg(feature = "widget-weather")]
pub mod weather;
#[cfg(feature = "widget-wsj")]
pub mod wsj;

use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{layout::Rect, Frame};

use crate::cache::ScopedCache;
use crate::llm::LlmProvider;
use crate::theme::Theme;

/// Per-tick context passed to `Widget::update`. Reserved for shared
/// per-tick state (HTTP clients, request budgets, …) once a widget actually
/// needs it — until then it's a stable seam, not a parking lot.
#[derive(Default)]
pub struct AppContext;

/// Construction-time dependencies handed to every widget factory. Bundling
/// these into one struct lets the registry hold all factories as the same
/// function pointer type. Widgets pick the fields they need; a new shared
/// dependency lands here once instead of in every widget's constructor.
pub struct WidgetCtx {
    /// `"main"` for the canonical instance, otherwise the suffix from
    /// `widget@<instance>` in the layout cell.
    pub instance: String,
    pub theme: std::sync::Arc<Theme>,
    /// `None` when llm.toml has `enabled = false` or no API key is on disk.
    /// Widgets that opt into LLM features must handle this case.
    pub llm: Option<std::sync::Arc<dyn LlmProvider>>,
    /// Per-widget persistent cache, already namespaced to `(kind, instance)`.
    /// See `src/cache/mod.rs` for the load/store/invalidate primitives.
    pub cache: ScopedCache,
}

/// The function pointer every widget exposes for the registry.
pub type WidgetFactory = fn(&WidgetCtx) -> Box<dyn Widget>;

/// Result returned by `Widget::handle_key`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventResult {
    /// Widget consumed the event; do not dispatch further.
    Handled,
    /// Widget ignored the event; fall through to global handlers.
    Ignored,
}

/// Parse a layout cell's `widget` field into `(kind, instance)`.
///
/// `clock`           → ("clock", "main")
/// `clock@home`      → ("clock", "home")
/// Empty `instance` after the @ → falls back to "main".
pub fn parse_widget_ref(s: &str) -> (String, String) {
    match s.split_once('@') {
        None => (s.to_string(), "main".to_string()),
        Some((kind, instance)) => {
            let instance = instance.trim();
            if instance.is_empty() {
                (kind.to_string(), "main".to_string())
            } else {
                (kind.to_string(), instance.to_string())
            }
        }
    }
}

/// Returns the per-instance TOML filename (without extension) for a
/// `(kind, instance)` pair.  `("clock","main")` → `"clock"`;
/// `("clock","home")` → `"clock@home"`. Used by `load_widget_toml`.
pub fn widget_config_stem(kind: &str, instance: &str) -> String {
    if instance == "main" {
        kind.to_string()
    } else {
        format!("{kind}@{instance}")
    }
}

#[async_trait]
pub trait Widget: Send + Sync {
    /// Fully-qualified widget id — `"clock"` or `"clock@home"`.
    fn id(&self) -> &str;

    /// Human-readable label rendered in the widget title bar.
    fn display_name(&self) -> &str;

    /// Stable kind string shared by every instance of this widget type
    /// (e.g. `"clock"`, `"stocks"`). Matches the `KIND` constant in the
    /// widget module and the descriptor in `widgets::registry`.
    #[allow(dead_code)] // reserved for per-kind command routing.
    fn kind(&self) -> &str;

    /// Instance suffix; `"main"` is the canonical instance.
    #[allow(dead_code)] // exposed for diagnostics / wizard introspection.
    fn instance(&self) -> &str {
        "main"
    }

    async fn update(&mut self, ctx: &AppContext) -> Result<()>;

    /// Returns whether the widget's displayed state has changed since the
    /// last call, then clears the dirty bit. The main loop ORs this across
    /// every widget on a `Tick` and skips the full `terminal.draw()` when
    /// nothing is dirty — a 250ms tick rate otherwise forces a full layout
    /// + render pass 4×/sec, which is the dominant idle-CPU cost.
    ///
    /// Default returns `true` (always redraw). Widgets opt in to clean
    /// renders by tracking their own `dirty: bool` — set it on data
    /// arrival inside `update()` or anywhere display-relevant state
    /// mutates, then return-and-clear here. Non-`Tick` events (key,
    /// mouse, paste, resize, config change) bypass this gate and always
    /// trigger a redraw, so widgets don't need to mark themselves dirty
    /// from their own `handle_key` / `handle_mouse` paths.
    fn take_dirty(&mut self) -> bool {
        true
    }

    fn render(&self, frame: &mut Frame, area: Rect, focused: bool);

    fn handle_key(&mut self, key: KeyEvent) -> EventResult;

    /// `area` matches the outer `Rect` passed to `render`, so the widget can
    /// reconstruct its internal layout and resolve hit-targets.
    fn handle_mouse(&mut self, _mouse: MouseEvent, _area: Rect) -> EventResult {
        EventResult::Ignored
    }

    /// Bracketed-paste payload from the terminal. The default implementation
    /// ignores it — widgets that own a text buffer (e.g. notes) should
    /// override to insert the full text atomically rather than walking
    /// `handle_key` per char, which would push one undo snapshot and one
    /// disk write per character.
    fn handle_paste(&mut self, _text: &str) -> EventResult {
        EventResult::Ignored
    }

    /// Handle a `:cmd arg1 arg2` from the command bar. Return `Ok(true)` if
    /// the widget claimed the command (focus jumps to it), `Ok(false)` to
    /// fall through to the next widget, or `Err` to surface a user-visible
    /// error in the command-bar feedback line.
    fn handle_command(&mut self, cmd: &str, args: &[&str]) -> Result<bool>;

    /// Serialise the widget's current config for diagnostics. Currently only
    /// used by debug surfaces; widgets can return a partial view.
    #[allow(dead_code)]
    fn config(&self) -> serde_json::Value;

    /// Live-reload entry point. The config watcher hands in the freshly
    /// parsed TOML as JSON; widgets typically `serde_json::from_value` into
    /// their own `Config` struct and rebuild internal state.
    fn apply_config(&mut self, config: serde_json::Value) -> Result<()>;

    /// `(key, description)` pairs surfaced by the `?` help overlay.
    fn keybindings(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }

    /// Called by `:scheme <name>` so palette changes propagate without a
    /// restart. Widgets that paint themed chrome rebuild their merged theme
    /// from the new app theme plus their own `[colors]` overrides.
    fn set_app_theme(&mut self, _theme: Arc<Theme>) {}

    /// Optional read-only view of a widget's polling cadence.
    /// Widgets that periodically fetch data (RSS feeds, network
    /// quotes, weather, mail, …) return
    /// `Some(self.poll.snapshot())` so the platform can observe —
    /// and, eventually, schedule against — their refresh cadence.
    /// Widgets without periodic fetches (notes, gallery, clock)
    /// inherit the `None` default and carry zero state.
    ///
    /// The hook is intentionally read-only: widgets own their own
    /// [`PollTracker`](crate::polling::PollTracker) (typically
    /// inside their state mutex) and call `is_due` / `mark_attempted`
    /// from their own [`update`](Self::update) implementation. The
    /// platform just peeks at "are you currently polling, and on
    /// what cadence?"
    ///
    /// See `docs/widget-sdk.md` § Polling for the recommended usage
    /// pattern.
    #[allow(dead_code)] // forward-looking platform surface
    fn poll_snapshot(&self) -> Option<crate::polling::PollSnapshot> {
        None
    }

    /// Ordered preference list of `Shift+<letter>` shortcut keys. The app
    /// walks widgets in registration order and grants the first letter not
    /// already claimed. Returning `&[]` opts out — the widget stays
    /// reachable via Tab / mouse click. Lifetime is the widget's own borrow
    /// so preferences can be sourced from user config.
    fn shortcut_preferences(&self) -> &[char] {
        &[]
    }

    /// Notification of the letter actually granted by the assignment pass
    /// (or `None` if every preference was taken). Widgets cache this to
    /// paint the highlight inside their title.
    fn set_shortcut(&mut self, _shortcut: Option<char>) {}

    /// The letter actually granted (or `None` if all preferences were
    /// taken). Default returns `None`; widgets that store their
    /// shortcut should override to return their cached field.
    /// Used by composite widgets (stacks) to surface each child's
    /// shortcut in their tab strip without each child having to
    /// expose internal fields.
    fn shortcut(&self) -> Option<char> {
        None
    }

    /// Dynamic suffix that the widget would normally append to its
    /// own title (e.g. "47 articles", "[outlook] alice@example.com").
    /// Returns `None` when the widget has no metadata to surface.
    ///
    /// Used by stack widgets to render `<tab> <tab> — <active metadata>`
    /// on the top border row in place of the active child's full
    /// title, since the stack owns that row. Should not include the
    /// widget's display name — only the suffix after it.
    fn title_metadata(&self) -> Option<String> {
        None
    }

    /// IDs of widgets owned by this widget (used by stack widgets only).
    /// Returns an empty vec for leaf widgets. The shortcut dispatcher
    /// walks these to assign `Shift+<letter>` to children inside a
    /// stack — see `app::assign_shortcuts`.
    fn composite_children(&self) -> Vec<String> {
        Vec::new()
    }

    /// Borrow a child by id (composite widgets only). Default returns
    /// `None`, which means the leaf widget owns no children. Stack
    /// widgets return `Some(&mut child)` so the shortcut dispatcher
    /// can call `set_shortcut` on the right widget and so the runtime
    /// can route Shift+letter into the right pane.
    fn composite_child_mut(&mut self, _child_id: &str) -> Option<&mut dyn Widget> {
        None
    }

    /// Read-only sibling of [`composite_child_mut`]. Used by the help
    /// overlay so it can list every stack child's keybindings — even
    /// the hidden tabs — without needing mutable access to the manager.
    fn composite_child(&self, _child_id: &str) -> Option<&dyn Widget> {
        None
    }

    /// For composite widgets: make the named child the active one.
    /// Returns `true` when the id matched and the widget switched.
    /// Default returns `false` (leaf widgets have nothing to switch).
    fn switch_to_composite_child(&mut self, _child_id: &str) -> bool {
        false
    }

    /// For composite widgets (stacks): the currently-active child's
    /// index. `None` for leaf widgets. Used by the runtime to
    /// persist active-tab state across runs.
    fn composite_active_index(&self) -> Option<usize> {
        None
    }

    /// For composite widgets (stacks): set the active child by
    /// index. No-op (and returns `false`) for leaf widgets or for
    /// out-of-range indices.
    fn set_composite_active_index(&mut self, _idx: usize) -> bool {
        false
    }

    /// Drain any "please bring me to the front" signal the widget has
    /// queued internally. The app polls this each tick; when `Some`,
    /// it promotes the named widget the same way `Shift+<letter>`
    /// does — walking the stack ancestry to flip the right tab
    /// visible and shifting input focus. The default returns `None`;
    /// widgets opt in when they need to grab attention (timer alarm
    /// fires, urgent notification, …). The returned id should be the
    /// widget's *own* id (or a child id when the widget itself is a
    /// composite). Treat this as a one-shot — the widget must clear
    /// its internal flag inside this call so the app doesn't promote
    /// repeatedly.
    fn take_focus_request(&mut self) -> Option<FocusRequest> {
        None
    }
}

/// Widget-initiated attention grab. The app's tick loop polls every
/// widget via `take_focus_request` and, on `Some`, walks the layout
/// to surface the named widget.
#[derive(Debug, Clone)]
pub struct FocusRequest {
    /// Id of the widget that should become focused. If the widget is
    /// a stack child, the app flips its parent stack's active tab to
    /// match before shifting focus.
    pub widget_id: String,
}

/// Owns the set of registered widgets and resolves them by id.
#[derive(Default)]
pub struct WidgetManager {
    widgets: HashMap<String, Box<dyn Widget>>,
    /// Insertion order — used to make Tab cycling deterministic.
    order: Vec<String>,
}

impl WidgetManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an already-boxed widget. Production widgets land here via
    /// the registry's `WidgetFactory` (which returns `Box<dyn Widget>`);
    /// tests box their fixtures by hand.
    pub fn register_boxed(&mut self, widget: Box<dyn Widget>) {
        let id = widget.id().to_string();
        if !self.widgets.contains_key(&id) {
            self.order.push(id.clone());
        }
        self.widgets.insert(id, widget);
    }

    pub fn get(&self, id: &str) -> Option<&dyn Widget> {
        self.widgets.get(id).map(|b| b.as_ref())
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut (dyn Widget + 'static)> {
        self.widgets.get_mut(id).map(|b| b.as_mut())
    }

    pub fn ids(&self) -> &[String] {
        &self.order
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_widget_ref_no_suffix_means_main() {
        assert_eq!(parse_widget_ref("clock"), ("clock".into(), "main".into()));
        assert_eq!(parse_widget_ref("stocks"), ("stocks".into(), "main".into()));
    }

    #[test]
    fn parse_widget_ref_explicit_instance() {
        assert_eq!(
            parse_widget_ref("clock@home"),
            ("clock".into(), "home".into())
        );
        assert_eq!(
            parse_widget_ref("stocks@compare"),
            ("stocks".into(), "compare".into())
        );
    }

    #[test]
    fn parse_widget_ref_empty_suffix_falls_back_to_main() {
        assert_eq!(parse_widget_ref("clock@"), ("clock".into(), "main".into()));
        assert_eq!(
            parse_widget_ref("clock@   "),
            ("clock".into(), "main".into())
        );
    }

    #[test]
    fn widget_config_stem_main_drops_suffix() {
        assert_eq!(widget_config_stem("clock", "main"), "clock");
        assert_eq!(widget_config_stem("stocks", "main"), "stocks");
    }

    #[test]
    fn widget_config_stem_appends_instance_for_non_main() {
        assert_eq!(widget_config_stem("clock", "home"), "clock@home");
        assert_eq!(widget_config_stem("stocks", "compare"), "stocks@compare");
    }
}
