//! Stack widget — a layout cell that holds up to 3 widgets and shows
//! one at a time, with a tab strip in the title bar for switching.
//!
//! See `docs/stack-spec.md` for the full design. This module owns the
//! runtime side: child storage, active-tab index, key handling, tab
//! strip rendering, and hidden-child poll throttling. Schema parsing
//! lives in `config::layout`; wizard sub-page lives in
//! `wizard::pages::assign_stack` (Phase 3); persistence lives in
//! `state::runtime` (Phase 4); shortcut routing lives in `app`
//! (Phase 2).
//!
//! ## Rendering strategy
//!
//! Each child widget paints its own `Block::default().borders(ALL).title(...)`
//! into the area handed to it. We let that happen, then **overlay**
//! the tab strip on the top border row, which replaces the child's
//! single-line title with our multi-tab strip. The border corners and
//! sides remain the child's. Ratatui's render order (last paints win)
//! gives us this for free without modifying any child widget.
//!
//! ## Hidden-child poll throttling
//!
//! Per spec §2, hidden children have their `update()` calls thinned
//! to one-per-`stack_hidden_poll_ratio` ticks. This doesn't change a
//! child's internal poll interval — for widgets that already poll at
//! minute-scale intervals (most of them), the saving is negligible.
//! For widgets that fire faster than the tick rate, it matters. The
//! configurable knob is in place for the future when per-widget
//! interval scaling becomes a real need.

#![allow(dead_code)]

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget as RatatuiWidget},
    Frame,
};

use crate::theme::Theme;

use super::{AppContext, EventResult, Widget};

/// One stack-cell widget. Owns 2–3 child widgets, tracks which one is
/// visible, intercepts `,` / `.` for tab rotation, and overlays a tab
/// strip on the active child's title row.
pub struct StackWidget {
    /// Synthetic id — `stack:<child1>+<child2>+<child3>`. Used for
    /// WidgetManager lookup and focus addressing.
    id: String,
    /// The child widgets, in display order. Tabs are presented in
    /// this order; rotation cycles through them.
    children: Vec<Box<dyn Widget>>,
    /// Index of the currently-visible child.
    active: usize,
    /// Hidden-child poll throttle. `1` = full rate. Tick counter
    /// below is checked against this to decide when to call
    /// `update()` on non-active children.
    poll_ratio: u32,
    /// Wraps around `poll_ratio` to decide when hidden children get a
    /// tick. Incremented every `update()` call regardless of which
    /// child is active.
    tick_counter: u64,
    /// Active app theme — used for tab strip styling. Updated on
    /// `set_app_theme` (in addition to being passed to children).
    theme: Arc<Theme>,
    /// Cache of the active child's display name for the `display_name`
    /// trait method. Recomputed when the active tab changes.
    active_display_name: String,
}

impl StackWidget {
    pub fn new(
        id: String,
        children: Vec<Box<dyn Widget>>,
        poll_ratio: u32,
        theme: Arc<Theme>,
    ) -> Self {
        assert!(
            children.len() >= 2,
            "StackWidget needs at least 2 children; single-child cells should degrade to a non-stack cell at the schema layer"
        );
        let active_display_name = children[0].display_name().to_string();
        Self {
            id,
            children,
            active: 0,
            poll_ratio: poll_ratio.max(1),
            tick_counter: 0,
            theme,
            active_display_name,
        }
    }

    /// Rotate to the next tab (with wrap-around). Updates the cached
    /// `active_display_name` so the title row picks up the new name on
    /// the next render.
    pub fn rotate_next(&mut self) {
        if self.children.len() <= 1 {
            return;
        }
        self.active = (self.active + 1) % self.children.len();
        self.refresh_active_name();
    }

    /// Rotate to the previous tab (with wrap-around).
    pub fn rotate_prev(&mut self) {
        if self.children.len() <= 1 {
            return;
        }
        self.active = (self.active + self.children.len() - 1) % self.children.len();
        self.refresh_active_name();
    }

    /// Switch to a specific child by its widget id (used by Phase 2's
    /// shortcut dispatcher). Returns `true` when the id was found and
    /// the active index changed (or already matched).
    pub fn switch_to(&mut self, widget_id: &str) -> bool {
        if let Some(idx) = self.children.iter().position(|w| w.id() == widget_id) {
            self.active = idx;
            self.refresh_active_name();
            true
        } else {
            false
        }
    }

    /// Borrow the child widget ids in tab order — used by the shortcut
    /// dispatcher when it walks into a stack.
    pub fn child_ids(&self) -> Vec<&str> {
        self.children.iter().map(|w| w.id()).collect()
    }

    fn refresh_active_name(&mut self) {
        if let Some(child) = self.children.get(self.active) {
            self.active_display_name = child.display_name().to_string();
        }
    }
}

#[async_trait]
impl Widget for StackWidget {
    fn id(&self) -> &str {
        &self.id
    }

    fn display_name(&self) -> &str {
        // The active tab's name dominates the title row; the tab strip
        // is the separate "you're in a stack" affordance.
        &self.active_display_name
    }

    fn kind(&self) -> &str {
        "stack"
    }

    fn instance(&self) -> &str {
        "main"
    }

    async fn update(&mut self, ctx: &AppContext) -> Result<()> {
        self.tick_counter = self.tick_counter.wrapping_add(1);
        let ratio = self.poll_ratio.max(1) as u64;
        let allow_hidden_tick = ratio == 1 || self.tick_counter % ratio == 0;
        let active = self.active;
        for (i, child) in self.children.iter_mut().enumerate() {
            if i == active || allow_hidden_tick {
                child.update(ctx).await?;
            }
        }
        Ok(())
    }

    fn render(&self, frame: &mut Frame, area: Rect, focused: bool) {
        if let Some(child) = self.children.get(self.active) {
            child.render(frame, area, focused);
        }
        // Overlay the tab strip on the active child's top border row,
        // replacing whatever title text it painted there. Border
        // corners (column 0 and the rightmost column) stay untouched.
        render_tab_strip(
            frame.buffer_mut(),
            area,
            self.children.iter().map(|w| w.display_name()).collect(),
            self.active,
            focused,
            &self.theme,
        );
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        match key.code {
            KeyCode::Char(',') => {
                self.rotate_prev();
                return EventResult::Handled;
            }
            KeyCode::Char('.') => {
                self.rotate_next();
                return EventResult::Handled;
            }
            _ => {}
        }
        if let Some(child) = self.children.get_mut(self.active) {
            child.handle_key(key)
        } else {
            EventResult::Ignored
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, area: Rect) -> EventResult {
        if let Some(child) = self.children.get_mut(self.active) {
            child.handle_mouse(mouse, area)
        } else {
            EventResult::Ignored
        }
    }

    fn handle_command(&mut self, cmd: &str, args: &[&str]) -> Result<bool> {
        // Route command-bar commands to every child; first claimant wins.
        // Stack itself doesn't own commands.
        for child in self.children.iter_mut() {
            if child.handle_command(cmd, args)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn config(&self) -> serde_json::Value {
        serde_json::json!({
            "kind": "stack",
            "active": self.active,
            "poll_ratio": self.poll_ratio,
            "children": self.children.iter().map(|c| c.id()).collect::<Vec<_>>(),
        })
    }

    fn apply_config(&mut self, config: serde_json::Value) -> Result<()> {
        // The stack itself has no config; forward to children if the
        // payload contains per-child configs. Currently a no-op —
        // children are reloaded through their own per-instance TOML
        // files via the config watcher.
        let _ = config;
        Ok(())
    }

    fn keybindings(&self) -> Vec<(&'static str, &'static str)> {
        let mut out = vec![
            (",", "rotate to previous tab"),
            (".", "rotate to next tab"),
        ];
        // Merge in the active child's bindings so the help overlay
        // reflects what's actually usable right now.
        if let Some(child) = self.children.get(self.active) {
            out.extend(child.keybindings());
        }
        out
    }

    fn set_app_theme(&mut self, theme: Arc<Theme>) {
        self.theme = theme.clone();
        for child in self.children.iter_mut() {
            child.set_app_theme(theme.clone());
        }
    }

    fn shortcut_preferences(&self) -> &[char] {
        // Stack itself doesn't claim a shortcut — its children do.
        // Phase 2's dispatcher walks into stacks; until then the
        // children's preferences are inaccessible. Returning &[] here
        // keeps the assignment pass simple.
        &[]
    }

    fn set_shortcut(&mut self, _shortcut: Option<char>) {
        // No-op: stack doesn't have its own shortcut.
    }

    fn composite_children(&self) -> Vec<String> {
        self.children.iter().map(|w| w.id().to_string()).collect()
    }

    fn composite_child_mut(&mut self, child_id: &str) -> Option<&mut dyn Widget> {
        self.children
            .iter_mut()
            .find(|w| w.id() == child_id)
            .map(|b| b.as_mut() as &mut dyn Widget)
    }

    fn switch_to_composite_child(&mut self, child_id: &str) -> bool {
        self.switch_to(child_id)
    }

    fn composite_active_index(&self) -> Option<usize> {
        Some(self.active)
    }

    fn set_composite_active_index(&mut self, idx: usize) -> bool {
        if idx >= self.children.len() {
            return false;
        }
        self.active = idx;
        self.refresh_active_name();
        true
    }
}

/// Paint the tab strip onto the top-border row of `area`. Tries full
/// titles first; falls back to single-letter initials when the joined
/// width exceeds the available column count. The active tab uses
/// `text_selected` styling; inactive tabs use `text_dim`. Border
/// corners (col 0 and rightmost col) are not touched.
fn render_tab_strip(
    buf: &mut Buffer,
    area: Rect,
    titles: Vec<&str>,
    active: usize,
    focused: bool,
    theme: &Theme,
) {
    if area.width < 4 || area.height == 0 {
        return;
    }
    // The strip lives ON the top border line (y = area.y), starting
    // one cell in from the left corner so we don't overwrite '┌' /
    // '└'-style glyphs.
    let strip_y = area.y;
    let strip_x = area.x.saturating_add(1);
    let strip_width = area.width.saturating_sub(2);

    let active_style = if focused {
        theme.text_selected
    } else {
        theme.text_focused
    };
    let idle_style = theme.text_dim;

    // First try full-title spans; if they fit, paint them. Otherwise
    // fall back to initials. Both modes include a leading space so
    // the strip looks visually distinct from the corner glyph.
    let full = build_tab_spans(&titles, active, false, active_style, idle_style);
    let spans = if span_width(&full) <= strip_width as usize {
        full
    } else {
        build_tab_spans(&titles, active, true, active_style, idle_style)
    };

    let para = Paragraph::new(Line::from(spans));
    let row = Rect {
        x: strip_x,
        y: strip_y,
        width: strip_width,
        height: 1,
    };
    para.render(row, buf);
}

fn build_tab_spans(
    titles: &[&str],
    active: usize,
    compact: bool,
    active_style: Style,
    idle_style: Style,
) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::with_capacity(titles.len() * 3);
    out.push(Span::raw(" "));
    for (i, title) in titles.iter().enumerate() {
        let label = if compact {
            title
                .chars()
                .find(|c| c.is_alphanumeric())
                .map(|c| c.to_ascii_uppercase().to_string())
                .unwrap_or_else(|| "?".into())
        } else {
            (*title).to_string()
        };
        let marker = if i == active { "• " } else { "  " };
        let body = format!("[{marker}{label}]");
        let style = if i == active {
            active_style.add_modifier(Modifier::BOLD)
        } else {
            idle_style
        };
        out.push(Span::styled(body, style));
        if i + 1 < titles.len() {
            out.push(Span::raw(" "));
        }
    }
    out
}

fn span_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widgets::{AppContext, Widget as WidgetTrait};

    /// Minimal widget fixture for stack-behaviour tests. Tracks
    /// `update` and `render` calls so assertions can verify
    /// delegation + throttling.
    struct StubWidget {
        id: String,
        name: String,
        update_calls: std::sync::Arc<std::sync::Mutex<usize>>,
    }

    impl StubWidget {
        fn new(id: &str) -> (Self, std::sync::Arc<std::sync::Mutex<usize>>) {
            let counter = std::sync::Arc::new(std::sync::Mutex::new(0));
            (
                Self {
                    id: id.to_string(),
                    name: id.to_string(),
                    update_calls: counter.clone(),
                },
                counter,
            )
        }
    }

    #[async_trait]
    impl WidgetTrait for StubWidget {
        fn id(&self) -> &str {
            &self.id
        }
        fn display_name(&self) -> &str {
            &self.name
        }
        fn kind(&self) -> &str {
            "stub"
        }
        async fn update(&mut self, _ctx: &AppContext) -> Result<()> {
            *self.update_calls.lock().unwrap() += 1;
            Ok(())
        }
        fn render(&self, _frame: &mut Frame, _area: Rect, _focused: bool) {}
        fn handle_key(&mut self, _key: KeyEvent) -> EventResult {
            EventResult::Ignored
        }
        fn handle_command(&mut self, _cmd: &str, _args: &[&str]) -> Result<bool> {
            Ok(false)
        }
        fn config(&self) -> serde_json::Value {
            serde_json::json!(null)
        }
        fn apply_config(&mut self, _config: serde_json::Value) -> Result<()> {
            Ok(())
        }
    }

    fn build_stack(ratio: u32) -> (StackWidget, Vec<std::sync::Arc<std::sync::Mutex<usize>>>) {
        let (a, ca) = StubWidget::new("a");
        let (b, cb) = StubWidget::new("b");
        let (c, cc) = StubWidget::new("c");
        let theme = std::sync::Arc::new(Theme::builtin_defaults());
        let stack = StackWidget::new(
            "stack:a+b+c".to_string(),
            vec![Box::new(a), Box::new(b), Box::new(c)],
            ratio,
            theme,
        );
        (stack, vec![ca, cb, cc])
    }

    #[tokio::test]
    async fn rotation_keys_cycle_active_index_with_wrap() {
        let (mut stack, _) = build_stack(1);
        assert_eq!(stack.active, 0);
        stack.handle_key(KeyEvent::from(KeyCode::Char('.')));
        assert_eq!(stack.active, 1);
        stack.handle_key(KeyEvent::from(KeyCode::Char('.')));
        assert_eq!(stack.active, 2);
        stack.handle_key(KeyEvent::from(KeyCode::Char('.')));
        assert_eq!(stack.active, 0); // wraps
        stack.handle_key(KeyEvent::from(KeyCode::Char(',')));
        assert_eq!(stack.active, 2); // wraps backward
    }

    #[tokio::test]
    async fn hidden_children_throttled_per_poll_ratio() {
        let (mut stack, counters) = build_stack(3);
        let ctx = AppContext::default();
        for _ in 0..6 {
            stack.update(&ctx).await.unwrap();
        }
        // Active (a) updates every tick → 6.
        // Hidden (b, c) update only when tick_counter % 3 == 0:
        //   tick=1: skip; tick=2: skip; tick=3: yes; tick=4: skip; tick=5: skip; tick=6: yes.
        //   → 2 updates each.
        assert_eq!(*counters[0].lock().unwrap(), 6, "active child should update every tick");
        assert_eq!(*counters[1].lock().unwrap(), 2, "hidden child should be throttled");
        assert_eq!(*counters[2].lock().unwrap(), 2, "hidden child should be throttled");
    }

    #[tokio::test]
    async fn poll_ratio_one_means_no_throttling() {
        let (mut stack, counters) = build_stack(1);
        let ctx = AppContext::default();
        for _ in 0..5 {
            stack.update(&ctx).await.unwrap();
        }
        for c in counters {
            assert_eq!(*c.lock().unwrap(), 5);
        }
    }

    #[test]
    fn switch_to_finds_child_by_id() {
        let (mut stack, _) = build_stack(1);
        assert!(stack.switch_to("b"));
        assert_eq!(stack.active, 1);
        assert!(stack.switch_to("a"));
        assert_eq!(stack.active, 0);
        assert!(!stack.switch_to("nope"));
        assert_eq!(stack.active, 0);
    }

    #[test]
    fn build_tab_spans_full_mode_contains_titles() {
        let theme = Theme::builtin_defaults();
        let spans = build_tab_spans(
            &["Clock", "Weather", "Stocks"],
            1,
            false,
            theme.text_selected,
            theme.text_dim,
        );
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(joined.contains("Clock"));
        assert!(joined.contains("• Weather"));
        assert!(joined.contains("Stocks"));
    }

    #[test]
    fn build_tab_spans_compact_mode_uses_initials() {
        let theme = Theme::builtin_defaults();
        let spans = build_tab_spans(
            &["Clock", "Weather", "Stocks"],
            0,
            true,
            theme.text_selected,
            theme.text_dim,
        );
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        // Active tab gets the `•` marker; initials are uppercase.
        assert!(joined.contains("• C"));
        assert!(joined.contains("W"));
        assert!(joined.contains("S"));
        assert!(!joined.contains("Clock"));
    }
}
