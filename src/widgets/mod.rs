pub mod calendar;
pub mod clock;
pub mod news;
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

#[async_trait]
pub trait Widget: Send + Sync {
    fn id(&self) -> &str;
    #[allow(dead_code)] // surfaced in status bar / command bar in later phases.
    fn display_name(&self) -> &str;

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
