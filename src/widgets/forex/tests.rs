// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Unit tests for the forex widget. Split out of `mod.rs` per the repo standard.

use super::*;

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

fn build_widget(cfg: ForexConfig) -> ForexWidget {
    ForexWidget::with_config(
        "main".to_string(),
        cfg,
        Arc::new(Theme::builtin_defaults()),
        ScopedCache::ephemeral(),
    )
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
    // Move selection to EUR (row 1) and hit `s`.
    w.state.lock().unwrap().selected = 1;
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
    w.state.lock().unwrap().selected = 1;
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
    // Initial selected = 1 (first alternate). ↑ should stay at 1
    // rather than slipping down to 0 (the primary row, which has
    // no graph / stats to surface).
    let mut w = build_widget(ForexConfig {
        primary: "USD".into(),
        watchlist: vec!["EUR".into(), "JPY".into()],
        crypto_watchlist: Vec::new(),
        ..Default::default()
    });
    assert_eq!(w.state.lock().unwrap().selected, 1);
    w.move_selection(-1);
    assert_eq!(
        w.state.lock().unwrap().selected,
        1,
        "↑ at top should not drop to primary"
    );
    w.move_selection(1);
    assert_eq!(w.state.lock().unwrap().selected, 2);
    // Two moves down past the last alternate clamps at last row.
    w.move_selection(5);
    assert_eq!(w.state.lock().unwrap().selected, 2);
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
    w.state.lock().unwrap().selected = 2; // JPY row
    w.handle_fx_command(&["EUR"]).unwrap();
    assert_eq!(w.primary, "USD", ":fx must not swap");
    assert_eq!(
        w.state.lock().unwrap().selected,
        1,
        ":fx on a known code should bounce selection to its row"
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
    let w = build_widget(ForexConfig {
        primary: "USD".into(),
        watchlist: vec!["EUR".into(), "JPY".into()],
        crypto_watchlist: Vec::new(),
        ..Default::default()
    });
    assert_eq!(w.state.lock().unwrap().selected, 1);
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
