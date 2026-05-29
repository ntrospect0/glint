// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Unit tests for the email widget. Split out of `mod.rs` per the repo standard.

use super::*;

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
