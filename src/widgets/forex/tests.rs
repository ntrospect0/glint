// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Unit tests for the forex widget. Split out of `mod.rs` per the repo standard.

use super::*;
use crate::widgets::test_support::buffer_text;

// ─────────────────────────────────────────────────────────────────────
// Render-tier tests (TestBackend)
// ─────────────────────────────────────────────────────────────────────

/// Collect the symbols from a single row over a half-open column range
/// `[x_start, x_end)`. Useful for asserting positional constraints.
fn buffer_row_range(buf: &ratatui::buffer::Buffer, y: u16, x_start: u16, x_end: u16) -> String {
    let mut out = String::new();
    for x in x_start..x_end {
        out.push_str(buf[(x, y)].symbol());
    }
    out
}

/// At Standard size (50 × 20) the terminal is too narrow for the stats
/// column (with_stats = false), so the 52-week range bar must not appear
/// anywhere in the buffer.
#[test]
fn standard_size_hides_expanded_strip() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let (w, h) = (50u16, 20u16);
    assert_eq!(
        crate::widgets::ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        crate::widgets::ViewTier::Standard,
        "precondition: 50×20 must resolve to Standard"
    );

    let (widget, _guard) = build_widget_isolated(ForexConfig::default());
    let q = Arc::new(ForexQuote {
        symbol: "USDEUR=X".into(),
        base: "USD".into(),
        quote: "EUR".into(),
        price: 0.9237,
        previous_close: 0.9200,
        day_high: None,
        day_low: None,
        fifty_two_week_high: Some(1.0500),
        fifty_two_week_low: Some(0.8800),
        series: (0..50).map(|i| 0.88 + i as f64 * 0.0034).collect(),
        series_timestamps: vec![],
        fetched_at: chrono::Local::now(),
    });
    widget
        .state
        .lock()
        .unwrap()
        .quotes
        .insert("USDEUR=X".into(), QuoteState::Ready(q));
    select_by_symbol(&widget, "EUR");

    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());
    assert!(
        !text.contains("52w"),
        "range bar must be absent when stats column is not rendered (50×20); buffer snippet: {:?}",
        &text[..text.len().min(300)]
    );
}

/// At Expanded size (90 × 25), a direct pair with 52wk data must render
/// the range bar — "52w" and the `●` marker appear in the buffer.
/// 90 cols gives body.width=88 which satisfies with_stats (≥ 81), so the
/// stats column is present and the bar renders inside it.
#[test]
fn expanded_size_shows_52w_range_for_direct_pair() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let (w, h) = (90u16, 25u16);
    assert_eq!(
        crate::widgets::ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        crate::widgets::ViewTier::Expanded,
        "precondition: 90×25 must resolve to Expanded"
    );

    let (widget, _guard) = build_widget_isolated(ForexConfig::default());
    let q = Arc::new(ForexQuote {
        symbol: "USDEUR=X".into(),
        base: "USD".into(),
        quote: "EUR".into(),
        price: 0.9237,
        previous_close: 0.9200,
        day_high: None,
        day_low: None,
        fifty_two_week_high: Some(1.0500),
        fifty_two_week_low: Some(0.8800),
        series: (0..50).map(|i| 0.88 + i as f64 * 0.0034).collect(),
        series_timestamps: vec![],
        fetched_at: chrono::Local::now(),
    });
    widget
        .state
        .lock()
        .unwrap()
        .quotes
        .insert("USDEUR=X".into(), QuoteState::Ready(q));
    select_by_symbol(&widget, "EUR");

    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());
    assert!(
        text.contains("52w"),
        "expanded strip must render '52w' at Expanded size; buffer snippet: {:?}",
        &text[..text.len().min(400)]
    );
    // The range bar marker must appear.
    assert!(
        text.contains('●'),
        "range bar marker '●' must appear for a direct pair at Expanded size"
    );
}

/// A cross pair (fifty_two_week_high/low = None) at Expanded size must not
/// crash and must render the "52w —" graceful placeholder instead of a bar.
/// 90 cols ensures with_stats=true so the stats column (and the bar line) exist.
#[test]
fn expanded_size_cross_pair_degrades_gracefully() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let (w, h) = (90u16, 25u16);
    let (widget, _guard) = build_widget_isolated(ForexConfig {
        primary: "EUR".into(),
        watchlist: vec!["JPY".into()],
        crypto_watchlist: Vec::new(),
        ..Default::default()
    });
    // Cross pair synthesised via USD pivot — 52wk fields intentionally None.
    let cross = Arc::new(ForexQuote {
        symbol: "EURJPY=X".into(),
        base: "EUR".into(),
        quote: "JPY".into(),
        price: 163.5,
        previous_close: 162.0,
        day_high: None,
        day_low: None,
        fifty_two_week_high: None,
        fifty_two_week_low: None,
        series: (0..50).map(|i| 160.0 + i as f64 * 0.1).collect(),
        series_timestamps: vec![],
        fetched_at: chrono::Local::now(),
    });
    widget
        .state
        .lock()
        .unwrap()
        .quotes
        .insert("EURJPY=X".into(), QuoteState::Ready(cross));
    select_by_symbol(&widget, "JPY");

    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    // Must not panic.
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());
    assert!(
        text.contains("52w"),
        "cross pair must still render '52w' placeholder; buffer snippet: {:?}",
        &text[..text.len().min(400)]
    );
    // No range-bar marker — there is no 52wk window to position it against.
    assert!(
        !text.contains('●'),
        "cross pair must not render '●' range-bar marker (no 52wk data)"
    );
}

/// After a copy, the row that was yanked should render `✅` in
/// place of `📋` until the pulse expires. Same glyph slot, same
/// hit-rect footprint — clicking the cell again mid-pulse must
/// still register as a copy.
#[test]
fn build_list_row_swaps_clipboard_for_check_when_pulsing() {
    let theme = Theme::builtin_defaults();
    let (line, hit) = build_list_row("EUR", false, true, Some(1.0823), None, true, &theme);
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.contains('✅'), "pulse should render ✅: {text:?}");
    assert!(
        !text.contains('📋'),
        "📋 should be absent during pulse: {text:?}"
    );
    assert!(
        hit.copy_present,
        "✅ cell must remain clickable so mid-pulse re-copy works"
    );
}

/// Default rendering (no pulse) keeps the `📋` clipboard glyph.
#[test]
fn build_list_row_renders_clipboard_glyph_when_not_pulsing() {
    let theme = Theme::builtin_defaults();
    let (line, _hit) = build_list_row("EUR", false, true, Some(1.0823), None, false, &theme);
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.contains('📋'), "default should render 📋: {text:?}");
    assert!(!text.contains('✅'), "no pulse → no ✅: {text:?}");
}

#[test]
fn build_alternates_concats_lists_dedupes_filters_primary() {
    let fiat: Vec<String> = ["EUR", "JPY", "USD", "eur"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let crypto: Vec<String> = ["BTC", "ETH", "btc"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let (out, cs) = build_alternates(&fiat, &crypto, "USD");
    assert_eq!(out, vec!["EUR", "JPY", "BTC", "ETH"]);
    assert_eq!(cs, 2, "crypto section starts after the two fiat entries");
}

#[test]
fn build_alternates_returns_empty_when_all_are_primary() {
    let fiat = vec!["usd".into(), "USD".into()];
    let crypto: Vec<String> = vec![];
    let (out, cs) = build_alternates(&fiat, &crypto, "USD");
    assert!(out.is_empty());
    assert_eq!(cs, 0);
}

#[test]
fn build_alternates_handles_only_crypto() {
    let (out, cs) = build_alternates(&[], &["BTC".into(), "ETH".into()], "USD");
    assert_eq!(out, vec!["BTC", "ETH"]);
    assert_eq!(cs, 0, "no fiat entries → crypto starts at index 0");
}

/// RAII guard that points `XDG_CONFIG_HOME` at an empty per-thread temp
/// directory for the lifetime of the guard. Ensures `runtime_state::load()`
/// finds no persisted period/selection and returns defaults, so widget
/// construction is deterministic regardless of what the user has saved on
/// disk. Mirrors the `TempHome` pattern used in `notes/tests.rs`.
///
/// Using a thread-unique path (pid + thread id) avoids clobbering other
/// test threads that might set `XDG_CONFIG_HOME` for their own isolation.
struct TempConfigDir(std::path::PathBuf);
impl TempConfigDir {
    fn set() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "glint-forex-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Safety: test-only; each thread gets its own path so the env
        // var is at worst overwritten by another test pointing at an
        // equally-empty temp dir.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        TempConfigDir(dir)
    }
}
impl Drop for TempConfigDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Build a forex widget with an empty runtime-state environment so
/// persisted selection/period from the user's real config can't leak
/// into tests. The `_guard` must be held for the duration of the test
/// body (drop at end of scope, not immediately after construction).
fn build_widget_isolated(cfg: ForexConfig) -> (ForexWidget, TempConfigDir) {
    let guard = TempConfigDir::set();
    let widget = ForexWidget::with_config(
        "main".to_string(),
        cfg,
        Arc::new(Theme::builtin_defaults()),
        ScopedCache::ephemeral(),
    );
    (widget, guard)
}

fn build_widget(cfg: ForexConfig) -> ForexWidget {
    ForexWidget::with_config(
        "main".to_string(),
        cfg,
        Arc::new(Theme::builtin_defaults()),
        ScopedCache::ephemeral(),
    )
}

/// Set the widget's selection to the row whose currency code matches
/// `symbol` (case-insensitive). Panics if the symbol is not found so
/// the test fails with a clear message rather than silently using the
/// wrong row.
fn select_by_symbol(widget: &ForexWidget, symbol: &str) {
    let rows = widget.all_rows();
    let idx = rows
        .iter()
        .position(|r| r.eq_ignore_ascii_case(symbol))
        .unwrap_or_else(|| panic!("symbol {symbol:?} not found in forex rows: {rows:?}"));
    widget.state.lock().unwrap().selected = idx;
}

fn rate(base: &str, quote: &str, price: f64) -> ForexQuote {
    ForexQuote {
        symbol: YahooForexProvider::symbol_for(base, quote),
        base: base.into(),
        quote: quote.into(),
        price,
        previous_close: price,
        day_high: None,
        day_low: None,
        fifty_two_week_high: None,
        fifty_two_week_low: None,
        series: vec![],
        series_timestamps: vec![],
        fetched_at: chrono::Local::now(),
    }
}

#[test]
fn default_canonical_unit_handles_jpy_and_krw() {
    assert_eq!(default_canonical_unit("USD"), 1.0);
    assert_eq!(default_canonical_unit("EUR"), 1.0);
    assert_eq!(default_canonical_unit("JPY"), 100.0);
    assert_eq!(default_canonical_unit("KRW"), 1000.0);
    assert_eq!(default_canonical_unit("VND"), 10_000.0);
    // Unknown codes fall back to 1.
    assert_eq!(default_canonical_unit("ZZZ"), 1.0);
}

#[test]
fn config_canonical_unit_override_wins_over_default() {
    let mut cfg = ForexConfig::default();
    cfg.canonical_units.insert("JPY".into(), 1.0); // override "100"
    assert_eq!(canonical_amount(&cfg, "JPY"), 1.0);
    // Code not in override map still falls through to default.
    assert_eq!(canonical_amount(&cfg, "KRW"), 1000.0);
}

#[test]
fn initial_amount_is_canonical_for_configured_primary() {
    let cfg = ForexConfig {
        primary: "JPY".into(),
        crypto_watchlist: Vec::new(),
        ..Default::default()
    };
    let w = build_widget(cfg);
    assert_eq!(w.amount, 100.0);
    assert_eq!(w.primary, "JPY");
}

#[test]
fn all_rows_lists_primary_first_then_non_duplicate_watchlist() {
    let cfg = ForexConfig {
        primary: "USD".into(),
        watchlist: vec!["EUR".into(), "USD".into(), "JPY".into()],
        crypto_watchlist: Vec::new(),
        ..Default::default()
    };
    let w = build_widget(cfg);
    // Primary shouldn't appear twice even when present in the watchlist.
    assert_eq!(w.all_rows(), vec!["USD", "EUR", "JPY"]);
}

#[test]
fn swap_primary_preserves_absolute_value_via_current_rate() {
    let cfg = ForexConfig {
        primary: "USD".into(),
        watchlist: vec!["EUR".into(), "JPY".into()],
        crypto_watchlist: Vec::new(),
        ..Default::default()
    };
    let mut w = build_widget(cfg);
    w.amount = 1523.80;
    // Pretend the rate is 1 USD = 0.9237 EUR.
    let usd_eur = rate("USD", "EUR", 0.9237);
    w.state
        .lock()
        .unwrap()
        .quotes
        .insert("USDEUR=X".into(), QuoteState::Ready(Arc::new(usd_eur)));
    w.swap_primary("EUR");
    assert_eq!(w.primary, "EUR");
    // 1523.80 USD × 0.9237 = 1407.53406 EUR
    assert!((w.amount - 1407.53406).abs() < 0.001, "amount={}", w.amount);
}

#[test]
fn swap_to_crypto_primary_forces_amount_to_one() {
    // Fiat-amount preservation is the wrong default for crypto:
    // "1523.80 USD × 1/90,000 BTC ≈ 0.01693 BTC" buries the
    // per-unit comparison the user actually wants from a crypto
    // primary. We always seed crypto primaries with 1.0.
    let mut w = build_widget(ForexConfig {
        primary: "USD".into(),
        watchlist: vec!["EUR".into()],
        crypto_watchlist: vec!["BTC".into()],
        ..Default::default()
    });
    w.amount = 1523.80;
    // Seed a rate so the converted-amount path would otherwise
    // produce a non-1.0 value; the crypto branch must override.
    let usd_btc = rate("USD", "BTC", 1.0 / 90_000.0);
    w.state.lock().unwrap().quotes.insert(
        YahooForexProvider::symbol_for("USD", "BTC"),
        QuoteState::Ready(Arc::new(usd_btc)),
    );
    w.swap_primary("BTC");
    assert_eq!(w.primary, "BTC");
    assert_eq!(w.amount, 1.0, "crypto primary always seeds amount=1.0");
}

#[test]
fn swap_primary_auto_selects_old_primary_row() {
    let cfg = ForexConfig {
        primary: "USD".into(),
        watchlist: vec!["EUR".into(), "JPY".into()],
        crypto_watchlist: Vec::new(),
        ..Default::default()
    };
    let mut w = build_widget(cfg);
    // Seed a rate so swap math doesn't fall back to canonical.
    w.state.lock().unwrap().quotes.insert(
        "USDEUR=X".into(),
        QuoteState::Ready(Arc::new(rate("USD", "EUR", 0.9))),
    );
    w.swap_primary("EUR");
    let rows = w.all_rows();
    let sel = w.state.lock().unwrap().selected;
    assert_eq!(rows.get(sel).map(|s| s.as_str()), Some("USD"));
}

#[test]
fn swap_key_promotes_selected_row_to_primary() {
    // Drive the `s` shortcut through handle_key so we exercise the
    // same path the user does, not just the helper.
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut w = build_widget(ForexConfig {
        primary: "USD".into(),
        watchlist: vec!["EUR".into(), "JPY".into()],
        crypto_watchlist: Vec::new(),
        ..Default::default()
    });
    // Seed a rate so swap_primary has something to convert through.
    w.state.lock().unwrap().quotes.insert(
        "USDEUR=X".into(),
        QuoteState::Ready(Arc::new(rate("USD", "EUR", 0.9))),
    );
    // Move selection to EUR and hit `s`.
    select_by_symbol(&w, "EUR");
    let _ = w.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
    assert_eq!(w.primary, "EUR");
}

#[test]
fn enter_key_also_promotes_selected_row_to_primary() {
    // Enter is an alias for `s` (the more discoverable / bigger
    // key gets the more common action).
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut w = build_widget(ForexConfig {
        primary: "USD".into(),
        watchlist: vec!["EUR".into(), "JPY".into()],
        crypto_watchlist: Vec::new(),
        ..Default::default()
    });
    w.state.lock().unwrap().quotes.insert(
        "USDEUR=X".into(),
        QuoteState::Ready(Arc::new(rate("USD", "EUR", 0.9))),
    );
    select_by_symbol(&w, "EUR");
    let _ = w.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(w.primary, "EUR");
}

#[test]
fn swap_key_is_safe_when_no_alternates_exist() {
    // Empty watchlist → only the primary row, selection stuck at 0.
    // Pressing `s` shouldn't swap (it would try to swap USD with
    // USD, which is the no-op branch of swap_primary).
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut w = build_widget(ForexConfig {
        primary: "USD".into(),
        watchlist: vec![],
        crypto_watchlist: Vec::new(),
        ..Default::default()
    });
    let amount_before = w.amount;
    let _ = w.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
    assert_eq!(w.primary, "USD");
    assert_eq!(w.amount, amount_before);
}

#[test]
fn move_selection_skips_primary_row() {
    // Initial selected = first alternate (EUR). ↑ should stay on EUR
    // rather than slipping to row 0 (the primary row, which has no
    // graph / stats to surface).
    let (mut w, _guard) = build_widget_isolated(ForexConfig {
        primary: "USD".into(),
        watchlist: vec!["EUR".into(), "JPY".into()],
        crypto_watchlist: Vec::new(),
        ..Default::default()
    });
    // Verify initial selection lands on EUR (first alternate, row 1).
    {
        let rows = w.all_rows();
        let sel = w.state.lock().unwrap().selected;
        assert_eq!(
            rows.get(sel).map(|s| s.as_str()),
            Some("EUR"),
            "initial selection must be EUR (first alternate)"
        );
    }
    w.move_selection(-1);
    {
        let rows = w.all_rows();
        let sel = w.state.lock().unwrap().selected;
        assert_eq!(
            rows.get(sel).map(|s| s.as_str()),
            Some("EUR"),
            "↑ at top alternate should stay on EUR, not drop to primary"
        );
    }
    w.move_selection(1);
    {
        let rows = w.all_rows();
        let sel = w.state.lock().unwrap().selected;
        assert_eq!(
            rows.get(sel).map(|s| s.as_str()),
            Some("JPY"),
            "↓ from EUR should land on JPY"
        );
    }
    // Two moves down past the last alternate clamps at the last row.
    w.move_selection(5);
    {
        let rows = w.all_rows();
        let sel = w.state.lock().unwrap().selected;
        assert_eq!(
            rows.get(sel).map(|s| s.as_str()),
            Some("JPY"),
            "extra ↓ past last alternate should clamp at JPY"
        );
    }
}

#[test]
fn swap_to_same_primary_is_no_op() {
    let mut w = build_widget(ForexConfig::default());
    let amount_before = w.amount;
    let primary_before = w.primary.clone();
    w.swap_primary("USD"); // already primary
    assert_eq!(w.amount, amount_before);
    assert_eq!(w.primary, primary_before);
}

#[test]
fn reset_amount_returns_to_canonical_for_current_primary() {
    let mut w = build_widget(ForexConfig {
        primary: "USD".into(),
        crypto_watchlist: Vec::new(),
        ..Default::default()
    });
    w.amount = 9999.0;
    w.reset_amount();
    assert_eq!(w.amount, 1.0);
}

#[test]
fn edit_commit_parses_decimal_buffer() {
    let mut w = build_widget(ForexConfig::default());
    w.enter_edit_mode();
    {
        let mut st = w.state.lock().unwrap();
        st.editing_amount = Some("1523.80".to_string());
    }
    let changed = w.commit_edit();
    assert!(changed);
    assert!((w.amount - 1523.80).abs() < f64::EPSILON);
    // Edit buffer cleared after commit.
    assert!(w.state.lock().unwrap().editing_amount.is_none());
}

#[test]
fn edit_cancel_drops_buffer_without_changing_amount() {
    let w = build_widget(ForexConfig::default());
    let amount_before = w.amount;
    w.enter_edit_mode();
    {
        let mut st = w.state.lock().unwrap();
        st.editing_amount = Some("999".to_string());
    }
    w.cancel_edit();
    assert_eq!(w.amount, amount_before);
    assert!(w.state.lock().unwrap().editing_amount.is_none());
}

#[test]
fn edit_commit_rejects_garbage_and_negative_input() {
    let mut w = build_widget(ForexConfig::default());
    let amount_before = w.amount;
    w.enter_edit_mode();
    {
        let mut st = w.state.lock().unwrap();
        st.editing_amount = Some("not a number".to_string());
    }
    assert!(!w.commit_edit());
    assert_eq!(w.amount, amount_before);

    w.enter_edit_mode();
    {
        let mut st = w.state.lock().unwrap();
        st.editing_amount = Some("-5".to_string());
    }
    assert!(!w.commit_edit());
    assert_eq!(w.amount, amount_before);
}

#[test]
fn fx_command_known_code_bounces_selection_does_not_swap() {
    // EUR is already in the alternates list, so :fx EUR just
    // selects EUR's row — it does NOT promote it to primary. Swap
    // is an explicit `p` / click-↔ gesture.
    let mut w = build_widget(ForexConfig {
        primary: "USD".into(),
        watchlist: vec!["EUR".into(), "JPY".into()],
        crypto_watchlist: Vec::new(),
        ..Default::default()
    });
    // Move selection off EUR first so we can prove the bounce works.
    select_by_symbol(&w, "JPY");
    w.handle_fx_command(&["EUR"]).unwrap();
    assert_eq!(w.primary, "USD", ":fx must not swap");
    // After :fx EUR the selection must land on EUR's row. Assert by symbol
    // rather than by raw index so it's order-independent.
    let rows = w.all_rows();
    let sel = w.state.lock().unwrap().selected;
    assert_eq!(
        rows.get(sel).map(|s| s.as_str()),
        Some("EUR"),
        ":fx on a known code should bounce selection to EUR's row"
    );
}

#[test]
fn fx_command_rejects_non_iso_codish_input() {
    let mut w = build_widget(ForexConfig::default());
    assert!(w.handle_fx_command(&["dollar"]).is_err());
    assert!(w.handle_fx_command(&["YENS!"]).is_err());
    assert!(w.handle_fx_command(&[""]).is_err());
    assert!(w.handle_fx_command(&[]).is_err());
}

#[test]
fn fx_command_unknown_code_pins_as_transient_lookup_does_not_swap() {
    // NOK isn't in the watchlist — :fx NOK pins it as the Lookup
    // row but leaves USD as primary. User must press `p` or click
    // ↔ to actually promote.
    let mut w = build_widget(ForexConfig {
        primary: "USD".into(),
        watchlist: vec!["EUR".into()],
        crypto_watchlist: Vec::new(),
        ..Default::default()
    });
    assert!(w.handle_fx_command(&["NOK"]).is_ok());
    assert_eq!(w.primary, "USD", ":fx must not swap");
    assert_eq!(
        w.state.lock().unwrap().transient_code.as_deref(),
        Some("NOK"),
        "NOK should be pinned as the Lookup row"
    );
    // Selection lands on the new Lookup row so the graph/stats
    // panels surface the pair immediately.
    let rows = w.all_rows();
    let sel = w.state.lock().unwrap().selected;
    assert_eq!(rows.get(sel).map(|s| s.as_str()), Some("NOK"));
}

#[test]
fn swap_moves_old_primary_to_top_of_alternates() {
    // Configured: USD primary with [EUR, JPY] alternates. After
    // swap to EUR: primary=EUR, alternates=[USD, JPY] (USD at
    // position 0 of alternates, which is row 1 of the full list).
    let mut w = build_widget(ForexConfig {
        primary: "USD".into(),
        watchlist: vec!["EUR".into(), "JPY".into()],
        crypto_watchlist: Vec::new(),
        ..Default::default()
    });
    // Seed a rate so the amount-conversion path doesn't fall back.
    w.state.lock().unwrap().quotes.insert(
        "USDEUR=X".into(),
        QuoteState::Ready(Arc::new(rate("USD", "EUR", 0.9))),
    );
    w.swap_primary("EUR");
    assert_eq!(w.primary, "EUR");
    assert_eq!(w.alternates, vec!["USD".to_string(), "JPY".to_string()]);
    // Selection auto-lands on the first alternate = USD.
    let rows = w.all_rows();
    let sel = w.state.lock().unwrap().selected;
    assert_eq!(rows.get(sel).map(|s| s.as_str()), Some("USD"));
}

#[test]
fn configured_primary_stays_anchored_across_multi_hop_swaps() {
    // The bug: USD→BTC→ETH used to drop USD because every swap
    // rebuilds from `config.{watchlist, crypto_watchlist}` (which
    // never contain USD) and only the *immediately previous*
    // primary got re-promoted. After the second hop, the
    // previous primary became BTC and USD fell off the list.
    let mut w = build_widget(ForexConfig {
        primary: "USD".into(),
        watchlist: vec!["EUR".into(), "JPY".into()],
        crypto_watchlist: vec!["BTC".into(), "ETH".into(), "SOL".into()],
        ..Default::default()
    });
    // USD → BTC
    w.swap_primary("BTC");
    assert!(w.alternates.contains(&"USD".to_string()), "after USD→BTC");
    // BTC → ETH (this is the multi-hop step that used to lose USD)
    w.swap_primary("ETH");
    assert!(
        w.alternates.contains(&"USD".to_string()),
        "USD must still be in alternates after multi-hop swap"
    );
    // USD anchored at position 0 of its native (fiat) category.
    assert_eq!(w.alternates[0], "USD");
    // BTC promoted as the "just-swapped-away" primary at the top
    // of the crypto section.
    assert_eq!(w.alternates[w.crypto_start], "BTC");

    // Another hop: ETH → SOL. USD still anchored.
    w.swap_primary("SOL");
    assert_eq!(w.alternates[0], "USD");
    assert!(w.alternates.contains(&"ETH".to_string()));
}

#[test]
fn swap_to_crypto_promotes_configured_primary_into_currency_alternates() {
    // User config: primary=USD, fiat watchlist doesn't include USD,
    // crypto_watchlist seeds BTC. Swapping primary to BTC must
    // surface USD in the currencies section (position 0 of fiat),
    // otherwise USD becomes uncomparable mid-session.
    let mut w = build_widget(ForexConfig {
        primary: "USD".into(),
        watchlist: vec!["EUR".into(), "JPY".into()],
        crypto_watchlist: vec!["BTC".into(), "ETH".into()],
        ..Default::default()
    });
    // Before swap, alternates carry only the configured lists
    // (no USD, since USD is primary).
    assert!(!w.alternates.contains(&"USD".to_string()));
    w.swap_primary("BTC");
    assert_eq!(w.primary, "BTC");
    // USD lands at position 0 of fiat; crypto_start bumps to
    // account for the inserted entry.
    assert_eq!(w.alternates[0], "USD");
    assert!(w.alternates.contains(&"USD".to_string()));
    // ETH is the only remaining crypto (BTC is now primary).
    assert_eq!(w.alternates.last().map(|s| s.as_str()), Some("ETH"));
}

#[test]
fn swap_back_to_configured_primary_restores_original_alternates_order() {
    // Drift scenario: config primary = USD, alternates = [EUR, GBP, JPY].
    // Swap USD → EUR → GBP → USD. After the final swap back to
    // the original primary, alternates must match the config
    // order (NOT [JPY, EUR, GBP] or some intermediate reshuffle).
    let mut w = build_widget(ForexConfig {
        primary: "USD".into(),
        watchlist: vec!["EUR".into(), "GBP".into(), "JPY".into()],
        crypto_watchlist: Vec::new(),
        ..Default::default()
    });
    // Seed rates so amount-conversion doesn't fall back at each hop.
    for (from, to, price) in &[
        ("USD", "EUR", 0.92_f64),
        ("EUR", "GBP", 0.85),
        ("GBP", "USD", 1.27),
    ] {
        w.state.lock().unwrap().quotes.insert(
            YahooForexProvider::symbol_for(from, to),
            QuoteState::Ready(Arc::new(rate(from, to, *price))),
        );
    }
    w.swap_primary("EUR");
    w.swap_primary("GBP");
    // Mid-walk: alternates have been reshuffled by the normal
    // "old primary onto the front" logic.
    assert_ne!(
        w.alternates,
        vec!["EUR".to_string(), "GBP".to_string(), "JPY".to_string()]
    );
    // Now swap home.
    w.swap_primary("USD");
    assert_eq!(w.primary, "USD");
    assert_eq!(
        w.alternates,
        vec!["EUR".to_string(), "GBP".to_string(), "JPY".to_string()],
        "swap-home should restore the configured alternate order, not the drifted one"
    );
    // Selection lands on the row that holds the previous primary
    // (GBP, at config index 1 → row 2).
    let rows = w.all_rows();
    let sel = w.state.lock().unwrap().selected;
    assert_eq!(rows.get(sel).map(|s| s.as_str()), Some("GBP"));
}

#[test]
fn swap_from_lookup_row_clears_transient_and_promotes() {
    // :fx NOK pins NOK as the Lookup row, then `p` promotes it.
    // After promotion: primary=NOK, alternates=[USD, EUR],
    // transient_code=None.
    let mut w = build_widget(ForexConfig {
        primary: "USD".into(),
        watchlist: vec!["EUR".into()],
        crypto_watchlist: Vec::new(),
        ..Default::default()
    });
    w.handle_fx_command(&["NOK"]).unwrap();
    assert_eq!(
        w.state.lock().unwrap().transient_code.as_deref(),
        Some("NOK")
    );
    w.swap_primary("NOK");
    assert_eq!(w.primary, "NOK");
    assert_eq!(w.alternates, vec!["USD".to_string(), "EUR".to_string()]);
    assert!(w.state.lock().unwrap().transient_code.is_none());
}

#[test]
fn initial_selection_lands_on_first_alternate() {
    let (w, _guard) = build_widget_isolated(ForexConfig {
        primary: "USD".into(),
        watchlist: vec!["EUR".into(), "JPY".into()],
        crypto_watchlist: Vec::new(),
        ..Default::default()
    });
    // Selection must land on EUR (the first alternate), not the primary row.
    let rows = w.all_rows();
    let sel = w.state.lock().unwrap().selected;
    assert_eq!(
        rows.get(sel).map(|s| s.as_str()),
        Some("EUR"),
        "initial selection must be the first alternate (EUR), not primary or another row"
    );
}

#[test]
fn row_value_for_primary_returns_amount_directly() {
    let mut w = build_widget(ForexConfig {
        primary: "USD".into(),
        watchlist: vec!["EUR".into()],
        crypto_watchlist: Vec::new(),
        ..Default::default()
    });
    w.amount = 250.0;
    let q = w.snapshot_quotes();
    assert_eq!(w.row_value("USD", &q), Some(250.0));
}

#[test]
fn row_value_for_quote_multiplies_amount_by_rate() {
    let mut w = build_widget(ForexConfig {
        primary: "USD".into(),
        watchlist: vec!["EUR".into()],
        crypto_watchlist: Vec::new(),
        ..Default::default()
    });
    w.amount = 100.0;
    w.state.lock().unwrap().quotes.insert(
        "USDEUR=X".into(),
        QuoteState::Ready(Arc::new(rate("USD", "EUR", 0.9237))),
    );
    let q = w.snapshot_quotes();
    let v = w.row_value("EUR", &q).unwrap();
    assert!((v - 92.37).abs() < 1e-6);
}

#[test]
fn fit_edit_buffer_right_pads_short_input_with_leading_spaces() {
    // "15" = 2 cells. Fit into 12 → 10 leading spaces + "15".
    // The trailing cursor lives outside the value field (in the
    // value-copy gap), so it's not part of this helper's output.
    let out = fit_edit_buffer_right("15", 12);
    assert_eq!(out.chars().count(), 12);
    assert_eq!(out, "          15");
}

#[test]
fn fit_edit_buffer_right_anchors_to_non_edit_alignment() {
    // The whole point of this helper is keeping the digits at the
    // same columns as the non-edit display so toggling `e` doesn't
    // shift them. format_amount(15.0) → "          15" should
    // match the edit-mode render for buf="15".
    assert_eq!(fit_edit_buffer_right("15", 12), format_amount(15.0));
    assert_eq!(
        fit_edit_buffer_right("1523.8000", 12),
        format_amount(1523.80)
    );
}

#[test]
fn fit_edit_buffer_right_truncates_left_when_input_overflows() {
    // "1234567890.123" = 14 cells, exceeds 12. Drop the 2 leftmost
    // chars so the most recently typed char stays visible at the
    // right edge — user is editing forward.
    let out = fit_edit_buffer_right("1234567890.123", 12);
    assert_eq!(out.chars().count(), 12);
    assert_eq!(out, "34567890.123");
}

#[test]
fn fit_edit_buffer_right_empty_buffer_is_full_width_padding() {
    let out = fit_edit_buffer_right("", 12);
    assert_eq!(out.chars().count(), 12);
    assert_eq!(out, "            ");
}

#[test]
fn is_iso_currency_codish_accepts_3_uppercase_letters_only() {
    assert!(is_iso_currency_codish("USD"));
    assert!(is_iso_currency_codish("JPY"));
    assert!(!is_iso_currency_codish("usd"));
    assert!(!is_iso_currency_codish("USDX"));
    assert!(!is_iso_currency_codish("US"));
    assert!(!is_iso_currency_codish("U$D"));
}

/// At Full size (120 × 45), the expanded strip must still render the 52-week
/// range bar (sparkline was removed; only the range bar row remains).
#[test]
fn full_size_shows_range_bar_without_sparkline() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let (w, h) = (120u16, 45u16);
    assert_eq!(
        crate::widgets::ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        crate::widgets::ViewTier::Full,
        "precondition: 120×45 must resolve to Full"
    );

    let (widget, _guard) = build_widget_isolated(ForexConfig::default());
    let q = Arc::new(ForexQuote {
        symbol: "USDEUR=X".into(),
        base: "USD".into(),
        quote: "EUR".into(),
        price: 0.9237,
        previous_close: 0.9200,
        day_high: None,
        day_low: None,
        fifty_two_week_high: Some(1.0500),
        fifty_two_week_low: Some(0.8800),
        series: (0..100).map(|i| 0.88 + i as f64 * 0.0017).collect(),
        series_timestamps: vec![],
        fetched_at: chrono::Local::now(),
    });
    widget
        .state
        .lock()
        .unwrap()
        .quotes
        .insert("USDEUR=X".into(), QuoteState::Ready(q));
    select_by_symbol(&widget, "EUR");

    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());
    assert!(
        text.contains("52w"),
        "expanded strip must render '52w' at Full size; \
         buffer snippet: {:?}",
        &text[..text.len().min(300)]
    );
    assert!(
        text.contains('●'),
        "range bar marker '●' must appear at Full size"
    );
}

/// At Expanded size (90 × 25), the 52-week range bar must appear within the
/// stats column a couple of rows below the stats text — NOT pinned to the
/// column's bottom row — and must be absent from the list and graph columns.
///
/// The bar now flows as part of `render_stats_panel`'s Paragraph (2 blank
/// lines after the last stat, then the bar line), so it sits near the top
/// of the column, not at its bottom edge.
///
/// Layout arithmetic at 90×25:
///   border → inner = (1,1,88,23); footer_h=1; body_height=22.
///   list column:  x=1,  width=27.
///   spacer:       x=28, width=1.
///   stats column: x=29, width=30.
///   spacer:       x=59, width=1.
///   graph column: x=60, width=29.
///   bottom_row = body.y + body.height − 1 = 1 + 22 − 1 = 22.
#[test]
fn range_bar_scoped_to_stats_column_not_list_or_graph() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    const WIDE_LIST_W: u16 = 27;
    const WIDE_STATS_W: u16 = 30;
    let (tw, th) = (90u16, 25u16);
    assert_eq!(
        crate::widgets::ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, tw, th)),
        crate::widgets::ViewTier::Expanded,
        "precondition: 90×25 must resolve to Expanded"
    );

    let (widget, _guard) = build_widget_isolated(ForexConfig::default());
    let q = Arc::new(ForexQuote {
        symbol: "USDEUR=X".into(),
        base: "USD".into(),
        quote: "EUR".into(),
        price: 0.9237,
        previous_close: 0.9200,
        day_high: None,
        day_low: None,
        fifty_two_week_high: Some(1.0500),
        fifty_two_week_low: Some(0.8800),
        series: (0..50).map(|i| 0.88 + i as f64 * 0.0034).collect(),
        series_timestamps: vec![],
        fetched_at: chrono::Local::now(),
    });
    widget
        .state
        .lock()
        .unwrap()
        .quotes
        .insert("USDEUR=X".into(), QuoteState::Ready(q));
    select_by_symbol(&widget, "EUR");

    let backend = TestBackend::new(tw, th);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();

    let inner_x: u16 = 1;
    let body_y: u16 = 1;
    let body_height: u16 = 22; // th - 2 (border) - 1 (footer)
    let bottom_row: u16 = body_y + body_height - 1; // 22
    let stats_col_x: u16 = inner_x + WIDE_LIST_W + 1; // 29
    let graph_col_x: u16 = stats_col_x + WIDE_STATS_W + 1; // 60

    // Collect all stats-column text across every body row.
    let mut stats_col_all = String::new();
    for y in body_y..(body_y + body_height) {
        stats_col_all.push_str(&buffer_row_range(buf, y, stats_col_x, stats_col_x + WIDE_STATS_W));
    }
    assert!(
        stats_col_all.contains("52w"),
        "range bar must appear somewhere in the stats column; \
         stats col text: {:?}",
        &stats_col_all[..stats_col_all.len().min(300)]
    );
    assert!(
        stats_col_all.contains('●'),
        "range bar marker '●' must appear in the stats column"
    );

    // Bar flows from the stats text, so it must NOT be pinned to the bottom row.
    let bottom_row_text = buffer_row_range(buf, bottom_row, stats_col_x, stats_col_x + WIDE_STATS_W);
    assert!(
        !bottom_row_text.contains("52w"),
        "bar must not be pinned to the stats column's bottom row (row {bottom_row}); \
         bottom row text: {bottom_row_text:?}"
    );

    // Bar must be absent from the list column.
    let mut list_col_all = String::new();
    for y in body_y..(body_y + body_height) {
        list_col_all.push_str(&buffer_row_range(buf, y, inner_x, inner_x + WIDE_LIST_W));
    }
    assert!(
        !list_col_all.contains("52w"),
        "range bar must not appear in the list column"
    );

    // Bar must be absent from the graph column.
    let mut graph_col_all = String::new();
    for y in body_y..(body_y + body_height) {
        graph_col_all.push_str(&buffer_row_range(buf, y, graph_col_x, tw - 1));
    }
    assert!(
        !graph_col_all.contains("52w"),
        "range bar must not bleed into the graph column"
    );
    assert!(
        !graph_col_all.contains('●'),
        "range bar marker must not appear in the graph column"
    );
}
