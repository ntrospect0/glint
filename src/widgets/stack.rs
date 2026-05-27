//! Stack widget — a layout cell that holds up to 3 widgets and shows
//! one at a time, with a tab strip in the title bar for switching.
//!
//! See `docs/stack-spec.md` for the full design. Related code lives
//! in `config::layout` (schema), `wizard::pages::assign_stack` (wizard
//! UI), `runtime_state` (active-tab persistence), and `app`
//! (Shift+<letter> routing walks into stacks).
//!
//! ## Rendering strategy
//!
//! Each child widget paints its own `Block::default().borders(ALL).title(...)`
//! into the area handed to it. We then overlay our tab strip on the
//! top border row, replacing the child's single-line title. Border
//! corners and sides remain the child's. Ratatui's render order
//! (last paints win) gives us this for free without modifying any
//! child widget.
//!
//! ## Hidden-child poll throttling
//!
//! Hidden children have their `update()` calls thinned to
//! one-per-`stack_hidden_poll_ratio` ticks (configured globally).
//! For widgets that poll at minute-scale intervals the saving is
//! negligible; for widgets that fire faster than the tick rate it
//! matters proportionally.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
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
    /// Screen-column ranges of each tab in the strip, captured during
    /// the last render so `handle_mouse` can route clicks on a tab
    /// title to `switch_to(child)` without re-running the fit ladder.
    /// `Default::default()` until the first render.
    tab_layout: Mutex<TabStripLayout>,
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
            tab_layout: Mutex::new(TabStripLayout::default()),
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

    /// Switch to a specific child by its widget id. Used by the
    /// `Shift+<letter>` dispatcher when the requested letter belongs
    /// to a child hidden inside this stack. Returns `true` when the
    /// id matched.
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
        // Let the child render its full block + content first; then
        // overlay our tab strip on the top border row, replacing the
        // child's own title text. This keeps the cell at its full
        // vertical height — no row tax — and the active widget's
        // metadata (article count, mailbox address, etc.) is rendered
        // by US on the same row as part of the strip, so the user
        // sees both the tabs and the metadata simultaneously.
        if let Some(child) = self.children.get(self.active) {
            child.render(frame, area, focused);
        }
        let tabs: Vec<(String, Option<char>)> = self
            .children
            .iter()
            .map(|w| (w.display_name().to_string(), w.shortcut()))
            .collect();
        let metadata = self
            .children
            .get(self.active)
            .and_then(|w| w.title_metadata());
        let layout = render_tab_strip(
            frame.buffer_mut(),
            area,
            &tabs,
            self.active,
            focused,
            &self.theme,
            metadata.as_deref(),
        );
        // Stash the hit-rects so the next mouse click can route a tab-strip
        // click to switch_to(child) instead of falling through to the child.
        *self.tab_layout.lock().expect("stack tab_layout poisoned") = layout;
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
        // Left-click on a tab title flips the stack to that child. Any
        // other mouse event (scroll, drag, click below the tab row, …)
        // falls through to the active child so the existing per-widget
        // mouse semantics still apply.
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            let layout = self.tab_layout.lock().expect("stack tab_layout poisoned").clone();
            if mouse.row == layout.row {
                for (idx, (start, end)) in layout.tab_ranges.iter().enumerate() {
                    if mouse.column >= *start && mouse.column < *end {
                        if idx != self.active {
                            self.active = idx;
                            self.refresh_active_name();
                        }
                        return EventResult::Handled;
                    }
                }
            }
        }
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
        // Stack-only bindings. Each child's own bindings reach the help
        // overlay via `composite_child` so the hidden tabs are visible
        // too — merging the active child's bindings in here would mask
        // the others and double-list the active one.
        vec![
            (",", "rotate to previous tab"),
            (".", "rotate to next tab"),
            ("click tab title", "switch to that stack tab"),
        ]
    }

    fn set_app_theme(&mut self, theme: Arc<Theme>) {
        self.theme = theme.clone();
        for child in self.children.iter_mut() {
            child.set_app_theme(theme.clone());
        }
    }

    fn shortcut_preferences(&self) -> &[char] {
        // Stack itself doesn't claim a shortcut — its children do,
        // and the assignment dispatcher walks into composites via
        // `composite_children` to reach them.
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

    fn composite_child(&self, child_id: &str) -> Option<&dyn Widget> {
        self.children
            .iter()
            .find(|w| w.id() == child_id)
            .map(|b| b.as_ref() as &dyn Widget)
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

/// Paint the tab strip onto the top border row of `area`, replacing
/// whatever title text the active child painted there. Layout:
///
/// `┌─ <tab> ─ <tab> ─ <tab> ──────── <active metadata> ─┐`
///
/// - **Tabs on the left**, separated by ` ─ ` so the visual divider
///   matches the surrounding border line.
/// - **`─` filler** between the last tab and the right-aligned
///   metadata. Filler is also what we use to fully overwrite any title
///   text the child painted before us — without it, a long child title
///   would bleed through past our tab labels.
/// - **Active widget's metadata right-aligned** at the top-right
///   corner in `metadata.focused` / `metadata.unfocused` — dim when the
///   pane isn't focused, lit when it is. Hidden entirely when the pane
///   is too narrow to fit `tabs + min_gap + metadata`.
///
/// Tab label styling:
/// - Shortcut letter (e.g. `N` in `News`, `E` in `Email`) always painted
///   in `theme.text_shortcut` so the `Shift+<letter>` affordance stays
///   visible in every state.
/// - Active tab: `widget_title.focused` when the pane is focused (the
///   background-highlight variant), `widget_title.unfocused` when not —
///   matches the single-widget title row.
/// - Inactive tabs: `text.dim`. The active-vs-inactive contrast
///   (`widget_title.unfocused` vs `text.dim`) is still legible in an
///   unfocused stack — that's what tells the user which child is on
///   top from across the dashboard.
///
/// Overflow: tries full titles + metadata first; drops metadata; falls
/// back to single-letter labels as the final attempt.
fn render_tab_strip(
    buf: &mut Buffer,
    area: Rect,
    tabs: &[(String, Option<char>)],
    active: usize,
    focused: bool,
    theme: &Theme,
    metadata: Option<&str>,
) -> TabStripLayout {
    if area.width < 4 || area.height == 0 {
        return TabStripLayout::default();
    }
    let border_style = theme.border_style(focused);
    let inner_width = area.width.saturating_sub(2) as usize;
    let strip_rect = Rect {
        x: area.x.saturating_add(1),
        y: area.y,
        width: area.width.saturating_sub(2),
        height: 1,
    };

    // Same fit ladder as before, just with metadata pinned to the
    // right corner instead of glued to the last tab:
    //   1. full tabs + right-aligned metadata
    //   2. full tabs, no metadata
    //   3. compact tabs + right-aligned metadata
    //   4. compact tabs, no metadata
    let attempts: [(bool, bool); 4] = [
        (false, metadata.is_some()),
        (false, false),
        (true, metadata.is_some()),
        (true, false),
    ];

    // Min gap chars between the last tab and the metadata so they
    // never kiss each other (matches the single-widget title row).
    const MIN_GAP: usize = 3;

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut chosen_ranges: Vec<(usize, usize)> = Vec::new();
    for (compact, show_meta) in attempts.iter() {
        let (left, left_ranges) =
            build_tab_label_spans_with_ranges(tabs, active, *compact, focused, theme);
        let meta_spans: Vec<Span<'static>> = if *show_meta {
            build_metadata_spans(metadata, theme.metadata_style(focused))
        } else {
            Vec::new()
        };
        let left_w = spans_width(&left);
        let meta_w = spans_width(&meta_spans);
        let need = left_w
            .saturating_add(if meta_spans.is_empty() { 0 } else { MIN_GAP })
            .saturating_add(meta_w);
        if need <= inner_width {
            spans = left;
            chosen_ranges = left_ranges;
            let filler_count = inner_width - left_w - meta_w;
            if filler_count > 0 {
                spans.push(Span::styled(
                    "─".repeat(filler_count),
                    border_style,
                ));
            }
            spans.extend(meta_spans);
            break;
        }
        // Keep latest attempt as the fallback if nothing fits; the
        // last (compact, no-meta) attempt is the most forgiving so the
        // loop's final value is the best we have.
        spans = left;
        chosen_ranges = left_ranges;
    }

    // Reset the background of every cell in the strip row before we
    // paint our spans. The active child has already painted its own
    // block + title underneath us, and ratatui's `Cell::set_style`
    // only overrides bg when the new span explicitly sets bg. If a
    // colorscheme gives `widget_title.focused` a background color,
    // those cells would stay tinted in the leftmost part of our strip
    // (where the child's title sat) — making it look like multiple
    // tabs are highlighted. Resetting first guarantees only the chars
    // we paint here decide the background.
    buf.set_style(strip_rect, Style::default().bg(Color::Reset));

    Paragraph::new(Line::from(spans)).render(strip_rect, buf);

    // Translate char offsets (relative to the start of the spans) into
    // absolute screen columns by adding strip_rect.x. handle_mouse uses
    // these to route clicks on a tab to `switch_to`.
    let base = strip_rect.x as usize;
    let tab_ranges = chosen_ranges
        .into_iter()
        .map(|(s, e)| {
            (
                base.saturating_add(s) as u16,
                base.saturating_add(e) as u16,
            )
        })
        .collect();
    TabStripLayout {
        row: strip_rect.y,
        tab_ranges,
    }
}

/// Screen-space hit-rect data for the tab strip. `row` is the row of
/// the strip (the cell's top border); `tab_ranges[i]` is the
/// `[start_col, end_col)` covered by the i-th tab. Empty when the cell
/// was too narrow to render the strip at all.
#[derive(Debug, Default, Clone)]
struct TabStripLayout {
    row: u16,
    tab_ranges: Vec<(u16, u16)>,
}

fn build_tab_label_spans(
    tabs: &[(String, Option<char>)],
    active: usize,
    compact: bool,
    focused: bool,
    theme: &Theme,
) -> Vec<Span<'static>> {
    build_tab_label_spans_with_ranges(tabs, active, compact, focused, theme).0
}

/// Same span layout as [`build_tab_label_spans`] but also returns the
/// `[start, end)` char offsets (relative to the start of the strip's
/// spans) of each tab's body — including the active tab's tee pads.
/// Callers translate the offsets to absolute screen columns by adding
/// `strip_rect.x` to each bound; the result is the hit-rect for that
/// tab when routing clicks.
fn build_tab_label_spans_with_ranges(
    tabs: &[(String, Option<char>)],
    active: usize,
    compact: bool,
    focused: bool,
    theme: &Theme,
) -> (Vec<Span<'static>>, Vec<(usize, usize)>) {
    let shortcut_style = theme.text_shortcut;
    // Active tab uses the same focused/unfocused pair as a single
    // widget's title; inactive tabs use text.dim. The dim vs
    // widget_title.unfocused contrast keeps the active tab readable
    // even when the pane has no focus.
    let active_style = theme.widget_title_style(focused);
    let dim_style = theme.text_dim;
    let border_style = theme.border_style(focused);

    // Reserved 1-char pad slot on each side of the *active* tab so
    // its width stays constant between focused/unfocused. When
    // focused the slots render as `┤` / `├` tee-junctions in the
    // border-focused color — the active tab visually notches into
    // the surrounding border line, matching the single-widget title.
    // When unfocused the slots are blanks. Inactive tabs get no pad.
    let (active_left, active_right) = if focused { ("┤", "├") } else { (" ", " ") };
    let active_pad_style = if focused { theme.border_focused } else { border_style };

    let mut out: Vec<Span<'static>> = Vec::with_capacity(tabs.len() * 5 + 1);
    let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(tabs.len());
    let last_idx = tabs.len().saturating_sub(1);

    // Char cursor — incremented as we push spans so we can record each
    // tab's [start, end) offsets for click hit-testing.
    let mut pos: usize = 0;
    let push = |out: &mut Vec<Span<'static>>,
                pos: &mut usize,
                content: String,
                style: Style| {
        *pos += content.chars().count();
        out.push(Span::styled(content, style));
    };

    // Leading flows out of the `┌` corner. When the first tab is active
    // we drop the trailing space so the active pad sits flush against
    // the leading line — the focused tee then notches directly into
    // the corner, matching the single-widget title row.
    let leading = if active == 0 { "─" } else { "─ " };
    push(&mut out, &mut pos, leading.to_string(), border_style);

    for (i, (title, shortcut)) in tabs.iter().enumerate() {
        let is_active = i == active;
        let body_style = if is_active { active_style } else { dim_style };

        let tab_start = pos;
        if is_active {
            push(&mut out, &mut pos, active_left.to_string(), active_pad_style);
        }

        let label_chars: Vec<char> = if compact {
            let ch = shortcut
                .map(|c| c.to_ascii_uppercase())
                .or_else(|| {
                    title
                        .chars()
                        .find(|c| c.is_alphanumeric())
                        .map(|c| c.to_ascii_uppercase())
                })
                .unwrap_or('?');
            vec![ch]
        } else {
            title.chars().collect()
        };

        let shortcut_idx = shortcut.and_then(|letter| {
            let lower = letter.to_ascii_lowercase();
            label_chars
                .iter()
                .position(|c| c.to_ascii_lowercase() == lower)
        });

        match shortcut_idx {
            Some(idx) => {
                if idx > 0 {
                    let before: String = label_chars[..idx].iter().collect();
                    push(&mut out, &mut pos, before, body_style);
                }
                let target = label_chars[idx].to_ascii_uppercase();
                push(&mut out, &mut pos, target.to_string(), shortcut_style);
                if idx + 1 < label_chars.len() {
                    let after: String = label_chars[idx + 1..].iter().collect();
                    push(&mut out, &mut pos, after, body_style);
                }
            }
            None => {
                let s: String = label_chars.iter().collect();
                push(&mut out, &mut pos, s, body_style);
            }
        }

        if is_active {
            push(&mut out, &mut pos, active_right.to_string(), active_pad_style);
        }
        ranges.push((tab_start, pos));

        if i + 1 < tabs.len() {
            // Drop the space on whichever side of the separator
            // touches an active tab: the active tab's own pad fills
            // that slot, keeping the strip width invariant and making
            // the focused tee notch into the border line instead of
            // floating in a gap.
            let next_active = (i + 1) == active;
            let sep = match (is_active, next_active) {
                (true, false) => "─ ",
                (false, true) => " ─",
                _ => " ─ ",
            };
            push(&mut out, &mut pos, sep.to_string(), border_style);
        }
    }

    // Trailing space so the filler ─s don't kiss the last label.
    // When the last tab is active the trailing pad already provides
    // the terminal char and the filler ─s connect to it directly.
    if active != last_idx {
        push(&mut out, &mut pos, " ".to_string(), border_style);
    }
    let _ = pos;
    (out, ranges)
}

fn build_metadata_spans(
    metadata: Option<&str>,
    style: ratatui::style::Style,
) -> Vec<Span<'static>> {
    let Some(meta) = metadata else {
        return Vec::new();
    };
    if meta.is_empty() {
        return Vec::new();
    }
    // Single-space padding on each side so a focused-style background
    // color (or any future highlight) doesn't kiss the border corner.
    vec![
        Span::styled(" ".to_string(), style),
        Span::styled(meta.to_string(), style),
        Span::styled(" ".to_string(), style),
    ]
}

fn spans_width(spans: &[Span<'_>]) -> usize {
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
    fn build_tab_label_spans_with_ranges_marks_each_tab_body() {
        // Ranges should cover the active tab including its ┤ / ├ pad
        // (so a click on the tee still activates) and bracket the
        // inactive labels too — but exclude the leading `─ ` and
        // inter-tab separators so a click on the line between tabs
        // does NOT count as either tab.
        let theme = Theme::builtin_defaults();
        let (spans, ranges) = build_tab_label_spans_with_ranges(
            &tabs(&[("News", Some('n')), ("Email", Some('e'))]),
            0,
            false,
            true,
            &theme,
        );
        assert_eq!(ranges.len(), 2, "one range per tab");
        // Reconstruct the joined string and read each tab's slice from
        // its range so we don't have to hard-code offsets.
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        let chars: Vec<char> = joined.chars().collect();
        let slice = |r: (usize, usize)| -> String { chars[r.0..r.1].iter().collect() };
        assert_eq!(slice(ranges[0]), "┤News├", "active tab body includes both tees");
        assert_eq!(slice(ranges[1]), "Email", "inactive tab is just the label");
    }

    #[test]
    fn handle_mouse_click_on_inactive_tab_switches_to_it() {
        // Simulate a render to populate `tab_layout`, then click on an
        // inactive tab's column range and check the active index moved
        // (and that subsequent clicks on the active tab are no-ops).
        let (mut stack, _) = build_stack(1);
        // Hand-populate the layout cache as render would — pretend the
        // strip is at row 0 with three 4-wide tabs starting at col 1.
        *stack.tab_layout.lock().unwrap() = TabStripLayout {
            row: 0,
            tab_ranges: vec![(1, 5), (6, 10), (11, 15)],
        };
        let click = |col: u16| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row: 0,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        assert_eq!(stack.active, 0);
        assert_eq!(
            stack.handle_mouse(click(8), Rect::new(0, 0, 20, 10)),
            EventResult::Handled
        );
        assert_eq!(stack.active, 1, "click in tab 1's range switches");
        assert_eq!(
            stack.handle_mouse(click(8), Rect::new(0, 0, 20, 10)),
            EventResult::Handled,
            "click on the now-active tab is still handled (no fall-through)"
        );
        assert_eq!(stack.active, 1, "active doesn't change on re-click");
        // A click outside any tab range should fall through to the
        // child (Ignored, since StubWidget doesn't claim mouse events).
        assert_eq!(
            stack.handle_mouse(click(50), Rect::new(0, 0, 60, 10)),
            EventResult::Ignored,
        );
        assert_eq!(stack.active, 1, "click outside ranges leaves active alone");
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

    fn tabs(items: &[(&str, Option<char>)]) -> Vec<(String, Option<char>)> {
        items
            .iter()
            .map(|(name, sc)| ((*name).to_string(), *sc))
            .collect()
    }

    #[test]
    fn build_tab_label_spans_full_mode_contains_titles() {
        let theme = Theme::builtin_defaults();
        // Active=0 so that the two trailing inactive tabs sit side by
        // side and the ` ─ ` inactive↔inactive separator appears in
        // the joined output.
        let spans = build_tab_label_spans(
            &tabs(&[("Clock", Some('c')), ("Weather", Some('w')), ("Stocks", Some('s'))]),
            0,
            false,
            true,
            &theme,
        );
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(joined.contains("Clock"));
        assert!(joined.contains("Weather"));
        assert!(joined.contains("Stocks"));
        // Tab separator between two inactive tabs is the horizontal-line
        // glyph wrapped in spaces, never a pipe.
        assert!(joined.contains(" ─ "));
        assert!(!joined.contains(" | "));
        // Arrows were removed when the title row was redesigned —
        // focus is now conveyed by tee-junction bracket pad, not by
        // ▶ ◀ glyphs.
        assert!(!joined.contains('▶'));
        assert!(!joined.contains('◀'));
    }

    #[test]
    fn build_tab_label_spans_active_tab_wrapped_in_tees_when_focused() {
        // Active tab gets ┤ on the left and ├ on the right when the
        // stack is focused. The brackets are styled in border_focused
        // so they connect visually to the surrounding `─` border.
        let theme = Theme::builtin_defaults();
        let spans = build_tab_label_spans(
            &tabs(&[("Clock", Some('c')), ("Weather", Some('w')), ("News", Some('n'))]),
            1, // Weather active
            false,
            true, // focused
            &theme,
        );
        let lefts: Vec<_> = spans
            .iter()
            .filter(|s| s.content.as_ref() == "┤")
            .collect();
        let rights: Vec<_> = spans
            .iter()
            .filter(|s| s.content.as_ref() == "├")
            .collect();
        assert_eq!(lefts.len(), 1, "exactly one ┤ for the active tab");
        assert_eq!(rights.len(), 1, "exactly one ├ for the active tab");
        assert_eq!(lefts[0].style, theme.border_focused);
        assert_eq!(rights[0].style, theme.border_focused);
    }

    #[test]
    fn build_tab_label_spans_tee_notches_flush_against_border() {
        // The bug this fixes: a leading space between the surrounding
        // `─` border line and the active tab's `┤` / `├` tees, which
        // made the focus indicator look like a break in the border
        // rather than a notch into it. The active pad must sit
        // directly adjacent to the surrounding line on both sides.
        let theme = Theme::builtin_defaults();

        // Active = first tab → leading collapses from `─ ` (2 chars)
        // to `─` (1 char) so `┤` notches into the corner glyph.
        let spans = build_tab_label_spans(
            &tabs(&[("News", Some('n')), ("Email", Some('e'))]),
            0,
            false,
            true,
            &theme,
        );
        assert_eq!(spans[0].content.as_ref(), "─");
        assert_eq!(spans[1].content.as_ref(), "┤");

        // Active in the middle → separators on each side lose their
        // inner space, leaving ` ─┤` before and `├─ ` after.
        let spans = build_tab_label_spans(
            &tabs(&[("News", Some('n')), ("Email", Some('e')), ("Stocks", Some('s'))]),
            1,
            false,
            true,
            &theme,
        );
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(joined.contains(" ─┤"), "separator before active should flush against `┤`");
        assert!(joined.contains("├─ "), "active should flush against the separator after `├`");
        assert!(!joined.contains(" ┤"), "no blank gap on the outside of `┤`");
        assert!(!joined.contains("├ "), "no blank gap on the outside of `├`");

        // Active = last tab → trailing space is dropped so the filler
        // `─` chars connect directly to `├`.
        let spans = build_tab_label_spans(
            &tabs(&[("News", Some('n')), ("Email", Some('e'))]),
            1,
            false,
            true,
            &theme,
        );
        let last = spans.last().expect("non-empty");
        assert_eq!(last.content.as_ref(), "├", "last span should be `├` with no trailing space");
    }

    #[test]
    fn build_tab_label_spans_active_pad_collapses_to_spaces_when_unfocused() {
        // Width must stay constant across focus states — when not
        // focused, the bracket slots fall back to plain spaces.
        let theme = Theme::builtin_defaults();
        let focused = build_tab_label_spans(
            &tabs(&[("Clock", Some('c')), ("Weather", Some('w'))]),
            1,
            false,
            true,
            &theme,
        );
        let unfocused = build_tab_label_spans(
            &tabs(&[("Clock", Some('c')), ("Weather", Some('w'))]),
            1,
            false,
            false,
            &theme,
        );
        assert_eq!(spans_width(&focused), spans_width(&unfocused));
        let unfocused_text: String =
            unfocused.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !unfocused_text.contains('┤') && !unfocused_text.contains('├'),
            "no tee glyphs in unfocused state"
        );
    }

    #[test]
    fn build_tab_label_spans_highlights_shortcut_letter() {
        let theme = Theme::builtin_defaults();
        let spans = build_tab_label_spans(
            &tabs(&[("News", Some('n')), ("Email", Some('e'))]),
            0,
            false,
            true,
            &theme,
        );
        let n_span = spans
            .iter()
            .find(|s| s.style == theme.text_shortcut && s.content == "N");
        let e_span = spans
            .iter()
            .find(|s| s.style == theme.text_shortcut && s.content == "E");
        assert!(n_span.is_some(), "N in 'News' should be shortcut-styled");
        assert!(e_span.is_some(), "E in 'Email' should be shortcut-styled");
    }

    #[test]
    fn build_tab_label_spans_active_uses_focused_style_when_pane_focused() {
        let theme = Theme::builtin_defaults();
        let spans = build_tab_label_spans(
            &tabs(&[("News", Some('n')), ("Email", Some('e'))]),
            0,
            false,
            true, // focused pane
            &theme,
        );
        let ews = spans
            .iter()
            .find(|s| s.content == "ews")
            .expect("'ews' span should exist");
        assert_eq!(
            ews.style, theme.widget_title_focused,
            "active tab body should use widget_title.focused when pane focused"
        );
        let mail = spans
            .iter()
            .find(|s| s.content == "mail")
            .expect("'mail' span should exist");
        assert_eq!(mail.style, theme.text_dim, "inactive tab body should be dim");
    }

    #[test]
    fn build_tab_label_spans_active_uses_unfocused_style_when_pane_unfocused() {
        // The user picked highlighting over dim/bright precisely to
        // keep the active tab distinguishable in unfocused stacks —
        // active uses widget_title.unfocused (bold no-bg), inactive
        // tabs use text.dim. The two are visibly different even with
        // no focus.
        let theme = Theme::builtin_defaults();
        let spans = build_tab_label_spans(
            &tabs(&[("News", Some('n')), ("Email", Some('e'))]),
            0,
            false,
            false, // unfocused pane
            &theme,
        );
        let ews = spans
            .iter()
            .find(|s| s.content == "ews")
            .expect("'ews' span should exist");
        assert_eq!(
            ews.style, theme.widget_title_unfocused,
            "active tab body should use widget_title.unfocused when pane unfocused"
        );
        let mail = spans
            .iter()
            .find(|s| s.content == "mail")
            .expect("'mail' span should exist");
        assert_eq!(mail.style, theme.text_dim, "inactive tab body should be dim");
        assert_ne!(
            theme.widget_title_unfocused, theme.text_dim,
            "active-unfocused must visibly differ from inactive-dim"
        );
    }

    #[test]
    fn build_metadata_spans_uses_supplied_style() {
        let theme = Theme::builtin_defaults();
        let spans = build_metadata_spans(Some("47 articles"), theme.metadata_focused);
        let meta = spans
            .iter()
            .find(|s| s.content == "47 articles")
            .expect("metadata span should exist");
        assert_eq!(
            meta.style, theme.metadata_focused,
            "metadata body should adopt the style we passed in"
        );
    }

    #[test]
    fn build_metadata_spans_pads_with_single_spaces() {
        // No more ` ─ ` separator — metadata is right-aligned in its
        // own corner now, and the leading/trailing space pad lets a
        // bg color (if the scheme adds one) breathe.
        let theme = Theme::builtin_defaults();
        let spans = build_metadata_spans(Some("47 articles"), theme.metadata_focused);
        assert_eq!(spans.first().map(|s| s.content.as_ref()), Some(" "));
        assert_eq!(spans.last().map(|s| s.content.as_ref()), Some(" "));
    }

    #[test]
    fn build_metadata_spans_none_when_absent() {
        let theme = Theme::builtin_defaults();
        assert!(build_metadata_spans(None, theme.metadata_focused).is_empty());
        assert!(build_metadata_spans(Some(""), theme.metadata_focused).is_empty());
    }

    #[test]
    fn build_tab_label_spans_compact_mode_uses_initials() {
        let theme = Theme::builtin_defaults();
        let spans = build_tab_label_spans(
            &tabs(&[("Clock", Some('c')), ("Weather", Some('w')), ("Stocks", Some('s'))]),
            0,
            true,
            true,
            &theme,
        );
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        // Initials are uppercase; no arrows surround the active one.
        assert!(joined.contains('C'));
        assert!(joined.contains("W"));
        assert!(joined.contains("S"));
        assert!(!joined.contains("Clock"));
        assert!(!joined.contains('▶'));
        assert!(!joined.contains('◀'));
    }
}
