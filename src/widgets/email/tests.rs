// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Unit tests for the email widget. Split out of `mod.rs` per the repo standard.

use super::*;
use std::sync::Arc;
use crate::widgets::test_support::buffer_text;

#[test]
fn default_config_has_outlook_and_inbox() {
    let c = EmailConfig::default();
    assert_eq!(c.provider, "outlook");
    assert_eq!(c.folders, vec!["INBOX".to_string()]);
    assert_eq!(c.latest_days, 7);
    assert!(!c.summarize_with_llm);
}

#[test]
fn normalize_sender_prefers_display_name() {
    let n = normalize_sender(&Some("Alice Smith".into()), "alice@example.com", 20);
    assert_eq!(n, "Alice Smith");
}

#[test]
fn normalize_sender_falls_back_to_username() {
    let n = normalize_sender(&None, "bob@example.com", 20);
    assert_eq!(n, "bob");
}

#[test]
fn normalize_sender_truncates_oversized_names() {
    let n = normalize_sender(
        &Some("Reginald Bartholomew Worthington".into()),
        "rbw@example.com",
        10,
    );
    assert!(n.chars().count() <= 10);
    assert!(n.ends_with('…'));
}

#[test]
fn truncate_body_in_place_is_noop_under_cap() {
    let mut s = String::from("short body");
    truncate_body_in_place(&mut s, 4096);
    assert_eq!(s, "short body");
}

#[test]
fn truncate_body_in_place_caps_long_body_with_ellipsis() {
    let mut s = "x".repeat(10_000);
    truncate_body_in_place(&mut s, 4096);
    assert!(s.chars().count() <= 4096);
    assert!(s.ends_with('…'));
}

#[test]
fn truncate_body_in_place_respects_char_boundaries() {
    // Multi-byte chars near the cutoff must not produce an invalid
    // UTF-8 String. Use a body of 100 emoji glyphs (4 bytes each)
    // and cap at 60 chars.
    let mut s: String = "😀".repeat(100);
    truncate_body_in_place(&mut s, 60);
    assert!(s.chars().count() <= 60);
    assert!(s.ends_with('…'));
    // String must still be valid UTF-8 — implicit in being a String.
}

#[test]
fn normalize_sender_truncates_by_cell_width_for_cjk_names() {
    // Each CJK glyph occupies 2 terminal cells. A 10-cell budget
    // fits 4 full glyphs + the 1-cell ellipsis (4*2 + 1 = 9 cells)
    // — adding a 5th glyph would exceed the budget. Char count is
    // 5 (4 glyphs + ellipsis) which differs from cell width: this
    // is exactly the case that the old chars-based truncate got
    // wrong, overflowing the sender column and pushing the date
    // off the right edge of the row.
    use unicode_width::UnicodeWidthStr;
    let n = normalize_sender(&Some("中華航空公司歡迎您".into()), "ca@example.com", 10);
    assert!(
        UnicodeWidthStr::width(n.as_str()) <= 10,
        "rendered cell width must fit budget, got {n:?} ({} cells)",
        UnicodeWidthStr::width(n.as_str())
    );
    assert!(n.ends_with('…'));
}

#[test]
fn truncate_keeps_string_intact_when_under_cell_budget() {
    // Mixed emoji + ASCII: "🌸China" — `🌸` = 2 cells, "China" = 5
    // cells → 7 cells total, fits in 10.
    let out = truncate("🌸China", 10);
    assert_eq!(out, "🌸China", "no truncation needed");
}

#[test]
fn truncate_uses_cell_width_not_char_count_for_emoji() {
    // "🌸China Airlines🌸 Fly to Ho Chi Minh City with Unbeatable Fares ✨"
    // has 2-cell glyphs at the start, middle, and end. Truncating
    // to 30 cells must produce a string whose terminal width is
    // ≤ 30 — not whose char count is ≤ 30 (which would let the
    // string overflow the column).
    use unicode_width::UnicodeWidthStr;
    let subject = "🌸China Airlines🌸 Fly to Ho Chi Minh City with Unbeatable Fares ✨";
    let out = truncate(subject, 30);
    assert!(
        UnicodeWidthStr::width(out.as_str()) <= 30,
        "{out:?} should fit in 30 cells (got {} cells)",
        UnicodeWidthStr::width(out.as_str())
    );
    assert!(out.ends_with('…'));
}

#[test]
fn pad_or_truncate_pads_emoji_prefix_to_exact_cell_width() {
    // The bug: emoji-prefixed subjects were padded to N *chars*
    // not N *cells*, so the trailing date column ended up shifted
    // off the right edge of the pane. Cell-width padding fixes
    // this — `🌸hi` is 4 cells; padding to 10 adds 6 spaces.
    use unicode_width::UnicodeWidthStr;
    let out = pad_or_truncate("🌸hi", 10);
    assert_eq!(
        UnicodeWidthStr::width(out.as_str()),
        10,
        "padded output must occupy exactly the requested cell width"
    );
}

#[test]
fn normalize_sender_strips_quotes_around_name() {
    let n = normalize_sender(&Some("\"Carol\"".into()), "carol@example.com", 20);
    assert_eq!(n, "Carol");
}

#[test]
fn format_received_uses_hhmm_today() {
    let now = Local::now();
    let s = format_received(now, now);
    // Format is HH:MM
    assert_eq!(s.len(), 5);
    assert_eq!(&s[2..3], ":");
}

#[test]
fn format_received_uses_mmdd_other_days() {
    let now = Local::now();
    let earlier = now - chrono::Duration::days(3);
    let s = format_received(now, earlier);
    // Format is MM/DD
    assert_eq!(s.len(), 5);
    assert_eq!(&s[2..3], "/");
}

#[test]
fn placeholder_renders_when_provider_unconfigured() {
    // No env / no token = provider Empty → widget should be ready=false
    // and never panic on render. We exercise construction here; the
    // render path is covered by the integration check in the harness.
    let cfg = EmailConfig {
        provider: "unknown".into(),
        ..EmailConfig::default()
    };
    let w = EmailWidget::with_config(cfg);
    assert!(!w.provider_ready);
    assert!(w.auth_hint.is_some());
}

// ── Render tests (split-pane at Expanded/Full) ───────────────────────────────


/// Construct a widget with the provider bypassed and a set of test messages
/// pre-loaded into the in-memory state. `provider_ready` is forced true so
/// the render path doesn't bail out with the placeholder screen.
fn make_widget_with_messages(messages: Vec<provider::EmailMessage>) -> EmailWidget {
    let cfg = EmailConfig {
        // "unknown" avoids any credential-loading at construction time.
        provider: "unknown".into(),
        ..EmailConfig::default()
    };
    let mut widget = EmailWidget::with_config(cfg);
    widget.provider_ready = true;
    {
        let mut st = widget.state.lock().unwrap();
        st.messages = messages.into_iter().map(Arc::new).collect();
    }
    widget
}

/// Build a test `EmailMessage` whose body has `line_count` newline-separated
/// lines of the form "Body line N." so individual lines are easily searchable.
fn make_message(line_count: usize) -> provider::EmailMessage {
    use chrono::Local;
    let body = (0..line_count)
        .map(|i| format!("Body line {}.", i))
        .collect::<Vec<_>>()
        .join("\n");
    provider::EmailMessage {
        id: "test-msg-1".into(),
        folder: "INBOX".into(),
        from_name: Some("Alice Smith".into()),
        from_address: "alice@example.com".into(),
        subject: "Weekly Report".into(),
        received: Local::now(),
        server_unread: false,
        plain_body: body,
        web_url: None,
    }
}

/// At Standard size (50 × 20) the widget is below the Expanded threshold, so
/// no read pane is rendered. Body lines beyond MAX_SUMMARY_LINES must be absent
/// from the buffer — they would only appear if the read pane existed.
#[test]
fn standard_size_read_pane_absent() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let (w, h) = (50u16, 20u16);
    assert_eq!(
        crate::widgets::ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        crate::widgets::ViewTier::Standard,
        "precondition: 50×20 must be Standard tier"
    );

    // A body with 20 lines; MAX_SUMMARY_LINES=5, so "Body line 5." through
    // "Body line 19." are the lines that the read pane would reveal.
    let widget = make_widget_with_messages(vec![make_message(20)]);

    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            widget.render(frame, frame.area(), false);
        })
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());
    assert!(
        !text.contains("Body line 5."),
        "read pane must be absent at Standard size; body line 5 must not appear \
         (buffer snippet: {:?})",
        &text[..text.len().min(300)]
    );
    assert!(
        !text.contains("Body line 10."),
        "body line 10 must not appear at Standard size"
    );
}

/// At a genuinely wide size (260 × 30) the widget clears READ_PANE_MIN_WIDTH (250),
/// so the split-pane layout is active. The selected message's subject must appear
/// in the read pane header and body lines beyond MAX_SUMMARY_LINES (≥ line 5)
/// must be visible — proving the read pane is not subject to the list's 5-line cap.
#[test]
fn expanded_size_read_pane_shows_full_body() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    // 260 wide: after the 2-col border the list_area is 258 cols, which
    // clears READ_PANE_MIN_WIDTH (250) and activates the split.
    let (w, h) = (260u16, 30u16);
    assert_eq!(
        crate::widgets::ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        crate::widgets::ViewTier::Full,
        "precondition: 260×30 must be Full tier"
    );

    let widget = make_widget_with_messages(vec![make_message(20)]);

    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            widget.render(frame, frame.area(), false);
        })
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());
    // The subject appears in the read pane header.
    assert!(
        text.contains("Weekly Report"),
        "read pane header must contain the message subject; buffer snippet: {:?}",
        &text[..text.len().min(400)]
    );
    // Body line 5 is beyond MAX_SUMMARY_LINES=5 (0-indexed lines 0-4 are
    // the first five lines); it must appear in the read pane.
    assert!(
        text.contains("Body line 5."),
        "read pane must show body lines beyond MAX_SUMMARY_LINES; \
         'Body line 5.' must appear in buffer (snippet: {:?})",
        &text[..text.len().min(400)]
    );
    // The vertical divider glyph must appear in the 3-col gutter between
    // the list pane and the read pane.
    assert!(
        text.contains('│'),
        "vertical divider '│' must appear in the gutter at Full width; \
         buffer snippet: {:?}",
        &text[..text.len().min(400)]
    );
}

/// At Expanded size (100 × 30) the widget is above the Expanded tier threshold
/// but `list_area.width` (98) is well below `READ_PANE_MIN_WIDTH` (250), so the
/// split must NOT activate. Body lines beyond MAX_SUMMARY_LINES must be absent.
#[test]
fn expanded_tier_below_min_width_read_pane_absent() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    // 100 wide: Expanded tier (< FULL_MIN_W=105) and list_area=98 < 250.
    let (w, h) = (100u16, 30u16);
    assert_eq!(
        crate::widgets::ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        crate::widgets::ViewTier::Expanded,
        "precondition: 100×30 must be Expanded tier"
    );

    let widget = make_widget_with_messages(vec![make_message(20)]);

    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            widget.render(frame, frame.area(), false);
        })
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());
    assert!(
        !text.contains("Body line 5."),
        "read pane must be absent at 100 cols (below READ_PANE_MIN_WIDTH=250); \
         body line 5 must not appear (snippet: {:?})",
        &text[..text.len().min(400)]
    );
    assert!(
        !text.contains("Body line 10."),
        "body line 10 must not appear when read pane is absent at 100 cols"
    );
}

// ── read_pane_active / e-key gating ─────────────────────────────────────────

/// At 260 × 30 (list_area = 258 ≥ READ_PANE_MIN_WIDTH = 250) the read pane
/// fires. After one render, `read_pane_active` must be true, and pressing `e`
/// must not toggle `expanded` — the inline body expand is suppressed while the
/// reading pane already shows the full body.
#[test]
fn wide_rect_e_key_does_not_inline_expand() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let (w, h) = (260u16, 30u16);
    let mut widget = make_widget_with_messages(vec![make_message(10)]);

    // Render once so the render path sets read_pane_active on the state.
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            widget.render(frame, frame.area(), true);
        })
        .unwrap();

    assert!(
        widget.state.lock().unwrap().read_pane_active,
        "read_pane_active must be true after rendering at 260 cols"
    );

    // e must be a no-op for the inline expand while the read pane is live.
    widget.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
    assert!(
        !widget.state.lock().unwrap().expanded,
        "expanded must remain false after pressing e with read pane active"
    );

    // Enter must also be a no-op.
    widget.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(
        !widget.state.lock().unwrap().expanded,
        "expanded must remain false after pressing Enter with read pane active"
    );
}

/// At a narrow rect (50 × 20, Standard tier) the read pane is absent.
/// `read_pane_active` must be false (the default), and pressing `e` must
/// toggle the inline expand as it always did.
#[test]
fn narrow_rect_e_key_toggles_inline_expand() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut widget = make_widget_with_messages(vec![make_message(10)]);

    // No render needed: read_pane_active defaults to false.
    assert!(
        !widget.state.lock().unwrap().read_pane_active,
        "read_pane_active must default to false"
    );

    widget.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
    assert!(
        widget.state.lock().unwrap().expanded,
        "expanded must become true after pressing e with no read pane"
    );

    // Second press collapses.
    widget.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
    assert!(
        !widget.state.lock().unwrap().expanded,
        "expanded must return to false after second e press"
    );
}
