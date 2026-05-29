// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Stack widget — a layout cell that holds up to 3 widgets and shows
//! one at a time, with a tab strip in the title bar for switching.
//!
//! Related code lives in `config::layout` (schema),
//! `wizard::pages::assign_stack` (wizard UI), `runtime_state`
//! (active-tab persistence), and `app` (Shift+<letter> routing walks
//! into stacks).
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

    fn take_dirty(&mut self) -> bool {
        // Only the active child's dirty bit drives a redraw — hidden
        // children paint nothing, and we deliberately leave their bits
        // pending so the next rotation/switch into them surfaces any
        // queued state changes on the first draw after they appear.
        self.children
            .get_mut(self.active)
            .map(|c| c.take_dirty())
            .unwrap_or(false)
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
        // Give the active child first crack at every key. Widgets that
        // accept arbitrary text input (Notes in insert mode is the
        // canonical example) must be able to consume `.` and `,` as
        // text without the stack swallowing them as rotation chords.
        // The child returns `Ignored` for keys it doesn't claim —
        // only then does the stack's `.` / `,` rotation kick in.
        if let Some(child) = self.children.get_mut(self.active) {
            match child.handle_key(key) {
                EventResult::Handled => return EventResult::Handled,
                EventResult::Ignored => {}
            }
        }
        match key.code {
            KeyCode::Char(',') => {
                self.rotate_prev();
                EventResult::Handled
            }
            KeyCode::Char('.') => {
                self.rotate_next();
                EventResult::Handled
            }
            _ => EventResult::Ignored,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, area: Rect) -> EventResult {
        // Left-click on a tab title flips the stack to that child. Any
        // other mouse event (scroll, drag, click below the tab row, …)
        // falls through to the active child so the existing per-widget
        // mouse semantics still apply.
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            let layout = self
                .tab_layout
                .lock()
                .expect("stack tab_layout poisoned")
                .clone();
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
        // Stack itself doesn't own commands. When a child claims, raise
        // it to active so the user actually sees the effect — `:news
        // nvidia` running on a hidden tab is invisible feedback and
        // strictly worse than the no-stack case.
        for idx in 0..self.children.len() {
            if self.children[idx].handle_command(cmd, args)? {
                if self.active != idx {
                    self.active = idx;
                    self.refresh_active_name();
                }
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
                spans.push(Span::styled("─".repeat(filler_count), border_style));
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
        .map(|(s, e)| (base.saturating_add(s) as u16, base.saturating_add(e) as u16))
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
    let active_pad_style = if focused {
        theme.border_focused
    } else {
        border_style
    };

    let mut out: Vec<Span<'static>> = Vec::with_capacity(tabs.len() * 5 + 1);
    let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(tabs.len());
    let last_idx = tabs.len().saturating_sub(1);

    // Char cursor — incremented as we push spans so we can record each
    // tab's [start, end) offsets for click hit-testing.
    let mut pos: usize = 0;
    let push = |out: &mut Vec<Span<'static>>, pos: &mut usize, content: String, style: Style| {
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
            push(
                &mut out,
                &mut pos,
                active_left.to_string(),
                active_pad_style,
            );
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
            push(
                &mut out,
                &mut pos,
                active_right.to_string(),
                active_pad_style,
            );
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
mod tests;
