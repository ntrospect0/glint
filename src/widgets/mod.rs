pub mod calendar;
pub mod clock;
pub mod email;
pub mod gallery;
pub mod news;
pub mod resources;
pub mod stocks;
pub mod weather;

use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{layout::Rect, Frame};

use crate::theme::Theme;

/// Shared app-wide context handed to widgets on each tick.
/// Kept empty in Phase 1; future phases will plug in HTTP/LLM clients here.
/// The app theme isn't carried here — each widget already caches its own
/// merged `Theme` at construction (and rebuilds on `apply_config`), so the
/// tick context doesn't need to thread it through.
#[derive(Default)]
pub struct AppContext;

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
    fn id(&self) -> &str;
    #[allow(dead_code)] // surfaced in status bar / command bar in later phases.
    fn display_name(&self) -> &str;

    /// Static identifier shared by every instance of this widget type
    /// (e.g. `"clock"`, `"stocks"`). No default — each widget supplies it.
    #[allow(dead_code)] // surfaced by future per-kind dispatch (e.g. multi-widget routing).
    fn kind(&self) -> &str;

    /// Instance suffix for this widget. `"main"` (the default) maps to the
    /// canonical id (e.g. `clock`); any other value composes into
    /// `<kind>@<instance>` (e.g. `clock@home`).
    #[allow(dead_code)] // exposed for diagnostics / wizard introspection.
    fn instance(&self) -> &str {
        "main"
    }

    async fn update(&mut self, ctx: &AppContext) -> Result<()>;

    fn render(&self, frame: &mut Frame, area: Rect, focused: bool);

    fn handle_key(&mut self, key: KeyEvent) -> EventResult;

    /// Per-widget mouse interaction. `area` is the same outer Rect the widget
    /// received in `render`, so the widget can reconstruct its internal layout.
    /// Default implementation ignores all clicks.
    fn handle_mouse(&mut self, _mouse: MouseEvent, _area: Rect) -> EventResult {
        EventResult::Ignored
    }

    #[allow(dead_code)] // routed from command bar in Phase 2+.
    fn handle_command(&mut self, cmd: &str, args: &[&str]) -> Result<bool>;

    #[allow(dead_code)] // used by config live-reload in Phase 2+.
    fn config(&self) -> serde_json::Value;

    #[allow(dead_code)] // wired by the config watcher in app::run.
    fn apply_config(&mut self, config: serde_json::Value) -> Result<()>;

    /// `(key, description)` pairs surfaced by the `?` help overlay. Default
    /// is empty — widgets opt in by overriding.
    fn keybindings(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }

    /// Swap the widget's app-level theme reference and rebuild its merged
    /// theme (app + widget's `[colors]` overrides). Called by `:scheme` so
    /// the user can change palettes without restarting glint. Default is a
    /// no-op for widgets that don't render themed chrome (none today, but
    /// the default keeps the trait extensible).
    fn set_app_theme(&mut self, _theme: Arc<Theme>) {}

    /// Prioritized list of `Shift+<letter>` shortcut keys the widget would
    /// like to claim for focus. The app walks widgets in registration order
    /// and assigns the first non-conflicting letter; later widgets fall
    /// through their list when their preferred letter is taken. Default
    /// `[]` = widget doesn't participate in shortcut focus (it'll still be
    /// reachable via Tab / Shift+Tab / mouse click).
    ///
    /// Lifetime is the widget's own borrow rather than `'static` so widgets
    /// can return preferences that came from the user's TOML config (which
    /// arrive as `Vec<char>`, not literals).
    fn shortcut_preferences(&self) -> &[char] {
        &[]
    }

    /// Called by the app after the assignment pass with the letter that
    /// was actually picked from `shortcut_preferences`, or `None` if every
    /// letter in the preference list was already taken. Widgets store this
    /// so they can paint the letter inside their title.
    fn set_shortcut(&mut self, _shortcut: Option<char>) {}
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

    pub fn register<W: Widget + 'static>(&mut self, widget: W) {
        let id = widget.id().to_string();
        if !self.widgets.contains_key(&id) {
            self.order.push(id.clone());
        }
        self.widgets.insert(id, Box::new(widget));
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
        assert_eq!(parse_widget_ref("clock@   "), ("clock".into(), "main".into()));
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
