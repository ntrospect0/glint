// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Unit tests for the stack widget. Split out of `mod.rs` per the repo standard.

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

/// When the active child claims a key (e.g. Notes in insert mode
/// consuming every Char as text input), the stack must NOT
/// interpret that same key as a rotation chord. Otherwise pasting
/// any text containing `.` rotates the stack instead of typing.
#[tokio::test]
async fn child_handled_key_suppresses_stack_rotation() {
    // A stub that always claims every key — stands in for an
    // in-insert-mode Notes widget.
    struct GreedyChild {
        id: String,
    }
    #[async_trait]
    impl WidgetTrait for GreedyChild {
        fn id(&self) -> &str {
            &self.id
        }
        fn display_name(&self) -> &str {
            &self.id
        }
        fn kind(&self) -> &str {
            "greedy"
        }
        async fn update(&mut self, _ctx: &AppContext) -> Result<()> {
            Ok(())
        }
        fn render(&self, _frame: &mut Frame, _area: Rect, _focused: bool) {}
        fn handle_key(&mut self, _key: KeyEvent) -> EventResult {
            EventResult::Handled
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
    let theme = std::sync::Arc::new(Theme::builtin_defaults());
    let mut stack = StackWidget::new(
        "stack:g+g".to_string(),
        vec![
            Box::new(GreedyChild { id: "g1".into() }),
            Box::new(GreedyChild { id: "g2".into() }),
        ],
        1,
        theme,
    );
    let before = stack.active;
    stack.handle_key(KeyEvent::from(KeyCode::Char('.')));
    stack.handle_key(KeyEvent::from(KeyCode::Char(',')));
    assert_eq!(stack.active, before, "greedy child must suppress rotation");
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
    assert_eq!(
        *counters[0].lock().unwrap(),
        6,
        "active child should update every tick"
    );
    assert_eq!(
        *counters[1].lock().unwrap(),
        2,
        "hidden child should be throttled"
    );
    assert_eq!(
        *counters[2].lock().unwrap(),
        2,
        "hidden child should be throttled"
    );
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

/// A `:news nvidia` typed by the user wants visible feedback. If
/// the news widget lives on a non-active tab, the stack has to
/// raise it on claim — otherwise the search runs invisibly under
/// whatever was on top.
#[tokio::test]
async fn command_claim_raises_child_to_active() {
    struct ClaimsCmd {
        id: String,
        cmd_match: &'static str,
    }
    #[async_trait]
    impl WidgetTrait for ClaimsCmd {
        fn id(&self) -> &str {
            &self.id
        }
        fn display_name(&self) -> &str {
            &self.id
        }
        fn kind(&self) -> &str {
            "claims-cmd"
        }
        async fn update(&mut self, _ctx: &AppContext) -> Result<()> {
            Ok(())
        }
        fn render(&self, _frame: &mut Frame, _area: Rect, _focused: bool) {}
        fn handle_key(&mut self, _key: KeyEvent) -> EventResult {
            EventResult::Ignored
        }
        fn handle_command(&mut self, cmd: &str, _args: &[&str]) -> Result<bool> {
            Ok(cmd == self.cmd_match)
        }
        fn config(&self) -> serde_json::Value {
            serde_json::json!(null)
        }
        fn apply_config(&mut self, _config: serde_json::Value) -> Result<()> {
            Ok(())
        }
    }
    let theme = std::sync::Arc::new(Theme::builtin_defaults());
    let mut stack = StackWidget::new(
        "stack:email+news".to_string(),
        vec![
            Box::new(ClaimsCmd {
                id: "email".into(),
                cmd_match: "email",
            }),
            Box::new(ClaimsCmd {
                id: "news".into(),
                cmd_match: "news",
            }),
        ],
        1,
        theme,
    );
    assert_eq!(stack.active, 0, "stack defaults to first child");
    let claimed = stack.handle_command("news", &["nvidia"]).unwrap();
    assert!(claimed, "news child must claim its own command");
    assert_eq!(
        stack.active, 1,
        "stack must raise the claiming child to active"
    );
}

/// Inverse: a command no child claims must not disturb the active
/// tab. Otherwise typing an unrelated command would silently
/// reshuffle which widget the user is looking at.
#[tokio::test]
async fn unclaimed_command_leaves_active_unchanged() {
    let (mut stack, _) = build_stack(1);
    stack.handle_key(KeyEvent::from(KeyCode::Char('.')));
    assert_eq!(stack.active, 1);
    let claimed = stack.handle_command("bogus", &[]).unwrap();
    assert!(!claimed);
    assert_eq!(stack.active, 1, "no claim → active stays put");
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
    assert_eq!(
        slice(ranges[0]),
        "┤News├",
        "active tab body includes both tees"
    );
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
        &tabs(&[
            ("Clock", Some('c')),
            ("Weather", Some('w')),
            ("Stocks", Some('s')),
        ]),
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
        &tabs(&[
            ("Clock", Some('c')),
            ("Weather", Some('w')),
            ("News", Some('n')),
        ]),
        1, // Weather active
        false,
        true, // focused
        &theme,
    );
    let lefts: Vec<_> = spans.iter().filter(|s| s.content.as_ref() == "┤").collect();
    let rights: Vec<_> = spans.iter().filter(|s| s.content.as_ref() == "├").collect();
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
        &tabs(&[
            ("News", Some('n')),
            ("Email", Some('e')),
            ("Stocks", Some('s')),
        ]),
        1,
        false,
        true,
        &theme,
    );
    let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        joined.contains(" ─┤"),
        "separator before active should flush against `┤`"
    );
    assert!(
        joined.contains("├─ "),
        "active should flush against the separator after `├`"
    );
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
    assert_eq!(
        last.content.as_ref(),
        "├",
        "last span should be `├` with no trailing space"
    );
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
    let unfocused_text: String = unfocused.iter().map(|s| s.content.as_ref()).collect();
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
    assert_eq!(
        mail.style, theme.text_dim,
        "inactive tab body should be dim"
    );
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
    assert_eq!(
        mail.style, theme.text_dim,
        "inactive tab body should be dim"
    );
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
        &tabs(&[
            ("Clock", Some('c')),
            ("Weather", Some('w')),
            ("Stocks", Some('s')),
        ]),
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
